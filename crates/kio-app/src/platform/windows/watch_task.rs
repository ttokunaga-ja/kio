//! Language-independent Task Scheduler automation through the OS COM API.
//! The script is embedded program code; request data is JSON, never script text.
use crate::platform::watch_service_model::{task_definition_matches, task_lookup};
use crate::watch_service::{ServiceManifest, service_error};
use base64::Engine;
use kio_core::{
    Result,
    control::ControlHandle,
    store_dir::{Publication, StoreDirectory},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, process::Command, time::Duration};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskQuery {
    hresult: u32,
    lookup_hresult: u32,
    pub(crate) present: bool,
    actual: Option<String>,
    enabled_xml: Option<String>,
    disabled_xml: Option<String>,
    pub(crate) state: u32,
    pub(crate) arguments: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskRecord {
    template_hash: String,
    enabled: bool,
    pending_enabled: Option<bool>,
}
impl TaskRecord {
    fn allowed(&self) -> Vec<&'static str> {
        let mut states = vec![if self.enabled { "true" } else { "false" }];
        if let Some(pending) = self.pending_enabled {
            let state = if pending { "true" } else { "false" };
            if !states.contains(&state) {
                states.push(state);
            }
        }
        states
    }
}

pub(crate) fn query(id: &str, manifest: Option<&ServiceManifest>) -> Result<TaskQuery> {
    invoke("query", id, manifest, &[])
}

fn invoke(
    operation: &str,
    id: &str,
    manifest: Option<&ServiceManifest>,
    allowed: &[&str],
) -> Result<TaskQuery> {
    let request = serde_json::to_vec(&serde_json::json!({"operation": operation, "id": id,
        "template": manifest.map(|m| m.definition.as_str()).unwrap_or(""), "allowed": allowed}))
    .map_err(service_error)?;
    let payload = base64::engine::general_purpose::STANDARD.encode(request);
    let script = concat!(
        "$request = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String([Console]::In.ReadToEnd())) | ConvertFrom-Json\n",
        include_str!("watch_task.ps1")
    );
    let encoded = base64::engine::general_purpose::STANDARD.encode(
        script
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    );
    let directory = system_directory()?;
    let program = directory.join("WindowsPowerShell/v1.0/powershell.exe");
    let program = kio_core::private_fs::verify_trusted_system_tool(&program)?;
    let mut command = Command::new(&program);
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
            &encoded,
        ])
        .current_dir(
            program
                .parent()
                .ok_or_else(|| service_error("OS tool has no installation directory"))?,
        )
        .env_clear()
        .env(
            "SystemRoot",
            directory
                .parent()
                .ok_or_else(|| service_error("OS system directory has no parent"))?,
        );
    let output = kio_process::run_bounded_command(
        &mut command,
        kio_process::BoundedProcessOptions {
            timeout: Duration::from_secs(15),
            max_stdout_bytes: 256 * 1024,
            max_stderr_bytes: 4096,
        },
        Some(kio_process::BoundedStdin::new(
            payload.into_bytes(),
            128 * 1024,
        )),
    )
    .map_err(service_error)?;
    if !output.status.success() {
        return Err(service_error("Task Scheduler automation failed"));
    }
    let result: TaskQuery =
        serde_json::from_str(output.stdout.trim_start_matches('\u{feff}').trim())
            .map_err(service_error)?;
    validate_query(result)
}

fn validate_query(result: TaskQuery) -> Result<TaskQuery> {
    // A missing COM component or a failed connection is never task absence.
    // Only the HRESULT returned by GetTask itself may establish absence.
    if result.hresult != 0 {
        return Err(service_error(format!(
            "Task Scheduler API HRESULT 0x{:08x}",
            result.hresult
        )));
    }
    if task_lookup(result.lookup_hresult)? != result.present
        || (result.present && result.actual.is_none())
    {
        return Err(service_error("inconsistent Task Scheduler response"));
    }
    Ok(result)
}

