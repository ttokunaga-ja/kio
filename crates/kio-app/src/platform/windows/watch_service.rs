use crate::platform::{runner_argv, xml_escape};
use crate::response::watch::NativeServiceState;
use crate::watch_service::{
    ServiceEnvironment, ServiceManifest, invalid, manifest_locator, run_native, run_native_output,
    service_error, validate_argument_path,
};
use kio_core::Result;
use std::env;
use std::path::{Path, PathBuf};

pub(crate) fn native_loaded_definition_matches(manifest: &ServiceManifest) -> Result<bool> {
    native_windows_task_definition_matches(manifest, true)
}

pub(crate) fn native_windows_task_definition_matches(
    manifest: &ServiceManifest,
    allow_absent_for_recovery: bool,
) -> Result<bool> {
    let output = run_native_output(
        schtasks_path()?,
        [
            "/query".into(),
            "/tn".into(),
            manifest.id.clone(),
            "/xml".into(),
        ],
    )?;
    if !output.status.success() {
        // A crash after Task Scheduler deleted the task but before the owned
        // XML cache was removed is recoverable: the exact private cache still
        // binds this name and uninstall may finish removing it.
        if output.stderr.contains("cannot find") || output.stderr.contains("not exist") {
            return Ok(allow_absent_for_recovery);
        }
        return Err(service_error(
            "Task Scheduler could not inspect native watch registration",
        ));
    }
    let manifest_path = manifest_locator(&manifest.id, &manifest.environment)?;
    let manifest_path = manifest_path
        .to_str()
        .ok_or_else(|| service_error("non-UTF-8 watch service manifest path"))?;
    let runner_args = runner_argv(Path::new(manifest_path))?;
    let expected_arguments =
        windows_arguments(&runner_args.iter().map(String::as_str).collect::<Vec<_>>())?;
    Ok(output
        .stdout
        .contains(&format!("<URI>\\{}</URI>", manifest.id))
        && output
            .stdout
            .contains("<LogonType>InteractiveToken</LogonType>")
        && output
            .stdout
            .contains("<RunLevel>LeastPrivilege</RunLevel>")
        && output
            .stdout
            .contains(&xml_escape(&manifest.binary.to_string_lossy())?)
        && output.stdout.contains(&xml_escape(&expected_arguments)?))
}

pub(crate) fn native_definition_path(
    id: &str,
    environment: &ServiceEnvironment,
) -> Result<PathBuf> {
    let path = environment
        .xdg_data_home
        .join("kio/watch-services-native")
        .join(format!("{id}.xml"));
    validate_argument_path(&path)?;
    Ok(path)
}

pub(crate) fn render_definition(
    id: &str,
    binary: &Path,
    _root: &Path,
    _interval: u64,
    _environment: &ServiceEnvironment,
    manifest_path: &Path,
) -> Result<String> {
    let runner_args = runner_argv(manifest_path)?;
    let arguments = windows_arguments(&runner_args.iter().map(String::as_str).collect::<Vec<_>>())?;
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?><Task version=\"1.4\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\"><RegistrationInfo><URI>\\{}</URI><Description>Kio offline watcher {}</Description></RegistrationInfo><Triggers><LogonTrigger><Enabled>true</Enabled></LogonTrigger></Triggers><Principals><Principal id=\"Author\"><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals><Settings><Enabled>true</Enabled><StartWhenAvailable>false</StartWhenAvailable><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy></Settings><Actions Context=\"Author\"><Exec><Command>{}</Command><Arguments>{}</Arguments></Exec></Actions></Task>",
        xml_escape(id)?,
        xml_escape(id)?,
        xml_escape(&binary.to_string_lossy())?,
        xml_escape(&arguments)?
    ))
}

pub(crate) fn native_enable(manifest: &ServiceManifest) -> Result<()> {
    run_native(
        schtasks_path()?,
        [
            "/create".into(),
            "/tn".into(),
            manifest.id.clone(),
            "/xml".into(),
            manifest.native_path.display().to_string(),
        ],
    )
    .map(|_| ())
}

