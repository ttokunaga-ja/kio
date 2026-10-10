//! Stable, owned read-only snapshots of the mutable cost ledger.
//!
//! SQLite must never be given the source path here: even a read-only WAL
//! connection can create source sidecars.  We instead bind and copy the
//! source main/WAL through directory capabilities, observe main/WAL/SHM both
//! before and after the query, and let SQLite create sidecars only privately.

use std::{
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

use crate::platform::ledger_snapshot::{
    Identity, PrivateStorage, bound_parent, filesystem_root, identity,
    normalize_snapshot_entry_path, normalized_absolute, same_identity, stable_parent_binding,
    validate_file_links, validate_test_barrier_parent, verify_directory_binding,
    verify_private_storage, verify_regular_binding,
};
use cap_primitives::fs as cap_fs;
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};

use super::{
    model::BatchRequestRow,
    ops_sql::{ledger_month_total, stalled_rows},
};

// A device ledger is expected to be compact. These streaming bounds prevent a
// hostile or accidentally enormous device cache from turning a read-only
// search-status check into unbounded disk I/O; total admits one main plus WAL
// and SHM-sized observation without allocating any leaf wholesale.
pub(crate) const MAX_LEAF_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 128 * 1024 * 1024;
const MAX_ATTEMPTS: usize = 8;
const RETRY_WINDOW: Duration = Duration::from_secs(5);
pub(crate) const SNAPSHOT_MAIN: &str = "ledger.sqlite";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LedgerSnapshotError {
    Missing,
    UnsafeIntegrity(String),
    UnstableBusy(String),
}

impl std::fmt::Display for LedgerSnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str("cost ledger is absent"),
            Self::UnsafeIntegrity(s) | Self::UnstableBusy(s) => f.write_str(s),
        }
    }
}
impl std::error::Error for LedgerSnapshotError {}

#[derive(Debug)]
pub(crate) enum AttemptError {
    Missing,
    Unsafe(String),
    Unstable(String),
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum SnapshotPhase {
    AfterInitialManifest,
    BeforePrivateSqliteOpen,
    BeforeBoundLifecycleRead,
    AfterBoundLifecycleRead,
    BeforeBoundLifecycleArtifactRead,
    AfterBoundLifecycleArtifactRead,
    AfterProbeBeforeRecheck,
    AfterFinalManifestBeforeParentRecheck,
}
#[cfg(test)]
type SnapshotHook = std::sync::Arc<dyn Fn(SnapshotPhase, usize, &Path) + Send + Sync>;

#[cfg(test)]
type ParentComponentHook = std::sync::Arc<dyn Fn(usize)>;
#[cfg(test)]
thread_local! {
    static PARENT_COMPONENT_HOOK: std::cell::RefCell<Option<ParentComponentHook>> = const {
        std::cell::RefCell::new(None)
    };
}

/// This is deliberately separate from the post-stat parent hook: a missing
/// component has no opened handle to bind.  It lets the test prove that a
/// first absence is never returned as a zero-ledger miss without a second
/// whole-parent observation.
#[cfg(test)]
type MissingParentHook = std::sync::Arc<dyn Fn()>;
#[cfg(test)]
thread_local! {
    static MISSING_PARENT_HOOK: std::cell::RefCell<Option<MissingParentHook>> = const {
        std::cell::RefCell::new(None)
    };
}
#[cfg(test)]
pub(crate) fn run_missing_parent_hook() {
    MISSING_PARENT_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow().as_ref() {
            hook();
        }
    });
}

#[cfg(test)]
type PrivateStorageCreateHook = std::sync::Arc<dyn Fn() -> Result<(), AttemptError>>;
#[cfg(test)]
thread_local! {
    static PRIVATE_STORAGE_CREATE_HOOK: std::cell::RefCell<Option<PrivateStorageCreateHook>> = const {
        std::cell::RefCell::new(None)
    };
}
#[cfg(test)]
pub(crate) fn run_private_storage_create_hook() -> Result<(), AttemptError> {
    PRIVATE_STORAGE_CREATE_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow().as_ref() {
            hook()
        } else {
            Ok(())
        }
    })
}
#[cfg(test)]
pub(crate) fn run_parent_component_hook(index: usize) {
    PARENT_COMPONENT_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow().as_ref() {
            hook(index);
        }
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Leaf {
    Absent,
    Present(Observed),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Observed {
    identity: Identity,
    pub(crate) bytes: u64,
    pub(crate) sha256: [u8; 32],
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Manifest {
    pub(crate) main: Leaf,
    pub(crate) wal: Leaf,
    pub(crate) shm: Leaf,
}

/// A private SQLite connection whose backing temporary directory outlives it.
pub struct LedgerReadSnapshot {
    conn: Connection,
    // Field order intentionally closes SQLite before deleting its private dir.
    _storage: PrivateStorage,
}

impl LedgerReadSnapshot {
    /// Capture a stable snapshot.  `Missing` is a normal no-create cache miss;
    /// unsafe source shape and source churn are deliberately distinct.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LedgerSnapshotError> {
        Self::open_with_attempts(path.as_ref(), MAX_ATTEMPTS, None)
    }

    fn open_with_attempts(
        path: &Path,
        attempts: usize,
        #[cfg(test)] hook: Option<SnapshotHook>,
        #[cfg(not(test))] _hook: Option<()>,
    ) -> Result<Self, LedgerSnapshotError> {
        // Preserve the raw descriptor spelling until the shared resolver binds
        // it. `Path::components` would normalize an alias such as a doubled
        // separator before the resolver could reject it.
        let path = normalize_snapshot_entry_path(path)?;
        let started = Instant::now();
        let mut last = None;
        for attempt in 0..attempts {
            let result = match stable_parent_binding(&path) {
                Ok(Some((parent, leaf, parent_path))) => attempt_snapshot(
                    &parent,
                    &parent_path,
                    &leaf,
                    #[cfg(test)]
                    hook.as_ref(),
                    #[cfg(not(test))]
                    None,
                    attempt,
                ),
                // Missing is returned only after two complete nofollow parent
                // walks observe the same absence.  If a parent appears between
                // them, stable_parent_binding returns Unstable and we bind and
                // observe it on a later attempt instead of treating spend as 0.
                Ok(None) => Err(AttemptError::Missing),
                Err(error) => Err(error),
            };
            match result {
                Ok(snapshot) => return Ok(snapshot),
                Err(AttemptError::Missing) => {
                    return match last {
                        Some(message) => Err(LedgerSnapshotError::UnstableBusy(message)),
                        None => Err(LedgerSnapshotError::Missing),
                    };
                }
                Err(AttemptError::Unsafe(message)) => {
                    return Err(LedgerSnapshotError::UnsafeIntegrity(message));
                }
                Err(AttemptError::Unstable(message)) => {
                    last = Some(message.clone());
                    if attempt + 1 == attempts || started.elapsed() >= RETRY_WINDOW {
                        return Err(LedgerSnapshotError::UnstableBusy(message));
                    }
                    std::thread::yield_now();
                }
            }
        }
        Err(LedgerSnapshotError::UnstableBusy(last.unwrap_or_else(
            || "ledger snapshot retry budget exhausted".into(),
        )))
    }

    #[cfg(test)]
    fn open_for_test(
        path: &Path,
        attempts: usize,
        hook: SnapshotHook,
    ) -> Result<Self, LedgerSnapshotError> {
        Self::open_with_attempts(path, attempts, Some(hook))
    }

    /// CL59 totals from the private snapshot: settled costs plus active batch
    /// reservations.  This is the only operation the read-only budget path
    /// needs, so the source connection is never exposed.
    pub fn month_total(
        &self,
        scope_id: Option<&str>,
        adapter_kind: Option<&str>,
        month: &str,
    ) -> Result<f64, LedgerSnapshotError> {
        ledger_month_total(&self.conn, scope_id, adapter_kind, month).map_err(snapshot_query_error)
    }

    /// CL37/CL68 stalled batch rows from the owned private snapshot. The
    /// source SQLite connection remains private, so status cannot create source
    /// sidecars or acquire a source write lock.
    pub fn stalled_rows(&self) -> Result<Vec<BatchRequestRow>, LedgerSnapshotError> {
        stalled_rows(&self.conn).map_err(snapshot_query_error)
    }
}

fn snapshot_query_error(error: crate::PipelineError) -> LedgerSnapshotError {
    match error {
        crate::PipelineError::Contract { .. }
        | crate::PipelineError::Corrupt { .. }
        | crate::PipelineError::Schema(_) => {
            LedgerSnapshotError::UnsafeIntegrity(format!("query ledger snapshot: {error}"))
        }
        crate::PipelineError::Sqlite(rusqlite::Error::SqliteFailure(code, _))
            if matches!(
                code.code,
                rusqlite::ErrorCode::DatabaseCorrupt
                    | rusqlite::ErrorCode::NotADatabase
                    | rusqlite::ErrorCode::SchemaChanged
            ) =>
        {
            LedgerSnapshotError::UnsafeIntegrity(format!("query ledger snapshot: {code}"))
        }
        error => LedgerSnapshotError::UnstableBusy(format!("query ledger snapshot: {error}")),
    }
}

pub(crate) fn bound_ambient_parent(
    path: &Path,
) -> Result<(fs::File, String, PathBuf), LedgerSnapshotError> {
    let leaf = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| LedgerSnapshotError::UnsafeIntegrity("ledger file name is invalid".into()))?
        .to_owned();
    let parent_path = path
        .parent()
        .ok_or_else(|| LedgerSnapshotError::UnsafeIntegrity("ledger has no parent".into()))?
        .to_owned();
    let root = filesystem_root(path)?;
    let mut dir =
        cap_fs::open_ambient_dir(&root, cap_primitives::ambient_authority()).map_err(|e| {
            LedgerSnapshotError::UnstableBusy(format!("open ledger filesystem root: {e}"))
        })?;
    let mut bound = root.clone();
    #[cfg(test)]
    let mut parent_component_index = 0usize;
    for c in parent_path.components() {
        let Component::Normal(name) = c else { continue };
        let before =
            cap_fs::stat(&dir, Path::new(name), cap_fs::FollowSymlinks::No).map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    #[cfg(test)]
                    run_missing_parent_hook();
                    LedgerSnapshotError::Missing
                } else {
                    LedgerSnapshotError::UnstableBusy(format!("stat ledger parent: {e}"))
                }
            })?;
        validate_dir(&before, &name.to_string_lossy())?;
        #[cfg(test)]
        {
            run_parent_component_hook(parent_component_index);
            parent_component_index += 1;
        }
        let child = cap_fs::open_dir_nofollow(&dir, Path::new(name)).map_err(|e| {
            LedgerSnapshotError::UnsafeIntegrity(format!("open ledger parent nofollow: {e}"))
        })?;
        let opened = cap_fs::Metadata::from_file(&child).map_err(|e| {
            LedgerSnapshotError::UnstableBusy(format!("inspect ledger parent: {e}"))
        })?;
        let after =
            cap_fs::stat(&dir, Path::new(name), cap_fs::FollowSymlinks::No).map_err(|e| {
                LedgerSnapshotError::UnsafeIntegrity(format!("re-stat ledger parent: {e}"))
            })?;
        validate_dir(&opened, &name.to_string_lossy())?;
        validate_dir(&after, &name.to_string_lossy())?;
        if !same_identity(&before, &opened) || !same_identity(&before, &after) {
            return Err(LedgerSnapshotError::UnsafeIntegrity(
                "ledger parent changed while opening".into(),
            ));
        }
        bound.push(name);
        verify_directory_binding(&bound, &child, &name.to_string_lossy())?;
        dir = child;
    }
    Ok((dir, leaf, parent_path))
}

