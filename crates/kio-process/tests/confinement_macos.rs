#![cfg(target_os = "macos")]

use std::{
    ffi::{CString, OsString},
    fs,
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    time::Duration,
};

use kio_process::{
    BoundedProcessOptions,
    confinement::{RenderResourceLimits, RenderSandbox},
};

#[test]
fn seatbelt_denies_network_and_unrelated_private_files() {
    let scratch = tempfile::tempdir().expect("private scratch directory");
    let outside = tempfile::NamedTempFile::new().expect("unrelated private file");
    fs::write(outside.path(), b"do not disclose").expect("write unrelated file");
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let port = listener.local_addr().unwrap().port();
    let sandbox = RenderSandbox::new(
        std::path::Path::new("/usr/bin/perl"),
        scratch.path(),
        [
            PathBuf::from("/usr/bin"),
            PathBuf::from("/usr/lib"),
            PathBuf::from("/System"),
        ],
        RenderResourceLimits::default(),
    )
    .expect("construct sandbox");
    let output = sandbox
        .run(
            [
                OsString::from("-MIO::Socket::INET"),
                OsString::from("-e"),
                OsString::from(
                    "open(my $f, '<', $ENV{KIO_UNRELATED}) and exit 31; my $s=IO::Socket::INET->new(PeerAddr=>'127.0.0.1',PeerPort=>$ENV{KIO_PORT},Proto=>'tcp',Timeout=>1); defined($s) and exit 32; print 'confined';",
                ),
            ],
            &[
                (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
                (
                    OsString::from("KIO_UNRELATED"),
                    outside.path().as_os_str().to_owned(),
                ),
                (OsString::from("KIO_PORT"), OsString::from(port.to_string())),
            ],
            BoundedProcessOptions {
                timeout: Duration::from_secs(10),
                max_stdout_bytes: 1024,
                max_stderr_bytes: 1024,
            },
        )
        .expect("seatbelt renderer execution");
    assert!(
        output.status.success(),
        "renderer status={:?} stdout={} stderr={}",
        output.status,
        output.stdout,
        output.stderr
    );
    assert_eq!(output.stdout, "confined");
    assert!(
        listener.accept().is_err(),
        "sandboxed renderer reached network listener"
    );
}

#[test]
fn selecting_a_renderer_file_does_not_admit_its_siblings() {
    let scratch = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("private.txt"), b"private sibling").unwrap();
    let sandbox = RenderSandbox::new(
        std::path::Path::new("/usr/bin/perl"),
        scratch.path(),
        [PathBuf::from("/usr/lib"), PathBuf::from("/System")],
        RenderResourceLimits::default(),
    )
    .unwrap();
    let output = sandbox
        .run(
            [
                OsString::from("-e"),
                OsString::from(
                    "open(my $f, '<', $ENV{KIO_SIBLING}) and exit 32; print 'confined';",
                ),
            ],
            &[(
                OsString::from("KIO_SIBLING"),
                home.path().join("private.txt").into_os_string(),
            )],
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
fn seatbelt_resolves_runtime_paths_without_reading_parent_contents() {
    let scratch = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let runtime = outside.path().join("runtime with spaces");
    fs::create_dir(&runtime).unwrap();
    let resource = runtime.join("bootstraprc");
    fs::write(&resource, b"fixture bootstrap").unwrap();
    let secret = outside.path().join("private.txt");
    fs::write(&secret, b"private").unwrap();
    let sandbox = RenderSandbox::new(
        std::path::Path::new("/usr/bin/perl"),
        scratch.path(),
        [runtime],
        RenderResourceLimits::default(),
    )
    .unwrap();
    // A direct open of bootstraprc succeeds even without ancestor metadata;
    // realpath reproduces LibreOffice's bootstrap lookup requirement.
    let output = sandbox
        .run(
            [
                OsString::from("-MCwd=realpath"),
                OsString::from("-e"),
                OsString::from(
                    "my $p=realpath($ARGV[0]); defined($p) or die qq(realpath: $!); \
                 open(my $f, '<', $p) or die qq(bootstrap: $!); \
                 <$f> eq 'fixture bootstrap' or die 'wrong resource'; \
                 if (open(my $s, '<', $ARGV[1])) { die 'private file readable'; } \
                 if (opendir(my $d, $ARGV[2])) { die 'parent contents readable'; } \
                 print 'resolved';",
                ),
                resource.into_os_string(),
                secret.into_os_string(),
                outside.path().as_os_str().to_owned(),
            ],
            &[(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))],
            BoundedProcessOptions {
                timeout: Duration::from_secs(10),
                max_stdout_bytes: 1024,
                max_stderr_bytes: 1024,
            },
        )
        .unwrap();
    assert!(output.status.success(), "{}", output.stderr);
    assert_eq!(output.stdout, "resolved");
}

#[test]
fn seatbelt_unix_ipc_stays_inside_private_scratch() {
    use std::os::unix::net::UnixListener;
    let scratch = tempfile::Builder::new()
        .prefix("kio-ipc-")
        .tempdir_in("/private/tmp")
        .unwrap();
    let outside = tempfile::Builder::new()
        .prefix("kio-peer-")
        .tempdir_in("/private/tmp")
        .unwrap();
    let host_socket = outside.path().join("host.sock");
    let listener = UnixListener::bind(&host_socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let sandbox = RenderSandbox::new(
        std::path::Path::new("/usr/bin/perl"),
        scratch.path(),
        [],
        RenderResourceLimits::default(),
    )
    .unwrap();
    let output = sandbox.run(
        [OsString::from("-MIO::Socket::UNIX"), OsString::from("-e"), OsString::from(
            "my $s=IO::Socket::UNIX->new(Type=>SOCK_STREAM,Local=>$ARGV[0],Listen=>1) or die qq(bind: $!); \
             my $c=IO::Socket::UNIX->new(Type=>SOCK_STREAM,Peer=>$ARGV[0]) or die qq(connect: $!); \
             my $a=$s->accept() or die qq(accept: $!); \
             my $h=IO::Socket::UNIX->new(Type=>SOCK_STREAM,Peer=>$ARGV[1]); \
             defined($h) and die 'host socket reachable'; print 'private ipc';"),
         scratch.path().join("renderer.sock").into_os_string(), host_socket.into_os_string()],
        &[], BoundedProcessOptions { timeout: Duration::from_secs(10), max_stdout_bytes: 1024, max_stderr_bytes: 4096 },
    ).unwrap();
    assert!(output.status.success(), "{}", output.stderr);
    assert_eq!(output.stdout, "private ipc");
    assert!(listener.accept().is_err());
}

#[test]
fn seatbelt_cannot_signal_an_unrelated_process() {
    struct FixtureChild(std::process::Child);
    impl Drop for FixtureChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let host = FixtureChild(
        std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap(),
    );
    let scratch = tempfile::tempdir().unwrap();
    let sandbox = RenderSandbox::new(
        std::path::Path::new("/bin/bash"),
        scratch.path(),
        [],
        RenderResourceLimits::default(),
    )
    .unwrap();
    let output = sandbox.run(
        [OsString::from("-c"), OsString::from("kill -0 $$ || exit 31; if kill -0 \"$1\" 2>/dev/null; then exit 32; fi; printf isolated"), OsString::from("fixture"), OsString::from(host.0.id().to_string())],
        &[], BoundedProcessOptions {timeout: Duration::from_secs(10), max_stdout_bytes: 1024, max_stderr_bytes: 1024},
    ).unwrap();
    assert!(output.status.success(), "{}", output.stderr);
    assert_eq!(output.stdout, "isolated");
}

#[test]
fn seatbelt_denies_fork_after_session_changes() {
    let scratch = tempfile::tempdir().unwrap();
    let sandbox = RenderSandbox::new(
        std::path::Path::new("/usr/bin/perl"),
        scratch.path(),
        [PathBuf::from("/usr/lib"), PathBuf::from("/System")],
        RenderResourceLimits::default(),
    )
    .unwrap();
    let output = sandbox
        .run(
            [
                OsString::from("-MPOSIX=setsid"),
                OsString::from("-e"),
                OsString::from(
                    "defined(setsid()) or die qq(setsid: $!); defined(fork()) and exit 31; print 'no fork';",
                ),
            ],
            &[],
            BoundedProcessOptions {
                timeout: Duration::from_secs(10),
                max_stdout_bytes: 1024,
                max_stderr_bytes: 1024,
            },
        )
        .unwrap();
    assert!(output.status.success(), "{}", output.stderr);
    assert_eq!(output.stdout, "no fork");
}

#[test]
fn posix_spawn_helper() {
    if std::env::var_os("KIO_POSIX_SPAWN_HELPER").as_deref() != Some(std::ffi::OsStr::new("1")) {
        return;
    }
    let program = CString::new("/usr/bin/true").unwrap();
    let environment = CString::new("PATH=/usr/bin:/bin").unwrap();
    let argv = [program.as_ptr().cast_mut(), std::ptr::null_mut()];
    let environment = [environment.as_ptr().cast_mut(), std::ptr::null_mut()];
    let mut pid = 0;
    let result = unsafe {
        libc::posix_spawn(
            &mut pid,
            program.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            argv.as_ptr(),
            environment.as_ptr(),
        )
    };
    if result == 0 {
        let mut status = 0;
        unsafe {
            libc::waitpid(pid, &mut status, 0);
        }
        std::process::exit(31);
    }
    std::process::exit(0);
}

#[test]
fn seatbelt_denies_posix_spawn() {
    let scratch = tempfile::tempdir().unwrap();
    let program = std::env::current_exe().expect("integration-test executable");
    let sandbox = RenderSandbox::new(
        &program,
        scratch.path(),
        [
            PathBuf::from("/usr/bin"),
            PathBuf::from("/usr/lib"),
            PathBuf::from("/System"),
        ],
        RenderResourceLimits::default(),
    )
    .unwrap();
    let output = sandbox
        .run(
            [
                OsString::from("--exact"),
                OsString::from("posix_spawn_helper"),
                OsString::from("--nocapture"),
            ],
            &[(
                OsString::from("KIO_POSIX_SPAWN_HELPER"),
                OsString::from("1"),
            )],
            BoundedProcessOptions {
                timeout: Duration::from_secs(10),
                max_stdout_bytes: 1024,
                max_stderr_bytes: 1024,
            },
        )
        .unwrap();
    assert!(output.status.success(), "{}", output.stderr);
}

#[test]
fn observed_physical_memory_cap_stops_single_process_renderer() {
    let scratch = tempfile::tempdir().unwrap();
    let sandbox = RenderSandbox::new(
        std::path::Path::new("/usr/bin/perl"),
        scratch.path(),
        [PathBuf::from("/usr/lib"), PathBuf::from("/System")],
        RenderResourceLimits {
            max_physical_memory_bytes: 1,
            ..RenderResourceLimits::default()
        },
    )
    .unwrap();
    let error = sandbox
        .run(
            [
                OsString::from("-e"),
                OsString::from("my $memory = 'x' x (64 * 1024 * 1024); sleep 30;"),
            ],
            &[],
            BoundedProcessOptions {
                timeout: Duration::from_secs(10),
                max_stdout_bytes: 1024,
                max_stderr_bytes: 1024,
            },
        )
        .expect_err("observed physical footprint must exceed a one-byte cap");
    assert!(matches!(
        error,
        kio_process::confinement::ConfinementError::Process(
            kio_process::BoundedProcessError::PhysicalMemoryLimit { limit: 1, .. }
        )
    ));
}

#[test]
fn sampled_scratch_cap_stops_many_individually_valid_files() {
    let scratch = tempfile::tempdir().unwrap();
    let sandbox = RenderSandbox::new(
        std::path::Path::new("/usr/bin/perl"),
        scratch.path(),
        [PathBuf::from("/usr/lib"), PathBuf::from("/System")],
        RenderResourceLimits {
            max_file_bytes: 1_000_000,
            ..RenderResourceLimits::default()
        },
    )
    .unwrap();
    let error = sandbox
        .run(
            [
                OsString::from("-e"),
                OsString::from(
                    "for my $n (1..2) { open(my $f, '>', qq(file$n)) or die $!; print $f 'x' x 700_000; close($f) or die $!; } sleep 30;",
                ),
            ],
            &[],
            BoundedProcessOptions {
                timeout: Duration::from_secs(10),
                max_stdout_bytes: 1024,
                max_stderr_bytes: 1024,
            },
        )
        .expect_err("aggregate scratch cap");
    assert!(matches!(
        error,
        kio_process::confinement::ConfinementError::Process(
            kio_process::BoundedProcessError::ScratchLimit {
                limit: 1_000_000,
                ..
            }
        )
    ));
}

#[test]
fn native_soffice_private_profile_probe_when_explicitly_requested() {
    if std::env::var_os("KIO_REAL_OFFICE").as_deref() != Some(std::ffi::OsStr::new("1")) {
        return;
    }
    let program = fs::canonicalize("/opt/homebrew/bin/soffice")
        .expect("explicit native probe requires installed soffice");
    let scratch = tempfile::tempdir().expect("private scratch");
    let profile = scratch.path().join("lo-profile");
    fs::create_dir(&profile).expect("private LibreOffice profile");
    fs::set_permissions(&profile, fs::Permissions::from_mode(0o700)).unwrap();
    let profile_url = format!("file://{}", profile.display());
    let sandbox = RenderSandbox::new(
        &program,
        scratch.path(),
        [
            program.parent().unwrap().to_owned(),
            program.parent().unwrap().parent().unwrap().to_owned(),
            PathBuf::from("/Applications/LibreOffice.app"),
            PathBuf::from("/System"),
            PathBuf::from("/bin"),
            PathBuf::from("/usr/bin"),
            PathBuf::from("/usr/lib"),
            PathBuf::from("/usr/share"),
            PathBuf::from("/Library/Fonts"),
            PathBuf::from("/System/Library/Fonts"),
            PathBuf::from("/private/var/db/timezone"),
        ],
        RenderResourceLimits::default(),
    )
    .expect("construct native soffice sandbox");
    let output = sandbox
        .run(
            [
                OsString::from("--headless"),
                OsString::from("--norestore"),
                OsString::from(format!("-env:UserInstallation={profile_url}")),
                OsString::from("--version"),
            ],
            &[
                (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
                (OsString::from("HOME"), profile.as_os_str().to_owned()),
                (OsString::from("TMPDIR"), profile.as_os_str().to_owned()),
            ],
            BoundedProcessOptions {
                timeout: Duration::from_secs(10),
                max_stdout_bytes: 64 * 1024,
                max_stderr_bytes: 64 * 1024,
            },
        )
        .expect("bounded native soffice probe");
    assert!(
        output.status.success(),
        "status={:?}, stderr={}",
        output.status,
        output.stderr
    );
    assert!(output.stdout.starts_with("LibreOffice"));
}
