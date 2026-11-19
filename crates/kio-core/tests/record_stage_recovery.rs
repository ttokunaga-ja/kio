//! A killed process cannot run Drop; saved stage identities must survive it.
use kio_core::{disk_records::RecordWriter, scope::Repository};
use std::{
    fs,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[test]
fn record_stage_crash_child() {
    let Some(ready) = std::env::var_os("KIO_RECORD_CRASH_READY") else {
        return;
    };
    let mut writer = RecordWriter::new().unwrap();
    for value in 0..512u64 {
        writer.push(&value).unwrap();
    }
    let records = writer.finish().unwrap();
    fs::write(ready, b"ready").unwrap();
    std::hint::black_box(&records);
    loop {
        thread::sleep(Duration::from_millis(100));
    }
}

struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn killed_record_writer_is_reported_and_reclaimed_on_retry() {
    // Keep the killed run separate from concurrent test processes' retries.
    if std::env::var_os("KIO_RECORD_CRASH_DRIVER").is_none() {
        let fixture = tempfile::tempdir().unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "killed_record_writer_is_reported_and_reclaimed_on_retry",
                "--nocapture",
            ])
            .env("KIO_RECORD_CRASH_DRIVER", "1")
            .env(
                "KIO_TEST_RECORD_STAGE_ROOT",
                fixture.path().join("registry"),
            )
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    let fixture = tempfile::tempdir().unwrap();
    let repo = Repository::init(fixture.path()).unwrap();
    let ready = fixture.path().join("ready");
    let exe = std::env::current_exe().unwrap();
    let mut child = Child(
        Command::new(exe)
            .args(["--exact", "record_stage_crash_child", "--nocapture"])
            .env("KIO_RECORD_CRASH_READY", &ready)
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let started = Instant::now();
    while !ready.exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "record child exited before creating its stage"
        );
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "record child did not become ready"
        );
        thread::sleep(Duration::from_millis(10));
    }
    // A live owner must not be reported or reclaimed by another process.
    assert!(repo.object_store().inspect_raw_stages().unwrap().is_empty());
    child.0.kill().unwrap();
    assert!(!child.0.wait().unwrap().success());
    let repo = Repository::open(fixture.path()).unwrap();
    let residues = repo.object_store().inspect_raw_stages().unwrap();
    assert!(
        residues.iter().any(|entry| entry.path.contains("record")),
        "killed writer's record stage was lost: {residues:?}"
    );
    let status = repo.status().unwrap();
    assert!(
        status
            .stage_residues
            .iter()
            .any(|entry| entry.path.contains("record")),
        "status must report record residues before a retry"
    );
    let mut retry = RecordWriter::new().unwrap();
    retry.push(&99u64).unwrap();
    let records = retry.finish().unwrap();
    assert_eq!(records.first().unwrap(), Some(99));
    assert!(repo.object_store().inspect_raw_stages().unwrap().is_empty());
}
