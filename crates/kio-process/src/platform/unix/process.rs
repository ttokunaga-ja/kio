use crate::*;

pub(crate) fn configure_process_isolation(command: &mut Command) {
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

pub(crate) fn child_has_exited(child: &mut Child) -> Result<bool, std::io::Error> {
    // Keep an exited child waitable until the caller has finished all possible
    // process-group cancellation. Reaping here would let its PID be reused
    // before a later output error or timeout sends a signal to that group.
    // SAFETY: the initialized record is writable for waitid, and Child owns
    // the direct PID that this wait observes without consuming its status.
    let mut information: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id() as libc::id_t,
            &mut information,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { information.si_pid() } != 0)
}

pub(crate) fn terminate_process_tree(child: &mut Child) {
    let _ = child.kill();
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
}
pub(crate) struct ProcessTree;
impl ProcessTree {
    pub(crate) fn new() -> std::io::Result<Self> {
        Ok(Self)
    }
    pub(crate) fn attach_and_resume(&mut self, _child: &Child) -> std::io::Result<()> {
        Ok(())
    }
    pub(crate) fn attach_supervised_and_resume(&mut self, _child: &Child) -> std::io::Result<()> {
        Ok(())
    }
    pub(crate) fn terminate_attached(&self) {}
    pub(crate) fn close_before_join(&mut self) {}
    pub(crate) fn supervised_exit(
        &self,
        child: &mut Child,
    ) -> Result<Option<ExitStatus>, BoundedProcessError> {
        if child_has_exited(child).map_err(BoundedProcessError::Wait)? {
            terminate_process_tree(child);
            return child.wait().map(Some).map_err(BoundedProcessError::Wait);
        }
        Ok(None)
    }
}
#[cfg(test)]
pub(crate) fn configure_test_environment(_command: &mut Command) {}
