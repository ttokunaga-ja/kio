use crate::watch::WatchError;
use rusqlite::Connection;
use std::fs::{self, File, OpenOptions};
use std::path::Path;

pub(crate) fn validate_private_queue_path(path: &Path) -> Result<(), WatchError> {
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
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(WatchError::io)?;
        }
    }
    Ok(())
}

pub(crate) fn open_queue_connection<F>(
    path: &Path,
    revalidate: F,
) -> Result<(Connection, File), WatchError>
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
    Ok((connection, exclusive_lock))
}
