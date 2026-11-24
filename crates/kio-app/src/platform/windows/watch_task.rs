//! Language-independent Task Scheduler automation through the OS COM API.
//! The script is embedded program code; request data is JSON, never script text.
use super::watch_task_registration::{
    TASK_SCRIPT, TaskQuery, TaskRecord, automation_command, automation_request, record_mismatch,
    scheduler_operation_error, validate_query,
};
use crate::watch_service::{ServiceManifest, service_error};
use kio_core::{
    Result,
    control::ControlHandle,
    store_dir::{Publication, StoreDirectory},
};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, time::Duration};

// Includes Windows PowerShell startup, COM work, and subprocess I/O.
const SCHEDULER_OPERATION_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) fn query(id: &str, manifest: Option<&ServiceManifest>) -> Result<TaskQuery> {
    invoke("query", id, manifest, &[])
}

fn invoke(
    operation: &str,
    id: &str,
    manifest: Option<&ServiceManifest>,
    allowed: &[&str],
) -> Result<TaskQuery> {
    let payload = automation_request(
        operation,
        id,
        manifest.map_or("", |m| &m.definition),
        allowed,
    )?;
    let directory = system_directory()?;
    let program = kio_core::private_fs::verify_trusted_system_tool(
        &directory.join("WindowsPowerShell/v1.0/powershell.exe"),
    )?;
    let windows = directory
        .parent()
        .ok_or_else(|| service_error("OS system directory has no parent"))?;
    let mut command = automation_command(&program, windows, TASK_SCRIPT)?;
    let failed = |error: &dyn std::fmt::Display| {
        service_error(format!("Task Scheduler {operation} for {id}: {error}"))
    };
    let output = kio_process::run_bounded_command(
        &mut command,
        kio_process::BoundedProcessOptions {
            timeout: SCHEDULER_OPERATION_TIMEOUT,
            max_stdout_bytes: 256 * 1024,
            max_stderr_bytes: 4096,
        },
        Some(kio_process::BoundedStdin::new(payload, 128 * 1024)),
    )
    .map_err(|error| failed(&error))?;
    if !output.status.success() {
        return Err(failed(&output.status));
    }
    let result: TaskQuery =
        serde_json::from_str(output.stdout.trim_start_matches('\u{feff}').trim())
            .map_err(|error| failed(&error))?;
    validate_query(result).map_err(|error| scheduler_operation_error(error, operation, id))
}

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
pub(crate) fn matches(m: &ServiceManifest, allow_absent: bool) -> Result<bool> {
    let query = query(&m.id, Some(m))?;
    if !query.present {
        return Ok(allow_absent);
    }
    verify_current(m, &query).map(|()| true)
}
pub(crate) fn verify_current(m: &ServiceManifest, query: &TaskQuery) -> Result<()> {
    let record = read_record(m)?.unwrap_or_else(|| initial_record(m));
    record_mismatch(query, &record).map_or(Ok(()), |fields| Err(query.mismatch_error(fields)))
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
        let actual = invoke("create", &m.id, Some(m), &record.allowed())?;
        return commit_enabled(m, record, &actual, true);
    }
    transition(m, &query, true, "enable")
}

fn transition(
    m: &ServiceManifest,
    query: &TaskQuery,
    enabled: bool,
    operation: &str,
) -> Result<()> {
    let existing = read_record(m)?;
    let publication = if existing.is_some() {
        Publication::Replace
    } else {
        Publication::CreateOnly
    };
    let mut record = existing.unwrap_or_else(|| initial_record(m));
    if let Some(fields) = record_mismatch(query, &record) {
        return Err(query.mismatch_error(fields));
    }
    if enabled && record.enabled && record.pending_enabled.is_none() {
        return Ok(());
    }
    record.pending_enabled = Some(enabled);
    save_record(m, &record, publication)?;
    let actual = invoke(operation, &m.id, Some(m), &record.allowed())?;
    commit_enabled(m, record, &actual, enabled)
}

fn commit_enabled(
    m: &ServiceManifest,
    mut record: TaskRecord,
    actual: &TaskQuery,
    enabled: bool,
) -> Result<()> {
    if let Some(fields) = actual.definition_mismatch(enabled) {
        return Err(actual.mismatch_error(fields));
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
    let task = query(&m.id, Some(m))?;
    if !task.present {
        return Ok(());
    }
    transition(m, &task, false, "stop")
}
pub(crate) fn uninstall(m: &ServiceManifest) -> Result<()> {
    let (store, leaf) = record_location(m)?;
    store.recover_atomic(&[&store])?;
    let task = query(&m.id, Some(m))?;
    if task.present {
        transition(m, &task, false, "stop")?;
        let record = read_record(m)?.unwrap_or_else(|| initial_record(m));
        invoke("delete", &m.id, Some(m), &record.allowed())?;
    }
    if let Some(bytes) = store.read_optional(&leaf, 4096)? {
        let _ = read_record(m)?;
        store.quarantine_then_remove(&leaf, &bytes, 4096)?;
    }
    Ok(())
}
