use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn same_binary_starts_worker_with_the_staged_runtime() {
    let temp = tempfile::tempdir().unwrap();
    let executable = env!("CARGO_BIN_EXE_kio");
    let mut worker = Command::new(executable)
        .args([kio_app::eg2::WORKER_ARGUMENT, "index"])
        .env("XDG_DATA_HOME", temp.path().join("empty-device"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = worker.stdin.take().unwrap();
    input.write_all(&[1]).unwrap();

    let deadline = Instant::now() + Duration::from_secs(45);
    while worker.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            worker.kill().unwrap();
            worker.wait().unwrap();
            panic!("worker did not return its initialization status");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = worker.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert_eq!(
        output.stdout,
        [kio_app::eg2::WorkerError::ModelMissing as u8]
    );
    assert!(output.stderr.is_empty());
}
