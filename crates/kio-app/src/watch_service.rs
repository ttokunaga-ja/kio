//! Per-user native scheduler registration for the offline foreground watcher.
//!
//! The native definition is an execution convenience only.  The watch instance
//! lock remains the liveness authority; scheduler state can only describe what
//! the OS has registered or been asked to start.

use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use kio_core::management::{ManagementAuthority, read_record};
use kio_core::store_dir::{Publication, StoreDirectory};
use kio_core::{ExitCode, KioError, Result};
#[cfg(windows)]
use kio_process::wait_supervised_child;
use kio_process::{BoundedProcessOptions, BoundedProcessOutput, run_bounded_command};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::commands::WatchServiceCommand;
use crate::management::{binding_for_repo, open_existing_managed_root};

const MANIFEST_DIRECTORY: &str = "watch-services";
const MUTATION_LOCK_DIRECTORY: &str = "watch-services/locks";
const MAX_MANIFEST_BYTES: u64 = 32 * 1024;
const VERSION: u8 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceManifest {
    version: u8,
    id: String,
    scope_id: String,
    root: PathBuf,
    binary: PathBuf,
    reconcile_interval_seconds: u64,
    native_path: PathBuf,
    definition: String,
    environment: ServiceEnvironment,
    phase: ServicePhase,
}

