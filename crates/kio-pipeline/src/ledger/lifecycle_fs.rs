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

use kio_core::control::ControlHandle;
use kio_core::store_dir::{Publication, StoreDirectory};
use rusqlite::{Connection, OpenFlags};

use super::schema::RETIRED_LEDGER_BASENAMES;
use crate::platform::ledger_lifecycle::{
    DirectoryIdentity, RegularFileIdentity, directory_identity, normalized_absolute_db_path,
    path_component_is_unsafe, prepare_sqlite_sidecars, regular_file_identity,
    revalidate_lock_contents, sqlite_path_from_retained_parent, try_inherited_parent,
    validate_lock_contents_before_acquire, validate_open_lock_before_acquire,
    verify_existing_ancestor_path, verify_private_parent_handle,
};
use crate::{PipelineError, Result};

pub(crate) const LIFECYCLE_LOCK: &str = "ledger.lifecycle.lock";
pub(crate) const LOCK_BYTES: &[u8] = b"kio-ledger-lifecycle-lock-v1\n";
pub(crate) const MAX_LOCK_BYTES: u64 = 128;

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
        if path.components().any(path_component_is_unsafe) {
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
    pub(crate) fn acquire_with_control(
        db_path: &Path,
        allow_create: bool,
        control: ControlHandle,
    ) -> Result<Self> {
        let db_path = normalized_absolute_db_path(db_path)?;
        let (parent_path, names) = ArtifactNames::from_db_path(&db_path)?;
        let (parent, inherited_parent) =
            open_or_create_parent(&parent_path, allow_create, control)?;
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
        control: ControlHandle,
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
        let parent = StoreDirectory::from_retained_with_control(
            retained,
            parent_path.to_path_buf(),
            control,
        )
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

    pub(crate) fn control(&self) -> &ControlHandle {
        self.parent.control()
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
        validate_lock_contents_before_acquire(&parent)?;
        let lock = parent
            .open_regular_read(lock_leaf, MAX_LOCK_BYTES)
            .map_err(core_private)?;
        let lock_identity = regular_file_identity(&lock, &absolute_lock)?;
        validate_open_lock_before_acquire(&lock)?;
        // Windows denies data reads through another handle while this range
        // is locked. Acquire before inspecting contents so contenders report
        // Locked, then validate bytes through the handle owning the lock.
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
    pub(crate) fn parent(&self) -> &StoreDirectory {
        &self.parent
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

    /// Immutable embedding recovery evidence. The caller supplies the exact
    /// authority-bound locator; ordinary lifecycle artifact guards stay closed.
    pub(super) fn create_embedding_artifact(&self, leaf: &Path, bytes: &[u8]) -> Result<()> {
        require_leaf(leaf)?;
        let name = leaf
            .to_str()
            .ok_or_else(|| contract("KIO-E-LEDGER-PATH-001", "invalid job locator"))?;
        if !name.starts_with("embedding-job-") || !name.ends_with(".json") {
            return Err(contract(
                "KIO-E-LEDGER-PATH-001",
                "invalid embedding job artifact",
            ));
        }
        self.revalidate_boundary()?;
        self.parent
            .write_atomic(leaf, bytes, Publication::CreateOnly)
            .map_err(core_private)?;
        self.parent
            .ensure_owner_private(leaf)
            .map_err(core_private)?;
        self.sync_parent()?;
        self.revalidate_boundary()
    }

    pub(super) fn remove_embedding_artifact(
        &self,
        leaf: &Path,
        bytes: &[u8],
        max: u64,
    ) -> Result<()> {
        require_leaf(leaf)?;
        let name = leaf
            .to_str()
            .ok_or_else(|| contract("KIO-E-LEDGER-PATH-001", "invalid job locator"))?;
        if !name.starts_with("embedding-job-") || !name.ends_with(".json") {
            return Err(contract(
                "KIO-E-LEDGER-PATH-001",
                "invalid embedding job artifact",
            ));
        }
        self.revalidate_boundary()?;
        self.parent
            .ensure_owner_private(leaf)
            .map_err(core_private)?;
        self.parent
            .quarantine_then_remove(leaf, bytes, max)
            .map_err(core_private)?;
        self.sync_parent()?;
        self.revalidate_boundary()
    }

    /// The single bounded deferred-cleanup manifest for one ledger authority.
    pub(super) fn write_embedding_cleanup_manifest(&self, leaf: &Path, bytes: &[u8]) -> Result<()> {
        require_leaf(leaf)?;
        let name = leaf
            .to_str()
            .ok_or_else(|| contract("KIO-E-LEDGER-PATH-001", "invalid cleanup locator"))?;
        if !name.starts_with("embedding-cleanup-")
            || !name.ends_with(".json")
            || bytes.len() > 256 * 1024
        {
            return Err(contract(
                "KIO-E-LEDGER-PATH-001",
                "invalid embedding cleanup manifest",
            ));
        }
        self.revalidate_boundary()?;
        let publication = if self.parent.contains_entry(leaf).map_err(core_private)? {
            self.parent
                .ensure_owner_private(leaf)
                .map_err(core_private)?;
            Publication::Replace
        } else {
            Publication::CreateOnly
        };
        self.parent
            .write_atomic(leaf, bytes, publication)
            .map_err(core_private)?;
        self.parent
            .ensure_owner_private(leaf)
            .map_err(core_private)?;
        self.sync_parent()?;
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
        let mut before = SqliteFiles::capture_required_main(self)?;
        prepare_sqlite_sidecars(self, &mut before, flags)?;
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

    pub(crate) fn revalidate_boundary(&self) -> Result<()> {
        // Keep the lock alive and locked for the complete session. Accessing
        // metadata also detects a closed/invalid handle rather than relying on
        // its presence in this struct alone.
        let live_lock_identity =
            regular_file_identity(&self.lock, &self.parent_path.join(LIFECYCLE_LOCK))?;
        if live_lock_identity != self.lock_identity {
            return Err(boundary_changed("retained lifecycle lock changed identity"));
        }

        if self.inherited_parent {
            // Inherited roots and snapshot parents are already bound through
            // retained handles. Their diagnostic paths may be renamed or
            // retargeted, so the retained capability is the authority below.
            verify_private_parent_handle(self.parent.root_handle().as_ref(), &self.parent_path)?;
        } else {
            let named_parent =
                StoreDirectory::open_with_control(&self.parent_path, self.control().clone())
                    .map_err(core_private)?;
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
        revalidate_lock_contents(
            &self.parent,
            &self.lock,
            &self.lock_identity,
            &self.parent_path,
        )?;
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

pub(crate) struct RetainedSqliteFile {
    leaf: PathBuf,
    _handle: File,
    identity: RegularFileIdentity,
}

pub(crate) struct SqliteFiles {
    main: RetainedSqliteFile,
    pub(crate) wal: Option<RetainedSqliteFile>,
    pub(crate) shm: Option<RetainedSqliteFile>,
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

pub(crate) fn capture_leaf(session: &FsSession, leaf: &Path) -> Result<Option<RetainedSqliteFile>> {
    if !session
        .parent()
        .contains_entry(leaf)
        .map_err(core_private)?
    {
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

fn open_or_create_parent(
    path: &Path,
    allow_create: bool,
    control: ControlHandle,
) -> Result<(StoreDirectory, bool)> {
    if let Some(parent) = try_inherited_parent(path, allow_create, control.clone())? {
        return Ok((parent, true));
    }
    match StoreDirectory::open_with_control(path, control.clone()) {
        Ok(parent) => {
            verify_existing_ancestor_path(path, &control)?;
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
        if let Ok(base) = StoreDirectory::open_with_control(&ancestor, control.clone()) {
            verify_existing_ancestor_path(&ancestor, &control)?;
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
        current = StoreDirectory::from_retained_with_control(
            retained,
            logical.clone(),
            current.control().clone(),
        )
        .map_err(core_private)?;
    }
    Ok(current)
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

pub(crate) fn require_leaf(path: &Path) -> Result<()> {
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(contract(
            "KIO-E-LEDGER-PATH-001",
            "ledger artifact must be one direct relative leaf",
        ));
    }
    Ok(())
}

pub(crate) fn contract(code: &'static str, message: impl Into<String>) -> PipelineError {
    PipelineError::contract(code, message)
}

pub(crate) fn core_private(error: kio_core::KioError) -> PipelineError {
    contract("KIO-E-LEDGER-PRIVATE-001", error.to_string())
}

pub(crate) fn boundary_changed(message: impl Into<String>) -> PipelineError {
    contract("KIO-E-LEDGER-BOUNDARY-RACE-001", message)
}

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
        let session = FsSession::acquire_with_control(
            &alias,
            true,
            kio_core::control::ControlHandle::default(),
        )
        .unwrap();
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
        let session = FsSession::acquire_with_control(
            &path,
            true,
            kio_core::control::ControlHandle::default(),
        )
        .unwrap();
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

        let uninitialized = FsSession::acquire_with_control(
            &path,
            false,
            kio_core::control::ControlHandle::default(),
        )
        .unwrap_err();
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
        let partial = FsSession::acquire_with_control(
            &path,
            false,
            kio_core::control::ControlHandle::default(),
        )
        .unwrap_err();
        assert!(matches!(
            partial,
            PipelineError::Contract {
                code: "KIO-E-LEDGER-INIT-PARTIAL-001",
                ..
            }
        ));

        fs::write(parent.join(RETIRED_LEDGER_BASENAMES[0]), b"retired").unwrap();
        let retired = FsSession::acquire_with_control(
            &path,
            false,
            kio_core::control::ControlHandle::default(),
        )
        .unwrap_err();
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
        let first = FsSession::acquire_with_control(
            &path,
            true,
            kio_core::control::ControlHandle::default(),
        )
        .unwrap();
        let lock_path = path.parent().unwrap().join(LIFECYCLE_LOCK);
        let before = regular_file_identity(&first.lock, &lock_path).unwrap();
        first.revalidate_boundary().unwrap();
        first.revalidate_boundary().unwrap();
        let error = FsSession::acquire_with_control(
            &path,
            false,
            kio_core::control::ControlHandle::default(),
        )
        .unwrap_err();
        assert!(matches!(error, PipelineError::Locked { .. }));
        first.revalidate_boundary().unwrap();
        drop(first);
        let second = FsSession::acquire_with_control(
            &path,
            false,
            kio_core::control::ControlHandle::default(),
        )
        .unwrap();
        second.revalidate_boundary().unwrap();
        assert_eq!(
            before,
            regular_file_identity(&second.lock, &lock_path).unwrap()
        );
        drop(second);
    }

    #[cfg(windows)]
    #[test]
    fn locked_lifecycle_revalidation_reads_from_zero_after_cursor_moves() {
        use std::io::{Seek, SeekFrom};

        let temp = private_tempdir();
        let path = db_path(temp.path());
        let mut session = FsSession::acquire_with_control(
            &path,
            true,
            kio_core::control::ControlHandle::default(),
        )
        .unwrap();
        session.lock.seek(SeekFrom::End(0)).unwrap();
        session.revalidate_boundary().unwrap();
        session.revalidate_boundary().unwrap();
        let contender = FsSession::acquire_with_control(
            &path,
            false,
            kio_core::control::ControlHandle::default(),
        )
        .unwrap_err();
        assert!(matches!(contender, PipelineError::Locked { .. }));
    }

    #[cfg(windows)]
    #[test]
    fn retained_snapshot_parent_rejects_replaced_locked_marker() {
        for replacement in [LOCK_BYTES, b"replacement marker".as_slice()] {
            let temp = private_tempdir();
            let path = db_path(temp.path());
            drop(
                FsSession::acquire_with_control(
                    &path,
                    true,
                    kio_core::control::ControlHandle::default(),
                )
                .unwrap(),
            );
            let parent_path = path.parent().unwrap();
            let parent = StoreDirectory::open(parent_path).unwrap();
            let session = FsSession::acquire_retained_snapshot_parent(
                parent.root_handle().as_ref(),
                parent_path,
                "cost-ledger.sqlite",
                ControlHandle::default(),
            )
            .unwrap();
            assert!(session.inherited_parent);
            session.revalidate_boundary().unwrap();
            session.revalidate_boundary().unwrap();

            let displaced = parent_path.join("displaced.lifecycle.lock");
            fs::rename(parent_path.join(LIFECYCLE_LOCK), &displaced)
                .expect("held Windows lock must allow same-volume rename");
            parent
                .write_atomic(
                    Path::new(LIFECYCLE_LOCK),
                    replacement,
                    Publication::CreateOnly,
                )
                .unwrap();
            assert_eq!(
                regular_file_identity(&session.lock, &displaced).unwrap(),
                session.lock_identity,
                "the original locked handle remains valid after displacement"
            );
            let error = session.revalidate_boundary().unwrap_err();
            assert!(matches!(
                error,
                PipelineError::Contract {
                    code: "KIO-E-LEDGER-BOUNDARY-RACE-001",
                    ..
                }
            ));
        }
    }

    #[cfg(windows)]
    #[test]
    fn retained_snapshot_parent_stays_bound_after_diagnostic_parent_replacement() {
        let temp = private_tempdir();
        let path = db_path(temp.path());
        drop(
            FsSession::acquire_with_control(
                &path,
                true,
                kio_core::control::ControlHandle::default(),
            )
            .unwrap(),
        );
        let parent_path = path.parent().unwrap();
        let parent = StoreDirectory::open(parent_path).unwrap();
        let displaced = temp.path().join("displaced.device");
        fs::rename(parent_path, &displaced)
            .expect("retained Windows parent must allow same-volume rename");
        // Rename before acquiring a regular-file lock: the directory
        // capability stays bound, while no named descendant handle prevents
        // Windows from moving its ancestor directory.
        let replacement = FsSession::acquire_with_control(
            &path,
            true,
            kio_core::control::ControlHandle::default(),
        )
        .unwrap();
        let session = FsSession::acquire_retained_snapshot_parent(
            parent.root_handle().as_ref(),
            parent_path,
            "cost-ledger.sqlite",
            ControlHandle::default(),
        )
        .unwrap();
        // The original diagnostic spelling now names an independent valid
        // private parent and lock. The snapshot session must keep using its
        // displaced capability, including the name-binding checks.
        assert_ne!(replacement.lock_identity, session.lock_identity);
        session.revalidate_boundary().unwrap();
        session.revalidate_boundary().unwrap();
        let contender = FsSession::acquire_retained_snapshot_parent(
            parent.root_handle().as_ref(),
            parent_path,
            "cost-ledger.sqlite",
            ControlHandle::default(),
        )
        .unwrap_err();
        assert!(matches!(contender, PipelineError::Locked { .. }));
    }

    #[cfg(windows)]
    fn publish_private_test_database(session: &FsSession) {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE TABLE sidecar_test (value INTEGER NOT NULL);")
            .unwrap();
        let image = connection.serialize("main").unwrap();
        session.create_only(&session.names.db, &image).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn private_sidecars_keep_identity_through_wal_checkpoint_and_reopen() {
        let temp = private_tempdir();
        let path = db_path(temp.path());
        let session = FsSession::acquire_with_control(
            &path,
            true,
            kio_core::control::ControlHandle::default(),
        )
        .unwrap();
        publish_private_test_database(&session);
        drop(session);

        for expected_rows in 1..=3 {
            let session = FsSession::acquire_with_control(
                &path,
                false,
                kio_core::control::ControlHandle::default(),
            )
            .unwrap();
            let mut bound = session
                .open_existing_sqlite(OpenFlags::SQLITE_OPEN_READ_WRITE)
                .unwrap();
            let wal_identity = bound.files.wal.as_ref().unwrap().identity;
            let shm_identity = bound.files.shm.as_ref().unwrap().identity;
            let mode: String = bound
                .connection()
                .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
                .unwrap();
            assert_eq!(mode, "wal");
            bound
                .connection()
                .execute("INSERT INTO sidecar_test VALUES (?1)", [expected_rows])
                .unwrap();
            let rows: i64 = bound
                .connection()
                .query_row("SELECT count(*) FROM sidecar_test", [], |row| row.get(0))
                .unwrap();
            assert_eq!(rows, expected_rows);
            let busy: i64 = bound
                .connection()
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
                .unwrap();
            assert_eq!(busy, 0);
            bound.revalidate_open().unwrap();
            assert_eq!(bound.files.wal.as_ref().unwrap().identity, wal_identity);
            assert_eq!(bound.files.shm.as_ref().unwrap().identity, shm_identity);
            for sidecar in [&bound.files.wal, &bound.files.shm] {
                kio_core::platform::verify_owner_private_handle(&sidecar.as_ref().unwrap()._handle)
                    .unwrap();
            }
            bound.finish().unwrap();
            let image = fs::read(&path).unwrap();
            assert_eq!(&image[18..20], &[2, 2], "WAL mode must persist in main");
            assert!(!path.with_file_name("cost-ledger.sqlite-wal").exists());
            assert!(!path.with_file_name("cost-ledger.sqlite-shm").exists());
        }
    }

    #[cfg(windows)]
    #[test]
    fn unsafe_existing_sqlite_leaf_rejects_before_sidecar_publication() {
        for unsafe_leaf in ["main", "wal", "shm"] {
            let temp = private_tempdir();
            let path = db_path(temp.path());
            let session = FsSession::acquire_with_control(
                &path,
                true,
                kio_core::control::ControlHandle::default(),
            )
            .unwrap();
            if unsafe_leaf != "main" {
                publish_private_test_database(&session);
            }
            let leaf = match unsafe_leaf {
                "main" => &session.names.db,
                "wal" => &session.names.wal,
                _ => &session.names.shm,
            };
            let unsafe_path = session.parent_path.join(leaf);
            fs::write(&unsafe_path, b"existing unsafe bytes").unwrap();
            let retained = File::open(&unsafe_path).unwrap();
            assert!(kio_core::platform::verify_owner_private_handle(&retained).is_err());
            let identity = regular_file_identity(&retained, &unsafe_path).unwrap();
            let error = session
                .open_existing_sqlite(OpenFlags::SQLITE_OPEN_READ_WRITE)
                .err()
                .expect("unsafe existing leaf must reject SQLite open");
            assert!(matches!(
                error,
                PipelineError::Contract {
                    code: "KIO-E-LEDGER-PRIVATE-001",
                    ..
                }
            ));
            assert_eq!(fs::read(&unsafe_path).unwrap(), b"existing unsafe bytes");
            let after = File::open(&unsafe_path).unwrap();
            assert_eq!(
                regular_file_identity(&after, &unsafe_path).unwrap(),
                identity
            );
            assert!(kio_core::platform::verify_owner_private_handle(&after).is_err());
            for sidecar in [&session.names.wal, &session.names.shm] {
                if sidecar != leaf {
                    assert!(!session.parent_path.join(sidecar).exists());
                }
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn hardlinked_existing_sidecar_rejects_without_creating_its_counterpart() {
        for unsafe_wal in [true, false] {
            let temp = private_tempdir();
            let path = db_path(temp.path());
            let session = FsSession::acquire_with_control(
                &path,
                true,
                kio_core::control::ControlHandle::default(),
            )
            .unwrap();
            publish_private_test_database(&session);
            session
                .create_only(&session.names.authority, b"hardlinked private bytes")
                .unwrap();
            let (leaf, absent) = if unsafe_wal {
                (&session.names.wal, &session.names.shm)
            } else {
                (&session.names.shm, &session.names.wal)
            };
            fs::hard_link(
                session.parent_path.join(&session.names.authority),
                session.parent_path.join(leaf),
            )
            .unwrap();
            assert!(
                session
                    .open_existing_sqlite(OpenFlags::SQLITE_OPEN_READ_WRITE)
                    .is_err()
            );
            assert!(!session.parent_path.join(absent).exists());
            assert_eq!(
                fs::read(session.parent_path.join(leaf)).unwrap(),
                b"hardlinked private bytes"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn missing_sqlite_main_never_creates_sidecars() {
        let temp = private_tempdir();
        let path = db_path(temp.path());
        let session = FsSession::acquire_with_control(
            &path,
            true,
            kio_core::control::ControlHandle::default(),
        )
        .unwrap();
        let error = session
            .open_existing_sqlite(OpenFlags::SQLITE_OPEN_READ_WRITE)
            .err()
            .expect("missing main must reject SQLite open");
        assert!(matches!(
            error,
            PipelineError::Contract {
                code: "KIO-E-LEDGER-MISSING-001",
                ..
            }
        ));
        for leaf in [&session.names.db, &session.names.wal, &session.names.shm] {
            assert!(!session.parent_path.join(leaf).exists());
        }
    }

    #[cfg(windows)]
    #[test]
    fn read_only_sqlite_open_does_not_precreate_sidecars() {
        let temp = private_tempdir();
        let path = db_path(temp.path());
        let session = FsSession::acquire_with_control(
            &path,
            true,
            kio_core::control::ControlHandle::default(),
        )
        .unwrap();
        publish_private_test_database(&session);
        let before = fs::read(&path).unwrap();
        let bound = session
            .open_existing_sqlite(OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
        let rows: i64 = bound
            .connection()
            .query_row("SELECT count(*) FROM sidecar_test", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 0);
        bound.finish().unwrap();
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(!session.parent_path.join(&session.names.wal).exists());
        assert!(!session.parent_path.join(&session.names.shm).exists());
    }

    #[cfg(unix)]
    #[test]
    fn dropping_session_explicitly_unlocks_a_duplicated_lock_description() {
        let temp = private_tempdir();
        let path = db_path(temp.path());
        let first = FsSession::acquire_with_control(
            &path,
            true,
            kio_core::control::ControlHandle::default(),
        )
        .unwrap();
        // `try_clone` duplicates the same open-file description, matching an
        // inherited descriptor across fork/exec. Keep it open after session
        // drop: success below proves Drop called unlock rather than relying on
        // the final descriptor close.
        let inherited = first.lock.try_clone().unwrap();
        drop(first);
        let second = FsSession::acquire_with_control(
            &path,
            false,
            kio_core::control::ControlHandle::default(),
        )
        .unwrap();
        drop(second);
        drop(inherited);
    }

    #[test]
    fn actual_sqlite_connection_is_opened_and_finished_under_the_session() {
        let temp = private_tempdir();
        let path = db_path(temp.path());
        let session = FsSession::acquire_with_control(
            &path,
            true,
            kio_core::control::ControlHandle::default(),
        )
        .unwrap();
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

        let session = FsSession::acquire_with_control(
            &inherited,
            true,
            kio_core::control::ControlHandle::default(),
        )
        .unwrap();
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
        assert!(
            FsSession::acquire_with_control(
                &parent.join("ledger.db"),
                false,
                kio_core::control::ControlHandle::default()
            )
            .is_err()
        );

        fs::remove_file(parent.join(LIFECYCLE_LOCK)).unwrap();
        fs::hard_link(&target, parent.join(LIFECYCLE_LOCK)).unwrap();
        assert!(
            FsSession::acquire_with_control(
                &parent.join("ledger.db"),
                false,
                kio_core::control::ControlHandle::default()
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_public_parent_and_lock_permissions() {
        let temp = private_tempdir();
        let path = db_path(temp.path());
        {
            let session = FsSession::acquire_with_control(
                &path,
                true,
                kio_core::control::ControlHandle::default(),
            )
            .unwrap();
            drop(session);
        }
        let parent = path.parent().unwrap();
        fs::set_permissions(parent, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            FsSession::acquire_with_control(
                &path,
                false,
                kio_core::control::ControlHandle::default()
            )
            .is_err()
        );
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(
            parent.join(LIFECYCLE_LOCK),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(
            FsSession::acquire_with_control(
                &path,
                false,
                kio_core::control::ControlHandle::default()
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn st01_ledger_accepts_external_writable_ancestor_without_repair() {
        let temp = private_tempdir();
        let ancestor = temp.path().join("external");
        fs::create_dir(&ancestor).unwrap();
        fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o777)).unwrap();
        let parent = ancestor.join("new/device");
        let path = parent.join("ledger.db");
        let control = kio_core::control::ControlHandle::default();
        let session = FsSession::acquire_with_control(&path, true, control.clone()).unwrap();
        assert_eq!(
            fs::metadata(&ancestor).unwrap().permissions().mode() & 0o777,
            0o777
        );
        assert_eq!(
            fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
            0o700
        );
        drop(session);
        let reopened = FsSession::acquire_with_control(&path, false, control).unwrap();
        drop(reopened);
        assert_eq!(
            fs::metadata(&ancestor).unwrap().permissions().mode() & 0o777,
            0o777
        );
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

        let error = FsSession::acquire_with_control(
            &path,
            true,
            kio_core::control::ControlHandle::default(),
        )
        .unwrap_err();
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
        let session = FsSession::acquire_with_control(
            &path,
            true,
            kio_core::control::ControlHandle::default(),
        )
        .unwrap();
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
        let session = FsSession::acquire_with_control(
            &path,
            true,
            kio_core::control::ControlHandle::default(),
        )
        .unwrap();
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
