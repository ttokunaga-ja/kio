use super::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(super) use macos::*;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(super) use windows::*;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub(super) use linux::*;

#[cfg(unix)]
pub(super) fn assert_pid_absent(pid: u32) -> Result<(), AcceptanceError> {
    if unsafe { libc::kill(pid as i32, 0) } == 0 {
        return Err(invalid("watcher process remains after stop"));
    }
    Ok(())
}

fn scheduler_query(
    deadline: Instant,
    program: &str,
    args: &[&str],
) -> Result<crate::runner::BoundedProcessOutput, AcceptanceError> {
    run_bounded_command(
        Command::new(program).args(args),
        BoundedProcessOptions {
            timeout: remaining_budget(
                deadline,
                if cfg!(windows) {
                    Duration::from_secs(60)
                } else {
                    COMMAND_TIMEOUT
                },
                "native scheduler observation",
            )?,
            max_stdout_bytes: MAX_OUTPUT,
            max_stderr_bytes: MAX_OUTPUT,
        },
        None,
    )
    .map_err(|e| AcceptanceError::Command(format!("native scheduler observation failed: {e}")))
}
