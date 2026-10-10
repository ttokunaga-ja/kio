use crate::watch::WatchError;
use kio_core::store_dir::StoreDirectory;
use rusqlite::Connection;
use std::fs::{self, File, OpenOptions};
use std::path::Path;

#[derive(Debug)]
pub(crate) struct WatchQueueStorage {
    connection: Connection,
    _exclusive_lock: File,
}
impl WatchQueueStorage {
    pub(crate) fn open_private(path: &Path) -> Result<Self, WatchError> {
        validate_private_queue_path(path)?;
        let (connection, exclusive_lock) = open_queue_connection(path)?;
        Ok(Self {
            connection,
            _exclusive_lock: exclusive_lock,
        })
    }
    pub(crate) fn open_private_at(
        directory: &StoreDirectory,
        leaf: &Path,
    ) -> Result<Self, WatchError> {
        validate_watch_queue_directory(directory)?;
        Self::open_private(&directory.path().join(leaf))
    }
    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }
    pub(crate) fn connection_mut(&mut self) -> &mut Connection {
        &mut self.connection
    }
    pub(crate) fn revalidate(&self) -> Result<(), WatchError> {
        Ok(())
    }
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
    }
    Ok(())
}

fn open_queue_connection(path: &Path) -> Result<(Connection, File), WatchError> {
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
    exclusive_lock.try_lock().map_err(|error| {
        WatchError::invariant(format!(
            "watch queue is already owned by another process: {error}"
        ))
    })?;
    let connection = Connection::open(path).map_err(WatchError::sqlite)?;
    Ok((connection, exclusive_lock))
}

pub(crate) fn validate_watch_queue_directory(directory: &StoreDirectory) -> Result<(), WatchError> {
    if kio_core::private_fs::resolve_inherited_private_root_with_control(
        directory.path(),
        directory.control(),
    )
    .map_err(|error| WatchError::invariant(error.to_string()))?
    .is_some()
    {
        return Err(WatchError::invariant(
            "descriptor-backed watch state queues are unsupported on this platform",
        ));
    }
    Ok(())
}
