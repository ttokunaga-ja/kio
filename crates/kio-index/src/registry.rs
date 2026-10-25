//! Device-local scope registry (`~/.local/share/kio/scope-registry.sqlite`).
//!
//! The registry is a search cache, never truth (03-data-model.md §4): it lists
//! candidate scopes for multi-scope search (05-runtime.md §1.8) and resolves
//! `scope_id -> kio_path` for Evidence Pointers (08-evidence-pointer-spec.md
//! §3.1). Losing it must be recoverable by re-running `kio init` / `kio index`
//! in each scope.

use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use cap_primitives::fs as cap_fs;
#[cfg(test)]
use rusqlite::Connection;
use rusqlite::{OpenFlags, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::platform::{
    FileIdentity, PrivateSnapshotStorage, RegistryParentBinding, fresh_registry_parent_binding,
    leaf_identity, observe_leaf, prepare_registry_writer, stable_registry_parent_binding,
    validate_regular,
};
use crate::{IndexError, Result};
#[cfg(test)]
use std::path::Component;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryEntry {
    pub scope_id: String,
    /// Absolute path of the `.kio` directory (truth root).
    pub kio_path: String,
    /// Absolute path of the folder that contains `.kio`.
    pub root_path: String,
    pub participates_in_global_search: bool,
    /// True once the scope has a search index (`kio index` completed).
    pub indexed: bool,
    pub last_seen_at: String,
}

/// One app-prevalidated move of a scope's device-local registration.
///
/// `old_kio_paths` is the exact path set observed for `scope_id` before the
/// app checked those former locations. It may include `new_registration`'s
/// path when a prior interrupted rebind already inserted the destination row.
/// This cache helper does not inspect the filesystem or claim that a supplied
/// old path is absent; that authority remains with the app's preflight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeRegistrationRebind {
    pub scope_id: String,
    pub old_kio_paths: Vec<String>,
    pub new_registration: RegistryEntry,
}

pub struct RegistryDb {
    conn: crate::platform::BoundSqliteConnection,
    // An inherited `/dev/fd/N` registry is opened through this duplicated
    // parent descriptor's procfs path. Keep it alive until SQLite closes so a
    // caller retargeting the original descriptor cannot redirect the database.
    // Windows writers likewise retain their verified or newly private parent
    // through SQLite's lifetime, including its path-based open.
    _inherited_parent: Option<kio_core::store_dir::StoreDirectory>,
    // Declared after `conn`, so the database closes before its private snapshot
    // directory is removed.
    _snapshot: Option<RegistrySnapshot>,
}

/// The size limits are deliberately registry-specific.  Snapshot reads and
/// copies are streaming, so these bounds limit I/O rather than allocating a
/// whole SQLite leaf in memory.
const PRIVATE_REGISTRY_MAIN: &str = "snapshot.sqlite";

pub(crate) const MAX_REGISTRY_LEAF_BYTES: u64 = 64 * 1024 * 1024;
const MAX_REGISTRY_TOTAL_BYTES: u64 = 128 * 1024 * 1024;
const REGISTRY_SNAPSHOT_RETRY_WINDOW: Duration = Duration::from_secs(5);
const MAX_REGISTRY_SNAPSHOT_ATTEMPTS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrySnapshotError {
    /// The registry main leaf and both sidecars are absent: a normal cache miss.
    Missing,
    /// An unsafe file shape, link, replacement, or integrity failure was found.
    UnsafeIntegrity(String),
    /// A concurrent change, busy source, or transient read prevented a stable
    /// owned snapshot before the bounded retry deadline.
    UnstableBusy(String),
}