#[cfg(windows)]
fn system_directory() -> Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    let mut buffer = vec![0_u16; 32768];
    let count = unsafe {
        windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW(
            buffer.as_mut_ptr(),
            buffer.len() as u32,
        )
    } as usize;
    if count == 0 || count >= buffer.len() {
        return Err(service_error("OS system directory unavailable"));
    }
    Ok(PathBuf::from(std::ffi::OsString::from_wide(
        &buffer[..count],
    )))
}

#[cfg(not(windows))]
fn system_directory() -> Result<PathBuf> {
    // Native execution is Windows-only; portable tests exercise the protocol
    // and ownership logic without resolving or starting a host OS tool.
    Err(service_error(
        "Windows system directory unavailable on this platform",
    ))
}

fn record_location(m: &ServiceManifest) -> Result<(StoreDirectory, PathBuf)> {
    let parent = m
        .native_path
        .parent()
        .ok_or_else(|| service_error("invalid native task cache"))?;
    let store = kio_core::private_fs::verify_private_creation_parent_with_control(
        parent,
        &ControlHandle::default(),
    )?;
    kio_core::private_fs::verify_private_directory_handle(&store)?;
    Ok((store, PathBuf::from(format!("{}.task-state.json", m.id))))
}
fn template_hash(m: &ServiceManifest) -> String {
    kio_core::cas::lower_hex(&Sha256::digest(m.definition.as_bytes()))
}
fn read_record(m: &ServiceManifest) -> Result<Option<TaskRecord>> {
    let (store, leaf) = record_location(m)?;
    if !store.contains_entry(&leaf)? {
        return Ok(None);
    }
    let bytes = kio_core::private_fs::read_private_file_at(
        &store,
        leaf.to_str()
            .ok_or_else(|| service_error("invalid task state name"))?,
        4096,
    )?;
    let record: TaskRecord = serde_json::from_slice(&bytes).map_err(service_error)?;
    if record.template_hash != template_hash(m) {
        return Err(service_error(
            "task proof does not match its private definition",
        ));
    }
    Ok(Some(record))
}
fn initial_record(m: &ServiceManifest) -> TaskRecord {
    // The private definition specifies Enabled=true. A state journal is needed
    // only when a Kio lifecycle operation explicitly changes that definition.
    TaskRecord {
        template_hash: template_hash(m),
        enabled: true,
        pending_enabled: None,
    }
}

fn save_record(m: &ServiceManifest, record: &TaskRecord, publication: Publication) -> Result<()> {
    let (store, leaf) = record_location(m)?;
    store.recover_atomic(&[&store])?;
    store.write_atomic(
        &leaf,
        &serde_json::to_vec(record).map_err(service_error)?,
        publication,
    )?;
    store.ensure_owner_private(&leaf)
}
fn matches_enabled(query: &TaskQuery, enabled: bool) -> bool {
    let Some(actual) = query.actual.as_deref() else {
        return false;
    };
    let expected = if enabled {
        query.enabled_xml.as_deref()
    } else {
        query.disabled_xml.as_deref()
    };
    expected.is_some_and(|xml| task_definition_matches(actual, xml))
}
fn matches_record(query: &TaskQuery, record: &TaskRecord) -> bool {
    matches_enabled(query, record.enabled)
        || record
            .pending_enabled
            .is_some_and(|enabled| matches_enabled(query, enabled))
}

pub(crate) fn matches(m: &ServiceManifest, allow_absent: bool) -> Result<bool> {
    let query = query(&m.id, Some(m))?;
    if !query.present {
        return Ok(allow_absent);
    }
    let record = read_record(m)?.unwrap_or_else(|| initial_record(m));
    Ok(matches_record(&query, &record))
}
pub(crate) fn enable(m: &ServiceManifest) -> Result<()> {
    let query = query(&m.id, Some(m))?;
    if !query.present {
        let existing = read_record(m)?;
        let publication = if existing.is_some() {
            Publication::Replace
        } else {
            Publication::CreateOnly
        };
        let mut record = existing.unwrap_or_else(|| initial_record(m));
        record.pending_enabled = Some(true);
        save_record(m, &record, publication)?;
        invoke("create", &m.id, Some(m), &record.allowed())?;
    }
    transition(m, true, "enable")
}

