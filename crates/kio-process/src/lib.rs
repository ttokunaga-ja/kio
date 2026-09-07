//! Bounded execution for trusted local helpers.
//!
//! This is deliberately a resource and child-tree boundary, not an operating
//! system sandbox. Callers that run a renderer or other untrusted input must
//! add a platform confinement wrapper before describing that operation as
//! network-isolated.

pub mod confinement;
#[cfg(windows)]
mod confinement_windows;

use std::{
    io::{Read, Write},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use thiserror::Error;

pub const DEFAULT_PROCESS_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_PROCESS_OUTPUT_LIMIT: usize = 1024 * 1024;

/// Limits applied to bounded subprocesses.  Both output limits are measured
/// in bytes before UTF-8 decoding, so a malicious process cannot make decoding
/// itself an unbounded allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundedProcessOptions {
    pub timeout: Duration,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
}

/// Owned input for a bounded subprocess.  The byte cap is checked
/// before the child is spawned, and the bytes are written under the same
/// deadline as process creation, output collection, waiting, and cleanup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedStdin {
    bytes: Vec<u8>,
    max_bytes: usize,
}

impl BoundedStdin {
    #[must_use]
    pub fn new(bytes: Vec<u8>, max_bytes: usize) -> Self {
        Self { bytes, max_bytes }
    }
}

impl Default for BoundedProcessOptions {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_PROCESS_TIMEOUT,
            max_stdout_bytes: DEFAULT_PROCESS_OUTPUT_LIMIT,
            max_stderr_bytes: DEFAULT_PROCESS_OUTPUT_LIMIT,
        }
    }
}

/// A fully collected subprocess result. Output is strictly decoded as UTF-8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedProcessOutput {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
    pub duration: Duration,
}

