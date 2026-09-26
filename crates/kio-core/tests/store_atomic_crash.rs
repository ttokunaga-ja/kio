//! Native abrupt-stop coverage for the bounded atomic workspace protocol.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use kio_core::{
    store_dir::{AtomicWorkspaceState, Publication, StoreDirectory},
    test_control::{DebugTestControl, install_scoped},
};
use tempfile::TempDir;

const CHILD: &str = "KIO_ATOMIC_CRASH_CHILD";
const ROOT: &str = "KIO_ATOMIC_CRASH_ROOT";
const MODE: &str = "KIO_ATOMIC_CRASH_MODE";
const POINT: &str = "KIO_ATOMIC_CRASH_POINT";

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn owner(root: &Path) -> StoreDirectory {
    StoreDirectory::open(&root.join("owner")).expect("retained owner")
}

fn target(root: &Path) -> StoreDirectory {
    StoreDirectory::open(&root.join("target")).expect("retained target")
}

fn fixture() -> TempDir {
    tempfile::tempdir().expect("fixture root")
}

fn wait_ready(child: &mut Child, path: &Path, point: &str, stderr_path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let expected = format!("point={point}\npid={}\n", child.id());
    loop {
        if let Ok(metadata) = fs::metadata(path)
            && metadata.len() <= 256
            && let Ok(marker) = fs::read_to_string(path)
            && marker == expected
        {
            return;
        }
        if let Some(status) = child.try_wait().expect("poll crash child") {
            panic!(
                "child exited before {point}: {status}; stderr retained at {}",
                stderr_path.display()
            );
        }
        assert!(
            Instant::now() < deadline,
            "child did not reach {point}; stderr retained at {}",
            stderr_path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn crash_at(root: &TempDir, mode: &str, point: &str) {
    let ready = root.path().join("ready");
    let stderr_path = root.path().join("crash-child.stderr");
    let stderr = fs::File::create(&stderr_path).expect("child stderr file");
    let mut command = Command::new(std::env::current_exe().expect("test executable"));
    command
        .args(["--exact", "atomic_crash_child", "--test-threads=1"])
        .env_clear()
        .env(CHILD, "1")
        .env(ROOT, root.path())
        .env(MODE, mode)
        .env(POINT, point)
        .env("KIO_TEST_DURABILITY_POINT", point)
        .env("KIO_TEST_DURABILITY_READY", &ready)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr));
    #[cfg(windows)]
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    let mut child = KillOnDrop(command.spawn().expect("crash child"));
    wait_ready(&mut child.0, &ready, point, &stderr_path);
    child.0.kill().expect("kill stopped child");
    let status = child.0.wait().expect("wait stopped child");
    assert!(!status.success(), "killed child unexpectedly succeeded");
}

fn recover(root: &TempDir) {
    let owner = owner(root.path());
    let target = target(root.path());
    let _ = owner.recover_atomic(&[&target]).expect("recovery");
    assert_eq!(
        owner.inspect_atomic().expect("inspection"),
        AtomicWorkspaceState::Clean
    );
    let _ = owner
        .recover_atomic(&[&target])
        .expect("idempotent recovery");
}

#[test]
fn atomic_crash_child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let root = PathBuf::from(std::env::var_os(ROOT).expect("fixture root"));
    let mode = std::env::var(MODE).expect("mode");
    let fixture_root = StoreDirectory::open(&root).expect("retained fixture root");
    let owner = StoreDirectory::from_retained(
        fixture_root
            .create_directory(Path::new("owner"))
            .expect("private owner directory"),
        root.join("owner"),
    )
    .expect("retained owner");
    let target = StoreDirectory::from_retained(
        fixture_root
            .create_directory(Path::new("target"))
            .expect("private target directory"),
        root.join("target"),
    )
    .expect("retained target");
    let seed = match mode.as_str() {
        "write" => b"old".as_slice(),
        "remove" => b"remove".as_slice(),
        other => panic!("unknown crash mode {other}"),
    };
    target
        .write_atomic_with_owner(&owner, Path::new("leaf"), seed, Publication::CreateOnly)
        .expect("seed working leaf");
    #[cfg(unix)]
    if mode == "remove" {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(root.join("target/leaf"), fs::Permissions::from_mode(0o644))
            .expect("ordinary working mode");
    }
    let _test_control = install_scoped(DebugTestControl::from_env());
    match mode.as_str() {
        "write" => {
            target
                .write_atomic_with_owner(&owner, Path::new("leaf"), b"new", Publication::Replace)
                .expect("write operation");
        }
        "remove" => {
            // Deliberately ordinary working-file permissions: the quarantined
            // file is evidence and must not be mistaken for a private journal.
            target
                .quarantine_then_remove_with_owner(&owner, Path::new("leaf"), b"remove", 64)
                .expect("remove operation");
        }
        other => panic!("unknown crash mode {other}"),
    }
}

#[test]
fn abrupt_write_staging_and_publication_recover_without_user_artifacts() {
    for (point, expected) in [
        ("atomic_write_staged", b"old".as_slice()),
        ("atomic_write_published", b"new".as_slice()),
    ] {
        let root = fixture();
        crash_at(&root, "write", point);
        recover(&root);
        assert_eq!(
            fs::read(root.path().join("target/leaf")).expect("published leaf"),
            expected
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            assert_eq!(
                fs::metadata(root.path().join("target/leaf"))
                    .unwrap()
                    .nlink(),
                1
            );
        }
        assert!(!root.path().join("target/.kio-atomic").exists());
    }
}

#[test]
fn abrupt_remove_ready_quarantine_and_delete_recover_idempotently() {
    for point in [
        "atomic_remove_ready",
        "atomic_remove_quarantined",
        "atomic_remove_deleted",
    ] {
        let root = fixture();
        crash_at(&root, "remove", point);
        recover(&root);
        assert!(!root.path().join("target/leaf").exists());
        assert!(!root.path().join("target/.kio-atomic").exists());
    }
}