/// Bind the ledger parent twice when the first walk reports a missing
/// component.  A missing parent is a normal no-create cache miss only when
/// the second complete capability walk reaches the same absence.  This closes
/// the equivalent all-absent race for the parent itself: a creator that wins
/// between observations makes the operation retry and bind the new parent.
pub(crate) fn stable_ambient_parent_binding(
    path: &Path,
) -> Result<Option<(fs::File, String, PathBuf)>, AttemptError> {
    match bound_parent(path) {
        Ok(binding) => Ok(Some(binding)),
        Err(LedgerSnapshotError::Missing) => match bound_parent(path) {
            Err(LedgerSnapshotError::Missing) => Ok(None),
            Ok(_) => Err(AttemptError::Unstable(
                "ledger parent appeared while confirming its absence".into(),
            )),
            Err(error) => Err(parent_binding_error(error)),
        },
        Err(error) => Err(parent_binding_error(error)),
    }
}

/// Rebind the canonical parent immediately before accepting a snapshot result.
/// The retained descriptor remains the read authority during capture, but it
/// must still name the directory currently reachable through the canonical
/// nofollow path. A different directory is substitution; a temporarily absent
/// path is ordinary churn and remains retryable.
fn fresh_parent_binding(
    held_parent: &fs::File,
    parent_path: &Path,
    main: &str,
) -> Result<fs::File, AttemptError> {
    let canonical_path = parent_path.join(main);
    let (fresh_parent, fresh_main, fresh_parent_path) = match bound_parent(&canonical_path) {
        Ok(binding) => binding,
        Err(LedgerSnapshotError::Missing) => {
            return Err(AttemptError::Unstable(
                "ledger canonical parent disappeared while snapshotting".into(),
            ));
        }
        Err(error) => return Err(parent_binding_error(error)),
    };
    if fresh_main != main || fresh_parent_path != parent_path {
        return Err(AttemptError::Unsafe(
            "ledger canonical parent rebound to an unexpected path".into(),
        ));
    }
    let held_metadata = cap_fs::Metadata::from_file(held_parent).map_err(|error| {
        AttemptError::Unstable(format!("inspect retained ledger parent: {error}"))
    })?;
    let fresh_metadata = cap_fs::Metadata::from_file(&fresh_parent).map_err(|error| {
        AttemptError::Unstable(format!("inspect freshly bound ledger parent: {error}"))
    })?;
    if !same_identity(&held_metadata, &fresh_metadata) {
        return Err(AttemptError::Unsafe(
            "ledger canonical parent identity changed while snapshotting".into(),
        ));
    }
    Ok(fresh_parent)
}

pub(crate) fn parent_binding_error(error: LedgerSnapshotError) -> AttemptError {
    match error {
        LedgerSnapshotError::Missing => AttemptError::Missing,
        LedgerSnapshotError::UnsafeIntegrity(message) => AttemptError::Unsafe(message),
        LedgerSnapshotError::UnstableBusy(message) => AttemptError::Unstable(message),
    }
}

pub(crate) fn validate_dir(m: &cap_fs::Metadata, label: &str) -> Result<(), LedgerSnapshotError> {
    if !m.is_dir() || m.file_type().is_symlink() {
        return Err(LedgerSnapshotError::UnsafeIntegrity(format!(
            "ledger parent is not a real directory: {label}"
        )));
    }
    Ok(())
}

pub(crate) fn validate_file(m: &cap_fs::Metadata, label: &str) -> Result<(), AttemptError> {
    if !m.is_file() || m.file_type().is_symlink() {
        return Err(AttemptError::Unsafe(format!(
            "ledger {label} is not a regular nofollow file"
        )));
    }
    if m.len() > MAX_LEAF_BYTES {
        return Err(AttemptError::Unsafe(format!(
            "ledger {label} exceeds bound"
        )));
    }
    validate_file_links(m, label)?;
    Ok(())
}

fn observe(
    parent: &fs::File,
    parent_path: &Path,
    name: &str,
    copy: Option<(&PrivateStorage, &str)>,
) -> Result<Leaf, AttemptError> {
    let before = match cap_fs::stat(parent, Path::new(name), cap_fs::FollowSymlinks::No) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Leaf::Absent),
        Err(e) => return Err(AttemptError::Unstable(format!("stat ledger {name}: {e}"))),
    };
    validate_file(&before, name)?;
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut input = cap_fs::open(parent, Path::new(name), &options)
        .map_err(|e| AttemptError::Unstable(format!("open ledger {name}: {e}")))?;
    let opened = cap_fs::Metadata::from_file(&input)
        .map_err(|e| AttemptError::Unstable(format!("inspect ledger {name}: {e}")))?;
    validate_file(&opened, name)?;
    verify_regular_binding(&parent_path.join(name), &input, name)?;
    if !same_identity(&before, &opened) || before.len() != opened.len() {
        return Err(AttemptError::Unsafe(format!(
            "ledger {name} changed while opening"
        )));
    }
    let mut output = match copy {
        Some((storage, private_name)) => Some(storage.create_file(private_name)?),
        None => None,
    };
    let mut hash = Sha256::new();
    let mut total = 0;
    let mut buf = [0; 65536];
    loop {
        let n = input
            .read(&mut buf)
            .map_err(|e| AttemptError::Unstable(format!("read ledger {name}: {e}")))?;
        if n == 0 {
            break;
        };
        total += n as u64;
        if total > MAX_LEAF_BYTES {
            return Err(AttemptError::Unsafe(format!(
                "ledger {name} exceeds bound while reading"
            )));
        };
        hash.update(&buf[..n]);
        if let Some(out) = output.as_mut() {
            out.write_all(&buf[..n])
                .map_err(|e| AttemptError::Unstable(format!("copy ledger {name}: {e}")))?;
        }
    }
    if let Some(out) = output.as_mut() {
        out.flush()
            .map_err(|e| AttemptError::Unstable(format!("flush ledger snapshot {name}: {e}")))?;
    }
    let after = cap_fs::stat(parent, Path::new(name), cap_fs::FollowSymlinks::No)
        .map_err(|e| AttemptError::Unstable(format!("restat ledger {name}: {e}")))?;
    validate_file(&after, name)?;
    verify_regular_binding(&parent_path.join(name), &input, name)?;
    if !same_identity(&opened, &after) || opened.len() != after.len() || total != opened.len() {
        return Err(AttemptError::Unsafe(format!(
            "ledger {name} changed while reading"
        )));
    };
    Ok(Leaf::Present(Observed {
        identity: identity(&opened),
        bytes: total,
        sha256: hash.finalize().into(),
    }))
}
fn manifest(
    parent: &fs::File,
    parent_path: &Path,
    main: &str,
    storage: Option<&PrivateStorage>,
) -> Result<Manifest, AttemptError> {
    capture_manifest(parent, parent_path, main, storage)
}

fn manifest_recheck(
    parent: &fs::File,
    parent_path: &Path,
    main: &str,
) -> Result<Manifest, AttemptError> {
    capture_manifest(parent, parent_path, main, None)
}

