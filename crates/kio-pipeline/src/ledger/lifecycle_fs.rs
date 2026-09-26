//! Retained filesystem capability for the device ledger lifecycle.
//!
//! SQLite cannot be opened portably relative to a retained directory handle.
//! This module therefore keeps the device-private parent and every existing
//! SQLite leaf pinned, checks the public parent/leaf bindings before and after
//! opening the *same* connection returned to the caller, and fails closed on a
//! changed binding.  An attacker already acting as the same OS account can
//! still race the pathname in the small interval around SQLite's path-based
//! open; that attacker is outside Kio's filesystem threat boundary.  Other OS
//! users cannot reach the owner-private parent, and reparse/symlink/hardlink
//! leaves are rejected.

use std::{
    fs::{File, TryLockError},
    path::{Component, Path, PathBuf},
};

use kio_core::store_dir::{Publication, StoreDirectory};
use rusqlite::{Connection, OpenFlags};

use super::schema::RETIRED_LEDGER_BASENAMES;
use crate::{PipelineError, Result};

pub(crate) const LIFECYCLE_LOCK: &str = "ledger.lifecycle.lock";
const LOCK_BYTES: &[u8] = b"kio-ledger-lifecycle-lock-v1\n";
const MAX_LOCK_BYTES: u64 = 128;

/// Direct children of the retained device-private ledger parent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ArtifactNames {
    pub(crate) db: PathBuf,
    pub(crate) wal: PathBuf,
    pub(crate) shm: PathBuf,
    pub(crate) authority: PathBuf,
    pub(crate) checkpoint: PathBuf,
    pub(crate) init_pending: PathBuf,
    pub(crate) restore_pending: PathBuf,
    pub(crate) write_pending: PathBuf,
    pub(crate) backup_proof: PathBuf,
}

impl ArtifactNames {
    pub(crate) fn from_db_path(path: &Path) -> Result<(PathBuf, Self)> {
        if !path.is_absolute() {
            return Err(contract(
                "KIO-E-LEDGER-PATH-001",
                "ledger path must be absolute",
            ));
        }
        if path.components().any(|component| {
            matches!(component, Component::CurDir | Component::ParentDir)
                || (matches!(component, Component::Prefix(_)) && !cfg!(windows))
        }) {
            return Err(contract(
                "KIO-E-LEDGER-PATH-001",
                "ledger path contains an unsafe component",
            ));
        }
        let parent = path.parent().ok_or_else(|| {
            contract(
                "KIO-E-LEDGER-PATH-001",
                "ledger path has no parent directory",
            )
        })?;
        let db = path
            .file_name()
            .ok_or_else(|| contract("KIO-E-LEDGER-PATH-001", "ledger path has no file name"))?;
        if db.is_empty() {
            return Err(contract(
                "KIO-E-LEDGER-PATH-001",
                "ledger path has an empty file name",
            ));
        }
        let append = |suffix: &str| {
            let mut value = db.to_owned();
            value.push(suffix);
            PathBuf::from(value)
        };
        Ok((
            parent.to_path_buf(),
            Self {
                db: PathBuf::from(db),
                wal: append("-wal"),
                shm: append("-shm"),
                authority: append(".authority.json"),
                checkpoint: append(".checkpoint.json"),
                init_pending: append(".init.pending"),
                restore_pending: append(".restore.pending"),
                write_pending: append(".write.pending.json"),
                backup_proof: append(".backup.proof.json"),
            },
        ))
    }

    fn classified(&self) -> [&Path; 8] {
        [
            &self.db,
            &self.wal,
            &self.shm,
            &self.authority,
            &self.checkpoint,
            &self.init_pending,
            &self.restore_pending,
            &self.write_pending,
        ]
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ArtifactSetState {
    Missing,
    Complete,
    Partial { present: Vec<PathBuf> },
}

/// Exclusive lifecycle session rooted in a retained owner-private directory.
pub(crate) struct FsSession {
    parent_path: PathBuf,
    db_path: PathBuf,
    names: ArtifactNames,
    parent: StoreDirectory,
    inherited_parent: bool,
    parent_identity: DirectoryIdentity,
    lock: File,
    lock_identity: RegularFileIdentity,
}

impl std::fmt::Debug for FsSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FsSession")
            .field("parent_path", &self.parent_path)
            .field("db_path", &self.db_path)
            .field("names", &self.names)
            .finish_non_exhaustive()
    }
}

impl FsSession {
    /// Acquire the stable device lifecycle lock. `allow_create` is reserved for
    /// explicit ledger initialization; ordinary opens never create a parent or
    /// lock and never repair an unsafe existing object.
    pub(crate) fn acquire(db_path: &Path, allow_create: bool) -> Result<Self> {
        let db_path = normalized_absolute_db_path(db_path)?;
        let (parent_path, names) = ArtifactNames::from_db_path(&db_path)?;
        let (parent, inherited_parent) = open_or_create_parent(&parent_path, allow_create)?;
        let sqlite_path = if inherited_parent {
            sqlite_path_from_retained_parent(&parent, &names.db)?
        } else {
            db_path
        };
        Self::acquire_bound_parent(
            parent_path,
            sqlite_path,
            names,
            parent,
            inherited_parent,
            allow_create,
        )
    }

    /// Acquire the lifecycle lock from a parent the snapshot reader has already
    /// bound through no-follow traversal. This path is read-only validation
    /// only: it never recreates a missing parent or reopens its diagnostic path.
    pub(crate) fn acquire_retained_snapshot_parent(
        parent: &File,
        parent_path: &Path,
        db_leaf: &str,
    ) -> Result<Self> {
        let db_path = parent_path.join(db_leaf);
        let (_, names) = ArtifactNames::from_db_path(&db_path)?;
        if names.db != Path::new(db_leaf) {
            return Err(contract(
                "KIO-E-LEDGER-PATH-001",
                "retained snapshot ledger leaf is not a direct file name",
            ));
        }
        let retained = parent.try_clone().map_err(|error| PipelineError::Io {
            path: parent_path.display().to_string(),
            message: error.to_string(),
        })?;
        let parent = StoreDirectory::from_retained(retained, parent_path.to_path_buf())
            .map_err(core_private)?;
        Self::acquire_bound_parent(
            parent_path.to_path_buf(),
            db_path,
            names,
            parent,
            // Never re-open a diagnostic path held by the snapshot reader.
            true,
            false,
        )
    }

