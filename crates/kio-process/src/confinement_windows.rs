//! Windows AppContainer launcher for [`RenderSandbox`](crate::confinement::RenderSandbox).
//!
//! This intentionally does not use `std::process::Command`: stable Rust cannot
//! attach `SECURITY_CAPABILITIES` to `STARTUPINFOEXW`.  The renderer receives an
//! AppContainer token with *zero* capabilities (in particular no internet or
//! private-network capability), an explicit environment, and only the three
//! standard handles in its inherited-handle list.

use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::Read,
    os::windows::{ffi::OsStrExt, io::FromRawHandle},
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_ID_BOTH_DIR_INFO, FILE_SHARE_NONE, FileIdBothDirectoryInfo, GetFileInformationByHandle,
    GetFileInformationByHandleEx, OPEN_EXISTING, WRITE_DAC,
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, GENERIC_ALL, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, LocalFree,
        SetHandleInformation, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Security::{
        Authorization::{
            EXPLICIT_ACCESS_W, GetNamedSecurityInfoW, SE_FILE_OBJECT, SET_ACCESS, SetEntriesInAclW,
            SetNamedSecurityInfoW, SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_USER,
        },
        CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, FreeSid, GetTokenInformation,
        OBJECT_INHERIT_ACE, PSID, SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES, TOKEN_QUERY,
        TOKEN_USER, TokenUser,
    },
    System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
            JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_JOB_TIME,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
        },
        Pipes::CreatePipe,
        Threading::{
            CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
            DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
            GetExitCodeProcess, InitializeProcThreadAttributeList, OpenProcessToken,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
            PROCESS_INFORMATION, ResumeThread, STARTUPINFOEXW, TerminateProcess,
            UpdateProcThreadAttribute, WaitForSingleObject,
        },
    },
};

use crate::confinement::{ConfinementError, RenderSandbox};
use crate::{BoundedProcessError, BoundedProcessOptions, BoundedProcessOutput};

