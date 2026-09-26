//! Durable, rebuildable dirty-work queue for the watcher.
//!
//! A queue entry is deliberately a request to re-check a registered root, not
//! evidence that the path in an OS event is still present or authorized.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kio_core::durability::{DurabilityPoint, checkpoint};
#[cfg(target_os = "linux")]
use kio_core::store_dir::{Publication, StoreDirectory};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{DirtyReason, WatchError, WatchRoot};

pub const QUEUE_CAPACITY: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirtyWork {
    pub root: WatchRoot,
    pub reason: DirtyReason,
    pub changed_paths: Vec<PathBuf>,
    pub full_scan: bool,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub work: DirtyWork,
    sequence: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    Ignored,
    Queued,
    CollapsedToFullScan,
}

#[derive(Debug)]
pub struct DirtyQueue {
    connection: Connection,
    _exclusive_lock: File,
    // Linux SQLite paths below `/proc/self/fd/<fd>` remain meaningful only
    // while this duplicated owner-private parent capability stays open.
    _retained_parent: Option<File>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredPaths(Vec<String>);

impl DirtyQueue {
    /// Opens a queue only below an existing owner-private directory. The caller
    /// still owns selecting this device-private location; a root path or an
    /// event path must never be used as this database path.
    pub fn open_private(path: &Path) -> Result<Self, WatchError> {
        validate_private_queue_path(path)?;
        Self::open_private_inner(path, None, || Ok(()))
    }

    /// Open SQLite below an already-verified, retained private parent without
    /// reopening its ambient pathname. Linux is the only supported bridge for
    /// SQLite's pathname-only API; callers on other platforms retain the
    /// ordinary path admission contract.
    #[cfg(target_os = "linux")]
    pub fn open_private_at(directory: &StoreDirectory, leaf: &Path) -> Result<Self, WatchError> {
        if leaf.components().count() != 1
            || !matches!(leaf.components().next(), Some(Component::Normal(_)))
        {
            return Err(WatchError::invariant(
                "watch queue leaf must be one normal relative component",
            ));
        }
        kio_core::private_fs::verify_private_directory_handle(directory).map_err(|error| {
            WatchError::invariant(format!("watch queue private parent is unsafe: {error}"))
        })?;
        use std::os::fd::AsRawFd;
        let parent = directory
            .root_handle()
            .as_ref()
            .try_clone()
            .map_err(WatchError::io)?;
        let bridge = PathBuf::from(format!(
            "/proc/self/fd/{}/{}",
            parent.as_raw_fd(),
            leaf.to_string_lossy()
        ));
        let leaves = prepare_retained_queue_leaves(directory, leaf)?;
        Self::open_private_inner(&bridge, Some(parent), || {
            verify_retained_queue_leaves(directory, &leaves)
        })
    }

    fn open_private_inner<F>(
        path: &Path,
        retained_parent: Option<File>,
        revalidate: F,
    ) -> Result<Self, WatchError>
    where
        F: Fn() -> Result<(), WatchError>,
    {
        let lock_path = path.with_extension(format!(
            "{}.lock",
            path.extension()
                .and_then(|value| value.to_str())
                .unwrap_or("queue")
        ));
        let exclusive_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(WatchError::io)?;
        revalidate()?;
        exclusive_lock.try_lock().map_err(|error| {
            WatchError::invariant(format!(
                "watch queue is already owned by another process: {error}"
            ))
        })?;
        let connection = Connection::open(path).map_err(WatchError::sqlite)?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(WatchError::sqlite)?;
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(WatchError::sqlite)?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS watch_dirty_queue (
                    root TEXT PRIMARY KEY NOT NULL,
                    scope_id TEXT NOT NULL,
                    root_generation INTEGER NOT NULL,
                    event_generation INTEGER NOT NULL,
                    sequence INTEGER NOT NULL,
                    reason TEXT NOT NULL,
                    paths_json TEXT NOT NULL,
                    full_scan INTEGER NOT NULL,
                    first_dirty_ms INTEGER NOT NULL,
                    updated_ms INTEGER NOT NULL,
                    retry_count INTEGER NOT NULL DEFAULT 0,
                    not_before_ms INTEGER NOT NULL DEFAULT 0,
                    claimed_until_ms INTEGER,
                    claim_generation INTEGER
                 );
                 CREATE INDEX IF NOT EXISTS watch_dirty_queue_ready
                    ON watch_dirty_queue(claimed_until_ms, updated_ms);",
            )
            .map_err(WatchError::sqlite)?;
        // A stopped process cannot retain a lease. Restart deliberately queues
        // startup full reconciliation separately, but releasing leases makes
        // any durable work visible before that scan too.
        connection
            .execute(
                "UPDATE watch_dirty_queue SET claimed_until_ms = NULL, claim_generation = NULL",
                [],
            )
            .map_err(WatchError::sqlite)?;
        revalidate()?;
        Ok(Self {
            connection,
            _exclusive_lock: exclusive_lock,
            _retained_parent: retained_parent,
        })
    }

