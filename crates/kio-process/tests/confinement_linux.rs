#![cfg(target_os = "linux")]

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Child, Command},
    time::Duration,
};

use kio_process::{
    BoundedProcessOptions,
    confinement::{RenderResourceLimits, RenderSandbox},
};

struct FixtureProcess(Child);
impl Drop for FixtureProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn renderer_procfs_does_not_expose_host_processes() {
    // This dedicated host fixture has no ambient credentials. It must be
    // invisible through the renderer's procfs even though it has the same UID.
    let host = FixtureProcess(
        Command::new("/bin/sleep")
            .arg("30")
            .env_clear()
            .env("KIO_PRIVATE_FIXTURE", "host-only")
            .spawn()
            .unwrap(),
    );
    let scratch = tempfile::tempdir().unwrap();
    let runtime = ["/bin", "/usr/bin", "/lib", "/lib64", "/usr/lib"]
        .into_iter()
        .map(PathBuf::from)
        .filter(|path| path.exists());
    let sandbox = RenderSandbox::new(
        Path::new("/bin/sh"),
        scratch.path(),
        runtime,
        RenderResourceLimits::default(),
    )
    .expect("Linux acceptance requires bubblewrap");
    let output = sandbox
        .run(
            [
                OsString::from("-c"),
                OsString::from(
                    "test -r /proc/self/status || exit 31; \
             if cat \"/proc/$1/environ\" >/dev/null 2>&1; then exit 32; fi; \
             if test -r \"/proc/$1/root/etc/passwd\"; then exit 33; fi; printf isolated",
                ),
                OsString::from("fixture"),
                OsString::from(host.0.id().to_string()),
            ],
            &[(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))],
            BoundedProcessOptions {
                timeout: Duration::from_secs(10),
                max_stdout_bytes: 1024,
                max_stderr_bytes: 4096,
            },
        )
        .expect("Linux acceptance requires working namespace confinement");
    assert!(output.status.success(), "{}", output.stderr);
    assert_eq!(output.stdout, "isolated");
}

#[test]
fn selecting_a_renderer_file_does_not_admit_its_siblings() {
    use std::{fs, os::unix::fs::PermissionsExt};
    let scratch = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let renderer = home.path().join("soffice");
    fs::write(home.path().join("private.txt"), b"private sibling").unwrap();
    fs::write(
        &renderer,
        b"#!/bin/sh\nif cat \"$KIO_SIBLING\" 2>/dev/null; then exit 31; fi\nprintf confined\n",
    )
    .unwrap();
    fs::set_permissions(&renderer, fs::Permissions::from_mode(0o700)).unwrap();
    let runtime = ["/bin", "/usr/bin", "/lib", "/lib64", "/usr/lib"]
        .into_iter()
        .map(PathBuf::from)
        .filter(|path| path.exists());
    let sandbox = RenderSandbox::new(
        &renderer,
        scratch.path(),
        runtime,
        RenderResourceLimits::default(),
    )
    .unwrap();
    let output = sandbox
        .run(
            std::iter::empty::<OsString>(),
            &[
                (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
                (
                    OsString::from("KIO_SIBLING"),
                    home.path().join("private.txt").into_os_string(),
                ),
            ],
            BoundedProcessOptions {
                timeout: Duration::from_secs(10),
                max_stdout_bytes: 1024,
                max_stderr_bytes: 1024,
            },
        )
        .unwrap();
    assert!(output.status.success(), "{}", output.stderr);
    assert_eq!(output.stdout, "confined");
}

#[test]
fn runtime_symlink_is_mounted_at_its_requested_alias() {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };
    let scratch = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let renderer = trusted.path().join("renderer");
    fs::write(trusted.path().join("runtime.txt"), b"alias-visible").unwrap();
    fs::write(&renderer, b"#!/bin/sh\ncat \"$KIO_ALIAS/runtime.txt\"\n").unwrap();
    fs::set_permissions(&renderer, fs::Permissions::from_mode(0o700)).unwrap();
    let alias_dir = tempfile::tempdir_in("/tmp").unwrap();
    let alias = alias_dir.path().join("runtime-alias");
    symlink(trusted.path(), &alias).unwrap();
    let runtime = ["/bin", "/usr/bin", "/lib", "/lib64", "/usr/lib"]
        .into_iter()
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .chain(std::iter::once(alias.clone()));
    let sandbox = RenderSandbox::new(
        &renderer,
        scratch.path(),
        runtime,
        RenderResourceLimits::default(),
    )
    .unwrap();
    let output = sandbox
        .run(
            std::iter::empty::<OsString>(),
            &[(OsString::from("KIO_ALIAS"), alias.into_os_string())],
            BoundedProcessOptions {
                timeout: Duration::from_secs(10),
                max_stdout_bytes: 1024,
                max_stderr_bytes: 1024,
            },
        )
        .unwrap();
    assert!(output.status.success(), "{}", output.stderr);
    assert_eq!(output.stdout, "alias-visible");
}

