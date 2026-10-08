//! Bounded execution for trusted local helpers.
//!
//! This is deliberately a resource and child-tree boundary, not an operating
//! system sandbox. Callers that run a renderer or other untrusted input must
//! add a platform confinement wrapper before describing that operation as
//! network-isolated.

pub mod confinement;
pub mod platform;

use platform::{
    ProcessTree, StdinPump, child_has_exited, configure_process_isolation, read_stream,
    terminate_process_tree,
};
use std::{
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
    #[error("renderer exceeded aggregate {resource} limit")]
    AggregateResourceLimit { resource: &'static str },
    #[error("cannot read aggregate {resource} resource accounting: {source}")]
    ResourceAccounting {
        resource: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "renderer exceeded sampled aggregate CPU budget of {limit_usec} microseconds (observed {observed_usec})"
    )]
    AggregateCpuLimit { limit_usec: u64, observed_usec: u64 },
    #[error("could not prove renderer scope cleanup: {0}")]
    RendererCleanup(#[source] std::io::Error),
    #[error("bounded subprocess emitted non-UTF-8 {stream}")]
    NonUtf8 { stream: &'static str },
}

enum StreamEvent {
    Data(&'static str, Vec<u8>),
    End,
    ReadError(&'static str, std::io::Error),
    OutputLimit(&'static str, usize),
}

enum OutputPipe {
    Stdout(std::process::ChildStdout),
    Stderr(std::process::ChildStderr),
}
enum StdinProgress {
    Complete,
    Pending,
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
    process_tree: ProcessTree,
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

        let mut process_tree = ProcessTree::new().map_err(BoundedProcessError::Isolation)?;
        let mut child = command.spawn().map_err(BoundedProcessError::Spawn)?;
        if let Err(error) = process_tree.attach_supervised_and_resume(&child) {
            process_tree.terminate_attached();
            terminate_process_tree(&mut child);
            let _ = child.wait();
            return Err(BoundedProcessError::Isolation(error));
        }
        Ok(Self {
            child,
            deadline,
            max_lifetime,
            terminal_status: None,
            process_tree,
        })
    }

    #[must_use]
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>, BoundedProcessError> {
        if let Some(status) = self.terminal_status {
            return Ok(Some(status));
        }
        if let Some(status) = self.process_tree.supervised_exit(&mut self.child)? {
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
        self.process_tree.terminate_attached();
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
        self.process_tree.terminate_attached();
        terminate_process_tree(&mut self.child);
        let _ = self.child.wait();
    }
}

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
    run_bounded_command_inner_impl(command, options, stdin, &mut ())
}

trait BoundedProcessMonitor {
    fn observe(&mut self, child: &Child) -> Result<(), BoundedProcessError>;
    fn needs_startup_pipe(&self) -> bool {
        false
    }
    fn operation_deadline(&self, deadline: Instant) -> Instant {
        deadline
    }
    fn on_spawn(&mut self, _child: &mut Child) -> Result<(), BoundedProcessError> {
        Ok(())
    }
    fn on_exit(&mut self, _child: &Child) -> Result<(), BoundedProcessError> {
        Ok(())
    }
    fn cleanup(&mut self) -> Result<(), BoundedProcessError> {
        Ok(())
    }
    fn reap(&mut self, child: &mut Child) -> Result<ExitStatus, BoundedProcessError> {
        child.wait().map_err(BoundedProcessError::Wait)
    }
}

impl BoundedProcessMonitor for () {
    fn observe(&mut self, _child: &Child) -> Result<(), BoundedProcessError> {
        Ok(())
    }
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
    command.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(
        if stdin.is_some() || monitor.needs_startup_pipe() {
            Stdio::piped()
        } else {
            Stdio::null()
        },
    );
    // This is deliberately before spawn: one monotonic deadline accounts for
    // all work within this boundary, including a slow spawn and cleanup.
    let started = Instant::now();
    let mut process_tree = ProcessTree::new().map_err(BoundedProcessError::Isolation)?;
    let mut child = command.spawn().map_err(BoundedProcessError::Spawn)?;
    if let Err(error) = process_tree.attach_and_resume(&child) {
        process_tree.terminate_attached();
        terminate_process_tree(&mut child);
        process_tree.close_before_join();
        let _ = child.wait();
        return Err(BoundedProcessError::Isolation(error));
    }
    if let Err(error) = monitor.on_spawn(&mut child) {
        process_tree.terminate_attached();
        terminate_process_tree(&mut child);
        let cleanup = monitor.cleanup();
        let reaped = monitor.reap(&mut child);
        cleanup?;
        reaped?;
        return Err(error);
    }
    let stdout = child.stdout.take().expect("stdout was configured as piped");
    let stderr = child.stderr.take().expect("stderr was configured as piped");
    let mut stdin_pump = StdinPump::new(&mut child, stdin);
    if let Err(error) = stdin_pump.prepare() {
        process_tree.terminate_attached();
        terminate_process_tree(&mut child);
        let cleanup = monitor.cleanup();
        let reaped = monitor.reap(&mut child);
        cleanup?;
        reaped?;
        return Err(error);
    }
    let (sender, receiver) = mpsc::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let stdout_reader = thread::spawn({
        let sender = sender.clone();
        let cancelled = Arc::clone(&cancelled);
        move || {
            read_stream(
                OutputPipe::Stdout(stdout),
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
                OutputPipe::Stderr(stderr),
                "stderr",
                options.max_stderr_bytes,
                sender,
                cancelled,
            )
        }
    });

    let deadline = monitor.operation_deadline(started + options.timeout);
    let mut child_exited = false;
    let mut stdout = None;
    let mut stderr = None;
    let mut finished_streams = 0;
    let result = 'run: loop {
        // Observe exit before sampling, without reaping on Unix. This keeps
        // the child's PID reserved through any later group cancellation while
        // avoiding memory observations after exit. Windows retains try_wait's
        // cached status and its independent Job Object cleanup authority.
        if !child_exited {
            let exited = child_has_exited(&mut child);
            match exited {
                Ok(exited) => {
                    child_exited = exited;
                    if exited && let Err(error) = monitor.on_exit(&child) {
                        break Err(error);
                    }
                }
                Err(error) => break Err(BoundedProcessError::Wait(error)),
            }
        }
        if !child_exited && let Err(error) = monitor.observe(&child) {
            break 'run Err(error);
        }
        let stdin_complete = match stdin_pump.progress(deadline, options.timeout) {
            Ok(StdinProgress::Complete) => true,
            Ok(StdinProgress::Pending) => false,
            Err(error) => break 'run Err(error),
        };
        if child_exited && !stdin_complete {
            break Err(BoundedProcessError::Write(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "bounded subprocess exited before consuming bounded stdin",
            )));
        }
        if child_exited && finished_streams == 2 && stdin_complete {
            break Ok(());
        }
        let now = Instant::now();
        if now >= deadline {
            break Err(BoundedProcessError::Timeout {
                timeout_ms: options.timeout.as_millis(),
            });
        }
        match stdin_pump.wait_pending(deadline, now) {
            Err(error) => break Err(error),
            Ok(false) => {}
            Ok(true) => {
                match receiver.try_recv() {
                    Ok(event) => {
                        if let Err(error) = record_stream_event(
                            event,
                            &mut stdout,
                            &mut stderr,
                            &mut finished_streams,
                        ) {
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
        process_tree.terminate_attached();
        terminate_process_tree(&mut child);
        // On Windows, dropping the Job Object is the second, independent
        // kill-on-close boundary. Do it before joining pipe workers so a
        // failed explicit termination call cannot leave inherited handles
        // keeping those workers blocked.
        process_tree.close_before_join();
    }
    let cleanup = monitor.cleanup();
    if cleanup.is_err() {
        cancelled.store(true, Ordering::Release);
        process_tree.terminate_attached();
        terminate_process_tree(&mut child);
        process_tree.close_before_join();
    }
    let waited = monitor.reap(&mut child);
    let stdin_result = stdin_pump.finish();
    let _ = stdout_reader.join();
    let _ = stderr_reader.join();
    cleanup?;
    result?;
    stdin_result?;
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
        crate::platform::configure_test_environment(&mut command);
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