const HUNDRED_NANOSECONDS_PER_SECOND: u64 = 10_000_000;
static PROFILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// Make a newly-created, empty scratch directory owner-private before any
/// renderer-controlled bytes are staged in it. The caller must invoke this
/// immediately after exclusive creation; non-empty or symlink paths fail.
pub(crate) fn protect_owner_private_scratch(path: &Path) -> Result<(), ConfinementError> {
    let wide_path = wide(path.as_os_str());
    let raw_directory = unsafe {
        CreateFileW(
            wide_path.as_ptr(),
            WRITE_DAC,
            FILE_SHARE_NONE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if raw_directory == INVALID_HANDLE_VALUE {
        return Err(ConfinementError::Profile(std::io::Error::last_os_error()));
    }
    let directory = OwnedHandle(raw_directory);
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(directory.0, &mut info) } == 0
        || info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT)
            != FILE_ATTRIBUTE_DIRECTORY
    {
        return Err(ConfinementError::Profile(std::io::Error::other(
            "scratch must be a newly-created empty non-symlink directory",
        )));
    }
    let mut first = vec![
        0_usize;
        (std::mem::size_of::<FILE_ID_BOTH_DIR_INFO>() + 1024)
            .div_ceil(std::mem::size_of::<usize>())
    ];
    if unsafe {
        GetFileInformationByHandleEx(
            directory.0,
            FileIdBothDirectoryInfo,
            first.as_mut_ptr().cast(),
            first.len() as u32,
        )
    } != 0
    {
        return Err(ConfinementError::Profile(std::io::Error::other(
            "scratch must be empty before staging",
        )));
    }
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(ConfinementError::Profile(std::io::Error::last_os_error()));
    }
    let mut size = 0;
    unsafe {
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut size);
    }
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
    unsafe {
        CloseHandle(token);
    }
    if ok == 0 {
        return Err(ConfinementError::Profile(std::io::Error::last_os_error()));
    }
    let sid = unsafe { (&*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut entry = EXPLICIT_ACCESS_W::default();
    entry.grfAccessPermissions = GENERIC_ALL;
    entry.grfAccessMode = SET_ACCESS;
    entry.grfInheritance = OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE;
    entry.Trustee.TrusteeForm = TRUSTEE_IS_SID;
    entry.Trustee.TrusteeType = TRUSTEE_IS_USER;
    entry.Trustee.ptstrName = sid.cast();
    let mut acl = std::ptr::null_mut();
    let code = unsafe { SetEntriesInAclW(1, &entry, std::ptr::null(), &mut acl) };
    if code != 0 {
        return Err(ConfinementError::Profile(
            std::io::Error::from_raw_os_error(code as i32),
        ));
    }
    let code = unsafe {
        SetSecurityInfo(
            directory.0,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | 0x8000_0000,
            sid,
            std::ptr::null_mut(),
            acl,
            std::ptr::null(),
        )
    };
    unsafe {
        LocalFree(acl as _);
    }
    if code != 0 {
        return Err(ConfinementError::Profile(
            std::io::Error::from_raw_os_error(code as i32),
        ));
    }
    Ok(())
}

/// Launch through `CreateProcessW` with a new AppContainer profile.  The
/// profile's zero capability count is deliberate: adding internetClient or
/// privateNetworkClientServer would turn network access back on.
pub(crate) fn run_windows_renderer<I, S>(
    sandbox: &RenderSandbox,
    args: I,
    environment: &[(OsString, OsString)],
    options: BoundedProcessOptions,
) -> Result<BoundedProcessOutput, ConfinementError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let profile = AppContainerProfile::create()?;
    grant_scratch_access(sandbox.scratch(), profile.sid)?;
    // We never relax ACLs on a renderer installation. A package must already
    // be executable/readable by AppContainers (normally through the Windows
    // All Application Packages ACE); otherwise CreateProcessW fails closed.
    for root in sandbox.runtime_roots() {
        verify_runtime_root(root)?;
    }

    let mut pipes = Pipes::new()?;
    let job = Job::new(sandbox.limits())?;
    let attributes = Attributes::new(profile.sid, pipes.child_handles())?;
    let application = wide(sandbox.program().as_os_str());
    let mut command_line = windows_command_line(sandbox.program().as_os_str(), args);
    let environment = environment_block(environment)?;
    let cwd = wide(sandbox.scratch().as_os_str());
    let started = Instant::now();
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = 0x0000_0100; // STARTF_USESTDHANDLES
    startup.StartupInfo.hStdInput = pipes.stdin_read;
    startup.StartupInfo.hStdOutput = pipes.stdout_write;
    startup.StartupInfo.hStdError = pipes.stderr_write;
    startup.lpAttributeList = attributes.list;
    let mut process = PROCESS_INFORMATION::default();
    let created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT
                | CREATE_SUSPENDED
                | CREATE_NO_WINDOW
                | CREATE_UNICODE_ENVIRONMENT,
            environment.as_ptr().cast(),
            cwd.as_ptr(),
            (&raw const startup).cast(),
            &mut process,
        )
    };
    if created == 0 {
        return Err(ConfinementError::Process(BoundedProcessError::Spawn(
            std::io::Error::last_os_error(),
        )));
    }
    // Parent ends are non-inheritable and can now be closed. The handle-list
    // attribute prevents unrelated inheritable host handles crossing this edge.
    pipes.close_child_ends();
    if let Err(error) = job
        .attach(process.hProcess)
        .and_then(|()| resume(process.hThread))
    {
        unsafe {
            // Attach can fail before the process joins this Job. Kill the
            // suspended primary process directly in that case, then wait for
            // it before releasing its handles.
            TerminateProcess(process.hProcess, 1);
            WaitForSingleObject(process.hProcess, u32::MAX);
            CloseHandle(process.hThread);
            CloseHandle(process.hProcess);
        }
        return Err(ConfinementError::Resource(error));
    }
    unsafe {
        CloseHandle(process.hThread);
    }
    let stdout = pipes.take_stdout()?;
    let stderr = pipes.take_stderr()?;
    let stdout_limit = options.max_stdout_bytes;
    let stderr_limit = options.max_stderr_bytes;
    let (sender, receiver) = mpsc::channel();
    let stderr_sender = sender.clone();
    std::thread::spawn(move || read_pipe(stdout, "stdout", stdout_limit, sender));
    std::thread::spawn(move || read_pipe(stderr, "stderr", stderr_limit, stderr_sender));
    let deadline = started + options.timeout;
    let mut output = [None, None];
    let status = loop {
        if let Err(error) =
            enforce_scratch_limit(sandbox.scratch(), sandbox.limits().max_file_bytes)
        {
            unsafe {
                TerminateJobObject(job.0, 1);
                WaitForSingleObject(process.hProcess, u32::MAX);
                CloseHandle(process.hProcess);
            }
            return Err(ConfinementError::Resource(error));
        }
        // Reader failures and output caps are terminal confinement failures,
        // not post-exit diagnostics. Observe them while the process runs.
        loop {
            match receiver.try_recv() {
                Ok((stream, result)) => match result {
                    Ok(value) => output[usize::from(stream == "stderr")] = Some(value),
                    Err(error) => {
                        unsafe {
                            TerminateJobObject(job.0, 1);
                            WaitForSingleObject(process.hProcess, u32::MAX);
                            CloseHandle(process.hProcess);
                        }
                        return Err(error);
                    }
                },
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
        let now = Instant::now();
        if now >= deadline {
            unsafe {
                TerminateJobObject(job.0, 1);
                CloseHandle(process.hProcess);
            }
            return Err(ConfinementError::Process(BoundedProcessError::Timeout {
                timeout_ms: options.timeout.as_millis(),
            }));
        }
        let wait = (deadline - now).min(Duration::from_millis(20)).as_millis() as u32;
        match unsafe { WaitForSingleObject(process.hProcess, wait) } {
            WAIT_TIMEOUT => {}
            WAIT_OBJECT_0 => {
                let mut code = 0;
                if unsafe { GetExitCodeProcess(process.hProcess, &mut code) } == 0 {
                    unsafe {
                        CloseHandle(process.hProcess);
                    }
                    return Err(ConfinementError::Process(BoundedProcessError::Wait(
                        std::io::Error::last_os_error(),
                    )));
                }
                break std::os::windows::process::ExitStatusExt::from_raw(code);
            }
            WAIT_FAILED => {
                unsafe {
                    TerminateJobObject(job.0, 1);
                    CloseHandle(process.hProcess);
                }
                return Err(ConfinementError::Process(BoundedProcessError::Wait(
                    std::io::Error::last_os_error(),
                )));
            }
            _ => unreachable!("WaitForSingleObject returned an unexpected value"),
        }
    };
    unsafe {
        CloseHandle(process.hProcess);
    }
    // Each pipe has one writer in the Job, so it reaches EOF when the process
    // exits. This bounded receive avoids a leaked descendant handle hanging us.
    while output.iter().any(Option::is_none) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok((stream, result)) => output[usize::from(stream == "stderr")] = Some(result?),
            Err(_) => {
                return Err(ConfinementError::Process(BoundedProcessError::Timeout {
                    timeout_ms: options.timeout.as_millis(),
                }));
            }
        }
    }
    Ok(BoundedProcessOutput {
        status,
        stdout: output[0].take().unwrap(),
        stderr: output[1].take().unwrap(),
        duration: started.elapsed(),
    })
}

