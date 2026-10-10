use super::unix_cache::{
    BOUND_CACHE_DEFAULT_VFS, BoundCache, initialize_bound_cache_vfs, is_bound_cache_fd_name,
};
use super::unix_cache::{
    BoundCacheParent, bind_cache_parent_unchecked, cache_file_identity,
    open_or_create_ambient_cache_parent,
};
use crate::Result;
use rusqlite::{Connection, OpenFlags};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
fn validate_linux_cache_path_lexical(path: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let raw = path.as_os_str().as_bytes();
    if raw.windows(2).any(|window| window == b"//")
        || (raw.len() > 1 && raw.ends_with(b"/"))
        || raw
            .split(|byte| *byte == b'/')
            .any(|component| component == b"." || component == b"..")
    {
        return Err(crate::IndexError::Schema(format!(
            "aggregator cache path is not canonical (lexical): {}",
            path.display()
        )));
    }
    Ok(())
}
pub(super) fn bind_cache_parent(
    path: &Path,
    create_parent: bool,
    control: &kio_core::control::ControlHandle,
) -> Result<BoundCacheParent> {
    validate_linux_cache_path_lexical(path)?;
    bind_cache_parent_unchecked(path, create_parent, control)
}
pub(super) fn open_or_create_cache_parent(
    path: &Path,
    create_missing: bool,
    control: &kio_core::control::ControlHandle,
) -> Result<(std::fs::File, PathBuf)> {
    if let Some(bound) = open_or_create_inherited_cache_parent(path, create_missing, control)? {
        return Ok(bound);
    }
    open_or_create_ambient_cache_parent(path, create_missing, control)
}
fn open_or_create_inherited_cache_parent(
    path: &Path,
    create_missing: bool,
    control: &kio_core::control::ControlHandle,
) -> Result<Option<(std::fs::File, std::path::PathBuf)>> {
    let Some((mut current, suffix)) =
        kio_core::private_fs::resolve_inherited_private_root_with_control(path, control).map_err(
            |error| {
                crate::IndexError::Schema(format!(
                    "resolve inherited aggregator cache capability: {error}"
                ))
            },
        )?
    else {
        return Ok(None);
    };
    let mut logical = current.path().to_path_buf();
    for component in suffix {
        let leaf = Path::new(&component);
        logical.push(&component);
        let next = if current.contains_entry(leaf).map_err(|error| {
            crate::IndexError::Schema(format!(
                "inspect inherited aggregator cache directory {}: {error}",
                logical.display()
            ))
        })? {
            current.open_directory(leaf).map_err(|error| {
                crate::IndexError::Schema(format!(
                    "open inherited aggregator cache directory {} without following links: {error}",
                    logical.display()
                ))
            })?
        } else if create_missing {
            current.create_directory(leaf).map_err(|error| {
                crate::IndexError::Schema(format!(
                    "create inherited private aggregator cache directory {}: {error}",
                    logical.display()
                ))
            })?
        } else {
            return Err(crate::IndexError::Schema(format!(
                "inherited aggregator cache directory is missing: {}",
                logical.display()
            )));
        };
        current = kio_core::store_dir::StoreDirectory::from_retained_with_control(
            next,
            logical.clone(),
            control.clone(),
        )
        .map_err(|error| {
            crate::IndexError::Schema(format!(
                "retain inherited aggregator cache directory {}: {error}",
                logical.display()
            ))
        })?;
        verify_private_cache_directory(current.root_handle().as_ref(), &logical)?;
    }
    let retained = current.root_handle().try_clone().map_err(|error| {
        crate::IndexError::Schema(format!(
            "retain inherited aggregator cache parent {}: {error}",
            path.display()
        ))
    })?;
    Ok(Some((retained, path.to_path_buf())))
}
fn verify_private_cache_directory(file: &std::fs::File, path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata().map_err(|error| {
        crate::IndexError::Schema(format!(
            "inspect inherited aggregator cache directory {}: {error}",
            path.display()
        ))
    })?;
    // SAFETY: geteuid has no arguments and no memory-safety preconditions.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(crate::IndexError::Schema(format!(
            "inherited aggregator cache directory is not owner-private: {}",
            path.display()
        )));
    }
    Ok(())
}
fn is_bound_cache_linux_parent_fd_name(value: &[u8]) -> bool {
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
fn linux_regular_fd_inventory() -> Result<BTreeMap<i32, (u64, u64)>> {
    use std::os::unix::fs::MetadataExt;

    let entries = std::fs::read_dir("/proc/self/fd").map_err(|e| {
        crate::IndexError::Schema(format!("inspect process descriptor inventory: {e}"))
    })?;
    let mut result = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|e| {
            crate::IndexError::Schema(format!("read process descriptor inventory entry: {e}"))
        })?;
        let Ok(fd) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Ok(metadata) = std::fs::metadata(entry.path()) else {
            continue;
        };
        if metadata.is_file() {
            result.insert(fd, (metadata.dev(), metadata.ino()));
        }
    }
    Ok(result)
}
static BOUND_CACHE_LINUX_OPEN: Mutex<()> = Mutex::new(());

std::thread_local! {
    static BOUND_CACHE_LINUX_EXPECTED: Cell<Option<(u64, u64)>> = const { Cell::new(None) };
}

pub(super) fn cache_internal_name_allowed(bytes: &[u8]) -> bool {
    is_bound_cache_fd_name(bytes) || is_bound_cache_linux_parent_fd_name(bytes)
}
pub(super) fn cache_sqlite_path(cache: &BoundCache) -> PathBuf {
    use std::os::fd::AsRawFd;

    let leaf = cache
        .public_path
        .file_name()
        .expect("bound cache has a file name");
    PathBuf::from(format!("/proc/self/fd/{}", cache._parent.as_raw_fd())).join(leaf)
}
pub(crate) fn open_bound_cache_connection(
    cache: &BoundCache,
    flags: OpenFlags,
) -> Result<Connection> {
    initialize_bound_cache_vfs()?;
    let path = cache_sqlite_path(cache);

    {
        use std::os::unix::ffi::OsStrExt;

        if !is_bound_cache_linux_parent_fd_name(path.as_os_str().as_bytes()) {
            return Err(crate::IndexError::Schema(
                "bound cache SQLite path is not an internal descriptor-root path".to_owned(),
            ));
        }
        let _open_guard = BOUND_CACHE_LINUX_OPEN.lock().map_err(|_| {
            crate::IndexError::Schema("bound cache SQLite open mutex is poisoned".to_owned())
        })?;
        let expected = cache_file_identity(&cache.file)?;
        BOUND_CACHE_LINUX_EXPECTED.with(|slot| slot.set(Some(expected)));
        let outcome = Connection::open_with_flags_and_vfs(&path, flags, "kio-bound-cache-unix");
        BOUND_CACHE_LINUX_EXPECTED.with(|slot| slot.set(None));
        Ok(outcome?)
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

    if !name.is_null()
        && is_bound_cache_linux_parent_fd_name(unsafe { CStr::from_ptr(name).to_bytes() })
        && flags & rusqlite::ffi::SQLITE_OPEN_MAIN_DB != 0
    {
        let expected = BOUND_CACHE_LINUX_EXPECTED.with(Cell::get);
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
            // SQLite must add exactly one descriptor for the retained cache
            // inode. Other trusted in-process opens cannot grant authority
            // over this cache; a second matching descriptor is fail-closed.
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
