use crate::platform::unix::scratch::sampled_scratch_bytes;
use crate::*;
use std::fs::File;

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

pub(crate) struct UnixRendererMonitor {
    scratch: File,
    scratch_path: std::path::PathBuf,
    max_scratch_bytes: u64,
    next_scratch_observation: Instant,
    physical_memory: Option<MacosPhysicalMemoryMonitor>,
}

impl UnixRendererMonitor {
    pub(crate) fn new(
        scratch: File,
        scratch_path: std::path::PathBuf,
        max_scratch_bytes: u64,
        physical_memory_limit: u64,
    ) -> Self {
        Self {
            scratch,
            scratch_path,
            max_scratch_bytes,
            next_scratch_observation: Instant::now() + Duration::from_millis(50),
            physical_memory: (physical_memory_limit != 0)
                .then(|| MacosPhysicalMemoryMonitor::new(physical_memory_limit)),
        }
    }
}

impl BoundedProcessMonitor for UnixRendererMonitor {
    fn observe(&mut self, child: &Child) -> Result<(), BoundedProcessError> {
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
struct MacosPhysicalMemoryMonitor {
    limit: u64,
    next_observation: Instant,
}

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
            // A concurrent exit is observed on the next loop; it is not a
            // monitoring failure and must not mask the child's exit status.
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
            Err(error) => Err(BoundedProcessError::PhysicalMemory(error)),
        }
    }
}

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
