//! Foreground watcher lifecycle. Device state is rebuildable and conveys no
//! management or send authority. Each pass reopens the registered root and
//! runs the same local index operation as the interactive command.

use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kio_core::management::{ManagementAuthority, ManagementBinding, read_record};
use kio_core::private_fs::read_private_file_at;
use kio_core::scope::new_ulid;
use kio_core::store_dir::{Publication, StoreDirectory};
use kio_core::{ExitCode, KioError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::commands::{CommandRequest, IndexArgs, WatchArgs, WatchCommand};
use crate::context::{AppContext, Interaction};
use crate::management::{binding_for_repo, open_existing_managed_root};
use crate::watch::{
    DirtyReason, NativeWatcher, Reconcile, ReconcileCompletion, WatchEngine, WatchError, WatchRoot,
};

const LOCK: &str = "instance.lock";
const STATE: &str = "state.json";
const STOP: &str = "stop.json";
const MAX_STATE_BYTES: u64 = 64 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstanceState {
    version: u8,
    instance: String,
    root: PathBuf,
    scope_id: String,
    pid: u32,
    backend: String,
    backlog: usize,
    degraded: bool,
    last_success_ms: Option<u64>,
    last_failure: Option<String>,
    observed_at_ms: u64,
    reconcile_interval_seconds: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StopRequest {
    instance: String,
}

pub(crate) fn run(args: WatchArgs) -> Result<Value> {
    let WatchArgs {
        root: configured_root,
        command,
    } = args;
    let command = match command {
        WatchCommand::Service(command) => {
            return crate::watch_service::run(configured_root, command);
        }
        command => command,
    };
    let cwd = crate::context_working_directory()?;
    let requested = configured_root.clone().unwrap_or(cwd.clone());
    let requested = if requested.is_absolute() {
        requested
    } else {
        cwd.join(requested)
    };
    let root = requested
        .canonicalize()
        .map_err(|e| KioError::io(e.to_string(), requested.display().to_string()))?;
    match command {
        WatchCommand::Run {
            reconcile_interval_seconds,
        } => run_foreground(&root, reconcile_interval_seconds),
        WatchCommand::Status => inspect(&root, false),
        WatchCommand::Stop => inspect(&root, true),
        WatchCommand::Service(_) => unreachable!("service commands return before root resolution"),
    }
}

pub(crate) fn inspect_liveness(root: &Path) -> Result<Value> {
    inspect(root, false)
}

fn inspect(root: &Path, stop: bool) -> Result<Value> {
    let Some(directory) = state_directory(root, false)? else {
        return Ok(json!({"status":"stopped", "root":root, "last_observation":null}));
    };
    let lock = open_instance_lock(&directory, false)?;
    let running = match lock {
        None => false,
        Some(file) => match file.try_lock() {
            Ok(()) => false,
            Err(TryLockError::WouldBlock) => true,
            Err(TryLockError::Error(error)) => return Err(watch_error(error)),
        },
    };
    let state = read_state(&directory, root)?;
    if stop && running {
        let state = state.as_ref().ok_or_else(|| {
            watch_error("watcher is starting; no instance has been published yet")
        })?;
        // A delayed stop for a previous instance can never terminate its
        // replacement. No PID signalling or process-name matching is used.
        let request = serde_json::to_vec(&StopRequest {
            instance: state.instance.clone(),
        })
        .map_err(watch_error)?;
        directory.write_atomic(Path::new(STOP), &request, Publication::Upsert)?;
    }
    Ok(json!({
        "status": if running { if stop { "stop_requested" } else { "running" } } else { "stopped" },
        "root":root,
        "last_observation":state,
    }))
}

fn run_foreground(root: &Path, interval: u64) -> Result<Value> {
    if !(1..=86400).contains(&interval) {
        return Err(KioError::invalid_usage(
            "reconciliation interval must be 1 through 86400 seconds",
        ));
    }
    let repo = open_existing_managed_root(root)?;
    let binding = binding_for_repo(&repo)?;
    let record = read_record(&binding)?;
    if record.authority != ManagementAuthority::Root {
        return Err(KioError::invalid_usage(
            "watch requires an explicitly registered root; use its management root",
        ));
    }
    let directory = state_directory(root, true)?
        .ok_or_else(|| watch_error("watch state directory is unavailable"))?;
    #[cfg(not(target_os = "linux"))]
    if kio_core::private_fs::resolve_inherited_private_root(directory.path())
        .map_err(watch_error)?
        .is_some()
    {
        return Err(watch_error(
            "descriptor-backed watch state queues are unsupported on this platform",
        ));
    }
    let lock = open_instance_lock(&directory, true)?
        .ok_or_else(|| watch_error("watch instance lock is unavailable"))?;
    lock.try_lock().map_err(|error| {
        watch_error(format!(
            "watcher is already running or cannot acquire its instance lock: {error}"
        ))
    })?;
    let instance = new_ulid(root);
    let watch_root =
        WatchRoot::new(&record.scope_id, root.to_path_buf(), 1).map_err(watch_error)?;
    let reconciler = LocalReconciler {
        binding,
        scope_id: record.scope_id.clone(),
    };
    #[cfg(target_os = "linux")]
    let engine = WatchEngine::open_with_interval_private_directory(
        &directory,
        Path::new("queue.sqlite"),
        [watch_root],
        reconciler,
        Duration::from_secs(interval),
    )
    .map_err(watch_error)?;
    #[cfg(not(target_os = "linux"))]
    let engine = WatchEngine::open_with_interval(
        &directory.path().join("queue.sqlite"),
        [watch_root],
        reconciler,
        Duration::from_secs(interval),
    )
    .map_err(watch_error)?;
    let engine = Arc::new(engine);
    // Register before startup reconciliation, so changes made during the full
    // scan remain queued. Initialization failure retains periodic recovery.
    let _native = NativeWatcher::start(Arc::clone(&engine)).ok();
    let mut state = InstanceState {
        version: 1,
        instance,
        root: root.to_path_buf(),
        scope_id: record.scope_id,
        pid: std::process::id(),
        backend: String::new(),
        backlog: 0,
        degraded: false,
        last_success_ms: None,
        last_failure: None,
        observed_at_ms: 0,
        reconcile_interval_seconds: interval,
    };
    publish_state(&directory, &engine, &mut state)?;
    crate::context::diagnostic(
        "watch started; reconciliation is local and grants no external-send permission",
    );
    let mut next_observation = Instant::now();
    loop {
        if requested_stop(&directory, &state.instance)? {
            engine.request_shutdown();
            break;
        }
        let worked = engine.reconcile_once().map_err(watch_error)?;
        if worked || Instant::now() >= next_observation {
            publish_state(&directory, &engine, &mut state)?;
            next_observation = Instant::now() + Duration::from_secs(1);
        }
        if !worked {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    publish_state(&directory, &engine, &mut state)?;
    // OS file locks release on crash as well. The persisted status is only a
    // dated observation; inspect checks the lock before reporting "running".
    drop(_native);
    drop(engine);
    drop(lock);
    Ok(json!({"status":"stopped", "root":root, "last_observation":state}))
}

struct LocalReconciler {
    binding: ManagementBinding,
    scope_id: String,
}

impl Reconcile for LocalReconciler {
    fn reconcile(
        &self,
        root: &WatchRoot,
        _: DirtyReason,
        _: &[PathBuf],
    ) -> std::result::Result<ReconcileCompletion, WatchError> {
        let result = (|| -> Result<Value> {
            self.binding.revalidate()?;
            let record = read_record(&self.binding)?;
            if record.scope_id != self.scope_id || record.authority != ManagementAuthority::Root {
                return Err(watch_error("registered watch root authority changed"));
            }
            let repo = open_existing_managed_root(&root.canonical_root)?;
            let current = binding_for_repo(&repo)?;
            if current.directory_identity() != self.binding.directory_identity()
                || repo.scope_identity()?.scope_id != self.scope_id
            {
                return Err(watch_error("registered watch root was replaced"));
            }
            let context = AppContext {
                working_directory: root.canonical_root.clone(),
                interaction: Arc::new(Unattended),
            };
            // execute reloads user configuration and runtime TLS settings for
            // every pass; a service never freezes startup consent or profiles.
            crate::execute(
                &context,
                CommandRequest::Index(IndexArgs {
                    preview: false,
                    yes: false,
                    online: false,
                    offline: true,
                    realtime: false,
                    batch: false,
                }),
            )
        })();
        let output = result.map_err(|error| WatchError::invariant(error.to_string()))?;
        if crate::peek_exit_override(&output).is_some()
            || output.get("status").and_then(Value::as_str) == Some("deferred")
        {
            // Keep the durable queue retry semantics, while making a partial
            // index result diagnosable without copying its potentially large
            // child-scope rows into watcher output or device state.
            eprintln!(
                "watch reconciliation incomplete: {}",
                incomplete_output_reason(&output)
            );
            return Ok(ReconcileCompletion::Incomplete);
        }
        Ok(ReconcileCompletion::Complete)
    }
}

fn incomplete_output_reason(output: &Value) -> String {
    let status = output
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("missing");
    let error_code = output
        .get("error_code")
        .and_then(Value::as_str)
        .unwrap_or("missing");
    let exit_code = crate::peek_exit_override(output)
        .map(|code| code.code().to_string())
        .unwrap_or_else(|| "none".into());
    let discovery = output.get("child_scope_discovery");
    let has_more = discovery
        .and_then(|value| value.get("has_more"))
        .and_then(Value::as_bool)
        .map(|value| value.to_string())
        .unwrap_or_else(|| "missing".into());
    let rows_total = discovery
        .and_then(|value| value.get("rows_total"))
        .and_then(Value::as_u64)
        .map(|value| value.to_string())
        .unwrap_or_else(|| "missing".into());
    format!(
        "status={status} error_code={error_code} exit_code={exit_code} child_discovery_has_more={has_more} child_discovery_rows_total={rows_total}"
    )
}

struct Unattended;
impl Interaction for Unattended {
    fn confirm(&self, _: &str) -> Result<bool> {
        Ok(false)
    }
    fn read_input(&self, _: usize) -> Result<Vec<u8>> {
        Err(watch_error("watcher cannot read interactive input"))
    }
    fn is_interactive(&self) -> bool {
        false
    }
    fn diagnostic(&self, message: &str) {
        eprintln!("{message}");
    }
}

fn publish_state<R: Reconcile>(
    directory: &StoreDirectory,
    engine: &WatchEngine<R>,
    state: &mut InstanceState,
) -> Result<()> {
    let status = engine.status().map_err(watch_error)?;
    state.backend = status.backend;
    state.backlog = status.backlog;
    state.degraded = status.degraded;
    state.last_success_ms = status.last_success.map(epoch_ms).transpose()?;
    state.last_failure = status.last_failure;
    state.observed_at_ms = epoch_ms(SystemTime::now())?;
    directory.write_atomic(
        Path::new(STATE),
        &serde_json::to_vec(state).map_err(watch_error)?,
        Publication::Upsert,
    )
}

fn read_state(directory: &StoreDirectory, root: &Path) -> Result<Option<InstanceState>> {
    let Some(bytes) = directory.read_optional(Path::new(STATE), MAX_STATE_BYTES)? else {
        return Ok(None);
    };
    let state: InstanceState = serde_json::from_slice(&bytes).map_err(watch_error)?;
    if state.version != 1 || state.root != root || state.instance.len() != 26 {
        return Err(watch_error("watch state binding is invalid"));
    }
    Ok(Some(state))
}

fn requested_stop(directory: &StoreDirectory, instance: &str) -> Result<bool> {
    let Some(bytes) = directory.read_optional(Path::new(STOP), 1024)? else {
        return Ok(false);
    };
    let request: StopRequest = serde_json::from_slice(&bytes).map_err(watch_error)?;
    Ok(request.instance == instance)
}

fn open_instance_lock(directory: &StoreDirectory, create: bool) -> Result<Option<File>> {
    if directory.read_optional(Path::new(LOCK), 0)?.is_none() {
        if !create {
            return Ok(None);
        }
        if let Err(error) = directory.write_atomic(Path::new(LOCK), &[], Publication::CreateOnly)
            && directory.read_optional(Path::new(LOCK), 0)?.is_none()
        {
            return Err(error);
        }
    }
    // Revalidates the retained private parent/DACL. State from a
    // different local user cannot serve as a stop request or queue authority.
    read_private_file_at(directory, LOCK, 0)?;
    directory.open_regular_read(Path::new(LOCK), 0).map(Some)
}

fn state_directory(root: &Path, create: bool) -> Result<Option<StoreDirectory>> {
    let registry = kio_index::registry::default_registry_path().map_err(crate::index_to_kio)?;
    let data = registry
        .parent()
        .ok_or_else(|| watch_error("device data directory is unavailable"))?;
    let key = kio_core::cas::lower_hex(&Sha256::digest(root.as_os_str().as_encoded_bytes()));
    let target = data.join("watch").join(key);
    if target.starts_with(root) {
        return Err(watch_error(
            "watch device state must be outside the watched root",
        ));
    }
    // `default_registry_path` can be inherited through a descriptor-backed
    // XDG directory. Keep that descriptor capability through private-state's
    // resolver; canonicalizing and reopening a diagnostic pathname here would
    // make a later rename select attacker-controlled state instead.
    if create {
        crate::private_state::ensure_private_directory(&target)
            .map(Some)
            .map_err(watch_error)
    } else {
        crate::private_state::open_readonly(&target).map_err(watch_error)
    }
}

fn epoch_ms(time: SystemTime) -> Result<u64> {
    time.duration_since(UNIX_EPOCH)
        .map_err(watch_error)?
        .as_millis()
        .try_into()
        .map_err(watch_error)
}

fn watch_error(error: impl std::fmt::Display) -> KioError {
    KioError::new(
        "KIO-E-WATCH-001",
        error.to_string(),
        json!({}),
        ExitCode::Failure,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incomplete_output_reason_is_bounded_and_identifies_child_discovery() {
        let mut output = json!({
            "status": "indexed",
            "error_code": "KIO-E-INDEX-PARTIAL-001",
            "child_scope_discovery": {"has_more": true, "rows_total": 128},
        });
        crate::set_exit_override(&mut output, ExitCode::PartialFailure);
        assert_eq!(
            incomplete_output_reason(&output),
            "status=indexed error_code=KIO-E-INDEX-PARTIAL-001 exit_code=3 child_discovery_has_more=true child_discovery_rows_total=128"
        );
    }
}
