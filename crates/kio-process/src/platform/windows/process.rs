use crate::*;

struct WindowsJob(windows_sys::Win32::Foundation::HANDLE);

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

impl Drop for WindowsJob {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

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
                if entry.th32OwnerProcessID == child.id()
                    && owner_thread.replace(entry.th32ThreadID).is_some()
                {
                    return Err(std::io::Error::other(
                        "suspended bounded process created more than one thread before isolation",
                    ));
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
#[doc(hidden)]
pub fn replace_windows_pre_attach_delay_for_test(delay: Option<Duration>) -> Option<Duration> {
    WINDOWS_PRE_ATTACH_DELAY.with(|slot| slot.replace(delay))
}

/// Test seam for the foreground-supervisor attach-failure cleanup path.
#[doc(hidden)]
pub fn replace_windows_supervised_attach_failure_for_test(enabled: bool) -> bool {
    WINDOWS_SUPERVISED_ATTACH_FAILURE.with(|slot| slot.replace(enabled))
}

fn attach_supervised_child(job: &WindowsJob, child: &Child) -> Result<(), std::io::Error> {
    let forced_failure = WINDOWS_SUPERVISED_ATTACH_FAILURE.with(std::cell::Cell::get);
    if forced_failure {
        return Err(std::io::Error::other(
            "synthetic Windows supervised-child Job attachment failure",
        ));
    }
    job.attach(child)
}

fn delay_before_windows_job_attach_for_test() {
    WINDOWS_PRE_ATTACH_DELAY.with(|delay| {
        if let Some(delay) = delay.get() {
            thread::sleep(delay);
        }
    });
}

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
pub(crate) fn configure_process_isolation(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;
    command.creation_flags(CREATE_SUSPENDED);
}
pub(crate) fn terminate_process_tree(child: &mut Child) {
    let _ = child.kill();
}

pub(crate) struct ProcessTree(Option<WindowsJob>);
impl ProcessTree {
    pub(crate) fn new() -> std::io::Result<Self> {
        WindowsJob::create().map(|job| Self(Some(job)))
    }
    pub(crate) fn attach_and_resume(&mut self, child: &Child) -> std::io::Result<()> {
        delay_before_windows_job_attach_for_test();
        self.0
            .as_ref()
            .expect("Windows Job Object was created before spawning evaluator")
            .attach(child)
            .and_then(|()| resume_suspended_process(child))
    }
    pub(crate) fn attach_supervised_and_resume(&mut self, child: &Child) -> std::io::Result<()> {
        attach_supervised_child(
            self.0.as_ref().expect("Windows Job created before spawn"),
            child,
        )
        .and_then(|()| resume_suspended_process(child))
    }
    pub(crate) fn terminate_attached(&self) {
        if let Some(job) = &self.0 {
            job.terminate();
        }
    }
    pub(crate) fn close_before_join(&mut self) {
        drop(self.0.take());
    }
    pub(crate) fn supervised_exit(
        &self,
        child: &mut Child,
    ) -> Result<Option<ExitStatus>, BoundedProcessError> {
        let status = child.try_wait().map_err(BoundedProcessError::Wait)?;
        if status.is_some() {
            self.terminate_attached();
        }
        Ok(status)
    }
}
pub(crate) fn child_has_exited(child: &mut Child) -> std::io::Result<bool> {
    child.try_wait().map(|status| status.is_some())
}
#[cfg(test)]
pub(crate) fn configure_test_environment(command: &mut Command) {
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
}

#[cfg(test)]
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