fn capture_manifest(
    parent: &fs::File,
    parent_path: &Path,
    main: &str,
    storage: Option<&PrivateStorage>,
) -> Result<Manifest, AttemptError> {
    let wal = format!("{main}-wal");
    let shm = format!("{main}-shm");
    let m = Manifest {
        main: observe(
            parent,
            parent_path,
            main,
            storage.map(|s| (s, SNAPSHOT_MAIN)),
        )?,
        wal: observe(
            parent,
            parent_path,
            &wal,
            storage.map(|s| (s, "ledger.sqlite-wal")),
        )?,
        shm: observe(parent, parent_path, &shm, None)?,
    };
    let total = [&m.main, &m.wal, &m.shm]
        .iter()
        .filter_map(|x| {
            if let Leaf::Present(o) = x {
                Some(o.bytes)
            } else {
                None
            }
        })
        .sum::<u64>();
    if total > MAX_TOTAL_BYTES {
        return Err(AttemptError::Unsafe(
            "ledger aggregate exceeds bound".into(),
        ));
    };
    match &m.main {
        Leaf::Absent if matches!(m.wal, Leaf::Absent) && matches!(m.shm, Leaf::Absent) => Ok(m),
        Leaf::Absent => Err(AttemptError::Unsafe(
            "ledger sidecar exists without main".into(),
        )),
        _ => Ok(m),
    }
}

fn manifest_is_all_absent(manifest: &Manifest) -> bool {
    matches!(manifest.main, Leaf::Absent)
        && matches!(manifest.wal, Leaf::Absent)
        && matches!(manifest.shm, Leaf::Absent)
}

/// Debug-only synchronization point for the missing-ledger stability gate.
/// The public CLI regression lets a separate process create a real ledger
/// after the first all-absent triad observation and before its confirmation.
/// The ready/release protocol is deliberately capability-bound to an
/// owner-private test directory: it neither overwrites nor follows an
/// environment-supplied pathname. Release builds compile this seam out.
fn maybe_wait_at_test_ledger_absent_recheck() -> Result<(), AttemptError> {
    #[cfg(debug_assertions)]
    let ready_path =
        std::env::var_os("KIO_TEST_LEDGER_SNAPSHOT_ABSENT_BARRIER_READY").map(PathBuf::from);
    #[cfg(not(debug_assertions))]
    let ready_path: Option<PathBuf> = None;
    let Some(ready_path) = ready_path else {
        return Ok(());
    };
    let ready_path = normalized_absolute(&ready_path).map_err(parent_binding_error)?;
    let (parent, ready_name, _) = match bound_parent(&ready_path) {
        Ok(binding) => binding,
        // A malformed or missing test-control path must never become the
        // normal Missing/no-ledger outcome.
        Err(LedgerSnapshotError::Missing) => {
            return Err(AttemptError::Unstable(
                "test ledger barrier parent is missing".into(),
            ));
        }
        Err(error) => return Err(parent_binding_error(error)),
    };
    validate_test_barrier_parent(&parent)?;
    let release_name = Path::new(&ready_name)
        .with_extension("release")
        .into_os_string()
        .into_string()
        .map_err(|_| AttemptError::Unsafe("test ledger barrier name is not UTF-8".into()))?;
    ensure_barrier_leaf_absent(&parent, &ready_name)?;
    ensure_barrier_leaf_absent(&parent, &release_name)?;

    let mut options = cap_fs::OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut ready = cap_fs::open(&parent, Path::new(&ready_name), &options).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            AttemptError::Unsafe("test ledger barrier ready leaf already exists".into())
        } else {
            AttemptError::Unstable(format!("create test ledger barrier ready leaf: {e}"))
        }
    })?;
    ready
        .write_all(b"ready")
        .and_then(|_| ready.sync_all())
        .map_err(|e| {
            AttemptError::Unstable(format!("write test ledger barrier ready leaf: {e}"))
        })?;
    let ready_metadata = cap_fs::Metadata::from_file(&ready).map_err(|e| {
        AttemptError::Unstable(format!("inspect test ledger barrier ready leaf: {e}"))
    })?;
    validate_file(&ready_metadata, &ready_name)?;

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match cap_fs::stat(
            &parent,
            Path::new(&release_name),
            cap_fs::FollowSymlinks::No,
        ) {
            Ok(metadata) => {
                validate_file(&metadata, &release_name)?;
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => {
                return Err(AttemptError::Unstable(format!(
                    "inspect test ledger barrier release leaf: {e}"
                )));
            }
        }
    }
    Err(AttemptError::Unstable(
        "timed out waiting for test ledger barrier release".into(),
    ))
}

fn ensure_barrier_leaf_absent(parent: &fs::File, name: &str) -> Result<(), AttemptError> {
    match cap_fs::stat(parent, Path::new(name), cap_fs::FollowSymlinks::No) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(AttemptError::Unsafe(format!(
            "test ledger barrier leaf already exists: {name}"
        ))),
        Err(e) => Err(AttemptError::Unstable(format!(
            "stat test ledger barrier leaf {name}: {e}"
        ))),
    }
}

fn manifest_identity_replaced(before: &Manifest, after: &Manifest) -> bool {
    fn changed(a: &Leaf, b: &Leaf) -> bool {
        matches!((a,b), (Leaf::Present(a), Leaf::Present(b)) if a.identity != b.identity)
    }
    changed(&before.main, &after.main)
        || changed(&before.wal, &after.wal)
        || changed(&before.shm, &after.shm)
}

fn classify_lifecycle_failure(
    parent: &fs::File,
    parent_path: &Path,
    main: &str,
    initial: &Manifest,
    error: crate::PipelineError,
) -> AttemptError {
    match error {
        crate::PipelineError::Contract { .. }
        | crate::PipelineError::Corrupt { .. }
        | crate::PipelineError::Schema(_) => match manifest_recheck(parent, parent_path, main) {
            Ok(current) if current != *initial => AttemptError::Unstable(
                "ledger changed while validating private lifecycle snapshot".into(),
            ),
            Ok(_) => AttemptError::Unsafe(format!("validate private ledger snapshot: {error}")),
            Err(AttemptError::Unsafe(message)) => AttemptError::Unsafe(message),
            Err(AttemptError::Missing) | Err(AttemptError::Unstable(_)) => AttemptError::Unstable(
                "ledger unavailable while validating private lifecycle snapshot".into(),
            ),
        },
        crate::PipelineError::Sqlite(error) => classify_sqlite_failure(
            parent,
            parent_path,
            main,
            initial,
            error,
            "validate private ledger snapshot",
        ),
        error => AttemptError::Unstable(format!("validate private ledger snapshot: {error}")),
    }
}