fn enforce_scratch_limit(root: &Path, max_file_bytes: u64) -> Result<(), std::io::Error> {
    fn visit(path: &Path, max: u64, total: &mut u64) -> Result<(), std::io::Error> {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let metadata = std::fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() {
                return Err(std::io::Error::other("renderer scratch contains a symlink"));
            }
            if metadata.is_dir() {
                visit(&entry.path(), max, total)?;
            } else if metadata.is_file() {
                if metadata.len() > max {
                    return Err(std::io::Error::other(
                        "renderer scratch file exceeds configured limit",
                    ));
                }
                *total = total
                    .checked_add(metadata.len())
                    .ok_or_else(|| std::io::Error::other("renderer scratch size overflow"))?;
                if *total > max {
                    return Err(std::io::Error::other(
                        "renderer scratch total exceeds configured limit",
                    ));
                }
            } else {
                return Err(std::io::Error::other(
                    "renderer scratch contains a non-regular file",
                ));
            }
        }
        Ok(())
    }
    let mut total = 0;
    visit(root, max_file_bytes, &mut total)
}

// `runtime_roots` are never ACL-mutated. The actual access decision is left to
// the kernel at CreateProcess/image-load time; this function records the
// deliberate fail-closed policy and rejects vanished roots before launch.
fn verify_runtime_root(path: &Path) -> Result<(), ConfinementError> {
    if path.exists() {
        Ok(())
    } else {
        Err(ConfinementError::Profile(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "renderer runtime root disappeared before launch",
        )))
    }
}

