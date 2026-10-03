//! Abrupt-stop coverage for a tag deletion's retained atomic workspace.

#![cfg(debug_assertions)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use kio_core::{
    scope::Repository,
    store_dir::{AtomicWorkspaceState, StoreDirectory},
    test_control::{DebugTestControl, install_scoped},
};
use tempfile::TempDir;

const CHILD: &str = "KIO_TAG_ATOMIC_CRASH_CHILD";
const ROOT: &str = "KIO_TAG_ATOMIC_CRASH_ROOT";

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn fixture() -> TempDir {
    tempfile::tempdir_in(
        std::env::temp_dir()
            .canonicalize()
            .expect("canonical temporary root"),
    )
    .expect("fixture root")
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

fn crash_at(root: &TempDir, point: &str) {
    let ready = root.path().join("ready");
    let stderr_path = root.path().join("crash-child.stderr");
    let stderr = fs::File::create(&stderr_path).expect("child stderr file");
    let mut command = Command::new(std::env::current_exe().expect("test executable"));
    command
        .args(["--exact", "tag_delete_crash_child", "--test-threads=1"])
        .env_clear()
        .env(CHILD, "1")
        .env(ROOT, root.path())
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

#[test]
fn tag_delete_crash_child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let root = PathBuf::from(std::env::var_os(ROOT).expect("fixture root"));
    let repo = Repository::open_for_recovery(&root).expect("retained repository");
    let _test_control = install_scoped(DebugTestControl::from_env());
    repo.delete_tag("gone").expect("delete tag");
}

#[test]
fn retried_tag_delete_recovers_interrupted_atomic_removal() {
    for point in [
        "atomic_remove_ready",
        "atomic_remove_quarantined",
        "atomic_remove_deleted",
    ] {
        let root = fixture();
        let repo = Repository::init(root.path()).expect("scope");
        fs::write(root.path().join("note.md"), b"original").unwrap();
        let original = repo
            .snapshot(Some("original"), None)
            .unwrap()
            .commit_hash
            .unwrap();
        fs::write(root.path().join("note.md"), b"current").unwrap();
        let current = repo
            .snapshot(Some("current"), None)
            .unwrap()
            .commit_hash
            .unwrap();
        repo.tag("gone", Some(&original)).unwrap();
        repo.tag("other", Some(&current)).unwrap();
        let kio = repo.kio_dir();
        let names = kio.join("refs/tags-v1/names.jsonl");
        let audit_before = fs::read(&names).unwrap();
        let head_before = fs::read(kio.join("HEAD")).unwrap();
        let other_ref = kio
            .join("refs/tags-v1")
            .join(kio_core::portable::portable_tag_leaf("other"));
        let other_before = fs::read(&other_ref).unwrap();

        crash_at(&root, point);
        let error = repo.delete_tag("gone").unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-NOT-FOUND-001");
        let directory = StoreDirectory::open(kio).unwrap();
        assert_eq!(
            directory.inspect_atomic().unwrap(),
            AtomicWorkspaceState::Clean
        );
        assert_eq!(fs::read(&names).unwrap(), audit_before);
        assert_eq!(fs::read(kio.join("HEAD")).unwrap(), head_before);
        assert_eq!(fs::read(&other_ref).unwrap(), other_before);
        assert_eq!(repo.resolve_commit("other").unwrap(), current);
        assert_eq!(repo.tag("gone", Some(&original)).unwrap(), original);
    }
}
