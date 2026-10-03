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
    path::{Component, Path, Prefix},
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
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_ID_BOTH_DIR_INFO, FILE_LIST_DIRECTORY, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_NONE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_DISK,
    FileIdBothDirectoryInfo, GetFileInformationByHandle, GetFileInformationByHandleEx, GetFileType,
    GetFinalPathNameByHandleW, OPEN_EXISTING, READ_CONTROL, VOLUME_NAME_DOS, WRITE_DAC,
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_FILES, GENERIC_ALL, HANDLE,
        HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, LocalFree, MAX_PATH, SetHandleInformation,
        WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Security::{
        Authorization::{
            EXPLICIT_ACCESS_W, GetNamedSecurityInfoW, SE_FILE_OBJECT, SET_ACCESS, SetEntriesInAclW,
            SetNamedSecurityInfoW, SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_USER,
        },
        CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, FreeSid, GetTokenInformation,
        OBJECT_INHERIT_ACE, PROTECTED_DACL_SECURITY_INFORMATION, PSID, SECURITY_ATTRIBUTES,
        SECURITY_CAPABILITIES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
            JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_JOB_TIME,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
            JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
            TerminateJobObject,
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
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | READ_CONTROL | WRITE_DAC,
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
    // FILE_ID_BOTH_DIR_INFO records require eight-byte alignment. The Win32
    // buffer length is in bytes, not the number of backing allocation elements.
    #[repr(align(8))]
    struct DirectoryEntries([u8; 4096]);
    let mut entries = DirectoryEntries([0; 4096]);
    let buffer_bytes = entries.0.len();
    let name_offset = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
    let invalid_records = || {
        ConfinementError::Profile(std::io::Error::other(
            "invalid scratch directory enumeration",
        ))
    };
    let mut seen_dot = false;
    let mut seen_dot_dot = false;
    loop {
        entries.0.fill(0);
        if unsafe {
            GetFileInformationByHandleEx(
                directory.0,
                FileIdBothDirectoryInfo,
                entries.0.as_mut_ptr().cast(),
                buffer_bytes as u32,
            )
        } == 0
        {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                break;
            }
            return Err(ConfinementError::Profile(error));
        }
        let bytes = &entries.0;
        let mut offset = 0;
        loop {
            let record = bytes.get(offset..).ok_or_else(&invalid_records)?;
            if record.len() < name_offset {
                return Err(invalid_records());
            }
            // Read only the fixed header fields; a short final filename need
            // not occupy the tail padding of the Rust structure.
            let next = u32::from_ne_bytes(record[..4].try_into().unwrap()) as usize;
            let length_offset = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileNameLength);
            let name_bytes =
                u32::from_ne_bytes(record[length_offset..length_offset + 4].try_into().unwrap())
                    as usize;
            if name_bytes == 0 || !name_bytes.is_multiple_of(2) {
                return Err(invalid_records());
            }
            let record_end = name_offset
                .checked_add(name_bytes)
                .filter(|end| *end <= record.len())
                .ok_or_else(&invalid_records)?;
            if next != 0 && (!next.is_multiple_of(8) || next < record_end || next >= record.len()) {
                return Err(invalid_records());
            }
            let name = &record[name_offset..record_end];
            let dot = [b'.', 0];
            let dot_dot = [b'.', 0, b'.', 0];
            let seen = if name == dot {
                &mut seen_dot
            } else if name == dot_dot {
                &mut seen_dot_dot
            } else {
                return Err(ConfinementError::Profile(std::io::Error::other(
                    "scratch must be empty before staging",
                )));
            };
            // At most two pseudoentries can be skipped across all batches.
            // Reject repeats so even a malformed enumerator is bounded.
            if *seen {
                return Err(invalid_records());
            }
            *seen = true;
            if next == 0 {
                break;
            }
            offset = offset.checked_add(next).ok_or_else(&invalid_records)?;
        }
    }
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(ConfinementError::Profile(std::io::Error::last_os_error()));
    }
    let token = OwnedHandle(token);
    let mut size = 0;
    let sized =
        unsafe { GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut size) };
    let error = std::io::Error::last_os_error();
    if sized != 0 || error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
        return Err(ConfinementError::Profile(error));
    }
    if (size as usize) < std::mem::size_of::<TOKEN_USER>() {
        return Err(ConfinementError::Profile(std::io::Error::other(
            "invalid TokenUser buffer length",
        )));
    }
    let mut buffer = vec![0_usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
    let ok = unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    };
    if ok == 0 {
        return Err(ConfinementError::Profile(std::io::Error::last_os_error()));
    }
    let sid = unsafe { (&*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_ALL_ACCESS,
        grfAccessMode: SET_ACCESS,
        grfInheritance: OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
        ..Default::default()
    };
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
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
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
    let cwd = renderer_current_directory(sandbox.scratch());
    let started = Instant::now();
    let deadline = started
        .checked_add(options.timeout)
        .ok_or(ConfinementError::Process(
            BoundedProcessError::InvalidLifetime,
        ))?;
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
    let primary = OwnedHandle(process.hProcess);
    let thread = OwnedHandle(process.hThread);
    // The handle-list attribute prevents unrelated host handles crossing this
    // edge. Close our copies of the child's pipe ends before observing EOF.
    pipes.close_child_ends();
    if let Err(error) = job.attach(primary.0) {
        // Assignment can fail before the suspended primary belongs to the Job.
        // Preserve the assignment error before making any cleanup API calls.
        unsafe {
            TerminateProcess(primary.0, 1);
            WaitForSingleObject(primary.0, remaining_wait_millis(deadline));
        }
        return Err(ConfinementError::Resource(error));
    }
    // All failures after assignment go through whole-Job cleanup, including
    // resume, pipe setup, wait/accounting, output, and scratch failures.
    let result = (|| {
        resume(thread.0).map_err(ConfinementError::Resource)?;
        drop(thread);
        let stdout = pipes.take_stdout()?;
        let stderr = pipes.take_stderr()?;
        let stdout_limit = options.max_stdout_bytes;
        let stderr_limit = options.max_stderr_bytes;
        let (sender, receiver) = mpsc::channel();
        let stderr_sender = sender.clone();
        std::thread::spawn(move || read_pipe(stdout, "stdout", stdout_limit, sender));
        std::thread::spawn(move || read_pipe(stderr, "stderr", stderr_limit, stderr_sender));
        let mut output = [None, None];
        let mut status = None;
        loop {
            check_renderer_deadline(deadline, options)?;
            while let Ok((stream, result)) = receiver.try_recv() {
                output[usize::from(stream == "stderr")] = Some(result?);
            }
            observe_scratch_until(sandbox, deadline, options)?;
            if status.is_none() {
                match unsafe { WaitForSingleObject(primary.0, 0) } {
                    WAIT_TIMEOUT => {}
                    WAIT_OBJECT_0 => {
                        let mut code = 0;
                        if unsafe { GetExitCodeProcess(primary.0, &mut code) } == 0 {
                            return Err(ConfinementError::Process(BoundedProcessError::Wait(
                                std::io::Error::last_os_error(),
                            )));
                        }
                        status = Some(std::os::windows::process::ExitStatusExt::from_raw(code));
                    }
                    WAIT_FAILED => {
                        return Err(ConfinementError::Process(BoundedProcessError::Wait(
                            std::io::Error::last_os_error(),
                        )));
                    }
                    _ => {
                        return Err(ConfinementError::Process(BoundedProcessError::Wait(
                            std::io::Error::other("unexpected renderer process wait result"),
                        )));
                    }
                }
            }
            if job.active_processes().map_err(ConfinementError::Resource)? == 0 {
                // A private non-inherited Job with no breakaway allowance cannot
                // acquire a new descendant after its last process has exited.
                // If the primary exited between its poll and this query, poll
                // it again on the next iteration to obtain its original status.
                if let Some(status) = status {
                    return finish_renderer_output(
                        sandbox, options, started, status, &receiver, output,
                    );
                }
            }
            // The primary handle stays signaled after exit. Sleeping here also
            // paces observation when descendants have already closed the pipes.
            std::thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(20)),
            );
        }
    })();
    if result.is_err() {
        job.terminate_until(deadline);
    }
    result
}