fn read_pipe(
    mut pipe: File,
    stream: &'static str,
    limit: usize,
    sender: mpsc::Sender<(&'static str, Result<String, ConfinementError>)>,
) {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    let result = loop {
        match pipe.read(&mut chunk) {
            Ok(0) => {
                break String::from_utf8(bytes).map_err(|_| {
                    ConfinementError::Process(BoundedProcessError::NonUtf8 { stream })
                });
            }
            Ok(count) => {
                if bytes.len().saturating_add(count) > limit {
                    break Err(ConfinementError::Process(
                        BoundedProcessError::OutputLimit { stream, limit },
                    ));
                }
                bytes.extend_from_slice(&chunk[..count]);
            }
            Err(error) => {
                break Err(ConfinementError::Process(BoundedProcessError::Read {
                    stream,
                    source: error,
                }));
            }
        }
    };
    let _ = sender.send((stream, result));
}

struct AppContainerProfile {
    name: Vec<u16>,
    sid: PSID,
}
impl AppContainerProfile {
    fn create() -> Result<Self, ConfinementError> {
        let name = wide(OsStr::new(&format!(
            "kio-render-{}-{}",
            std::process::id(),
            unique_suffix()
        )));
        let mut sid = std::ptr::null_mut();
        let hr = unsafe {
            CreateAppContainerProfile(
                name.as_ptr(),
                name.as_ptr(),
                name.as_ptr(),
                std::ptr::null(),
                0,
                &mut sid,
            )
        };
        if hr < 0 {
            return Err(ConfinementError::Profile(
                std::io::Error::from_raw_os_error(hr),
            ));
        }
        Ok(Self { name, sid })
    }
}
impl Drop for AppContainerProfile {
    fn drop(&mut self) {
        unsafe {
            DeleteAppContainerProfile(self.name.as_ptr());
            FreeSid(self.sid);
        }
    }
}

struct Job(HANDLE);
impl Job {
    fn new(limits: crate::confinement::RenderResourceLimits) -> Result<Self, ConfinementError> {
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(ConfinementError::Resource(std::io::Error::last_os_error()));
        }
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
            | JOB_OBJECT_LIMIT_JOB_MEMORY
            | JOB_OBJECT_LIMIT_JOB_TIME;
        // soffice is a launcher which normally starts soffice.bin. Limit the
        // whole job, not just the launcher, while leaving enough room for its
        // expected helper process.
        info.BasicLimitInformation.ActiveProcessLimit = 8;
        info.BasicLimitInformation.PerJobUserTimeLimit = i64::try_from(
            limits
                .cpu_seconds
                .saturating_mul(HUNDRED_NANOSECONDS_PER_SECOND),
        )
        .unwrap_or(i64::MAX);
        info.JobMemoryLimit = usize::try_from(limits.max_address_space_bytes).unwrap_or(usize::MAX);
        if unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                std::mem::size_of_val(&info) as u32,
            )
        } == 0
        {
            unsafe {
                CloseHandle(handle);
            }
            return Err(ConfinementError::Resource(std::io::Error::last_os_error()));
        }
        Ok(Self(handle))
    }
    fn attach(&self, process: HANDLE) -> Result<(), std::io::Error> {
        if unsafe { AssignProcessToJobObject(self.0, process) } == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct Attributes {
    list: windows_sys::Win32::System::Threading::LPPROC_THREAD_ATTRIBUTE_LIST,
    #[allow(dead_code)]
    memory: Box<[usize]>,
    security: Box<SECURITY_CAPABILITIES>,
    handles: Box<[HANDLE; 3]>,
}
impl Attributes {
    fn new(sid: PSID, handles: [HANDLE; 3]) -> Result<Box<Self>, ConfinementError> {
        let mut size = 0;
        unsafe {
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 2, 0, &mut size);
        }
        let words = size.div_ceil(std::mem::size_of::<usize>());
        let mut memory = vec![0_usize; words].into_boxed_slice();
        let list = memory.as_mut_ptr().cast();
        if unsafe { InitializeProcThreadAttributeList(list, 2, 0, &mut size) } == 0 {
            return Err(ConfinementError::Profile(std::io::Error::last_os_error()));
        }
        // Attribute values must outlive CreateProcessW. Keep this allocation
        // boxed so references supplied to UpdateProcThreadAttribute remain
        // stable even after this constructor returns.
        let mut attributes = Box::new(Self {
            list,
            memory,
            security: Box::new(SECURITY_CAPABILITIES {
                AppContainerSid: sid,
                Capabilities: std::ptr::null_mut(),
                CapabilityCount: 0,
                Reserved: 0,
            }),
            handles: Box::new(handles),
        });
        if unsafe {
            UpdateProcThreadAttribute(
                attributes.list,
                0,
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
                (&raw mut *attributes.security).cast(),
                std::mem::size_of_val(&*attributes.security),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        } == 0
            || unsafe {
                UpdateProcThreadAttribute(
                    attributes.list,
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    attributes.handles.as_ptr().cast(),
                    std::mem::size_of_val(&attributes.handles),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            } == 0
        {
            unsafe {
                DeleteProcThreadAttributeList(attributes.list);
            }
            return Err(ConfinementError::Profile(std::io::Error::last_os_error()));
        }
        Ok(attributes)
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.list);
        }
    }
}

