#![cfg(windows)]

use kio_process::{
    BoundedProcessError, BoundedProcessOptions,
    confinement::{
        ConfinementError, RenderResourceLimits, RenderSandbox, protect_owner_private_scratch,
    },
};
use std::{
    ffi::OsString,
    net::TcpListener,
    os::windows::ffi::OsStrExt,
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
fn environment(root: OsString, scratch: &Path) -> [(OsString, OsString); 2] {
    [
        (OsString::from("SystemRoot"), root),
        (
            OsString::from("LOCALAPPDATA"),
            scratch.as_os_str().to_owned(),
        ),
    ]
}
fn options(timeout: Duration, cap: usize) -> BoundedProcessOptions {
    BoundedProcessOptions {
        timeout,
        max_stdout_bytes: cap,
        max_stderr_bytes: cap,
    }
}

// Keep the bytes: icacls uses the Windows console code page, including CP932.
fn acl_bytes(path: &Path) -> Vec<u8> {
    let output = Command::new("icacls").arg(path).output().expect("icacls");
    assert!(output.status.success(), "icacls failed: {output:?}");
    output.stdout
}

// LocalAlloc-owned ACLs and returned security descriptors require LocalFree.
struct LocalSecurityAllocation(*mut std::ffi::c_void);
impl Drop for LocalSecurityAllocation {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::LocalFree(self.0) };
    }
}

fn current_user_token_buffer() -> Vec<usize> {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        Security::{GetTokenInformation, TOKEN_QUERY, TokenUser},
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };
    let mut token = std::ptr::null_mut();
    assert_ne!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
        0
    );
    let mut size = 0;
    unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut size) };
    let valid_size =
        size as usize >= std::mem::size_of::<windows_sys::Win32::Security::TOKEN_USER>();
    if !valid_size {
        unsafe { CloseHandle(token) };
    }
    assert!(
        valid_size,
        "TokenUser sizing returned an invalid buffer length: {size}"
    );
    let mut buffer = vec![0_usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    };
    let error = std::io::Error::last_os_error();
    unsafe { CloseHandle(token) };
    assert_ne!(ok, 0, "read TokenUser: {error}");
    buffer
}

fn create_current_user_scratch(path: &Path, sid: windows_sys::Win32::Security::PSID) {
    use windows_sys::Win32::{
        Security::{
            Authorization::{
                EXPLICIT_ACCESS_W, SET_ACCESS, SetEntriesInAclW, TRUSTEE_IS_SID, TRUSTEE_IS_USER,
            },
            CONTAINER_INHERIT_ACE, InitializeSecurityDescriptor, OBJECT_INHERIT_ACE,
            SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
            SetSecurityDescriptorControl, SetSecurityDescriptorDacl, SetSecurityDescriptorOwner,
        },
        Storage::FileSystem::{CreateDirectoryW, DELETE, FILE_GENERIC_READ, WRITE_DAC},
    };
    // An elevated token's default owner can be Administrators. Specify TokenUser
    // during exclusive creation; never repair ownership of an existing object.
    // Start with read/delete/ACL-write access so protection must add full control.
    let mut entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_GENERIC_READ | WRITE_DAC | DELETE,
        grfAccessMode: SET_ACCESS,
        grfInheritance: OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
        ..Default::default()
    };
    entry.Trustee.TrusteeForm = TRUSTEE_IS_SID;
    entry.Trustee.TrusteeType = TRUSTEE_IS_USER;
    entry.Trustee.ptstrName = sid.cast();
    let mut acl = std::ptr::null_mut();
    assert_eq!(
        unsafe { SetEntriesInAclW(1, &entry, std::ptr::null(), &mut acl) },
        0
    );
    let _acl = LocalSecurityAllocation(acl.cast());
    let mut descriptor = SECURITY_DESCRIPTOR::default();
    let descriptor_ptr = (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast();
    assert_ne!(
        unsafe { InitializeSecurityDescriptor(descriptor_ptr, 1) },
        0
    );
    assert_ne!(
        unsafe { SetSecurityDescriptorOwner(descriptor_ptr, sid, 0) },
        0
    );
    assert_ne!(
        unsafe { SetSecurityDescriptorDacl(descriptor_ptr, 1, acl, 0) },
        0
    );
    assert_ne!(
        unsafe {
            SetSecurityDescriptorControl(descriptor_ptr, SE_DACL_PROTECTED, SE_DACL_PROTECTED)
        },
        0
    );
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor_ptr,
        bInheritHandle: 0,
    };
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    assert_ne!(
        unsafe { CreateDirectoryW(path.as_ptr(), &attributes) },
        0,
        "create TokenUser-owned scratch: {}",
        std::io::Error::last_os_error()
    );
}