    fn acquire_bound_parent(
        parent_path: PathBuf,
        db_path: PathBuf,
        names: ArtifactNames,
        parent: StoreDirectory,
        inherited_parent: bool,
        allow_create: bool,
    ) -> Result<Self> {
        verify_private_parent_handle(parent.root_handle().as_ref(), &parent_path)?;
        let parent_identity = directory_identity(parent.root_handle().as_ref(), &parent_path)?;
        let lock_leaf = Path::new(LIFECYCLE_LOCK);

        reject_retired_names(&parent)?;

        if !parent.contains_entry(lock_leaf).map_err(core_private)? {
            if !allow_create {
                let any_artifact = names.classified().into_iter().try_fold(
                    false,
                    |present, leaf| -> Result<bool> {
                        Ok(present || parent.contains_entry(leaf).map_err(core_private)?)
                    },
                )?;
                return if any_artifact {
                    Err(contract(
                        "KIO-E-LEDGER-INIT-PARTIAL-001",
                        "ledger lifecycle lock is missing from a partial artifact set",
                    ))
                } else {
                    Err(contract(
                        "KIO-E-LEDGER-UNINITIALIZED-001",
                        "ledger is not initialized",
                    ))
                };
            }
            parent
                .write_atomic(lock_leaf, LOCK_BYTES, Publication::CreateOnly)
                .map_err(core_private)?;
        }
        parent
            .ensure_owner_private(lock_leaf)
            .map_err(core_private)?;
        let absolute_lock = parent_path.join(lock_leaf);
        let observed =
            kio_core::private_fs::read_private_file_at(&parent, LIFECYCLE_LOCK, MAX_LOCK_BYTES)
                .map_err(core_private)?;
        if observed != LOCK_BYTES {
            return Err(contract(
                "KIO-E-LEDGER-PRIVATE-001",
                "ledger lifecycle lock has unexpected contents",
            ));
        }
        let lock = parent
            .open_regular_read(lock_leaf, MAX_LOCK_BYTES)
            .map_err(core_private)?;
        let lock_identity = regular_file_identity(&lock, &absolute_lock)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(PipelineError::locked(absolute_lock.display().to_string()));
            }
            Err(TryLockError::Error(error)) => {
                return Err(PipelineError::Io {
                    path: absolute_lock.display().to_string(),
                    message: error.to_string(),
                });
            }
        }

        let session = Self {
            parent_path,
            db_path,
            names,
            parent,
            inherited_parent,
            parent_identity,
            lock,
            lock_identity,
        };
        session.revalidate_boundary()?;
        Ok(session)
    }

    #[must_use]
    pub(crate) fn names(&self) -> &ArtifactNames {
        &self.names
    }

    pub(crate) fn artifact_state(&self) -> Result<ArtifactSetState> {
        let mut present = Vec::new();
        for leaf in self.names.classified() {
            if self.exists_safe(leaf)? {
                present.push(leaf.to_path_buf());
            }
        }
        if present.is_empty() {
            return Ok(ArtifactSetState::Missing);
        }
        let required = [
            self.names.db.as_path(),
            self.names.authority.as_path(),
            self.names.checkpoint.as_path(),
        ];
        let complete = required
            .iter()
            .all(|leaf| present.iter().any(|p| p == leaf))
            && !present.iter().any(|p| p == &self.names.init_pending)
            && !present.iter().any(|p| p == &self.names.restore_pending)
            && !present.iter().any(|p| p == &self.names.write_pending);
        if complete {
            Ok(ArtifactSetState::Complete)
        } else {
            Ok(ArtifactSetState::Partial { present })
        }
    }

    pub(crate) fn contains_leaf(&self, leaf: &Path) -> Result<bool> {
        self.exists_safe(leaf)
    }

    pub(crate) fn exists_safe(&self, leaf: &Path) -> Result<bool> {
        require_leaf(leaf)?;
        self.revalidate_boundary()?;
        if !self.parent.contains_entry(leaf).map_err(core_private)? {
            self.revalidate_boundary()?;
            return Ok(false);
        }
        self.parent
            .ensure_owner_private(leaf)
            .map_err(core_private)?;
        let retained = self
            .parent
            .open_regular_read(leaf, u64::MAX)
            .map_err(core_private)?;
        let _ = regular_file_identity(&retained, &self.parent_path.join(leaf))?;
        self.revalidate_boundary()?;
        Ok(true)
    }

    pub(crate) fn read_private(&self, leaf: &Path, max_bytes: u64) -> Result<Vec<u8>> {
        require_leaf(leaf)?;
        self.revalidate_boundary()?;
        if !self.parent.contains_entry(leaf).map_err(core_private)? {
            self.revalidate_boundary()?;
            return Err(contract(
                "KIO-E-LEDGER-MISSING-001",
                format!("ledger artifact is missing: {}", leaf.display()),
            ));
        }
        self.parent
            .ensure_owner_private(leaf)
            .map_err(core_private)?;
        let bytes = self
            .parent
            .read_optional(leaf, max_bytes)
            .map_err(core_private)?
            .ok_or_else(|| {
                boundary_changed(format!(
                    "ledger artifact disappeared while being read: {}",
                    leaf.display()
                ))
            })?;
        self.revalidate_boundary()?;
        Ok(bytes)
    }

    pub(crate) fn read_optional_private(
        &self,
        leaf: &Path,
        max_bytes: u64,
    ) -> Result<Option<Vec<u8>>> {
        require_leaf(leaf)?;
        self.revalidate_boundary()?;
        if !self.parent.contains_entry(leaf).map_err(core_private)? {
            self.revalidate_boundary()?;
            return Ok(None);
        }
        self.parent
            .ensure_owner_private(leaf)
            .map_err(core_private)?;
        let Some(bytes) = self
            .parent
            .read_optional(leaf, max_bytes)
            .map_err(core_private)?
        else {
            return Err(boundary_changed(format!(
                "ledger artifact disappeared while being read: {}",
                leaf.display()
            )));
        };
        self.revalidate_boundary()?;
        Ok(Some(bytes))
    }

    pub(crate) fn create_only(&self, leaf: &Path, bytes: &[u8]) -> Result<()> {
        require_leaf(leaf)?;
        if ![
            self.names.db.as_path(),
            self.names.authority.as_path(),
            self.names.checkpoint.as_path(),
            self.names.init_pending.as_path(),
            self.names.restore_pending.as_path(),
            self.names.write_pending.as_path(),
            self.names.backup_proof.as_path(),
        ]
        .contains(&leaf)
        {
            return Err(contract(
                "KIO-E-LEDGER-PATH-001",
                "create-only publication is limited to declared lifecycle artifacts",
            ));
        }
        self.revalidate_boundary()?;
        self.parent
            .write_atomic(leaf, bytes, Publication::CreateOnly)
            .map_err(core_private)?;
        self.parent
            .ensure_owner_private(leaf)
            .map_err(core_private)?;
        self.revalidate_boundary()
    }

    /// Replace an already-existing verified private leaf. This deliberately
    /// has no upsert behavior, so checkpoint loss remains distinguishable from
    /// an ordinary update and authority files cannot be silently recreated.
    pub(crate) fn replace_existing(&self, leaf: &Path, bytes: &[u8]) -> Result<()> {
        require_leaf(leaf)?;
        if leaf != self.names.checkpoint && leaf != self.names.backup_proof {
            return Err(contract(
                "KIO-E-LEDGER-PATH-001",
                "only the ledger checkpoint or backup proof is replaceable",
            ));
        }
        self.revalidate_boundary()?;
        self.parent
            .ensure_owner_private(leaf)
            .map_err(core_private)?;
        self.parent
            .write_atomic(leaf, bytes, Publication::Replace)
            .map_err(core_private)?;
        self.parent
            .ensure_owner_private(leaf)
            .map_err(core_private)?;
        self.revalidate_boundary()
    }

    pub(crate) fn remove_exact(&self, leaf: &Path, expected: &[u8], max_bytes: u64) -> Result<()> {
        require_leaf(leaf)?;
        if leaf != self.names.init_pending
            && leaf != self.names.restore_pending
            && leaf != self.names.write_pending
        {
            return Err(contract(
                "KIO-E-LEDGER-PATH-001",
                "only an exact lifecycle pending marker is removable",
            ));
        }
        self.revalidate_boundary()?;
        self.parent
            .ensure_owner_private(leaf)
            .map_err(core_private)?;
        self.parent
            .quarantine_then_remove(leaf, expected, max_bytes)
            .map_err(core_private)?;
        self.revalidate_boundary()
    }

    pub(crate) fn recover_atomic_storage(&self) -> Result<bool> {
        self.revalidate_boundary()?;
        let recovered = self
            .parent
            .recover_atomic(&[&self.parent])
            .map_err(core_private)?;
        self.revalidate_boundary()?;
        Ok(recovered)
    }

    pub(crate) fn sync_parent(&self) -> Result<()> {
        self.revalidate_boundary()?;
        self.parent.sync().map_err(core_private)?;
        self.revalidate_boundary()
    }

    /// Establish the SQLite main file as a private create-only leaf before
    /// SQLite sees it. SQLite then opens an existing file and cannot choose a
    /// permissive mode from the process umask.
    #[cfg(test)]
    pub(crate) fn prepare_empty_db_create_only(&self) -> Result<()> {
        self.create_only(&self.names.db, b"")
    }

    /// Open the actual connection used by the lifecycle operation. CREATE and
    /// URI flags are rejected so a caller cannot redirect or implicitly heal a
    /// missing ledger through SQLite-specific pathname interpretation.
    pub(crate) fn open_existing_sqlite(
        &self,
        flags: OpenFlags,
    ) -> Result<BoundSqliteConnection<'_>> {
        if flags.contains(OpenFlags::SQLITE_OPEN_CREATE)
            || flags.contains(OpenFlags::SQLITE_OPEN_URI)
        {
            return Err(contract(
                "KIO-E-LEDGER-SQLITE-FLAGS-001",
                "ledger SQLite open must not create a file or interpret a URI",
            ));
        }
        self.revalidate_boundary()?;
        let before = SqliteFiles::capture_required_main(self)?;
        self.revalidate_boundary()?;
        let connection = Connection::open_with_flags(&self.db_path, flags)?;
        let mut bound = BoundSqliteConnection {
            session: self,
            connection: Some(connection),
            files: before,
            finished: false,
        };
        bound.capture_and_revalidate_after_open()?;
        Ok(bound)
    }

    fn revalidate_boundary(&self) -> Result<()> {
        // Keep the lock alive and locked for the complete session. Accessing
        // metadata also detects a closed/invalid handle rather than relying on
        // its presence in this struct alone.
        let live_lock_identity =
            regular_file_identity(&self.lock, &self.parent_path.join(LIFECYCLE_LOCK))?;
        if live_lock_identity != self.lock_identity {
            return Err(boundary_changed("retained lifecycle lock changed identity"));
        }

        if self.inherited_parent {
            // The parent was duplicated from `/dev/fd/N` before this session
            // began. Reopening that spelling would let a later `dup2` choose
            // a different root, so the retained capability itself is the only
            // authority for every check below.
            verify_private_parent_handle(self.parent.root_handle().as_ref(), &self.parent_path)?;
        } else {
            let named_parent = StoreDirectory::open(&self.parent_path).map_err(core_private)?;
            verify_private_parent_handle(named_parent.root_handle().as_ref(), &self.parent_path)?;
            if directory_identity(named_parent.root_handle().as_ref(), &self.parent_path)?
                != self.parent_identity
            {
                return Err(boundary_changed(
                    "ledger parent path no longer names the retained directory",
                ));
            }
            named_parent
                .ensure_owner_private(Path::new(LIFECYCLE_LOCK))
                .map_err(core_private)?;
            let named_lock = named_parent
                .open_regular_read(Path::new(LIFECYCLE_LOCK), MAX_LOCK_BYTES)
                .map_err(core_private)?;
            if regular_file_identity(&named_lock, &self.parent_path.join(LIFECYCLE_LOCK))?
                != self.lock_identity
            {
                return Err(boundary_changed(
                    "ledger lifecycle lock path no longer names the retained lock",
                ));
            }
        }
        let observed = kio_core::private_fs::read_private_file_at(
            &self.parent,
            LIFECYCLE_LOCK,
            MAX_LOCK_BYTES,
        )
        .map_err(core_private)?;
        if observed != LOCK_BYTES {
            return Err(boundary_changed("ledger lifecycle lock contents changed"));
        }
        Ok(())
    }
}

