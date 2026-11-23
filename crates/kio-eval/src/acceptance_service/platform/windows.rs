use super::*;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::{
    Foundation::{ERROR_INVALID_PARAMETER, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Threading::{OpenProcess, WaitForSingleObject},
};

pub(in crate::acceptance_service) fn assert_pid_absent(pid: u32) -> Result<(), AcceptanceError> {
    const SYNCHRONIZE: u32 = 0x0010_0000;
    let raw = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
    if raw.is_null() {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            Ok(())
        } else {
            Err(invalid(format!(
                "cannot inspect stopped watcher PID {pid}: {error}"
            )))
        };
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    match unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_TIMEOUT => Err(invalid(format!("watcher PID {pid} remains after stop"))),
        _ => Err(invalid(format!(
            "cannot verify exit of watcher PID {pid}: {}",
            std::io::Error::last_os_error()
        ))),
    }
}

pub(in crate::acceptance_service) fn registered_definition(
    private: &Path,
    id: &str,
) -> Result<PathBuf, AcceptanceError> {
    let cache = private.join("xdg-data/kio/watch-services-native");
    let name = format!("{id}.xml");
    let store = kio_core::private_fs::verify_private_directory(&cache)
        .map_err(|error| invalid(format!("cannot verify native task cache: {error}")))?;
    store
        .ensure_owner_private(Path::new(&name))
        .map_err(|error| {
            invalid(format!(
                "cannot verify installed native task definition: {error}"
            ))
        })?;
    Ok(cache.join(name))
}

pub(in crate::acceptance_service) fn assert_native_unregistered(
    id: &str,
) -> Result<(), AcceptanceError> {
    let identity = id.strip_prefix("io.kio.watch.").unwrap_or_default();
    if identity.len() != 32 || !identity.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid("native task ID is invalid"));
    }
    let system_root = std::env::var_os("SystemRoot")
        .ok_or_else(|| invalid("SystemRoot is unavailable for native task verification"))?;
    let powershell =
        PathBuf::from(system_root).join("System32/WindowsPowerShell/v1.0/powershell.exe");
    // An access or transport error is not proof of absence. Only GetTask's
    // file/path-not-found HRESULTs are accepted, independent of locale.
    let script = format!(
        r#"$ErrorActionPreference = 'Stop'
$service = New-Object -ComObject 'Schedule.Service'
$service.Connect()
$folder = $service.GetFolder('\')
try {{ $null = $folder.GetTask('{id}') }}
catch {{
    $exception = $_.Exception
    while ($null -ne $exception.InnerException) {{ $exception = $exception.InnerException }}
    $code = [long]$exception.HResult -band 4294967295L
    if ($code -eq 2147942402L -or $code -eq 2147942403L) {{ 'absent'; exit 0 }}
    throw
}}
throw 'Task Scheduler still has service after uninstall'"#
    );
    let output = scheduler_query(
        path_arg(&powershell)?,
        &[
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ],
    )?;
    if !output.status.success() || output.stdout.trim() != "absent" {
        return Err(invalid(format!(
            "Task Scheduler did not confirm task absence: {}",
            output.stderr.chars().take(512).collect::<String>()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_probe_rejects_a_live_pid() {
        assert!(assert_pid_absent(std::process::id()).is_err());
    }
}