fn assert_current_user_scratch(
    path: &Path,
    sid: windows_sys::Win32::Security::PSID,
    full_control: bool,
) {
    use windows_sys::Win32::{
        Foundation::GENERIC_ALL,
        Security::{
            ACCESS_ALLOWED_ACE, ACE_HEADER,
            Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT},
            CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
            GetSecurityDescriptorControl, INHERITED_ACE, OBJECT_INHERIT_ACE,
            OWNER_SECURITY_INFORMATION, SE_DACL_PROTECTED,
        },
        Storage::FileSystem::FILE_ALL_ACCESS,
    };
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut owner = std::ptr::null_mut();
    let mut acl = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    assert_eq!(
        unsafe {
            GetNamedSecurityInfoW(
                path.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                &mut acl,
                std::ptr::null_mut(),
                &mut descriptor,
            )
        },
        0
    );
    let _descriptor = LocalSecurityAllocation(descriptor);
    assert_ne!(
        unsafe { EqualSid(owner, sid) },
        0,
        "scratch owner must be TokenUser"
    );
    let mut control = 0;
    let mut revision = 0;
    assert_ne!(
        unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) },
        0
    );
    assert_ne!(
        control & SE_DACL_PROTECTED,
        0,
        "scratch DACL must be protected"
    );
    assert!(!acl.is_null(), "scratch must have a non-null DACL");
    assert_eq!(
        unsafe { (*acl).AceCount },
        1,
        "only TokenUser may have an ACE"
    );
    let mut ace = std::ptr::null_mut();
    assert_ne!(unsafe { GetAce(acl, 0, &mut ace) }, 0);
    // ACCESS_ALLOWED_ACE_TYPE is zero; inspect the header before casting the body.
    assert_eq!(unsafe { (*ace.cast::<ACE_HEADER>()).AceType }, 0);
    let ace = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
    assert_eq!(
        u32::from(ace.Header.AceFlags) & INHERITED_ACE,
        0,
        "root ACE must be explicit"
    );
    assert_eq!(
        u32::from(ace.Header.AceFlags) & (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE),
        OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
        "children must inherit owner access"
    );
    assert_ne!(
        unsafe { EqualSid(std::ptr::addr_of!(ace.SidStart).cast_mut().cast(), sid) },
        0,
        "the sole ACE must grant TokenUser"
    );
    let has_full_control =
        ace.Mask & GENERIC_ALL != 0 || ace.Mask & FILE_ALL_ACCESS == FILE_ALL_ACCESS;
    assert_eq!(
        has_full_control, full_control,
        "unexpected full-control grant"
    );
}

#[test]
fn owner_private_scratch_empty_directory_is_protected_and_owner_only() {
    let holder = tempfile::tempdir().expect("scratch holder");
    let scratch = holder.path().join("owner-private-scratch");
    let token_user = current_user_token_buffer();
    let sid = unsafe {
        (&*token_user
            .as_ptr()
            .cast::<windows_sys::Win32::Security::TOKEN_USER>())
            .User
            .Sid
    };
    create_current_user_scratch(&scratch, sid);
    assert_current_user_scratch(&scratch, sid, false);
    protect_owner_private_scratch(&scratch).expect("protect empty scratch");
    assert_current_user_scratch(&scratch, sid, true);
}

