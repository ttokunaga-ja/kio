//! Per-user native scheduler registration for the offline foreground watcher.
//!
//! The native definition is an execution convenience only.  The watch instance
//! lock remains the liveness authority; scheduler state can only describe what
//! the OS has registered or been asked to start.

use crate::context::AppContext;
use crate::response::watch::{
    NativeServiceState, ServicePhase, WatchResponse, WatchServiceInstalledResponse,
    WatchServiceLifecycleResponse, WatchServiceStatusResponse,
};

use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use kio_core::control::ControlHandle;
use kio_core::management::{ManagementAuthority, read_record};
use kio_core::store_dir::{Publication, StoreDirectory};
use kio_core::{ExitCode, KioError, Result};
use kio_process::{BoundedProcessOptions, BoundedProcessOutput, run_bounded_command};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use serde_json::Value;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::commands::WatchServiceCommand;
use crate::management::{binding_for_repo, open_existing_managed_root};
use crate::platform::{
    native_after_remove, native_definition_path, native_enable, native_loaded_definition_matches,
    native_start, native_state, native_stop, native_uninstall, render_definition,
    run_watch_service_runner,
};

const MANIFEST_DIRECTORY: &str = "watch-services";
const MUTATION_LOCK_DIRECTORY: &str = "watch-services/locks";
pub(crate) const MAX_MANIFEST_BYTES: u64 = 32 * 1024;
const VERSION: u8 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServiceManifest {
    version: u8,
    pub(crate) id: String,
    pub(crate) scope_id: String,
    pub(crate) root: PathBuf,
    pub(crate) binary: PathBuf,
    pub(crate) reconcile_interval_seconds: u64,
    pub(crate) native_path: PathBuf,
    pub(crate) definition: String,
    pub(crate) environment: ServiceEnvironment,
    phase: ServicePhase,
}

/// The scheduler has no useful inherited shell environment.  Persist the
/// complete small environment the child needs so starting from login uses the
/// same device-private Kio directories as an interactive invocation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServiceEnvironment {
    pub(crate) home: PathBuf,
    pub(crate) userprofile: PathBuf,
    pub(crate) xdg_config_home: PathBuf,
    pub(crate) xdg_data_home: PathBuf,
    pub(crate) xdg_cache_home: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeRegistration {
    Absent,
    Owned,
    Foreign,
}

pub(crate) fn run(
    ctx: &AppContext,
    root_argument: Option<PathBuf>,
    command: WatchServiceCommand,
    control: &ControlHandle,
) -> Result<WatchResponse> {
    match command {
        WatchServiceCommand::Install {
            reconcile_interval_seconds,
        } => install(ctx, root_argument, reconcile_interval_seconds, control),
        WatchServiceCommand::Start => {
            mutate_existing(ctx, root_argument, Lifecycle::Start, control)
        }
        WatchServiceCommand::Stop => mutate_existing(ctx, root_argument, Lifecycle::Stop, control),
        WatchServiceCommand::Status => status(ctx, root_argument, control),
        WatchServiceCommand::Uninstall => {
            mutate_existing(ctx, root_argument, Lifecycle::Uninstall, control)
        }
        WatchServiceCommand::Runner { manifest } => {
            run_watch_service_runner(manifest, control).map(WatchResponse::ServiceRunner)
        }
    }
}

fn install(
    ctx: &AppContext,
    root_argument: Option<PathBuf>,
    interval: u64,
    control: &ControlHandle,
) -> Result<WatchResponse> {
    if !(1..=86_400).contains(&interval) {
        return Err(invalid(
            "reconciliation interval must be 1 through 86400 seconds",
        ));
    }
    let root = resolve_root(ctx, root_argument, true)?;
    let repo = open_existing_managed_root(&root, control)?;
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
    let _native_location = NativeDefinitionLocation::admit(&manifest.native_path, control)?;
    let store = manifest_store(true, control)?
        .ok_or_else(|| service_error("device manifest store unavailable"))?;
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
        return recover_install(&store, &leaf, existing, &NativeScheduler(control.clone()));
    }
    let scheduler = NativeScheduler(control.clone());
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

/// Complete an Installing intent after interruption, without replacing another
/// owner's service. A completed installation is not an interrupted operation.
fn recover_install<S: Scheduler>(
    store: &StoreDirectory,
    leaf: &Path,
    mut manifest: ServiceManifest,
    scheduler: &S,
) -> Result<WatchResponse> {
    if manifest.phase != ServicePhase::Installing {
        return Err(service_error("watch service is already installed"));
    }
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
    Ok(WatchResponse::ServiceInstalled(
        WatchServiceInstalledResponse {
            status: "installed".to_owned(),
            service_id: manifest.id,
            root: manifest.root,
            native_registration: "enabled_not_started".to_owned(),
        },
    ))
}