    pub fn enqueue(
        &mut self,
        root: &WatchRoot,
        reason: DirtyReason,
        paths: impl IntoIterator<Item = PathBuf>,
    ) -> Result<EnqueueOutcome, WatchError> {
        let mut now = now_ms()?;
        let root_key = root_key(root);
        let transaction = self.connection.transaction().map_err(WatchError::sqlite)?;
        let existing = transaction
            .query_row(
                "SELECT paths_json, full_scan, first_dirty_ms, sequence, event_generation, updated_ms FROM watch_dirty_queue WHERE root = ?1",
                [&root_key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?, row.get::<_, i64>(3)?, row.get::<_, i64>(4)?, row.get::<_, i64>(5)?)),
            )
            .optional()
            .map_err(WatchError::sqlite)?;
        let mut full_scan = reason.requires_full_scan();
        let mut merged = BTreeSet::new();
        let (first_dirty_ms, sequence, event_generation) =
            if let Some((stored, existing_full, first, sequence, event_generation, updated)) =
                existing
            {
                if now < updated {
                    now = updated;
                    full_scan = true;
                }
                full_scan |= existing_full != 0;
                if !full_scan {
                    merge_paths(&mut merged, decode_paths(&stored)?);
                }
                (
                    first,
                    sequence
                        .checked_add(1)
                        .ok_or_else(|| WatchError::invariant("watch queue sequence overflow"))?,
                    event_generation
                        .checked_add(1)
                        .ok_or_else(|| WatchError::invariant("watch event generation overflow"))?,
                )
            } else {
                (now, 1, 1)
            };
        if !full_scan {
            merge_paths(
                &mut merged,
                paths
                    .into_iter()
                    .map(normalize_relative)
                    .collect::<Result<Vec<_>, _>>()?,
            );
            if merged.iter().any(|path| path.as_os_str().is_empty())
                || merged.len() > QUEUE_CAPACITY
            {
                full_scan = true;
                merged.clear();
            }
        }
        let paths_json = encode_paths(&merged)?;
        transaction.execute(
            "INSERT INTO watch_dirty_queue
              (root, scope_id, root_generation, event_generation, sequence, reason, paths_json, full_scan, first_dirty_ms, updated_ms, claimed_until_ms, claim_generation)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, NULL)
             ON CONFLICT(root) DO UPDATE SET
               scope_id = excluded.scope_id, root_generation = excluded.root_generation,
               event_generation = excluded.event_generation, sequence = excluded.sequence,
               reason = excluded.reason, paths_json = excluded.paths_json, full_scan = excluded.full_scan,
               first_dirty_ms = excluded.first_dirty_ms, updated_ms = excluded.updated_ms,
               retry_count = 0, not_before_ms = 0, claimed_until_ms = NULL, claim_generation = NULL",
            params![root_key, root.scope_id, root.generation as i64, event_generation, sequence, reason.as_str(), paths_json, if full_scan { 1_i64 } else { 0_i64 }, first_dirty_ms, now],
        ).map_err(WatchError::sqlite)?;
        transaction.commit().map_err(WatchError::sqlite)?;
        Ok(if full_scan {
            EnqueueOutcome::CollapsedToFullScan
        } else {
            EnqueueOutcome::Queued
        })
    }

    pub fn enqueue_full(
        &mut self,
        root: &WatchRoot,
        reason: DirtyReason,
    ) -> Result<EnqueueOutcome, WatchError> {
        self.enqueue(root, reason, [])
    }

    pub fn pending_count(&self) -> Result<usize, WatchError> {
        let count: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM watch_dirty_queue", [], |row| {
                row.get(0)
            })
            .map_err(WatchError::sqlite)?;
        usize::try_from(count).map_err(|_| WatchError::invariant("negative watch queue count"))
    }

    pub fn claim_ready(
        &mut self,
        now: SystemTime,
        debounce: Duration,
        max_wait: Duration,
        lease: Duration,
    ) -> Result<Option<Claim>, WatchError> {
        let now = millis(now)?;
        let transaction = self.connection.transaction().map_err(WatchError::sqlite)?;
        let row = transaction.query_row(
            "SELECT root, scope_id, root_generation, event_generation, sequence, reason, paths_json, full_scan, updated_ms, first_dirty_ms
             FROM watch_dirty_queue WHERE (claimed_until_ms IS NULL OR claimed_until_ms <= ?1)
                AND not_before_ms <= ?1
             ORDER BY first_dirty_ms LIMIT 1", [now],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?, row.get::<_, i64>(3)?, row.get::<_, i64>(4)?, row.get::<_, String>(5)?, row.get::<_, String>(6)?, row.get::<_, i64>(7)?, row.get::<_, i64>(8)?, row.get::<_, i64>(9)?)),
        ).optional().map_err(WatchError::sqlite)?;
        let Some((
            root,
            scope_id,
            root_generation,
            event_generation,
            sequence,
            reason,
            paths,
            full_scan,
            updated,
            first,
        )) = row
        else {
            return Ok(None);
        };
        let debounce_ms = duration_ms(debounce)?;
        let max_wait_ms = duration_ms(max_wait)?;
        if full_scan == 0
            && now.saturating_sub(updated) < debounce_ms
            && now.saturating_sub(first) < max_wait_ms
        {
            return Ok(None);
        }
        let lease_until = now
            .checked_add(duration_ms(lease)?)
            .ok_or_else(|| WatchError::invariant("watch lease overflow"))?;
        let changed = transaction.execute("UPDATE watch_dirty_queue SET claimed_until_ms = ?1, claim_generation = ?2 WHERE root = ?3 AND sequence = ?4", params![lease_until, event_generation, root, sequence]).map_err(WatchError::sqlite)?;
        if changed != 1 {
            return Err(WatchError::invariant(
                "watch queue claim changed concurrently",
            ));
        }
        transaction.commit().map_err(WatchError::sqlite)?;
        checkpoint(DurabilityPoint::QueueClaimed)
            .map_err(|error| WatchError::invariant(error.to_string()))?;
        Ok(Some(Claim {
            sequence,
            work: DirtyWork {
                root: WatchRoot {
                    scope_id,
                    canonical_root: PathBuf::from(root),
                    generation: u64::try_from(root_generation)
                        .map_err(|_| WatchError::invariant("negative root generation"))?,
                },
                reason: DirtyReason::parse(&reason)?,
                changed_paths: if full_scan != 0 {
                    Vec::new()
                } else {
                    decode_paths(&paths)?
                },
                full_scan: full_scan != 0,
                generation: u64::try_from(event_generation)
                    .map_err(|_| WatchError::invariant("negative event generation"))?,
            },
        }))
    }

    /// Clears only the exact claimed generation. A callback that received a
    /// newer event while reconciling therefore leaves the new work durable.
    pub fn complete(&mut self, claim: &Claim, complete: bool) -> Result<(), WatchError> {
        let root = root_key(&claim.work.root);
        let transaction = self.connection.transaction().map_err(WatchError::sqlite)?;
        if complete {
            transaction.execute("DELETE FROM watch_dirty_queue WHERE root = ?1 AND sequence = ?2 AND event_generation = ?3 AND claim_generation = ?3", params![root, claim.sequence, claim.work.generation as i64]).map_err(WatchError::sqlite)?;
        } else {
            let now = now_ms()?;
            transaction.execute("UPDATE watch_dirty_queue SET claimed_until_ms = NULL, claim_generation = NULL, retry_count = retry_count + 1, not_before_ms = ?1 + MIN(60000, 1000 * (1 << MIN(retry_count, 6))) WHERE root = ?2 AND sequence = ?3 AND event_generation = ?4", params![now, root, claim.sequence, claim.work.generation as i64]).map_err(WatchError::sqlite)?;
        }
        transaction.commit().map_err(WatchError::sqlite)?;
        if complete {
            checkpoint(DurabilityPoint::QueueCompleted)
                .map_err(|error| WatchError::invariant(error.to_string()))?;
        }
        Ok(())
    }

    pub fn discard(&mut self, claim: &Claim) -> Result<(), WatchError> {
        self.connection.execute("DELETE FROM watch_dirty_queue WHERE root = ?1 AND sequence = ?2 AND event_generation = ?3", params![root_key(&claim.work.root), claim.sequence, claim.work.generation as i64]).map_err(WatchError::sqlite)?;
        Ok(())
    }
}

