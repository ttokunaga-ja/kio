use super::super::unix::watch_queue::{open_queue_connection, validate_private_queue_path};
use crate::watch::WatchError;
use kio_core::store_dir::{Publication, StoreDirectory};
use rusqlite::Connection;
use std::fs::File;
use std::path::{Component, Path, PathBuf};

#[derive(Debug)]
pub(crate) struct WatchQueueStorage {
    connection: Connection,
    _exclusive_lock: File,
    // SQLite's proc-fd path stays valid until after the connection and lock close.
    retained_parent: Option<RetainedParent>,
}
#[derive(Debug)]
struct RetainedParent {
    _handle: File,
    directory: StoreDirectory,
    leaves: RetainedQueueLeaves,
}
impl WatchQueueStorage {
    pub(crate) fn open_private(path: &Path) -> Result<Self, WatchError> {
        validate_private_queue_path(path)?;
        let (connection, exclusive_lock) = open_queue_connection(path, || Ok(()))?;
        Ok(Self {
            connection,
            _exclusive_lock: exclusive_lock,
            retained_parent: None,
        })
    }
    pub(crate) fn open_private_at(
        directory: &StoreDirectory,
        leaf: &Path,
    ) -> Result<Self, WatchError> {
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
        let (connection, exclusive_lock) =
            open_queue_connection(&bridge, || verify_retained_queue_leaves(directory, &leaves))?;
        Ok(Self {
            connection,
            _exclusive_lock: exclusive_lock,
            retained_parent: Some(RetainedParent {
                _handle: parent,
                directory: directory.clone(),
                leaves,
            }),
        })
    }
    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }
    pub(crate) fn connection_mut(&mut self) -> &mut Connection {
        &mut self.connection
    }
    pub(crate) fn revalidate(&self) -> Result<(), WatchError> {
        if let Some(parent) = &self.retained_parent {
            verify_retained_queue_leaves(&parent.directory, &parent.leaves)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RetainedLeafIdentity {
    device: u64,
    inode: u64,
}

#[derive(Debug)]
struct RetainedQueueLeaves(Vec<(PathBuf, RetainedLeafIdentity)>);

/// Create every SQLite pathname that this queue can use with owner-only mode
/// before SQLite is allowed to open it. All operations below use the retained
/// directory capability, so an ambient replacement of its diagnostic path
/// cannot select a different namespace.
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

fn retained_queue_error(error: kio_core::KioError) -> WatchError {
    WatchError::invariant(format!("watch queue private leaf is unsafe: {error}"))
}

pub(crate) fn validate_watch_queue_directory(
    _directory: &StoreDirectory,
) -> Result<(), WatchError> {
    Ok(())
}
