//! Real abrupt-stop recovery for large Unix stage captures.
#![cfg(unix)]

use kio_core::{
    control::ControlHandle,
    store_dir::{Publication, StageEntryState, StageIdentity, StageWriter, StoreDirectory},
    test_control::DebugTestControl,
};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const CHILD: &str = "KIO_STAGE_CAPTURE_CHILD";
const ROOT: &str = "KIO_STAGE_CAPTURE_ROOT";
const MODE: &str = "KIO_STAGE_CAPTURE_MODE";

struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn capture_crash_child() {
    let Some(root) = std::env::var_os(ROOT).filter(|_| std::env::var_os(CHILD).is_some()) else {
        return;
    };
    let root = PathBuf::from(root);
    let stages = StoreDirectory::open(&root.join("stages"))
        .unwrap()
        .rebind_with_control(ControlHandle::new(DebugTestControl::from_env()));
    let target = StoreDirectory::open(&root.join("target")).unwrap();
    let mut writer = StageWriter::create(&stages, Path::new("source")).unwrap();
    writer.write_chunk(b"owned payload").unwrap();
    writer.sync().unwrap();
    let identity = writer.identity().clone();
    let mut journal = fs::File::create(root.join("identity.json")).unwrap();
    journal
        .write_all(&serde_json::to_vec(&identity).unwrap())
        .unwrap();
    journal.sync_all().unwrap();
    fs::File::open(&root).unwrap().sync_all().unwrap();
    match std::env::var(MODE).unwrap().as_str() {
        "publish" => writer
            .publish(&target, Path::new("public"), Publication::Replace)
            .unwrap(),
        "discard" => {
            drop(writer);
            stages
                .discard_stage(Path::new("source"), &identity)
                .unwrap();
        }
        mode => panic!("unexpected capture mode {mode}"),
    }
}

#[test]
fn interrupted_publication_and_discard_reclaim_the_saved_identity_after_capture() {
    for mode in ["publish", "discard"] {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let parent = StoreDirectory::open(root.path()).unwrap();
        for leaf in ["stages", "target"] {
            parent.create_directory(Path::new(leaf)).unwrap();
        }
        fs::write(root.path().join("target/public"), b"previous").unwrap();
        let ready = root.path().join("ready");
        let stderr_path = root.path().join("child.stderr");
        let mut child = KillOnDrop(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "capture_crash_child", "--test-threads=1"])
                .env_clear()
                .env(CHILD, "1")
                .env(ROOT, root.path())
                .env(MODE, mode)
                .env("KIO_TEST_DURABILITY_POINT", "stage_captured")
                .env("KIO_TEST_DURABILITY_READY", &ready)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::from(fs::File::create(&stderr_path).unwrap()))
                .spawn()
                .unwrap(),
        );
        let expected = format!("point=stage_captured\npid={}\n", child.0.id());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if fs::read_to_string(&ready).is_ok_and(|s| s == expected) {
                break;
            }
            if let Some(status) = child.0.try_wait().unwrap() {
                panic!(
                    "capture child exited {status}: {}",
                    fs::read_to_string(&stderr_path).unwrap()
                );
            }
            assert!(
                Instant::now() < deadline,
                "capture child did not reach barrier"
            );
            thread::sleep(Duration::from_millis(10));
        }
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());
        let identity: StageIdentity =
            serde_json::from_slice(&fs::read(root.path().join("identity.json")).unwrap()).unwrap();
        let stages = StoreDirectory::open(&root.path().join("stages")).unwrap();
        assert!(!stages.contains_entry(Path::new("source")).unwrap());
        let inventory = stages
            .inspect_stages(&[(PathBuf::from("source"), identity.clone())], &[])
            .unwrap();
        assert_eq!(inventory.len(), 1);
        assert!(
            matches!(&inventory[0].state, StageEntryState::Captured { source, payload_present: true } if source == Path::new("source"))
        );
        assert!(
            stages
                .discard_stage(Path::new("source"), &identity)
                .unwrap()
        );
        assert!(stages.entries(Path::new("")).unwrap().is_empty());
        assert!(
            !stages
                .discard_stage(Path::new("source"), &identity)
                .unwrap()
        );
        assert_eq!(
            fs::read(root.path().join("target/public")).unwrap(),
            b"previous"
        );
    }
}