#[derive(Clone, Copy)]
enum Lifecycle {
    Start,
    Stop,
    Uninstall,
}

fn mutate_existing(
    ctx: &AppContext,
    root_argument: Option<PathBuf>,
    lifecycle: Lifecycle,
    control: &ControlHandle,
) -> Result<WatchResponse> {
    let (store, leaf, manifest) = find_manifest(ctx, root_argument, control)?;
    let _native_location = NativeDefinitionLocation::admit(&manifest.native_path, control)?;
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
    let scheduler = NativeScheduler(control.clone());
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
            transition_checked(&scheduler, &manifest, lifecycle, registration, |root| {
                crate::watch_command::inspect_liveness(root, control)
                    .map(|response| Some(response.status))
            })?;
            Ok(WatchResponse::ServiceLifecycle(
                WatchServiceLifecycleResponse {
                    status: "start_requested".to_owned(),
                    service_id: manifest.id,
                    root: manifest.root,
                },
            ))
        }
        Lifecycle::Stop => {
            // Disable scheduler restart before asking the foreground instance to
            // stop; otherwise a failed process may be restarted during shutdown.
            transition_checked(&scheduler, &manifest, lifecycle, registration, |root| {
                crate::watch_command::inspect_liveness(root, control)
                    .map(|response| Some(response.status))
            })?;
            Ok(WatchResponse::ServiceLifecycle(
                WatchServiceLifecycleResponse {
                    status: "stopped".to_owned(),
                    service_id: manifest.id,
                    root: manifest.root,
                },
            ))
        }
        Lifecycle::Uninstall => {
            transition_checked(&scheduler, &manifest, lifecycle, registration, |root| {
                crate::watch_command::inspect_liveness(root, control)
                    .map(|response| Some(response.status))
            })?;
            let bytes = serde_json::to_vec(&manifest).map_err(service_error)?;
            store.quarantine_then_remove(&leaf, &bytes, MAX_MANIFEST_BYTES)?;
            Ok(WatchResponse::ServiceLifecycle(
                WatchServiceLifecycleResponse {
                    status: "uninstalled".to_owned(),
                    service_id: manifest.id,
                    root: manifest.root,
                },
            ))
        }
    }
}

fn transition_checked<S: Scheduler>(
    scheduler: &S,
    manifest: &ServiceManifest,
    lifecycle: Lifecycle,
    registration: NativeRegistration,
    inspect_liveness: impl FnOnce(&Path) -> Result<Option<String>>,
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
        Lifecycle::Start => {
            // The caller retains the service mutation lock across this check
            // and the native command. Instance-lock liveness also detects a
            // watcher still starting, before it publishes its state or PID.
            let liveness = inspect_liveness(&manifest.root)?;
            match liveness.as_deref() {
                Some("stopped") => scheduler.start(manifest),
                Some("running") => Err(service_error("watcher is already running")),
                _ => Err(service_error("watch instance liveness is unavailable")),
            }
        }
        // A prior interrupted stop or uninstall can already have removed the
        // native registration. Both operations remain safe to retry.
        Lifecycle::Stop if registration == NativeRegistration::Absent => Ok(()),
        Lifecycle::Stop => scheduler.stop(manifest),
        Lifecycle::Uninstall if registration == NativeRegistration::Absent => Ok(()),
        Lifecycle::Uninstall => scheduler.uninstall(manifest),
    }
}

fn status(
    ctx: &AppContext,
    root_argument: Option<PathBuf>,
    control: &ControlHandle,
) -> Result<WatchResponse> {
    let (_, _, manifest) = find_manifest(ctx, root_argument, control)?;
    let scheduler = NativeScheduler(control.clone());
    let registration = scheduler.inspect(&manifest)?;
    if registration == NativeRegistration::Foreign {
        return Err(service_error(
            "native registration does not match its owned manifest",
        ));
    }
    let native_state = scheduler.state(&manifest, registration)?;
    // The device-private watcher lock is still authoritative even if a user
    // removed the root after installing the service.
    let liveness = crate::watch_command::inspect_liveness(&manifest.root, control)?;
    Ok(WatchResponse::ServiceStatus(WatchServiceStatusResponse {
        status: "service".to_owned(),
        service_id: manifest.id,
        root: manifest.root,
        install_phase: manifest.phase,
        native_registration: native_state,
        instance: liveness,
    }))
}