fn root_key(root: &WatchRoot) -> String {
    root.canonical_root.to_string_lossy().into_owned()
}
fn encode_paths(paths: &BTreeSet<PathBuf>) -> Result<String, WatchError> {
    serde_json::to_string(&StoredPaths(
        paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
    ))
    .map_err(WatchError::json)
}
fn decode_paths(raw: &str) -> Result<Vec<PathBuf>, WatchError> {
    let StoredPaths(paths) = serde_json::from_str(raw).map_err(WatchError::json)?;
    paths
        .into_iter()
        .map(|p| normalize_relative(PathBuf::from(p)))
        .collect()
}
fn merge_paths(target: &mut BTreeSet<PathBuf>, paths: impl IntoIterator<Item = PathBuf>) {
    for path in paths {
        if target.iter().any(|old| path.starts_with(old)) {
            continue;
        }
        target.retain(|old| !old.starts_with(&path));
        target.insert(path);
    }
}
pub(crate) fn normalize_relative(path: PathBuf) -> Result<PathBuf, WatchError> {
    if path.as_os_str().is_empty() {
        return Ok(path);
    }
    if path.is_absolute()
        || path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(WatchError::invariant(
            "dirty path must be relative to its registered root",
        ));
    }
    Ok(path)
}
fn now_ms() -> Result<i64, WatchError> {
    millis(SystemTime::now())
}
fn millis(time: SystemTime) -> Result<i64, WatchError> {
    i64::try_from(
        time.duration_since(UNIX_EPOCH)
            .map_err(|_| WatchError::invariant("clock before unix epoch"))?
            .as_millis(),
    )
    .map_err(|_| WatchError::invariant("clock milliseconds overflow"))
}
fn duration_ms(duration: Duration) -> Result<i64, WatchError> {
    i64::try_from(duration.as_millis())
        .map_err(|_| WatchError::invariant("duration milliseconds overflow"))
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RetainedLeafIdentity {
    device: u64,
    inode: u64,
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct RetainedQueueLeaves(Vec<(PathBuf, RetainedLeafIdentity)>);

/// Create every SQLite pathname that this queue can use with owner-only mode
/// before SQLite is allowed to open it. All operations below use the retained
/// directory capability, so an ambient replacement of its diagnostic path
/// cannot select a different namespace.
#[cfg(target_os = "linux")]
fn prepare_retained_queue_leaves(
    directory: &StoreDirectory,
    queue_leaf: &Path,
) -> Result<RetainedQueueLeaves, WatchError> {
    let leaves = sqlite_leaf_names(queue_leaf)?;
    let mut identities = Vec::with_capacity(leaves.len());
    for leaf in leaves {
        if !directory
            .contains_entry(&leaf)
            .map_err(retained_queue_error)?
        {
            // A concurrent safe initializer may win the create-only race.
            // In either case, re-open below with no-follow semantics and
            // reject anything that is not an owner-private regular file.
            let _ = directory.write_atomic(&leaf, &[], Publication::CreateOnly);
        }
        directory
            .ensure_owner_private(&leaf)
            .map_err(retained_queue_error)?;
        identities.push((leaf.clone(), retained_leaf_identity(directory, &leaf)?));
    }
    Ok(RetainedQueueLeaves(identities))
}

#[cfg(target_os = "linux")]
fn verify_retained_queue_leaves(
    directory: &StoreDirectory,
    leaves: &RetainedQueueLeaves,
) -> Result<(), WatchError> {
    for (leaf, expected) in &leaves.0 {
        directory
            .ensure_owner_private(leaf)
            .map_err(retained_queue_error)?;
        if retained_leaf_identity(directory, leaf)? != *expected {
            return Err(WatchError::invariant(
                "watch queue leaf changed while SQLite was opened",
            ));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn retained_leaf_identity(
    directory: &StoreDirectory,
    leaf: &Path,
) -> Result<RetainedLeafIdentity, WatchError> {
    use std::os::unix::fs::MetadataExt;

    let file = directory
        .open_regular_read(leaf, u64::MAX)
        .map_err(retained_queue_error)?;
    let metadata = file.metadata().map_err(WatchError::io)?;
    Ok(RetainedLeafIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(target_os = "linux")]
fn sqlite_leaf_names(queue_leaf: &Path) -> Result<[PathBuf; 4], WatchError> {
    if queue_leaf.components().count() != 1
        || !matches!(queue_leaf.components().next(), Some(Component::Normal(_)))
    {
        return Err(WatchError::invariant(
            "watch queue leaf must be one normal relative component",
        ));
    }
    let queue = queue_leaf.to_path_buf();
    let lock = queue.with_extension(format!(
        "{}.lock",
        queue
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("queue")
    ));
    let name = queue_leaf
        .file_name()
        .expect("one normal queue component has a file name")
        .to_string_lossy();
    Ok([
        queue,
        lock,
        PathBuf::from(format!("{name}-wal")),
        PathBuf::from(format!("{name}-shm")),
    ])
}

#[cfg(target_os = "linux")]
fn retained_queue_error(error: kio_core::KioError) -> WatchError {
    WatchError::invariant(format!("watch queue private leaf is unsafe: {error}"))
}

fn validate_private_queue_path(path: &Path) -> Result<(), WatchError> {
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(WatchError::invariant(
            "watch queue path must be absolute and have a leaf",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| WatchError::invariant("watch queue path lacks a parent"))?;
    let meta = fs::symlink_metadata(parent).map_err(WatchError::io)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(WatchError::invariant(
            "watch queue parent must be a real private directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            return Err(WatchError::invariant(
                "watch queue parent is not owner-private",
            ));
        }
    }
    if path.exists() {
        let meta = fs::symlink_metadata(path).map_err(WatchError::io)?;
        if !meta.is_file() || meta.file_type().is_symlink() {
            return Err(WatchError::invariant("watch queue is not a regular file"));
        }
    } else {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(WatchError::io)?;
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(WatchError::io)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[cfg(target_os = "linux")]
    use kio_core::store_dir::StoreDirectory;

    #[cfg(target_os = "linux")]
    struct RemoveDirectoryOnDrop(PathBuf);

    #[cfg(target_os = "linux")]
    impl Drop for RemoveDirectoryOnDrop {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn private_dir() -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        dir
    }
    fn root() -> WatchRoot {
        WatchRoot {
            scope_id: "scope".into(),
            canonical_root: PathBuf::from("/private/root"),
            generation: 4,
        }
    }
    fn queue() -> DirtyQueue {
        let dir = private_dir();
        let path = dir.keep().join("queue.sqlite");
        DirtyQueue::open_private(&path).unwrap()
    }
    #[test]
    fn deduplicates_and_ancestor_covers_descendants() {
        let mut q = queue();
        q.enqueue(
            &root(),
            DirtyReason::Native,
            [PathBuf::from("a/b"), PathBuf::from("a")],
        )
        .unwrap();
        let c = q
            .claim_ready(
                SystemTime::now() + Duration::from_secs(3),
                Duration::ZERO,
                Duration::ZERO,
                Duration::from_secs(1),
            )
            .unwrap()
            .unwrap();
        assert_eq!(c.work.changed_paths, vec![PathBuf::from("a")]);
    }
    #[test]
    fn overflow_collapses_to_root_scan() {
        let mut q = queue();
        let paths = (0..=QUEUE_CAPACITY)
            .map(|i| PathBuf::from(format!("{i}")))
            .collect::<Vec<_>>();
        assert_eq!(
            q.enqueue(&root(), DirtyReason::Native, paths).unwrap(),
            EnqueueOutcome::CollapsedToFullScan
        );
        let c = q
            .claim_ready(
                SystemTime::now() + Duration::from_secs(3),
                Duration::ZERO,
                Duration::ZERO,
                Duration::from_secs(1),
            )
            .unwrap()
            .unwrap();
        assert!(c.work.full_scan);
    }
    #[test]
    fn newer_event_survives_completion() {
        let mut q = queue();
        q.enqueue(&root(), DirtyReason::Native, [PathBuf::from("a")])
            .unwrap();
        let c = q
            .claim_ready(
                SystemTime::now() + Duration::from_secs(3),
                Duration::ZERO,
                Duration::ZERO,
                Duration::from_secs(1),
            )
            .unwrap()
            .unwrap();
        q.enqueue(&root(), DirtyReason::Native, [PathBuf::from("b")])
            .unwrap();
        q.complete(&c, true).unwrap();
        assert_eq!(q.pending_count().unwrap(), 1);
    }
    #[test]
    fn restart_recovers_pending_work() {
        let dir = private_dir();
        let path = dir.path().join("queue.sqlite");
        {
            let mut q = DirtyQueue::open_private(&path).unwrap();
            q.enqueue_full(&root(), DirtyReason::Startup).unwrap();
        }
        let q = DirtyQueue::open_private(&path).unwrap();
        assert_eq!(q.pending_count().unwrap(), 1);
    }
    #[test]
    fn second_process_cannot_clear_a_live_lease() {
        let dir = private_dir();
        let path = dir.path().join("queue.sqlite");
        let _first = DirtyQueue::open_private(&path).unwrap();
        assert!(DirtyQueue::open_private(&path).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn retained_parent_survives_rename_and_fd_replacement() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::PermissionsExt;

        let original = private_dir();
        let replacement = private_dir();
        let moved = original.path().with_extension("retained");
        let _moved_cleanup = RemoveDirectoryOnDrop(moved.clone());
        let original_handle = File::open(original.path()).unwrap();
        let directory = StoreDirectory::from_retained(
            original_handle.try_clone().unwrap(),
            original.path().to_path_buf(),
        )
        .unwrap();
        let mut queue = DirtyQueue::open_private_at(&directory, Path::new("queue.sqlite")).unwrap();

        // The queue duplicated the verified parent capability. Replacing the
        // original directory name and its descriptor number must not redirect
        // SQLite or its lock into either attacker-controlled replacement.
        fs::rename(original.path(), &moved).unwrap();
        fs::create_dir(original.path()).unwrap();
        fs::set_permissions(original.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let replacement_handle = File::open(replacement.path()).unwrap();
        assert!(
            unsafe { libc::dup2(replacement_handle.as_raw_fd(), original_handle.as_raw_fd()) } >= 0
        );

        queue.enqueue_full(&root(), DirtyReason::Manual).unwrap();
        assert!(moved.join("queue.sqlite").is_file());
        assert!(moved.join("queue.sqlite.lock").is_file());
        assert!(!original.path().join("queue.sqlite").exists());
        assert!(!replacement.path().join("queue.sqlite").exists());

        drop(queue);
        drop(directory);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn retained_queue_refuses_sqlite_sidecar_symlinks_without_touching_victim() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        for leaf in [
            "queue.sqlite",
            "queue.sqlite.lock",
            "queue.sqlite-wal",
            "queue.sqlite-shm",
        ] {
            let state = private_dir();
            let victim = private_dir();
            let victim_file = victim.path().join("victim");
            fs::write(&victim_file, b"unchanged").unwrap();
            fs::set_permissions(&victim_file, fs::Permissions::from_mode(0o600)).unwrap();
            symlink(&victim_file, state.path().join(leaf)).unwrap();
            let directory = StoreDirectory::open(state.path()).unwrap();

            assert!(DirtyQueue::open_private_at(&directory, Path::new("queue.sqlite")).is_err());
            assert_eq!(fs::read(&victim_file).unwrap(), b"unchanged");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn retained_queue_precreates_owner_private_sqlite_files() {
        use std::os::unix::fs::MetadataExt;

        let state = private_dir();
        let directory = StoreDirectory::open(state.path()).unwrap();
        let _queue = DirtyQueue::open_private_at(&directory, Path::new("queue.sqlite")).unwrap();

        for leaf in [
            "queue.sqlite",
            "queue.sqlite.lock",
            "queue.sqlite-wal",
            "queue.sqlite-shm",
        ] {
            let metadata = fs::metadata(state.path().join(leaf)).unwrap();
            assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
            assert_eq!(metadata.mode() & 0o077, 0);
        }
    }
}