/// The scheduler has no useful inherited shell environment.  Persist the
/// complete small environment the child needs so starting from login uses the
/// same device-private Kio directories as an interactive invocation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ServiceEnvironment {
    home: PathBuf,
    userprofile: PathBuf,
    xdg_config_home: PathBuf,
    xdg_data_home: PathBuf,
    xdg_cache_home: PathBuf,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ServicePhase {
    Installing,
    Installed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeRegistration {
    Absent,
    Owned,
    Foreign,
}

pub(crate) fn run(root_argument: Option<PathBuf>, command: WatchServiceCommand) -> Result<Value> {
    match command {
        WatchServiceCommand::Install {
            reconcile_interval_seconds,
        } => install(root_argument, reconcile_interval_seconds),
        WatchServiceCommand::Start => mutate_existing(root_argument, Lifecycle::Start),
        WatchServiceCommand::Stop => mutate_existing(root_argument, Lifecycle::Stop),
        WatchServiceCommand::Status => status(root_argument),
        WatchServiceCommand::Uninstall => mutate_existing(root_argument, Lifecycle::Uninstall),
        WatchServiceCommand::Runner { manifest } => run_runner(manifest),
    }
}

/// Start the Windows Task Scheduler child only after re-binding its private
/// manifest to this executable, the native task, and the managed root.
///
/// The scheduler does not provide a trustworthy inherited environment.  This
/// runner therefore sets the captured device paths on the child `Command`
/// rather than mutating this process environment.
#[cfg(windows)]
fn run_runner(manifest_path: PathBuf) -> Result<Value> {
    let manifest = load_runner_manifest(&manifest_path)?;
    let current_binary = env::current_exe()
        .map_err(service_error)?
        .canonicalize()
        .map_err(service_error)?;
    let current_binary = kio_core::private_fs::verify_trusted_executable(&current_binary)?;
    let manifest_binary = kio_core::private_fs::verify_trusted_executable(&manifest.binary)?;
    if current_binary != manifest_binary {
        return Err(service_error(
            "watch service runner executable does not match its manifest",
        ));
    }
    let root = manifest.root.canonicalize().map_err(service_error)?;
    if root != manifest.root {
        return Err(service_error(
            "watch service root no longer matches its canonical manifest path",
        ));
    }
    let repo = open_existing_managed_root(&root)?;
    let binding = binding_for_repo(&repo)?;
    let record = read_record(&binding)?;
    if record.authority != ManagementAuthority::Root || record.scope_id != manifest.scope_id {
        return Err(service_error(
            "watch service manifest no longer matches the managed root authority",
        ));
    }
    let scheduler = NativeScheduler;
    if scheduler.inspect(&manifest)? != NativeRegistration::Owned
        || !native_runner_registration_matches(&manifest)?
    {
        return Err(service_error(
            "watch service runner refuses an absent or foreign native registration",
        ));
    }
    let mut child = watcher_child_command(&manifest)?;
    let status = wait_supervised_child(&mut child).map_err(service_error)?;
    if !status.success() {
        return Err(service_error(format!(
            "watch service child exited unsuccessfully: {status}"
        )));
    }
    Ok(json!({"status":"watcher_exited", "service_id":manifest.id}))
}

#[cfg(not(windows))]
fn run_runner(_manifest_path: PathBuf) -> Result<Value> {
    Err(invalid(
        "watch service runner is reserved for the Windows Task Scheduler",
    ))
}

#[cfg(windows)]
fn load_runner_manifest(manifest_path: &Path) -> Result<ServiceManifest> {
    if !manifest_path.is_absolute() {
        return Err(invalid("watch service manifest path must be absolute"));
    }
    // This first read proves every ancestor and the leaf are owner-private
    // without trusting scheduler-provided environment variables.
    let private_bytes = kio_core::private_fs::read_private_file(manifest_path, MAX_MANIFEST_BYTES)?;
    let manifest: ServiceManifest =
        serde_json::from_slice(&private_bytes).map_err(service_error)?;
    validate_manifest(&manifest)?;
    validate_runner_manifest_locator(manifest_path, &manifest)?;
    let parent = manifest_path
        .parent()
        .ok_or_else(|| service_error("watch service manifest has no parent"))?;
    let leaf = manifest_path
        .file_name()
        .ok_or_else(|| service_error("watch service manifest has no file name"))?;
    // Retain the verified parent before opening the leaf a second time.  The
    // byte comparison catches a replacement between the private-file audit
    // and capability-relative read.
    let store = StoreDirectory::open(parent)?;
    let file = store.open_regular_read(Path::new(leaf), MAX_MANIFEST_BYTES)?;
    store.ensure_owner_private(Path::new(leaf))?;
    let mut retained_bytes = Vec::new();
    file.take(MAX_MANIFEST_BYTES.saturating_add(1))
        .read_to_end(&mut retained_bytes)
        .map_err(service_error)?;
    if retained_bytes.len() > MAX_MANIFEST_BYTES as usize {
        return Err(service_error(
            "watch service manifest exceeds its byte limit",
        ));
    }
    if retained_bytes != private_bytes {
        return Err(service_error(
            "watch service manifest changed while the runner was binding it",
        ));
    }
    Ok(manifest)
}

#[cfg(windows)]
fn watcher_child_command(manifest: &ServiceManifest) -> Result<Command> {
    let mut command = Command::new(&manifest.binary);
    command.args(watch_argv(manifest)?.into_iter().skip(1));
    for (name, value) in service_environment_pairs(&manifest.environment)? {
        command.env(name, value);
    }
    Ok(command)
}

#[cfg(any(windows, test))]
fn validate_runner_manifest_locator(
    manifest_path: &Path,
    manifest: &ServiceManifest,
) -> Result<()> {
    if manifest_path != manifest_locator(&manifest.id, &manifest.environment)? {
        return Err(service_error(
            "watch service runner manifest locator does not bind its environment and identifier",
        ));
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn runner_argv(manifest_path: &Path) -> Result<Vec<String>> {
    let manifest_path = manifest_path
        .to_str()
        .ok_or_else(|| invalid("watch service manifest path is not valid UTF-8"))?;
    Ok(vec![
        "watch".into(),
        "service".into(),
        "runner".into(),
        "--manifest".into(),
        manifest_path.to_owned(),
    ])
}

fn install(root_argument: Option<PathBuf>, interval: u64) -> Result<Value> {
    if !(1..=86_400).contains(&interval) {
        return Err(invalid(
            "reconciliation interval must be 1 through 86400 seconds",
        ));
    }
    let root = resolve_root(root_argument, true)?;
    let repo = open_existing_managed_root(&root)?;
    let binding = binding_for_repo(&repo)?;
    let record = read_record(&binding)?;
    if record.authority != ManagementAuthority::Root {
        return Err(invalid(
            "watch service requires an explicitly registered management root",
        ));
    }
    let binary = env::current_exe()
        .map_err(service_error)?
        .canonicalize()
        .map_err(service_error)?;
    let binary = kio_core::private_fs::verify_trusted_executable(&binary)?;
    validate_argument_path(&root)?;
    validate_argument_path(&binary)?;
    let id = stable_id(&record.scope_id, &root);
    let environment = capture_environment()?;
    let manifest_path = manifest_locator(&id, &environment)?;
    let native_path = native_definition_path(&id, &environment)?;
    let definition =
        render_definition(&id, &binary, &root, interval, &environment, &manifest_path)?;
    let manifest = ServiceManifest {
        version: VERSION,
        id: id.clone(),
        scope_id: record.scope_id,
        root,
        binary,
        reconcile_interval_seconds: interval,
        native_path,
        definition,
        environment,
        phase: ServicePhase::Installing,
    };
    let _native_location = NativeDefinitionLocation::admit(&manifest.native_path)?;
    let store =
        manifest_store(true)?.ok_or_else(|| service_error("device manifest store unavailable"))?;
    let leaf = manifest_leaf(&manifest.id)?;
    let _mutation_lock = acquire_mutation_lock(&store, &manifest.id)?;
    if let Some(bytes) = store.read_optional(&leaf, MAX_MANIFEST_BYTES)? {
        let existing: ServiceManifest = serde_json::from_slice(&bytes).map_err(service_error)?;
        validate_manifest(&existing)?;
        if existing.root != manifest.root || existing.scope_id != manifest.scope_id {
            return Err(service_error(
                "watch service identity is already bound to another root",
            ));
        }
        if !same_installation(&existing, &manifest) {
            return Err(service_error(
                "watch service is already recorded with a different executable or environment; inspect or uninstall it first",
            ));
        }
        return recover_install(&store, &leaf, existing, &NativeScheduler);
    }
    let scheduler = NativeScheduler;
    match scheduler.inspect(&manifest)? {
        NativeRegistration::Absent => {}
        NativeRegistration::Owned => {
            return Err(service_error(
                "watch service registration already exists without a manifest",
            ));
        }
        NativeRegistration::Foreign => {
            return Err(service_error(
                "refusing to replace a foreign native service registration",
            ));
        }
    }
    // Persist intent before touching the scheduler: a crash leaves an auditable,
    // removable installing record rather than an unowned registration.
    write_manifest(&store, &leaf, &manifest, Publication::CreateOnly)?;
    store.ensure_owner_private(&leaf)?;
    recover_install(&store, &leaf, manifest, &scheduler)
}

fn same_installation(left: &ServiceManifest, right: &ServiceManifest) -> bool {
    left.version == right.version
        && left.id == right.id
        && left.scope_id == right.scope_id
        && left.root == right.root
        && left.binary == right.binary
        && left.reconcile_interval_seconds == right.reconcile_interval_seconds
        && left.native_path == right.native_path
        && left.definition == right.definition
        && left.environment == right.environment
}

/// Complete a previously persisted install intent. This is deliberately
/// idempotent: an interruption after either the manifest write or native
/// registration can be resumed without replacing a service owned by somebody
/// else.
fn recover_install<S: Scheduler>(
    store: &StoreDirectory,
    leaf: &Path,
    mut manifest: ServiceManifest,
    scheduler: &S,
) -> Result<Value> {
    match scheduler.inspect(&manifest)? {
        NativeRegistration::Absent => scheduler.install(&manifest)?,
        NativeRegistration::Owned => {}
        NativeRegistration::Foreign => {
            return Err(service_error(
                "refusing to replace a foreign native service registration",
            ));
        }
    }
    manifest.phase = ServicePhase::Installed;
    write_manifest(store, leaf, &manifest, Publication::Replace)?;
    store.ensure_owner_private(leaf)?;
    Ok(json!({
        "status": "installed",
        "service_id": manifest.id,
        "root": manifest.root,
        "native_registration": "enabled_not_started",
    }))
}

#[derive(Clone, Copy)]
enum Lifecycle {
    Start,
    Stop,
    Uninstall,
}

fn mutate_existing(root_argument: Option<PathBuf>, lifecycle: Lifecycle) -> Result<Value> {
    let (store, leaf, manifest) = find_manifest(root_argument)?;
    let _native_location = NativeDefinitionLocation::admit(&manifest.native_path)?;
    let _mutation_lock = acquire_mutation_lock(&store, &manifest.id)?;
    // Re-read only after the retained OS lock is held.  A lifecycle operation
    // must never act on a manifest that a concurrent uninstall has replaced.
    let bytes = store
        .read_optional(&leaf, MAX_MANIFEST_BYTES)?
        .ok_or_else(|| {
            service_error("watch service manifest disappeared during lifecycle operation")
        })?;
    let manifest: ServiceManifest = serde_json::from_slice(&bytes).map_err(service_error)?;
    validate_manifest(&manifest)?;
    let scheduler = NativeScheduler;
    let registration = scheduler.inspect(&manifest)?;
    if registration == NativeRegistration::Foreign {
        return Err(service_error(
            "native registration no longer matches its owned manifest",
        ));
    }
    match lifecycle {
        Lifecycle::Start => {
            if !manifest.root.is_dir() {
                return Err(service_error(
                    "watch root no longer exists; refusing to start service",
                ));
            }
            transition_checked(&scheduler, &manifest, lifecycle, registration)?;
            Ok(json!({"status":"start_requested", "service_id":manifest.id, "root":manifest.root}))
        }
        Lifecycle::Stop => {
            // Disable scheduler restart before asking the foreground instance to
            // stop; otherwise a failed process may be restarted during shutdown.
            transition_checked(&scheduler, &manifest, lifecycle, registration)?;
            Ok(json!({"status":"stopped", "service_id":manifest.id, "root":manifest.root}))
        }
        Lifecycle::Uninstall => {
            transition_checked(&scheduler, &manifest, lifecycle, registration)?;
            let bytes = serde_json::to_vec(&manifest).map_err(service_error)?;
            store.quarantine_then_remove(&leaf, &bytes, MAX_MANIFEST_BYTES)?;
            Ok(json!({"status":"uninstalled", "service_id":manifest.id, "root":manifest.root}))
        }
    }
}

fn transition_checked<S: Scheduler>(
    scheduler: &S,
    manifest: &ServiceManifest,
    lifecycle: Lifecycle,
    registration: NativeRegistration,
) -> Result<()> {
    if registration == NativeRegistration::Foreign {
        return Err(service_error(
            "native registration no longer matches its owned manifest",
        ));
    }
    match lifecycle {
        Lifecycle::Start if registration != NativeRegistration::Owned => Err(service_error(
            "native registration is absent; reinstall the watch service",
        )),
        Lifecycle::Start => scheduler.start(manifest),
        // A prior interrupted stop or uninstall can already have removed the
        // native registration. Both operations remain safe to retry.
        Lifecycle::Stop if registration == NativeRegistration::Absent => Ok(()),
        Lifecycle::Stop => scheduler.stop(manifest),
        Lifecycle::Uninstall if registration == NativeRegistration::Absent => Ok(()),
        Lifecycle::Uninstall => scheduler.uninstall(manifest),
    }
}

fn status(root_argument: Option<PathBuf>) -> Result<Value> {
    let (_, _, manifest) = find_manifest(root_argument)?;
    let scheduler = NativeScheduler;
    let registration = scheduler.inspect(&manifest)?;
    if registration == NativeRegistration::Foreign {
        return Err(service_error(
            "native registration does not match its owned manifest",
        ));
    }
    let native_state = scheduler.state(&manifest, registration)?;
    // The device-private watcher lock is still authoritative even if a user
    // removed the root after installing the service.
    let liveness = crate::watch_command::inspect_liveness(&manifest.root)?;
    Ok(json!({
        "status":"service",
        "service_id":manifest.id,
        "root":manifest.root,
        "install_phase":manifest.phase,
        "native_registration":native_state,
        "instance":liveness,
    }))
}

fn find_manifest(
    root_argument: Option<PathBuf>,
) -> Result<(StoreDirectory, PathBuf, ServiceManifest)> {
    let requested = resolve_root(root_argument, false)?;
    let store = manifest_store(false)?
        .ok_or_else(|| service_error("no watch service is installed for this root"))?;
    let Some(entries) = store.entries_optional(Path::new(MANIFEST_DIRECTORY))? else {
        return Err(service_error("no watch service is installed for this root"));
    };
    let mut found = Vec::new();
    for entry in entries {
        if !entry.is_regular_file || entry.is_directory {
            continue;
        }
        let name = entry.name.to_string_lossy();
        if !name.ends_with(".json") || name.len() > 80 {
            continue;
        }
        let leaf = Path::new(MANIFEST_DIRECTORY).join(name.as_ref());
        let Some(bytes) = store.read_optional(&leaf, MAX_MANIFEST_BYTES)? else {
            continue;
        };
        let manifest: ServiceManifest = serde_json::from_slice(&bytes).map_err(service_error)?;
        validate_manifest(&manifest)?;
        if leaf != manifest_leaf(&manifest.id)? {
            return Err(service_error(
                "watch service manifest leaf does not bind its identifier",
            ));
        }
        if manifest.root == requested {
            found.push((leaf, manifest));
        }
    }
    match found.len() {
        1 => {
            let (leaf, manifest) = found.pop().expect("checked length");
            Ok((store, leaf, manifest))
        }
        0 => Err(service_error("no watch service is installed for this root")),
        _ => Err(service_error("ambiguous watch service manifests for root")),
    }
}

fn resolve_root(root_argument: Option<PathBuf>, require_existing: bool) -> Result<PathBuf> {
    let cwd = crate::context_working_directory()?;
    let path = root_argument.unwrap_or(cwd.clone());
    let absolute = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    let lexical = lexical_absolute(&absolute)?;
    if require_existing {
        lexical
            .canonicalize()
            .map_err(|error| KioError::io(error.to_string(), lexical.display().to_string()))
    } else if lexical.exists() {
        lexical.canonicalize().map_err(service_error)
    } else {
        Ok(lexical)
    }
}

fn lexical_absolute(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err(invalid(
            "watch service root must resolve to an absolute path",
        ));
    }
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                clean.push(component.as_os_str())
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !clean.pop() {
                    return Err(invalid("watch service root escapes its filesystem root"));
                }
            }
        }
    }
    Ok(clean)
}