type RendererPipeResult = (&'static str, Result<String, ConfinementError>);

fn check_renderer_deadline(
    deadline: Instant,
    options: BoundedProcessOptions,
) -> Result<(), ConfinementError> {
    if Instant::now() >= deadline {
        return Err(ConfinementError::Process(BoundedProcessError::Timeout {
            timeout_ms: options.timeout.as_millis(),
        }));
    }
    Ok(())
}

fn remaining_wait_millis(deadline: Instant) -> u32 {
    // u32::MAX means INFINITE in WaitForSingleObject, so never produce it.
    deadline
        .saturating_duration_since(Instant::now())
        .as_millis()
        .min(u128::from(u32::MAX - 1)) as u32
}

fn observe_scratch_until(
    sandbox: &RenderSandbox,
    deadline: Instant,
    options: BoundedProcessOptions,
) -> Result<(), ConfinementError> {
    check_renderer_deadline(deadline, options)?;
    let started = Instant::now();
    let budget = SCRATCH_SCAN_BUDGET.min(deadline.saturating_duration_since(started));
    let result = enforce_scratch_limit_with_budget(
        sandbox.scratch(),
        sandbox.limits().max_file_bytes,
        budget,
        &mut || started.elapsed(),
    );
    // Command timeout takes precedence when its deadline clipped the scan.
    check_renderer_deadline(deadline, options)?;
    result.map_err(ConfinementError::Resource)
}

// Called only after the entire Job is empty and the primary status is known.
// Complete pipes do not skip the final scan, and a final scan never gets a new
// command timeout. Once empty, no renderer process can race this observation.
fn finish_renderer_output(
    sandbox: &RenderSandbox,
    options: BoundedProcessOptions,
    started: Instant,
    status: std::process::ExitStatus,
    receiver: &mpsc::Receiver<RendererPipeResult>,
    mut output: [Option<String>; 2],
) -> Result<BoundedProcessOutput, ConfinementError> {
    let deadline = started
        .checked_add(options.timeout)
        .ok_or(ConfinementError::Process(
            BoundedProcessError::InvalidLifetime,
        ))?;
    observe_scratch_until(sandbox, deadline, options)?;
    while output.iter().any(Option::is_none) {
        check_renderer_deadline(deadline, options)?;
        match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok((stream, result)) => output[usize::from(stream == "stderr")] = Some(result?),
            Err(_) => {
                return Err(ConfinementError::Process(BoundedProcessError::Timeout {
                    timeout_ms: options.timeout.as_millis(),
                }));
            }
        }
    }
    check_renderer_deadline(deadline, options)?;
    Ok(BoundedProcessOutput {
        status,
        stdout: output[0].take().unwrap(),
        stderr: output[1].take().unwrap(),
        duration: started.elapsed(),
    })
}

