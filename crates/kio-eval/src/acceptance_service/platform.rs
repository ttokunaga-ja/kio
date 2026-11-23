use super::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(super) use macos::*;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(super) use windows::*;

#[cfg(unix)]
pub(super) fn assert_pid_absent(pid: u32) -> Result<(), AcceptanceError> {
    if unsafe { libc::kill(pid as i32, 0) } == 0 {
        return Err(invalid("watcher process remains after stop"));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn registered_definition(private: &Path, id: &str) -> Result<PathBuf, AcceptanceError> {
    Ok(private
        .join("xdg-config/systemd/user")
        .join(format!("{id}.service")))
}

#[cfg(target_os = "linux")]
pub(super) fn assert_native_unregistered(id: &str) -> Result<(), AcceptanceError> {
    if scheduler_query("/usr/bin/systemctl", &["--user", "is-enabled", id])?
        .status
        .success()
    {
        return Err(invalid("systemd still enables service after uninstall"));
    }
    Ok(())
}

fn scheduler_query(
    program: &str,
    args: &[&str],
) -> Result<crate::runner::BoundedProcessOutput, AcceptanceError> {
    run_bounded_command(
        Command::new(program).args(args),
        BoundedProcessOptions {
            timeout: COMMAND_TIMEOUT,
            max_stdout_bytes: MAX_OUTPUT,
            max_stderr_bytes: MAX_OUTPUT,
        },
        None,
    )
    .map_err(|e| AcceptanceError::Command(e.to_string()))
}