fn manifest_store(create: bool) -> Result<Option<StoreDirectory>> {
    let registry = kio_index::registry::default_registry_path().map_err(crate::index_to_kio)?;
    let Some(data) = registry.parent() else {
        return Err(service_error("device data directory is unavailable"));
    };
    if !data.exists() {
        if !create {
            return Ok(None);
        }
        fs::create_dir_all(data).map_err(service_error)?;
    }
    let store = StoreDirectory::open(data)?;
    if create {
        store.create_directory_all(Path::new(MANIFEST_DIRECTORY))?;
    }
    Ok(Some(store))
}

fn manifest_leaf(id: &str) -> Result<PathBuf> {
    if !id.starts_with("io.kio.watch.")
        || id.len() > 80
        || !id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.')
    {
        return Err(service_error("unsafe watch service identifier"));
    }
    Ok(Path::new(MANIFEST_DIRECTORY).join(format!("{id}.json")))
}

/// The scheduler receives this absolute locator as an argument.  It is
/// derived from the manifest's captured data directory, never from the
/// scheduler process environment.
fn manifest_locator(id: &str, environment: &ServiceEnvironment) -> Result<PathBuf> {
    let leaf = manifest_leaf(id)?;
    let path = environment.xdg_data_home.join("kio").join(leaf);
    validate_argument_path(&path)?;
    Ok(path)
}

fn stable_id(scope_id: &str, root: &Path) -> String {
    let mut hash = Sha256::new();
    hash.update(scope_id.as_bytes());
    hash.update([0]);
    hash.update(root.as_os_str().as_encoded_bytes());
    format!(
        "io.kio.watch.{}",
        kio_core::cas::lower_hex(&hash.finalize())[..32].to_owned()
    )
}

fn validate_manifest(manifest: &ServiceManifest) -> Result<()> {
    if manifest.version != VERSION
        || manifest.reconcile_interval_seconds == 0
        || manifest.reconcile_interval_seconds > 86_400
    {
        return Err(service_error("watch service manifest is invalid"));
    }
    manifest_leaf(&manifest.id)?;
    validate_argument_path(&manifest.root)?;
    validate_argument_path(&manifest.binary)?;
    validate_environment(&manifest.environment)?;
    if manifest.definition.len() > 16_384 || manifest.definition.contains('\0') {
        return Err(service_error("watch service definition is invalid"));
    }
    if manifest.id != stable_id(&manifest.scope_id, &manifest.root)
        || manifest.native_path != native_definition_path(&manifest.id, &manifest.environment)?
        || manifest.definition
            != render_definition(
                &manifest.id,
                &manifest.binary,
                &manifest.root,
                manifest.reconcile_interval_seconds,
                &manifest.environment,
                &manifest_locator(&manifest.id, &manifest.environment)?,
            )?
    {
        return Err(service_error(
            "watch service manifest does not match its derived identity",
        ));
    }
    Ok(())
}

fn capture_environment() -> Result<ServiceEnvironment> {
    let home = environment_path_optional("HOME")?
        .or(environment_path_optional("USERPROFILE")?)
        .ok_or_else(|| service_error("HOME and USERPROFILE are unavailable"))?;
    let userprofile = environment_path_optional("USERPROFILE")?.unwrap_or_else(|| home.clone());
    let xdg_config_home =
        environment_path_optional("XDG_CONFIG_HOME")?.unwrap_or_else(|| home.join(".config"));
    let xdg_data_home =
        environment_path_optional("XDG_DATA_HOME")?.unwrap_or_else(|| home.join(".local/share"));
    let xdg_cache_home =
        environment_path_optional("XDG_CACHE_HOME")?.unwrap_or_else(|| home.join(".cache"));
    let environment = ServiceEnvironment {
        home,
        userprofile,
        xdg_config_home,
        xdg_data_home,
        xdg_cache_home,
    };
    validate_environment(&environment)?;
    Ok(environment)
}

fn environment_path_optional(name: &str) -> Result<Option<PathBuf>> {
    let Some(value) = env::var_os(name) else {
        return Ok(None);
    };
    if value.to_str().is_none() {
        return Err(invalid(format!("{name} is not valid UTF-8")));
    }
    let path = PathBuf::from(value);
    validate_argument_path(&path)?;
    Ok(Some(path))
}