#[test]
fn owner_private_scratch_rejects_nonempty_and_releases_its_handle() {
    let scratch = tempfile::tempdir().expect("scratch");
    std::fs::write(scratch.path().join(".already-staged"), b"x").expect("stage fixture");
    let before = acl_bytes(scratch.path());
    let error = protect_owner_private_scratch(scratch.path()).expect_err("reject staged bytes");
    let ConfinementError::Profile(error) = error else {
        panic!("nonempty scratch must fail its profile precondition: {error}");
    };
    assert_eq!(error.to_string(), "scratch must be empty before staging");
    assert_eq!(
        acl_bytes(scratch.path()),
        before,
        "nonempty scratch ACL changed"
    );
    let renamed = scratch.path().with_extension("renamed");
    std::fs::rename(scratch.path(), &renamed).expect("failure path released directory handle");
    std::fs::remove_dir_all(renamed).expect("remove renamed fixture");
}

#[test]
fn owner_private_scratch_rejects_junction_without_touching_target_acl() {
    let target = tempfile::tempdir().expect("target");
    let holder = tempfile::tempdir().expect("holder");
    let junction = holder.path().join("junction");
    let before = acl_bytes(target.path());
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
        acl_bytes(target.path()),
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
            &environment(root, scratch.path()),
            options(Duration::from_secs(15), 1024),
        )
        .expect("launch");
    assert!(result.status.success());
    assert!(output.is_file());
}

#[test]
fn appcontainer_relative_output_uses_canonical_unicode_space_scratch() {
    let holder = tempfile::tempdir().expect("scratch holder");
    let scratch = holder.path().join("描画 scratch");
    std::fs::create_dir(&scratch).expect("unicode scratch");
    let canonical = std::fs::canonicalize(&scratch).expect("canonical scratch");
    let local_app_data = scratch.join("local-app-data");
    std::fs::create_dir(&local_app_data).expect("fresh LOCALAPPDATA");
    let (sandbox, root) = sandbox(&canonical);
    let environment = [
        (OsString::from("SystemRoot"), root),
        (
            OsString::from("LOCALAPPDATA"),
            local_app_data.into_os_string(),
        ),
    ];
    let result = sandbox
        .run(
            ["/d", "/s", "/c", "echo kio-cwd-marker>relative-marker.txt"],
            &environment,
            options(Duration::from_secs(15), 1024),
        )
        .expect("launch in unicode scratch");
    assert!(result.status.success(), "relative write failed: {result:?}");
    assert!(
        result.stderr.is_empty(),
        "unexpected stderr: {}",
        result.stderr
    );
    assert_eq!(
        std::fs::read(canonical.join("relative-marker.txt")).expect("relative marker"),
        b"kio-cwd-marker\r\n"
    );
    assert_eq!(std::fs::canonicalize(&scratch).unwrap(), canonical);
}

// Reuse this integration executable so the denial tests make real stdlib OS
// calls without relying on another interpreter, ambient PATH, or shell output.
fn denial_actor_sandbox(scratch: &Path) -> (RenderSandbox, Vec<(OsString, OsString)>) {
    let program = scratch.join("denial-actor.exe");
    std::fs::copy(std::env::current_exe().expect("test executable"), &program)
        .expect("stage denial actor");
    let root = std::env::var_os("SystemRoot").expect("SystemRoot");
    let sandbox = RenderSandbox::new(&program, scratch, [], RenderResourceLimits::default())
        .expect("denial actor sandbox");
    (sandbox, environment(root, scratch).into())
}

fn assert_actor_outcome(result: &kio_process::BoundedProcessOutput, attempt: &str, outcome: &str) {
    assert!(result.status.success(), "denial actor failed: {result:?}");
    let attempted = result
        .stdout
        .find(attempt)
        .expect("actual OS attempt marker");
    let completed = result
        .stdout
        .find(outcome)
        .expect("specific blocked-operation outcome marker");
    assert!(
        attempted < completed,
        "outcome must follow its actual attempt"
    );
    assert!(
        result.stderr.is_empty(),
        "unexpected actor stderr: {}",
        result.stderr
    );
}

