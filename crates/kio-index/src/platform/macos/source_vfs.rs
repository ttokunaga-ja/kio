//! macos source SQLite descriptor path and open verification.
use crate::platform::source::BoundSourceIndex;
use crate::platform::unix_source_vfs::{
    BOUND_SOURCE_DEFAULT_VFS, ensure_source_vfs_registered, is_bound_source_fd_name,
};
use crate::{IndexError, Result};
use rusqlite::{Connection, OpenFlags};
use std::path::PathBuf;

fn sqlite_path(source: &BoundSourceIndex) -> PathBuf {
    use std::os::fd::AsRawFd;
    PathBuf::from(format!("/dev/fd/{}", source.file.as_raw_fd()))
}

pub(crate) fn open_bound_source_connection(
    source: &BoundSourceIndex,
    flags: OpenFlags,
) -> Result<Connection> {
    use std::os::unix::ffi::OsStrExt;

    let path = sqlite_path(source);
    if !is_bound_source_fd_name(path.as_os_str().as_bytes()) {
        return Err(IndexError::Schema(
            "bound source SQLite path is not an internal descriptor path".to_owned(),
        ));
    }
    ensure_source_vfs_registered()?;
    Ok(Connection::open_with_flags_and_vfs(
        &path,
        flags,
        "kio-bound-source-unix",
    )?)
}

pub(crate) unsafe extern "C" fn bound_source_x_open(
    _: *mut rusqlite::ffi::sqlite3_vfs,
    name: rusqlite::ffi::sqlite3_filename,
    file: *mut rusqlite::ffi::sqlite3_file,
    flags: std::ffi::c_int,
    out_flags: *mut std::ffi::c_int,
) -> std::ffi::c_int {
    let Some(default_vfs) = BOUND_SOURCE_DEFAULT_VFS.get() else {
        return rusqlite::ffi::SQLITE_CANTOPEN;
    };
    let default_vfs = *default_vfs as *mut rusqlite::ffi::sqlite3_vfs;
    let Some(callback) = (unsafe { (*default_vfs).xOpen }) else {
        return rusqlite::ffi::SQLITE_CANTOPEN;
    };

    unsafe { callback(default_vfs, name, file, flags, out_flags) }
}

pub(crate) fn source_internal_name_allowed(bytes: &[u8]) -> bool {
    is_bound_source_fd_name(bytes)
}