fn validate_environment(environment: &ServiceEnvironment) -> Result<()> {
    for path in [
        &environment.home,
        &environment.userprofile,
        &environment.xdg_config_home,
        &environment.xdg_data_home,
        &environment.xdg_cache_home,
    ] {
        validate_argument_path(path)?;
    }
    Ok(())
}

fn service_environment_pairs(
    environment: &ServiceEnvironment,
) -> Result<Vec<(&'static str, String)>> {
    Ok(vec![
        (
            "HOME",
            environment
                .home
                .to_str()
                .ok_or_else(|| invalid("HOME is not valid UTF-8"))?
                .to_owned(),
        ),
        (
            "USERPROFILE",
            environment
                .userprofile
                .to_str()
                .ok_or_else(|| invalid("USERPROFILE is not valid UTF-8"))?
                .to_owned(),
        ),
        (
            "XDG_CONFIG_HOME",
            environment
                .xdg_config_home
                .to_str()
                .ok_or_else(|| invalid("XDG_CONFIG_HOME is not valid UTF-8"))?
                .to_owned(),
        ),
        (
            "XDG_DATA_HOME",
            environment
                .xdg_data_home
                .to_str()
                .ok_or_else(|| invalid("XDG_DATA_HOME is not valid UTF-8"))?
                .to_owned(),
        ),
        (
            "XDG_CACHE_HOME",
            environment
                .xdg_cache_home
                .to_str()
                .ok_or_else(|| invalid("XDG_CACHE_HOME is not valid UTF-8"))?
                .to_owned(),
        ),
    ])
}

fn write_manifest(
    store: &StoreDirectory,
    leaf: &Path,
    manifest: &ServiceManifest,
    publication: Publication,
) -> Result<()> {
    let bytes = serde_json::to_vec(manifest).map_err(service_error)?;
    store.write_atomic(leaf, &bytes, publication)
}

fn acquire_mutation_lock(store: &StoreDirectory, id: &str) -> Result<File> {
    let leaf = Path::new(MUTATION_LOCK_DIRECTORY).join(format!("{id}.lock"));
    store.create_directory_all(Path::new(MUTATION_LOCK_DIRECTORY))?;
    if store.read_optional(&leaf, 0)?.is_none()
        && let Err(error) = store.write_atomic(&leaf, &[], Publication::CreateOnly)
        && store.read_optional(&leaf, 0)?.is_none()
    {
        return Err(error);
    }
    store.ensure_owner_private(&leaf)?;
    let lock = store.open_regular_read(&leaf, 0)?;
    lock.try_lock().map_err(|error| {
        service_error(format!(
            "watch service lifecycle is already being changed: {error}"
        ))
    })?;
    Ok(lock)
}

fn validate_argument_path(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path.to_str().is_none()
        || path
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .any(|b| *b == b'\n' || *b == b'\r' || *b == 0)
    {
        return Err(invalid(
            "watch service path is not safe for a native scheduler definition",
        ));
    }
    Ok(())
}

trait Scheduler {
    fn inspect(&self, manifest: &ServiceManifest) -> Result<NativeRegistration>;
    fn install(&self, manifest: &ServiceManifest) -> Result<()>;
    fn start(&self, manifest: &ServiceManifest) -> Result<()>;
    fn stop(&self, manifest: &ServiceManifest) -> Result<()>;
    fn uninstall(&self, manifest: &ServiceManifest) -> Result<()>;
    fn state(&self, manifest: &ServiceManifest, registration: NativeRegistration) -> Result<Value>;
}

#[cfg(test)]
fn install_checked<S: Scheduler>(scheduler: &S, manifest: &ServiceManifest) -> Result<()> {
    match scheduler.inspect(manifest)? {
        NativeRegistration::Absent => scheduler.install(manifest),
        NativeRegistration::Owned => Err(service_error(
            "native registration already exists without a manifest",
        )),
        NativeRegistration::Foreign => Err(service_error(
            "refusing to replace a foreign native service registration",
        )),
    }
}

/// An admitted native-definition location. Missing suffixes are recorded only
/// after the nearest existing ancestor has passed the full trust check. No
/// lookup, lock, or publication may treat an unsafe parent as benign absence.
struct NativeDefinitionLocation {
    directory: StoreDirectory,
    missing: Vec<PathBuf>,
    leaf: PathBuf,
}