impl Drop for FsSession {
    fn drop(&mut self) {
        // `File::unlock` maps to flock(LOCK_UN) on Unix.  Unlike waiting for
        // the final close of this open-file description, it releases a lock
        // inherited by a just-spawned child that still holds a duplicate FD.
        // Drop cannot report an unlock error; a later lifecycle session will
        // fail closed if the platform did not release it.
        let _ = self.lock.unlock();
    }
}

/// The caller must consume this guard with [`Self::finish`]. A plain drop still
/// closes SQLite safely, but cannot report a final namespace substitution.
pub(crate) struct BoundSqliteConnection<'a> {
    session: &'a FsSession,
    connection: Option<Connection>,
    files: SqliteFiles,
    finished: bool,
}

impl BoundSqliteConnection<'_> {
    pub(crate) fn connection(&self) -> &Connection {
        self.connection
            .as_ref()
            .expect("bound SQLite connection is live until finish")
    }

    /// Revalidate all currently named SQLite leaves while the actual used
    /// connection remains open. Newly-created WAL/SHM leaves are captured here.
    pub(crate) fn revalidate_open(&mut self) -> Result<()> {
        self.session.revalidate_boundary()?;
        self.files.revalidate_and_capture_sidecars(self.session)?;
        self.session.revalidate_boundary()
    }

    /// Revalidate while the connection is still live, close that same
    /// connection, then recheck the parent and lifecycle lock. WAL/SHM may be
    /// legitimately removed by SQLite while the last connection closes, so
    /// their named bindings are checked before close and their retained handles
    /// remain alive through it.
    pub(crate) fn finish(mut self) -> Result<()> {
        self.revalidate_open()?;
        self.connection.take();
        self.session.revalidate_boundary()?;
        self.finished = true;
        Ok(())
    }

    fn capture_and_revalidate_after_open(&mut self) -> Result<()> {
        self.revalidate_open()
    }
}