#[test]
#[ignore = "invoked only inside the AppContainer by the denial parent tests"]
fn appcontainer_denial_actor() {
    match std::env::var("KIO_DENIAL_MODE")
        .expect("explicit actor mode")
        .as_str()
    {
        "private-file" => {
            let target = std::env::var_os("KIO_DENIAL_TARGET").expect("explicit private target");
            println!("KIO_ATTEMPT_PRIVATE_FILE");
            let error = match std::fs::read(target) {
                Err(error) => error,
                Ok(_) => panic!("private read unexpectedly succeeded"),
            };
            assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
            assert_eq!(error.raw_os_error(), Some(5));
            println!("KIO_DENIED_PRIVATE_FILE_5");
        }
        "loopback" => {
            let port: u16 = std::env::var("KIO_DENIAL_PORT")
                .expect("explicit loopback port")
                .parse()
                .expect("valid loopback port");
            let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
            println!("KIO_ATTEMPT_LOOPBACK");
            let error = match std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2))
            {
                Err(error) => error,
                Ok(_) => panic!("loopback connection unexpectedly succeeded"),
            };
            match error.kind() {
                std::io::ErrorKind::PermissionDenied => {
                    assert_eq!(error.raw_os_error(), Some(10013));
                    println!("KIO_DENIED_LOOPBACK_10013");
                }
                std::io::ErrorKind::TimedOut => println!("KIO_BLOCKED_LOOPBACK_BOUNDED_TIMEOUT"),
                _ => panic!("unexpected loopback error: {error:?}"),
            }
        }
        _ => panic!("unknown actor mode"),
    }
}

#[test]
fn appcontainer_cannot_read_unrelated_private_file() {
    let scratch = tempfile::tempdir().expect("scratch");
    let private = tempfile::tempdir().expect("private holder");
    let private_directory = private.path().join("owner-private");
    let token_user = current_user_token_buffer();
    let sid = unsafe {
        (&*token_user
            .as_ptr()
            .cast::<windows_sys::Win32::Security::TOKEN_USER>())
            .User
            .Sid
    };
    create_current_user_scratch(&private_directory, sid);
    protect_owner_private_scratch(&private_directory).expect("protect private fixture");
    assert_current_user_scratch(&private_directory, sid, true);
    let secret = private_directory.join("secret.txt");
    std::fs::write(&secret, b"kio-private-secret").expect("secret");
    assert_eq!(std::fs::read(&secret).unwrap(), b"kio-private-secret");
    let before_directory = acl_bytes(&private_directory);
    let before_file = acl_bytes(&secret);
    let (sandbox, mut environment) = denial_actor_sandbox(scratch.path());
    environment.extend([
        (
            OsString::from("KIO_DENIAL_MODE"),
            OsString::from("private-file"),
        ),
        (
            OsString::from("KIO_DENIAL_TARGET"),
            secret.as_os_str().to_owned(),
        ),
    ]);
    let result = sandbox
        .run(
            [
                "--exact",
                "appcontainer_denial_actor",
                "--ignored",
                "--nocapture",
            ],
            &environment,
            options(Duration::from_secs(15), 4096),
        )
        .expect("launch private-file denial actor");
    assert_actor_outcome(
        &result,
        "KIO_ATTEMPT_PRIVATE_FILE",
        "KIO_DENIED_PRIVATE_FILE_5",
    );
    assert!(!result.stdout.contains("kio-private-secret"));
    assert_eq!(
        acl_bytes(&private_directory),
        before_directory,
        "private directory ACL changed"
    );
    assert_eq!(acl_bytes(&secret), before_file, "private file ACL changed");
}