const MAX_SCRATCH_SCAN_DEPTH: usize = 32;
const MAX_SCRATCH_SCAN_ENTRIES: usize = 10_000;
const SCRATCH_SCAN_BUDGET: Duration = Duration::from_secs(1);

// Metadata-only file observations remain compatible with active writers.
// Directory pins additionally request list access so their sharing restrictions
// participate in Windows share-access checks.
fn open_scratch_entry(
    path: &Path,
    access: u32,
    share: u32,
) -> Result<(OwnedHandle, BY_HANDLE_FILE_INFORMATION), std::io::Error> {
    let path = wide(path.as_os_str());
    let raw = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            share,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let handle = OwnedHandle(raw);
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(handle.0, &mut info) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(std::io::Error::other(
            "renderer scratch contains a reparse point",
        ));
    }
    if unsafe { GetFileType(handle.0) } != FILE_TYPE_DISK {
        return Err(std::io::Error::other(
            "renderer scratch contains a non-regular file",
        ));
    }
    Ok((handle, info))
}

fn pin_scratch_directory(
    path: &Path,
    observed: Option<&BY_HANDLE_FILE_INFORMATION>,
) -> Result<OwnedHandle, std::io::Error> {
    // Deny write and delete sharing while this directory and its descendants
    // are observed. Attribute access alone would not establish this pin.
    let (handle, info) = open_scratch_entry(
        path,
        FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ,
    )?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(std::io::Error::other(
            "renderer scratch directory changed type",
        ));
    }
    if observed.is_some_and(|previous| {
        previous.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
            || previous.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || previous.dwVolumeSerialNumber != info.dwVolumeSerialNumber
            || previous.nFileIndexHigh != info.nFileIndexHigh
            || previous.nFileIndexLow != info.nFileIndexLow
    }) {
        return Err(std::io::Error::other(
            "renderer scratch directory changed identity before pinning",
        ));
    }
    Ok(handle)
}