impl Drop for BoundSqliteConnection<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.connection.take();
        }
    }
}

struct RetainedSqliteFile {
    leaf: PathBuf,
    _handle: File,
    identity: RegularFileIdentity,
}

struct SqliteFiles {
    main: RetainedSqliteFile,
    wal: Option<RetainedSqliteFile>,
    shm: Option<RetainedSqliteFile>,
}

impl SqliteFiles {
    fn capture_required_main(session: &FsSession) -> Result<Self> {
        let main = capture_leaf(session, &session.names.db)?.ok_or_else(|| {
            contract(
                "KIO-E-LEDGER-MISSING-001",
                "ledger SQLite main file is missing",
            )
        })?;
        let wal = capture_leaf(session, &session.names.wal)?;
        let shm = capture_leaf(session, &session.names.shm)?;
        Ok(Self { main, wal, shm })
    }

    fn revalidate_and_capture_sidecars(&mut self, session: &FsSession) -> Result<()> {
        revalidate_retained_leaf(session, &self.main)?;
        revalidate_or_capture(session, &session.names.wal, &mut self.wal)?;
        revalidate_or_capture(session, &session.names.shm, &mut self.shm)
    }
}

fn revalidate_or_capture(
    session: &FsSession,
    leaf: &Path,
    retained: &mut Option<RetainedSqliteFile>,
) -> Result<()> {
    if let Some(existing) = retained.as_ref() {
        return revalidate_retained_leaf(session, existing);
    }
    *retained = capture_leaf(session, leaf)?;
    Ok(())
}

fn capture_leaf(session: &FsSession, leaf: &Path) -> Result<Option<RetainedSqliteFile>> {
    if !session.parent.contains_entry(leaf).map_err(core_private)? {
        session.revalidate_boundary()?;
        return Ok(None);
    }
    session
        .parent
        .ensure_owner_private(leaf)
        .map_err(core_private)?;
    let handle = session
        .parent
        .open_regular_read(leaf, u64::MAX)
        .map_err(core_private)?;
    let identity = regular_file_identity(&handle, &session.parent_path.join(leaf))?;
    Ok(Some(RetainedSqliteFile {
        leaf: leaf.to_path_buf(),
        _handle: handle,
        identity,
    }))
}

