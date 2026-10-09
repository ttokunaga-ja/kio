use super::super::unix::watch_queue::{open_queue_connection, validate_private_queue_path};
use crate::watch::WatchError;
use kio_core::store_dir::StoreDirectory;
use rusqlite::Connection;
use std::fs::File;
use std::path::Path;
#[derive(Debug)]
pub(crate) struct WatchQueueStorage {
    connection: Connection,
    _exclusive_lock: File,
}
impl WatchQueueStorage {
    pub(crate) fn open_private(path: &Path) -> Result<Self, WatchError> {
        validate_private_queue_path(path)?;
        let (connection, exclusive_lock) = open_queue_connection(path, || Ok(()))?;
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

pub(crate) fn validate_watch_queue_directory(directory: &StoreDirectory) -> Result<(), WatchError> {
    if kio_core::private_fs::resolve_inherited_private_root(directory.path())
        .map_err(|error| WatchError::invariant(error.to_string()))?
        .is_some()
    {
        return Err(WatchError::invariant(
            "descriptor-backed watch state queues are unsupported on this platform",
        ));
    }
    Ok(())
}