impl std::fmt::Display for RegistrySnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str("scope registry is absent"),
            Self::UnsafeIntegrity(message) | Self::UnstableBusy(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for RegistrySnapshotError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LeafObservation {
    Absent,
    Present(FileObservation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileObservation {
    pub(crate) identity: FileIdentity,
    pub(crate) bytes: u64,
    pub(crate) sha256: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RegistryManifest {
    main: LeafObservation,
    wal: LeafObservation,
    shm: LeafObservation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PrivateSnapshotManifest {
    main: FileObservation,
    wal: Option<FileObservation>,
}

struct RegistrySnapshot {
    // This is the descriptor-relative capability used to validate the copied
    // leaves immediately before SQLite is allowed to open the private path.
    // Keep it alive for the lifetime of the connection/snapshot.
    _private_dir: fs::File,
    _storage: PrivateSnapshotStorage,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapshotTestPhase {
    BeforeSourceLeafOpen,
    BeforeSourceLeafRestat,
    AfterInitialManifest,
    BeforeSnapshotSqliteOpen,
    AfterSnapshotQueryBeforeRecheck,
    AfterFinalManifestBeforeParentRecheck,
}

#[cfg(test)]
pub(crate) type SnapshotTestHook =
    std::sync::Arc<dyn Fn(SnapshotTestPhase, usize, Option<&Path>) + Send + Sync>;

/// `$XDG_DATA_HOME/kio/scope-registry.sqlite`, falling back to
/// `$HOME/.local/share/kio/scope-registry.sqlite` (03-data-model.md §4).
pub fn default_registry_path() -> Result<PathBuf> {
    default_registry_path_with_control(&kio_core::control::ControlHandle::default())
}

pub fn default_registry_path_with_control(
    control: &kio_core::control::ControlHandle,
) -> Result<PathBuf> {
    // R12-6 / R13-6: honor the XDG validity rules AND require an absolute `HOME`
    // for the fallback (empty/relative treated as unset), so neither a bad
    // `XDG_DATA_HOME` nor a bad `HOME` lands the registry in a CWD-relative `kio/`.
    let data_home = kio_core::xdg::xdg_dir("XDG_DATA_HOME")
        .or_else(|| control.home_dir().map(|home| home.join(".local/share")))
        .ok_or_else(|| {
            crate::IndexError::Schema(
                "cannot resolve an absolute user data directory; refusing a CWD-relative registry"
                    .to_owned(),
            )
        })?;
    Ok(data_home.join("kio/scope-registry.sqlite"))
}

pub(crate) fn index_error_from_snapshot(error: RegistrySnapshotError) -> crate::IndexError {
    match error {
        RegistrySnapshotError::Missing => crate::IndexError::RegistryMissing,
        RegistrySnapshotError::UnsafeIntegrity(message) => {
            crate::IndexError::RegistryUnsafeIntegrity(message)
        }
        RegistrySnapshotError::UnstableBusy(message) => {
            crate::IndexError::RegistryUnstableBusy(message)
        }
    }
}

fn snapshot_query_error(error: crate::IndexError) -> RegistrySnapshotError {
    match error {
        crate::IndexError::Sqlite(rusqlite::Error::SqliteFailure(code, _))
            if matches!(
                code.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            RegistrySnapshotError::UnstableBusy(format!("query registry snapshot: {code}"))
        }
        crate::IndexError::Sqlite(rusqlite::Error::SqliteFailure(code, _))
            if matches!(
                code.code,
                rusqlite::ErrorCode::DatabaseCorrupt
                    | rusqlite::ErrorCode::NotADatabase
                    | rusqlite::ErrorCode::SchemaChanged
                    // The fixed lookup SQL is product-owned; SQLITE_ERROR here
                    // (for example a missing required scopes column) proves a
                    // malformed stable snapshot schema rather than transient I/O.
                    | rusqlite::ErrorCode::Unknown
            ) =>
        {
            RegistrySnapshotError::UnsafeIntegrity(format!(
                "stable registry snapshot query failed: {code}"
            ))
        }
        error @ crate::IndexError::Sqlite(
            rusqlite::Error::InvalidColumnName(_)
            | rusqlite::Error::InvalidColumnType(_, _, _)
            | rusqlite::Error::FromSqlConversionFailure(_, _, _)
            | rusqlite::Error::SqlInputError { .. },
        ) => RegistrySnapshotError::UnsafeIntegrity(format!(
            "stable registry snapshot returned malformed row/schema: {error}"
        )),
        error => RegistrySnapshotError::UnstableBusy(format!(
            "stable registry snapshot query failed: {error}"
        )),
    }
}

#[derive(Debug)]
pub(crate) enum SnapshotAttemptError {
    Missing,
    Unsafe(String),
    Unstable(String),
}

pub(crate) fn registry_parent_binding_error(error: RegistrySnapshotError) -> SnapshotAttemptError {
    match error {
        RegistrySnapshotError::Missing => SnapshotAttemptError::Missing,
        RegistrySnapshotError::UnsafeIntegrity(message) => SnapshotAttemptError::Unsafe(message),
        RegistrySnapshotError::UnstableBusy(message) => SnapshotAttemptError::Unstable(message),
    }
}

/// A missing canonical parent is a cache miss only after two complete nofollow
/// walks agree. Each snapshot retry obtains a new binding rather than retaining
/// a directory that may have been detached from the canonical path.
pub(crate) fn validate_directory(
    metadata: &cap_fs::Metadata,
    label: &str,
) -> std::result::Result<(), RegistrySnapshotError> {
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(RegistrySnapshotError::UnsafeIntegrity(format!(
            "registry parent component must be a real directory: {label}"
        )));
    }
    Ok(())
}

#[cfg(test)]
type ParentComponentHook = std::sync::Arc<dyn Fn(usize)>;

#[cfg(test)]
#[derive(Default)]
pub(crate) struct RegistryTestHooks {
    snapshot: Option<SnapshotTestHook>,
    parent_component: Option<ParentComponentHook>,
}

#[cfg(test)]
pub(crate) fn run_parent_component_hook(hooks: Option<&RegistryTestHooks>, index: usize) {
    if let Some(hook) = hooks.and_then(|hooks| hooks.parent_component.as_ref()) {
        hook(index);
    }
}

fn observe_manifest(
    parent: &fs::File,
    parent_path: &Path,
    main: &str,
    storage: Option<&PrivateSnapshotStorage>,
    missing_is_cache_miss: bool,
    #[cfg(test)] hook: Option<&SnapshotTestHook>,
    #[cfg(test)] attempt: usize,
) -> std::result::Result<RegistryManifest, SnapshotAttemptError> {
    let wal = format!("{main}-wal");
    let shm = format!("{main}-shm");
    let main_observation = observe_leaf(
        parent,
        parent_path,
        main,
        storage.map(|storage| (storage, PRIVATE_REGISTRY_MAIN)),
        #[cfg(test)]
        hook,
        #[cfg(test)]
        attempt,
    )?;
    let wal_observation = observe_leaf(
        parent,
        parent_path,
        &wal,
        storage.map(|storage| (storage, "snapshot.sqlite-wal")),
        #[cfg(test)]
        hook,
        #[cfg(test)]
        attempt,
    )?;
    let shm_observation = observe_leaf(
        parent,
        parent_path,
        &shm,
        None,
        #[cfg(test)]
        hook,
        #[cfg(test)]
        attempt,
    )?;
    let manifest = RegistryManifest {
        main: main_observation,
        wal: wal_observation,
        shm: shm_observation,
    };
    let total = [&manifest.main, &manifest.wal, &manifest.shm]
        .iter()
        .filter_map(|leaf| match leaf {
            LeafObservation::Present(observation) => Some(observation.bytes),
            LeafObservation::Absent => None,
        })
        .try_fold(0u64, |total, bytes| total.checked_add(bytes))
        .ok_or_else(|| {
            SnapshotAttemptError::Unsafe("scope registry aggregate size overflow".to_owned())
        })?;
    if total > MAX_REGISTRY_TOTAL_BYTES {
        return Err(SnapshotAttemptError::Unsafe(format!(
            "scope registry exceeds {} byte aggregate limit",
            MAX_REGISTRY_TOTAL_BYTES
        )));
    }
    if missing_is_cache_miss {
        match (&manifest.main, &manifest.wal, &manifest.shm) {
            (LeafObservation::Absent, LeafObservation::Absent, LeafObservation::Absent) => {
                return Err(SnapshotAttemptError::Missing);
            }
            (LeafObservation::Absent, _, _) => {
                return Err(SnapshotAttemptError::Unsafe(
                    "scope registry sidecar exists without main database".to_owned(),
                ));
            }
            _ => {}
        }
    }
    // Final observation intentionally records every presence shape.  A source
    // main disappearing after a valid initial capture is concurrent drift
    // (retryable), not a cache miss or an integrity fallback.
    Ok(manifest)
}

fn sqlite_snapshot_error(
    error: rusqlite::Error,
    operation: &str,
    fixed_schema_probe: bool,
) -> SnapshotAttemptError {
    match error {
        rusqlite::Error::SqliteFailure(code, _)
            if matches!(
                code.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            SnapshotAttemptError::Unstable(format!("{operation}: {code}"))
        }
        rusqlite::Error::SqliteFailure(code, _)
            if matches!(
                code.code,
                rusqlite::ErrorCode::DatabaseCorrupt
                    | rusqlite::ErrorCode::NotADatabase
                    | rusqlite::ErrorCode::SchemaChanged
            ) || (fixed_schema_probe && code.code == rusqlite::ErrorCode::Unknown) =>
        {
            SnapshotAttemptError::Unsafe(format!("{operation}: {code}"))
        }
        // Private snapshot setup/open/query errors such as CANTOPEN and IOERR
        // do not prove source or owned-copy corruption.  Retry inside the
        // bounded snapshot window and fail as UnstableBusy if they persist.
        error => SnapshotAttemptError::Unstable(format!("{operation}: {error}")),
    }
}

fn classify_snapshot_sqlite_failure(
    parent: &fs::File,
    parent_path: &Path,
    main: &str,
    initial: &RegistryManifest,
    error: SnapshotAttemptError,
) -> SnapshotAttemptError {
    // A copied main/WAL pair can be temporarily incoherent while a writer is
    // rotating/checkpointing.  Re-observe all source leaves before calling a
    // stable copy malformed; source drift is retryable rather than corruption.
    match observe_manifest(
        parent,
        parent_path,
        main,
        None,
        false,
        #[cfg(test)]
        None,
        #[cfg(test)]
        0,
    ) {
        Ok(current) if &current != initial => SnapshotAttemptError::Unstable(
            "scope registry changed while preparing SQLite snapshot".to_owned(),
        ),
        Ok(_) => error,
        Err(SnapshotAttemptError::Unsafe(message)) => SnapshotAttemptError::Unsafe(message),
        Err(SnapshotAttemptError::Missing) | Err(SnapshotAttemptError::Unstable(_)) => {
            SnapshotAttemptError::Unstable(
                "scope registry could not be rechecked after snapshot SQLite failure".to_owned(),
            )
        }
    }
}

/// Verify the private copy through the already-held directory capability
/// before SQLite sees its path.  The source manifest is intentionally used
/// only for bytes/digest expectations: a copied file has a different identity
/// from its source.  This rejects pre-created sidecars, replacement leaves,
/// links, and every name SQLite could otherwise interpret before the first
/// private SQLite open.
fn verify_private_snapshot_leaf(
    private_dir: &fs::File,
    name: &str,
    expected: &FileObservation,
) -> std::result::Result<FileObservation, SnapshotAttemptError> {
    let before = cap_fs::stat(private_dir, Path::new(name), cap_fs::FollowSymlinks::No).map_err(
        |error| {
            SnapshotAttemptError::Unsafe(format!(
                "private registry snapshot {name} is missing or unsafe: {error}"
            ))
        },
    )?;
    validate_regular(&before, name)?;
    if before.len() != expected.bytes {
        return Err(SnapshotAttemptError::Unsafe(format!(
            "private registry snapshot {name} has unexpected size"
        )));
    }
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut file = cap_fs::open(private_dir, Path::new(name), &options).map_err(|error| {
        SnapshotAttemptError::Unsafe(format!("open private registry snapshot {name}: {error}"))
    })?;
    let opened = cap_fs::Metadata::from_file(&file).map_err(|error| {
        SnapshotAttemptError::Unstable(format!("inspect private registry snapshot {name}: {error}"))
    })?;
    validate_regular(&opened, name)?;
    let before_identity = leaf_identity(&before).ok_or_else(|| {
        SnapshotAttemptError::Unsafe(format!(
            "private registry snapshot {name} has no usable identity"
        ))
    })?;
    let opened_identity = leaf_identity(&opened).ok_or_else(|| {
        SnapshotAttemptError::Unsafe(format!(
            "opened private registry snapshot {name} has no usable identity"
        ))
    })?;
    if before_identity != opened_identity || before.len() != opened.len() {
        return Err(SnapshotAttemptError::Unsafe(format!(
            "private registry snapshot {name} changed while opening"
        )));
    }
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buf).map_err(|error| {
            SnapshotAttemptError::Unstable(format!(
                "read private registry snapshot {name}: {error}"
            ))
        })?;
        if read == 0 {
            break;
        }
        bytes = bytes.checked_add(read as u64).ok_or_else(|| {
            SnapshotAttemptError::Unsafe(format!("private registry snapshot {name} size overflow"))
        })?;
        if bytes > MAX_REGISTRY_LEAF_BYTES {
            return Err(SnapshotAttemptError::Unsafe(format!(
                "private registry snapshot {name} exceeds byte limit"
            )));
        }
        hash.update(&buf[..read]);
    }
    let after = cap_fs::stat(private_dir, Path::new(name), cap_fs::FollowSymlinks::No).map_err(
        |error| {
            SnapshotAttemptError::Unsafe(format!(
                "reinspect private registry snapshot {name}: {error}"
            ))
        },
    )?;
    validate_regular(&after, name)?;
    let actual_sha256: [u8; 32] = hash.finalize().into();
    let after_identity = leaf_identity(&after).ok_or_else(|| {
        SnapshotAttemptError::Unsafe(format!(
            "reinspected private registry snapshot {name} has no usable identity"
        ))
    })?;
    if opened_identity != after_identity
        || opened.len() != after.len()
        || bytes != expected.bytes
        || actual_sha256 != expected.sha256
    {
        return Err(SnapshotAttemptError::Unsafe(format!(
            "private registry snapshot {name} changed or differs from its source copy"
        )));
    }
    Ok(FileObservation {
        identity: opened_identity,
        bytes,
        sha256: actual_sha256,
    })
}

fn verify_private_snapshot_before_sqlite(
    private_dir: &fs::File,
    initial: &RegistryManifest,
) -> std::result::Result<PrivateSnapshotManifest, SnapshotAttemptError> {
    let main = PRIVATE_REGISTRY_MAIN;
    let wal = format!("{PRIVATE_REGISTRY_MAIN}-wal");
    let shm = format!("{PRIVATE_REGISTRY_MAIN}-shm");
    let mut allowed = std::collections::BTreeSet::new();
    let LeafObservation::Present(main_expected) = &initial.main else {
        return Err(SnapshotAttemptError::Unsafe(
            "private registry snapshot has no main source observation".to_owned(),
        ));
    };
    allowed.insert(main.to_owned());
    let private_main = verify_private_snapshot_leaf(private_dir, main, main_expected)?;
    let private_wal = if let LeafObservation::Present(wal_expected) = &initial.wal {
        allowed.insert(wal.clone());
        Some(verify_private_snapshot_leaf(
            private_dir,
            &wal,
            wal_expected,
        )?)
    } else {
        None
    };
    if !matches!(
        cap_fs::stat(private_dir, Path::new(&shm), cap_fs::FollowSymlinks::No),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    ) {
        return Err(SnapshotAttemptError::Unsafe(format!(
            "private registry snapshot has unexpected pre-open sidecar {shm}"
        )));
    }
    let entries = cap_fs::read_base_dir(private_dir).map_err(|error| {
        SnapshotAttemptError::Unstable(format!("enumerate private registry snapshot: {error}"))
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            SnapshotAttemptError::Unstable(format!(
                "enumerate private registry snapshot entry: {error}"
            ))
        })?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            return Err(SnapshotAttemptError::Unsafe(
                "private registry snapshot has a non-UTF-8 leaf".to_owned(),
            ));
        };
        if !allowed.contains(&name) {
            return Err(SnapshotAttemptError::Unsafe(format!(
                "private registry snapshot has unexpected pre-open leaf {name}"
            )));
        }
    }
    Ok(PrivateSnapshotManifest {
        main: private_main,
        wal: private_wal,
    })
}

fn snapshot_attempt(
    binding: &RegistryParentBinding,
    control: &kio_core::control::ControlHandle,
    #[cfg(test)] hooks: Option<&RegistryTestHooks>,
    #[cfg(not(test))] _hooks: Option<&()>,
    #[cfg(test)] attempt: usize,
    #[cfg(not(test))] _attempt: usize,
) -> std::result::Result<
    (crate::platform::BoundSqliteConnection, RegistrySnapshot),
    SnapshotAttemptError,
> {
    #[cfg(test)]
    let hook = hooks.and_then(|hooks| hooks.snapshot.as_ref());
    let parent = binding.parent.as_ref();
    let parent_path = binding.parent_path.as_path();
    let main = binding.leaf.as_str();
    let storage = PrivateSnapshotStorage::create()?;
    let initial = match observe_manifest(
        parent,
        parent_path,
        main,
        Some(&storage),
        true,
        #[cfg(test)]
        hook,
        #[cfg(test)]
        attempt,
    ) {
        Ok(initial) => initial,
        Err(SnapshotAttemptError::Missing) => {
            #[cfg(test)]
            if let Some(hook) = hook {
                hook(SnapshotTestPhase::AfterInitialManifest, attempt, None);
            }
            let fresh_parent = fresh_registry_parent_binding(
                binding,
                control,
                #[cfg(test)]
                hooks,
            )?;
            return match observe_manifest(
                fresh_parent.as_ref(),
                parent_path,
                main,
                None,
                true,
                #[cfg(test)]
                hook,
                #[cfg(test)]
                attempt,
            ) {
                Err(SnapshotAttemptError::Missing) => {
                    #[cfg(test)]
                    if let Some(hook) = hook {
                        hook(
                            SnapshotTestPhase::AfterFinalManifestBeforeParentRecheck,
                            attempt,
                            None,
                        );
                    }
                    fresh_registry_parent_binding(
                        binding,
                        control,
                        #[cfg(test)]
                        hooks,
                    )?;
                    Err(SnapshotAttemptError::Missing)
                }
                Ok(_) => Err(SnapshotAttemptError::Unstable(
                    "scope registry appeared while confirming an all-absent snapshot".to_owned(),
                )),
                Err(error) => Err(error),
            };
        }
        Err(error) => return Err(error),
    };
    let private_dir = storage.capability()?;
    let private_initial = verify_private_snapshot_before_sqlite(&private_dir, &initial)?;
    #[cfg(test)]
    if let Some(hook) = hook {
        hook(SnapshotTestPhase::AfterInitialManifest, attempt, None);
        hook(
            SnapshotTestPhase::BeforeSnapshotSqliteOpen,
            attempt,
            Some(storage.path()),
        );
    }
    let private_current = verify_private_snapshot_before_sqlite(&private_dir, &initial)?;
    if private_initial != private_current {
        return Err(SnapshotAttemptError::Unsafe(
            "private registry snapshot changed before SQLite open".to_owned(),
        ));
    }
    let snapshot_path = storage.path().join(PRIVATE_REGISTRY_MAIN);
    let private_parent = kio_core::store_dir::StoreDirectory::from_retained(
        private_dir
            .try_clone()
            .map_err(|error| SnapshotAttemptError::Unstable(error.to_string()))?,
        storage.path().to_path_buf(),
    )
    .map_err(|error| SnapshotAttemptError::Unsafe(error.to_string()))?;
    let conn = match crate::platform::open_private_registry_snapshot(
        &snapshot_path,
        &private_parent,
        &storage,
    ) {
        Ok(conn) => conn,
        Err(error) => {
            return Err(classify_snapshot_sqlite_failure(
                parent,
                parent_path,
                main,
                &initial,
                match error {
                    IndexError::Sqlite(error) => {
                        sqlite_snapshot_error(error, "open registry snapshot", false)
                    }
                    error => SnapshotAttemptError::Unsafe(error.to_string()),
                },
            ));
        }
    };
    if let Err(error) = conn.busy_timeout(Duration::from_millis(5000)) {
        return Err(classify_snapshot_sqlite_failure(
            parent,
            parent_path,
            main,
            &initial,
            sqlite_snapshot_error(error, "configure registry snapshot", false),
        ));
    }
    if let Err(error) = conn.pragma_update(None, "query_only", "ON") {
        return Err(classify_snapshot_sqlite_failure(
            parent,
            parent_path,
            main,
            &initial,
            sqlite_snapshot_error(error, "make registry snapshot query-only", false),
        ));
    }
    // Execute a real query before the final source recheck.  This exercises
    // committed WAL visibility without ever passing the source path to SQLite.
    if let Err(error) = conn
        .query_row("SELECT 1 FROM scopes LIMIT 1", [], |row| {
            row.get::<_, i64>(0)
        })
        .optional()
    {
        return Err(classify_snapshot_sqlite_failure(
            parent,
            parent_path,
            main,
            &initial,
            sqlite_snapshot_error(error, "query registry snapshot", true),
        ));
    }
    #[cfg(test)]
    if let Some(hook) = hook {
        hook(
            SnapshotTestPhase::AfterSnapshotQueryBeforeRecheck,
            attempt,
            None,
        );
    }
    let fresh_parent = fresh_registry_parent_binding(
        binding,
        control,
        #[cfg(test)]
        hooks,
    )?;
    let final_manifest = observe_manifest(
        fresh_parent.as_ref(),
        parent_path,
        main,
        None,
        false,
        #[cfg(test)]
        hook,
        #[cfg(test)]
        attempt,
    )?;
    if initial != final_manifest {
        return Err(SnapshotAttemptError::Unstable(
            "scope registry main/WAL/SHM changed while snapshotting".to_owned(),
        ));
    }
    #[cfg(test)]
    if let Some(hook) = hook {
        hook(
            SnapshotTestPhase::AfterFinalManifestBeforeParentRecheck,
            attempt,
            None,
        );
    }
    fresh_registry_parent_binding(
        binding,
        control,
        #[cfg(test)]
        hooks,
    )?;
    Ok((
        conn,
        RegistrySnapshot {
            _private_dir: private_dir,
            _storage: storage,
        },
    ))
}

pub(crate) fn prepare_private_registry_writer(
    path: &Path,
) -> Result<(PathBuf, Option<kio_core::store_dir::StoreDirectory>)> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| IndexError::Schema(error.to_string()))?
            .join(path)
    };
    let parent = absolute
        .parent()
        .ok_or_else(|| IndexError::Schema("registry writer path has no parent".into()))?;
    let retained = kio_core::store_dir::StoreDirectory::open_or_create_directories(parent)
        .map_err(|error| IndexError::Schema(error.to_string()))?;
    kio_core::private_fs::verify_private_directory_handle(&retained)
        .map_err(|error| IndexError::Schema(error.to_string()))?;
    Ok((absolute, Some(retained)))
}