fn revalidate_retained_leaf(session: &FsSession, retained: &RetainedSqliteFile) -> Result<()> {
    session
        .parent
        .ensure_owner_private(&retained.leaf)
        .map_err(core_private)?;
    let named = session
        .parent
        .open_regular_read(&retained.leaf, u64::MAX)
        .map_err(core_private)?;
    if regular_file_identity(&named, &session.parent_path.join(&retained.leaf))?
        != retained.identity
    {
        return Err(boundary_changed(format!(
            "SQLite artifact changed identity: {}",
            retained.leaf.display()
        )));
    }
    Ok(())
}

fn open_or_create_parent(path: &Path, allow_create: bool) -> Result<(StoreDirectory, bool)> {
    #[cfg(target_os = "linux")]
    if let Some((root, suffix)) =
        kio_core::private_fs::resolve_inherited_private_root(path).map_err(core_private)?
    {
        return open_or_create_inherited_parent(root, &suffix, path, allow_create)
            .map(|parent| (parent, true));
    }
    match StoreDirectory::open(path) {
        Ok(parent) => {
            verify_existing_ancestor_path(path)?;
            return Ok((parent, false));
        }
        Err(error) if !allow_create => {
            let code = match std::fs::symlink_metadata(path) {
                Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => {
                    "KIO-E-LEDGER-UNINITIALIZED-001"
                }
                _ => "KIO-E-LEDGER-PRIVATE-001",
            };
            return Err(contract(code, error.to_string()));
        }
        Err(_) => {}
    }

    let mut ancestor = path.to_path_buf();
    let (base, suffix) = loop {
        if let Ok(base) = StoreDirectory::open(&ancestor) {
            verify_existing_ancestor_path(&ancestor)?;
            let suffix = path.strip_prefix(&ancestor).map_err(|_| {
                contract(
                    "KIO-E-LEDGER-PATH-001",
                    "cannot bind ledger parent beneath an existing ancestor",
                )
            })?;
            break (base, suffix.to_path_buf());
        }
        if !ancestor.pop() {
            return Err(contract(
                "KIO-E-LEDGER-PRIVATE-001",
                "cannot find a safe existing ancestor for ledger parent",
            ));
        }
    };
    if suffix.as_os_str().is_empty() {
        return Ok((base, false));
    }
    create_missing_private_path(base, &ancestor, &suffix).map(|parent| (parent, false))
}

/// Traverse only the validated suffix of an inherited private root. The root
/// descriptor was duplicated before this function received it; no operation
/// below reimports the original `/dev/fd/N` spelling.
#[cfg(target_os = "linux")]
fn open_or_create_inherited_parent(
    mut current: StoreDirectory,
    suffix: &[std::ffi::OsString],
    path: &Path,
    allow_create: bool,
) -> Result<StoreDirectory> {
    verify_private_parent_handle(current.root_handle().as_ref(), current.path())?;
    let mut logical = current.path().to_path_buf();
    for name in suffix {
        let leaf = Path::new(name);
        logical.push(name);
        if current.contains_entry(leaf).map_err(core_private)? {
            let retained = current.open_directory(leaf).map_err(core_private)?;
            let next =
                StoreDirectory::from_retained(retained, logical.clone()).map_err(core_private)?;
            verify_private_parent_handle(next.root_handle().as_ref(), &logical)?;
            current = next;
            continue;
        }
        if !allow_create {
            return Err(contract(
                "KIO-E-LEDGER-UNINITIALIZED-001",
                format!("ledger parent is missing: {}", logical.display()),
            ));
        }
        let retained = current.create_directory(leaf).map_err(core_private)?;
        let next =
            StoreDirectory::from_retained(retained, logical.clone()).map_err(core_private)?;
        verify_private_parent_handle(next.root_handle().as_ref(), &logical)?;
        current = next;
    }
    // `path` is retained only for precise diagnostics: the actual authority is
    // `current`'s duplicated descriptor-relative handle.
    verify_private_parent_handle(current.root_handle().as_ref(), path)?;
    Ok(current)
}

/// SQLite accepts only a path. On Linux inherited roots are retained directory
/// capabilities, so address the main leaf through this process's *duplicated*
/// descriptor. The descriptor remains owned by `FsSession` for the complete
/// SQLite lifetime; a caller replacing its original `/dev/fd/N` cannot retarget
/// this path.
#[cfg(target_os = "linux")]
fn sqlite_path_from_retained_parent(parent: &StoreDirectory, leaf: &Path) -> Result<PathBuf> {
    use std::os::fd::AsRawFd;

    require_leaf(leaf)?;
    Ok(PathBuf::from(format!(
        "/proc/self/fd/{}",
        parent.root_handle().as_raw_fd()
    ))
    .join(leaf))
}

#[cfg(not(target_os = "linux"))]
fn sqlite_path_from_retained_parent(_parent: &StoreDirectory, _leaf: &Path) -> Result<PathBuf> {
    Err(contract(
        "KIO-E-LEDGER-PRIVATE-001",
        "inherited ledger roots are unsupported on this platform",
    ))
}

fn create_missing_private_path(
    mut current: StoreDirectory,
    base_path: &Path,
    suffix: &Path,
) -> Result<StoreDirectory> {
    let mut logical = base_path.to_path_buf();
    for component in suffix.components() {
        let Component::Normal(name) = component else {
            return Err(contract(
                "KIO-E-LEDGER-PATH-001",
                "ledger parent contains an unsafe creation component",
            ));
        };
        let leaf = Path::new(name);
        if current.contains_entry(leaf).map_err(core_private)? {
            return Err(contract(
                "KIO-E-LEDGER-PRIVATE-001",
                format!(
                    "ledger parent creation encountered an existing unsafe or raced component: {}",
                    logical.join(leaf).display()
                ),
            ));
        }
        let retained = match current.create_directory(leaf) {
            Ok(retained) => retained,
            Err(error) => {
                if current.contains_entry(leaf).map_err(core_private)? {
                    return Err(contract(
                        "KIO-E-LEDGER-PRIVATE-001",
                        format!(
                            "ledger parent creation raced an existing component: {}",
                            logical.join(leaf).display()
                        ),
                    ));
                }
                return Err(core_private(error));
            }
        };
        logical.push(leaf);
        current = StoreDirectory::from_retained(retained, logical.clone()).map_err(core_private)?;
    }
    Ok(current)
}