#[derive(Debug, Error)]
pub enum BoundedProcessError {
    #[error("could not start bounded subprocess: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("could not configure bounded subprocess isolation: {0}")]
    Isolation(#[source] std::io::Error),
    #[error("bounded subprocess stdin exceeds input limit of {limit} bytes")]
    InputLimit { limit: usize },
    #[error("could not write bounded subprocess stdin: {0}")]
    Write(#[source] std::io::Error),
    #[error("could not read bounded subprocess {stream}: {source}")]
    Read {
        stream: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("could not wait for bounded subprocess: {0}")]
    Wait(#[source] std::io::Error),
    #[error("bounded subprocess exceeded timeout of {timeout_ms} ms")]
    Timeout { timeout_ms: u128 },
    #[error("bounded subprocess {stream} exceeded output limit of {limit} bytes")]
    OutputLimit { stream: &'static str, limit: usize },
    #[error("could not observe bounded macOS subprocess physical memory: {0}")]
    PhysicalMemory(#[source] std::io::Error),
    #[error(
        "bounded macOS subprocess exceeded observed physical-memory limit of {limit} bytes (observed {observed} bytes)"
    )]
    PhysicalMemoryLimit { limit: u64, observed: u64 },
    #[error("bounded subprocess emitted non-UTF-8 {stream}")]
    NonUtf8 { stream: &'static str },
}

enum StreamEvent {
    Data(&'static str, Vec<u8>),
    End,
    ReadError(&'static str, std::io::Error),
    OutputLimit(&'static str, usize),
}

fn record_stream_event(
    event: StreamEvent,
    stdout: &mut Option<Vec<u8>>,
    stderr: &mut Option<Vec<u8>>,
    finished_streams: &mut usize,
) -> Result<(), BoundedProcessError> {
    match event {
        StreamEvent::Data("stdout", bytes) => *stdout = Some(bytes),
        StreamEvent::Data("stderr", bytes) => *stderr = Some(bytes),
        StreamEvent::Data(_, _) => unreachable!("only stdout and stderr are configured"),
        StreamEvent::End => *finished_streams += 1,
        StreamEvent::ReadError(stream, source) => {
            return Err(BoundedProcessError::Read { stream, source });
        }
        StreamEvent::OutputLimit(stream, limit) => {
            return Err(BoundedProcessError::OutputLimit { stream, limit });
        }
    }
    Ok(())
}

#[cfg(unix)]
fn read_stream<R: Read + std::os::fd::AsRawFd + Send + 'static>(
    mut reader: R,
    stream: &'static str,
    limit: usize,
    sender: mpsc::Sender<StreamEvent>,
    cancelled: Arc<AtomicBool>,
) {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        let mut descriptor = libc::pollfd {
            fd: reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // Poll rather than blocking in `read` so cancellation can also release
        // readers whose pipe write end escaped the bounded process group.
        let polled = unsafe { libc::poll(&mut descriptor, 1, 10) };
        if polled == 0 {
            continue;
        }
        if polled < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            let _ = sender.send(StreamEvent::ReadError(stream, error));
            return;
        }
        match reader.read(&mut chunk) {
            Ok(0) => {
                let _ = sender.send(StreamEvent::Data(stream, bytes));
                let _ = sender.send(StreamEvent::End);
                return;
            }
            Ok(count) => {
                let Some(total) = bytes.len().checked_add(count) else {
                    let _ = sender.send(StreamEvent::OutputLimit(stream, limit));
                    return;
                };
                if total > limit {
                    let _ = sender.send(StreamEvent::OutputLimit(stream, limit));
                    return;
                }
                bytes.extend_from_slice(&chunk[..count]);
            }
            Err(error) => {
                let _ = sender.send(StreamEvent::ReadError(stream, error));
                return;
            }
        }
    }
}

// Windows kills the complete Job Object on cancellation, closing inherited
// output handles. Keep the platform's existing blocking reader: it avoids
// changing its pipe semantics while the Job Object provides the cancellation
// boundary that Unix process groups cannot enforce after `setsid`.
#[cfg(not(unix))]
fn read_stream<R: Read + Send + 'static>(
    mut reader: R,
    stream: &'static str,
    limit: usize,
    sender: mpsc::Sender<StreamEvent>,
    _cancelled: Arc<AtomicBool>,
) {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => {
                let _ = sender.send(StreamEvent::Data(stream, bytes));
                let _ = sender.send(StreamEvent::End);
                return;
            }
            Ok(count) => {
                let Some(total) = bytes.len().checked_add(count) else {
                    let _ = sender.send(StreamEvent::OutputLimit(stream, limit));
                    return;
                };
                if total > limit {
                    let _ = sender.send(StreamEvent::OutputLimit(stream, limit));
                    return;
                }
                bytes.extend_from_slice(&chunk[..count]);
            }
            Err(error) => {
                let _ = sender.send(StreamEvent::ReadError(stream, error));
                return;
            }
        }
    }
}

fn terminate_process_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        // A direct child can call setsid before cancellation, at which point
        // it is no longer in the process group created for it. Kill and reap
        // that known child independently before best-effort group cleanup.
        let _ = child.kill();
        unsafe {
            // `configure_process_isolation` makes the child the process-group leader;
            // a negative PID targets every descendant in that group.
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
}

#[cfg(unix)]
fn configure_process_isolation(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
}

#[cfg(windows)]
fn configure_process_isolation(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;

    command.creation_flags(CREATE_SUSPENDED);
}

#[cfg(all(not(unix), not(windows)))]
fn configure_process_isolation(_command: &mut Command) {}

#[cfg(windows)]
struct WindowsJob(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl WindowsJob {
    fn create() -> Result<Self, std::io::Error> {
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::JobObjects::{
                CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
        };

        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                std::mem::size_of_val(&limits) as u32,
            ) == 0
            {
                let error = std::io::Error::last_os_error();
                CloseHandle(handle);
                return Err(error);
            }
            Ok(Self(handle))
        }
    }

    fn attach(&self, child: &Child) -> Result<(), std::io::Error> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

        if unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle() as _) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn terminate(&self) {
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0, 1);
        }
    }
}

#[cfg(windows)]
impl Drop for WindowsJob {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
fn resume_suspended_process(child: &Child) -> Result<(), std::io::Error> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                Thread32Next,
            },
            Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
        },
    };

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let result = (|| {
            let mut entry = THREADENTRY32 {
                dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
                ..Default::default()
            };
            if Thread32First(snapshot, &mut entry) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut owner_thread = None;
            loop {
                if entry.th32OwnerProcessID == child.id() {
                    if owner_thread.replace(entry.th32ThreadID).is_some() {
                        return Err(std::io::Error::other(
                            "suspended bounded process created more than one thread before isolation",
                        ));
                    }
                }
                entry = THREADENTRY32 {
                    dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
                    ..Default::default()
                };
                if Thread32Next(snapshot, &mut entry) == 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                        break;
                    }
                    return Err(error);
                }
            }
            let owner_thread = owner_thread.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "could not find suspended bounded process owner thread",
                )
            })?;
            let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, owner_thread);
            if thread.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let previous_count = ResumeThread(thread);
            let resume_error = if previous_count == u32::MAX {
                Some(std::io::Error::last_os_error())
            } else if previous_count != 1 {
                Some(std::io::Error::other(
                    "bounded process owner thread was not suspended exactly once",
                ))
            } else {
                None
            };
            CloseHandle(thread);
            resume_error.map_or(Ok(()), Err)
        })();
        CloseHandle(snapshot);
        result
    }
}

