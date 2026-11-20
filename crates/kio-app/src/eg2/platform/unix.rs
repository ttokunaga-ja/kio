use crate::eg2::{EngineSlot, MEMORY_BYTES, WorkerError};
use std::fs::File;
use std::io;
use std::os::fd::FromRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus};

pub(in crate::eg2) fn configure_command(command: &mut Command) {
    command.process_group(0);
    for (name, _) in std::env::vars_os() {
        let name_bytes = name.as_encoded_bytes();
        if name_bytes.starts_with(b"LD_") || name_bytes.starts_with(b"DYLD_") {
            command.env_remove(name);
        }
    }
    // SAFETY: only async-signal-safe system calls are used before exec.
    unsafe {
        command.pre_exec(|| {
            let zero = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::setrlimit(libc::RLIMIT_CORE, &zero) != 0 {
                return Err(io::Error::last_os_error());
            }
            #[cfg(target_os = "linux")]
            {
                let mut address_space = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                if libc::getrlimit(libc::RLIMIT_AS, &mut address_space) != 0 {
                    return Err(io::Error::last_os_error());
                }
                let cap = (2 * MEMORY_BYTES) as libc::rlim_t;
                address_space.rlim_cur = address_space.rlim_cur.min(cap);
                address_space.rlim_max = address_space.rlim_max.min(cap);
                if libc::setrlimit(libc::RLIMIT_AS, &address_space) != 0 {
                    return Err(io::Error::last_os_error());
                }
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

pub(in crate::eg2) fn private_stdout() -> Result<File, WorkerError> {
    use std::os::fd::AsRawFd;
    let null = File::options()
        .write(true)
        .open("/dev/null")
        .map_err(|_| WorkerError::Startup)?;
    // SAFETY: the duplicated descriptor is ours, and redirection is complete
    // before loading LiteRT-LM or starting any of its threads.
    unsafe {
        let fd = libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 3);
        if fd < 0 {
            return Err(WorkerError::Startup);
        }
        let output = File::from_raw_fd(fd);
        if libc::dup2(null.as_raw_fd(), libc::STDOUT_FILENO) < 0
            || libc::dup2(null.as_raw_fd(), libc::STDERR_FILENO) < 0
        {
            return Err(WorkerError::Startup);
        }
        Ok(output)
    }
}

pub(in crate::eg2) struct ProcessLimits {
    pub(in crate::eg2) bytes: u64,
}

impl ProcessLimits {
    pub(in crate::eg2) fn attach(_: &Child) -> Result<Self, WorkerError> {
        Ok(Self {
            bytes: MEMORY_BYTES,
        })
    }

    pub(in crate::eg2) fn check(&self, child: &Child) -> Result<(), WorkerError> {
        match footprint(child.id()) {
            Ok(bytes) if bytes > self.bytes => Err(WorkerError::MemoryLimit),
            Ok(_) => Ok(()),
            Err(error)
                if matches!(error.kind(), io::ErrorKind::NotFound)
                    || error.raw_os_error() == Some(libc::ESRCH) =>
            {
                Ok(())
            }
            Err(_) => Err(WorkerError::Startup),
        }
    }
}

pub(in crate::eg2) fn memory_exit(_: ExitStatus) -> bool {
    // Unix signals alone do not identify the cause of termination. The
    // footprint monitor and the C API status report observable memory limits.
    false
}

#[cfg(target_os = "linux")]
fn footprint(pid: u32) -> io::Result<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
    // VmHWM preserves a peak even if the worker frees a burst before polling.
    Ok(status
        .lines()
        .find_map(|line| {
            let kib = line
                .strip_prefix("VmHWM:")?
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()?;
            Some(kib.saturating_mul(1024))
        })
        .unwrap_or(0)) // An exited/zombie process no longer has memory counters.
}

#[cfg(target_os = "macos")]
fn footprint(pid: u32) -> io::Result<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage_info_v0>::zeroed();
    // SAFETY: RUSAGE_INFO_V0 writes a rusage_info_v0.
    if unsafe {
        libc::proc_pid_rusage(
            pid as libc::pid_t,
            libc::RUSAGE_INFO_V0,
            usage.as_mut_ptr().cast(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { usage.assume_init() }.ri_phys_footprint)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn footprint(_: u32) -> io::Result<u64> {
    Err(io::ErrorKind::Unsupported.into())
}

pub(in crate::eg2) fn exit_worker(code: i32) -> ! {
    // SAFETY: exit immediately even if a native thread holds a C runtime lock.
    unsafe { libc::_exit(code) }
}