fn classify_sqlite_failure(
    parent: &fs::File,
    parent_path: &Path,
    main: &str,
    initial: &Manifest,
    error: rusqlite::Error,
    operation: &str,
) -> AttemptError {
    match manifest_recheck(parent, parent_path, main) {
        Ok(current) if current != *initial => {
            AttemptError::Unstable(format!("ledger changed during {operation}"))
        }
        Err(AttemptError::Unsafe(message)) => AttemptError::Unsafe(message),
        Err(_) => AttemptError::Unstable(format!("ledger unavailable during {operation}")),
        Ok(_) => match error {
            rusqlite::Error::InvalidQuery => {
                AttemptError::Unsafe(format!("{operation}: stable ledger schema mismatch"))
            }
            rusqlite::Error::SqliteFailure(code, _)
                if matches!(
                    code.code,
                    rusqlite::ErrorCode::DatabaseCorrupt
                        | rusqlite::ErrorCode::NotADatabase
                        | rusqlite::ErrorCode::SchemaChanged
                ) =>
            {
                AttemptError::Unsafe(format!("{operation}: {code}"))
            }
            error => AttemptError::Unstable(format!("{operation}: {error}")),
        },
    }
}
fn attempt_snapshot(
    parent: &fs::File,
    parent_path: &Path,
    main: &str,
    #[cfg(test)] hook: Option<&SnapshotHook>,
    #[cfg(not(test))] _hook: Option<&()>,
    #[cfg(test)] attempt: usize,
    #[cfg(not(test))] _attempt: usize,
) -> Result<LedgerReadSnapshot, AttemptError> {
    // Do not allocate private storage before the two-pass no-ledger decision:
    // a genuine no-create miss must be independent of TMPDIR availability.
    // Present sources are observed once without copying, then copied and
    // compared after private storage exists.
    let observed = manifest(parent, parent_path, main, None)?;
    #[cfg(test)]
    if let Some(hook) = hook {
        hook(SnapshotPhase::AfterInitialManifest, attempt, parent_path);
    }
    if manifest_is_all_absent(&observed) {
        maybe_wait_at_test_ledger_absent_recheck()?;
        let fresh_parent = fresh_parent_binding(parent, parent_path, main)?;
        let confirmed = manifest_recheck(&fresh_parent, parent_path, main)?;
        if manifest_is_all_absent(&confirmed) {
            #[cfg(test)]
            if let Some(hook) = hook {
                hook(
                    SnapshotPhase::AfterFinalManifestBeforeParentRecheck,
                    attempt,
                    parent_path,
                );
            }
            let final_parent = fresh_parent_binding(parent, parent_path, main)?;
            #[cfg(test)]
            if let Some(hook) = hook {
                hook(
                    SnapshotPhase::BeforeBoundLifecycleArtifactRead,
                    attempt,
                    parent_path,
                );
            }
            let artifact_state = super::lifecycle::snapshot_artifact_state_from_retained_parent(
                &final_parent,
                parent_path,
                main,
            );
            #[cfg(test)]
            if let Some(hook) = hook {
                hook(
                    SnapshotPhase::AfterBoundLifecycleArtifactRead,
                    attempt,
                    parent_path,
                );
            }
            return match artifact_state {
                Ok(super::lifecycle_fs::ArtifactSetState::Missing) => Err(AttemptError::Missing),
                Ok(super::lifecycle_fs::ArtifactSetState::Complete)
                | Ok(super::lifecycle_fs::ArtifactSetState::Partial { .. }) => {
                    Err(AttemptError::Unsafe(
                        "ledger lifecycle artifacts remain after database disappearance".into(),
                    ))
                }
                Err(error) => Err(classify_absence_lifecycle_failure(error)),
            };
        }
        return Err(AttemptError::Unstable(
            "ledger appeared while confirming an all-absent snapshot".into(),
        ));
    }
    let storage = PrivateStorage::create()?;
    let initial = manifest(parent, parent_path, main, Some(&storage))?;
    if observed != initial {
        return Err(if manifest_identity_replaced(&observed, &initial) {
            AttemptError::Unsafe("ledger changed while creating private snapshot storage".into())
        } else {
            AttemptError::Unstable("ledger changed while creating private snapshot storage".into())
        });
    }
    verify_private_storage(&storage, &initial)?;
    #[cfg(test)]
    if let Some(hook) = hook {
        hook(
            SnapshotPhase::BeforePrivateSqliteOpen,
            attempt,
            storage.path(),
        );
    }
    verify_private_storage(&storage, &initial)?;
    let snapshot = storage.path().join(SNAPSHOT_MAIN);
    let conn = Connection::open_with_flags(&snapshot, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| AttemptError::Unstable(format!("open private ledger snapshot: {e}")))?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|e| AttemptError::Unstable(format!("configure private ledger snapshot: {e}")))?;
    conn.pragma_update(None, "query_only", "ON").map_err(|e| {
        AttemptError::Unstable(format!("make private ledger snapshot query only: {e}"))
    })?;
    // Preserve the canonical parent binding before consulting lifecycle
    // sidecars by pathname. This keeps a detached/replaced parent from being
    // mistaken for a stable authority validation failure.
    let lifecycle_parent = fresh_parent_binding(parent, parent_path, main)?;
    // Bind the copied database to the authority/checkpoint sidecars without
    // reopening the live SQLite source. A missing, partial, or corrupt
    // authority is unsafe/unstable, never the all-absent `Missing` result.
    #[cfg(test)]
    if let Some(hook) = hook {
        hook(
            SnapshotPhase::BeforeBoundLifecycleRead,
            attempt,
            parent_path,
        );
    }
    let lifecycle_validation = super::lifecycle::validate_read_snapshot_from_retained_parent(
        &lifecycle_parent,
        parent_path,
        main,
        &conn,
    );
    #[cfg(test)]
    if let Some(hook) = hook {
        hook(SnapshotPhase::AfterBoundLifecycleRead, attempt, parent_path);
    }
    lifecycle_validation
        .map_err(|error| classify_lifecycle_failure(parent, parent_path, main, &initial, error))?;
    // The real table probe then proves committed WAL visibility before source recheck.
    conn.query_row("SELECT 1 FROM cost_ledger LIMIT 1", [], |r| {
        r.get::<_, i64>(0)
    })
    .or_else(|e| {
        if matches!(e, rusqlite::Error::QueryReturnedNoRows) {
            Ok(0)
        } else {
            Err(e)
        }
    })
    .map_err(|e| {
        classify_sqlite_failure(
            parent,
            parent_path,
            main,
            &initial,
            e,
            "query private ledger snapshot",
        )
    })?;
    #[cfg(test)]
    if let Some(hook) = hook {
        hook(
            SnapshotPhase::AfterProbeBeforeRecheck,
            attempt,
            storage.path(),
        );
    }
    let fresh_parent = fresh_parent_binding(parent, parent_path, main)?;
    let final_manifest = manifest_recheck(&fresh_parent, parent_path, main)?;
    if initial != final_manifest {
        return Err(if manifest_identity_replaced(&initial, &final_manifest) {
            AttemptError::Unsafe("ledger source identity changed while snapshotting".into())
        } else {
            AttemptError::Unstable("ledger main/WAL/SHM changed while snapshotting".into())
        });
    }
    #[cfg(test)]
    if let Some(hook) = hook {
        hook(
            SnapshotPhase::AfterFinalManifestBeforeParentRecheck,
            attempt,
            parent_path,
        );
    }
    fresh_parent_binding(parent, parent_path, main)?;
    Ok(LedgerReadSnapshot {
        conn,
        _storage: storage,
    })
}

