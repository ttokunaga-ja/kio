use crate::platform::{runner_argv, watch_task_definition::xml_escape};
use crate::response::watch::NativeServiceState;
use crate::watch_service::{ServiceEnvironment, ServiceManifest, invalid, service_error};
use kio_core::Result;
use std::path::{Path, PathBuf};

pub(crate) fn native_registration_absent(manifest: &ServiceManifest) -> Result<bool> {
    Ok(!super::watch_task::query(&manifest.id, None)?.present)
}

pub(crate) fn native_loaded_definition_matches(manifest: &ServiceManifest) -> Result<bool> {
    super::watch_task::matches(manifest, true)
}

pub(crate) fn native_windows_task_definition_matches(
    manifest: &ServiceManifest,
    allow_absent_for_recovery: bool,
) -> Result<bool> {
    super::watch_task::matches(manifest, allow_absent_for_recovery)
}

pub(crate) fn native_prepare_replacement(_manifest: &ServiceManifest) -> Result<()> {
    Ok(())
}

pub(crate) fn native_manifest_anchor(manifest: &ServiceManifest) -> Result<Option<PathBuf>> {
    super::watch_task_definition::manifest_anchor_from_cache(&manifest.id, &manifest.native_path)
        .map(Some)
}

pub(crate) fn native_manifest_locator(id: &str) -> Result<Option<PathBuf>> {
    let task = super::watch_task::query(id, None)?;
    if !task.present {
        return Ok(None);
    }
    let args = task
        .arguments
        .ok_or_else(|| service_error("foreign native task actions"))?;
    super::watch_task_definition::manifest_from_arguments(&args).map(Some)
}

pub(crate) const NATIVE_DEFINITION_EXTENSION: &str = "xml";

pub(crate) fn native_definition_path(
    id: &str,
    environment: &ServiceEnvironment,
) -> Result<PathBuf> {
    Ok(super::watch_task_definition::native_task_cache_path(
        &environment.xdg_data_home,
        &format!("{id}.{NATIVE_DEFINITION_EXTENSION}"),
    ))
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
    super::watch_task::enable(manifest)
}
pub(crate) fn native_start(manifest: &ServiceManifest) -> Result<()> {
    super::watch_task::start(manifest)
}
pub(crate) fn native_stop(manifest: &ServiceManifest) -> Result<()> {
    super::watch_task::stop(manifest)
}
pub(crate) fn native_uninstall(manifest: &ServiceManifest) -> Result<()> {
    super::watch_task::uninstall(manifest)
}
pub(crate) fn native_after_remove() -> Result<()> {
    Ok(())
}
pub(crate) fn native_state(manifest: &ServiceManifest) -> Result<NativeServiceState> {
    let task = super::watch_task::query(&manifest.id, Some(manifest))?;
    super::watch_task::verify_current(manifest, &task)?;
    Ok(NativeServiceState {
        registered: true,
        active: matches!(task.state, 2 | 4),
    })
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
