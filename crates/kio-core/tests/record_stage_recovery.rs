//! A killed process cannot run Drop; saved stage identities must survive it.
use kio_core::{
    cas::ObjectStore,
    disk_records::{RecordWriter, SORT_BUFFER_RECORDS},
    scope::Repository,
    store_dir::StoreDirectory,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[test]
fn record_stage_crash_child() {
    let Some(ready) = std::env::var_os("KIO_RECORD_CRASH_READY") else {
        return;
    };
    let ready = PathBuf::from(ready);
    let store = ObjectStore::new(ready.parent().unwrap().join(".kio"));
    let mut writer = RecordWriter::new(&store.record_stages().unwrap()).unwrap();
    for value in 0..=SORT_BUFFER_RECORDS {
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
fn record_stages_follow_the_store_instead_of_the_cache() {
    let fixture = tempfile::tempdir().unwrap();
    let ready = fixture.path().join("ready");
    StoreDirectory::open(fixture.path())
        .unwrap()
        .create_directory(Path::new(".kio"))
        .unwrap();
    let cache = fixture.path().join("unrelated-cache");
    let mut child = Child(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "record_stage_crash_child", "--nocapture"])
            .env("KIO_RECORD_CRASH_READY", &ready)
            .env("XDG_CACHE_HOME", &cache)
            .spawn()
            .unwrap(),
    );
    wait_until_ready(&mut child, &ready);
    let stages = fixture.path().join(".kio/internal/record-stages/stages");
    assert!(fs::read_dir(stages).unwrap().next().is_some());
    assert!(!cache.exists());
}

fn wait_until_ready(child: &mut Child, ready: &std::path::Path) {
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
}

#[test]
fn killed_record_writer_is_reported_and_reclaimed_on_retry() {
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
    wait_until_ready(&mut child, &ready);
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
    let other_fixture = tempfile::tempdir().unwrap();
    let other = Repository::init(other_fixture.path()).unwrap();
    let mut other_writer =
        RecordWriter::new(&other.object_store().record_stages().unwrap()).unwrap();
    for value in 0..=SORT_BUFFER_RECORDS {
        other_writer.push(&value).unwrap();
    }
    assert!(
        other
            .object_store()
            .inspect_raw_stages()
            .unwrap()
            .is_empty()
    );
    assert!(!repo.object_store().inspect_raw_stages().unwrap().is_empty());

    let mut retry = RecordWriter::new(&repo.object_store().record_stages().unwrap()).unwrap();
    retry.push(&99u64).unwrap();
    let records = retry.finish().unwrap();
    assert_eq!(records.first().unwrap(), Some(99));
    assert!(repo.object_store().inspect_raw_stages().unwrap().is_empty());
}