fn find_manifest(
    ctx: &AppContext,
    root_argument: Option<PathBuf>,
    control: &ControlHandle,
) -> Result<(StoreDirectory, PathBuf, ServiceManifest)> {
    let requested = resolve_root(ctx, root_argument, false)?;
    let store = manifest_store(false, control)?
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

fn resolve_root(
    ctx: &AppContext,
    root_argument: Option<PathBuf>,
    require_existing: bool,
) -> Result<PathBuf> {
    let cwd = ctx.cwd().to_path_buf();
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

fn manifest_store(create: bool, control: &ControlHandle) -> Result<Option<StoreDirectory>> {
    let registry = kio_index::registry::default_registry_path_with_control(control)
        .map_err(crate::object_persistence::index_to_kio)?;
    let Some(data) = registry.parent() else {
        return Err(service_error("device data directory is unavailable"));
    };
    if !data.exists() {
        if !create {
            return Ok(None);
        }
        fs::create_dir_all(data).map_err(service_error)?;
    }
    let store = StoreDirectory::open_with_control(data, control.clone())?;
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
pub(crate) fn manifest_locator(id: &str, environment: &ServiceEnvironment) -> Result<PathBuf> {
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

pub(crate) fn validate_manifest(manifest: &ServiceManifest) -> Result<()> {
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

pub(crate) fn service_environment_pairs(
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

pub(crate) fn validate_argument_path(path: &Path) -> Result<()> {
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

pub(crate) trait Scheduler {
    fn inspect(&self, manifest: &ServiceManifest) -> Result<NativeRegistration>;
    fn install(&self, manifest: &ServiceManifest) -> Result<()>;
    fn start(&self, manifest: &ServiceManifest) -> Result<()>;
    fn stop(&self, manifest: &ServiceManifest) -> Result<()>;
    fn uninstall(&self, manifest: &ServiceManifest) -> Result<()>;
    fn state(
        &self,
        manifest: &ServiceManifest,
        registration: NativeRegistration,
    ) -> Result<NativeServiceState>;
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
    fn admit(path: &Path, control: &ControlHandle) -> Result<Self> {
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
        let directory =
            kio_core::private_fs::verify_private_creation_parent_with_control(parent, control)?;
        missing.reverse();
        Ok(Self {
            directory,
            missing,
            leaf: PathBuf::from(leaf),
        })
    }

    fn revalidate(&self) -> Result<()> {
        let current = kio_core::private_fs::verify_private_creation_parent_with_control(
            self.directory.path(),
            self.directory.control(),
        )?;
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
            self.directory = StoreDirectory::from_retained_with_control(
                handle,
                self.directory.path().join(component),
                self.directory.control().clone(),
            )?;
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

pub(crate) struct NativeScheduler(pub(crate) ControlHandle);
impl Scheduler for NativeScheduler {
    fn inspect(&self, manifest: &ServiceManifest) -> Result<NativeRegistration> {
        let location = NativeDefinitionLocation::admit(&manifest.native_path, &self.0)?;
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
        let mut location = NativeDefinitionLocation::admit(&manifest.native_path, &self.0)?;
        location.publish(manifest.definition.as_bytes())?;
        location.require_owned(manifest)?;
        native_enable(manifest)?;
        location.require_owned(manifest)
    }
    fn start(&self, manifest: &ServiceManifest) -> Result<()> {
        let location = NativeDefinitionLocation::admit(&manifest.native_path, &self.0)?;
        location.require_owned(manifest)?;
        native_start(manifest)?;
        location.require_owned(manifest)
    }
    fn stop(&self, manifest: &ServiceManifest) -> Result<()> {
        let location = NativeDefinitionLocation::admit(&manifest.native_path, &self.0)?;
        location.require_owned(manifest)?;
        native_stop(manifest)?;
        location.require_owned(manifest)
    }
    fn uninstall(&self, manifest: &ServiceManifest) -> Result<()> {
        let location = NativeDefinitionLocation::admit(&manifest.native_path, &self.0)?;
        location.require_owned(manifest)?;
        native_uninstall(manifest)?;
        // Recheck the retained XML/plist/unit cache without querying a Windows
        // registration that native_uninstall has intentionally just removed.
        location.require_owned(manifest)?;
        location.directory.remove_file(&location.leaf)?;
        native_after_remove()?;
        location.revalidate()
    }
    fn state(
        &self,
        manifest: &ServiceManifest,
        registration: NativeRegistration,
    ) -> Result<NativeServiceState> {
        let location = NativeDefinitionLocation::admit(&manifest.native_path, &self.0)?;
        if location.inspect(manifest.definition.as_bytes())? != registration {
            return Err(service_error("native registration changed during status"));
        }
        match registration {
            NativeRegistration::Absent => Ok(NativeServiceState {
                registered: false,
                active: false,
            }),
            NativeRegistration::Foreign => Err(service_error("foreign native registration")),
            NativeRegistration::Owned => {
                let state = native_state(manifest)?;
                location.require_owned(manifest)?;
                Ok(state)
            }
        }
    }
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
            max_stdout_bytes: 16 * 1024,
            max_stderr_bytes: 16 * 1024,
        },
        None,
    )
    .map_err(service_error)
}
fn bounded_output(output: &BoundedProcessOutput) -> String {
    output.stderr.chars().take(512).collect()
}

pub(crate) fn invalid(message: impl Into<String>) -> KioError {
    KioError::new(
        "KIO-E-WATCH-SERVICE-INVALID-001",
        message.into(),
        json!({}),
        ExitCode::InvalidUsage,
    )
}
pub(crate) fn service_error(error: impl std::fmt::Display) -> KioError {
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
    use crate::platform::{runner_argv, validate_runner_manifest_locator, xml_escape};

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
        let mut location =
            NativeDefinitionLocation::admit(&path, &ControlHandle::default()).unwrap();
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
                NativeDefinitionLocation::admit(
                    &unsafe_parent.join("missing/user/unit"),
                    &ControlHandle::default()
                )
                .is_err()
            );
            assert_eq!(fs::read_dir(&unsafe_parent).unwrap().count(), 0);
        }
        symlink(&root, root.join("alias")).unwrap();
        assert!(
            NativeDefinitionLocation::admit(
                &root.join("alias/missing/unit"),
                &ControlHandle::default()
            )
            .is_err()
        );
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
        let mut location =
            NativeDefinitionLocation::admit(&path, &ControlHandle::default()).unwrap();
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
        let mut location =
            NativeDefinitionLocation::admit(&parent.join("unit"), &ControlHandle::default())
                .unwrap();
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

    impl FakeScheduler {
        fn owned() -> Self {
            Self {
                registration: RefCell::new(NativeRegistration::Owned),
                install_fails: false,
                start_fails: false,
                stop_fails: false,
                uninstall_fails: false,
                installs: Cell::new(0),
                starts: Cell::new(0),
                stops: Cell::new(0),
                uninstalls: Cell::new(0),
            }
        }
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
        fn state(
            &self,
            _: &ServiceManifest,
            registration: NativeRegistration,
        ) -> Result<NativeServiceState> {
            Ok(NativeServiceState {
                registered: registration == NativeRegistration::Owned,
                active: false,
            })
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
        let binary = root.join("kio");
        let rendered = render_definition(
            "io.kio.watch.abc",
            &binary,
            root,
            300,
            &manifest().environment,
            Path::new("/tmp/data/kio/watch-services/io.kio.watch.abc.json"),
        )
        .unwrap();
        assert!(rendered.contains("watch &amp; root") || rendered.contains("watch & root"));
        #[cfg(windows)]
        assert!(rendered.contains(&format!(
            "<Command>{}</Command>",
            xml_escape(&binary.to_string_lossy()).unwrap()
        )));
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
                |_| {
                    Ok(
                        json!({"status":"stopped", "last_observation":{"pid":std::process::id()}})
                            .get("status")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    )
                },
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
    fn completed_duplicate_install_is_rejected_without_scheduler_or_manifest_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let store = StoreDirectory::open(&temp.path().canonicalize().unwrap()).unwrap();
        let leaf = Path::new("service.json");
        let mut manifest = manifest();
        manifest.phase = ServicePhase::Installed;
        write_manifest(&store, leaf, &manifest, Publication::CreateOnly).unwrap();
        let before = store
            .read_optional(leaf, MAX_MANIFEST_BYTES)
            .unwrap()
            .unwrap();
        for registration in [NativeRegistration::Owned, NativeRegistration::Absent] {
            let fake = FakeScheduler::owned();
            *fake.registration.borrow_mut() = registration;
            let error = recover_install(&store, leaf, manifest.clone(), &fake).unwrap_err();
            assert!(error.to_string().contains("already installed"));
            assert_eq!(fake.installs.get(), 0);
            assert_eq!(fake.starts.get(), 0);
            assert_eq!(fake.stops.get(), 0);
            assert_eq!(fake.uninstalls.get(), 0);
            assert_eq!(
                store
                    .read_optional(leaf, MAX_MANIFEST_BYTES)
                    .unwrap()
                    .unwrap(),
                before
            );
        }
    }

    #[test]
    fn interrupted_install_resumes_owned_or_absent_registration_and_refuses_foreign() {
        for registration in [
            NativeRegistration::Owned,
            NativeRegistration::Absent,
            NativeRegistration::Foreign,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let store = StoreDirectory::open(&temp.path().canonicalize().unwrap()).unwrap();
            let leaf = Path::new("service.json");
            let manifest = manifest();
            write_manifest(&store, leaf, &manifest, Publication::CreateOnly).unwrap();
            let before = store
                .read_optional(leaf, MAX_MANIFEST_BYTES)
                .unwrap()
                .unwrap();
            let fake = FakeScheduler::owned();
            *fake.registration.borrow_mut() = registration;
            let result = recover_install(&store, leaf, manifest, &fake);
            let bytes = store
                .read_optional(leaf, MAX_MANIFEST_BYTES)
                .unwrap()
                .unwrap();
            let stored: ServiceManifest = serde_json::from_slice(&bytes).unwrap();
            match registration {
                NativeRegistration::Owned | NativeRegistration::Absent => {
                    assert_eq!(
                        serde_json::to_value(result.unwrap()).unwrap()["status"],
                        "installed"
                    );
                    assert_eq!(stored.phase, ServicePhase::Installed);
                    assert_eq!(
                        fake.installs.get(),
                        u8::from(registration == NativeRegistration::Absent)
                    );
                    assert_eq!(fake.inspect(&stored).unwrap(), NativeRegistration::Owned);
                }
                NativeRegistration::Foreign => {
                    assert!(result.unwrap_err().to_string().contains("foreign"));
                    assert_eq!(stored.phase, ServicePhase::Installing);
                    assert_eq!(bytes, before);
                    assert_eq!(fake.installs.get(), 0);
                }
            }
            assert_eq!(fake.starts.get(), 0);
        }
    }

    #[test]
    fn running_duplicate_start_is_rejected_without_scheduler_restart() {
        let manifest = manifest();
        let fake = FakeScheduler::owned();
        let error = transition_checked(
            &fake,
            &manifest,
            Lifecycle::Start,
            NativeRegistration::Owned,
            |root| {
                assert_eq!(root, manifest.root);
                // The authoritative held lock is running even before state/PID publication.
                Ok(json!({"status":"running", "last_observation":null})
                    .get("status")
                    .and_then(Value::as_str)
                    .map(str::to_owned))
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("already running"));
        assert_eq!(fake.starts.get(), 0);
    }

    #[test]
    fn start_refuses_liveness_inspection_error_without_scheduler_mutation() {
        let manifest = manifest();
        let fake = FakeScheduler::owned();
        let error = transition_checked(
            &fake,
            &manifest,
            Lifecycle::Start,
            NativeRegistration::Owned,
            |_| Err(service_error("synthetic instance lock inspection failure")),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("instance lock inspection failure")
        );
        assert_eq!(fake.starts.get(), 0);
    }

    #[test]
    fn start_refuses_unknown_instance_liveness_without_scheduler_mutation() {
        let manifest = manifest();
        for liveness in [
            json!({}),
            json!({"status":"unknown"}),
            json!({"status":null}),
        ] {
            let fake = FakeScheduler::owned();
            let error = transition_checked(
                &fake,
                &manifest,
                Lifecycle::Start,
                NativeRegistration::Owned,
                |_| {
                    Ok(liveness
                        .get("status")
                        .and_then(Value::as_str)
                        .map(str::to_owned))
                },
            )
            .unwrap_err();
            assert!(error.to_string().contains("instance liveness"));
            assert_eq!(fake.starts.get(), 0);
        }
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
                NativeRegistration::Owned,
                |_| Ok(json!({"status":"stopped"})
                    .get("status")
                    .and_then(Value::as_str)
                    .map(str::to_owned)),
            )
            .is_err()
        );
        assert_eq!(fake.inspect(&manifest).unwrap(), NativeRegistration::Owned);
        assert!(
            transition_checked(
                &fake,
                &manifest,
                Lifecycle::Uninstall,
                NativeRegistration::Foreign,
                |_| panic!("foreign registrations must not inspect liveness"),
            )
            .is_err()
        );
        assert_eq!(fake.uninstalls.get(), 0);
    }
}