// Native acceptance tests: backend absence is a failure, never a skip. The
// ignored entry point is only a bounded self-helper, invoked explicitly below.
const FIXTURE_ROLE: &str = "KIO_RENDERER_FIXTURE_ROLE";
const FIXTURE_ROOT: &str = "KIO_RENDERER_FIXTURE_ROOT";

fn native_runtime() -> impl Iterator<Item = PathBuf> {
    ["/bin", "/usr/bin", "/lib", "/lib64", "/usr/lib"]
        .into_iter()
        .map(PathBuf::from)
        .filter(|path| path.exists())
}

fn fixture_args() -> [&'static str; 4] {
    [
        "--exact",
        "renderer_fixture_entry",
        "--ignored",
        "--nocapture",
    ]
}

fn fixture_environment(root: &Path, role: &str) -> Vec<(OsString, OsString)> {
    vec![
        (FIXTURE_ROLE.into(), role.into()),
        (FIXTURE_ROOT.into(), root.as_os_str().into()),
    ]
}

fn run_fixture(
    root: &Path,
    role: &str,
    limits: RenderResourceLimits,
) -> Result<kio_process::BoundedProcessOutput, kio_process::confinement::ConfinementError> {
    let sandbox = RenderSandbox::new(
        &std::env::current_exe().unwrap(),
        root,
        native_runtime(),
        limits,
    )
    .expect("native Linux acceptance requires the real confinement backend");
    sandbox.run(
        fixture_args(),
        &fixture_environment(root, role),
        BoundedProcessOptions {
            timeout: limits.wall_timeout,
            max_stdout_bytes: 4096,
            max_stderr_bytes: 4096,
        },
    )
}

fn test_limits() -> RenderResourceLimits {
    RenderResourceLimits {
        wall_timeout: Duration::from_secs(15),
        cpu_seconds: 10,
        ..RenderResourceLimits::default()
    }
}

fn wait_for_file(path: &Path, timeout: Duration) {
    let start = std::time::Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < timeout,
            "fixture did not create {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn fixture_scope(root: &Path) -> PathBuf {
    let membership = std::fs::read_to_string(root.join("membership")).unwrap();
    let relative = membership
        .trim()
        .strip_prefix("0::/")
        .expect("cgroup v2 membership");
    let path = Path::new(relative);
    assert!(
        path.components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
    );
    let unit = path.file_name().unwrap().to_str().unwrap();
    assert!(
        unit.starts_with("kio-render-") && unit.ends_with(".scope"),
        "{relative}"
    );
    Path::new("/sys/fs/cgroup").join(path)
}

fn assert_scope_empty(root: &Path) {
    let scope = fixture_scope(root);
    match std::fs::read_to_string(scope.join("cgroup.events")) {
        Ok(events) => assert!(
            events.lines().any(|line| line == "populated 0"),
            "{}: {events}",
            scope.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("cannot prove scope cleanup: {error}"),
    }
}

fn spawn_fixture(root: &Path, role: &str) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(fixture_args())
        .env_clear()
        .envs(fixture_environment(root, role))
        .spawn()
        .unwrap()
}

