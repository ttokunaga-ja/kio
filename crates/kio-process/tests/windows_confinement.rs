#![cfg(windows)]

use kio_process::{
    BoundedProcessError, BoundedProcessOptions,
    confinement::{
        ConfinementError, RenderResourceLimits, RenderSandbox, protect_owner_private_scratch,
    },
};
use std::{
    ffi::OsString,
    io::Read,
    net::TcpListener,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc,
    time::Duration,
};

fn sandbox(scratch: &Path) -> (RenderSandbox, OsString) {
    let system_root = std::env::var_os("SystemRoot").expect("SystemRoot");
    let program = PathBuf::from(&system_root).join("System32").join("cmd.exe");
    let sandbox = RenderSandbox::new(
        &program,
        scratch,
        [program.parent().expect("parent").to_owned()],
        RenderResourceLimits::default(),
    )
    .expect("sandbox");
    (sandbox, system_root)
}
fn environment(root: OsString) -> [(OsString, OsString); 1] {
    [(OsString::from("SystemRoot"), root)]
}
fn options(timeout: Duration, cap: usize) -> BoundedProcessOptions {
    BoundedProcessOptions {
        timeout,
        max_stdout_bytes: cap,
        max_stderr_bytes: cap,
    }
}

fn acl_text(path: &Path) -> String {
    String::from_utf8(
        Command::new("icacls")
            .arg(path)
            .output()
            .expect("icacls")
            .stdout,
    )
    .expect("icacls output is UTF-8")
}

#[test]
fn owner_private_scratch_empty_directory_is_protected_and_owner_only() {
    let scratch = tempfile::tempdir().expect("scratch");
    protect_owner_private_scratch(scratch.path()).expect("protect empty scratch");
    let acl = acl_text(scratch.path());
    assert!(acl.contains("(F)"), "owner must retain full control: {acl}");
    assert!(!acl.contains("Everyone:"), "broad ACL retained: {acl}");
    // Inherited ACEs carry `(I)` in icacls output; the root DACL must be protected.
    assert!(!acl.contains("(I)(F)"), "root ACL was inherited: {acl}");
}

#[test]
fn owner_private_scratch_rejects_nonempty_and_releases_its_handle() {
    let scratch = tempfile::tempdir().expect("scratch");
    std::fs::write(scratch.path().join("already-staged"), b"x").expect("stage fixture");
    assert!(protect_owner_private_scratch(scratch.path()).is_err());
    let renamed = scratch.path().with_extension("renamed");
    std::fs::rename(scratch.path(), &renamed).expect("failure path released directory handle");
    std::fs::remove_dir_all(renamed).expect("remove renamed fixture");
}

#[test]
fn owner_private_scratch_rejects_junction_without_touching_target_acl() {
    let target = tempfile::tempdir().expect("target");
    let holder = tempfile::tempdir().expect("holder");
    let junction = holder.path().join("junction");
    let before = acl_text(target.path());
    let result = Command::new("cmd")
        .args(["/d", "/s", "/c", "mklink", "/J"])
        .arg(&junction)
        .arg(target.path())
        .status()
        .expect("mklink /J");
    assert!(
        result.success(),
        "ordinary junction creation must not require elevation"
    );
    assert!(protect_owner_private_scratch(&junction).is_err());
    assert_eq!(
        acl_text(target.path()),
        before,
        "junction target ACL changed"
    );
}

#[test]
fn appcontainer_writes_its_private_scratch() {
    let scratch = tempfile::tempdir().expect("scratch");
    let (sandbox, root) = sandbox(scratch.path());
    let output = scratch.path().join("output.pdf");
    let command = format!("echo %PDF-1.0>{}", output.display());
    let result = sandbox
        .run(
            ["/d", "/s", "/c", &command],
            &environment(root),
            options(Duration::from_secs(15), 1024),
        )
        .expect("launch");
    assert!(result.status.success());
    assert!(output.is_file());
}

#[test]
fn appcontainer_cannot_read_unrelated_private_file() {
    let scratch = tempfile::tempdir().expect("scratch");
    let private = tempfile::tempdir().expect("private");
    let secret = private.path().join("secret.txt");
    std::fs::write(&secret, "kio-private-secret").expect("secret");
    let (sandbox, root) = sandbox(scratch.path());
    let command = format!("type {}", secret.display());
    let result = sandbox
        .run(
            ["/d", "/s", "/c", &command],
            &environment(root),
            options(Duration::from_secs(15), 1024),
        )
        .expect("launch");
    assert!(!result.status.success());
    assert!(!result.stdout.contains("kio-private-secret"));
}