struct Pipes {
    stdin_read: HANDLE,
    stdin_write: HANDLE,
    stdout_read: HANDLE,
    stdout_write: HANDLE,
    stderr_read: HANDLE,
    stderr_write: HANDLE,
}
impl Pipes {
    fn new() -> Result<Self, ConfinementError> {
        unsafe {
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: std::ptr::null_mut(),
                bInheritHandle: 1,
            };
            let (
                mut stdin_read,
                mut stdin_write,
                mut stdout_read,
                mut stdout_write,
                mut stderr_read,
                mut stderr_write,
            ) = (
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if CreatePipe(&mut stdin_read, &mut stdin_write, &sa, 0) == 0
                || CreatePipe(&mut stdout_read, &mut stdout_write, &sa, 0) == 0
                || CreatePipe(&mut stderr_read, &mut stderr_write, &sa, 0) == 0
                || SetHandleInformation(stdin_write, HANDLE_FLAG_INHERIT, 0) == 0
                || SetHandleInformation(stdout_read, HANDLE_FLAG_INHERIT, 0) == 0
                || SetHandleInformation(stderr_read, HANDLE_FLAG_INHERIT, 0) == 0
            {
                return Err(ConfinementError::Profile(std::io::Error::last_os_error()));
            }
            Ok(Self {
                stdin_read,
                stdin_write,
                stdout_read,
                stdout_write,
                stderr_read,
                stderr_write,
            })
        }
    }
    fn child_handles(&self) -> [HANDLE; 3] {
        [self.stdin_read, self.stdout_write, self.stderr_write]
    }
    fn close_child_ends(&mut self) {
        unsafe {
            CloseHandle(self.stdin_read);
            CloseHandle(self.stdin_write);
            CloseHandle(self.stdout_write);
            CloseHandle(self.stderr_write);
            self.stdin_read = std::ptr::null_mut();
            self.stdin_write = std::ptr::null_mut();
            self.stdout_write = std::ptr::null_mut();
            self.stderr_write = std::ptr::null_mut();
        }
    }
    fn take_stdout(&mut self) -> Result<File, ConfinementError> {
        let handle = std::mem::replace(&mut self.stdout_read, std::ptr::null_mut());
        if handle.is_null() {
            Err(ConfinementError::Profile(std::io::Error::other(
                "stdout pipe already consumed",
            )))
        } else {
            Ok(unsafe { File::from_raw_handle(handle as _) })
        }
    }
    fn take_stderr(&mut self) -> Result<File, ConfinementError> {
        let handle = std::mem::replace(&mut self.stderr_read, std::ptr::null_mut());
        if handle.is_null() {
            Err(ConfinementError::Profile(std::io::Error::other(
                "stderr pipe already consumed",
            )))
        } else {
            Ok(unsafe { File::from_raw_handle(handle as _) })
        }
    }
}
impl Drop for Pipes {
    fn drop(&mut self) {
        for handle in [
            self.stdin_read,
            self.stdin_write,
            self.stdout_read,
            self.stdout_write,
            self.stderr_read,
            self.stderr_write,
        ] {
            if !handle.is_null() {
                unsafe {
                    CloseHandle(handle);
                }
            }
        }
    }
}