#[cfg(windows)]
thread_local! {
    static WINDOWS_PRE_ATTACH_DELAY: std::cell::Cell<Option<Duration>> = const {
        std::cell::Cell::new(None)
    };
}

// This is public only because `kio-eval` retains the pre-existing Windows
// regression that proves attachment happens before execution. It is hidden
// from normal API documentation and has no production caller.
#[cfg(windows)]
#[doc(hidden)]
pub fn replace_windows_pre_attach_delay_for_test(delay: Option<Duration>) -> Option<Duration> {
    WINDOWS_PRE_ATTACH_DELAY.with(|slot| slot.replace(delay))
}

#[cfg(windows)]
fn delay_before_windows_job_attach_for_test() {
    WINDOWS_PRE_ATTACH_DELAY.with(|delay| {
        if let Some(delay) = delay.get() {
            thread::sleep(delay);
        }
    });
}

#[cfg(not(windows))]
fn attach_process_tree(_child: &Child) -> Result<Option<()>, std::io::Error> {
    Ok(None)
}

#[cfg(windows)]
fn terminate_attached_tree(job: &Option<WindowsJob>) {
    if let Some(job) = job {
        job.terminate();
    }
}

#[cfg(not(windows))]
fn terminate_attached_tree(_job: &Option<()>) {}

#[cfg(windows)]
fn close_attached_tree_before_join(job: &mut Option<WindowsJob>) {
    drop(job.take());
}

#[cfg(not(windows))]
fn close_attached_tree_before_join(_job: &mut Option<()>) {}

/// Run a trusted Kio-under-test command under evaluator resource bounds.
///
/// On Unix the child gets its own process group; on Windows it starts
/// suspended, joins a kill-on-close Job Object, and is resumed only after that
/// isolation succeeds. A timeout, output overflow, or stream failure
/// kills the ordinary child tree before returning. This is a guard against
/// product bugs, not an operating-system sandbox for a hostile executable.
/// Unix descendants that deliberately create a new session are outside the
/// process-tree termination guarantee; cancellation still stops the bounded
/// I/O workers and returns without waiting for their inherited pipe handles.
pub fn run_bounded_command(
    command: &mut Command,
    options: BoundedProcessOptions,
    stdin: Option<BoundedStdin>,
) -> Result<BoundedProcessOutput, BoundedProcessError> {
    #[cfg(target_os = "macos")]
    return run_bounded_command_inner(command, options, stdin, None);
    #[cfg(not(target_os = "macos"))]
    run_bounded_command_inner(command, options, stdin)
}

/// Run a macOS renderer with a periodically observed physical-footprint cap.
/// This is intentionally crate-private: callers use `RenderSandbox`, which
/// also denies `process-fork` so the direct-child observation covers the
/// renderer lifecycle.
#[cfg(target_os = "macos")]
pub(crate) fn run_bounded_command_with_macos_physical_memory_limit(
    command: &mut Command,
    options: BoundedProcessOptions,
    stdin: Option<BoundedStdin>,
    limit: u64,
) -> Result<BoundedProcessOutput, BoundedProcessError> {
    let monitor = (limit != 0).then(|| MacosPhysicalMemoryMonitor::new(limit));
    run_bounded_command_inner(command, options, stdin, monitor)
}

#[cfg(target_os = "macos")]
fn run_bounded_command_inner(
    command: &mut Command,
    options: BoundedProcessOptions,
    stdin: Option<BoundedStdin>,
    mut physical_memory_monitor: Option<MacosPhysicalMemoryMonitor>,
) -> Result<BoundedProcessOutput, BoundedProcessError> {
    run_bounded_command_inner_impl(command, options, stdin, &mut physical_memory_monitor)
}

#[cfg(not(target_os = "macos"))]
fn run_bounded_command_inner(
    command: &mut Command,
    options: BoundedProcessOptions,
    stdin: Option<BoundedStdin>,
) -> Result<BoundedProcessOutput, BoundedProcessError> {
    run_bounded_command_inner_impl(command, options, stdin, &mut ())
}

