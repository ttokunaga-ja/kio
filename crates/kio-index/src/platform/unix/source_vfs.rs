//! Descriptor-bound source SQLite VFS registration.
use crate::{IndexError, Result};
use std::ffi::CStr;
use std::sync::{Once, OnceLock};
const BOUND_SOURCE_VFS_NAME: &CStr = c"kio-bound-source-unix";
static BOUND_SOURCE_VFS_INIT: Once = Once::new();
static BOUND_SOURCE_VFS_RESULT: OnceLock<std::result::Result<(), String>> = OnceLock::new();
pub(crate) static BOUND_SOURCE_DEFAULT_VFS: OnceLock<usize> = OnceLock::new();

pub(crate) fn ensure_source_vfs_registered() -> Result<()> {
    BOUND_SOURCE_VFS_INIT.call_once(|| {
        let result = unsafe {
            let original = rusqlite::ffi::sqlite3_vfs_find(std::ptr::null());
            if original.is_null() {
                Err("SQLite has no default VFS".to_owned())
            } else {
                let _ = BOUND_SOURCE_DEFAULT_VFS.set(original as usize);
                let mut wrapped = Box::new(*original);
                wrapped.zName = BOUND_SOURCE_VFS_NAME.as_ptr();
                wrapped.xOpen = Some(crate::platform::source_vfs::bound_source_x_open);
                wrapped.xFullPathname = Some(bound_source_x_full_pathname);
                let code = rusqlite::ffi::sqlite3_vfs_register(Box::into_raw(wrapped), 0);
                if code == rusqlite::ffi::SQLITE_OK {
                    Ok(())
                } else {
                    Err(format!(
                        "register bound source SQLite VFS: SQLite error {code}"
                    ))
                }
            }
        };
        let _ = BOUND_SOURCE_VFS_RESULT.set(result);
    });
    BOUND_SOURCE_VFS_RESULT
        .get()
        .expect("VFS initializer sets result")
        .as_ref()
        .map_err(|e| IndexError::Schema(e.clone()))?;
    Ok(())
}

pub(crate) unsafe extern "C" fn bound_source_x_full_pathname(
    _: *mut rusqlite::ffi::sqlite3_vfs,
    name: *const std::ffi::c_char,
    output_len: std::ffi::c_int,
    output: *mut std::ffi::c_char,
) -> std::ffi::c_int {
    if name.is_null() || output.is_null() || output_len <= 0 {
        return rusqlite::ffi::SQLITE_CANTOPEN;
    }
    let bytes = unsafe { CStr::from_ptr(name).to_bytes() };
    if crate::platform::source_vfs::source_internal_name_allowed(bytes) {
        if bytes.len() + 1 > output_len as usize {
            return rusqlite::ffi::SQLITE_CANTOPEN;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr().cast::<std::ffi::c_char>(),
                output,
                bytes.len(),
            );
            *output.add(bytes.len()) = 0;
        }
        return rusqlite::ffi::SQLITE_OK;
    }
    let Some(default_vfs) = BOUND_SOURCE_DEFAULT_VFS.get() else {
        return rusqlite::ffi::SQLITE_CANTOPEN;
    };
    let default_vfs = *default_vfs as *mut rusqlite::ffi::sqlite3_vfs;
    let Some(callback) = (unsafe { (*default_vfs).xFullPathname }) else {
        return rusqlite::ffi::SQLITE_CANTOPEN;
    };
    unsafe { callback(default_vfs, name, output_len, output) }
}

pub(crate) fn is_bound_source_fd_name(value: &[u8]) -> bool {
    value
        .strip_prefix(b"/dev/fd/")
        .is_some_and(|fd| !fd.is_empty() && fd.iter().all(u8::is_ascii_digit))
}