pub(crate) fn verify_private_registry_parent(
    handle: &fs::File,
    path: &Path,
) -> std::result::Result<(), RegistrySnapshotError> {
    let retained = kio_core::store_dir::StoreDirectory::from_retained(
        handle
            .try_clone()
            .map_err(|error| RegistrySnapshotError::UnstableBusy(error.to_string()))?,
        path.to_path_buf(),
    )
    .map_err(|error| RegistrySnapshotError::UnsafeIntegrity(error.to_string()))?;
    kio_core::private_fs::verify_private_directory_handle(&retained)
        .map_err(|error| RegistrySnapshotError::UnsafeIntegrity(error.to_string()))
}

impl RegistryDb {
    /// Open the registry as a strictly read-only cache. Evidence operations use
    /// this path so an absent registry never creates directories, a database,
    /// WAL sidecars, or schema state.
    pub fn open_read_only(
        path: impl AsRef<Path>,
    ) -> std::result::Result<Self, RegistrySnapshotError> {
        Self::open_read_only_with_control(path, &kio_core::control::ControlHandle::default())
    }

    pub fn open_read_only_with_control(
        path: impl AsRef<Path>,
        control: &kio_core::control::ControlHandle,
    ) -> std::result::Result<Self, RegistrySnapshotError> {
        Self::open_read_only_with_budget(path.as_ref(), REGISTRY_SNAPSHOT_RETRY_WINDOW, control)
    }

    /// Return a private, owned snapshot rather than opening the mutable source
    /// registry.  SQLite is therefore free to create a private SHM file while
    /// the source main/WAL/SHM leaves remain untouched.  This is deliberately
    /// stable-or-fail, not a claim of a cross-file atomic filesystem snapshot.
    fn open_read_only_with_budget(
        path: &Path,
        retry_window: Duration,
        control: &kio_core::control::ControlHandle,
    ) -> std::result::Result<Self, RegistrySnapshotError> {
        let started = Instant::now();
        let mut saw_instability = None;
        for attempt in 0..MAX_REGISTRY_SNAPSHOT_ATTEMPTS {
            let result = match stable_registry_parent_binding(
                path,
                control,
                #[cfg(test)]
                None,
            ) {
                Ok(Some(binding)) => snapshot_attempt(&binding, control, None, attempt),
                Ok(None) => Err(SnapshotAttemptError::Missing),
                Err(error) => Err(error),
            };
            match result {
                Ok((conn, snapshot)) => {
                    return Ok(Self {
                        conn,
                        _inherited_parent: None,
                        _snapshot: Some(snapshot),
                    });
                }
                Err(SnapshotAttemptError::Unsafe(message)) => {
                    return Err(RegistrySnapshotError::UnsafeIntegrity(message));
                }
                Err(SnapshotAttemptError::Missing) => {
                    return match saw_instability {
                        Some(message) => Err(RegistrySnapshotError::UnstableBusy(message)),
                        None => Err(RegistrySnapshotError::Missing),
                    };
                }
                Err(SnapshotAttemptError::Unstable(message)) => {
                    saw_instability = Some(message.clone());
                    if started.elapsed() >= retry_window
                        || attempt + 1 == MAX_REGISTRY_SNAPSHOT_ATTEMPTS
                    {
                        return Err(RegistrySnapshotError::UnstableBusy(message));
                    }
                }
            }
            // Keep retrying within the same five-second busy boundary used by
            // registry SQLite handles.  A short yield avoids a hot spin while
            // preserving a bounded failure mode.
            std::thread::yield_now();
        }
        Err(RegistrySnapshotError::UnstableBusy(
            saw_instability
                .unwrap_or_else(|| "scope registry snapshot retry budget exhausted".to_owned()),
        ))
    }

    #[cfg(test)]
    fn open_read_only_for_test(
        path: &Path,
        attempts: usize,
        hook: SnapshotTestHook,
    ) -> std::result::Result<Self, RegistrySnapshotError> {
        Self::open_read_only_for_test_with_hooks(
            path,
            attempts,
            RegistryTestHooks {
                snapshot: Some(hook),
                ..RegistryTestHooks::default()
            },
        )
    }

    #[cfg(test)]
    fn open_read_only_for_test_with_hooks(
        path: &Path,
        attempts: usize,
        hooks: RegistryTestHooks,
    ) -> std::result::Result<Self, RegistrySnapshotError> {
        let control = kio_core::control::ControlHandle::default();
        let mut saw_instability = None;
        for attempt in 0..attempts {
            let result = match stable_registry_parent_binding(path, &control, Some(&hooks)) {
                Ok(Some(binding)) => snapshot_attempt(&binding, &control, Some(&hooks), attempt),
                Ok(None) => Err(SnapshotAttemptError::Missing),
                Err(error) => Err(error),
            };
            match result {
                Ok((conn, snapshot)) => {
                    return Ok(Self {
                        conn,
                        _inherited_parent: None,
                        _snapshot: Some(snapshot),
                    });
                }
                Err(SnapshotAttemptError::Unsafe(message)) => {
                    return Err(RegistrySnapshotError::UnsafeIntegrity(message));
                }
                Err(SnapshotAttemptError::Missing) => {
                    return match saw_instability {
                        Some(message) => Err(RegistrySnapshotError::UnstableBusy(message)),
                        None => Err(RegistrySnapshotError::Missing),
                    };
                }
                Err(SnapshotAttemptError::Unstable(message)) if attempt + 1 == attempts => {
                    return Err(RegistrySnapshotError::UnstableBusy(message));
                }
                Err(SnapshotAttemptError::Unstable(message)) => saw_instability = Some(message),
            }
        }
        Err(RegistrySnapshotError::UnstableBusy(
            "scope registry test retry budget exhausted".to_owned(),
        ))
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_control(path, &kio_core::control::ControlHandle::default())
    }

    pub fn open_with_control(
        path: impl AsRef<Path>,
        control: &kio_core::control::ControlHandle,
    ) -> Result<Self> {
        let (sqlite_path, inherited_parent) = prepare_registry_writer(path.as_ref(), control)?;
        let parent = inherited_parent
            .as_ref()
            .ok_or_else(|| IndexError::Schema("registry writer has no retained parent".into()))?;
        let conn = crate::platform::open_private_registry_connection(
            &sqlite_path,
            parent,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        // P6 (05 §1.8 / docs/05:565): serialize concurrent writers with WAL +
        // busy_timeout so a parallel `kio init`/`index` upsert waits (up to 5s)
        // for the write lock instead of hitting SQLITE_BUSY and silently dropping
        // the scope registration, and so a concurrent reader sees the last
        // committed snapshot rather than a transient open failure. WAL is a
        // persistent DB property; busy_timeout is per-connection.
        conn.busy_timeout(Duration::from_millis(5000))?;
        let _journal_mode: String =
            conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS scopes (
                scope_id TEXT NOT NULL,
                kio_path TEXT NOT NULL,
                root_path TEXT NOT NULL,
                participates_in_global_search INTEGER NOT NULL DEFAULT 1,
                indexed INTEGER NOT NULL DEFAULT 0,
                last_seen_at TEXT NOT NULL,
                PRIMARY KEY (scope_id, kio_path)
            );",
        )?;
        Ok(Self {
            conn,
            _inherited_parent: inherited_parent,
            _snapshot: None,
        })
    }

    pub fn open_default() -> Result<Self> {
        Self::open_default_with_control(&kio_core::control::ControlHandle::default())
    }

    pub fn open_default_with_control(control: &kio_core::control::ControlHandle) -> Result<Self> {
        Self::open_with_control(default_registry_path_with_control(control)?, control)
    }

    pub fn open_default_read_only() -> Result<Self> {
        Self::open_default_read_only_with_control(&kio_core::control::ControlHandle::default())
    }

    pub fn open_default_read_only_with_control(
        control: &kio_core::control::ControlHandle,
    ) -> Result<Self> {
        Self::open_read_only_with_control(default_registry_path_with_control(control)?, control)
            .map_err(index_error_from_snapshot)
    }

    /// R15-3: retire every registration for `kio_path` whose `scope_id` differs
    /// from `current_scope_id`, returning the number of rows removed. A deleted-
    /// then-re-`init`ed `.kio` mints a FRESH `scope_id` at the SAME path; because
    /// the primary key is `(scope_id, kio_path)`, the stale row otherwise survives
    /// forever and multi-scope search returns the same document twice — once via the
    /// dead `scope_id` whose Evidence Pointers can no longer resolve
    /// (`KIO-E-EVIDENCE-SCOPE-UNREACHABLE-001`). The registry is a recoverable search
    /// cache (03 §4), so dropping a stale row is always safe: the live scope re-adds
    /// itself here. Call this immediately before [`upsert`](Self::upsert).
    pub fn retire_stale_kio_path(&self, kio_path: &str, current_scope_id: &str) -> Result<usize> {
        let removed = self.conn.execute(
            "DELETE FROM scopes WHERE kio_path = ?1 AND scope_id != ?2",
            params![kio_path, current_scope_id],
        )?;
        Ok(removed)
    }

    pub fn upsert(&self, entry: &RegistryEntry) -> Result<()> {
        self.conn.execute(
            "INSERT INTO scopes (
                scope_id, kio_path, root_path,
                participates_in_global_search, indexed, last_seen_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT (scope_id, kio_path) DO UPDATE SET
                root_path = excluded.root_path,
                participates_in_global_search = excluded.participates_in_global_search,
                indexed = MAX(scopes.indexed, excluded.indexed),
                last_seen_at = excluded.last_seen_at",
            params![
                entry.scope_id,
                entry.kio_path,
                entry.root_path,
                entry.participates_in_global_search as i64,
                entry.indexed as i64,
                entry.last_seen_at,
            ],
        )?;
        Ok(())
    }