#[test]
fn appcontainer_cannot_send_loopback_tcp_body() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let port = address.port();
    {
        use std::io::{Read, Write};
        let mut control = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2))
            .expect("unsandboxed control connects to the same endpoint");
        control
            .write_all(b"kio-loopback-positive-control")
            .expect("send control bytes");
        control
            .shutdown(std::net::Shutdown::Write)
            .expect("finish control write");
        let (mut accepted, _) = listener.accept().expect("accept positive control");
        accepted
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut bytes = Vec::new();
        accepted
            .read_to_end(&mut bytes)
            .expect("read positive control");
        assert_eq!(bytes, b"kio-loopback-positive-control");
    }
    assert_eq!(
        listener
            .accept()
            .expect_err("control listener must be drained")
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
    let scratch = tempfile::tempdir().expect("scratch");
    let (sandbox, mut environment) = denial_actor_sandbox(scratch.path());
    environment.extend([
        (
            OsString::from("KIO_DENIAL_MODE"),
            OsString::from("loopback"),
        ),
        (
            OsString::from("KIO_DENIAL_PORT"),
            OsString::from(port.to_string()),
        ),
    ]);
    let result = sandbox
        .run(
            [
                "--exact",
                "appcontainer_denial_actor",
                "--ignored",
                "--nocapture",
            ],
            &environment,
            options(Duration::from_secs(15), 4096),
        )
        .expect("launch loopback denial actor");
    let permission_denied = result.stdout.contains("KIO_DENIED_LOOPBACK_10013");
    let bounded_blocked = result
        .stdout
        .contains("KIO_BLOCKED_LOOPBACK_BOUNDED_TIMEOUT");
    assert_ne!(
        permission_denied, bounded_blocked,
        "exactly one blocked-connection outcome is required"
    );
    assert_actor_outcome(
        &result,
        "KIO_ATTEMPT_LOOPBACK",
        if permission_denied {
            "KIO_DENIED_LOOPBACK_10013"
        } else {
            "KIO_BLOCKED_LOOPBACK_BOUNDED_TIMEOUT"
        },
    );
    let error = listener
        .accept()
        .expect_err("forbidden loopback connection accepted");
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
}

#[test]
fn timeout_kills_renderer_job_before_delayed_child_can_write() {
    let scratch = tempfile::tempdir().expect("scratch");
    let marker = scratch.path().join("escaped-child.txt");
    stage_launcher_and_child(scratch.path(), "echo escaped>escaped-child.txt", true);
    let (sandbox, root) = sandbox(scratch.path());
    let child_environment = environment(root, scratch.path());
    let (sender, result) = mpsc::channel();
    let started = std::time::Instant::now();
    let worker = std::thread::spawn(move || {
        sender
            .send(sandbox.run(
                ["/d", "/c", "launcher.cmd"],
                &child_environment,
                options(Duration::from_secs(2), 4096),
            ))
            .unwrap();
    });
    wait_ready_child(scratch.path(), &result);
    let error = result
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .expect_err("timeout");
    worker.join().unwrap();
    assert!(
        matches!(
            error,
            ConfinementError::Process(BoundedProcessError::Timeout { timeout_ms: 2000 })
        ),
        "actual handshake-fixture error: {error:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "Job cleanup was not prompt"
    );
    // A surviving child would observe release and write its marker. The child
    // has proved readiness and was never released before whole-Job cleanup.
    std::fs::write(scratch.path().join("release-child"), b"release").unwrap();
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
            &environment(root, scratch.path()),
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

fn wait_ready_child<T>(scratch: &Path, result: &mpsc::Receiver<T>) {
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
}

fn release_ready_child<T>(scratch: &Path, result: &mpsc::Receiver<T>) {
    wait_ready_child(scratch, result);
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
    let child_environment = environment(root, scratch.path());
    let worker = std::thread::spawn(move || {
        sender
            .send(sandbox.run(
                ["/d", "/c", "launcher.cmd"],
                &child_environment,
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
    let child_environment = environment(root, scratch.path());
    let worker = std::thread::spawn(move || {
        sender
            .send(sandbox.run(
                ["/d", "/c", "launcher.cmd"],
                &child_environment,
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
    let child_environment = environment(root, scratch.path());
    let worker = std::thread::spawn(move || {
        sender
            .send(sandbox.run(
                ["/d", "/c", "launcher.cmd"],
                &child_environment,
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
    let child_environment = environment(root, scratch.path());
    let worker = std::thread::spawn(move || {
        sender
            .send(sandbox.run(
                ["/d", "/c", "launcher.cmd"],
                &child_environment,
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
            &environment(root, scratch.path()),
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