trait BoundedProcessMonitor {
    fn observe(&mut self, child: &Child) -> Result<(), BoundedProcessError>;
}

impl BoundedProcessMonitor for () {
    fn observe(&mut self, _child: &Child) -> Result<(), BoundedProcessError> {
        Ok(())
    }
}

#[cfg(target_os = "macos")]
struct MacosPhysicalMemoryMonitor {
    limit: u64,
    next_observation: Instant,
}

#[cfg(target_os = "macos")]
impl MacosPhysicalMemoryMonitor {
    fn new(limit: u64) -> Self {
        Self {
            limit,
            // The child needs a brief opportunity to exec the renderer before
            // the first sample. Subsequent samples are bounded to this same
            // short interval.
            next_observation: Instant::now() + Duration::from_millis(50),
        }
    }
}

#[cfg(target_os = "macos")]
impl BoundedProcessMonitor for Option<MacosPhysicalMemoryMonitor> {
    fn observe(&mut self, child: &Child) -> Result<(), BoundedProcessError> {
        let Some(monitor) = self.as_mut() else {
            return Ok(());
        };
        let now = Instant::now();
        if now < monitor.next_observation {
            return Ok(());
        }
        monitor.next_observation = now + Duration::from_millis(50);
        match macos_physical_footprint(child.id()) {
            Ok(observed) if observed > monitor.limit => {
                Err(BoundedProcessError::PhysicalMemoryLimit {
                    limit: monitor.limit,
                    observed,
                })
            }
            Ok(_) => Ok(()),
            // A concurrent exit is observed by try_wait below; it is not a
            // monitoring failure and must not mask the child's exit status.
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
            Err(error) => Err(BoundedProcessError::PhysicalMemory(error)),
        }
    }
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct RusageInfoV0 {
    uuid: [u8; 16],
    user_time: u64,
    system_time: u64,
    pkg_idle_wakeups: u64,
    interrupt_wakeups: u64,
    pageins: u64,
    wired_size: u64,
    resident_size: u64,
    physical_footprint: u64,
    proc_start_abstime: u64,
    proc_exit_abstime: u64,
}

#[cfg(target_os = "macos")]
fn macos_physical_footprint(pid: u32) -> Result<u64, std::io::Error> {
    #[link(name = "proc")]
    unsafe extern "C" {
        fn proc_pid_rusage(
            pid: libc::pid_t,
            flavor: libc::c_int,
            buffer: *mut libc::c_void,
        ) -> libc::c_int;
    }
    const RUSAGE_INFO_V0: libc::c_int = 0;
    let mut usage = std::mem::MaybeUninit::<RusageInfoV0>::zeroed();
    if unsafe {
        proc_pid_rusage(
            pid as libc::pid_t,
            RUSAGE_INFO_V0,
            usage.as_mut_ptr().cast::<libc::c_void>(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { usage.assume_init() }.physical_footprint)
}

fn run_bounded_command_inner_impl<M: BoundedProcessMonitor>(
    command: &mut Command,
    options: BoundedProcessOptions,
    stdin: Option<BoundedStdin>,
    monitor: &mut M,
) -> Result<BoundedProcessOutput, BoundedProcessError> {
    if let Some(input) = stdin.as_ref()
        && input.bytes.len() > input.max_bytes
    {
        return Err(BoundedProcessError::InputLimit {
            limit: input.max_bytes,
        });
    }
    configure_process_isolation(command);
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    // This is deliberately before spawn: one monotonic deadline accounts for
    // all work within this boundary, including a slow spawn and cleanup.
    let started = Instant::now();
    #[cfg(windows)]
    let mut process_tree = Some(WindowsJob::create().map_err(BoundedProcessError::Isolation)?);
    let mut child = command.spawn().map_err(BoundedProcessError::Spawn)?;
    #[cfg(windows)]
    let isolation = {
        delay_before_windows_job_attach_for_test();
        process_tree
            .as_ref()
            .expect("Windows Job Object was created before spawning evaluator")
            .attach(&child)
            .and_then(|()| resume_suspended_process(&child))
    };
    #[cfg(windows)]
    if let Err(error) = isolation {
        terminate_attached_tree(&process_tree);
        terminate_process_tree(&mut child);
        close_attached_tree_before_join(&mut process_tree);
        let _ = child.wait();
        return Err(BoundedProcessError::Isolation(error));
    }
    #[cfg(not(windows))]
    let mut process_tree = match attach_process_tree(&child) {
        Ok(process_tree) => process_tree,
        Err(error) => {
            terminate_process_tree(&mut child);
            let _ = child.wait();
            return Err(BoundedProcessError::Isolation(error));
        }
    };
    let stdout = child.stdout.take().expect("stdout was configured as piped");
    let stderr = child.stderr.take().expect("stderr was configured as piped");
    #[cfg(unix)]
    let mut child_stdin = child.stdin.take();
    #[cfg(unix)]
    let mut stdin_offset = 0_usize;
    #[cfg(unix)]
    if let Some(handle) = child_stdin.as_ref() {
        use std::os::fd::AsRawFd;
        let flags = unsafe { libc::fcntl(handle.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(handle.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                < 0
        {
            let error = std::io::Error::last_os_error();
            terminate_attached_tree(&process_tree);
            terminate_process_tree(&mut child);
            let _ = child.wait();
            return Err(BoundedProcessError::Write(error));
        }
    }
    #[cfg(not(unix))]
    let stdin_writer = stdin.map(|input| {
        let mut handle = child.stdin.take().expect("stdin was configured as piped");
        thread::spawn(move || handle.write_all(&input.bytes))
    });
    let (sender, receiver) = mpsc::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let stdout_reader = thread::spawn({
        let sender = sender.clone();
        let cancelled = Arc::clone(&cancelled);
        move || {
            read_stream(
                stdout,
                "stdout",
                options.max_stdout_bytes,
                sender,
                cancelled,
            )
        }
    });
    let stderr_reader = thread::spawn({
        let cancelled = Arc::clone(&cancelled);
        move || {
            read_stream(
                stderr,
                "stderr",
                options.max_stderr_bytes,
                sender,
                cancelled,
            )
        }
    });

    let deadline = started + options.timeout;
    let mut status = None;
    let mut stdout = None;
    let mut stderr = None;
    let mut finished_streams = 0;
    #[allow(unused_labels)]
    let result = 'run: loop {
        // Once try_wait has produced a status, the PID is reaped and must not
        // be sampled again: a later observation could address a recycled PID.
        // Route monitor failures through the shared cancellation/reap path.
        if status.is_none()
            && let Err(error) = monitor.observe(&child)
        {
            break 'run Err(error);
        }
        #[cfg(unix)]
        let mut stdin_wait_pending = false;
        #[cfg(unix)]
        let mut stdin_burst = 0_usize;
        #[cfg(unix)]
        if let (Some(input), Some(handle)) = (stdin.as_ref(), child_stdin.as_mut()) {
            while stdin_offset < input.bytes.len() {
                if Instant::now() >= deadline {
                    break 'run Err(BoundedProcessError::Timeout {
                        timeout_ms: options.timeout.as_millis(),
                    });
                }
                use std::os::fd::AsRawFd;
                let mut descriptor = libc::pollfd {
                    fd: handle.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let polled = unsafe { libc::poll(&mut descriptor, 1, 0) };
                if polled < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() != std::io::ErrorKind::Interrupted {
                        break 'run Err(BoundedProcessError::Write(error));
                    }
                    continue;
                }
                if polled == 0 {
                    stdin_wait_pending = true;
                    break;
                }
                if descriptor.revents
                    & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)
                    != 0
                {
                    match handle.write(&input.bytes[stdin_offset..]) {
                        Ok(0) => {
                            break 'run Err(BoundedProcessError::Write(std::io::Error::new(
                                std::io::ErrorKind::WriteZero,
                                "bounded subprocess stdin accepted no bytes",
                            )));
                        }
                        Ok(count) => {
                            stdin_offset += count;
                            stdin_burst += count;
                            // A continuously writable pipe must not starve
                            // output-limit/read events. Yield after a bounded
                            // burst, but use readiness polling below so this
                            // is not the former fixed-delay write throttle.
                            if stdin_burst >= 1024 * 1024 {
                                stdin_wait_pending = true;
                                break;
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            stdin_wait_pending = true;
                            break;
                        }
                        Err(error) => break 'run Err(BoundedProcessError::Write(error)),
                    }
                } else {
                    stdin_wait_pending = true;
                    break;
                }
            }
            if stdin_offset == input.bytes.len() {
                // EOF is part of the protocol, but never wait on it outside
                // the shared deadline.
                stdin_wait_pending = false;
                child_stdin.take();
            }
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(next) => status = next,
                Err(error) => break Err(BoundedProcessError::Wait(error)),
            }
        }
        #[cfg(unix)]
        let stdin_complete = stdin
            .as_ref()
            .is_none_or(|input| stdin_offset == input.bytes.len());
        #[cfg(not(unix))]
        let stdin_complete = stdin_writer
            .as_ref()
            .is_none_or(|writer| writer.is_finished());
        if status.is_some() && !stdin_complete {
            break Err(BoundedProcessError::Write(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "bounded subprocess exited before consuming bounded stdin",
            )));
        }
        if status.is_some() && finished_streams == 2 && stdin_complete {
            break Ok(());
        }
        let now = Instant::now();
        if now >= deadline {
            break Err(BoundedProcessError::Timeout {
                timeout_ms: options.timeout.as_millis(),
            });
        }
        #[cfg(unix)]
        if stdin_wait_pending {
            use std::os::fd::AsRawFd;
            let mut descriptor = libc::pollfd {
                fd: child_stdin
                    .as_ref()
                    .expect("stdin remains open while a write is pending")
                    .as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            // Waiting on the writable pipe prevents a fixed 10 ms delay per
            // write phase. Output events are still consumed below when already
            // available; otherwise the next status/output poll remains bounded
            // to the same short interval and shared deadline.
            let wait = (deadline - now).min(Duration::from_millis(10));
            let wait_ms = wait
                .as_millis()
                .clamp(1, i32::MAX as u128)
                .try_into()
                .expect("clamped poll timeout fits i32");
            let polled = unsafe { libc::poll(&mut descriptor, 1, wait_ms) };
            if polled < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    break Err(BoundedProcessError::Write(error));
                }
            }
            match receiver.try_recv() {
                Ok(event) => {
                    if let Err(error) =
                        record_stream_event(event, &mut stdout, &mut stderr, &mut finished_streams)
                    {
                        break Err(error);
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) if finished_streams == 2 => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    break Err(BoundedProcessError::Read {
                        stream: "output",
                        source: std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "output reader disconnected",
                        ),
                    });
                }
            }
            continue;
        }
        match receiver.recv_timeout((deadline - now).min(Duration::from_millis(10))) {
            Ok(event) => {
                if let Err(error) =
                    record_stream_event(event, &mut stdout, &mut stderr, &mut finished_streams)
                {
                    break Err(error);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) if finished_streams == 2 => {
                // EOF on both output pipes is not process completion. A child
                // can close stdout/stderr and continue running, so keep
                // polling its status under the same deadline instead of
                // falling through to an unbounded `wait` below.
                thread::sleep((deadline - now).min(Duration::from_millis(10)));
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Err(BoundedProcessError::Read {
                    stream: "output",
                    source: std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "output reader disconnected",
                    ),
                });
            }
        }
    };
    if result.is_err() {
        // Readers wake within their short poll interval even when a descendant
        // has escaped the Unix process group and retains a pipe write end.
        cancelled.store(true, Ordering::Release);
        terminate_attached_tree(&process_tree);
        terminate_process_tree(&mut child);
        // On Windows, dropping the Job Object is the second, independent
        // kill-on-close boundary. Do it before joining pipe workers so a
        // failed explicit termination call cannot leave inherited handles
        // keeping those workers blocked.
        close_attached_tree_before_join(&mut process_tree);
    }
    let waited = child.wait().map_err(BoundedProcessError::Wait);
    #[cfg(not(unix))]
    let stdin_result = stdin_writer.map(|writer| {
        writer
            .join()
            .map_err(|_| {
                BoundedProcessError::Write(std::io::Error::other("stdin writer panicked"))
            })?
            .map_err(BoundedProcessError::Write)
    });
    let _ = stdout_reader.join();
    let _ = stderr_reader.join();
    result?;
    #[cfg(not(unix))]
    if let Some(result) = stdin_result {
        result?;
    }
    let status = waited?;
    let stdout = String::from_utf8(stdout.expect("stdout reader completed"))
        .map_err(|_| BoundedProcessError::NonUtf8 { stream: "stdout" })?;
    let stderr = String::from_utf8(stderr.expect("stderr reader completed"))
        .map_err(|_| BoundedProcessError::NonUtf8 { stream: "stderr" })?;
    Ok(BoundedProcessOutput {
        status,
        stdout,
        stderr,
        duration: started.elapsed(),
    })
}