    /// Atomically replace app-prevalidated former registrations with their new
    /// paths. This is deliberately narrower than pruning: only the exact
    /// `(scope_id, kio_path)` pairs supplied by the app are deleted.
    pub fn rebind_scope_registrations(
        &mut self,
        rebinds: &[ScopeRegistrationRebind],
    ) -> Result<()> {
        let transaction = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut seen_scope_ids = BTreeSet::new();
        let mut seen_new_kio_paths = BTreeSet::new();
        for rebind in rebinds {
            if !seen_scope_ids.insert(rebind.scope_id.as_str()) {
                return Err(crate::IndexError::Contract(format!(
                    "scope registration rebind repeats scope_id: {}",
                    rebind.scope_id
                )));
            }
            if rebind.scope_id != rebind.new_registration.scope_id {
                return Err(crate::IndexError::Contract(
                    "scope registration rebind scope_id disagrees with new registration".to_owned(),
                ));
            }
            if !seen_new_kio_paths.insert(rebind.new_registration.kio_path.as_str()) {
                return Err(crate::IndexError::Contract(format!(
                    "scope registration rebind repeats destination kio_path: {}",
                    rebind.new_registration.kio_path
                )));
            }
            let expected = rebind
                .old_kio_paths
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>();
            if expected.len() != rebind.old_kio_paths.len() {
                return Err(crate::IndexError::Contract(format!(
                    "scope registration rebind repeats an old kio_path for scope_id: {}",
                    rebind.scope_id
                )));
            }
            let actual = {
                let mut statement = transaction
                    .prepare("SELECT kio_path FROM scopes WHERE scope_id = ?1 ORDER BY kio_path")?;
                statement
                    .query_map([&rebind.scope_id], |row| row.get::<_, String>(0))?
                    .collect::<std::result::Result<BTreeSet<_>, _>>()?
            };
            let resumed = BTreeSet::from([rebind.new_registration.kio_path.clone()]);
            if actual != expected && actual != resumed {
                return Err(crate::IndexError::Contract(format!(
                    "scope registration rebind observed unexpected paths for scope_id: {}",
                    rebind.scope_id
                )));
            }

            for old_kio_path in &rebind.old_kio_paths {
                if old_kio_path != &rebind.new_registration.kio_path {
                    transaction.execute(
                        "DELETE FROM scopes WHERE scope_id = ?1 AND kio_path = ?2",
                        params![&rebind.scope_id, old_kio_path],
                    )?;
                }
            }
            // A moved scope must be indexed again before global search uses its
            // new path, even when a resumed destination was indexed earlier.
            transaction.execute(
                "INSERT INTO scopes (
                    scope_id, kio_path, root_path,
                    participates_in_global_search, indexed, last_seen_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                ON CONFLICT(scope_id, kio_path) DO UPDATE SET
                    root_path = excluded.root_path,
                    participates_in_global_search = excluded.participates_in_global_search,
                    indexed = excluded.indexed,
                    last_seen_at = excluded.last_seen_at",
                params![
                    &rebind.scope_id,
                    &rebind.new_registration.kio_path,
                    &rebind.new_registration.root_path,
                    rebind.new_registration.participates_in_global_search as i64,
                    false,
                    &rebind.new_registration.last_seen_at,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Scopes eligible for default cross-scope search (05-runtime.md §1.8):
    /// `participates_in_global_search = true` and indexed. Deterministic order.
    pub fn search_targets(&self) -> Result<Vec<RegistryEntry>> {
        self.query_entries(
            "SELECT scope_id, kio_path, root_path,
                    participates_in_global_search, indexed, last_seen_at
             FROM scopes
             WHERE participates_in_global_search = 1 AND indexed = 1
             ORDER BY root_path, scope_id",
            params![],
        )
    }

    /// All registrations for a scope_id, most recently seen first
    /// (08-evidence-pointer-spec.md §3.1 step 1b).
    pub fn lookup_scope_id(&self, scope_id: &str) -> Result<Vec<RegistryEntry>> {
        self.query_entries(
            "SELECT scope_id, kio_path, root_path,
                    participates_in_global_search, indexed, last_seen_at
             FROM scopes
             WHERE scope_id = ?1
             ORDER BY last_seen_at DESC, kio_path",
            params![scope_id],
        )
    }

    /// Evidence callers use this typed variant so an owned snapshot query
    /// failure cannot be mistaken for a missing cache and bypass duplicate
    /// resolution safeguards.
    pub fn lookup_scope_id_snapshot(
        &self,
        scope_id: &str,
    ) -> std::result::Result<Vec<RegistryEntry>, RegistrySnapshotError> {
        self.lookup_scope_id(scope_id).map_err(snapshot_query_error)
    }

    pub fn get(&self, scope_id: &str, kio_path: &str) -> Result<Option<RegistryEntry>> {
        let entry = self
            .conn
            .query_row(
                "SELECT scope_id, kio_path, root_path,
                        participates_in_global_search, indexed, last_seen_at
                 FROM scopes
                 WHERE scope_id = ?1 AND kio_path = ?2",
                params![scope_id, kio_path],
                row_to_entry,
            )
            .optional()?;
        Ok(entry)
    }

    /// Every registration, deterministic order — PB25's `--registry-prune`
    /// (step4b-contract-tests-p2b.md §H) enumerates the whole table to find
    /// unreachable rows, not just search targets or one scope_id's rows.
    pub fn all_entries(&self) -> Result<Vec<RegistryEntry>> {
        self.query_entries(
            "SELECT scope_id, kio_path, root_path,
                    participates_in_global_search, indexed, last_seen_at
             FROM scopes
             ORDER BY root_path, scope_id",
            params![],
        )
    }

    /// PB25: remove one `(scope_id, kio_path)` registration — used only for a
    /// row proven unreachable (no re-init, no re-discovery possible); a live
    /// duplicate is never removed here (PB21's fail-closed dedupe is a user
    /// decision, not automatic).
    pub fn remove(&self, scope_id: &str, kio_path: &str) -> Result<bool> {
        let removed = self.conn.execute(
            "DELETE FROM scopes WHERE scope_id = ?1 AND kio_path = ?2",
            params![scope_id, kio_path],
        )?;
        Ok(removed > 0)
    }

    fn query_entries(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<Vec<RegistryEntry>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params, row_to_entry)?;
        let mut entries = Vec::new();
        for row in rows {
            entries.push(row?);
        }
        Ok(entries)
    }
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<RegistryEntry> {
    Ok(RegistryEntry {
        scope_id: row.get(0)?,
        kio_path: row.get(1)?,
        root_path: row.get(2)?,
        participates_in_global_search: row.get::<_, i64>(3)? != 0,
        indexed: row.get::<_, i64>(4)? != 0,
        last_seen_at: row.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    include!("private_test_fixtures.rs");

    #[cfg(debug_assertions)]
    #[test]
    fn owner_control_registry_reads_xdg_at_resolution_time() {
        use kio_core::control::ControlHandle;
        use kio_core::test_control::{TestEnvGuard, test_env_lock};
        let _lock = test_env_lock().lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let outer = dir.path().join("outer");
        let inner = dir.path().join("inner");
        let control = ControlHandle::default();
        let _first = TestEnvGuard::set("XDG_DATA_HOME", &outer);
        assert_eq!(
            default_registry_path_with_control(&control).unwrap(),
            outer.join("kio/scope-registry.sqlite")
        );
        let _second = TestEnvGuard::set("XDG_DATA_HOME", &inner);
        assert_eq!(
            default_registry_path_with_control(&control).unwrap(),
            inner.join("kio/scope-registry.sqlite")
        );
    }

    #[derive(Debug, PartialEq, Eq)]
    struct RegistrySnapshot {
        bytes: Vec<u8>,
        is_file: bool,
        len: u64,
        readonly: bool,
        #[cfg(unix)]
        mode: u32,
        #[cfg(unix)]
        nlink: u64,
    }

    fn snapshot_registry(path: &Path) -> RegistrySnapshot {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        let metadata = std::fs::symlink_metadata(path).unwrap();
        RegistrySnapshot {
            bytes: std::fs::read(path).unwrap(),
            is_file: metadata.file_type().is_file(),
            len: metadata.len(),
            readonly: metadata.permissions().readonly(),
            #[cfg(unix)]
            mode: metadata.mode(),
            #[cfg(unix)]
            nlink: metadata.nlink(),
        }
    }

    fn entry(scope_id: &str, root: &str, participates: bool, indexed: bool) -> RegistryEntry {
        RegistryEntry {
            scope_id: scope_id.to_owned(),
            kio_path: format!("{root}/.kio"),
            root_path: root.to_owned(),
            participates_in_global_search: participates,
            indexed,
            last_seen_at: "2026-07-03T00:00:00Z".to_owned(),
        }
    }

    fn open_temp() -> (PrivateTestDirectory, RegistryDb) {
        let dir = private_test_dir();
        let db = RegistryDb::open(dir.path().join("scope-registry.sqlite")).unwrap();
        (dir, db)
    }

    #[cfg(target_os = "linux")]
    fn inherited_private_tempdir() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;

        tempfile::Builder::new()
            .prefix("kio-registry-inherited-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn writer_and_snapshot_keep_registry_files_private_and_refuse_unsafe_main() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let (directory, db) = open_temp();
        let path = directory.path().join("scope-registry.sqlite");
        db.upsert(&entry("scope-private", "/scope-private", true, true))
            .unwrap();
        for child in fs::read_dir(directory.path()).unwrap() {
            let child = child.unwrap();
            let metadata = child.metadata().unwrap();
            if metadata.is_file() {
                assert_eq!(metadata.mode() & 0o777, 0o600, "{}", child.path().display());
            }
        }
        let snapshot = RegistryDb::open_read_only(&path).unwrap();
        assert_eq!(snapshot.all_entries().unwrap().len(), 1);
        drop(snapshot);
        drop(db);
        let bytes = fs::read(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(RegistryDb::open(&path).is_err());
        assert!(matches!(
            RegistryDb::open_read_only(&path),
            Err(RegistrySnapshotError::UnsafeIntegrity(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o644);
    }

    #[test]
    fn open_sets_wal_and_busy_timeout() {
        // P6: the registry must open with WAL + a 5000ms busy_timeout so parallel
        // init/index writers serialize instead of silently dropping registrations.
        let (_dir, db) = open_temp();
        let journal: String = db
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal.to_lowercase(), "wal");
        let timeout: i64 = db
            .conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(timeout, 5000);
    }

    #[test]
    fn read_only_open_never_creates_a_missing_registry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("scope-registry.sqlite");
        assert!(RegistryDb::open_read_only(&path).is_err());
        assert!(!path.exists());
        assert!(!path.parent().unwrap().exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inherited_registry_writer_keeps_duplicated_parent_after_fd_retarget() {
        use std::os::fd::AsRawFd;

        let first = inherited_private_tempdir();
        let replacement = inherited_private_tempdir();
        let held_first = fs::File::open(first.path()).unwrap();
        // SAFETY: fcntl duplicates a live descriptor; the returned descriptor
        // is owned by this test and closed below.
        let descriptor = unsafe { libc::fcntl(held_first.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 100) };
        assert!(descriptor >= 100);
        let inherited = PathBuf::from(format!("/dev/fd/{descriptor}/kio/scope-registry.sqlite"));

        let db = RegistryDb::open(&inherited).unwrap();
        let replacement_root = fs::File::open(replacement.path()).unwrap();
        // SAFETY: both descriptors are live directory descriptors. Replacing
        // the caller-owned spelling must not retarget RegistryDb's duplicate.
        assert_eq!(
            unsafe { libc::dup2(replacement_root.as_raw_fd(), descriptor) },
            descriptor
        );
        db.upsert(&entry("scope_fd", "/tmp/fd", true, true))
            .unwrap();
        drop(db);

        assert!(first.path().join("kio/scope-registry.sqlite").is_file());
        assert!(
            !replacement
                .path()
                .join("kio/scope-registry.sqlite")
                .exists()
        );
        // SAFETY: this test owns `descriptor` after the dup2 above.
        assert_eq!(unsafe { libc::close(descriptor) }, 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn read_only_reads_inherited_descriptor_registry_without_mutating_source() {
        use std::os::fd::AsRawFd;

        let root = inherited_private_tempdir();
        let held_root = fs::File::open(root.path()).unwrap();
        let path = PathBuf::from(format!(
            "/dev/fd/{}/kio/scope-registry.sqlite",
            held_root.as_raw_fd()
        ));
        let writer = RegistryDb::open(&path).unwrap();
        writer
            .upsert(&entry(
                "scope_inherited_read",
                "/tmp/inherited-read",
                true,
                true,
            ))
            .unwrap();
        drop(writer);
        let source = root.path().join("kio/scope-registry.sqlite");
        let before = snapshot_registry(&source);

        let reader = RegistryDb::open_read_only(&path).unwrap();
        assert_eq!(
            reader
                .lookup_scope_id("scope_inherited_read")
                .unwrap()
                .len(),
            1
        );
        drop(reader);
        assert_eq!(snapshot_registry(&source), before);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn read_only_inherited_missing_suffix_is_a_zero_effect_cache_miss() {
        use std::os::fd::AsRawFd;

        let root = inherited_private_tempdir();
        let held_root = fs::File::open(root.path()).unwrap();
        let path = PathBuf::from(format!(
            "/dev/fd/{}/missing/scope-registry.sqlite",
            held_root.as_raw_fd()
        ));

        assert!(matches!(
            RegistryDb::open_read_only(&path),
            Err(RegistrySnapshotError::Missing)
        ));
        assert!(!root.path().join("missing").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn read_only_rejects_inherited_descriptor_retarget_during_snapshot() {
        use std::os::fd::AsRawFd;
        use std::sync::atomic::{AtomicBool, Ordering};

        let first = inherited_private_tempdir();
        let replacement = inherited_private_tempdir();
        let first_path = first.path().join("kio/scope-registry.sqlite");
        let replacement_path = replacement.path().join("kio/scope-registry.sqlite");
        let writer = RegistryDb::open(&first_path).unwrap();
        writer
            .upsert(&entry("scope_first", "/tmp/first", true, true))
            .unwrap();
        drop(writer);
        drop(RegistryDb::open(&replacement_path).unwrap());

        let held_first = fs::File::open(first.path()).unwrap();
        // SAFETY: fcntl duplicates a live directory descriptor and this test
        // closes the returned descriptor after the snapshot attempt.
        let descriptor = unsafe { libc::fcntl(held_first.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 100) };
        assert!(descriptor >= 100);
        let inherited = PathBuf::from(format!("/dev/fd/{descriptor}/kio/scope-registry.sqlite"));
        let replacement_root = fs::File::open(replacement.path()).unwrap();
        let swapped = std::sync::Arc::new(AtomicBool::new(false));
        let swapped_once = swapped.clone();
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, _, _| {
            if phase == SnapshotTestPhase::AfterInitialManifest
                && !swapped_once.swap(true, Ordering::SeqCst)
            {
                // SAFETY: both descriptors remain live for the hook and the
                // test owns `descriptor` until it closes it below.
                assert_eq!(
                    unsafe { libc::dup2(replacement_root.as_raw_fd(), descriptor) },
                    descriptor
                );
            }
        });

        assert!(matches!(
            RegistryDb::open_read_only_for_test(&inherited, 1, hook),
            Err(RegistrySnapshotError::UnsafeIntegrity(_))
        ));
        // SAFETY: this test owns `descriptor` after the dup2 above.
        assert_eq!(unsafe { libc::close(descriptor) }, 0);
    }

    #[cfg(unix)]
    #[test]
    fn read_only_rejects_symlinked_registry_parent_without_touching_target() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let victim = tempfile::tempdir().unwrap();
        let linked_parent = root.path().join("registry-parent");
        symlink(victim.path(), &linked_parent).unwrap();
        let path = linked_parent.join("scope-registry.sqlite");

        assert!(matches!(
            RegistryDb::open_read_only(&path),
            Err(RegistrySnapshotError::UnsafeIntegrity(_))
        ));
        assert!(!victim.path().join("scope-registry.sqlite").exists());
    }

    #[test]
    fn read_only_open_reads_existing_registry_without_schema_changes() {
        let (dir, db) = open_temp();
        db.upsert(&entry("scope_a", "/tmp/a", true, true)).unwrap();
        let db = RegistryDb::open_read_only(dir.path().join("scope-registry.sqlite")).unwrap();
        assert_eq!(db.lookup_scope_id("scope_a").unwrap().len(), 1);
    }

    #[test]
    fn read_only_lookup_does_not_create_wal_sidecars_or_mutate_registry() {
        let dir = private_test_dir();
        let path = dir.path().join("scope-registry.sqlite");
        let wal_path = dir.path().join("scope-registry.sqlite-wal");
        let shm_path = dir.path().join("scope-registry.sqlite-shm");
        let expected = entry("scope_a", "/tmp/a", true, true);

        // Create a WAL-mode registry with a committed lookup row, then close the
        // writer. Closing the only writer checkpoints it, leaving no sidecars for
        // the read-only evidence path to inherit.
        let writer = RegistryDb::open(&path).unwrap();
        writer.upsert(&expected).unwrap();
        let journal_mode: String = writer
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode.to_lowercase(), "wal");
        drop(writer);
        assert!(
            !wal_path.exists(),
            "test precondition: writer close must leave no -wal sidecar"
        );
        assert!(
            !shm_path.exists(),
            "test precondition: writer close must leave no -shm sidecar"
        );
        let before = snapshot_registry(&path);
        assert_eq!(before.bytes.get(18), Some(&2), "WAL read version missing");
        assert_eq!(before.bytes.get(19), Some(&2), "WAL write version missing");

        let reader = RegistryDb::open_read_only(&path).unwrap();
        assert_eq!(reader.lookup_scope_id("scope_a").unwrap(), vec![expected]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let temp = reader._snapshot.as_ref().unwrap()._storage.path();
            assert_eq!(fs::metadata(temp).unwrap().mode() & 0o777, 0o700);
            assert_eq!(
                fs::metadata(temp.join(PRIVATE_REGISTRY_MAIN))
                    .unwrap()
                    .mode()
                    & 0o777,
                0o600
            );
            // Any SQLite sidecars here are private implementation state; the
            // source sidecars remain absent below.
        }
        drop(reader);

        assert_eq!(snapshot_registry(&path), before, "main registry changed");
        let wal_exists = wal_path.exists();
        let shm_exists = shm_path.exists();
        assert!(
            !wal_exists && !shm_exists,
            "read-only lookup created WAL sidecars: -wal={wal_exists}, -shm={shm_exists}"
        );
    }

    #[test]
    fn read_only_snapshot_sees_committed_uncheckpointed_wal_without_touching_source() {
        let dir = private_test_dir();
        let path = dir.path().join("scope-registry.sqlite");
        let wal = dir.path().join("scope-registry.sqlite-wal");
        let shm = dir.path().join("scope-registry.sqlite-shm");
        let expected = entry("scope_wal", "/tmp/wal", true, true);

        let writer = RegistryDb::open(&path).unwrap();
        writer
            .conn
            .pragma_update(None, "wal_autocheckpoint", 0i64)
            .unwrap();
        // Keep a reader transaction alive so the committed WAL frame remains
        // available for the snapshot instead of relying on close/checkpoint.
        let reader = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        reader
            .execute_batch("BEGIN; SELECT count(*) FROM scopes;")
            .unwrap();
        writer.upsert(&expected).unwrap();
        assert!(wal.is_file(), "fixture requires a source WAL");
        assert!(shm.is_file(), "fixture requires a source SHM");
        let fresh = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let count: i64 = fresh
            .query_row(
                "SELECT count(*) FROM scopes WHERE scope_id = ?1",
                params!["scope_wal"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "fixture committed row must be normally visible");
        let before_main = snapshot_registry(&path);
        let before_wal = snapshot_registry(&wal);
        let before_shm = snapshot_registry(&shm);

        let snapshot = RegistryDb::open_read_only(&path).unwrap();
        assert_eq!(
            snapshot.lookup_scope_id("scope_wal").unwrap(),
            vec![expected]
        );
        drop(snapshot);

        assert_eq!(snapshot_registry(&path), before_main);
        assert_eq!(snapshot_registry(&wal), before_wal);
        assert_eq!(snapshot_registry(&shm), before_shm);
        drop(fresh);
        drop(reader);
        drop(writer);
    }

    #[test]
    fn read_only_missing_registry_is_typed_and_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("scope-registry.sqlite");
        assert!(matches!(
            RegistryDb::open_read_only(&path),
            Err(RegistrySnapshotError::Missing)
        ));
        assert!(!path.parent().unwrap().exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_registry_path_normalization_only_rewrites_var_alias() {
        assert_eq!(
            crate::platform::normalized_absolute_registry_path(Path::new(
                "/var/folders/kio/scope-registry.sqlite"
            ))
            .unwrap(),
            PathBuf::from("/private/var/folders/kio/scope-registry.sqlite")
        );
        assert_eq!(
            crate::platform::normalized_absolute_registry_path(Path::new(
                "/tmp/kio/scope-registry.sqlite"
            ))
            .unwrap(),
            PathBuf::from("/tmp/kio/scope-registry.sqlite")
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_only_snapshot_rejects_immediate_parent_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        symlink(&target, &link).unwrap();
        let path = link.join("scope-registry.sqlite");
        assert!(matches!(
            RegistryDb::open_read_only(&path),
            Err(RegistrySnapshotError::UnsafeIntegrity(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn read_only_snapshot_rejects_nested_ancestor_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        symlink(&target, &link).unwrap();
        let child = link.join("child");
        fs::create_dir(target.join("child")).unwrap();
        let path = child.join("scope-registry.sqlite");
        assert!(matches!(
            RegistryDb::open_read_only(&path),
            Err(RegistrySnapshotError::UnsafeIntegrity(_))
        ));
    }

    #[test]
    fn parent_component_regular_directory_replacement_between_stat_and_open_is_unsafe() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("nested");
        fs::create_dir(&nested).unwrap();
        let path = nested.join("scope-registry.sqlite");
        let moved = dir.path().join("moved");
        let final_index = crate::platform::normalized_absolute_registry_path(&path)
            .unwrap()
            .parent()
            .unwrap()
            .components()
            .filter(|component| matches!(component, Component::Normal(_)))
            .count()
            - 1;
        let hooks = RegistryTestHooks {
            parent_component: Some(std::sync::Arc::new(move |index| {
                if index == final_index {
                    fs::rename(&nested, &moved).unwrap();
                    fs::create_dir(&nested).unwrap();
                }
            })),
            ..RegistryTestHooks::default()
        };
        let result = RegistryDb::open_read_only_for_test_with_hooks(&path, 1, hooks);
        match result {
            Err(RegistrySnapshotError::UnsafeIntegrity(_)) => {}
            Err(error) => panic!("expected unsafe replacement failure, got {error}"),
            Ok(_) => panic!("expected replacement failure"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn all_absent_whole_parent_substitution_is_unsafe_not_missing() {
        let dir = private_test_dir();
        let canonical_parent = dir.path().join("canonical");
        private_test_subdirectory(&canonical_parent);
        let path = canonical_parent.join("scope-registry.sqlite");
        let moved_parent = dir.path().join("detached");
        let replacement_parent = canonical_parent.clone();
        let replacement_path = path.clone();
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, attempt, _| {
            if attempt == 0 && phase == SnapshotTestPhase::AfterFinalManifestBeforeParentRecheck {
                fs::rename(&replacement_parent, &moved_parent).unwrap();
                private_test_subdirectory(&replacement_parent);
                let replacement = RegistryDb::open(&replacement_path).unwrap();
                replacement
                    .upsert(&entry("replacement", "/tmp/replacement", true, true))
                    .unwrap();
            }
        });

        match RegistryDb::open_read_only_for_test(&path, 1, hook) {
            Err(RegistrySnapshotError::UnsafeIntegrity(message)) => {
                assert!(message.contains("canonical parent identity changed"));
            }
            Err(error) => panic!("expected unsafe parent substitution, got {error}"),
            Ok(_) => panic!("detached all-absent parent must not produce a snapshot"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn present_snapshot_whole_parent_substitution_is_unsafe() {
        let dir = private_test_dir();
        let canonical_parent = dir.path().join("canonical");
        private_test_subdirectory(&canonical_parent);
        let path = canonical_parent.join("scope-registry.sqlite");
        let original = RegistryDb::open(&path).unwrap();
        original
            .upsert(&entry("original", "/tmp/original", true, true))
            .unwrap();
        drop(original);
        let moved_parent = dir.path().join("detached");
        let replacement_parent = canonical_parent.clone();
        let replacement_path = path.clone();
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, attempt, _| {
            if attempt == 0 && phase == SnapshotTestPhase::AfterFinalManifestBeforeParentRecheck {
                fs::rename(&replacement_parent, &moved_parent).unwrap();
                private_test_subdirectory(&replacement_parent);
                let replacement = RegistryDb::open(&replacement_path).unwrap();
                replacement
                    .upsert(&entry("replacement", "/tmp/replacement", true, true))
                    .unwrap();
            }
        });

        match RegistryDb::open_read_only_for_test(&path, 1, hook) {
            Err(RegistrySnapshotError::UnsafeIntegrity(message)) => {
                assert!(message.contains("canonical parent identity changed"));
            }
            Err(error) => panic!("expected unsafe parent substitution, got {error}"),
            Ok(_) => panic!("detached private snapshot must not be accepted"),
        }
    }

    #[test]
    fn snapshot_final_all_absent_is_unstable_not_missing() {
        let dir = private_test_dir();
        let path = dir.path().join("scope-registry.sqlite");
        let writer = RegistryDb::open(&path).unwrap();
        writer
            .upsert(&entry("scope_a", "/tmp/a", true, true))
            .unwrap();
        drop(writer);
        let main = path.clone();
        let wal = dir.path().join("scope-registry.sqlite-wal");
        let shm = dir.path().join("scope-registry.sqlite-shm");
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, _, _| {
            if phase == SnapshotTestPhase::AfterSnapshotQueryBeforeRecheck {
                let _ = fs::remove_file(&main);
                let _ = fs::remove_file(&wal);
                let _ = fs::remove_file(&shm);
            }
        });
        assert!(matches!(
            RegistryDb::open_read_only_for_test(&path, 1, hook),
            Err(RegistrySnapshotError::UnstableBusy(_))
        ));
    }

    #[test]
    fn snapshot_absent_to_present_sidecar_is_unstable() {
        let dir = private_test_dir();
        let path = dir.path().join("scope-registry.sqlite");
        let writer = RegistryDb::open(&path).unwrap();
        writer
            .upsert(&entry("scope_a", "/tmp/a", true, true))
            .unwrap();
        drop(writer);
        let shm = dir.path().join("scope-registry.sqlite-shm");
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, _, _| {
            if phase == SnapshotTestPhase::AfterSnapshotQueryBeforeRecheck {
                if !shm.exists() {
                    drop(private_test_file(&shm));
                }
                fs::write(&shm, b"changed").unwrap();
            }
        });
        assert!(matches!(
            RegistryDb::open_read_only_for_test(&path, 1, hook),
            Err(RegistrySnapshotError::UnstableBusy(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn private_snapshot_preopen_sidecar_symlink_is_unsafe() {
        use std::os::unix::fs::symlink;

        let dir = private_test_dir();
        let path = dir.path().join("scope-registry.sqlite");
        let writer = RegistryDb::open(&path).unwrap();
        writer
            .upsert(&entry("scope_a", "/tmp/a", true, true))
            .unwrap();
        drop(writer);
        let target = dir.path().join("outside-shm-target");
        fs::write(&target, b"outside").unwrap();
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, _, private| {
            if phase == SnapshotTestPhase::BeforeSnapshotSqliteOpen {
                let private = private.expect("pre-open hook carries private snapshot path");
                symlink(&target, private.join("snapshot.sqlite-shm")).unwrap();
            }
        });
        assert!(matches!(
            RegistryDb::open_read_only_for_test(&path, 1, hook),
            Err(RegistrySnapshotError::UnsafeIntegrity(_))
        ));
    }

    #[test]
    fn private_snapshot_preopen_main_replacement_is_unsafe() {
        let dir = private_test_dir();
        let path = dir.path().join("scope-registry.sqlite");
        let writer = RegistryDb::open(&path).unwrap();
        writer
            .upsert(&entry("scope_a", "/tmp/a", true, true))
            .unwrap();
        drop(writer);
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, _, private| {
            if phase == SnapshotTestPhase::BeforeSnapshotSqliteOpen {
                let private = private.expect("pre-open hook carries private snapshot path");
                let main = private.join(PRIVATE_REGISTRY_MAIN);
                let replacement = private.join("replacement.sqlite");
                fs::copy(&main, &replacement).unwrap();
                #[cfg(windows)]
                fs::remove_file(&main).unwrap();
                fs::rename(&replacement, &main).unwrap();
            }
        });
        assert!(matches!(
            RegistryDb::open_read_only_for_test(&path, 1, hook),
            Err(RegistrySnapshotError::UnsafeIntegrity(_))
        ));
    }

    #[test]
    fn snapshot_identity_replacement_is_unstable() {
        let dir = private_test_dir();
        let path = dir.path().join("scope-registry.sqlite");
        let writer = RegistryDb::open(&path).unwrap();
        writer
            .upsert(&entry("scope_a", "/tmp/a", true, true))
            .unwrap();
        drop(writer);
        let replacement = dir.path().join("replacement.sqlite");
        let source = path.clone();
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, _, _| {
            if phase == SnapshotTestPhase::AfterSnapshotQueryBeforeRecheck {
                write_private_test_file(&replacement, fs::read(&source).unwrap());
                #[cfg(windows)]
                fs::remove_file(&source).unwrap();
                fs::rename(&replacement, &source).unwrap();
            }
        });
        assert!(matches!(
            RegistryDb::open_read_only_for_test(&path, 1, hook),
            Err(RegistrySnapshotError::UnstableBusy(_))
        ));
    }

    #[cfg(unix)]
    fn source_shm_rotation_during_observation(
        phase_to_rotate: SnapshotTestPhase,
        rotate_every_attempt: bool,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = private_test_dir();
        let path = dir.path().join("scope-registry.sqlite");
        let expected = entry("scope_a", "/tmp/a", true, true);
        let writer = RegistryDb::open(&path).unwrap();
        writer.upsert(&expected).unwrap();
        drop(writer);
        let shm = crate::platform::normalized_absolute_registry_path(
            &dir.path().join("scope-registry.sqlite-shm"),
        )
        .unwrap();
        write_private_test_file(&shm, b"initial");

        let rotations = std::sync::Arc::new(AtomicUsize::new(0));
        let hook_rotations = std::sync::Arc::clone(&rotations);
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, attempt, source| {
            if phase != phase_to_rotate
                || (!rotate_every_attempt && attempt != 0)
                || source != Some(shm.as_path())
            {
                return;
            }
            let replacement = shm.with_extension(format!("replacement-{attempt}"));
            write_private_test_file(&replacement, format!("rotated-{attempt}"));
            fs::rename(&replacement, &shm).unwrap();
            hook_rotations.fetch_add(1, Ordering::SeqCst);
        });

        let result = RegistryDb::open_read_only_for_test(&path, 2, hook);
        assert_eq!(
            rotations.load(Ordering::SeqCst),
            if rotate_every_attempt { 2 } else { 1 }
        );
        if rotate_every_attempt {
            assert!(matches!(
                result,
                Err(RegistrySnapshotError::UnstableBusy(_))
            ));
        } else {
            let snapshot = result.expect("stable retry after source SHM rotation");
            assert_eq!(snapshot.lookup_scope_id("scope_a").unwrap(), vec![expected]);
        }
    }

    #[cfg(unix)]
    #[test]
    fn source_shm_rotation_between_stat_and_open_retries_or_reports_busy() {
        source_shm_rotation_during_observation(SnapshotTestPhase::BeforeSourceLeafOpen, false);
        source_shm_rotation_during_observation(SnapshotTestPhase::BeforeSourceLeafOpen, true);
    }

    #[cfg(unix)]
    #[test]
    fn source_shm_rotation_between_read_and_restat_retries_or_reports_busy() {
        source_shm_rotation_during_observation(SnapshotTestPhase::BeforeSourceLeafRestat, false);
        source_shm_rotation_during_observation(SnapshotTestPhase::BeforeSourceLeafRestat, true);
    }

    #[test]
    fn snapshot_churn_exhausts_deterministic_test_retry_budget() {
        let dir = private_test_dir();
        let path = dir.path().join("scope-registry.sqlite");
        let writer = RegistryDb::open(&path).unwrap();
        writer
            .upsert(&entry("scope_a", "/tmp/a", true, true))
            .unwrap();
        drop(writer);
        let shm = dir.path().join("scope-registry.sqlite-shm");
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, attempt, _| {
            if phase == SnapshotTestPhase::AfterSnapshotQueryBeforeRecheck {
                if !shm.exists() {
                    drop(private_test_file(&shm));
                }
                fs::write(&shm, format!("churn-{attempt}")).unwrap();
            }
        });
        assert!(matches!(
            RegistryDb::open_read_only_for_test(&path, 2, hook),
            Err(RegistrySnapshotError::UnstableBusy(_))
        ));
    }

    #[test]
    fn snapshot_instability_followed_by_disappearance_is_not_missing() {
        let dir = private_test_dir();
        let path = dir.path().join("scope-registry.sqlite");
        let writer = RegistryDb::open(&path).unwrap();
        writer
            .upsert(&entry("scope_a", "/tmp/a", true, true))
            .unwrap();
        drop(writer);
        let main = path.clone();
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, attempt, _| {
            if phase == SnapshotTestPhase::AfterSnapshotQueryBeforeRecheck && attempt == 0 {
                fs::remove_file(&main).unwrap();
            }
        });
        assert!(matches!(
            RegistryDb::open_read_only_for_test(&path, 2, hook),
            Err(RegistrySnapshotError::UnstableBusy(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_wal_disappearance_is_unstable() {
        let dir = private_test_dir();
        let path = dir.path().join("scope-registry.sqlite");
        let wal = dir.path().join("scope-registry.sqlite-wal");
        let writer = RegistryDb::open(&path).unwrap();
        writer
            .conn
            .pragma_update(None, "wal_autocheckpoint", 0i64)
            .unwrap();
        writer
            .upsert(&entry("scope_a", "/tmp/a", true, true))
            .unwrap();
        assert!(wal.exists());
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, _, _| {
            if phase == SnapshotTestPhase::AfterSnapshotQueryBeforeRecheck {
                fs::remove_file(&wal).unwrap();
            }
        });
        assert!(matches!(
            RegistryDb::open_read_only_for_test(&path, 1, hook),
            Err(RegistrySnapshotError::UnstableBusy(_))
        ));
        drop(writer);
    }

    // Windows retains the canonical parent without delete sharing for the entire
    // snapshot attempt.  The Unix replacement race above is therefore
    // structurally impossible: prove that the OS blocks it and that the stable
    // original binding has the expected cache-miss semantics instead.
    #[cfg(windows)]
    #[test]
    fn all_absent_bound_parent_blocks_substitution_and_returns_true_missing() {
        use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

        let dir = tempfile::tempdir().unwrap();
        let canonical_parent = dir.path().join("canonical");
        private_test_subdirectory(&canonical_parent);
        let path = canonical_parent.join("scope-registry.sqlite");
        let moved_parent = dir.path().join("detached");
        let replacement_parent = canonical_parent.clone();
        let hook_moved_parent = moved_parent.clone();
        let rename_checked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_checked = std::sync::Arc::clone(&rename_checked);
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, attempt, _| {
            if attempt == 0 && phase == SnapshotTestPhase::AfterFinalManifestBeforeParentRecheck {
                let error = fs::rename(&replacement_parent, &hook_moved_parent)
                    .expect_err("the retained no-delete-share parent must block substitution");
                assert_eq!(
                    error.raw_os_error(),
                    Some(ERROR_SHARING_VIOLATION as i32),
                    "the canonical parent must remain bound for the full snapshot attempt"
                );
                hook_checked.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });

        assert!(matches!(
            RegistryDb::open_read_only_for_test(&path, 1, hook),
            Err(RegistrySnapshotError::Missing)
        ));
        assert!(
            rename_checked.load(std::sync::atomic::Ordering::SeqCst),
            "the test hook must prove the no-delete-share boundary"
        );
        assert!(
            canonical_parent.is_dir(),
            "the original parent must remain live"
        );
        assert!(
            !moved_parent.exists(),
            "a denied replacement must not create a detached parent"
        );
    }

    #[cfg(windows)]
    #[test]
    fn present_bound_parent_blocks_substitution_and_keeps_original_snapshot() {
        use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

        let dir = tempfile::tempdir().unwrap();
        let canonical_parent = dir.path().join("canonical");
        private_test_subdirectory(&canonical_parent);
        let path = canonical_parent.join("scope-registry.sqlite");
        let original = RegistryDb::open(&path).unwrap();
        original
            .upsert(&entry("original", "/tmp/original", true, true))
            .unwrap();
        drop(original);
        let moved_parent = dir.path().join("detached");
        let replacement_parent = canonical_parent.clone();
        let hook_moved_parent = moved_parent.clone();
        let rename_checked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_checked = std::sync::Arc::clone(&rename_checked);
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, attempt, _| {
            if attempt == 0 && phase == SnapshotTestPhase::AfterFinalManifestBeforeParentRecheck {
                let error = fs::rename(&replacement_parent, &hook_moved_parent)
                    .expect_err("the retained no-delete-share parent must block substitution");
                assert_eq!(error.raw_os_error(), Some(ERROR_SHARING_VIOLATION as i32));
                hook_checked.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });

        let snapshot = RegistryDb::open_read_only_for_test(&path, 1, hook)
            .expect("the original stable parent and snapshot must remain usable");
        assert_eq!(
            snapshot.lookup_scope_id("original").unwrap(),
            vec![entry("original", "/tmp/original", true, true)]
        );
        assert!(
            rename_checked.load(std::sync::atomic::Ordering::SeqCst),
            "the test hook must prove the no-delete-share boundary"
        );
        assert!(
            canonical_parent.is_dir(),
            "the original parent must remain live"
        );
        assert!(
            !moved_parent.exists(),
            "substitution must not have occurred"
        );
    }

    // A live Windows SQLite writer owns its WAL without delete sharing.  The
    // Unix disappearance race is thus impossible while the writer is alive;
    // prove the sharing boundary and that the untouched WAL-backed snapshot is
    // accepted from its original source rather than a fabricated transition.
    #[cfg(windows)]
    #[test]
    fn live_wal_blocks_deletion_and_preserves_original_snapshot() {
        use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

        let dir = private_test_dir();
        let path = dir.path().join("scope-registry.sqlite");
        let wal = dir.path().join("scope-registry.sqlite-wal");
        let expected = entry("scope_a", "/tmp/a", true, true);
        let writer = RegistryDb::open(&path).unwrap();
        writer
            .conn
            .pragma_update(None, "wal_autocheckpoint", 0i64)
            .unwrap();
        writer.upsert(&expected).unwrap();
        assert!(wal.exists());
        let delete_checked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_checked = std::sync::Arc::clone(&delete_checked);
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, _, _| {
            if phase == SnapshotTestPhase::AfterSnapshotQueryBeforeRecheck {
                let error = fs::remove_file(&wal)
                    .expect_err("the live writer must block WAL deletion on Windows");
                assert_eq!(error.raw_os_error(), Some(ERROR_SHARING_VIOLATION as i32));
                hook_checked.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });

        let snapshot = RegistryDb::open_read_only_for_test(&path, 1, hook)
            .expect("an unchanged, WAL-backed snapshot must remain valid");
        assert_eq!(snapshot.lookup_scope_id("scope_a").unwrap(), vec![expected]);
        assert!(
            delete_checked.load(std::sync::atomic::Ordering::SeqCst),
            "the test hook must prove the live-WAL sharing boundary"
        );
        drop(writer);
    }

    #[test]
    fn stable_malformed_registry_schema_is_unsafe() {
        let dir = private_test_dir();
        let path = dir.path().join("scope-registry.sqlite");
        drop(private_test_file(&path));
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE scopes (scope_id TEXT);")
            .unwrap();
        drop(conn);
        let snapshot = RegistryDb::open_read_only(&path).unwrap();
        match snapshot.lookup_scope_id_snapshot("scope") {
            Err(RegistrySnapshotError::UnsafeIntegrity(_)) => {}
            Err(error) => panic!("expected malformed schema unsafe error, got {error}"),
            Ok(_) => panic!("malformed schema lookup unexpectedly succeeded"),
        }
    }

    #[test]
    fn stable_empty_sqlite_registry_without_scopes_table_is_unsafe() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scope-registry.sqlite");
        drop(Connection::open(&path).unwrap());
        assert!(matches!(
            RegistryDb::open_read_only(&path),
            Err(RegistrySnapshotError::UnsafeIntegrity(_))
        ));
    }

    #[test]
    fn busy_snapshot_query_error_is_retryable() {
        let error = crate::IndexError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        ));
        assert!(matches!(
            snapshot_query_error(error),
            RegistrySnapshotError::UnstableBusy(_)
        ));
    }

    #[test]
    fn snapshot_query_cannot_open_and_io_errors_are_retryable() {
        for code in [rusqlite::ffi::SQLITE_CANTOPEN, rusqlite::ffi::SQLITE_IOERR] {
            let error = crate::IndexError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                None,
            ));
            assert!(matches!(
                snapshot_query_error(error),
                RegistrySnapshotError::UnstableBusy(_)
            ));
        }
    }

    #[test]
    fn snapshot_query_proven_corruption_and_schema_errors_are_unsafe() {
        for code in [
            rusqlite::ffi::SQLITE_CORRUPT,
            rusqlite::ffi::SQLITE_NOTADB,
            rusqlite::ffi::SQLITE_SCHEMA,
        ] {
            let error = crate::IndexError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                None,
            ));
            assert!(matches!(
                snapshot_query_error(error),
                RegistrySnapshotError::UnsafeIntegrity(_)
            ));
        }
    }

    #[test]
    fn private_snapshot_cannot_open_and_io_errors_are_retryable() {
        for code in [rusqlite::ffi::SQLITE_CANTOPEN, rusqlite::ffi::SQLITE_IOERR] {
            let error = rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None);
            assert!(matches!(
                sqlite_snapshot_error(error, "open registry snapshot", false),
                SnapshotAttemptError::Unstable(_)
            ));
        }
    }

    #[test]
    fn proven_snapshot_corruption_and_schema_errors_are_unsafe() {
        for code in [
            rusqlite::ffi::SQLITE_CORRUPT,
            rusqlite::ffi::SQLITE_NOTADB,
            rusqlite::ffi::SQLITE_SCHEMA,
        ] {
            let error = rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None);
            assert!(matches!(
                sqlite_snapshot_error(error, "query registry snapshot", true),
                SnapshotAttemptError::Unsafe(_)
            ));
        }
    }

    #[test]
    fn sqlite_error_is_unsafe_only_for_fixed_schema_probe() {
        let open_error = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
            None,
        );
        assert!(matches!(
            sqlite_snapshot_error(open_error, "open registry snapshot", false),
            SnapshotAttemptError::Unstable(_)
        ));
        let probe_error = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
            None,
        );
        assert!(matches!(
            sqlite_snapshot_error(probe_error, "query registry snapshot", true),
            SnapshotAttemptError::Unsafe(_)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_registry_path_policy_accepts_absolute_disk_canonical_disk_or_unc_roots() {
        for (accepted, expected_root) in [
            (r"C:\kio\scope-registry.sqlite", r"C:\"),
            (r"\\?\C:\kio\scope-registry.sqlite", r"\\?\C:\"),
            (
                r"\\server\share\kio\scope-registry.sqlite",
                r"\\server\share\",
            ),
        ] {
            let normalized =
                crate::platform::normalized_absolute_registry_path(Path::new(accepted))
                    .unwrap_or_else(|error| panic!("{accepted}: {error}"));
            assert_eq!(normalized.as_os_str(), Path::new(accepted).as_os_str());
            assert_eq!(
                crate::platform::registry_filesystem_root(&normalized)
                    .unwrap()
                    .as_os_str(),
                Path::new(expected_root).as_os_str(),
                "{accepted}"
            );
        }
        for rejected in [
            r"kio\scope-registry.sqlite",
            r"\kio\scope-registry.sqlite",
            r"C:kio\scope-registry.sqlite",
            r"C:\kio\..\scope-registry.sqlite",
            r"\\?\C:",
            r"\\?\C:kio\scope-registry.sqlite",
            r"\\?\C:/kio/scope-registry.sqlite",
            r"\\?\C:\kio/scope-registry.sqlite",
            r"\\?\C:\kio\..\scope-registry.sqlite",
            r"\\?\C:\kio\.\scope-registry.sqlite",
            r"\\?\UNC\server\share\kio\scope-registry.sqlite",
            r"\\?\Volume{00000000-0000-0000-0000-000000000000}\kio\scope-registry.sqlite",
            r"\\.\PhysicalDrive0",
            r"\\.\C:\kio\scope-registry.sqlite",
        ] {
            assert!(
                crate::platform::normalized_absolute_registry_path(Path::new(rejected)).is_err(),
                "{rejected}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_read_only_snapshot_accepts_canonical_drive_path_without_touching_source() {
        let (dir, writer) = open_temp();
        let path = dir.path().join("scope-registry.sqlite");
        let expected = entry("scope_canonical", dir.path().to_str().unwrap(), true, true);
        writer.upsert(&expected).unwrap();
        drop(writer);

        let canonical = fs::canonicalize(&path).unwrap();
        assert!(matches!(
            canonical.components().next(),
            Some(Component::Prefix(prefix))
                if matches!(prefix.kind(), std::path::Prefix::VerbatimDisk(_))
        ));
        assert_eq!(
            crate::platform::normalized_absolute_registry_path(&canonical)
                .unwrap()
                .as_os_str(),
            canonical.as_os_str(),
            "canonical source path must retain its exact namespace"
        );
        let binding = crate::platform::registry_parent_and_leaf(
            &canonical,
            &kio_core::control::ControlHandle::default(),
            None,
        )
        .unwrap();
        assert_eq!(binding.parent_path, canonical.parent().unwrap());
        drop(binding);
        let before = snapshot_registry(&path);
        let before_names = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<BTreeSet<_>>();
        assert_eq!(before_names.len(), 1, "fixture must have no WAL sidecars");

        let snapshot = RegistryDb::open_read_only(&canonical).unwrap();
        assert_eq!(
            snapshot
                .lookup_scope_id_snapshot("scope_canonical")
                .unwrap(),
            vec![expected]
        );
        drop(snapshot);

        assert_eq!(snapshot_registry(&path), before, "source registry changed");
        let after_names = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<BTreeSet<_>>();
        assert_eq!(after_names, before_names, "source sidecar files changed");
    }

    #[cfg(windows)]
    fn windows_security_descriptor(file: &fs::File) -> Vec<u8> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::{
            Foundation::{HANDLE, LocalFree},
            Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT},
            Security::{
                DACL_SECURITY_INFORMATION, GetSecurityDescriptorLength, OWNER_SECURITY_INFORMATION,
            },
        };

        let mut descriptor = std::ptr::null_mut();
        // SAFETY: the retained file handle is live, and GetSecurityInfo owns
        // the returned descriptor allocation until LocalFree below.
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle() as HANDLE,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(status, 0, "inspect fixture security descriptor");
        assert!(!descriptor.is_null());
        // SAFETY: GetSecurityInfo returned a valid self-relative descriptor.
        unsafe {
            let len = GetSecurityDescriptorLength(descriptor) as usize;
            let bytes = std::slice::from_raw_parts(descriptor.cast::<u8>(), len).to_vec();
            LocalFree(descriptor as _);
            bytes
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_registry_writer_creates_private_nested_parents_and_preserves_them_on_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let kio = data.join("kio");
        let path = kio.join("scope-registry.sqlite");
        assert!(!data.exists());
        let expected = entry("scope_private_parent", "/tmp/private-parent", true, true);
        let writer = RegistryDb::open(&path).unwrap();
        writer.upsert(&expected).unwrap();
        let before = [&data, &kio].map(|directory| {
            let retained = kio_core::private_fs::verify_private_directory(directory)
                .expect("new registry parent must have a protected owner-only DACL");
            let handle = retained.root_handle();
            (
                kio_core::platform::directory_handle_identity(handle.as_ref()).unwrap(),
                windows_security_descriptor(handle.as_ref()),
            )
        });
        assert!(writer._inherited_parent.is_some());
        drop(writer);

        let reopened = RegistryDb::open(&path).unwrap();
        assert_eq!(
            reopened.lookup_scope_id("scope_private_parent").unwrap(),
            vec![expected]
        );
        let after = [&data, &kio].map(|directory| {
            let retained = kio_core::private_fs::verify_private_directory(directory).unwrap();
            let handle = retained.root_handle();
            (
                kio_core::platform::directory_handle_identity(handle.as_ref()).unwrap(),
                windows_security_descriptor(handle.as_ref()),
            )
        });
        assert_eq!(
            after, before,
            "reopen must preserve directory identity and ACL"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_registry_writer_preserves_trusted_existing_parent_acl() {
        let dir = private_test_dir();
        let retained = kio_core::private_fs::verify_private_creation_parent(dir.path()).unwrap();
        let handle = retained.root_handle();
        let before_acl = windows_security_descriptor(handle.as_ref());
        let before_identity = kio_core::platform::directory_handle_identity(handle.as_ref());

        let writer = RegistryDb::open(dir.path().join("scope-registry.sqlite")).unwrap();
        writer
            .upsert(&entry("scope_existing_parent", "/tmp/existing", true, true))
            .unwrap();
        assert_eq!(
            writer
                .lookup_scope_id("scope_existing_parent")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(windows_security_descriptor(handle.as_ref()), before_acl);
        assert_eq!(
            kio_core::platform::directory_handle_identity(handle.as_ref()),
            before_identity
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_registry_writer_accepts_external_null_dacl_ancestor_without_acl_repair() {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::{
            Foundation::HANDLE,
            Security::Authorization::{SE_FILE_OBJECT, SetSecurityInfo},
            Security::{DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION},
            Storage::FileSystem::{
                FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, READ_CONTROL, WRITE_DAC,
            },
        };

        let dir = tempfile::tempdir().unwrap();
        let ancestor = dir.path().join("unsafe");
        fs::create_dir(&ancestor).unwrap();
        let handle = fs::OpenOptions::new()
            .read(true)
            .access_mode(FILE_READ_ATTRIBUTES | READ_CONTROL | WRITE_DAC)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(&ancestor)
            .unwrap();
        // SAFETY: this isolated fixture handle is live. A protected null DACL
        // intentionally grants everyone access and must never be repaired.
        assert_eq!(
            unsafe {
                SetSecurityInfo(
                    handle.as_raw_handle() as HANDLE,
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        assert!(kio_core::private_fs::verify_private_creation_parent(&ancestor).is_ok());
        // The external ancestor is allowed, but cannot itself be adopted as
        // an existing owner-private Kio directory or direct writer parent.
        assert!(kio_core::private_fs::verify_private_directory(&ancestor).is_err());
        let before_acl = windows_security_descriptor(&handle);
        let before_identity = kio_core::platform::directory_handle_identity(&handle).unwrap();
        let path = ancestor.join("data/kio/scope-registry.sqlite");

        let direct = ancestor.join("direct.sqlite");
        assert!(RegistryDb::open(&direct).is_err());
        assert!(!direct.exists());
        let writer = RegistryDb::open(&path).expect("private descendants under external ancestor");
        for owned in [ancestor.join("data"), ancestor.join("data/kio")] {
            kio_core::private_fs::verify_private_directory(&owned)
                .expect("new Kio directory must have a protected owner-only DACL");
        }
        kio_core::private_fs::read_private_file(&path, 1024 * 1024)
            .expect("new registry database must be an owner-private regular file");
        drop(writer);
        assert_eq!(windows_security_descriptor(&handle), before_acl);
        assert_eq!(
            kio_core::platform::directory_handle_identity(&handle),
            Some(before_identity)
        );
        assert_eq!(fs::read_dir(&ancestor).unwrap().count(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn windows_registry_writer_rejects_nondirectory_ancestor_without_creating_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let ancestor = dir.path().join("occupied");
        fs::write(&ancestor, b"preserve this file").unwrap();
        let before = snapshot_registry(&ancestor);
        let before_acl = windows_security_descriptor(&fs::File::open(&ancestor).unwrap());

        assert!(RegistryDb::open(ancestor.join("data/kio/scope-registry.sqlite")).is_err());
        assert_eq!(snapshot_registry(&ancestor), before);
        assert_eq!(
            windows_security_descriptor(&fs::File::open(&ancestor).unwrap()),
            before_acl
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn windows_read_only_missing_registry_never_creates_private_parents() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let path = data.join("kio/scope-registry.sqlite");

        assert!(matches!(
            RegistryDb::open_read_only(&path),
            Err(RegistrySnapshotError::Missing)
        ));
        assert!(!data.exists());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn snapshot_rejects_oversized_leaf_before_sqlite_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scope-registry.sqlite");
        let file = fs::File::create(&path).unwrap();
        file.set_len(MAX_REGISTRY_LEAF_BYTES + 1).unwrap();
        assert!(matches!(
            RegistryDb::open_read_only(&path),
            Err(RegistrySnapshotError::UnsafeIntegrity(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn read_only_snapshot_rejects_hardlinked_wal_leaf() {
        use std::fs::hard_link;

        let dir = private_test_dir();
        let path = dir.path().join("scope-registry.sqlite");
        let wal = dir.path().join("scope-registry.sqlite-wal");
        let victim = dir.path().join("wal-victim");
        let writer = RegistryDb::open(&path).unwrap();
        writer
            .conn
            .pragma_update(None, "wal_autocheckpoint", 0i64)
            .unwrap();
        writer
            .upsert(&entry("scope_wal", "/tmp/wal", true, true))
            .unwrap();
        assert!(wal.is_file());
        hard_link(&wal, &victim).unwrap();
        let error = match RegistryDb::open_read_only(&path) {
            Ok(_) => panic!("hardlinked WAL must be rejected"),
            Err(error) => error,
        };
        assert!(matches!(error, RegistrySnapshotError::UnsafeIntegrity(_)));
        drop(writer);
    }

    #[cfg(unix)]
    #[test]
    fn read_only_snapshot_rejects_symlinked_main_leaf() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.sqlite");
        fs::write(&target, b"not a registry").unwrap();
        let path = dir.path().join("scope-registry.sqlite");
        symlink(&target, &path).unwrap();
        assert!(matches!(
            RegistryDb::open_read_only(&path),
            Err(RegistrySnapshotError::UnsafeIntegrity(_))
        ));
    }

    #[test]
    fn search_targets_filters_participation_and_indexed() {
        let (_dir, db) = open_temp();
        db.upsert(&entry("scope_a", "/tmp/a", true, true)).unwrap();
        db.upsert(&entry("scope_b", "/tmp/b", false, true)).unwrap();
        db.upsert(&entry("scope_c", "/tmp/c", true, false)).unwrap();
        let targets = db.search_targets().unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].scope_id, "scope_a");
    }

    #[test]
    fn upsert_is_idempotent_and_updates_fields() {
        let (_dir, db) = open_temp();
        db.upsert(&entry("scope_a", "/tmp/a", true, false)).unwrap();
        let mut updated = entry("scope_a", "/tmp/a", true, true);
        updated.last_seen_at = "2026-07-04T00:00:00Z".to_owned();
        db.upsert(&updated).unwrap();
        let got = db.get("scope_a", "/tmp/a/.kio").unwrap().unwrap();
        assert!(got.indexed);
        assert_eq!(got.last_seen_at, "2026-07-04T00:00:00Z");
        assert_eq!(db.lookup_scope_id("scope_a").unwrap().len(), 1);
    }

    #[test]
    fn indexed_flag_is_not_cleared_by_later_unindexed_upsert() {
        let (_dir, db) = open_temp();
        db.upsert(&entry("scope_a", "/tmp/a", true, true)).unwrap();
        db.upsert(&entry("scope_a", "/tmp/a", true, false)).unwrap();
        assert!(db.get("scope_a", "/tmp/a/.kio").unwrap().unwrap().indexed);
    }

    #[test]
    fn rebind_scope_registrations_replaces_exact_old_rows_in_one_transaction() {
        let (_dir, mut db) = open_temp();
        db.upsert(&entry("scope_a", "/tmp/a-old", true, true))
            .unwrap();
        db.upsert(&entry("scope_b", "/tmp/b-old", false, true))
            .unwrap();
        let mut new_a = entry("scope_a", "/tmp/a-new", true, true);
        new_a.last_seen_at = "2026-07-05T00:00:00Z".to_owned();
        let new_b = entry("scope_b", "/tmp/b-new", false, true);

        db.rebind_scope_registrations(&[
            ScopeRegistrationRebind {
                scope_id: "scope_a".to_owned(),
                old_kio_paths: vec!["/tmp/a-old/.kio".to_owned()],
                new_registration: new_a.clone(),
            },
            ScopeRegistrationRebind {
                scope_id: "scope_b".to_owned(),
                old_kio_paths: vec!["/tmp/b-old/.kio".to_owned()],
                new_registration: new_b.clone(),
            },
        ])
        .unwrap();

        let rows_a = db.lookup_scope_id("scope_a").unwrap();
        let rows_b = db.lookup_scope_id("scope_b").unwrap();
        assert_eq!(rows_a.len(), 1);
        assert_eq!(rows_b.len(), 1);
        assert_eq!(rows_a[0].kio_path, new_a.kio_path);
        assert_eq!(rows_b[0].kio_path, new_b.kio_path);
        assert!(!rows_a[0].indexed);
        assert!(!rows_b[0].indexed);
    }

    #[test]
    fn rebind_scope_registrations_rejects_unexpected_rows_and_rolls_back_every_scope() {
        let (_dir, mut db) = open_temp();
        db.upsert(&entry("scope_a", "/tmp/a-old", true, true))
            .unwrap();
        db.upsert(&entry("scope_a", "/tmp/a-unexpected", true, true))
            .unwrap();
        db.upsert(&entry("scope_b", "/tmp/b-old", true, true))
            .unwrap();

        let result = db.rebind_scope_registrations(&[
            ScopeRegistrationRebind {
                scope_id: "scope_b".to_owned(),
                old_kio_paths: vec!["/tmp/b-old/.kio".to_owned()],
                new_registration: entry("scope_b", "/tmp/b-new", true, false),
            },
            ScopeRegistrationRebind {
                scope_id: "scope_a".to_owned(),
                old_kio_paths: vec!["/tmp/a-old/.kio".to_owned()],
                new_registration: entry("scope_a", "/tmp/a-new", true, false),
            },
        ]);
        assert!(matches!(result, Err(crate::IndexError::Contract(_))));
        assert!(db.get("scope_b", "/tmp/b-old/.kio").unwrap().is_some());
        assert!(db.get("scope_b", "/tmp/b-new/.kio").unwrap().is_none());
    }

    #[test]
    fn rebind_scope_registrations_accepts_already_completed_singleton() {
        let (_dir, mut db) = open_temp();
        db.upsert(&entry("scope_a", "/tmp/a-new", true, true))
            .unwrap();
        let new_registration = entry("scope_a", "/tmp/a-new", false, true);

        db.rebind_scope_registrations(&[ScopeRegistrationRebind {
            scope_id: "scope_a".to_owned(),
            old_kio_paths: vec!["/tmp/a-old/.kio".to_owned()],
            new_registration: new_registration.clone(),
        }])
        .unwrap();

        let rows = db.lookup_scope_id("scope_a").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kio_path, new_registration.kio_path);
        assert!(!rows[0].indexed);
        assert!(!rows[0].participates_in_global_search);
    }

    // R15-3: a deleted-then-re-`init`ed `.kio` mints a fresh scope_id at the same
    // path. Retiring the stale row before re-registering leaves exactly one row for
    // that path (no duplicate search target, no dead-pointer scope_id).
    #[test]
    fn retire_stale_kio_path_removes_only_other_scope_ids_at_same_path() {
        let (_dir, db) = open_temp();
        // Old scope_id registered + indexed at /tmp/a.
        db.upsert(&entry("scope_old", "/tmp/a", true, true))
            .unwrap();
        // An unrelated path must be untouched.
        db.upsert(&entry("scope_x", "/tmp/other", true, true))
            .unwrap();

        // Re-init: fresh scope_id at the SAME `.kio` path. Retire, then re-register.
        let removed = db
            .retire_stale_kio_path("/tmp/a/.kio", "scope_new")
            .unwrap();
        assert_eq!(removed, 1, "exactly the stale same-path row is retired");
        db.upsert(&entry("scope_new", "/tmp/a", true, true))
            .unwrap();

        // Only the fresh registration survives at /tmp/a.
        assert!(db.lookup_scope_id("scope_old").unwrap().is_empty());
        assert_eq!(db.lookup_scope_id("scope_new").unwrap().len(), 1);
        // The unrelated path is untouched.
        assert_eq!(db.lookup_scope_id("scope_x").unwrap().len(), 1);
        // Exactly one search target remains for the re-init'd path.
        let targets = db.search_targets().unwrap();
        assert_eq!(
            targets
                .iter()
                .filter(|t| t.kio_path == "/tmp/a/.kio")
                .count(),
            1
        );
    }

    #[test]
    fn lookup_scope_id_orders_by_last_seen_desc() {
        let (_dir, db) = open_temp();
        let mut old = entry("scope_a", "/tmp/old", true, true);
        old.last_seen_at = "2026-07-01T00:00:00Z".to_owned();
        db.upsert(&old).unwrap();
        let mut new = entry("scope_a", "/tmp/new", true, true);
        new.last_seen_at = "2026-07-02T00:00:00Z".to_owned();
        db.upsert(&new).unwrap();
        let found = db.lookup_scope_id("scope_a").unwrap();
        assert_eq!(found[0].root_path, "/tmp/new");
    }

    // PB25 (step4b-contract-tests-p2b.md §H): `all_entries`/`remove` are the
    // primitives `kio repair registry-prune` uses to enumerate the whole
    // table and delete a proven-unreachable row.
    #[test]
    fn all_entries_lists_every_row_and_remove_deletes_exactly_one() {
        let (_dir, db) = open_temp();
        db.upsert(&entry("scope_a", "/tmp/a", true, true)).unwrap();
        db.upsert(&entry("scope_b", "/tmp/b", true, true)).unwrap();
        assert_eq!(db.all_entries().unwrap().len(), 2);

        assert!(db.remove("scope_a", "/tmp/a/.kio").unwrap());
        let remaining = db.all_entries().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].scope_id, "scope_b");

        // Idempotent: removing an already-absent row is `false`, not an error.
        assert!(!db.remove("scope_a", "/tmp/a/.kio").unwrap());
    }
}
