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

const PID_ANCHOR_HELPER: &str = "KIO_PROCESS_PID_ANCHOR_HELPER";
const PID_ANCHOR_ROOT: &str = "KIO_PROCESS_PID_ANCHOR_ROOT";

fn pid_anchor_helper(mode: &str, root: &std::path::Path) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "pid_anchor_fixture_helper",
            "--nocapture",
            "--test-threads=1",
        ])
        .env_clear()
        .env(PID_ANCHOR_HELPER, mode)
        .env(PID_ANCHOR_ROOT, root);
    command
}

#[test]
fn pid_anchor_fixture_helper() {
    let Some(mode) = std::env::var_os(PID_ANCHOR_HELPER) else {
        return;
    };
    let root = std::path::PathBuf::from(std::env::var_os(PID_ANCHOR_ROOT).unwrap());
    let deadline = Instant::now() + Duration::from_secs(5);
    if mode == "parent" {
        use std::os::unix::process::CommandExt;
        let mut command = pid_anchor_helper("escaped", &root);
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        // An escaped descendant deliberately retains the output pipe after
        // this parent exits. Its stop-file/deadline keeps the fixture bounded.
        let _escaped = command.spawn().unwrap();
        while !root.join("ready").exists() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        fs::write(root.join("parent.pid"), std::process::id().to_string()).unwrap();
        std::process::exit(23);
    }
    assert_eq!(mode, "escaped");
    fs::write(root.join("ready"), b"ready").unwrap();
    let mut emitted = false;
    while !root.join("stop").exists() && Instant::now() < deadline {
        if !emitted && root.join("emit").exists() {
            use std::io::Write;
            // A late pipe overflow must enter group cleanup after the direct
            // child has exited, without first releasing that child's PID.
            let _ = std::io::stdout().write_all(&[b'x'; 4096]);
            emitted = true;
        }
        thread::sleep(Duration::from_millis(2));
    }
    fs::write(root.join("done"), b"done").unwrap();
}

fn child_exit_is_waitable(pid: libc::pid_t) -> std::io::Result<bool> {
    let mut information: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut information,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result == 0 {
        Ok(unsafe { information.si_pid() } == pid)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[test]
fn exited_child_retains_pid_until_late_escaped_pipe_error_cleanup() {
    struct StopEscaped(std::path::PathBuf);
    impl Drop for StopEscaped {
        fn drop(&mut self) {
            let _ = fs::write(self.0.join("stop"), b"stop");
        }
    }

    let root = tempfile::tempdir().unwrap();
    let stop = StopEscaped(root.path().to_owned());
    let mut command = pid_anchor_helper("parent", root.path());
    let runner = thread::spawn(move || {
        run_bounded_command(
            &mut command,
            BoundedProcessOptions {
                timeout: Duration::from_secs(3),
                max_stdout_bytes: 1024,
                max_stderr_bytes: 1024,
            },
            None,
        )
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    let pid = loop {
        if let Ok(value) = fs::read_to_string(root.path().join("parent.pid")) {
            break value.parse::<libc::pid_t>().unwrap();
        }
        assert!(Instant::now() < deadline, "fixture parent did not start");
        thread::sleep(Duration::from_millis(2));
    };
    loop {
        if child_exit_is_waitable(pid).expect("runner must retain its exited child") {
            break;
        }
        assert!(Instant::now() < deadline, "fixture parent did not exit");
        thread::sleep(Duration::from_millis(2));
    }
    // Give the bounded runner several status-poll cycles. This assertion
    // inspects the kernel's child table, not a cached std::process status.
    thread::sleep(Duration::from_millis(100));
    assert!(child_exit_is_waitable(pid).expect("exited PID must remain reserved"));
    assert!(
        !runner.is_finished(),
        "escaped pipe must still be held open"
    );
    fs::write(root.path().join("emit"), b"emit").unwrap();
    let error = runner.join().unwrap().unwrap_err();
    assert!(matches!(
        error,
        BoundedProcessError::OutputLimit {
            stream: "stdout",
            limit: 1024
        }
    ));
    assert_eq!(
        child_exit_is_waitable(pid).unwrap_err().raw_os_error(),
        Some(libc::ECHILD),
        "cleanup must eventually reap the direct child"
    );
    drop(stop);
    let cleanup_deadline = Instant::now() + Duration::from_secs(2);
    while !root.path().join("done").exists() {
        assert!(
            Instant::now() < cleanup_deadline,
            "escaped fixture did not stop"
        );
        thread::sleep(Duration::from_millis(2));
    }
}
