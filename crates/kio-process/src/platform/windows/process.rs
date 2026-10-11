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

/// Immutable controls for explicit Windows process regression fixtures.
#[doc(hidden)]
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessControl {
    pre_attach_delay: Option<Duration>,
    supervised_attach_failure: bool,
}
impl ProcessControl {
    pub fn pre_attach_delay(delay: Duration) -> Self {
        Self {
            pre_attach_delay: Some(delay),
            ..Self::default()
        }
    }
    pub fn supervised_attach_failure() -> Self {
        Self {
            supervised_attach_failure: true,
            ..Self::default()
        }
    }
}
struct ControlledProcessMonitor(ProcessControl);
impl BoundedProcessMonitor for ControlledProcessMonitor {
    fn process_tree(&mut self) -> std::io::Result<ProcessTree> {
        ProcessTree::new_with_control(self.0)
    }
    fn observe(&mut self, _child: &Child) -> Result<(), BoundedProcessError> {
        Ok(())
    }
}
#[doc(hidden)]
pub fn run_bounded_command_with_control_for_test(
    command: &mut Command,
    options: BoundedProcessOptions,
    stdin: Option<BoundedStdin>,
    control: ProcessControl,
) -> Result<BoundedProcessOutput, BoundedProcessError> {
    run_bounded_command_inner_impl(
        command,
        options,
        stdin,
        &mut ControlledProcessMonitor(control),
    )
}
#[doc(hidden)]
pub fn wait_supervised_child_with_control_for_test(
    command: &mut Command,
    control: ProcessControl,
) -> Result<ExitStatus, BoundedProcessError> {
    wait_supervised_child_inner(command, control)
}
pub fn wait_supervised_child(command: &mut Command) -> Result<ExitStatus, BoundedProcessError> {
    wait_supervised_child_inner(command, ProcessControl::default())
}
fn wait_supervised_child_inner(
    command: &mut Command,
    control: ProcessControl,
) -> Result<ExitStatus, BoundedProcessError> {
    configure_process_isolation(command);
    let mut process_tree =
        ProcessTree::new_with_control(control).map_err(BoundedProcessError::Isolation)?;
    let mut child = command.spawn().map_err(BoundedProcessError::Spawn)?;
    if let Err(error) = process_tree.attach_supervised_and_resume(&child) {
        // `attach` can fail before the suspended child has joined the Job.
        // Terminating only the Job would then leave that child suspended
        // forever, so terminate the direct child independently before reaping.
        process_tree.terminate_attached();
        terminate_process_tree(&mut child);
        process_tree.close_before_join();
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

pub(crate) struct ProcessTree {
    job: Option<WindowsJob>,
    control: ProcessControl,
}
impl ProcessTree {
    pub(crate) fn new() -> std::io::Result<Self> {
        Self::new_with_control(ProcessControl::default())
    }
    fn new_with_control(control: ProcessControl) -> std::io::Result<Self> {
        WindowsJob::create().map(|job| Self {
            job: Some(job),
            control,
        })
    }
    pub(crate) fn attach_and_resume(&mut self, child: &Child) -> std::io::Result<()> {
        if let Some(delay) = self.control.pre_attach_delay {
            thread::sleep(delay);
        }
        self.job
            .as_ref()
            .expect("Windows Job Object was created before spawning evaluator")
            .attach(child)
            .and_then(|()| resume_suspended_process(child))
    }
    pub(crate) fn attach_supervised_and_resume(&mut self, child: &Child) -> std::io::Result<()> {
        if self.control.supervised_attach_failure {
            return Err(std::io::Error::other(
                "synthetic Windows supervised-child Job attachment failure",
            ));
        }
        self.job
            .as_ref()
            .expect("Windows Job created before spawn")
            .attach(child)
            .and_then(|()| resume_suspended_process(child))
    }
    pub(crate) fn terminate_attached(&self) {
        if let Some(job) = &self.job {
            job.terminate();
        }
    }
    pub(crate) fn close_before_join(&mut self) {
        drop(self.job.take());
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

    use super::{BoundedProcessError, ProcessControl, wait_supervised_child_with_control_for_test};

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
        let started = Instant::now();
        let result = wait_supervised_child_with_control_for_test(
            &mut command,
            ProcessControl::supervised_attach_failure(),
        );
        assert!(matches!(result, Err(BoundedProcessError::Isolation(_))));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!sentinel.exists());
        fs::remove_dir(&root).unwrap();
    }
}