fn classify_absence_lifecycle_failure(error: crate::PipelineError) -> AttemptError {
    match error {
        crate::PipelineError::Contract { .. }
        | crate::PipelineError::Corrupt { .. }
        | crate::PipelineError::Schema(_) => AttemptError::Unsafe(format!(
            "inspect absent ledger lifecycle artifacts: {error}"
        )),
        error => AttemptError::Unstable(format!(
            "inspect absent ledger lifecycle artifacts: {error}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::ops_sql::phase1_intent;
    use crate::ledger::{LedgerDb, RequestKind, TaskKey};

    /// Snapshot fixtures initialize through the lifecycle boundary, then use a
    /// separate raw SQLite connection for deliberate source mutations.  The
    /// private snapshot connection never receives that writable connection.
    fn ledger() -> (tempfile::TempDir, PathBuf, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let path = private_test_dir(dir.path(), "device").join("cost-ledger.sqlite");
        initialize_ledger(&path);
        let conn = fixture_connection(&path);
        (dir, path, conn)
    }

    fn fixture_connection(path: &Path) -> Connection {
        #[cfg(not(windows))]
        {
            Connection::open(path).unwrap()
        }
        #[cfg(windows)]
        {
            // Establish private main/WAL/SHM bindings through the production
            // lifecycle boundary before a raw test connection can use them.
            // Keep both SQLite connections alive through the first real table
            // read, so finishing the bound connection cannot remove sidecars
            // that the returned fixture connection will still need.
            let session = super::super::lifecycle_fs::FsSession::acquire(path, false).unwrap();
            let bound = session
                .open_existing_sqlite(OpenFlags::SQLITE_OPEN_READ_WRITE)
                .unwrap();
            let conn =
                Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
            let _: i64 = conn
                .query_row("SELECT count(*) FROM cost_ledger", [], |row| row.get(0))
                .unwrap();
            bound.finish().unwrap();
            // The lifecycle session and its lock end here, before the caller
            // runs a snapshot or deliberately mutates the source fixture.
            conn
        }
    }

    fn private_test_dir(parent: &Path, name: &str) -> PathBuf {
        let path = parent.join(name);
        #[cfg(windows)]
        {
            let parent = kio_core::store_dir::StoreDirectory::open(parent).unwrap();
            drop(parent.create_directory(Path::new(name)).unwrap());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::canonicalize(path).unwrap()
    }

    fn private_test_root(dir: &tempfile::TempDir) -> PathBuf {
        let path = dir.path().join("device");
        if path.exists() {
            fs::canonicalize(path).unwrap()
        } else {
            private_test_dir(dir.path(), "device")
        }
    }

    fn initialize_ledger(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        drop(LedgerDb::initialize(path).unwrap());
    }

    #[cfg(target_os = "linux")]
    fn high_test_descriptor_minimum() -> i32 {
        let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) },
            0
        );
        // SAFETY: getrlimit returned success and initialized the structure.
        let limit = unsafe { limit.assume_init() }.rlim_cur;
        let upper = limit.min(i32::MAX as libc::rlim_t) as i32;
        assert!(
            upper > 32,
            "RLIMIT_NOFILE is too small for descriptor tests"
        );
        upper - 16
    }

    /// The shared test runner may use umask 022, so inherited-descriptor
    /// fixtures must make the descriptor root owner-private explicitly.
    #[cfg(target_os = "linux")]
    fn private_fd_root() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;

        let mut builder = tempfile::Builder::new();
        builder.permissions(fs::Permissions::from_mode(0o700));
        builder.tempdir().unwrap()
    }

    #[derive(Debug, PartialEq, Eq)]
    enum SourceLeaf {
        Absent,
        Present {
            regular: bool,
            symlink: bool,
            readonly: bool,
            bytes: u64,
            sha256: [u8; 32],
            #[cfg(unix)]
            mode: u32,
            #[cfg(unix)]
            nlink: u64,
            #[cfg(unix)]
            dev: u64,
            #[cfg(unix)]
            ino: u64,
        },
    }

    fn source_bytes(path: &Path) -> Vec<(String, SourceLeaf)> {
        [
            "",
            "-wal",
            "-shm",
            ".authority.json",
            ".checkpoint.json",
            ".init.pending",
            ".write.pending.json",
        ]
        .into_iter()
        .map(|suffix| {
            (
                suffix.to_owned(),
                PathBuf::from(format!("{}{}", path.display(), suffix)),
            )
        })
        .chain(std::iter::once((
            "ledger.lifecycle.lock".to_owned(),
            path.parent().unwrap().join("ledger.lifecycle.lock"),
        )))
        .map(|(name, leaf)| {
            let state = match fs::symlink_metadata(&leaf) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => SourceLeaf::Absent,
                Err(error) => panic!("observe {}: {error}", leaf.display()),
                Ok(metadata) => {
                    let bytes = fs::read(&leaf).unwrap();
                    #[cfg(unix)]
                    use std::os::unix::fs::MetadataExt;
                    SourceLeaf::Present {
                        regular: metadata.file_type().is_file(),
                        symlink: metadata.file_type().is_symlink(),
                        readonly: metadata.permissions().readonly(),
                        bytes: metadata.len(),
                        sha256: Sha256::digest(&bytes).into(),
                        #[cfg(unix)]
                        mode: metadata.mode(),
                        #[cfg(unix)]
                        nlink: metadata.nlink(),
                        #[cfg(unix)]
                        dev: metadata.dev(),
                        #[cfg(unix)]
                        ino: metadata.ino(),
                    }
                }
            };
            (name, state)
        })
        .collect()
    }

    #[test]
    fn snapshot_totals_include_costs_and_active_reservations_without_source_writes() {
        let (_dir, path, db) = ledger();
        db.execute(
            "INSERT INTO cost_ledger (scope_id,adapter_kind,input_hash,tool_profile_hash,submission_seq,batch_job_id,usd,estimated,outcome,month,recorded_at)
             VALUES ('scope-a','markdownize','input-a','profile-a',1,'job-a',2.5,0,'succeeded','2026-08',0)", [],
        ).unwrap();
        let key = TaskKey::new("scope-a", "markdownize", "input-b", "profile-a");
        phase1_intent(&db, &key, RequestKind::Batch, 1.25, None).unwrap();
        let before = source_bytes(&path);
        let snapshot = LedgerReadSnapshot::open(&path).unwrap();
        assert_eq!(snapshot.month_total(None, None, "2026-08").unwrap(), 3.75);
        assert_eq!(
            snapshot
                .month_total(Some("scope-a"), None, "2026-08")
                .unwrap(),
            3.75
        );
        assert_eq!(
            before,
            source_bytes(&path),
            "source leaves must be byte-identical"
        );
    }

    #[test]
    fn snapshot_reads_committed_wal_without_copying_shm() {
        let (_dir, path, db) = ledger();
        db.execute_batch("PRAGMA wal_autocheckpoint = 0;").unwrap();
        // Keep a reader transaction and the writer alive: WAL must remain the
        // committed source of truth while the private main+WAL copy is opened.
        let reader = fixture_connection(&path);
        reader
            .execute_batch("BEGIN; SELECT count(*) FROM cost_ledger;")
            .unwrap();
        db.execute(
            "INSERT INTO cost_ledger (scope_id,adapter_kind,input_hash,tool_profile_hash,submission_seq,batch_job_id,usd,estimated,outcome,month,recorded_at)
             VALUES ('scope-w','markdownize','input-w','profile-w',1,'job-w',4.0,0,'succeeded','2026-08',0)", [],
        ).unwrap();
        assert!(path.with_file_name("cost-ledger.sqlite-wal").exists());
        assert!(path.with_file_name("cost-ledger.sqlite-shm").exists());
        let before = source_bytes(&path);
        let snapshot = LedgerReadSnapshot::open(&path).unwrap();
        assert_eq!(snapshot.month_total(None, None, "2026-08").unwrap(), 4.0);
        assert_eq!(before, source_bytes(&path));
        drop(reader);
    }

    #[cfg(windows)]
    #[test]
    fn windows_ledger_path_policy_accepts_absolute_disk_canonical_disk_or_unc_roots() {
        for (accepted, expected_root) in [
            (r"C:\kio\cost-ledger.sqlite", r"C:\"),
            (r"\\?\C:\kio\cost-ledger.sqlite", r"\\?\C:\"),
            (r"\\server\share\kio\cost-ledger.sqlite", r"\\server\share\"),
        ] {
            let normalized = normalized_absolute(Path::new(accepted))
                .unwrap_or_else(|error| panic!("{accepted}: {error}"));
            assert_eq!(normalized.as_os_str(), Path::new(accepted).as_os_str());
            assert_eq!(
                filesystem_root(&normalized).unwrap().as_os_str(),
                Path::new(expected_root).as_os_str(),
                "{accepted}"
            );
        }
        for rejected in [
            r"kio\cost-ledger.sqlite",
            r"\kio\cost-ledger.sqlite",
            r"C:kio\cost-ledger.sqlite",
            r"C:\kio\..\cost-ledger.sqlite",
            r"\\?\C:",
            r"\\?\C:kio\cost-ledger.sqlite",
            r"\\?\C:/kio/cost-ledger.sqlite",
            r"\\?\C:\kio/cost-ledger.sqlite",
            r"\\?\C:\kio\..\cost-ledger.sqlite",
            r"\\?\C:\kio\.\cost-ledger.sqlite",
            r"\\?\UNC\server\share\kio\cost-ledger.sqlite",
            r"\\?\Volume{00000000-0000-0000-0000-000000000000}\kio\cost-ledger.sqlite",
            r"\\.\PhysicalDrive0",
            r"\\.\C:\kio\cost-ledger.sqlite",
        ] {
            assert!(
                normalized_absolute(Path::new(rejected)).is_err(),
                "{rejected}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_canonical_drive_snapshot_reads_wal_without_changing_source_artifacts() {
        let (_dir, path, db) = ledger();
        db.execute_batch("PRAGMA wal_autocheckpoint = 0;").unwrap();
        let reader = fixture_connection(&path);
        reader
            .execute_batch("BEGIN; SELECT count(*) FROM cost_ledger;")
            .unwrap();
        db.execute(
            "INSERT INTO cost_ledger (scope_id,adapter_kind,input_hash,tool_profile_hash,submission_seq,batch_job_id,usd,estimated,outcome,month,recorded_at)
             VALUES ('scope-w','markdownize','input-w','profile-w',1,'job-w',4.0,0,'succeeded','2026-08',0)", [],
        ).unwrap();
        let canonical = fs::canonicalize(&path).unwrap();
        assert!(matches!(
            canonical.components().next(),
            Some(Component::Prefix(prefix))
                if matches!(prefix.kind(), std::path::Prefix::VerbatimDisk(_))
        ));
        assert_eq!(
            normalized_absolute(&canonical).unwrap().as_os_str(),
            canonical.as_os_str(),
            "canonical source path must retain its exact namespace"
        );
        assert!(canonical.with_file_name("cost-ledger.sqlite-wal").exists());
        assert!(canonical.with_file_name("cost-ledger.sqlite-shm").exists());
        let before = source_bytes(&canonical);
        let before_names = fs::read_dir(canonical.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<std::collections::BTreeSet<_>>();

        let snapshot = LedgerReadSnapshot::open(&canonical).unwrap();
        assert_eq!(snapshot.month_total(None, None, "2026-08").unwrap(), 4.0);
        assert!(snapshot.stalled_rows().unwrap().is_empty());
        drop(snapshot);

        assert_eq!(
            source_bytes(&canonical),
            before,
            "main/WAL/SHM and lifecycle authority/checkpoint/intent must remain unchanged"
        );
        let after_names = fs::read_dir(canonical.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(after_names, before_names, "source artifact files changed");
        drop(reader);
    }

    #[test]
    fn missing_ledger_is_a_no_create_miss() {
        let dir = tempfile::tempdir().unwrap();
        let path = private_test_root(&dir).join("missing.sqlite");
        assert!(matches!(
            LedgerReadSnapshot::open(&path),
            Err(LedgerSnapshotError::Missing)
        ));
        assert!(!path.exists());
    }

    #[test]
    fn present_ledger_with_missing_authority_is_unsafe_not_zero() {
        let (_dir, path, _writer) = ledger();
        fs::remove_file(PathBuf::from(format!("{}.authority.json", path.display()))).unwrap();
        assert!(matches!(
            LedgerReadSnapshot::open(&path),
            Err(LedgerSnapshotError::UnsafeIntegrity(_))
        ));
    }

    #[test]
    fn absent_database_with_lifecycle_artifacts_is_unsafe_not_zero() {
        let (_dir, path, writer) = ledger();
        drop(writer);
        fs::remove_file(&path).unwrap();
        assert!(matches!(
            LedgerReadSnapshot::open(&path),
            Err(LedgerSnapshotError::UnsafeIntegrity(_))
        ));
    }

    #[test]
    fn all_absent_miss_never_attempts_private_temp_storage() {
        let dir = tempfile::tempdir().unwrap();
        let path = private_test_root(&dir).join("missing.sqlite");
        let attempted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = attempted.clone();
        PRIVATE_STORAGE_CREATE_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(std::sync::Arc::new(move || {
                observed.store(true, std::sync::atomic::Ordering::SeqCst);
                Err(AttemptError::Unstable(
                    "forced unusable private ledger snapshot storage".into(),
                ))
            }));
        });
        let result = LedgerReadSnapshot::open(&path);
        PRIVATE_STORAGE_CREATE_HOOK.with(|slot| *slot.borrow_mut() = None);
        assert!(matches!(result, Err(LedgerSnapshotError::Missing)));
        assert!(
            !attempted.load(std::sync::atomic::Ordering::SeqCst),
            "a true all-absent miss must not depend on writable temporary storage"
        );
    }

    #[test]
    fn missing_parent_is_a_no_create_miss() {
        let dir = tempfile::tempdir().unwrap();
        let missing_parent = private_test_root(&dir).join("missing-parent");
        let path = missing_parent.join("cost-ledger.sqlite");
        assert!(matches!(
            LedgerReadSnapshot::open(&path),
            Err(LedgerSnapshotError::Missing)
        ));
        assert!(!missing_parent.exists());
    }

    #[test]
    fn missing_parent_appearing_before_confirmation_is_retried_and_observed() {
        let dir = tempfile::tempdir().unwrap();
        let missing_parent = private_test_root(&dir).join("appearing-parent");
        let path = missing_parent.join("cost-ledger.sqlite");
        let source = path.clone();
        MISSING_PARENT_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(std::sync::Arc::new(move || {
                private_test_dir(missing_parent.parent().unwrap(), "appearing-parent");
                initialize_ledger(&source);
                fixture_connection(&source)
                    .execute(
                        "INSERT INTO cost_ledger (
                            scope_id, adapter_kind, input_hash, tool_profile_hash,
                            submission_seq, batch_job_id, usd, estimated, outcome,
                            month, recorded_at
                         ) VALUES ('scope-parent-race', 'embedding', 'input-parent-race',
                            'profile-parent-race', 1, 'job-parent-race', 1.0, 0, 'succeeded',
                            '2026-08', 0)",
                        [],
                    )
                    .unwrap();
            }));
        });
        let hook: SnapshotHook = std::sync::Arc::new(|_, _, _| {});
        let result = LedgerReadSnapshot::open_for_test(&path, 2, hook);
        MISSING_PARENT_HOOK.with(|slot| *slot.borrow_mut() = None);
        let snapshot = result.unwrap();
        assert_eq!(snapshot.month_total(None, None, "2026-08").unwrap(), 1.0);
    }

    #[test]
    fn all_absent_source_appearing_before_confirmation_is_retried() {
        let dir = tempfile::tempdir().unwrap();
        let path = private_test_root(&dir).join("cost-ledger.sqlite");
        let source = path.clone();
        let hook: SnapshotHook = std::sync::Arc::new(move |phase, attempt, _| {
            if attempt == 0 && matches!(phase, SnapshotPhase::AfterInitialManifest) {
                initialize_ledger(&source);
                fixture_connection(&source)
                    .execute(
                        "INSERT INTO cost_ledger (
                            scope_id, adapter_kind, input_hash, tool_profile_hash,
                            submission_seq, batch_job_id, usd, estimated, outcome,
                            month, recorded_at
                         ) VALUES ('scope-race', 'embedding', 'input-race',
                            'profile-race', 1, 'job-race', 1.0, 0, 'succeeded',
                            '2026-08', 0)",
                        [],
                    )
                    .unwrap();
            }
        });
        let snapshot = LedgerReadSnapshot::open_for_test(&path, 2, hook).unwrap();
        assert_eq!(snapshot.month_total(None, None, "2026-08").unwrap(), 1.0);
    }

    #[cfg(unix)]
    #[test]
    fn all_absent_whole_parent_substitution_is_unsafe_not_missing() {
        let dir = tempfile::tempdir().unwrap();
        let canonical_parent = private_test_dir(&private_test_root(&dir), "canonical");
        let path = canonical_parent.join("cost-ledger.sqlite");
        let moved_parent = private_test_root(&dir).join("detached");
        let replacement_parent = canonical_parent.clone();
        let replacement_path = path.clone();
        let hook: SnapshotHook = std::sync::Arc::new(move |phase, attempt, _| {
            if attempt == 0 && matches!(phase, SnapshotPhase::AfterFinalManifestBeforeParentRecheck)
            {
                fs::rename(&replacement_parent, &moved_parent).unwrap();
                fs::create_dir(&replacement_parent).unwrap();
                initialize_ledger(&replacement_path);
            }
        });

        match LedgerReadSnapshot::open_for_test(&path, 1, hook) {
            Err(LedgerSnapshotError::UnsafeIntegrity(message)) => {
                assert!(message.contains("canonical parent identity changed"));
            }
            Err(error) => panic!("expected unsafe parent substitution, got {error}"),
            Ok(_) => panic!("detached all-absent parent must not produce a snapshot"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn present_snapshot_whole_parent_substitution_is_unsafe() {
        let dir = tempfile::tempdir().unwrap();
        let canonical_parent = private_test_dir(&private_test_root(&dir), "canonical");
        let path = canonical_parent.join("cost-ledger.sqlite");
        initialize_ledger(&path);
        let moved_parent = private_test_root(&dir).join("detached");
        let replacement_parent = canonical_parent.clone();
        let replacement_path = path.clone();
        let hook: SnapshotHook = std::sync::Arc::new(move |phase, attempt, _| {
            if attempt == 0 && matches!(phase, SnapshotPhase::AfterFinalManifestBeforeParentRecheck)
            {
                fs::rename(&replacement_parent, &moved_parent).unwrap();
                fs::create_dir(&replacement_parent).unwrap();
                initialize_ledger(&replacement_path);
            }
        });

        match LedgerReadSnapshot::open_for_test(&path, 1, hook) {
            Err(LedgerSnapshotError::UnsafeIntegrity(message)) => {
                assert!(message.contains("canonical parent identity changed"));
            }
            Err(error) => panic!("expected unsafe parent substitution, got {error}"),
            Ok(_) => panic!("detached private snapshot must not be accepted"),
        }
    }

    // Windows retains the canonical parent without delete sharing for the full
    // snapshot attempt. The Unix parent-replacement race is therefore
    // structurally impossible there: prove the sharing boundary while checking
    // the original stable binding retains its true missing semantics.
    #[cfg(windows)]
    #[test]
    fn all_absent_bound_parent_blocks_substitution_and_returns_true_missing() {
        use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

        let dir = tempfile::tempdir().unwrap();
        let canonical_parent = private_test_dir(&private_test_root(&dir), "canonical");
        let path = canonical_parent.join("cost-ledger.sqlite");
        let moved_parent = private_test_root(&dir).join("detached");
        let replacement_parent = canonical_parent.clone();
        let hook_moved_parent = moved_parent.clone();
        let rename_checked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_checked = std::sync::Arc::clone(&rename_checked);
        let hook: SnapshotHook = std::sync::Arc::new(move |phase, attempt, _| {
            if attempt == 0 && matches!(phase, SnapshotPhase::AfterFinalManifestBeforeParentRecheck)
            {
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
            LedgerReadSnapshot::open_for_test(&path, 1, hook),
            Err(LedgerSnapshotError::Missing)
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
        let canonical_parent = private_test_dir(&private_test_root(&dir), "canonical");
        let path = canonical_parent.join("cost-ledger.sqlite");
        initialize_ledger(&path);
        let original = fixture_connection(&path);
        original
            .execute(
                "INSERT INTO cost_ledger (scope_id,adapter_kind,input_hash,tool_profile_hash,submission_seq,batch_job_id,usd,estimated,outcome,month,recorded_at)
                 VALUES ('scope-original','markdownize','input-original','profile-original',1,'job-original',2.5,0,'succeeded','2026-08',0)",
                [],
            )
            .unwrap();
        drop(original);
        let moved_parent = private_test_root(&dir).join("detached");
        let replacement_parent = canonical_parent.clone();
        let hook_moved_parent = moved_parent.clone();
        let rename_checked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_checked = std::sync::Arc::clone(&rename_checked);
        let hook: SnapshotHook = std::sync::Arc::new(move |phase, attempt, _| {
            if attempt == 0 && matches!(phase, SnapshotPhase::AfterFinalManifestBeforeParentRecheck)
            {
                let error = fs::rename(&replacement_parent, &hook_moved_parent)
                    .expect_err("the retained no-delete-share parent must block substitution");
                assert_eq!(error.raw_os_error(), Some(ERROR_SHARING_VIOLATION as i32));
                hook_checked.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });

        let snapshot = LedgerReadSnapshot::open_for_test(&path, 1, hook)
            .expect("the original stable parent and snapshot must remain usable");
        assert_eq!(snapshot.month_total(None, None, "2026-08").unwrap(), 2.5);
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

    #[cfg(unix)]
    #[test]
    fn symlinked_parent_is_unsafe() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = dir.path().join("linked-parent");
        symlink(&target, &link).unwrap();
        let path = link.join("cost-ledger.sqlite");
        assert!(matches!(
            LedgerReadSnapshot::open(&path),
            Err(LedgerSnapshotError::UnsafeIntegrity(_))
        ));
    }

    #[test]
    fn parent_replacement_between_stat_and_open_is_unsafe() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("nested");
        fs::create_dir(&nested).unwrap();
        let path = nested.join("cost-ledger.sqlite");
        let moved = dir.path().join("moved");
        let final_index = normalized_absolute(&path)
            .unwrap()
            .parent()
            .unwrap()
            .components()
            .filter(|component| matches!(component, Component::Normal(_)))
            .count()
            - 1;
        PARENT_COMPONENT_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(std::sync::Arc::new(move |index| {
                if index == final_index {
                    fs::rename(&nested, &moved).unwrap();
                    fs::create_dir(&nested).unwrap();
                }
            }));
        });
        let result = LedgerReadSnapshot::open(&path);
        PARENT_COMPONENT_HOOK.with(|slot| *slot.borrow_mut() = None);
        assert!(matches!(
            result,
            Err(LedgerSnapshotError::UnsafeIntegrity(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn linked_source_leaf_is_unsafe() {
        use std::fs::hard_link;
        let (dir, path, _db) = ledger();
        hard_link(&path, dir.path().join("alias.sqlite")).unwrap();
        assert!(matches!(
            LedgerReadSnapshot::open(&path),
            Err(LedgerSnapshotError::UnsafeIntegrity(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_source_sidecar_is_unsafe() {
        use std::os::unix::fs::symlink;
        let (dir, path, _db) = ledger();
        let target = dir.path().join("sidecar-target");
        fs::write(&target, b"x").unwrap();
        let wal = PathBuf::from(format!("{}-wal", path.display()));
        let _ = fs::remove_file(&wal);
        symlink(&target, wal).unwrap();
        assert!(matches!(
            LedgerReadSnapshot::open(&path),
            Err(LedgerSnapshotError::UnsafeIntegrity(_))
        ));
    }

    #[test]
    fn oversized_source_leaf_is_unsafe_before_reading() {
        let (dir, path, db) = ledger();
        drop(db);
        let oversized = dir.path().join("oversized-wal");
        let wal = PathBuf::from(format!("{}-wal", path.display()));
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&oversized)
            .unwrap();
        file.set_len(MAX_LEAF_BYTES + 1).unwrap();
        drop(file);
        if wal.exists() {
            fs::remove_file(&wal).unwrap();
        }
        fs::rename(oversized, &wal).unwrap();
        match LedgerReadSnapshot::open(&path) {
            Err(LedgerSnapshotError::UnsafeIntegrity(message)) => {
                assert!(message.contains("ledger cost-ledger.sqlite-wal exceeds bound"));
            }
            Err(error) => panic!("expected size-bound integrity failure, got {error}"),
            Ok(_) => panic!("oversized WAL must not produce a snapshot"),
        }
    }

    #[test]
    fn source_sidecar_appearing_after_initial_manifest_is_retryable() {
        let (_dir, path, db) = ledger();
        drop(db);
        let source = path.clone();
        let hook: SnapshotHook = std::sync::Arc::new(move |phase, _, _| {
            if matches!(phase, SnapshotPhase::AfterProbeBeforeRecheck) {
                fs::write(format!("{}-wal", source.display()), b"churn").unwrap();
            }
        });
        assert!(matches!(
            LedgerReadSnapshot::open_for_test(&path, 1, hook),
            Err(LedgerSnapshotError::UnstableBusy(_))
        ));
    }

    #[test]
    fn unexpected_private_leaf_before_sqlite_is_integrity_failure() {
        let (_dir, path, _db) = ledger();
        let hook: SnapshotHook = std::sync::Arc::new(move |phase, _, private| {
            if matches!(phase, SnapshotPhase::BeforePrivateSqliteOpen) {
                fs::write(private.join("injected"), b"not sqlite").unwrap();
            }
        });
        assert!(matches!(
            LedgerReadSnapshot::open_for_test(&path, 1, hook),
            Err(LedgerSnapshotError::UnsafeIntegrity(_))
        ));
    }

    #[test]
    fn source_content_drift_after_probe_is_retryable() {
        let (_dir, path, _db) = ledger();
        let source = path.clone();
        let hook: SnapshotHook = std::sync::Arc::new(move |phase, _, _| {
            if matches!(phase, SnapshotPhase::AfterProbeBeforeRecheck) {
                fs::write(&source, b"tampered").unwrap();
            }
        });
        assert!(matches!(
            LedgerReadSnapshot::open_for_test(&path, 1, hook),
            Err(LedgerSnapshotError::UnstableBusy(_))
        ));
    }

    #[test]
    fn source_absent_after_probe_is_retryable_not_missing() {
        let (_dir, path, db) = ledger();
        drop(db);
        let source = path.clone();
        let hook: SnapshotHook = std::sync::Arc::new(move |phase, _, _| {
            if matches!(phase, SnapshotPhase::AfterProbeBeforeRecheck) {
                for suffix in ["", "-wal", "-shm"] {
                    let _ = fs::remove_file(format!("{}{}", source.display(), suffix));
                }
            }
        });
        assert!(matches!(
            LedgerReadSnapshot::open_for_test(&path, 1, hook),
            Err(LedgerSnapshotError::UnstableBusy(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn source_identity_replacement_after_probe_is_unsafe() {
        let (dir, path, _db) = ledger();
        let source = path.clone();
        let replacement = dir.path().join("replacement.sqlite");
        let hook: SnapshotHook = std::sync::Arc::new(move |phase, _, _| {
            if matches!(phase, SnapshotPhase::AfterProbeBeforeRecheck) {
                fs::copy(&source, &replacement).unwrap();
                fs::rename(&replacement, &source).unwrap();
            }
        });
        assert!(matches!(
            LedgerReadSnapshot::open_for_test(&path, 1, hook),
            Err(LedgerSnapshotError::UnsafeIntegrity(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn live_writer_blocks_source_replacement_and_preserves_original_snapshot() {
        use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

        let (dir, path, writer) = ledger();
        writer
            .execute(
                "INSERT INTO cost_ledger (scope_id,adapter_kind,input_hash,tool_profile_hash,submission_seq,batch_job_id,usd,estimated,outcome,month,recorded_at)
                 VALUES ('scope-original','markdownize','input-original','profile-original',1,'job-original',4.0,0,'succeeded','2026-08',0)",
                [],
            )
            .unwrap();
        let original_source = source_bytes(&path);
        let original_identity = kio_core::platform::real_regular_file_identity(&path)
            .unwrap()
            .expect("the source must be a real regular file");
        let moved = dir.path().join("moved.sqlite");
        let source = path.clone();
        let hook_moved = moved.clone();
        let replacement_checked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_checked = std::sync::Arc::clone(&replacement_checked);
        let hook: SnapshotHook = std::sync::Arc::new(move |phase, _, _| {
            if matches!(phase, SnapshotPhase::AfterProbeBeforeRecheck) {
                let error = fs::rename(&source, &hook_moved)
                    .expect_err("the live writer handle must block replacement on Windows");
                assert_eq!(error.raw_os_error(), Some(ERROR_SHARING_VIOLATION as i32));
                hook_checked.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });

        let snapshot = LedgerReadSnapshot::open_for_test(&path, 1, hook)
            .expect("the unchanged original snapshot must remain usable");
        assert_eq!(snapshot.month_total(None, None, "2026-08").unwrap(), 4.0);
        assert!(
            replacement_checked.load(std::sync::atomic::Ordering::SeqCst),
            "the test hook must prove the live-source sharing boundary"
        );
        assert_eq!(source_bytes(&path), original_source);
        assert_eq!(
            kio_core::platform::real_regular_file_identity(&path).unwrap(),
            Some(original_identity)
        );
        assert!(path.exists(), "the original source must remain live");
        assert!(
            !moved.exists(),
            "the denied replacement must not create a moved source"
        );
        drop(writer);
    }

    #[test]
    fn repeated_valid_source_drift_exhausts_test_attempt_budget() {
        let (_dir, path, _db) = ledger();
        let source = path.clone();
        let hook: SnapshotHook = std::sync::Arc::new(move |phase, attempt, _| {
            if matches!(phase, SnapshotPhase::AfterProbeBeforeRecheck) {
                fixture_connection(&source)
                    .execute_batch(&format!("PRAGMA user_version = {};", attempt + 10))
                    .unwrap();
            }
        });
        assert!(matches!(
            LedgerReadSnapshot::open_for_test(&path, 2, hook),
            Err(LedgerSnapshotError::UnstableBusy(_))
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inherited_directory_descriptor_is_a_capability_relative_ledger_parent() {
        use std::os::fd::AsRawFd;

        let directory = private_fd_root();
        let retained_root = fs::File::open(directory.path()).unwrap();
        let actual = private_test_dir(directory.path(), "kio").join("cost-ledger.sqlite");
        initialize_ledger(&actual);
        let db = Connection::open(&actual).unwrap();
        db
            .execute(
                "INSERT INTO cost_ledger (scope_id,adapter_kind,input_hash,tool_profile_hash,submission_seq,batch_job_id,usd,estimated,outcome,month,recorded_at) VALUES ('descriptor','embedding','input','profile',1,'job',2.0,0,'succeeded','2026-08',0)",
                [],
            )
            .unwrap();
        let inherited = PathBuf::from(format!(
            "/dev/fd/{}/kio/cost-ledger.sqlite",
            retained_root.as_raw_fd()
        ));

        let snapshot = LedgerReadSnapshot::open(&inherited).unwrap();
        assert_eq!(snapshot.month_total(None, None, "2026-08").unwrap(), 2.0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inherited_snapshot_keeps_lifecycle_reads_on_the_bound_parent_after_fd_swap() {
        use std::os::fd::AsRawFd;

        let root_a = private_fd_root();
        let root_b = private_fd_root();
        let actual = private_test_dir(root_a.path(), "kio").join("cost-ledger.sqlite");
        initialize_ledger(&actual);
        let held_a = fs::File::open(root_a.path()).unwrap();
        let held_b = fs::File::open(root_b.path()).unwrap();
        let restore_a = fs::File::open(root_a.path()).unwrap();
        let descriptor = unsafe {
            libc::fcntl(
                held_a.as_raw_fd(),
                libc::F_DUPFD_CLOEXEC,
                high_test_descriptor_minimum(),
            )
        };
        assert!(descriptor >= high_test_descriptor_minimum());
        let inherited = PathBuf::from(format!("/dev/fd/{descriptor}/kio/cost-ledger.sqlite"));
        let swapped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let restored = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_b = held_b;
        let hook_a = held_a;
        let hook_swapped = std::sync::Arc::clone(&swapped);
        let hook_restored = std::sync::Arc::clone(&restored);
        let hook: SnapshotHook = std::sync::Arc::new(move |phase, _, _| match phase {
            SnapshotPhase::BeforeBoundLifecycleRead => {
                assert_eq!(
                    unsafe { libc::dup2(hook_b.as_raw_fd(), descriptor) },
                    descriptor
                );
                hook_swapped.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            SnapshotPhase::AfterBoundLifecycleRead => {
                assert_eq!(
                    unsafe { libc::dup2(hook_a.as_raw_fd(), descriptor) },
                    descriptor
                );
                hook_restored.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            _ => {}
        });

        let snapshot = LedgerReadSnapshot::open_for_test(&inherited, 1, hook)
            .expect("lifecycle authority must remain bound to root A");
        assert_eq!(snapshot.month_total(None, None, "2026-08").unwrap(), 0.0);
        assert!(swapped.load(std::sync::atomic::Ordering::SeqCst));
        assert!(restored.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!root_b.path().join("kio").exists());
        assert_eq!(
            unsafe { libc::dup2(restore_a.as_raw_fd(), descriptor) },
            descriptor
        );
        assert_eq!(unsafe { libc::close(descriptor) }, 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inherited_all_absent_snapshot_checks_retained_lifecycle_artifacts_after_fd_swap() {
        use std::os::fd::AsRawFd;

        let root_a = private_fd_root();
        let root_b = private_fd_root();
        let actual = private_test_dir(root_a.path(), "kio").join("cost-ledger.sqlite");
        initialize_ledger(&actual);
        for suffix in ["", "-wal", "-shm"] {
            let candidate = PathBuf::from(format!("{}{}", actual.display(), suffix));
            if candidate.exists() {
                fs::remove_file(candidate).unwrap();
            }
        }
        assert!(actual.with_extension("sqlite.authority.json").exists());
        assert!(actual.with_extension("sqlite.checkpoint.json").exists());

        let held_a = fs::File::open(root_a.path()).unwrap();
        let held_b = fs::File::open(root_b.path()).unwrap();
        let restore_a = fs::File::open(root_a.path()).unwrap();
        let descriptor = unsafe {
            libc::fcntl(
                held_a.as_raw_fd(),
                libc::F_DUPFD_CLOEXEC,
                high_test_descriptor_minimum(),
            )
        };
        assert!(descriptor >= high_test_descriptor_minimum());
        let inherited = PathBuf::from(format!("/dev/fd/{descriptor}/kio/cost-ledger.sqlite"));
        let hook_b = held_b;
        let hook_a = held_a;
        let hook: SnapshotHook = std::sync::Arc::new(move |phase, _, _| match phase {
            SnapshotPhase::BeforeBoundLifecycleArtifactRead => {
                assert_eq!(
                    unsafe { libc::dup2(hook_b.as_raw_fd(), descriptor) },
                    descriptor
                );
            }
            SnapshotPhase::AfterBoundLifecycleArtifactRead => {
                assert_eq!(
                    unsafe { libc::dup2(hook_a.as_raw_fd(), descriptor) },
                    descriptor
                );
            }
            _ => {}
        });

        let result = LedgerReadSnapshot::open_for_test(&inherited, 1, hook);
        assert!(matches!(
            result,
            Err(LedgerSnapshotError::UnsafeIntegrity(message))
                if message.contains("lifecycle artifacts remain")
        ));
        assert!(!root_b.path().join("kio").exists());
        assert_eq!(
            unsafe { libc::dup2(restore_a.as_raw_fd(), descriptor) },
            descriptor
        );
        assert_eq!(unsafe { libc::close(descriptor) }, 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inherited_ledger_descriptor_rejects_aliases_closed_and_non_directory_fds() {
        use std::os::fd::AsRawFd;

        let directory = private_fd_root();
        let retained_root = fs::File::open(directory.path()).unwrap();
        let fd = retained_root.as_raw_fd();
        let aliases = [
            format!("/dev/fd/{fd}"),
            "/dev/fd/not-a-number/kio/cost-ledger.sqlite".to_owned(),
            "/dev/fd/-1/kio/cost-ledger.sqlite".to_owned(),
            "/dev/fd/2147483648/kio/cost-ledger.sqlite".to_owned(),
            "/dev/fd/999999999999999999999999/kio/cost-ledger.sqlite".to_owned(),
            format!("/dev/fd/0{fd}/kio/cost-ledger.sqlite"),
            format!("/dev/fd//{fd}/kio/cost-ledger.sqlite"),
            format!("/dev/fd/{fd}//kio/cost-ledger.sqlite"),
            format!("/dev/fd/{fd}/./kio/cost-ledger.sqlite"),
            format!("/dev/fd/{fd}/kio/../other/cost-ledger.sqlite"),
            format!("/dev/fd/{fd}/kio/cost-ledger.sqlite/"),
            format!("/dev//fd/{fd}/kio/cost-ledger.sqlite"),
        ];
        for alias in aliases {
            assert!(matches!(
                LedgerReadSnapshot::open(Path::new(&alias)),
                Err(LedgerSnapshotError::UnsafeIntegrity(_))
            ));
        }

        let file_path = directory.path().join("not-a-directory");
        fs::write(&file_path, b"regular").unwrap();
        let retained_file = fs::File::open(&file_path).unwrap();
        let non_directory = PathBuf::from(format!(
            "/dev/fd/{}/kio/cost-ledger.sqlite",
            retained_file.as_raw_fd()
        ));
        assert!(matches!(
            LedgerReadSnapshot::open(&non_directory),
            Err(LedgerSnapshotError::UnsafeIntegrity(_))
        ));
        drop(retained_file);

        let proc_alias = PathBuf::from(format!("/proc/self/fd/{fd}/kio/cost-ledger.sqlite"));
        assert!(matches!(
            LedgerReadSnapshot::open(&proc_alias),
            Err(LedgerSnapshotError::UnsafeIntegrity(_))
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inherited_closed_descriptor_is_rejected_in_isolated_process() {
        const CHILD: &str = "KIO_TEST_LEDGER_SNAPSHOT_CLOSED_FD_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "ledger::snapshot::tests::inherited_closed_descriptor_is_rejected_in_isolated_process",
                    "--test-threads=1",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        use std::os::fd::AsRawFd;

        let root = private_fd_root();
        let held = fs::File::open(root.path()).unwrap();
        let closed_fd = unsafe {
            libc::fcntl(
                held.as_raw_fd(),
                libc::F_DUPFD_CLOEXEC,
                high_test_descriptor_minimum(),
            )
        };
        assert!(closed_fd >= high_test_descriptor_minimum());
        assert_eq!(unsafe { libc::close(closed_fd) }, 0);
        let closed_path = PathBuf::from(format!("/dev/fd/{closed_fd}/cost-ledger.sqlite"));
        assert!(matches!(
            LedgerReadSnapshot::open(&closed_path),
            Err(LedgerSnapshotError::UnsafeIntegrity(_))
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inherited_missing_cannot_join_absence_across_fd_root_substitution() {
        use std::os::fd::AsRawFd;

        let root_a = private_fd_root();
        let root_b = private_fd_root();
        let held_a = fs::File::open(root_a.path()).unwrap();
        let held_b = fs::File::open(root_b.path()).unwrap();
        let descriptor = unsafe {
            libc::fcntl(
                held_a.as_raw_fd(),
                libc::F_DUPFD_CLOEXEC,
                high_test_descriptor_minimum(),
            )
        };
        assert!(descriptor >= high_test_descriptor_minimum());
        let path = PathBuf::from(format!("/dev/fd/{descriptor}/kio/cost-ledger.sqlite"));
        let swapped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let swapped_once = swapped.clone();
        MISSING_PARENT_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(std::sync::Arc::new(move || {
                if !swapped_once.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    assert_eq!(
                        unsafe { libc::dup2(held_b.as_raw_fd(), descriptor) },
                        descriptor
                    );
                }
            }));
        });
        let result = LedgerReadSnapshot::open(&path);
        MISSING_PARENT_HOOK.with(|slot| *slot.borrow_mut() = None);
        assert_eq!(unsafe { libc::close(descriptor) }, 0);
        assert!(swapped.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            matches!(result, Err(LedgerSnapshotError::UnsafeIntegrity(message)) if message.contains("root identity changed"))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inherited_missing_rechecks_fd_root_after_final_absence_walk() {
        use std::os::fd::AsRawFd;

        let root_a = private_fd_root();
        let root_b = private_fd_root();
        let held_a = fs::File::open(root_a.path()).unwrap();
        let held_b = fs::File::open(root_b.path()).unwrap();
        let descriptor = unsafe {
            libc::fcntl(
                held_a.as_raw_fd(),
                libc::F_DUPFD_CLOEXEC,
                high_test_descriptor_minimum(),
            )
        };
        assert!(descriptor >= high_test_descriptor_minimum());
        let path = PathBuf::from(format!("/dev/fd/{descriptor}/kio/cost-ledger.sqlite"));
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_in_hook = calls.clone();
        MISSING_PARENT_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(std::sync::Arc::new(move || {
                if calls_in_hook.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 2 {
                    assert_eq!(
                        unsafe { libc::dup2(held_b.as_raw_fd(), descriptor) },
                        descriptor
                    );
                }
            }));
        });
        let result = LedgerReadSnapshot::open(&path);
        MISSING_PARENT_HOOK.with(|slot| *slot.borrow_mut() = None);
        assert_eq!(unsafe { libc::close(descriptor) }, 0);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert!(
            matches!(result, Err(LedgerSnapshotError::UnsafeIntegrity(message)) if message.contains("after final absence walk"))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inherited_ledger_descriptor_rejects_symlink_suffix_without_accepting_victim() {
        use std::{os::fd::AsRawFd, os::unix::fs::symlink};

        let directory = private_fd_root();
        let victim = private_fd_root();
        let retained_root = fs::File::open(directory.path()).unwrap();
        let child = directory.path().join("kio");
        symlink(victim.path(), &child).unwrap();
        let inherited = PathBuf::from(format!(
            "/dev/fd/{}/kio/cost-ledger.sqlite",
            retained_root.as_raw_fd()
        ));

        assert!(matches!(
            LedgerReadSnapshot::open(&inherited),
            Err(LedgerSnapshotError::UnsafeIntegrity(_))
        ));
        assert!(!victim.path().join("cost-ledger.sqlite").exists());
    }

    #[test]
    fn stable_schema_mismatch_is_unsafe_not_retryable() {
        let (_dir, path, db) = ledger();
        db.execute_batch("DROP INDEX idx_cost_ledger_month;")
            .unwrap();
        assert!(matches!(
            LedgerReadSnapshot::open(&path),
            Err(LedgerSnapshotError::UnsafeIntegrity(_))
        ));
    }

    #[test]
    fn stable_missing_cost_table_is_unsafe_before_probe() {
        let (_dir, path, db) = ledger();
        db.execute_batch("DROP TABLE cost_ledger;").unwrap();
        assert!(matches!(
            LedgerReadSnapshot::open(&path),
            Err(LedgerSnapshotError::UnsafeIntegrity(_))
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_var_alias_is_normalized_once() {
        assert_eq!(
            normalized_absolute(Path::new("/var/tmp/cost-ledger.sqlite")).unwrap(),
            PathBuf::from("/private/var/tmp/cost-ledger.sqlite")
        );
        assert_eq!(
            normalized_absolute(Path::new("/private/var/tmp/cost-ledger.sqlite")).unwrap(),
            PathBuf::from("/private/var/tmp/cost-ledger.sqlite")
        );
    }
}