impl NativeDefinitionLocation {
    fn admit(path: &Path) -> Result<Self> {
        if !path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
        {
            return Err(service_error(
                "native definition path must be absolute and normalized",
            ));
        }
        let leaf = path
            .file_name()
            .ok_or_else(|| service_error("native definition has no leaf"))?;
        let mut parent = path
            .parent()
            .ok_or_else(|| service_error("native definition has no parent"))?;
        let mut missing = Vec::new();
        loop {
            match fs::symlink_metadata(parent) {
                Ok(_) => break,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    missing.push(PathBuf::from(parent.file_name().ok_or_else(|| {
                        service_error("native definition has no existing ancestor")
                    })?));
                    parent = parent.parent().ok_or_else(|| {
                        service_error("native definition has no existing ancestor")
                    })?;
                }
                Err(error) => return Err(service_error(error)),
            }
        }
        let directory = kio_core::private_fs::verify_private_creation_parent(parent)?;
        missing.reverse();
        Ok(Self {
            directory,
            missing,
            leaf: PathBuf::from(leaf),
        })
    }

    fn revalidate(&self) -> Result<()> {
        let current = kio_core::private_fs::verify_private_creation_parent(self.directory.path())?;
        if kio_core::management::directory_identity_from_handle(&current.root_handle())?
            != kio_core::management::directory_identity_from_handle(&self.directory.root_handle())?
        {
            return Err(service_error("native definition parent changed identity"));
        }
        Ok(())
    }

    fn create_parents(&mut self) -> Result<()> {
        self.revalidate()?;
        for component in std::mem::take(&mut self.missing) {
            // Create-only: never repair or adopt a concurrently supplied path.
            let handle = self.directory.create_directory(&component)?;
            kio_core::store_dir::restrict_new_private_directory(&handle)?;
            self.directory =
                StoreDirectory::from_retained(handle, self.directory.path().join(component))?;
            self.revalidate()?;
        }
        Ok(())
    }

    fn inspect(&self, expected: &[u8]) -> Result<NativeRegistration> {
        self.revalidate()?;
        if !self.missing.is_empty() || !self.directory.contains_entry(&self.leaf)? {
            return Ok(NativeRegistration::Absent);
        }
        let mut file = match self.directory.open_regular_read(&self.leaf, 64 * 1024) {
            Ok(file) => file,
            Err(_) => return Ok(NativeRegistration::Foreign),
        };
        if kio_core::private_fs::verify_trusted_owned_regular_file_handle(&file).is_err() {
            return Ok(NativeRegistration::Foreign);
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(64 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(service_error)?;
        if bytes != expected
            || kio_core::private_fs::verify_trusted_owned_regular_file_handle(&file).is_err()
        {
            return Ok(NativeRegistration::Foreign);
        }
        self.revalidate()?;
        Ok(NativeRegistration::Owned)
    }

    fn require_owned(&self, manifest: &ServiceManifest) -> Result<()> {
        if self.inspect(manifest.definition.as_bytes())? != NativeRegistration::Owned {
            return Err(service_error(
                "native definition no longer matches its owned manifest",
            ));
        }
        Ok(())
    }

    fn publish(&mut self, expected: &[u8]) -> Result<()> {
        if self.inspect(expected)? != NativeRegistration::Absent {
            return Err(service_error(
                "native watch registration changed during install",
            ));
        }
        self.create_parents()?;
        self.directory
            .write_atomic(&self.leaf, expected, Publication::CreateOnly)?;
        self.revalidate()
    }
}

struct NativeScheduler;
impl Scheduler for NativeScheduler {
    fn inspect(&self, manifest: &ServiceManifest) -> Result<NativeRegistration> {
        let location = NativeDefinitionLocation::admit(&manifest.native_path)?;
        let registration = location.inspect(manifest.definition.as_bytes())?;
        if registration != NativeRegistration::Owned {
            return Ok(registration);
        }
        let matches = native_loaded_definition_matches(manifest)?;
        location.require_owned(manifest)?;
        Ok(if matches {
            NativeRegistration::Owned
        } else {
            NativeRegistration::Foreign
        })
    }
    fn install(&self, manifest: &ServiceManifest) -> Result<()> {
        let mut location = NativeDefinitionLocation::admit(&manifest.native_path)?;
        location.publish(manifest.definition.as_bytes())?;
        location.require_owned(manifest)?;
        native_enable(manifest)?;
        location.require_owned(manifest)
    }
    fn start(&self, manifest: &ServiceManifest) -> Result<()> {
        let location = NativeDefinitionLocation::admit(&manifest.native_path)?;
        location.require_owned(manifest)?;
        native_start(manifest)?;
        location.require_owned(manifest)
    }
    fn stop(&self, manifest: &ServiceManifest) -> Result<()> {
        let location = NativeDefinitionLocation::admit(&manifest.native_path)?;
        location.require_owned(manifest)?;
        native_stop(manifest)?;
        location.require_owned(manifest)
    }
    fn uninstall(&self, manifest: &ServiceManifest) -> Result<()> {
        let location = NativeDefinitionLocation::admit(&manifest.native_path)?;
        location.require_owned(manifest)?;
        native_uninstall(manifest)?;
        // Recheck the retained XML/plist/unit cache without querying a Windows
        // registration that native_uninstall has intentionally just removed.
        location.require_owned(manifest)?;
        location.directory.remove_file(&location.leaf)?;
        native_after_remove()?;
        location.revalidate()
    }
    fn state(&self, manifest: &ServiceManifest, registration: NativeRegistration) -> Result<Value> {
        let location = NativeDefinitionLocation::admit(&manifest.native_path)?;
        if location.inspect(manifest.definition.as_bytes())? != registration {
            return Err(service_error("native registration changed during status"));
        }
        match registration {
            NativeRegistration::Absent => Ok(json!({"registered":false, "active":false})),
            NativeRegistration::Foreign => Err(service_error("foreign native registration")),
            NativeRegistration::Owned => {
                let state = native_state(manifest)?;
                location.require_owned(manifest)?;
                Ok(state)
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn native_loaded_definition_matches(manifest: &ServiceManifest) -> Result<bool> {
    let output = run_native_output(
        "/bin/launchctl",
        [
            "print".into(),
            format!("gui/{}/{}", unsafe { libc::geteuid() }, manifest.id),
        ],
    )?;
    // A stopped service is deliberately booted out, leaving only our verified
    // plist. A successful query, however, must bind the loaded service to the
    // exact executable and root before it can be controlled or deleted.
    if !output.status.success() {
        // `launchctl print` uses exit 113 for a genuinely unloaded service.
        // Other failures are an ambiguous manager error and must fail closed.
        if output.status.code() == Some(113)
            && (output.stderr.contains("Could not find service")
                || output.stderr.contains("Bad request"))
        {
            return Ok(true);
        }
        return Err(service_error(
            "launchctl could not inspect native watch registration",
        ));
    }
    let expected = watch_argv(manifest)?;
    let path = manifest
        .native_path
        .to_str()
        .ok_or_else(|| service_error("non-UTF-8 native definition path"))?;
    let binary = manifest
        .binary
        .to_str()
        .ok_or_else(|| service_error("non-UTF-8 watch binary path"))?;
    let lines = output.stdout.lines().map(str::trim).collect::<Vec<_>>();
    let exact_path = lines.iter().any(|line| *line == format!("path = {path}"));
    let exact_program = lines
        .iter()
        .any(|line| *line == format!("program = {binary}"));
    let Some(start) = lines.iter().position(|line| *line == "arguments = {") else {
        return Ok(false);
    };
    let Some(end) = lines[start + 1..].iter().position(|line| *line == "}") else {
        return Ok(false);
    };
    let observed = lines[start + 1..start + 1 + end]
        .iter()
        .map(|line| {
            // launchctl's verified output prints one bare argv value per line.
            // Any assignment, escaping, or blank line is ambiguous and rejected.
            (!line.is_empty() && !line.contains(" = ")).then(|| (*line).to_owned())
        })
        .collect::<Option<Vec<_>>>();
    let environment = service_environment_pairs(&manifest.environment)?;
    let environment_matches = environment.iter().all(|(name, value)| {
        lines
            .iter()
            .any(|line| *line == format!("{name} => {value}"))
    });
    Ok(exact_path
        && exact_program
        && environment_matches
        && observed.as_deref() == Some(expected.as_slice()))
}

#[cfg(any(windows, target_os = "macos"))]
fn watch_argv(manifest: &ServiceManifest) -> Result<Vec<String>> {
    Ok(vec![
        manifest
            .binary
            .to_str()
            .ok_or_else(|| service_error("non-UTF-8 watch binary path"))?
            .to_owned(),
        "watch".into(),
        "--root".into(),
        manifest
            .root
            .to_str()
            .ok_or_else(|| service_error("non-UTF-8 watch root path"))?
            .to_owned(),
        "run".into(),
        "--reconcile-interval-seconds".into(),
        manifest.reconcile_interval_seconds.to_string(),
    ])
}

#[cfg(all(unix, not(target_os = "macos")))]
fn native_loaded_definition_matches(manifest: &ServiceManifest) -> Result<bool> {
    let output = run_native_output(
        systemctl_path()?,
        [
            "--user".into(),
            "show".into(),
            "--property=FragmentPath,ExecStart,Environment".into(),
            manifest.id.clone(),
        ],
    )?;
    if !output.status.success() {
        return Err(service_error(
            "systemd could not inspect native watch registration",
        ));
    }
    let environment = service_environment_pairs(&manifest.environment)?;
    Ok(output
        .stdout
        .contains(manifest.native_path.to_string_lossy().as_ref())
        && output
            .stdout
            .contains(manifest.binary.to_string_lossy().as_ref())
        && output
            .stdout
            .contains(manifest.root.to_string_lossy().as_ref())
        && environment
            .iter()
            .all(|(name, value)| output.stdout.contains(&format!("{name}={value}"))))
}

#[cfg(windows)]
fn native_loaded_definition_matches(manifest: &ServiceManifest) -> Result<bool> {
    native_windows_task_definition_matches(manifest, true)
}

/// Runner startup has a stricter condition than lifecycle recovery: a missing
/// task can be safely uninstalled, but it cannot authorize a manually invoked
/// hidden runner to start a watcher.
#[cfg(windows)]
fn native_runner_registration_matches(manifest: &ServiceManifest) -> Result<bool> {
    native_windows_task_definition_matches(manifest, false)
}

#[cfg(windows)]
fn native_windows_task_definition_matches(
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

#[cfg(target_os = "macos")]
fn native_definition_path(id: &str, environment: &ServiceEnvironment) -> Result<PathBuf> {
    Ok(environment
        .home
        .join("Library/LaunchAgents")
        .join(format!("{id}.plist")))
}
#[cfg(all(unix, not(target_os = "macos")))]
fn native_definition_path(id: &str, environment: &ServiceEnvironment) -> Result<PathBuf> {
    Ok(environment
        .xdg_config_home
        .join("systemd/user")
        .join(format!("{id}.service")))
}
#[cfg(windows)]
fn native_definition_path(id: &str, environment: &ServiceEnvironment) -> Result<PathBuf> {
    let path = environment
        .xdg_data_home
        .join("kio/watch-services-native")
        .join(format!("{id}.xml"));
    validate_argument_path(&path)?;
    Ok(path)
}

#[cfg(target_os = "macos")]
fn render_definition(
    id: &str,
    binary: &Path,
    root: &Path,
    interval: u64,
    environment: &ServiceEnvironment,
    _manifest_path: &Path,
) -> Result<String> {
    let args = [
        binary.as_os_str().to_string_lossy().into_owned(),
        "watch".into(),
        "--root".into(),
        root.as_os_str().to_string_lossy().into_owned(),
        "run".into(),
        "--reconcile-interval-seconds".into(),
        interval.to_string(),
    ];
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>{}</string><key>ProgramArguments</key><array>{}</array><key>EnvironmentVariables</key><dict>{}</dict><key>RunAtLoad</key><true/><key>KeepAlive</key><true/></dict></plist>\n",
        xml_escape(id)?,
        args.iter()
            .map(|arg| Ok(format!("<string>{}</string>", xml_escape(arg)?)))
            .collect::<Result<Vec<_>>>()?
            .join(""),
        service_environment_pairs(environment)?
            .iter()
            .map(|(name, value)| Ok(format!(
                "<key>{}</key><string>{}</string>",
                xml_escape(name)?,
                xml_escape(value)?
            )))
            .collect::<Result<Vec<_>>>()?
            .join("")
    ))
}
#[cfg(all(unix, not(target_os = "macos")))]
fn render_definition(
    id: &str,
    binary: &Path,
    root: &Path,
    interval: u64,
    environment: &ServiceEnvironment,
    _manifest_path: &Path,
) -> Result<String> {
    let args = [
        binary.as_os_str().to_string_lossy().into_owned(),
        "watch".into(),
        "--root".into(),
        root.as_os_str().to_string_lossy().into_owned(),
        "run".into(),
        "--reconcile-interval-seconds".into(),
        interval.to_string(),
    ];
    Ok(format!(
        "[Unit]\nDescription=Kio offline watch {}\n\n[Service]\nType=simple\nEnvironment={}\nExecStart={}\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
        systemd_escape(id)?,
        service_environment_pairs(environment)?
            .iter()
            .map(|(name, value)| systemd_escape(&format!("{name}={value}")))
            .collect::<Result<Vec<_>>>()?
            .join(" "),
        args.iter()
            .map(|arg| systemd_escape(arg))
            .collect::<Result<Vec<_>>>()?
            .join(" ")
    ))
}
#[cfg(windows)]
fn render_definition(
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

#[cfg(target_os = "macos")]
fn native_enable(manifest: &ServiceManifest) -> Result<()> {
    // The per-user LaunchAgent directory is loaded at login. Installing only
    // writes the verified plist, so it cannot start this session's watcher.
    let _ = manifest;
    Ok(())
}
#[cfg(target_os = "macos")]
fn native_start(manifest: &ServiceManifest) -> Result<()> {
    let target = format!("gui/{}/{}", unsafe { libc::geteuid() }, manifest.id);
    let printed = run_native_output("/bin/launchctl", ["print".into(), target.clone()])?;
    if !printed.status.success() {
        run_native(
            "/bin/launchctl",
            [
                "bootstrap".into(),
                format!("gui/{}", unsafe { libc::geteuid() }),
                manifest.native_path.display().to_string(),
            ],
        )?;
    }
    run_native("/bin/launchctl", ["kickstart".into(), "-k".into(), target]).map(|_| ())
}
#[cfg(target_os = "macos")]
fn native_stop(manifest: &ServiceManifest) -> Result<()> {
    run_native(
        "/bin/launchctl",
        [
            "bootout".into(),
            format!("gui/{}", unsafe { libc::geteuid() }),
            manifest.native_path.display().to_string(),
        ],
    )
    .map(|_| ())
}
#[cfg(target_os = "macos")]
fn native_uninstall(manifest: &ServiceManifest) -> Result<()> {
    let target = format!("gui/{}/{}", unsafe { libc::geteuid() }, manifest.id);
    let printed = run_native_output("/bin/launchctl", ["print".into(), target])?;
    if printed.status.success() {
        native_stop(manifest)
    } else if printed.status.code() == Some(113)
        && (printed.stderr.contains("Could not find service")
            || printed.stderr.contains("Bad request"))
    {
        Ok(())
    } else {
        Err(service_error(
            "launchctl could not inspect service before uninstall",
        ))
    }
}
#[cfg(target_os = "macos")]
fn native_after_remove() -> Result<()> {
    Ok(())
}
#[cfg(target_os = "macos")]
fn native_state(manifest: &ServiceManifest) -> Result<Value> {
    let active = run_native_output(
        "/bin/launchctl",
        [
            "print".into(),
            format!("gui/{}/{}", unsafe { libc::geteuid() }, manifest.id),
        ],
    )?
    .status
    .success();
    Ok(json!({"registered":true,"active":active}))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn native_enable(manifest: &ServiceManifest) -> Result<()> {
    run_systemctl(["daemon-reload".into()])?;
    run_systemctl(["enable".into(), manifest.id.clone()]).map(|_| ())
}
#[cfg(all(unix, not(target_os = "macos")))]
fn native_start(manifest: &ServiceManifest) -> Result<()> {
    native_enable(manifest)?;
    run_systemctl(["start".into(), manifest.id.clone()]).map(|_| ())
}
#[cfg(all(unix, not(target_os = "macos")))]
fn native_stop(manifest: &ServiceManifest) -> Result<()> {
    run_systemctl(["disable".into(), manifest.id.clone()])?;
    run_systemctl(["stop".into(), manifest.id.clone()]).map(|_| ())
}
#[cfg(all(unix, not(target_os = "macos")))]
fn native_uninstall(manifest: &ServiceManifest) -> Result<()> {
    native_stop(manifest)
}
#[cfg(all(unix, not(target_os = "macos")))]
fn native_after_remove() -> Result<()> {
    run_systemctl(["daemon-reload".into()]).map(|_| ())
}
#[cfg(all(unix, not(target_os = "macos")))]
fn native_state(manifest: &ServiceManifest) -> Result<Value> {
    let enabled = run_native_output(
        systemctl_path()?,
        ["--user".into(), "is-enabled".into(), manifest.id.clone()],
    )?
    .status
    .success();
    let active = run_native_output(
        systemctl_path()?,
        ["--user".into(), "is-active".into(), manifest.id.clone()],
    )?
    .status
    .success();
    Ok(json!({"registered":enabled,"active":active}))
}

#[cfg(windows)]
fn native_enable(manifest: &ServiceManifest) -> Result<()> {
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
#[cfg(windows)]
fn native_start(manifest: &ServiceManifest) -> Result<()> {
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
#[cfg(windows)]
fn native_stop(manifest: &ServiceManifest) -> Result<()> {
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
#[cfg(windows)]
fn native_uninstall(manifest: &ServiceManifest) -> Result<()> {
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
#[cfg(windows)]
fn native_after_remove() -> Result<()> {
    Ok(())
}
#[cfg(windows)]
fn native_state(manifest: &ServiceManifest) -> Result<Value> {
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
    Ok(json!({"registered":true,"active":output.stdout.contains("Running")}))
}

fn run_native<I, P>(program: P, args: I) -> Result<BoundedProcessOutput>
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
fn run_native_output<I, P>(program: P, args: I) -> Result<BoundedProcessOutput>
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
            max_stdout_bytes: 16 * 1024,
            max_stderr_bytes: 16 * 1024,
        },
        None,
    )
    .map_err(service_error)
}
#[cfg(all(unix, not(target_os = "macos")))]
fn systemctl_path() -> Result<&'static str> {
    if Path::new("/usr/bin/systemctl").is_file() {
        Ok("/usr/bin/systemctl")
    } else if Path::new("/bin/systemctl").is_file() {
        Ok("/bin/systemctl")
    } else {
        Err(service_error("systemctl is unavailable"))
    }
}
#[cfg(windows)]
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
#[cfg(all(unix, not(target_os = "macos")))]
fn run_systemctl(args: impl IntoIterator<Item = String>) -> Result<BoundedProcessOutput> {
    run_native(
        systemctl_path()?,
        std::iter::once("--user".to_owned()).chain(args),
    )
}
fn bounded_output(output: &BoundedProcessOutput) -> String {
    output.stderr.chars().take(512).collect()
}

#[cfg(any(windows, target_os = "macos", test))]
fn xml_escape(value: &str) -> Result<String> {
    if value
        .chars()
        .any(|c| c == '\0' || c == '\r' || c == '\n' || (c.is_control() && c != '\t'))
    {
        return Err(invalid(
            "native scheduler value contains a control character",
        ));
    }
    Ok(value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\"', "&quot;")
        .replace('\'', "&apos;"))
}
#[cfg(all(unix, not(target_os = "macos")))]
fn systemd_escape(value: &str) -> Result<String> {
    if value.chars().any(|c| c.is_control()) {
        return Err(invalid(
            "native scheduler value contains a control character",
        ));
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('\"', "\\\"")
    ))
}
#[cfg(windows)]
fn windows_arguments(values: &[&str]) -> Result<String> {
    values
        .iter()
        .map(|value| windows_quote(value))
        .collect::<Result<Vec<_>>>()
        .map(|parts| parts.join(" "))
}

/// Quote one argv element using the CommandLineToArgvW backslash rule: runs of
/// backslashes are doubled only when they precede a quote or the closing quote.
#[cfg(windows)]
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
fn invalid(message: impl Into<String>) -> KioError {
    KioError::new(
        "KIO-E-WATCH-SERVICE-INVALID-001",
        message.into(),
        json!({}),
        ExitCode::InvalidUsage,
    )
}
fn service_error(error: impl std::fmt::Display) -> KioError {
    KioError::new(
        "KIO-E-WATCH-SERVICE-001",
        error.to_string(),
        json!({}),
        ExitCode::Failure,
    )
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;

    #[cfg(unix)]
    fn native_test_root() -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        (temp, root)
    }

    #[cfg(unix)]
    #[test]
    fn native_location_creates_private_suffix_below_trusted_shared_read_parent() {
        use std::os::unix::fs::PermissionsExt;
        let (_temp, root) = native_test_root();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let path = root.join("systemd/user/kio.service");
        let mut location = NativeDefinitionLocation::admit(&path).unwrap();
        assert_eq!(
            location.inspect(b"unit").unwrap(),
            NativeRegistration::Absent
        );
        assert!(
            !root.join("systemd").exists(),
            "inspection must not create suffixes"
        );
        location.publish(b"unit").unwrap();
        assert_eq!(
            location.inspect(b"unit").unwrap(),
            NativeRegistration::Owned
        );
        for path in [root.join("systemd"), root.join("systemd/user")] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o755
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            location.inspect(b"unit").unwrap(),
            NativeRegistration::Owned
        );
        location.directory.remove_file(&location.leaf).unwrap();
        assert_eq!(
            location.inspect(b"unit").unwrap(),
            NativeRegistration::Absent
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_location_rejects_writable_and_symlink_ancestors_before_mutation() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let (_temp, root) = native_test_root();
        let unsafe_parent = root.join("unsafe");
        fs::create_dir(&unsafe_parent).unwrap();
        for mode in [0o777, 0o775] {
            fs::set_permissions(&unsafe_parent, fs::Permissions::from_mode(mode)).unwrap();
            assert!(
                NativeDefinitionLocation::admit(&unsafe_parent.join("missing/user/unit")).is_err()
            );
            assert_eq!(fs::read_dir(&unsafe_parent).unwrap().count(), 0);
        }
        symlink(&root, root.join("alias")).unwrap();
        assert!(NativeDefinitionLocation::admit(&root.join("alias/missing/unit")).is_err());
        assert!(!root.join("missing").exists());
        assert!(!root.join(".kio-atomic").exists());
    }

    #[cfg(unix)]
    #[test]
    fn native_location_preserves_foreign_symlink_writable_and_hardlinked_leaves() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let (_temp, root) = native_test_root();
        let path = root.join("unit");
        fs::write(&path, b"foreign").unwrap();
        let mut location = NativeDefinitionLocation::admit(&path).unwrap();
        assert_eq!(
            location.inspect(b"expected").unwrap(),
            NativeRegistration::Foreign
        );
        assert!(location.publish(b"expected").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"foreign");
        assert!(!root.join(".kio-atomic").exists());
        fs::write(&path, b"expected").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(
            location.inspect(b"expected").unwrap(),
            NativeRegistration::Foreign
        );
        assert!(location.publish(b"expected").is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        fs::hard_link(&path, root.join("hardlink")).unwrap();
        assert_eq!(
            location.inspect(b"expected").unwrap(),
            NativeRegistration::Foreign
        );
        fs::remove_file(&path).unwrap();
        symlink(root.join("hardlink"), &path).unwrap();
        assert_eq!(
            location.inspect(b"expected").unwrap(),
            NativeRegistration::Foreign
        );
        assert!(location.publish(b"expected").is_err());
        assert_eq!(fs::read(root.join("hardlink")).unwrap(), b"expected");
        assert!(
            fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!root.join(".kio-atomic").exists());
    }

    #[cfg(unix)]
    #[test]
    fn native_location_revalidates_permissions_and_parent_identity() {
        use std::os::unix::fs::PermissionsExt;
        let (_temp, root) = native_test_root();
        let parent = root.join("native");
        fs::create_dir(&parent).unwrap();
        let mut location = NativeDefinitionLocation::admit(&parent.join("unit")).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(location.publish(b"unit").is_err());
        assert_eq!(fs::read_dir(&parent).unwrap().count(), 0);
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        fs::rename(&parent, root.join("old")).unwrap();
        fs::create_dir(&parent).unwrap();
        assert!(location.publish(b"unit").is_err());
        assert_eq!(fs::read_dir(&parent).unwrap().count(), 0);
        assert_eq!(fs::read_dir(root.join("old")).unwrap().count(), 0);
    }

    struct FakeScheduler {
        registration: RefCell<NativeRegistration>,
        install_fails: bool,
        start_fails: bool,
        stop_fails: bool,
        uninstall_fails: bool,
        installs: Cell<u8>,
        starts: Cell<u8>,
        stops: Cell<u8>,
        uninstalls: Cell<u8>,
    }

    impl Scheduler for FakeScheduler {
        fn inspect(&self, _: &ServiceManifest) -> Result<NativeRegistration> {
            Ok(*self.registration.borrow())
        }
        fn install(&self, _: &ServiceManifest) -> Result<()> {
            self.installs.set(self.installs.get() + 1);
            if self.install_fails {
                Err(service_error("synthetic scheduler failure"))
            } else {
                *self.registration.borrow_mut() = NativeRegistration::Owned;
                Ok(())
            }
        }
        fn start(&self, _: &ServiceManifest) -> Result<()> {
            self.starts.set(self.starts.get() + 1);
            if self.start_fails {
                Err(service_error("synthetic start failure"))
            } else {
                Ok(())
            }
        }
        fn stop(&self, _: &ServiceManifest) -> Result<()> {
            self.stops.set(self.stops.get() + 1);
            if self.stop_fails {
                Err(service_error("synthetic stop failure"))
            } else {
                Ok(())
            }
        }
        fn uninstall(&self, _: &ServiceManifest) -> Result<()> {
            self.uninstalls.set(self.uninstalls.get() + 1);
            if self.uninstall_fails {
                Err(service_error("synthetic uninstall failure"))
            } else {
                *self.registration.borrow_mut() = NativeRegistration::Absent;
                Ok(())
            }
        }
        fn state(&self, _: &ServiceManifest, _: NativeRegistration) -> Result<Value> {
            Ok(json!({}))
        }
    }

    fn manifest() -> ServiceManifest {
        ServiceManifest {
            version: VERSION,
            id: "io.kio.watch.0123456789abcdef0123456789abcdef".into(),
            scope_id: "scope".into(),
            root: PathBuf::from("/tmp/root"),
            binary: PathBuf::from("/tmp/kio"),
            reconcile_interval_seconds: 300,
            native_path: PathBuf::from("/tmp/watch.plist"),
            definition: "definition".into(),
            environment: ServiceEnvironment {
                home: PathBuf::from("/tmp"),
                userprofile: PathBuf::from("/tmp"),
                xdg_config_home: PathBuf::from("/tmp/config"),
                xdg_data_home: PathBuf::from("/tmp/data"),
                xdg_cache_home: PathBuf::from("/tmp/cache"),
            },
            phase: ServicePhase::Installing,
        }
    }
    #[test]
    fn renders_escaped_native_definition_without_shell_syntax() {
        let root = Path::new("/tmp/watch & root");
        let rendered = render_definition(
            "io.kio.watch.abc",
            Path::new("/tmp/kio"),
            root,
            300,
            &manifest().environment,
            Path::new("/tmp/data/kio/watch-services/io.kio.watch.abc.json"),
        )
        .unwrap();
        assert!(rendered.contains("watch &amp; root") || rendered.contains("watch & root"));
        assert!(!rendered.contains("$()"));
    }
    #[test]
    fn stable_identifier_binds_scope_and_root() {
        assert_ne!(
            stable_id("scope-a", Path::new("/tmp/a")),
            stable_id("scope-b", Path::new("/tmp/a"))
        );
        assert_ne!(
            stable_id("scope-a", Path::new("/tmp/a")),
            stable_id("scope-a", Path::new("/tmp/b"))
        );
    }
    #[test]
    fn rejects_unsafe_native_values() {
        assert!(xml_escape("line\nbreak").is_err());
    }

    #[test]
    fn runner_argv_carries_only_the_explicit_manifest_locator() {
        let locator = Path::new("/tmp/data/kio/watch-services/io.kio.watch.abc.json");
        assert_eq!(
            runner_argv(locator).unwrap(),
            vec![
                "watch",
                "service",
                "runner",
                "--manifest",
                "/tmp/data/kio/watch-services/io.kio.watch.abc.json",
            ]
        );
    }

    #[test]
    fn service_child_environment_is_complete_and_does_not_need_global_mutation() {
        assert_eq!(
            service_environment_pairs(&manifest().environment).unwrap(),
            vec![
                ("HOME", "/tmp".to_owned()),
                ("USERPROFILE", "/tmp".to_owned()),
                ("XDG_CONFIG_HOME", "/tmp/config".to_owned()),
                ("XDG_DATA_HOME", "/tmp/data".to_owned()),
                ("XDG_CACHE_HOME", "/tmp/cache".to_owned()),
            ]
        );
    }

    #[test]
    fn runner_rejects_foreign_manifest_locator() {
        let manifest = manifest();
        let foreign = Path::new("/tmp/other/watch-services/io.kio.watch.foreign.json");
        assert!(validate_runner_manifest_locator(foreign, &manifest).is_err());
    }

    #[test]
    fn manifest_parser_rejects_unexpected_fields() {
        let mut value = serde_json::to_value(manifest()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("foreign_runner_path".into(), json!("C:\\foreign\\kio.exe"));
        assert!(serde_json::from_value::<ServiceManifest>(value).is_err());
    }

    #[test]
    fn fake_scheduler_never_installs_over_owned_or_foreign_registration() {
        let manifest = manifest();
        for registration in [NativeRegistration::Owned, NativeRegistration::Foreign] {
            let fake = FakeScheduler {
                registration: RefCell::new(registration),
                install_fails: false,
                start_fails: false,
                stop_fails: false,
                uninstall_fails: false,
                installs: Cell::new(0),
                starts: Cell::new(0),
                stops: Cell::new(0),
                uninstalls: Cell::new(0),
            };
            assert!(install_checked(&fake, &manifest).is_err());
            assert_eq!(fake.installs.get(), 0);
        }
    }

    #[test]
    fn failed_fake_install_leaves_callers_installing_manifest_unchanged() {
        let manifest = manifest();
        let fake = FakeScheduler {
            registration: RefCell::new(NativeRegistration::Absent),
            install_fails: true,
            start_fails: false,
            stop_fails: false,
            uninstall_fails: false,
            installs: Cell::new(0),
            starts: Cell::new(0),
            stops: Cell::new(0),
            uninstalls: Cell::new(0),
        };
        assert!(install_checked(&fake, &manifest).is_err());
        assert_eq!(fake.installs.get(), 1);
        assert_eq!(manifest.phase, ServicePhase::Installing);
    }

    #[test]
    fn fake_scheduler_transitions_install_start_stop_start_uninstall() {
        let manifest = manifest();
        let fake = FakeScheduler {
            registration: RefCell::new(NativeRegistration::Absent),
            install_fails: false,
            start_fails: false,
            stop_fails: false,
            uninstall_fails: false,
            installs: Cell::new(0),
            starts: Cell::new(0),
            stops: Cell::new(0),
            uninstalls: Cell::new(0),
        };
        install_checked(&fake, &manifest).unwrap();
        for lifecycle in [
            Lifecycle::Start,
            Lifecycle::Stop,
            Lifecycle::Start,
            Lifecycle::Uninstall,
        ] {
            transition_checked(
                &fake,
                &manifest,
                lifecycle,
                fake.inspect(&manifest).unwrap(),
            )
            .unwrap();
        }
        assert_eq!(
            (
                fake.installs.get(),
                fake.starts.get(),
                fake.stops.get(),
                fake.uninstalls.get()
            ),
            (1, 2, 1, 1)
        );
        assert_eq!(fake.inspect(&manifest).unwrap(), NativeRegistration::Absent);
    }

    #[test]
    fn fake_scheduler_preserves_recovery_and_refuses_foreign_names() {
        let manifest = manifest();
        let fake = FakeScheduler {
            registration: RefCell::new(NativeRegistration::Owned),
            install_fails: false,
            start_fails: true,
            stop_fails: false,
            uninstall_fails: false,
            installs: Cell::new(0),
            starts: Cell::new(0),
            stops: Cell::new(0),
            uninstalls: Cell::new(0),
        };
        assert!(
            transition_checked(
                &fake,
                &manifest,
                Lifecycle::Start,
                NativeRegistration::Owned
            )
            .is_err()
        );
        assert_eq!(fake.inspect(&manifest).unwrap(), NativeRegistration::Owned);
        assert!(
            transition_checked(
                &fake,
                &manifest,
                Lifecycle::Uninstall,
                NativeRegistration::Foreign
            )
            .is_err()
        );
        assert_eq!(fake.uninstalls.get(), 0);
    }
}