fn grant_scratch_access(path: &Path, app_sid: PSID) -> Result<(), ConfinementError> {
    // The caller establishes an owner-private directory before staging
    // content. Preserve its current-user ACE while adding the invocation SID;
    // protect the resulting DACL from parent-directory inheritance.
    let wide_path = wide(path.as_os_str());
    let mut previous_acl = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut previous_acl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(ConfinementError::Profile(
            std::io::Error::from_raw_os_error(status as i32),
        ));
    }
    let mut acl = std::ptr::null_mut();
    let mut entry = EXPLICIT_ACCESS_W::default();
    entry.grfAccessPermissions = GENERIC_ALL;
    entry.grfAccessMode = SET_ACCESS;
    entry.grfInheritance = OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE;
    entry.Trustee.TrusteeForm = TRUSTEE_IS_SID;
    entry.Trustee.TrusteeType = TRUSTEE_IS_USER;
    entry.Trustee.ptstrName = app_sid.cast();
    let code = unsafe { SetEntriesInAclW(1, &entry, previous_acl, &mut acl) };
    unsafe {
        LocalFree(descriptor);
    }
    if code != 0 {
        return Err(ConfinementError::Profile(
            std::io::Error::from_raw_os_error(code as i32),
        ));
    }
    let code = unsafe {
        SetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | 0x8000_0000,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            acl,
            std::ptr::null(),
        )
    };
    unsafe {
        LocalFree(acl as _);
    }
    if code != 0 {
        Err(ConfinementError::Profile(
            std::io::Error::from_raw_os_error(code as i32),
        ))
    } else {
        Ok(())
    }
}

fn resume(thread: HANDLE) -> Result<(), std::io::Error> {
    if unsafe { ResumeThread(thread) } == u32::MAX {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}
fn unique_suffix() -> u128 {
    u128::from(PROFILE_SEQUENCE.fetch_add(1, Ordering::Relaxed))
}
fn environment_block(values: &[(OsString, OsString)]) -> Result<Vec<u16>, ConfinementError> {
    let mut out = Vec::new();
    for (key, value) in values {
        if key.is_empty() || key.to_string_lossy().contains('=') {
            return Err(ConfinementError::Profile(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid explicit renderer environment name",
            )));
        }
        out.extend(
            OsStr::new(&format!(
                "{}={}",
                key.to_string_lossy(),
                value.to_string_lossy()
            ))
            .encode_wide(),
        );
        out.push(0);
    }
    out.push(0);
    Ok(out)
}
fn windows_command_line<I, S>(program: &OsStr, args: I) -> Vec<u16>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = quote_windows_arg(program);
    for arg in args {
        command.push(' ');
        command.push_str(&quote_windows_arg(arg.as_ref()));
    }
    wide(OsStr::new(&command))
}
fn quote_windows_arg(value: &OsStr) -> String {
    let value = value.to_string_lossy();
    if !value.contains([' ', '\t', '"']) {
        return value.into_owned();
    }
    let mut out = String::from("\"");
    let mut slashes = 0;
    for ch in value.chars() {
        match ch {
            '\\' => slashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(slashes * 2 + 1));
                out.push('"');
                slashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(slashes));
                out.push(ch);
                slashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('"');
    out
}
