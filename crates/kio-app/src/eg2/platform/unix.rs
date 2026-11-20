use crate::eg2::{EngineSlot, WorkerError};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus};
use std::thread::JoinHandle;

pub(in crate::eg2) fn configure_command(command: &mut Command, memory_bytes: u64) {
    command.process_group(0);
    for (name, _) in std::env::vars_os() {
        let name_bytes = name.as_encoded_bytes();
        if name_bytes.starts_with(b"LD_") || name_bytes.starts_with(b"DYLD_") {
            command.env_remove(name);
        }
    }
    // SAFETY: only async-signal-safe system calls are used before exec.
    unsafe {
        command.pre_exec(move || {
            let zero = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::setrlimit(libc::RLIMIT_CORE, &zero) != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::getrlimit(libc::RLIMIT_AS, &mut limit) != 0 {
                return Err(io::Error::last_os_error());
            }
            // The address-space ceiling is a kernel bound on the worker's
            // mappings for its entire lifetime, including idle intervals.
            let cap = memory_bytes as libc::rlim_t;
            limit.rlim_cur = limit.rlim_cur.min(cap);
            limit.rlim_max = limit.rlim_max.min(cap);
            if libc::setrlimit(libc::RLIMIT_AS, &limit) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

pub(in crate::eg2) fn configure_worker(slot: EngineSlot) -> Result<(), WorkerError> {
    // SAFETY: these calls act on the calling worker only.
    unsafe {
        #[cfg(target_os = "linux")]
        if libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) != 0 {
            return Err(WorkerError::Startup);
        }
        if slot == EngineSlot::Index && libc::setpriority(libc::PRIO_PROCESS, 0, 10) != 0 {
            return Err(WorkerError::Startup);
        }
    }
    Ok(())
}

pub(in crate::eg2) fn private_output(
    capture_diagnostics: bool,
) -> Result<(File, Option<File>), WorkerError> {
    let null = File::options()
        .write(true)
        .open("/dev/null")
        .map_err(|_| WorkerError::Startup)?;
    // SAFETY: the duplicated descriptors are ours, and redirection is
    // complete before loading LiteRT-LM or starting any of its threads.
    unsafe {
        let duplicate = |source| {
            let fd = libc::fcntl(source, libc::F_DUPFD_CLOEXEC, 3);
            if fd < 0 {
                return Err(WorkerError::Startup);
            }
            Ok(File::from_raw_fd(fd))
        };
        let output = duplicate(libc::STDOUT_FILENO)?;
        let diagnostics = capture_diagnostics
            .then(|| duplicate(libc::STDERR_FILENO))
            .transpose()?;
        if libc::dup2(null.as_raw_fd(), libc::STDOUT_FILENO) < 0
            || libc::dup2(null.as_raw_fd(), libc::STDERR_FILENO) < 0
        {
            return Err(WorkerError::Startup);
        }
        Ok((output, diagnostics))
    }
}

pub(in crate::eg2) struct ProcessLimits;

impl ProcessLimits {
    pub(in crate::eg2) fn attach(_: &Child, _: u64) -> Result<Self, WorkerError> {
        Ok(Self)
    }

    pub(in crate::eg2) fn check(&self) -> Result<(), WorkerError> {
        Ok(())
    }
}

pub(in crate::eg2) fn peek_exit(child: &mut Child) -> io::Result<Option<ExitStatus>> {
    // Keep the leader's PID reserved until stop() has signalled its group.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::waitid(
            libc::P_PID,
            child.id(),
            &mut info,
            libc::WNOHANG | libc::WEXITED | libc::WNOWAIT,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if unsafe { info.si_pid() } == 0 {
        return Ok(None);
    }
    let status = unsafe { info.si_status() };
    let raw = if info.si_code == libc::CLD_EXITED {
        status << 8
    } else {
        status
    };
    Ok(Some(ExitStatus::from_raw(raw)))
}

pub(in crate::eg2) fn terminate_group(child: &Child) {
    // SAFETY: the unreaped child's PID is the process group ID set at spawn.
    unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) };
}

pub(in crate::eg2) fn configure_pipes(stdin: &ChildStdin, stdout: &ChildStdout) -> io::Result<()> {
    for fd in [stdin.as_raw_fd(), stdout.as_raw_fd()] {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub(in crate::eg2) fn interrupt_io(_: &JoinHandle<()>) {}

pub(in crate::eg2) fn memory_exit(_: ExitStatus) -> bool {
    // Unix signals alone do not identify the cause of an abort. The C API
    // reports resource exhaustion when a failed allocation is recoverable.
    false
}

pub(in crate::eg2) struct SlotGate(File);

impl SlotGate {
    pub(in crate::eg2) fn open(slot: EngineSlot) -> Result<Self, WorkerError> {
        // Fixed machine-wide sticky directory. These empty coordination files
        // outlive individual workers and are never unlinked. Anyone may open
        // them read-only and take a kernel lock, independent of HOME or XDG.
        #[cfg(test)]
        let root = std::env::temp_dir();
        #[cfg(all(not(test), target_os = "macos"))]
        let root = "/private/tmp";
        #[cfg(all(not(test), not(target_os = "macos")))]
        let root = "/tmp";
        let path = std::path::Path::new(&root).join(format!("kio-eg2-engine-{}.lock", slot.name()));
        let flags = libc::O_NOFOLLOW | libc::O_NONBLOCK;
        let open = || File::options().read(true).custom_flags(flags).open(&path);
        let file = match open() {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match File::options()
                    .write(true)
                    .create_new(true)
                    .mode(0o644)
                    .custom_flags(flags)
                    .open(&path)
                {
                    Ok(created) => {
                        if unsafe { libc::fchmod(created.as_raw_fd(), 0o644) } != 0 {
                            return Err(WorkerError::Startup);
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(_) => return Err(WorkerError::Startup),
                }
                open()
            }
            result => result,
        }
        .map_err(|_| WorkerError::Startup)?;
        let metadata = file.metadata().map_err(|_| WorkerError::Startup)?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.len() != 0
            || metadata.mode() & 0o022 != 0
        {
            return Err(WorkerError::Startup);
        }
        Ok(Self(file))
    }

    pub(in crate::eg2) fn try_acquire(&mut self) -> Result<bool, WorkerError> {
        match self.0.try_lock() {
            Ok(()) => Ok(true),
            Err(std::fs::TryLockError::WouldBlock) => Ok(false),
            Err(_) => Err(WorkerError::Startup),
        }
    }
}

pub(in crate::eg2) fn exit_worker(code: i32) -> ! {
    // SAFETY: exit immediately even if a native thread holds a C runtime lock.
    unsafe { libc::_exit(code) }
}