#[test]
#[ignore = "self-helper entry point, invoked explicitly by native acceptance tests"]
fn renderer_fixture_entry() {
    let role = std::env::var(FIXTURE_ROLE).expect("fixture role required");
    let root = PathBuf::from(std::env::var_os(FIXTURE_ROOT).expect("fixture root required"));
    if role == "controller-fd" {
        use std::os::fd::AsRawFd;
        let private = tempfile::tempfile().unwrap();
        let descriptor = unsafe { libc::fcntl(private.as_raw_fd(), libc::F_DUPFD, 200) };
        assert_eq!(descriptor, 200);
        let output = run_fixture(&root, "controls", test_limits()).unwrap();
        unsafe {
            libc::close(descriptor);
        }
        assert!(output.status.success(), "{}", output.stderr);
        return;
    }
    if role == "controller" {
        let output = run_fixture(&root, "timeout", test_limits()).unwrap();
        assert!(output.status.success());
        return;
    }
    if !role.starts_with("worker-") {
        std::fs::write(
            root.join("membership"),
            std::fs::read("/proc/self/cgroup").unwrap(),
        )
        .unwrap();
    }
    if role.starts_with("worker-") {
        // Each descendant escapes the launcher's process group and retains its
        // inherited stdout/stderr; only whole-scope cleanup covers both.
        assert!(unsafe { libc::setsid() } >= 0);
        std::fs::write(root.join(format!("ready-{}", std::process::id())), b"ready").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(25);
        while !root.join("go").exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut memory = Vec::new();
        if role == "worker-memory" {
            wait_for_file(&root.join("release-memory"), Duration::from_secs(10));
            memory.resize(40 * 1024 * 1024, 0_u8);
            for page in memory.chunks_mut(4096) {
                page[0] = 1;
            }
        }
        while std::time::Instant::now() < deadline {
            if role == "worker-cpu" {
                let mut value = 1_u64;
                for _ in 0..100_000 {
                    value = std::hint::black_box(value.wrapping_mul(31).wrapping_add(7));
                }
                std::hint::black_box(value);
            } else {
                std::hint::black_box(&memory);
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        return;
    }
    if role == "renderer-env" {
        assert_eq!(
            std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap(),
            "unix:path=/nonexistent-renderer-bus"
        );
        assert_eq!(
            std::env::var("XDG_RUNTIME_DIR").unwrap(),
            "/nonexistent-renderer-runtime"
        );
        assert!(!Path::new("/run/user").exists());
        return;
    }
    if role == "controls" {
        for name in [
            "DBUS_SESSION_BUS_ADDRESS",
            "XDG_RUNTIME_DIR",
            "SYSTEMD_BUS_ADDRESS",
            "LISTEN_FDS",
            "LISTEN_PID",
        ] {
            assert!(
                std::env::var_os(name).is_none(),
                "manager variable leaked: {name}"
            );
        }
        for path in [
            "/sys/fs/cgroup/cgroup.procs",
            "/run/user",
            "/run/systemd/private",
            "/var/run/dbus/system_bus_socket",
        ] {
            assert!(!Path::new(path).exists(), "control mount exposed: {path}");
        }
        assert_eq!(
            unsafe { libc::fcntl(200, libc::F_GETFD) },
            -1,
            "ambient descriptor inherited"
        );
        return;
    }
    if role == "tasks" {
        let mut threads = Vec::new();
        for _ in 0..80 {
            match std::thread::Builder::new().stack_size(64 * 1024).spawn(|| {
                std::thread::sleep(Duration::from_secs(25));
            }) {
                Ok(thread) => threads.push(thread),
                Err(error) => {
                    assert_eq!(error.raw_os_error(), Some(libc::EAGAIN));
                    std::fs::write(root.join("handled-task-denial"), b"handled").unwrap();
                    // Deliberately report success despite handling the denial.
                    std::process::exit(0);
                }
            }
        }
        panic!("task limit allowed 80 threads");
    }
    let worker_role = match role.as_str() {
        "memory" => "worker-memory",
        "cpu" => "worker-cpu",
        _ => "worker-idle",
    };
    let _children: Vec<_> = (0..4).map(|_| spawn_fixture(&root, worker_role)).collect();
    let start = std::time::Instant::now();
    loop {
        let ready = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("ready-"))
            .count();
        if ready == 4 {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "workers did not start"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    std::fs::write(root.join("go"), b"go").unwrap();
    match role.as_str() {
        "success" => (),
        "failure" => std::process::exit(37),
        "concurrent" => {
            wait_for_file(&root.join("release"), Duration::from_secs(10));
            assert!(Path::new("/proc/self/status").exists());
        }
        _ => std::thread::sleep(Duration::from_secs(25)),
    }
}

#[test]
fn aggregate_memory_oom_kills_multiple_individually_small_workers() {
    let scratch = tempfile::tempdir().unwrap();
    let limits = RenderResourceLimits {
        max_aggregate_memory_bytes: 96 * 1024 * 1024,
        ..test_limits()
    };
    let (result, observed_oom_kill, accounting) = std::thread::scope(|threads| {
        let render = threads.spawn(|| run_fixture(scratch.path(), "memory", limits));
        wait_for_file(&scratch.path().join("go"), Duration::from_secs(8));
        let scope = fixture_scope(scratch.path());
        assert_eq!(
            std::fs::read_to_string(scope.join("memory.max"))
                .unwrap()
                .trim(),
            limits.max_aggregate_memory_bytes.to_string()
        );
        // Observe kernel OOM killing independently of the product's accounting
        // sample: scope removal can invalidate that sample before it is read.
        let events = std::fs::File::open(scope.join("memory.events")).unwrap();
        std::fs::write(scratch.path().join("release-memory"), b"release").unwrap();
        let start = std::time::Instant::now();
        let mut last_sample = String::new();
        let mut observed_oom_kill = false;
        while !render.is_finished() && start.elapsed() < Duration::from_secs(20) {
            use std::os::unix::fs::FileExt;
            let mut bytes = [0_u8; 4096];
            match events.read_at(&mut bytes, 0) {
                Ok(length) => {
                    last_sample = String::from_utf8_lossy(&bytes[..length]).into_owned();
                    observed_oom_kill |= last_sample.lines().any(|line| {
                        let mut fields = line.split_whitespace();
                        matches!(fields.next(), Some("oom_kill" | "oom_group_kill"))
                            && fields
                                .next()
                                .and_then(|value| value.parse::<u64>().ok())
                                .is_some_and(|value| value > 0)
                    });
                }
                Err(error) => {
                    last_sample = format!(
                        "retained accounting unavailable: {error}; last sample: {last_sample}"
                    );
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        (
            render.join().unwrap(),
            observed_oom_kill,
            format!("independent OOM observed={observed_oom_kill}; {last_sample}"),
        )
    });
    assert!(
        observed_oom_kill,
        "kernel OOM killing was not independently observed; {result:?}; {accounting}"
    );
    // A removed cgroup may make the product's final memory counter read return
    // ENODEV. This is a fail-closed accounting result, not an inferred OOM cause.
    let expected_failure = match &result {
        Err(kio_process::confinement::ConfinementError::Process(
            kio_process::BoundedProcessError::AggregateResourceLimit { resource: "memory" },
        )) => true,
        Err(kio_process::confinement::ConfinementError::Process(
            kio_process::BoundedProcessError::ResourceAccounting {
                resource: "memory",
                source,
            },
        )) => source.raw_os_error() == Some(libc::ENODEV),
        _ => false,
    };
    assert!(expected_failure, "{result:?}; {accounting}");
    assert!(
        scratch.path().join("go").exists(),
        "all four workers must precede OOM"
    );
    assert_scope_empty(scratch.path());
}

#[test]
fn handled_task_denial_cannot_be_accepted_as_renderer_success() {
    let scratch = tempfile::tempdir().unwrap();
    let result = run_fixture(scratch.path(), "tasks", test_limits());
    assert!(
        matches!(
            result,
            Err(kio_process::confinement::ConfinementError::Process(
                kio_process::BoundedProcessError::AggregateResourceLimit { resource: "task" }
            ))
        ),
        "{result:?}"
    );
    assert!(scratch.path().join("handled-task-denial").exists());
    assert_scope_empty(scratch.path());
}

#[test]
fn aggregate_cpu_budget_counts_all_workers() {
    let scratch = tempfile::tempdir().unwrap();
    let result = run_fixture(
        scratch.path(),
        "cpu",
        RenderResourceLimits {
            cpu_seconds: 1,
            ..test_limits()
        },
    );
    match result {
        Err(kio_process::confinement::ConfinementError::Process(
            kio_process::BoundedProcessError::AggregateCpuLimit {
                limit_usec,
                observed_usec,
            },
        )) => {
            assert_eq!(limit_usec, 1_000_000);
            assert!(observed_usec >= limit_usec);
        }
        other => panic!("expected sampled aggregate CPU failure: {other:?}"),
    }
    assert!(scratch.path().join("go").exists());
    assert_scope_empty(scratch.path());
}

#[test]
fn timeout_cleans_escaped_sessions_and_inherited_output_pipes() {
    let scratch = tempfile::tempdir().unwrap();
    let start = std::time::Instant::now();
    let result = run_fixture(
        scratch.path(),
        "timeout",
        RenderResourceLimits {
            wall_timeout: Duration::from_secs(4),
            ..test_limits()
        },
    );
    assert!(
        matches!(
            result,
            Err(kio_process::confinement::ConfinementError::Process(
                kio_process::BoundedProcessError::Timeout { .. }
            ))
        ),
        "{result:?}"
    );
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "operation plus five-second cleanup grace exceeded"
    );
    assert!(scratch.path().join("go").exists());
    assert_scope_empty(scratch.path());
}

#[test]
fn successful_and_failed_renderers_leave_no_descendants() {
    for (role, code) in [("success", 0), ("failure", 37)] {
        let scratch = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        let output = run_fixture(scratch.path(), role, test_limits()).unwrap();
        assert_eq!(output.status.code(), Some(code), "{}", output.stderr);
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "cleanup waited for pipe-holding descendants"
        );
        assert_scope_empty(scratch.path());
    }
}

#[test]
fn concurrent_renderer_scope_cleanup_does_not_kill_another_invocation() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    std::thread::scope(|threads| {
        let live = threads.spawn(|| run_fixture(first.path(), "concurrent", test_limits()));
        wait_for_file(&first.path().join("go"), Duration::from_secs(8));
        let scope = fixture_scope(first.path());
        for (file, expected) in [
            (
                "memory.max",
                test_limits().max_aggregate_memory_bytes.to_string(),
            ),
            ("memory.swap.max", "0".to_owned()),
            ("memory.oom.group", "1".to_owned()),
            ("pids.max", "64".to_owned()),
            ("cpu.max", "100000 100000".to_owned()),
        ] {
            assert_eq!(
                std::fs::read_to_string(scope.join(file)).unwrap().trim(),
                expected,
                "effective {file}"
            );
        }
        let output = run_fixture(second.path(), "success", test_limits()).unwrap();
        assert!(output.status.success());
        assert_ne!(fixture_scope(first.path()), fixture_scope(second.path()));
        assert_scope_empty(second.path());
        let events =
            std::fs::read_to_string(fixture_scope(first.path()).join("cgroup.events")).unwrap();
        assert!(events.lines().any(|line| line == "populated 1"));
        std::fs::write(first.path().join("release"), b"release").unwrap();
        assert!(live.join().unwrap().unwrap().status.success());
    });
    assert_scope_empty(first.path());
}

#[test]
fn parent_death_during_rendering_empties_the_scope() {
    let scratch = tempfile::tempdir().unwrap();
    let mut parent = FixtureProcess(spawn_fixture(scratch.path(), "controller"));
    wait_for_file(&scratch.path().join("go"), Duration::from_secs(8));
    parent.0.kill().unwrap();
    parent.0.wait().unwrap();
    let scope = fixture_scope(scratch.path());
    let start = std::time::Instant::now();
    loop {
        match std::fs::read_to_string(scope.join("cgroup.events")) {
            Ok(events) if events.lines().any(|line| line == "populated 0") => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Ok(_) => (),
            Err(error) => panic!("cleanup inspection failed: {error}"),
        }
        assert!(
            start.elapsed() < Duration::from_secs(6),
            "scope survived parent death"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn renderer_does_not_inherit_manager_environment_controls_or_ambient_fd() {
    let scratch = tempfile::tempdir().unwrap();
    // The non-CLOEXEC FD exists only inside this dedicated controller, so
    // parallel tests cannot accidentally inherit it from the test runner.
    let mut controller = FixtureProcess(spawn_fixture(scratch.path(), "controller-fd"));
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(status) = controller.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            start.elapsed() < Duration::from_secs(21),
            "controller exceeded operation plus cleanup bound"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success());
    assert_scope_empty(scratch.path());
}

#[test]
fn runtime_control_mounts_and_aliases_are_refused_before_renderer_execution() {
    use std::os::unix::fs::symlink;
    let scratch = tempfile::tempdir().unwrap();
    let alias_root = tempfile::tempdir().unwrap();
    let alias = alias_root.path().join("control-alias");
    symlink("/sys/fs/cgroup", &alias).unwrap();
    for control in [
        PathBuf::from("/"),
        PathBuf::from("/run"),
        PathBuf::from("/proc"),
        PathBuf::from("/sys/fs/cgroup"),
        alias,
    ] {
        let sandbox = RenderSandbox::new(
            &std::env::current_exe().unwrap(),
            scratch.path(),
            native_runtime().chain(std::iter::once(control)),
            test_limits(),
        );
        let result = sandbox.and_then(|sandbox| {
            sandbox.run(
                fixture_args(),
                &fixture_environment(scratch.path(), "controls"),
                BoundedProcessOptions {
                    timeout: Duration::from_secs(5),
                    max_stdout_bytes: 4096,
                    max_stderr_bytes: 4096,
                },
            )
        });
        assert!(
            matches!(
                result,
                Err(kio_process::confinement::ConfinementError::Runtime(_))
            ),
            "{result:?}"
        );
        assert!(
            !scratch.path().join("membership").exists(),
            "renderer executed before mount refusal"
        );
    }
}

#[test]
fn explicit_renderer_environment_cannot_redirect_the_manager_connection() {
    let scratch = tempfile::tempdir().unwrap();
    let sandbox = RenderSandbox::new(
        &std::env::current_exe().unwrap(),
        scratch.path(),
        native_runtime(),
        test_limits(),
    )
    .unwrap();
    let mut environment = fixture_environment(scratch.path(), "renderer-env");
    environment.extend([
        (
            "DBUS_SESSION_BUS_ADDRESS".into(),
            "unix:path=/nonexistent-renderer-bus".into(),
        ),
        (
            "XDG_RUNTIME_DIR".into(),
            "/nonexistent-renderer-runtime".into(),
        ),
    ]);
    let output = sandbox
        .run(
            fixture_args(),
            &environment,
            BoundedProcessOptions {
                timeout: Duration::from_secs(15),
                max_stdout_bytes: 4096,
                max_stderr_bytes: 4096,
            },
        )
        .unwrap();
    assert!(output.status.success(), "{}", output.stderr);
    assert_scope_empty(scratch.path());
}
