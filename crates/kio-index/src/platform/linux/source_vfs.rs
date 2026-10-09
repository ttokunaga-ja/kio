//! linux source SQLite descriptor path and open verification.
use crate::platform::source::BoundSourceIndex;
use crate::platform::source::{SourceFileIdentity, source_file_identity};
use crate::platform::unix_source_vfs::{
    BOUND_SOURCE_DEFAULT_VFS, ensure_source_vfs_registered, is_bound_source_fd_name,
};
use crate::{IndexError, Result};
use rusqlite::{Connection, OpenFlags};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::ffi::CStr;
use std::path::PathBuf;
use std::sync::Mutex;
static BOUND_SOURCE_LINUX_OPEN: Mutex<()> = Mutex::new(());
std::thread_local! {
static BOUND_SOURCE_LINUX_EXPECTED: Cell<Option<SourceFileIdentity>> = const { Cell::new(None) };
}

fn sqlite_path(source: &BoundSourceIndex) -> PathBuf {
    use std::os::fd::AsRawFd;
    let leaf = source
        .public_path
        .file_name()
        .expect("bound source index has a file name");
    PathBuf::from(format!("/proc/self/fd/{}", source._parent.as_raw_fd())).join(leaf)
}

pub(crate) fn open_bound_source_connection(
    source: &BoundSourceIndex,
    flags: OpenFlags,
) -> Result<Connection> {
    let path = sqlite_path(source);

    ensure_source_vfs_registered()?;

    if !is_bound_source_linux_parent_fd_name(path.as_os_str().as_encoded_bytes()) {
        return Err(IndexError::Schema(
            "bound source SQLite path is not an internal descriptor-root path".to_owned(),
        ));
    }
    let _open_guard = BOUND_SOURCE_LINUX_OPEN
        .lock()
        .map_err(|_| IndexError::Schema("bound source SQLite open mutex is poisoned".to_owned()))?;
    let expected = source_file_identity(&source.file)?;
    BOUND_SOURCE_LINUX_EXPECTED.with(|slot| slot.set(Some(expected)));
    let outcome = Connection::open_with_flags_and_vfs(&path, flags, "kio-bound-source-unix");
    BOUND_SOURCE_LINUX_EXPECTED.with(|slot| slot.set(None));
    let conn = outcome?;
    Ok(conn)
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
    if !name.is_null()
        && is_bound_source_linux_parent_fd_name(unsafe { CStr::from_ptr(name).to_bytes() })
        && flags & rusqlite::ffi::SQLITE_OPEN_MAIN_DB != 0
    {
        let expected = BOUND_SOURCE_LINUX_EXPECTED.with(Cell::get);
        let Some(expected) = expected else {
            return rusqlite::ffi::SQLITE_CANTOPEN;
        };
        let Ok(before) = linux_regular_fd_inventory() else {
            return rusqlite::ffi::SQLITE_CANTOPEN;
        };
        let code = unsafe { callback(default_vfs, name, file, flags, out_flags) };
        if code != rusqlite::ffi::SQLITE_OK {
            return code;
        }
        let verified = linux_regular_fd_inventory().is_ok_and(|after| {
            let opened: Vec<_> = after
                .iter()
                .filter(|(fd, identity)| before.get(fd) != Some(*identity))
                .collect();
            // The main database must add exactly one descriptor for the
            // retained source. Concurrent same-process work may open other
            // regular files, which cannot grant authority over this source;
            // it is trusted in-process code. A second descriptor for this
            // identity is fail-closed, so another operation cannot obscure
            // which descriptor SQLite selected.
            opened
                .iter()
                .filter(|(_, identity)| **identity == expected)
                .count()
                == 1
        });
        if verified {
            return rusqlite::ffi::SQLITE_OK;
        }
        if !file.is_null() {
            let methods = unsafe { (*file).pMethods };
            if !methods.is_null()
                && let Some(close) = unsafe { (*methods).xClose }
            {
                let _ = unsafe { close(file) };
            }
        }
        return rusqlite::ffi::SQLITE_CANTOPEN;
    }

    unsafe { callback(default_vfs, name, file, flags, out_flags) }
}

pub(crate) fn is_bound_source_linux_parent_fd_name(value: &[u8]) -> bool {
    let Some(value) = value.strip_prefix(b"/proc/self/fd/") else {
        return false;
    };
    let Some(separator) = value.iter().position(|byte| *byte == b'/') else {
        return false;
    };
    let (fd, leaf_with_separator) = value.split_at(separator);
    let leaf = &leaf_with_separator[1..];
    !fd.is_empty()
        && fd.iter().all(u8::is_ascii_digit)
        && !leaf.is_empty()
        && !leaf.contains(&b'/')
        && leaf != b"."
        && leaf != b".."
}

pub(crate) fn linux_regular_fd_inventory() -> Result<BTreeMap<i32, SourceFileIdentity>> {
    use std::os::unix::fs::MetadataExt;

    let entries = std::fs::read_dir("/proc/self/fd")
        .map_err(|e| IndexError::Schema(format!("inspect process descriptor inventory: {e}")))?;
    let mut result = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|e| {
            IndexError::Schema(format!("read process descriptor inventory entry: {e}"))
        })?;
        let Ok(fd) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Ok(metadata) = std::fs::metadata(entry.path()) else {
            continue;
        };
        if metadata.is_file() {
            result.insert(
                fd,
                SourceFileIdentity {
                    dev: metadata.dev(),
                    ino: metadata.ino(),
                },
            );
        }
    }
    Ok(result)
}

pub(crate) fn source_internal_name_allowed(bytes: &[u8]) -> bool {
    is_bound_source_fd_name(bytes) || is_bound_source_linux_parent_fd_name(bytes)
}
