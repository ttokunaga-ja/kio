use crate::response::watch::WatchServiceRunnerResponse;
use crate::watch_service::{invalid, service_error};
use kio_core::Result;
use kio_process::{BoundedProcessOptions, BoundedProcessOutput, run_bounded_command};
use std::{env, path::PathBuf, process::Command, time::Duration};

pub(crate) fn run_watch_service_runner(
    _manifest_path: PathBuf,
    _control: &kio_core::control::ControlHandle,
) -> Result<WatchServiceRunnerResponse> {
    Err(invalid(
        "watch service runner is reserved for the Windows Task Scheduler",
    ))
}

pub(crate) fn run_native<I, P>(program: P, args: I) -> Result<BoundedProcessOutput>
where
    I: IntoIterator<Item = String>,
    P: AsRef<std::ffi::OsStr>,
{
    let output = run_native_output(program, args)?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(service_error(format!(
            "native scheduler command failed: {}",
            bounded_output(&output)
        )))
    }
}
pub(crate) fn run_native_output<I, P>(program: P, args: I) -> Result<BoundedProcessOutput>
where
    I: IntoIterator<Item = String>,
    P: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new(program);
    command.args(args).env_clear().env("LC_ALL", "C");
    for name in [
        "HOME",
        "USERPROFILE",
        "SystemRoot",
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
    ] {
        if let Some(value) = env::var_os(name) {
            command.env(name, value);
        }
    }
    run_bounded_command(
        &mut command,
        BoundedProcessOptions {
            timeout: Duration::from_secs(10),
            max_stdout_bytes: 64 * 1024,
            max_stderr_bytes: 16 * 1024,
        },
        None,
    )
    .map_err(service_error)
}
fn bounded_output(output: &BoundedProcessOutput) -> String {
    output.stderr.chars().take(512).collect()
}