#[cfg(test)]
fn enforce_scratch_limit_with_clock(
    root: &Path,
    max_file_bytes: u64,
    elapsed: &mut impl FnMut() -> Duration,
) -> Result<(), std::io::Error> {
    enforce_scratch_limit_with_budget(root, max_file_bytes, SCRATCH_SCAN_BUDGET, elapsed)
}

fn enforce_scratch_limit_with_budget(
    root: &Path,
    max_file_bytes: u64,
    budget: Duration,
    elapsed: &mut impl FnMut() -> Duration,
) -> Result<(), std::io::Error> {
    fn check_deadline(
        budget: Duration,
        elapsed: &mut impl FnMut() -> Duration,
    ) -> Result<(), std::io::Error> {
        if elapsed() > budget {
            return Err(std::io::Error::other(
                "renderer scratch scan exceeded observation budget",
            ));
        }
        Ok(())
    }

    fn visit(
        path: &Path,
        observed: Option<&BY_HANDLE_FILE_INFORMATION>,
        depth: usize,
        max: u64,
        counts: (&mut u64, &mut usize),
        budget: Duration,
        elapsed: &mut impl FnMut() -> Duration,
    ) -> Result<(), std::io::Error> {
        let (total, entries) = counts;
        if depth > MAX_SCRATCH_SCAN_DEPTH {
            return Err(std::io::Error::other(
                "renderer scratch scan exceeded depth limit",
            ));
        }
        check_deadline(budget, elapsed)?;
        let _directory = pin_scratch_directory(path, observed)?;
        check_deadline(budget, elapsed)?;
        // read_dir streams entries. _directory and every caller's guard remain
        // alive throughout iteration, including while each child is visited.
        let mut directory = std::fs::read_dir(path)?;
        loop {
            check_deadline(budget, elapsed)?;
            let next = directory.next();
            check_deadline(budget, elapsed)?;
            let Some(entry) = next else { break };
            let entry = entry?;
            *entries = entries
                .checked_add(1)
                .ok_or_else(|| std::io::Error::other("renderer scratch entry count overflow"))?;
            if *entries > MAX_SCRATCH_SCAN_ENTRIES {
                return Err(std::io::Error::other(
                    "renderer scratch scan exceeded entry limit",
                ));
            }
            let child_path = entry.path();
            let (_entry, info) = match open_scratch_entry(
                &child_path,
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
            ) {
                Ok(entry) => entry,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            check_deadline(budget, elapsed)?;
            if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                // The metadata handle does not pin the name. Retain it and
                // require the stronger directory open to identify the same
                // object before resolving any descendants through its path.
                visit(
                    &child_path,
                    Some(&info),
                    depth + 1,
                    max,
                    (total, entries),
                    budget,
                    elapsed,
                )?;
            } else {
                let size = (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow);
                if size > max {
                    return Err(std::io::Error::other(
                        "renderer scratch file exceeds configured limit",
                    ));
                }
                *total = total
                    .checked_add(size)
                    .ok_or_else(|| std::io::Error::other("renderer scratch size overflow"))?;
                if *total > max {
                    return Err(std::io::Error::other(
                        "renderer scratch total exceeds configured limit",
                    ));
                }
            }
        }
        check_deadline(budget, elapsed)
    }
    check_deadline(budget, elapsed)?;
    visit(
        root,
        None,
        0,
        max_file_bytes,
        (&mut 0, &mut 0),
        budget,
        elapsed,
    )
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
        let memory_limit = usize::try_from(limits.max_aggregate_memory_bytes)
            .ok()
            .filter(|value| *value != 0)
            .ok_or_else(|| {
                ConfinementError::Resource(std::io::Error::other("invalid aggregate memory limit"))
            })?;
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
        info.JobMemoryLimit = memory_limit;
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
    fn active_processes(&self) -> Result<u32, std::io::Error> {
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        if unsafe {
            QueryInformationJobObject(
                self.0,
                JobObjectBasicAccountingInformation,
                (&raw mut info).cast(),
                std::mem::size_of_val(&info) as u32,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(info.ActiveProcesses)
    }

    fn terminate_until(&self, deadline: Instant) {
        // Keep the first execution error. Cleanup cannot extend the original
        // deadline; closing this Job remains the fallback if termination or
        // accounting fails, or the timeout has already exhausted that budget.
        if unsafe { TerminateJobObject(self.0, 1) } == 0 {
            return;
        }
        loop {
            match self.active_processes() {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            std::thread::sleep(remaining.min(Duration::from_millis(20)));
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
                    std::mem::size_of_val(&*attributes.handles),
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
    let mut entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: GENERIC_ALL,
        grfAccessMode: SET_ACCESS,
        grfInheritance: OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
        ..Default::default()
    };
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
fn renderer_current_directory(scratch: &Path) -> Vec<u16> {
    let canonical = wide(scratch.as_os_str());
    if !scratch.is_absolute()
        || !matches!(
            scratch.components().next(),
            Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::VerbatimDisk(_))
        )
    {
        return canonical;
    }
    // Only the process CWD spelling may change. Keep the canonical scratch
    // for grants and observation, and retain all other path namespaces.
    let ordinary = &canonical[4..];
    if ordinary.len() > MAX_PATH as usize {
        return canonical;
    }
    // Ask Win32 to interpret the ordinary spelling itself. In particular,
    // trailing dots/spaces and DOS device names must never redirect the CWD.
    let raw = unsafe {
        CreateFileW(
            ordinary.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return canonical;
    }
    let directory = OwnedHandle(raw);
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(directory.0, &mut info) } == 0
        || info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT)
            != FILE_ATTRIBUTE_DIRECTORY
    {
        return canonical;
    }
    let mut resolved = vec![0_u16; canonical.len()];
    let length = unsafe {
        GetFinalPathNameByHandleW(
            directory.0,
            resolved.as_mut_ptr(),
            resolved.len() as u32,
            FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
        )
    } as usize;
    if length == canonical.len() - 1 && resolved[..length] == canonical[..length] {
        ordinary.to_vec()
    } else {
        canonical
    }
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

#[cfg(test)]
mod scratch_scan_tests {

    #[test]
    fn renderer_cwd_ordinary_spelling_round_trips_to_canonical_unicode_directory() {
        use std::os::windows::ffi::OsStringExt;

        let holder = tempfile::tempdir().unwrap();
        let scratch = holder.path().join("描画 scratch");
        std::fs::create_dir(&scratch).unwrap();
        let canonical = std::fs::canonicalize(&scratch).unwrap();
        let cwd = renderer_current_directory(&canonical);
        assert_eq!(cwd.last(), Some(&0));
        let cwd = std::path::PathBuf::from(OsString::from_wide(&cwd[..cwd.len() - 1]));
        assert!(matches!(
            cwd.components().next(),
            Some(std::path::Component::Prefix(prefix))
                if matches!(prefix.kind(), std::path::Prefix::Disk(_))
        ));
        assert_eq!(std::fs::canonicalize(&cwd).unwrap(), canonical);
        assert_eq!(std::fs::canonicalize(&scratch).unwrap(), canonical);
    }

    #[test]
    fn renderer_cwd_keeps_verbatim_semantics_for_reserved_and_trailing_names() {
        let holder = tempfile::tempdir().unwrap();
        let canonical_holder = std::fs::canonicalize(holder.path()).unwrap();
        let ordinary = canonical_holder.join("semantic");
        std::fs::create_dir(&ordinary).unwrap();
        for name in ["semantic.", "semantic ", "NUL"] {
            let scratch = canonical_holder.join(name);
            std::fs::create_dir(&scratch).expect("create exact verbatim directory");
            let canonical = std::fs::canonicalize(&scratch).unwrap();
            assert_eq!(canonical.file_name(), Some(OsStr::new(name)));
            assert_eq!(
                renderer_current_directory(&canonical),
                wide(canonical.as_os_str()),
                "ordinary CWD spelling must not redirect {name:?}"
            );
            assert_eq!(std::fs::canonicalize(&scratch).unwrap(), canonical);
            std::fs::remove_dir(&canonical).unwrap();
        }
        assert!(std::fs::read_dir(ordinary).unwrap().next().is_none());
    }
    use super::*;

    fn scan(root: &Path, max: u64) -> Result<(), std::io::Error> {
        enforce_scratch_limit_with_clock(root, max, &mut || Duration::ZERO)
    }

    fn final_scan_sandbox(root: &Path, max: u64) -> RenderSandbox {
        RenderSandbox::new(
            &std::env::current_exe().unwrap(),
            root,
            [],
            crate::confinement::RenderResourceLimits {
                max_file_bytes: max,
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn completed_output_still_requires_a_final_scratch_scan() {
        let root = tempfile::tempdir().unwrap();
        let sandbox = final_scan_sandbox(root.path(), 4);
        // The earlier periodic observation succeeds. A subsequently completed
        // renderer can still leave output that the completion scan must reject.
        scan(root.path(), 4).unwrap();
        std::fs::write(root.path().join("late-output"), b"12345").unwrap();
        let (_sender, receiver) = mpsc::channel();
        let error = finish_renderer_output(
            &sandbox,
            BoundedProcessOptions::default(),
            Instant::now(),
            std::os::windows::process::ExitStatusExt::from_raw(0),
            &receiver,
            [Some(String::new()), Some(String::new())],
        )
        .unwrap_err();
        assert!(matches!(error, ConfinementError::Resource(ref cause)
            if cause.to_string().contains("file exceeds")));
    }

    #[test]
    fn final_scratch_scan_preserves_primary_status_and_output() {
        let root = tempfile::tempdir().unwrap();
        let sandbox = final_scan_sandbox(root.path(), 4);
        std::fs::write(root.path().join("output"), b"1234").unwrap();
        let (_sender, receiver) = mpsc::channel();
        let result = finish_renderer_output(
            &sandbox,
            BoundedProcessOptions::default(),
            Instant::now(),
            std::os::windows::process::ExitStatusExt::from_raw(7),
            &receiver,
            [Some("stdout".to_owned()), Some("stderr".to_owned())],
        )
        .unwrap();
        assert_eq!(result.status.code(), Some(7));
        assert_eq!(result.stdout, "stdout");
        assert_eq!(result.stderr, "stderr");
    }

    #[test]
    fn completed_output_does_not_reset_the_original_deadline() {
        let root = tempfile::tempdir().unwrap();
        let sandbox = final_scan_sandbox(root.path(), 0);
        let (_sender, receiver) = mpsc::channel();
        let options = BoundedProcessOptions {
            timeout: Duration::ZERO,
            ..Default::default()
        };
        let error = finish_renderer_output(
            &sandbox,
            options,
            Instant::now(),
            std::os::windows::process::ExitStatusExt::from_raw(0),
            &receiver,
            [Some(String::new()), Some(String::new())],
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ConfinementError::Process(BoundedProcessError::Timeout { .. })
        ));
    }

    #[test]
    fn scratch_scan_honors_a_shorter_remaining_command_budget() {
        let root = tempfile::tempdir().unwrap();
        let budget = Duration::from_millis(5);
        enforce_scratch_limit_with_budget(root.path(), 0, budget, &mut || budget).unwrap();
        let error = enforce_scratch_limit_with_budget(root.path(), 0, budget, &mut || {
            budget + Duration::from_nanos(1)
        })
        .unwrap_err();
        assert!(error.to_string().contains("observation budget"));
    }

    #[test]
    fn scratch_entry_limit_counts_empty_files_and_directories() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..MAX_SCRATCH_SCAN_ENTRIES {
            let path = root.path().join(index.to_string());
            if index % 2 == 0 {
                File::create(path).unwrap();
            } else {
                std::fs::create_dir(path).unwrap();
            }
        }
        scan(root.path(), 0).unwrap();
        File::create(root.path().join("over-limit")).unwrap();
        assert!(
            scan(root.path(), 0)
                .unwrap_err()
                .to_string()
                .contains("entry limit")
        );
    }

    #[test]
    fn scratch_depth_limit_accepts_exact_boundary() {
        let root = tempfile::tempdir().unwrap();
        let mut path = root.path().to_path_buf();
        for _ in 0..MAX_SCRATCH_SCAN_DEPTH {
            path.push("d");
            std::fs::create_dir(&path).unwrap();
        }
        scan(root.path(), 0).unwrap();
        std::fs::create_dir(path.join("d")).unwrap();
        assert!(
            scan(root.path(), 0)
                .unwrap_err()
                .to_string()
                .contains("depth limit")
        );
    }

    #[test]
    fn scratch_byte_limits_cover_single_file_and_total() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a"), b"1234").unwrap();
        scan(root.path(), 4).unwrap();
        assert!(
            scan(root.path(), 3)
                .unwrap_err()
                .to_string()
                .contains("file exceeds")
        );
        std::fs::write(root.path().join("b"), b"1").unwrap();
        assert!(
            scan(root.path(), 4)
                .unwrap_err()
                .to_string()
                .contains("total exceeds")
        );
        scan(root.path(), 5).unwrap();
    }

    #[test]
    fn scratch_observation_allows_an_active_file_writer() {
        let root = tempfile::tempdir().unwrap();
        let _writer = File::create(root.path().join("output")).unwrap();
        scan(root.path(), 0).unwrap();
    }

    #[test]
    fn scratch_observation_deadline_is_deterministic() {
        let root = tempfile::tempdir().unwrap();
        enforce_scratch_limit_with_clock(root.path(), 0, &mut || SCRATCH_SCAN_BUDGET).unwrap();
        let mut ticks = 0;
        let error = enforce_scratch_limit_with_clock(root.path(), 0, &mut || {
            ticks += 1;
            if ticks < 5 {
                Duration::ZERO
            } else {
                SCRATCH_SCAN_BUDGET + Duration::from_nanos(1)
            }
        })
        .unwrap_err();
        assert!(error.to_string().contains("observation budget"));
        assert_eq!(ticks, 5);
    }

    #[test]
    fn scratch_directory_pin_rejects_a_replaced_observed_directory() {
        let parent = tempfile::tempdir().unwrap();
        let child = parent.path().join("child");
        std::fs::create_dir(&child).unwrap();
        let (_metadata, observed) = open_scratch_entry(
            &child,
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
        )
        .unwrap();
        drop(pin_scratch_directory(&child, Some(&observed)).unwrap());
        // Attribute-only observations permit a name swap. A later strong open
        // must reject the replacement even though it is also a regular directory.
        std::fs::rename(&child, parent.path().join("moved-child")).unwrap();
        std::fs::create_dir(&child).unwrap();
        let error = pin_scratch_directory(&child, Some(&observed))
            .err()
            .expect("replacement must not inherit the observed directory identity");
        assert!(error.to_string().contains("changed identity"));
        // The rejected strong open releases its handle on the error path.
        std::fs::rename(&child, parent.path().join("replacement")).unwrap();
    }

    #[test]
    fn scratch_pinned_ancestors_block_directory_swap() {
        use std::os::windows::fs::OpenOptionsExt;

        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("scratch");
        let child = root.join("child");
        std::fs::create_dir_all(&child).unwrap();
        let mut ticks = 0;
        let mut attempted_swap = false;
        enforce_scratch_limit_with_clock(&root, 0, &mut || {
            ticks += 1;
            // Root and child have both acquired their retained directory
            // guards at this checkpoint, before the child's read_dir call.
            if ticks == 8 {
                attempted_swap = true;
                assert!(std::fs::rename(&root, parent.path().join("moved-root")).is_err());
                assert!(std::fs::rename(&child, root.join("moved-child")).is_err());
                assert!(std::fs::remove_dir(&child).is_err());
                // FSCTL_SET_REPARSE_POINT needs a write-capable handle. The
                // guard must also prevent acquiring that access in place.
                assert!(
                    std::fs::OpenOptions::new()
                        .access_mode(windows_sys::Win32::Foundation::GENERIC_WRITE)
                        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                        .open(&child)
                        .is_err()
                );
            }
            Duration::ZERO
        })
        .unwrap();
        assert!(attempted_swap);
        // RAII releases every guard after the observation.
        std::fs::rename(&root, parent.path().join("moved-root")).unwrap();
    }

    #[test]
    fn scratch_rejects_junction_root_and_child() {
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("outside");
        let root = parent.path().join("scratch");
        std::fs::create_dir(&target).unwrap();
        std::fs::create_dir(&root).unwrap();
        let junction = root.join("junction");
        let result = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "junction creation failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            scan(&junction, 0)
                .unwrap_err()
                .to_string()
                .contains("reparse point")
        );
        assert!(
            scan(&root, 0)
                .unwrap_err()
                .to_string()
                .contains("reparse point")
        );
        std::fs::remove_dir(junction).unwrap();
    }
}