fn normalized_absolute_db_path(path: &Path) -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        if !path.is_absolute() {
            return Err(contract(
                "KIO-E-LEDGER-PATH-001",
                "ledger path must be absolute",
            ));
        }
        let mut normalized = PathBuf::from("/");
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => {
                    if normalized == Path::new("/") && name == "var" {
                        normalized.push("private");
                        normalized.push("var");
                    } else {
                        normalized.push(name);
                    }
                }
                _ => {
                    return Err(contract(
                        "KIO-E-LEDGER-PATH-001",
                        "ledger path is not normalized",
                    ));
                }
            }
        }
        Ok(normalized)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(path.to_path_buf())
    }
}

#[cfg(unix)]
fn verify_existing_ancestor_path(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    // Validate every already-existing component before creating anything
    // below it. Root-owned sticky directories (for example /private/tmp) are
    // the sole writable-by-others exception, matching private_fs.
    // SAFETY: geteuid has no arguments and no memory-safety preconditions.
    let uid = unsafe { libc::geteuid() };
    let mut prefix = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(value) => prefix.push(value),
            _ => {
                return Err(contract(
                    "KIO-E-LEDGER-PATH-001",
                    "ledger parent contains an unsafe component",
                ));
            }
        }
        let directory = StoreDirectory::open(&prefix).map_err(core_private)?;
        let metadata = directory
            .root_handle()
            .metadata()
            .map_err(|error| PipelineError::Io {
                path: prefix.display().to_string(),
                message: error.to_string(),
            })?;
        let owner_trusted = metadata.uid() == uid || metadata.uid() == 0;
        let writable_by_others = metadata.mode() & 0o022 != 0;
        let root_owned_sticky = metadata.uid() == 0 && metadata.mode() & 0o1000 != 0;
        if !metadata.is_dir() || !owner_trusted || (writable_by_others && !root_owned_sticky) {
            return Err(contract(
                "KIO-E-LEDGER-PRIVATE-001",
                format!(
                    "ledger creation ancestor is not privately controlled: {}",
                    prefix.display()
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn verify_existing_ancestor_path(path: &Path) -> Result<()> {
    // StoreDirectory already opened every component without traversing a
    // reparse point. The newly created final parent receives the protected ACL
    // and is checked through its retained handle before lock publication.
    StoreDirectory::open(path).map(|_| ()).map_err(core_private)
}

fn reject_retired_names(parent: &StoreDirectory) -> Result<()> {
    for name in RETIRED_LEDGER_BASENAMES {
        if parent
            .contains_entry(Path::new(name))
            .map_err(core_private)?
        {
            return Err(contract(
                "KIO-E-LEDGER-LEGACY-JSONL-001",
                format!("retired ledger artifact exists: {name}"),
            ));
        }
    }
    Ok(())
}

fn require_leaf(path: &Path) -> Result<()> {
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(contract(
            "KIO-E-LEDGER-PATH-001",
            "ledger artifact must be one direct relative leaf",
        ));
    }
    Ok(())
}

fn contract(code: &'static str, message: impl Into<String>) -> PipelineError {
    PipelineError::contract(code, message)
}

fn core_private(error: kio_core::KioError) -> PipelineError {
    contract("KIO-E-LEDGER-PRIVATE-001", error.to_string())
}

fn boundary_changed(message: impl Into<String>) -> PipelineError {
    contract("KIO-E-LEDGER-BOUNDARY-RACE-001", message)
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RegularFileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
fn directory_identity(file: &File, path: &Path) -> Result<DirectoryIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata().map_err(|error| PipelineError::Io {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    if !metadata.is_dir() {
        return Err(contract(
            "KIO-E-LEDGER-PRIVATE-001",
            "ledger parent handle is not a directory",
        ));
    }
    Ok(DirectoryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(unix)]
fn regular_file_identity(file: &File, path: &Path) -> Result<RegularFileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata().map_err(|error| PipelineError::Io {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(contract(
            "KIO-E-LEDGER-PRIVATE-001",
            format!(
                "ledger leaf is not a single-link regular file: {}",
                path.display()
            ),
        ));
    }
    Ok(RegularFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(unix)]
fn verify_private_parent_handle(file: &File, path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata().map_err(|error| PipelineError::Io {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    // SAFETY: geteuid has no arguments and no memory-safety preconditions.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(contract(
            "KIO-E-LEDGER-PRIVATE-001",
            format!("ledger parent is not owner-private: {}", path.display()),
        ));
    }
    Ok(())
}

#[cfg(windows)]
type DirectoryIdentity = kio_core::cas::WindowsDirectoryIdentity;
#[cfg(windows)]
type RegularFileIdentity = kio_core::cas::WindowsRegularFileIdentity;

#[cfg(windows)]
fn directory_identity(file: &File, path: &Path) -> Result<DirectoryIdentity> {
    kio_core::cas::windows_directory_handle_identity(file).ok_or_else(|| {
        contract(
            "KIO-E-LEDGER-PRIVATE-001",
            format!(
                "ledger parent is reparse or non-directory: {}",
                path.display()
            ),
        )
    })
}

#[cfg(windows)]
fn regular_file_identity(file: &File, path: &Path) -> Result<RegularFileIdentity> {
    kio_core::cas::windows_regular_file_handle_identity(file).ok_or_else(|| {
        contract(
            "KIO-E-LEDGER-PRIVATE-001",
            format!(
                "ledger leaf is reparse, nonregular, or hardlinked: {}",
                path.display()
            ),
        )
    })
}

#[cfg(windows)]
fn verify_private_parent_handle(file: &File, path: &Path) -> Result<()> {
    directory_identity(file, path)?;
    kio_core::private_fs::verify_owner_private_handle(file).map_err(core_private)
}

#[cfg(not(any(unix, windows)))]
compile_error!("ledger lifecycle filesystem capability requires Unix or Windows");

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn db_path(root: &Path) -> PathBuf {
        root.join("device").join("cost-ledger.sqlite")
    }

    fn private_tempdir() -> tempfile::TempDir {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut builder = tempfile::Builder::new();
            builder.permissions(fs::Permissions::from_mode(0o700));
            #[cfg(target_os = "macos")]
            {
                builder.tempdir_in("/private/tmp").unwrap()
            }
            #[cfg(not(target_os = "macos"))]
            {
                builder.tempdir().unwrap()
            }
        }
        #[cfg(windows)]
        {
            tempfile::tempdir().unwrap()
        }
    }

    #[test]
    fn names_are_derived_from_db_filename() {
        let path = if cfg!(windows) {
            PathBuf::from(r"C:\kio\custom.db")
        } else {
            PathBuf::from("/tmp/kio/custom.db")
        };
        let (_, names) = ArtifactNames::from_db_path(&path).unwrap();
        assert_eq!(names.db, Path::new("custom.db"));
        assert_eq!(names.wal, Path::new("custom.db-wal"));
        assert_eq!(names.shm, Path::new("custom.db-shm"));
        assert_eq!(names.authority, Path::new("custom.db.authority.json"));
        assert_eq!(names.checkpoint, Path::new("custom.db.checkpoint.json"));
        assert_eq!(names.init_pending, Path::new("custom.db.init.pending"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_var_root_spelling_maps_once_to_private_var() {
        let temp = tempfile::tempdir_in("/private/var/tmp").unwrap();
        let canonical = temp.path().join("device/ledger.db");
        let alias = PathBuf::from("/var").join(
            canonical
                .strip_prefix("/private/var")
                .expect("test directory is below /private/var"),
        );
        let session = FsSession::acquire(&alias, true).unwrap();
        assert_eq!(session.db_path, canonical);
        let authority = session.names().authority.clone();
        session.create_only(&authority, b"private").unwrap();
        assert_eq!(session.read_private(&authority, 64).unwrap(), b"private");
        assert!(canonical.with_extension("db.authority.json").exists());
    }

    #[test]
    fn creates_private_parent_and_classifies_partial_state() {
        let temp = private_tempdir();
        let path = db_path(temp.path());
        let session = FsSession::acquire(&path, true).unwrap();
        assert_eq!(session.artifact_state().unwrap(), ArtifactSetState::Missing);
        session
            .create_only(&session.names().init_pending, b"pending")
            .unwrap();
        assert!(matches!(
            session.artifact_state().unwrap(),
            ArtifactSetState::Partial { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn missing_lock_distinguishes_uninitialized_partial_and_retired() {
        let temp = private_tempdir();
        let parent = temp.path().join("device");
        fs::create_dir(&parent).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        let path = parent.join("ledger.db");

        let uninitialized = FsSession::acquire(&path, false).unwrap_err();
        assert!(matches!(
            uninitialized,
            PipelineError::Contract {
                code: "KIO-E-LEDGER-UNINITIALIZED-001",
                ..
            }
        ));

        fs::write(parent.join("ledger.db.authority.json"), b"partial").unwrap();
        #[cfg(unix)]
        fs::set_permissions(
            parent.join("ledger.db.authority.json"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let partial = FsSession::acquire(&path, false).unwrap_err();
        assert!(matches!(
            partial,
            PipelineError::Contract {
                code: "KIO-E-LEDGER-INIT-PARTIAL-001",
                ..
            }
        ));

        fs::write(parent.join(RETIRED_LEDGER_BASENAMES[0]), b"retired").unwrap();
        let retired = FsSession::acquire(&path, false).unwrap_err();
        assert!(matches!(
            retired,
            PipelineError::Contract {
                code: "KIO-E-LEDGER-LEGACY-JSONL-001",
                ..
            }
        ));
    }

    #[test]
    fn lifecycle_lock_is_exclusive_and_never_replaced() {
        let temp = private_tempdir();
        let path = db_path(temp.path());
        let first = FsSession::acquire(&path, true).unwrap();
        let lock_path = path.parent().unwrap().join(LIFECYCLE_LOCK);
        let before = fs::metadata(&lock_path).unwrap();
        let error = FsSession::acquire(&path, false).unwrap_err();
        assert!(matches!(error, PipelineError::Locked { .. }));
        drop(first);
        let second = FsSession::acquire(&path, false).unwrap();
        drop(second);
        let after = fs::metadata(lock_path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
        }
    }

    #[cfg(unix)]
    #[test]
    fn dropping_session_explicitly_unlocks_a_duplicated_lock_description() {
        let temp = private_tempdir();
        let path = db_path(temp.path());
        let first = FsSession::acquire(&path, true).unwrap();
        // `try_clone` duplicates the same open-file description, matching an
        // inherited descriptor across fork/exec. Keep it open after session
        // drop: success below proves Drop called unlock rather than relying on
        // the final descriptor close.
        let inherited = first.lock.try_clone().unwrap();
        drop(first);
        let second = FsSession::acquire(&path, false).unwrap();
        drop(second);
        drop(inherited);
    }

    #[test]
    fn actual_sqlite_connection_is_opened_and_finished_under_the_session() {
        let temp = private_tempdir();
        let path = db_path(temp.path());
        let session = FsSession::acquire(&path, true).unwrap();
        session.prepare_empty_db_create_only().unwrap();
        let bound = session
            .open_existing_sqlite(
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .unwrap();
        bound
            .connection()
            .execute_batch("CREATE TABLE capability_test (value INTEGER NOT NULL);")
            .unwrap();
        bound.finish().unwrap();
        assert!(session.exists_safe(session.names().db.as_path()).unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inherited_parent_keeps_writes_and_sqlite_on_the_duplicated_root() {
        use std::os::fd::AsRawFd;

        let root_a = private_tempdir();
        let root_b = private_tempdir();
        let held_a = File::open(root_a.path()).unwrap();
        let held_b = File::open(root_b.path()).unwrap();
        let descriptor = unsafe { libc::fcntl(held_a.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        assert!(descriptor >= 3, "allocate inherited directory descriptor");
        let inherited = PathBuf::from(format!("/dev/fd/{descriptor}/device/cost-ledger.sqlite"));

        let session = FsSession::acquire(&inherited, true).unwrap();
        // Retargeting the caller's descriptor after acquire must not redirect a
        // lock-protected marker or SQLite's path-only open to root_b.
        assert_eq!(
            unsafe { libc::dup2(held_b.as_raw_fd(), descriptor) },
            descriptor
        );
        session.prepare_empty_db_create_only().unwrap();
        session
            .create_only(&session.names().authority, b"authority")
            .unwrap();
        let bound = session
            .open_existing_sqlite(
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .unwrap();
        bound.finish().unwrap();

        assert!(root_a.path().join("device/cost-ledger.sqlite").exists());
        assert!(
            root_a
                .path()
                .join("device/cost-ledger.sqlite.authority.json")
                .exists()
        );
        assert!(!root_b.path().join("device").exists());
        drop(session);
        assert_eq!(unsafe { libc::close(descriptor) }, 0);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_and_hardlinked_lock_leaves() {
        use std::os::unix::fs::symlink;

        let temp = private_tempdir();
        let parent = temp.path().join("device");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        let target = temp.path().join("target");
        fs::write(&target, LOCK_BYTES).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&target, parent.join(LIFECYCLE_LOCK)).unwrap();
        assert!(FsSession::acquire(&parent.join("ledger.db"), false).is_err());

        fs::remove_file(parent.join(LIFECYCLE_LOCK)).unwrap();
        fs::hard_link(&target, parent.join(LIFECYCLE_LOCK)).unwrap();
        assert!(FsSession::acquire(&parent.join("ledger.db"), false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_public_parent_and_lock_permissions() {
        let temp = private_tempdir();
        let path = db_path(temp.path());
        {
            let session = FsSession::acquire(&path, true).unwrap();
            drop(session);
        }
        let parent = path.parent().unwrap();
        fs::set_permissions(parent, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(FsSession::acquire(&path, false).is_err());
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(
            parent.join(LIFECYCLE_LOCK),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(FsSession::acquire(&path, false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_existing_creation_ancestor_is_rejected_without_mutation() {
        let temp = private_tempdir();
        let unsafe_ancestor = temp.path().join("unsafe");
        fs::create_dir(&unsafe_ancestor).unwrap();
        fs::set_permissions(&unsafe_ancestor, fs::Permissions::from_mode(0o777)).unwrap();
        let missing_parent = unsafe_ancestor.join("new").join("device");
        let path = missing_parent.join("ledger.db");

        assert!(FsSession::acquire(&path, true).is_err());
        assert!(!unsafe_ancestor.join("new").exists());
    }

    #[cfg(unix)]
    #[test]
    fn arbitrary_symlink_creation_ancestor_is_refused_without_mutation() {
        use std::os::unix::fs::symlink;

        let temp = private_tempdir();
        let target = temp.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        let alias = temp.path().join("alias");
        symlink(&target, &alias).unwrap();
        let path = alias.join("device/ledger.db");

        let error = FsSession::acquire(&path, true).unwrap_err();
        assert!(matches!(
            error,
            PipelineError::Contract {
                code: "KIO-E-LEDGER-PRIVATE-001",
                ..
            }
        ));
        assert!(!target.join("device").exists());
    }

    #[cfg(unix)]
    #[test]
    fn retained_parent_swap_is_detected_before_artifact_write() {
        let temp = private_tempdir();
        let path = db_path(temp.path());
        let session = FsSession::acquire(&path, true).unwrap();
        let parent = path.parent().unwrap();
        let displaced = temp.path().join("displaced");
        fs::rename(parent, &displaced).unwrap();
        fs::create_dir(parent).unwrap();
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).unwrap();
        fs::copy(displaced.join(LIFECYCLE_LOCK), parent.join(LIFECYCLE_LOCK)).unwrap();
        fs::set_permissions(
            parent.join(LIFECYCLE_LOCK),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();

        let leaf = session.names().authority.clone();
        let error = session.create_only(&leaf, b"secret").unwrap_err();
        assert!(
            matches!(
                error,
                PipelineError::Contract {
                    code: "KIO-E-LEDGER-BOUNDARY-RACE-001",
                    ..
                }
            ),
            "unexpected error: {error:?}"
        );
        assert!(!displaced.join(&leaf).exists());
        assert!(!parent.join(&leaf).exists());
    }

    #[cfg(unix)]
    #[test]
    fn sqlite_main_replacement_is_detected_while_used_connection_is_live() {
        let temp = private_tempdir();
        let path = db_path(temp.path());
        let session = FsSession::acquire(&path, true).unwrap();
        session.prepare_empty_db_create_only().unwrap();
        let mut bound = session
            .open_existing_sqlite(
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .unwrap();
        let displaced = path.with_extension("displaced");
        fs::rename(&path, &displaced).unwrap();
        fs::write(&path, b"").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let error = bound.revalidate_open().unwrap_err();
        assert!(matches!(
            error,
            PipelineError::Contract {
                code: "KIO-E-LEDGER-BOUNDARY-RACE-001",
                ..
            }
        ));
    }

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
}