pub(crate) fn native_start(manifest: &ServiceManifest) -> Result<()> {
    run_native(
        schtasks_path()?,
        [
            "/change".into(),
            "/tn".into(),
            manifest.id.clone(),
            "/enable".into(),
        ],
    )?;
    run_native(
        schtasks_path()?,
        ["/run".into(), "/tn".into(), manifest.id.clone()],
    )
    .map(|_| ())
}

pub(crate) fn native_stop(manifest: &ServiceManifest) -> Result<()> {
    run_native(
        schtasks_path()?,
        [
            "/change".into(),
            "/tn".into(),
            manifest.id.clone(),
            "/disable".into(),
        ],
    )?;
    run_native(
        schtasks_path()?,
        ["/end".into(), "/tn".into(), manifest.id.clone()],
    )
    .map(|_| ())
}

pub(crate) fn native_uninstall(manifest: &ServiceManifest) -> Result<()> {
    let query = run_native_output(
        schtasks_path()?,
        ["/query".into(), "/tn".into(), manifest.id.clone()],
    )?;
    if !query.status.success()
        && (query.stderr.contains("cannot find") || query.stderr.contains("not exist"))
    {
        return Ok(());
    }
    if !native_loaded_definition_matches(manifest)? {
        return Err(service_error(
            "Task Scheduler registration changed before uninstall",
        ));
    }
    let _ = native_stop(manifest);
    run_native(
        schtasks_path()?,
        [
            "/delete".into(),
            "/tn".into(),
            manifest.id.clone(),
            "/f".into(),
        ],
    )
    .map(|_| ())
}

pub(crate) fn native_after_remove() -> Result<()> {
    Ok(())
}

pub(crate) fn native_state(manifest: &ServiceManifest) -> Result<NativeServiceState> {
    let output = run_native_output(
        schtasks_path()?,
        [
            "/query".into(),
            "/tn".into(),
            manifest.id.clone(),
            "/fo".into(),
            "list".into(),
        ],
    )?;
    if !output.status.success() {
        return Err(service_error(
            "Task Scheduler could not report native watch registration",
        ));
    }
    Ok(NativeServiceState {
        registered: true,
        active: output.stdout.contains("Running"),
    })
}

fn schtasks_path() -> Result<PathBuf> {
    let root = env::var_os("SystemRoot")
        .map(PathBuf::from)
        .ok_or_else(|| service_error("SystemRoot is unavailable"))?;
    let path = root.join("System32/schtasks.exe");
    if path.is_file() {
        Ok(path)
    } else {
        Err(service_error("schtasks.exe is unavailable"))
    }
}

fn windows_arguments(values: &[&str]) -> Result<String> {
    values
        .iter()
        .map(|value| windows_quote(value))
        .collect::<Result<Vec<_>>>()
        .map(|parts| parts.join(" "))
}

/// Quote one argv element using the CommandLineToArgvW backslash rule: runs of
/// backslashes are doubled only when they precede a quote or the closing quote.
fn windows_quote(value: &str) -> Result<String> {
    if value.chars().any(char::is_control) {
        return Err(invalid(
            "native scheduler value contains a control character",
        ));
    }
    let mut quoted = String::from("\"");
    let mut slashes = 0usize;
    for ch in value.chars() {
        if ch == '\\' {
            slashes += 1;
            continue;
        }
        if ch == '\"' {
            quoted.push_str(&"\\".repeat(slashes.saturating_mul(2).saturating_add(1)));
            quoted.push('\"');
        } else {
            quoted.push_str(&"\\".repeat(slashes));
            quoted.push(ch);
        }
        slashes = 0;
    }
    quoted.push_str(&"\\".repeat(slashes.saturating_mul(2)));
    quoted.push('\"');
    Ok(quoted)
}