fn transition(m: &ServiceManifest, enabled: bool, operation: &str) -> Result<()> {
    let existing = read_record(m)?;
    let publication = if existing.is_some() {
        Publication::Replace
    } else {
        Publication::CreateOnly
    };
    let mut record = existing.unwrap_or_else(|| initial_record(m));
    if !matches_record(&query(&m.id, Some(m))?, &record) {
        return Err(service_error("foreign Task Scheduler definition"));
    }
    record.pending_enabled = Some(enabled);
    save_record(m, &record, publication)?;
    let actual = invoke(operation, &m.id, Some(m), &record.allowed())?;
    if !matches_enabled(&actual, enabled) {
        return Err(service_error(
            "task did not commit the requested full definition",
        ));
    }
    record.enabled = enabled;
    record.pending_enabled = None;
    save_record(m, &record, Publication::Replace)
}
pub(crate) fn start(m: &ServiceManifest) -> Result<()> {
    enable(m)?;
    let record = read_record(m)?.unwrap_or_else(|| initial_record(m));
    invoke("start", &m.id, Some(m), &record.allowed()).map(|_| ())
}
pub(crate) fn stop(m: &ServiceManifest) -> Result<()> {
    if !query(&m.id, Some(m))?.present {
        return Ok(());
    }
    transition(m, false, "stop")
}
pub(crate) fn uninstall(m: &ServiceManifest) -> Result<()> {
    let (store, leaf) = record_location(m)?;
    store.recover_atomic(&[&store])?;
    if query(&m.id, Some(m))?.present {
        stop(m)?;
        let record = read_record(m)?.unwrap_or_else(|| initial_record(m));
        invoke("delete", &m.id, Some(m), &record.allowed())?;
    }
    if let Some(bytes) = store.read_optional(&leaf, 4096)? {
        let _ = read_record(m)?;
        store.quarantine_then_remove(&leaf, &bytes, 4096)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply() -> TaskQuery {
        TaskQuery { hresult: 0, lookup_hresult: 0, present: true,
            actual: Some("<Task><Settings><Enabled>true</Enabled></Settings><Actions><Exec><Command>kio</Command></Exec></Actions></Task>".into()),
            enabled_xml: Some("<Task><Settings><Enabled>true</Enabled></Settings><Actions><Exec><Command>kio</Command></Exec></Actions></Task>".into()),
            disabled_xml: Some("<Task><Settings><Enabled>false</Enabled></Settings><Actions><Exec><Command>kio</Command></Exec></Actions></Task>".into()), state: 3, arguments: None }
    }
    #[test]
    fn scheduler_connection_failure_cannot_masquerade_as_absence() {
        let mut result = reply();
        result.hresult = 0x80070002;
        result.present = false;
        assert!(validate_query(result).is_err());
        let mut result = reply();
        result.lookup_hresult = 0x80070002;
        result.present = false;
        result.actual = None;
        assert!(!validate_query(result).unwrap().present);
        let mut result = reply();
        result.lookup_hresult = 0x80070005;
        result.present = false;
        assert!(validate_query(result).is_err());
    }
    #[test]
    fn os_only_task_fields_and_unapproved_enable_changes_are_foreign() {
        let record = TaskRecord {
            template_hash: "fixture".into(),
            enabled: true,
            pending_enabled: None,
        };
        assert!(matches_record(&reply(), &record));
        let mut foreign = reply();
        foreign.actual = foreign.actual.map(|xml| {
            xml.replace(
                "</Exec>",
                "<WorkingDirectory>foreign</WorkingDirectory></Exec>",
            )
        });
        assert!(!matches_record(&foreign, &record));
        let mut disabled = reply();
        disabled.actual = disabled.disabled_xml.clone();
        assert!(!matches_record(&disabled, &record));
        let pending = TaskRecord {
            pending_enabled: Some(false),
            ..record
        };
        assert!(matches_record(&disabled, &pending));
        assert!(!matches_enabled(&reply(), false));
        assert!(matches_enabled(&disabled, false));
    }
}