#[test]
fn appcontainer_cannot_send_loopback_tcp_body() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    listener.set_nonblocking(true).expect("nonblocking");
    let port = listener.local_addr().expect("address").port();
    let (sent, received) = mpsc::channel();
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut body = String::new();
                    let _ = stream.read_to_string(&mut body);
                    let _ = sent.send(Some(body));
                    return;
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(_) => {
                    let _ = sent.send(None);
                    return;
                }
            }
        }
    });
    let scratch = tempfile::tempdir().expect("scratch");
    let (sandbox, root) = sandbox(scratch.path());
    let command = format!(
        "powershell -NoProfile -NonInteractive -Command \"$c=New-Object Net.Sockets.TcpClient('127.0.0.1',{port});$s=$c.GetStream();$b=[Text.Encoding]::ASCII.GetBytes('forbidden');$s.Write($b,0,$b.Length)\""
    );
    let _ = sandbox.run(
        ["/d", "/s", "/c", &command],
        &environment(root),
        options(Duration::from_secs(15), 4096),
    );
    assert_ne!(
        received
            .recv_timeout(Duration::from_secs(4))
            .expect("listener result"),
        Some("forbidden".to_owned())
    );
}

#[test]
fn timeout_kills_renderer_job_before_delayed_child_can_write() {
    let scratch = tempfile::tempdir().expect("scratch");
    let marker = scratch.path().join("escaped-child.txt");
    let (sandbox, root) = sandbox(scratch.path());
    let command = format!(
        "start \"\" /b cmd /d /c \"ping -n 5 127.0.0.1>nul & echo escaped>{}\" & ping -n 5 127.0.0.1>nul",
        marker.display()
    );
    let error = sandbox
        .run(
            ["/d", "/s", "/c", &command],
            &environment(root),
            options(Duration::from_millis(100), 4096),
        )
        .expect_err("timeout");
    assert!(matches!(
        error,
        ConfinementError::Process(BoundedProcessError::Timeout { .. })
    ));
    std::thread::sleep(Duration::from_millis(600));
    assert!(!marker.exists(), "child escaped Job Object");
}

#[test]
fn output_cap_terminates_renderer_promptly() {
    let scratch = tempfile::tempdir().expect("scratch");
    let (sandbox, root) = sandbox(scratch.path());
    let error = sandbox
        .run(
            [
                "/d",
                "/s",
                "/c",
                "for /L %i in (1,1,1000) do @echo 012345678901234567890123456789",
            ],
            &environment(root),
            options(Duration::from_secs(15), 64),
        )
        .expect_err("output cap");
    assert!(matches!(
        error,
        ConfinementError::Process(BoundedProcessError::OutputLimit {
            stream: "stdout",
            limit: 64
        })
    ));
}

// The child uses filesystem handshakes so readiness and permitted completion
// are explicit. No network delay tool or new helper binary is required.
fn stage_launcher_and_child(scratch: &Path, child_action: &str, redirect_pipes: bool) {
    let redirect = if redirect_pipes { " >nul 2>nul" } else { "" };
    std::fs::write(
        scratch.join("launcher.cmd"),
        format!("@echo off\r\nstart \"\" /b \"%SystemRoot%\\System32\\cmd.exe\" /d /c child.cmd{redirect}\r\nexit /b 7\r\n"),
    ).unwrap();
    std::fs::write(
        scratch.join("child.cmd"),
        format!("@echo off\r\necho ready>child-ready\r\n:waiting\r\nif not exist release-child goto waiting\r\n{child_action}\r\nexit /b 0\r\n"),
    ).unwrap();
}

fn release_ready_child<T>(scratch: &Path, result: &mpsc::Receiver<T>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !scratch.join("child-ready").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "descendant did not report readiness"
        );
        assert!(
            matches!(result.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "renderer returned before its descendant reported readiness"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // The child cannot finish until we release it, even though its launcher
    // exits and redirected pipes can already report EOF to the host.
    let early = result.recv_timeout(Duration::from_millis(250));
    std::fs::write(scratch.join("release-child"), b"release").unwrap();
    assert!(
        matches!(early, Err(mpsc::RecvTimeoutError::Timeout)),
        "renderer returned while its descendant was waiting for release"
    );
}

#[test]
fn launcher_exit_and_pipe_eof_wait_for_descendant_output() {
    let scratch = tempfile::tempdir().unwrap();
    stage_launcher_and_child(scratch.path(), "echo complete>child-output", true);
    let (sandbox, root) = sandbox(scratch.path());
    let (sender, result) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        sender
            .send(sandbox.run(
                ["/d", "/c", "launcher.cmd"],
                &environment(root),
                options(Duration::from_secs(15), 4096),
            ))
            .unwrap();
    });
    release_ready_child(scratch.path(), &result);
    let output = result
        .recv_timeout(Duration::from_secs(15))
        .unwrap()
        .unwrap();
    worker.join().unwrap();
    assert_eq!(output.status.code(), Some(7), "preserve launcher status");
    assert_eq!(
        std::fs::read_to_string(scratch.path().join("child-output"))
            .unwrap()
            .trim(),
        "complete"
    );
}

