use super::unix_cache::{
    BOUND_CACHE_DEFAULT_VFS, BoundCache, initialize_bound_cache_vfs, is_bound_cache_fd_name,
};
use crate::Result;
use crate::platform::BoundSqliteConnection;
use rusqlite::OpenFlags;
use std::path::PathBuf;
pub(super) fn cache_internal_name_allowed(bytes: &[u8]) -> bool {
    is_bound_cache_fd_name(bytes)
}
pub(super) fn cache_sqlite_path(cache: &BoundCache) -> PathBuf {
    use std::os::fd::AsRawFd;
    PathBuf::from(format!("/dev/fd/{}", cache.file.as_raw_fd()))
}
pub(crate) fn open_bound_cache_connection(
    cache: &BoundCache,
    flags: OpenFlags,
) -> Result<BoundSqliteConnection> {
    initialize_bound_cache_vfs()?;
    let path = cache_sqlite_path(cache);

    {
        use std::os::unix::ffi::OsStrExt;

        if !is_bound_cache_fd_name(path.as_os_str().as_bytes()) {
            return Err(crate::IndexError::Schema(
                "bound cache SQLite path is not an internal descriptor path".to_owned(),
            ));
        }
        BoundSqliteConnection::open::<()>(&path, flags, Some("kio-bound-cache-unix"), None)
    }
}
pub(super) unsafe extern "C" fn bound_cache_x_open(
    _: *mut rusqlite::ffi::sqlite3_vfs,
    name: rusqlite::ffi::sqlite3_filename,
    file: *mut rusqlite::ffi::sqlite3_file,
    flags: std::ffi::c_int,
    out_flags: *mut std::ffi::c_int,
) -> std::ffi::c_int {
    let Some(default_vfs) = BOUND_CACHE_DEFAULT_VFS.get() else {
        return rusqlite::ffi::SQLITE_CANTOPEN;
    };
    let default_vfs = *default_vfs as *mut rusqlite::ffi::sqlite3_vfs;
    let Some(callback) = (unsafe { (*default_vfs).xOpen }) else {
        return rusqlite::ffi::SQLITE_CANTOPEN;
    };

    unsafe { callback(default_vfs, name, file, flags, out_flags) }
}
