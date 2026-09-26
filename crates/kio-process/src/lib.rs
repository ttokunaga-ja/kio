//! Bounded execution for trusted local helpers.
//!
//! This is deliberately a resource and child-tree boundary, not an operating
//! system sandbox. Callers that run a renderer or other untrusted input must
//! add a platform confinement wrapper before describing that operation as
//! network-isolated.

pub mod confinement;
#[cfg(windows)]
mod confinement_windows;

#[cfg(unix)]
use std::fs::File;
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
    #[error("supervised child lifetime must be nonzero and representable by Instant")]
    InvalidLifetime,
    #[error("bounded subprocess {stream} exceeded output limit of {limit} bytes")]
    OutputLimit { stream: &'static str, limit: usize },
    #[error("could not observe bounded macOS subprocess physical memory: {0}")]
    PhysicalMemory(#[source] std::io::Error),
    #[error(
        "bounded macOS subprocess exceeded observed physical-memory limit of {limit} bytes (observed {observed} bytes)"
    )]
    PhysicalMemoryLimit { limit: u64, observed: u64 },
    #[error(
        "bounded renderer scratch exceeded sampled aggregate file limit of {limit} bytes (observed {observed} bytes)"
    )]
    ScratchLimit { limit: u64, observed: u64 },
    #[error("could not inspect bounded renderer scratch: {0}")]
    ScratchInspection(#[source] std::io::Error),
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

/// A trusted long-running child with a process-tree lifetime boundary.
///
/// Unlike [`run_bounded_command`], this intentionally discards every standard
/// stream. It is for callers that need to poll a tunnel or watcher while
/// retaining a deadline and cleanup authority over its descendants. The
/// deadline is enforced whenever [`Self::try_wait`] is polled and again when
/// the handle is dropped; callers must keep polling while the child is live.
pub struct SupervisedChild {
    child: Child,
    deadline: Instant,
    max_lifetime: Duration,
    terminal_status: Option<ExitStatus>,
    #[cfg(windows)]
    job: WindowsJob,
}

impl SupervisedChild {
    pub fn spawn(
        command: &mut Command,
        max_lifetime: Duration,
    ) -> Result<Self, BoundedProcessError> {
        if max_lifetime.is_zero() {
            return Err(BoundedProcessError::InvalidLifetime);
        }
        let deadline = Instant::now()
            .checked_add(max_lifetime)
            .ok_or(BoundedProcessError::InvalidLifetime)?;
        configure_process_isolation(command);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        #[cfg(windows)]
        {
            let job = WindowsJob::create().map_err(BoundedProcessError::Isolation)?;
            let mut child = command.spawn().map_err(BoundedProcessError::Spawn)?;
            if let Err(error) = attach_supervised_child(&job, &child)
                .and_then(|()| resume_suspended_process(&child))
            {
                // An attach failure leaves the direct process suspended and
                // outside the Job, so clean it explicitly before returning.
                job.terminate();
                terminate_process_tree(&mut child);
                let _ = child.wait();
                return Err(BoundedProcessError::Isolation(error));
            }
            return Ok(Self {
                child,
                deadline,
                max_lifetime,
                terminal_status: None,
                job,
            });
        }

        #[cfg(not(windows))]
        {
            let child = command.spawn().map_err(BoundedProcessError::Spawn)?;
            Ok(Self {
                child,
                deadline,
                max_lifetime,
                terminal_status: None,
            })
        }
    }

    #[must_use]
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>, BoundedProcessError> {
        if let Some(status) = self.terminal_status {
            return Ok(Some(status));
        }
        #[cfg(unix)]
        {
            // Observe without reaping: the direct child's PID still anchors
            // its process group until descendants have been terminated.
            let mut information: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.child.id() as libc::id_t,
                    &mut information,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result != 0 {
                return Err(BoundedProcessError::Wait(std::io::Error::last_os_error()));
            }
            if unsafe { information.si_pid() } != 0 {
                return self.terminate_and_wait().map(Some);
            }
        }
        #[cfg(not(unix))]
        if let Some(status) = self.child.try_wait().map_err(BoundedProcessError::Wait)? {
            // A Windows Job handle retains the descendant tree independently
            // of the direct process's PID or exit state.
            #[cfg(windows)]
            self.job.terminate();
            self.terminal_status = Some(status);
            return Ok(Some(status));
        }
        if Instant::now() >= self.deadline {
            let _ = self.terminate_and_wait()?;
            return Err(BoundedProcessError::Timeout {
                timeout_ms: self.max_lifetime.as_millis(),
            });
        }
        Ok(None)
    }

    pub fn terminate_and_wait(&mut self) -> Result<ExitStatus, BoundedProcessError> {
        if let Some(status) = self.terminal_status {
            return Ok(status);
        }
        // Kill the process tree while the direct child still owns its PID.
        // Reaping first could allow a later Drop to signal a reused PID.
        #[cfg(windows)]
        self.job.terminate();
        terminate_process_tree(&mut self.child);
        let status = self.child.wait().map_err(BoundedProcessError::Wait)?;
        self.terminal_status = Some(status);
        Ok(status)
    }
}

impl Drop for SupervisedChild {
    fn drop(&mut self) {
        if self.terminal_status.is_some() {
            return;
        }
        #[cfg(windows)]
        self.job.terminate();
        terminate_process_tree(&mut self.child);
        let _ = self.child.wait();
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
    static WINDOWS_SUPERVISED_ATTACH_FAILURE: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
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

/// Test seam for the foreground-supervisor attach-failure cleanup path.
#[cfg(windows)]
#[doc(hidden)]
pub fn replace_windows_supervised_attach_failure_for_test(enabled: bool) -> bool {
    WINDOWS_SUPERVISED_ATTACH_FAILURE.with(|slot| slot.replace(enabled))
}

#[cfg(windows)]
fn attach_supervised_child(job: &WindowsJob, child: &Child) -> Result<(), std::io::Error> {
    let forced_failure = WINDOWS_SUPERVISED_ATTACH_FAILURE.with(std::cell::Cell::get);
    if forced_failure {
        return Err(std::io::Error::other(
            "synthetic Windows supervised-child Job attachment failure",
        ));
    }
    job.attach(child)
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

/// Wait for a trusted long-running foreground child without imposing a
/// synthetic timeout or capturing its output.
///
/// This is for a supervisor whose own lifetime is the cancellation boundary.
/// On Windows the child is created suspended, attached to a kill-on-close Job
/// Object, and resumed only after the attachment succeeds.  Therefore ending
/// the supervisor cannot leave its watcher child running in the background.
/// It is deliberately narrower than [`run_bounded_command`]: callers must
/// already have validated the executable, arguments, and environment.
#[cfg(windows)]
pub fn wait_supervised_child(command: &mut Command) -> Result<ExitStatus, BoundedProcessError> {
    configure_process_isolation(command);
    let job = WindowsJob::create().map_err(BoundedProcessError::Isolation)?;
    let mut child = command.spawn().map_err(BoundedProcessError::Spawn)?;
    if let Err(error) =
        attach_supervised_child(&job, &child).and_then(|()| resume_suspended_process(&child))
    {
        // `attach` can fail before the suspended child has joined the Job.
        // Terminating only the Job would then leave that child suspended
        // forever, so terminate the direct child independently before reaping.
        job.terminate();
        terminate_process_tree(&mut child);
        drop(job);
        let _ = child.wait();
        return Err(BoundedProcessError::Isolation(error));
    }
    // The job remains owned by this supervisor until the child has exited.
    // If the supervisor is terminated, the OS closes its handle and kills
    // every process in the job before any watcher can become an orphan.
    child.wait().map_err(BoundedProcessError::Wait)
}

/// Run a Unix renderer with a sampled aggregate scratch cap. The per-file
/// RLIMIT_FSIZE remains the kernel-enforced immediate write limit; this
/// monitor catches multiple individually-valid scratch files.
#[cfg(unix)]
pub(crate) fn run_bounded_command_with_unix_renderer_limits(
    command: &mut Command,
    options: BoundedProcessOptions,
    stdin: Option<BoundedStdin>,
    scratch: &std::path::Path,
    max_scratch_bytes: u64,
    physical_memory_limit: u64,
) -> Result<BoundedProcessOutput, BoundedProcessError> {
    use std::os::unix::fs::OpenOptionsExt;

    let scratch_path = scratch.to_owned();
    let scratch = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(scratch)
        .map_err(BoundedProcessError::ScratchInspection)?;
    if !scratch
        .metadata()
        .map_err(BoundedProcessError::ScratchInspection)?
        .is_dir()
    {
        return Err(BoundedProcessError::ScratchInspection(
            std::io::Error::other("renderer scratch is not a directory"),
        ));
    }
    let mut monitor = UnixRendererMonitor::new(
        scratch,
        scratch_path,
        max_scratch_bytes,
        physical_memory_limit,
    );
    run_bounded_command_inner_impl(command, options, stdin, &mut monitor)
}

#[cfg(unix)]
struct UnixRendererMonitor {
    scratch: File,
    scratch_path: std::path::PathBuf,
    max_scratch_bytes: u64,
    next_scratch_observation: Instant,
    #[cfg(target_os = "macos")]
    physical_memory: Option<MacosPhysicalMemoryMonitor>,
}

#[cfg(unix)]
impl UnixRendererMonitor {
    fn new(
        scratch: File,
        scratch_path: std::path::PathBuf,
        max_scratch_bytes: u64,
        physical_memory_limit: u64,
    ) -> Self {
        #[cfg(not(target_os = "macos"))]
        let _ = physical_memory_limit;
        Self {
            scratch,
            scratch_path,
            max_scratch_bytes,
            next_scratch_observation: Instant::now() + Duration::from_millis(50),
            #[cfg(target_os = "macos")]
            physical_memory: (physical_memory_limit != 0)
                .then(|| MacosPhysicalMemoryMonitor::new(physical_memory_limit)),
        }
    }
}

#[cfg(unix)]
impl BoundedProcessMonitor for UnixRendererMonitor {
    fn observe(&mut self, child: &Child) -> Result<(), BoundedProcessError> {
        #[cfg(not(target_os = "macos"))]
        let _ = child;
        #[cfg(target_os = "macos")]
        self.physical_memory.observe(child)?;
        let now = Instant::now();
        if now < self.next_scratch_observation {
            return Ok(());
        }
        self.next_scratch_observation = now + Duration::from_millis(50);
        let observed = sampled_scratch_bytes(&self.scratch, &self.scratch_path)
            .map_err(BoundedProcessError::ScratchInspection)?;
        if observed > self.max_scratch_bytes {
            return Err(BoundedProcessError::ScratchLimit {
                limit: self.max_scratch_bytes,
                observed,
            });
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod unix_renderer_scratch_tests {
    use std::{fs, fs::File};

    use super::sampled_scratch_bytes;

    #[test]
    fn counts_regular_files_in_nested_scratch_directories() {
        let scratch = tempfile::tempdir().unwrap();
        fs::write(scratch.path().join("top.bin"), [0_u8; 3]).unwrap();
        fs::create_dir(scratch.path().join("nested")).unwrap();
        fs::write(scratch.path().join("nested").join("child.bin"), [0_u8; 5]).unwrap();

        assert_eq!(
            sampled_scratch_bytes(&File::open(scratch.path()).unwrap(), scratch.path()).unwrap(),
            8
        );
    }

    #[test]
    fn rejects_a_named_root_that_no_longer_matches_the_retained_directory() {
        let parent = tempfile::tempdir().unwrap();
        let scratch = parent.path().join("scratch");
        fs::create_dir(&scratch).unwrap();
        let retained = File::open(&scratch).unwrap();
        fs::rename(&scratch, parent.path().join("former-scratch")).unwrap();
        fs::create_dir(&scratch).unwrap();

        let error = sampled_scratch_bytes(&retained, &scratch).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("changed after retained monitor setup")
        );
    }

    #[test]
    fn rejects_a_descendant_swapped_for_a_symlink() {
        use std::os::unix::fs::symlink;

        let scratch = tempfile::tempdir().unwrap();
        let child = scratch.path().join("child");
        fs::create_dir(&child).unwrap();
        fs::write(child.join("before-swap.bin"), [0_u8; 1]).unwrap();
        let parked = scratch.path().join("parked-child");
        fs::rename(&child, &parked).unwrap();
        symlink(&parked, &child).unwrap();

        let error = sampled_scratch_bytes(&File::open(scratch.path()).unwrap(), scratch.path())
            .unwrap_err();
        assert!(error.to_string().contains("contains a symlink"));
    }

    #[test]
    fn ignores_private_unix_socket_bytes_without_accepting_other_special_files() {
        use std::os::unix::net::UnixListener;

        let scratch = tempfile::tempdir().unwrap();
        let socket = UnixListener::bind(scratch.path().join("renderer.sock")).unwrap();

        assert_eq!(
            sampled_scratch_bytes(&File::open(scratch.path()).unwrap(), scratch.path()).unwrap(),
            0
        );
        drop(socket);
    }

    #[test]
    fn rejects_scratch_trees_deeper_than_the_bounded_scan_depth() {
        let scratch = tempfile::tempdir().unwrap();
        let mut current = scratch.path().to_owned();
        for index in 0..33 {
            current.push(format!("level-{index}"));
            fs::create_dir(&current).unwrap();
        }

        let error = sampled_scratch_bytes(&File::open(scratch.path()).unwrap(), scratch.path())
            .unwrap_err();
        assert!(error.to_string().contains("depth limit"));
    }

    #[test]
    fn rejects_scratch_trees_exceeding_the_entry_bound() {
        let scratch = tempfile::tempdir().unwrap();
        for index in 0..10_001 {
            fs::write(scratch.path().join(format!("entry-{index}")), b"").unwrap();
        }

        let error = sampled_scratch_bytes(&File::open(scratch.path()).unwrap(), scratch.path())
            .unwrap_err();
        assert!(error.to_string().contains("entry limit"));
    }
}

#[cfg(unix)]
fn sampled_scratch_bytes(root: &File, root_path: &std::path::Path) -> Result<u64, std::io::Error> {
    use std::{
        ffi::{CStr, CString},
        os::{fd::AsRawFd, unix::fs::MetadataExt},
    };

    const MAX_SCAN_DEPTH: usize = 32;
    const MAX_SCAN_ENTRIES: usize = 10_000;
    // This is an observation budget, not a resource quota for the child. A
    // short scheduler preemption must not make an otherwise bounded tree
    // fail; the renderer's independent command timeout remains authoritative.
    const SCAN_DEADLINE: Duration = Duration::from_secs(1);

    struct Directory(*mut libc::DIR);

    impl Directory {
        fn open(fd: libc::c_int) -> Result<Self, std::io::Error> {
            // SAFETY: `fd` is an owned descriptor returned by `openat`. On
            // success `fdopendir` transfers that ownership to `closedir`.
            let directory = unsafe { libc::fdopendir(fd) };
            if directory.is_null() {
                let error = std::io::Error::last_os_error();
                // SAFETY: `fdopendir` did not take ownership on failure.
                unsafe { libc::close(fd) };
                return Err(error);
            }
            Ok(Self(directory))
        }

        fn fd(&self) -> libc::c_int {
            // SAFETY: `self.0` is owned by this `Directory` and remains live
            // until `closedir` in `Drop`.
            unsafe { libc::dirfd(self.0) }
        }

        fn next(&mut self) -> Result<Option<&CStr>, std::io::Error> {
            // `readdir` distinguishes end-of-directory from an error through
            // errno, so clear it before every call.
            #[cfg(target_os = "macos")]
            unsafe {
                *libc::__error() = 0;
            }
            #[cfg(not(target_os = "macos"))]
            unsafe {
                *libc::__errno_location() = 0;
            }
            // SAFETY: `self.0` remains valid until `Drop`; the returned entry
            // is consumed before another `readdir` call.
            let entry = unsafe { libc::readdir(self.0) };
            if entry.is_null() {
                let error = std::io::Error::last_os_error();
                return if error.raw_os_error() == Some(0) {
                    Ok(None)
                } else {
                    Err(error)
                };
            }
            // SAFETY: POSIX guarantees a NUL-terminated `d_name` in a
            // successful `readdir` result.
            Ok(Some(unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }))
        }
    }

    impl Drop for Directory {
        fn drop(&mut self) {
            // SAFETY: `Directory` exclusively owns this DIR pointer.
            unsafe { libc::closedir(self.0) };
        }
    }

    fn scan_depth_exceeded() -> std::io::Error {
        std::io::Error::other("renderer scratch scan exceeded depth limit")
    }

    fn scan_entry_limit_exceeded() -> std::io::Error {
        std::io::Error::other("renderer scratch scan exceeded entry limit")
    }

    fn scan_deadline_exceeded() -> std::io::Error {
        std::io::Error::other("renderer scratch scan exceeded one-second observation budget")
    }

    fn disappeared(error: &std::io::Error) -> bool {
        matches!(error.raw_os_error(), Some(libc::ENOENT))
    }

    fn open_directory(parent: libc::c_int, name: &CStr) -> Result<Directory, std::io::Error> {
        // SAFETY: `parent` is a live directory descriptor and `name` is a
        // NUL-terminated single component obtained from `readdir`.
        let fd = unsafe {
            libc::openat(
                parent,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Directory::open(fd)
    }

    fn same_identity(left: &libc::stat, right: &libc::stat) -> bool {
        left.st_dev == right.st_dev && left.st_ino == right.st_ino
    }

    fn visit(
        directory: Directory,
        depth: usize,
        total: &mut u64,
        entries: &mut usize,
        started: Instant,
    ) -> Result<(), std::io::Error> {
        if depth > MAX_SCAN_DEPTH {
            return Err(scan_depth_exceeded());
        }
        if started.elapsed() > SCAN_DEADLINE {
            return Err(scan_deadline_exceeded());
        }
        let parent = directory.fd();
        let mut directory = directory;
        while let Some(name) = directory.next()? {
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            *entries = entries
                .checked_add(1)
                .ok_or_else(|| std::io::Error::other("renderer scratch entry count overflow"))?;
            if *entries > MAX_SCAN_ENTRIES {
                return Err(scan_entry_limit_exceeded());
            }
            if started.elapsed() > SCAN_DEADLINE {
                return Err(scan_deadline_exceeded());
            }
            let name = CString::new(name.to_bytes()).expect("readdir names cannot contain NUL");
            let mut before = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: `parent` is a live directory descriptor, `name` is a
            // NUL-terminated child name, and `before` has sufficient storage.
            let result = unsafe {
                libc::fstatat(
                    parent,
                    name.as_ptr(),
                    before.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if result != 0 {
                let error = std::io::Error::last_os_error();
                if disappeared(&error) {
                    continue;
                }
                return Err(error);
            }
            // SAFETY: successful `fstatat` initialized `before`.
            let before = unsafe { before.assume_init() };
            match before.st_mode & libc::S_IFMT {
                libc::S_IFREG => {
                    let size = u64::try_from(before.st_size).map_err(|_| {
                        std::io::Error::other("renderer scratch regular file has a negative size")
                    })?;
                    *total = total
                        .checked_add(size)
                        .ok_or_else(|| std::io::Error::other("renderer scratch size overflow"))?;
                }
                libc::S_IFDIR => {
                    let child = match open_directory(parent, &name) {
                        Ok(child) => child,
                        Err(error) if disappeared(&error) => continue,
                        Err(error) => return Err(error),
                    };
                    let mut after = std::mem::MaybeUninit::<libc::stat>::uninit();
                    // SAFETY: the descriptor backing `child` is live and
                    // `after` has sufficient storage for `fstat`.
                    if unsafe { libc::fstat(child.fd(), after.as_mut_ptr()) } != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    // SAFETY: successful `fstat` initialized `after`.
                    let after = unsafe { after.assume_init() };
                    if !same_identity(&before, &after) {
                        return Err(std::io::Error::other(
                            "renderer scratch directory changed during scan",
                        ));
                    }
                    visit(child, depth + 1, total, entries, started)?;
                }
                libc::S_IFLNK => {
                    return Err(std::io::Error::other("renderer scratch contains a symlink"));
                }
                // LibreOffice's confined single-instance IPC is a pathname
                // Unix socket beneath this invocation's private scratch.
                // It consumes an entry but does not represent scratch bytes.
                libc::S_IFSOCK => {}
                _ => {
                    return Err(std::io::Error::other(
                        "renderer scratch contains a non-regular file",
                    ));
                }
            }
        }
        Ok(())
    }
    let retained = root.metadata()?;
    let named = std::fs::symlink_metadata(root_path)?;
    if !named.is_dir() || named.dev() != retained.dev() || named.ino() != retained.ino() {
        return Err(std::io::Error::other(
            "renderer scratch changed after retained monitor setup",
        ));
    }
    let mut total = 0;
    let mut entries = 0;
    let directory = open_directory(root.as_raw_fd(), c".")?;
    visit(directory, 0, &mut total, &mut entries, Instant::now())?;
    Ok(total)
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
        // Reap before sampling: once try_wait has produced a status, the PID
        // must never be observed again because it could be recycled. Route a
        // monitor failure for a still-live child through shared cancellation.
        if status.is_none() {
            match child.try_wait() {
                Ok(next) => status = next,
                Err(error) => break Err(BoundedProcessError::Wait(error)),
            }
        }
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

#[cfg(all(test, windows))]
mod windows_supervision_tests {
    use std::{
        env, fs,
        process::Command,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use super::{
        BoundedProcessError, replace_windows_supervised_attach_failure_for_test,
        wait_supervised_child,
    };

    struct AttachFailureGuard(bool);

    impl AttachFailureGuard {
        fn install() -> Self {
            Self(replace_windows_supervised_attach_failure_for_test(true))
        }
    }

    impl Drop for AttachFailureGuard {
        fn drop(&mut self) {
            let _ = replace_windows_supervised_attach_failure_for_test(self.0);
        }
    }

    #[test]
    fn supervised_attach_failure_kills_suspended_child_before_it_can_execute() {
        let root = env::temp_dir().join(format!(
            "kio-process-supervision-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let sentinel = root.join("executed.txt");
        let command_processor = env::var_os("SystemRoot")
            .map(|root| std::path::PathBuf::from(root).join("System32/cmd.exe"))
            .expect("Windows test host supplies SystemRoot");
        let script = format!("type nul > \"{}\"", sentinel.display());
        let mut command = Command::new(command_processor);
        command.args(["/C", &script]);
        let _guard = AttachFailureGuard::install();
        let started = Instant::now();
        let result = wait_supervised_child(&mut command);
        assert!(matches!(result, Err(BoundedProcessError::Isolation(_))));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!sentinel.exists());
        fs::remove_dir(&root).unwrap();
    }
}

#[cfg(test)]
mod supervised_child_tests {
    use std::{process::Command, thread, time::Duration};

    use super::{BoundedProcessError, SupervisedChild};

    const HELPER: &str = "KIO_PROCESS_SUPERVISED_CHILD_HELPER";

    #[test]
    fn supervised_child_test_helper() {
        match std::env::var(HELPER).ok().as_deref() {
            Some("grandchild") => {
                let output =
                    std::path::PathBuf::from(std::env::var_os("KIO_SUPERVISED_OUTPUT").unwrap());
                std::fs::write(output.with_extension("ready"), b"ready").unwrap();
                thread::sleep(Duration::from_secs(1));
                std::fs::write(output, b"descendant survived").unwrap();
            }
            Some(mode @ ("parent-exit" | "parent-stay")) => {
                let output =
                    std::path::PathBuf::from(std::env::var_os("KIO_SUPERVISED_OUTPUT").unwrap());
                let mut descendant = helper_command();
                descendant
                    .env(HELPER, "grandchild")
                    .env("KIO_SUPERVISED_OUTPUT", &output);
                // This fixture must exit before its descendant so the outer
                // supervisor's process-tree cleanup, rather than wait(), is tested.
                #[expect(
                    clippy::zombie_processes,
                    reason = "intentional orphan in supervisor fixture"
                )]
                let _descendant = descendant.spawn().unwrap();
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while !output.with_extension("ready").exists() {
                    assert!(std::time::Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(5));
                }
                std::fs::write(output.with_extension("parent-ready"), b"ready").unwrap();
                if mode == "parent-stay" {
                    thread::sleep(Duration::from_secs(10));
                }
            }
            Some(_) => thread::sleep(Duration::from_secs(10)),
            None => {}
        }
    }

    fn helper_command() -> Command {
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args([
                "--exact",
                "supervised_child_tests::supervised_child_test_helper",
                "--test-threads=1",
            ])
            .env_clear()
            .env(HELPER, "1");
        #[cfg(windows)]
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", system_root);
        }
        command
    }

    #[test]
    fn supervised_child_terminates_direct_child() {
        let mut command = helper_command();
        let mut child = SupervisedChild::spawn(&mut command, Duration::from_secs(2)).unwrap();
        assert!(child.try_wait().unwrap().is_none());
        assert!(!child.terminate_and_wait().unwrap().success());
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn supervised_child_poll_enforces_deadline() {
        let mut command = helper_command();
        let mut child = SupervisedChild::spawn(&mut command, Duration::from_millis(1)).unwrap();
        thread::sleep(Duration::from_millis(10));
        assert!(matches!(
            child.try_wait(),
            Err(BoundedProcessError::Timeout { .. })
        ));
    }

    #[test]
    fn supervised_child_rejects_zero_lifetime() {
        let mut command = helper_command();
        assert!(matches!(
            SupervisedChild::spawn(&mut command, Duration::ZERO),
            Err(BoundedProcessError::InvalidLifetime)
        ));
    }

    #[test]
    fn supervised_child_drop_stops_ready_descendants() {
        descendants_are_stopped("parent-stay");
    }

    #[test]
    fn supervised_child_natural_exit_stops_ready_descendants_before_reap() {
        descendants_are_stopped("parent-exit");
    }

    fn descendants_are_stopped(mode: &str) {
        let fixture = tempfile::tempdir().unwrap();
        let output = fixture.path().join("survived");
        let mut command = helper_command();
        command
            .env(HELPER, mode)
            .env("KIO_SUPERVISED_OUTPUT", &output);
        let mut child = SupervisedChild::spawn(&mut command, Duration::from_secs(8)).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !output.with_extension("parent-ready").exists() {
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        if mode == "parent-exit" {
            while child.try_wait().unwrap().is_none() {
                assert!(std::time::Instant::now() < deadline);
                thread::sleep(Duration::from_millis(5));
            }
        }
        drop(child);
        thread::sleep(Duration::from_millis(1100));
        assert!(
            !output.exists(),
            "ordinary descendant escaped supervised lifetime"
        );
    }
}
