#![cfg(unix)]

use std::{
    fs,
    process::Command,
    thread,
    time::{Duration, Instant},
};

use kio_process::{BoundedProcessError, BoundedProcessOptions, run_bounded_command};

fn shell(command: &str) -> Command {
    let mut process = Command::new("sh");
    process.arg("-c").arg(command);
    process
}

#[test]
fn timeout_kills_process_group_descendant() {
    let directory = tempfile::tempdir().expect("temporary marker directory");
    let marker = directory.path().join("descendant.pid");
    let script = format!(
        "(printf '%s' \"$$\" > '{}' ; sleep 30) & wait",
        marker.display()
    );
    let mut command = shell(&script);
    let error = run_bounded_command(
        &mut command,
        BoundedProcessOptions {
            timeout: Duration::from_millis(100),
            max_stdout_bytes: 1024,
            max_stderr_bytes: 1024,
        },
        None,
    )
    .expect_err("long-running process must time out");
    assert!(matches!(
        error,
        BoundedProcessError::Timeout { timeout_ms: 100 }
    ));

    let pid = (0..50)
        .find_map(|_| {
            fs::read_to_string(&marker)
                .ok()
                .and_then(|value| value.parse::<i32>().ok())
                .or_else(|| {
                    thread::sleep(Duration::from_millis(10));
                    None
                })
        })
        .expect("descendant records PID before parent timeout");
    let mut alive = true;
    for _ in 0..50 {
        alive = unsafe { libc::kill(pid, 0) == 0 };
        if !alive {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !alive,
        "timeout must terminate the process-group descendant"
    );
}

#[test]
fn timeout_kills_and_reaps_direct_child() {
    let mut command = shell("exec sleep 30");
    let started = Instant::now();
    let error = run_bounded_command(
        &mut command,
        BoundedProcessOptions {
            timeout: Duration::from_millis(100),
            max_stdout_bytes: 1024,
            max_stderr_bytes: 1024,
        },
        None,
    )
    .expect_err("direct child must time out");
    assert!(matches!(
        error,
        BoundedProcessError::Timeout { timeout_ms: 100 }
    ));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn output_cap_returns_before_timeout() {
    let mut command = shell("while :; do printf x; done");
    let started = Instant::now();
    let error = run_bounded_command(
        &mut command,
        BoundedProcessOptions {
            timeout: Duration::from_secs(2),
            max_stdout_bytes: 1024,
            max_stderr_bytes: 1024,
        },
        None,
    )
    .expect_err("unbounded output must be rejected");
    assert!(matches!(
        error,
        BoundedProcessError::OutputLimit {
            stream: "stdout",
            limit: 1024
        }
    ));
    assert!(started.elapsed() < Duration::from_secs(2));
}