#[test]
fn launcher_exit_does_not_stop_descendant_scratch_monitoring() {
    let scratch = tempfile::tempdir().unwrap();
    stage_launcher_and_child(
        scratch.path(),
        "for /L %%i in (1,1,2000) do @echo 012345678901234567890123456789 >>oversized\r\n:hold\r\nif not exist cleanup-probe goto hold\r\necho survived>child-survived",
        true,
    );
    let (_, root) = sandbox(scratch.path());
    let program = PathBuf::from(&root).join("System32").join("cmd.exe");
    let sandbox = RenderSandbox::new(
        &program,
        scratch.path(),
        [program.parent().unwrap().to_path_buf()],
        RenderResourceLimits {
            max_file_bytes: 2048,
            ..Default::default()
        },
    )
    .unwrap();
    let (sender, result) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        sender
            .send(sandbox.run(
                ["/d", "/c", "launcher.cmd"],
                &environment(root),
                options(Duration::from_secs(15), 4096),
            ))
            .unwrap();
    });
    release_ready_child(scratch.path(), &result);
    let error = result
        .recv_timeout(Duration::from_secs(15))
        .unwrap()
        .unwrap_err();
    worker.join().unwrap();
    std::fs::write(scratch.path().join("cleanup-probe"), b"probe").unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !scratch.path().join("child-survived").exists(),
        "descendant survived resource cleanup"
    );
    assert!(
        matches!(error, ConfinementError::Resource(ref cause)
        if cause.to_string().contains("exceeds configured limit")),
        "{error}"
    );
}

#[test]
fn late_oversized_child_output_is_rejected_when_child_exits_immediately() {
    let scratch = tempfile::tempdir().unwrap();
    // Unlike the ongoing-monitoring test, this child has no post-write hold:
    // it exits as soon as the oversized final file has been written. Either a
    // periodic observation or the mandatory Job-empty final scan must reject it.
    stage_launcher_and_child(
        scratch.path(),
        "for /L %%i in (1,1,2000) do @echo 012345678901234567890123456789 >>oversized",
        true,
    );
    let (_, root) = sandbox(scratch.path());
    let program = PathBuf::from(&root).join("System32").join("cmd.exe");
    let sandbox = RenderSandbox::new(
        &program,
        scratch.path(),
        [program.parent().unwrap().to_path_buf()],
        RenderResourceLimits {
            max_file_bytes: 2048,
            ..Default::default()
        },
    )
    .unwrap();
    let (sender, result) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        sender
            .send(sandbox.run(
                ["/d", "/c", "launcher.cmd"],
                &environment(root),
                options(Duration::from_secs(15), 4096),
            ))
            .unwrap();
    });
    release_ready_child(scratch.path(), &result);
    let error = result
        .recv_timeout(Duration::from_secs(15))
        .unwrap()
        .unwrap_err();
    worker.join().unwrap();
    assert!(
        matches!(error, ConfinementError::Resource(ref cause)
        if cause.to_string().contains("exceeds configured limit")),
        "{error}"
    );
}

#[test]
fn launcher_exit_does_not_stop_descendant_output_monitoring() {
    let scratch = tempfile::tempdir().unwrap();
    stage_launcher_and_child(
        scratch.path(),
        "for /L %%i in (1,1,2000) do @echo 012345678901234567890123456789\r\n:hold\r\nif not exist cleanup-probe goto hold\r\necho survived>child-survived",
        false,
    );
    let (sandbox, root) = sandbox(scratch.path());
    let (sender, result) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        sender
            .send(sandbox.run(
                ["/d", "/c", "launcher.cmd"],
                &environment(root),
                options(Duration::from_secs(15), 64),
            ))
            .unwrap();
    });
    release_ready_child(scratch.path(), &result);
    let error = result
        .recv_timeout(Duration::from_secs(15))
        .unwrap()
        .unwrap_err();
    worker.join().unwrap();
    std::fs::write(scratch.path().join("cleanup-probe"), b"probe").unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !scratch.path().join("child-survived").exists(),
        "descendant survived resource cleanup"
    );
    assert!(
        matches!(
            error,
            ConfinementError::Process(BoundedProcessError::OutputLimit {
                stream: "stdout",
                limit: 64,
            })
        ),
        "{error}"
    );
}

#[test]
fn launcher_exit_and_pipe_eof_do_not_bypass_descendant_timeout() {
    let scratch = tempfile::tempdir().unwrap();
    stage_launcher_and_child(scratch.path(), "echo complete>child-output", true);
    let (sandbox, root) = sandbox(scratch.path());
    let started = std::time::Instant::now();
    let error = sandbox
        .run(
            ["/d", "/c", "launcher.cmd"],
            &environment(root),
            options(Duration::from_secs(2), 4096),
        )
        .unwrap_err();
    assert!(
        matches!(
            error,
            ConfinementError::Process(BoundedProcessError::Timeout { timeout_ms: 2000 })
        ),
        "{error}"
    );
    assert!(
        scratch.path().join("child-ready").exists(),
        "descendant must have started"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "timeout unexpectedly reset or cleanup unbounded"
    );
    // If a descendant survived cleanup, releasing its handshake would permit
    // it to create this output. Give asynchronous Job termination a grace only
    // in the assertion, never in the renderer's command deadline.
    std::fs::write(scratch.path().join("release-child"), b"release").unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert!(!scratch.path().join("child-output").exists());
}
