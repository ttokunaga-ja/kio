use super::{bind_cache_parent, open_or_create_cache_parent};
#[cfg(target_os = "macos")]
use super::{bound_cache_x_open, cache_internal_name_allowed};
use crate::Result;
use cap_primitives::fs as cap_fs;
#[cfg(target_os = "macos")]
use std::ffi::CStr;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::sync::{Once, OnceLock};
/// A cache leaf and its parent directory retained as capabilities.  The parent
/// must stay alive: a public cache pathname can be redirected after opening it.
pub(crate) struct BoundCache {
    pub(super) _parent: std::fs::File,
    pub(super) file: std::fs::File,
    pub(super) public_path: PathBuf,
}

pub(super) struct BoundCacheParent {
    pub(super) parent: std::fs::File,
    pub(super) public_path: PathBuf,
}

impl BoundCache {
    pub(crate) fn file(&self) -> &std::fs::File {
        &self.file
    }
    pub(crate) fn public_path(&self) -> &Path {
        &self.public_path
    }
}
pub(super) fn bind_cache_parent_unchecked(
    path: &Path,
    create_parent: bool,
    control: &kio_core::control::ControlHandle,
) -> Result<BoundCacheParent> {
    let lexical_parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().ok_or_else(|| {
        crate::IndexError::Schema(format!(
            "aggregator cache path has no file name: {}",
            path.display()
        ))
    })?;
    let (parent, resolved_parent) =
        open_or_create_cache_parent(lexical_parent, create_parent, control)?;
    let retained = kio_core::store_dir::StoreDirectory::from_retained(
        parent
            .try_clone()
            .map_err(|error| crate::IndexError::Schema(error.to_string()))?,
        resolved_parent.clone(),
    )
    .map_err(|error| crate::IndexError::Schema(error.to_string()))?;
    kio_core::private_fs::verify_private_directory_handle(&retained)
        .map_err(|error| crate::IndexError::Schema(error.to_string()))?;
    Ok(BoundCacheParent {
        parent,
        public_path: resolved_parent.join(file_name),
    })
}

pub(super) fn open_or_create_ambient_cache_parent(
    path: &Path,
    create_missing: bool,
    control: &kio_core::control::ControlHandle,
) -> Result<(std::fs::File, std::path::PathBuf)> {
    let mut existing = path;
    while matches!(std::fs::symlink_metadata(existing), Err(ref e) if e.kind() == std::io::ErrorKind::NotFound)
    {
        existing = existing.parent().ok_or_else(|| {
            crate::IndexError::Schema(format!(
                "no existing aggregator cache ancestor for {}",
                path.display()
            ))
        })?;
    }
    let before = std::fs::symlink_metadata(existing).map_err(|e| {
        crate::IndexError::Schema(format!(
            "inspect aggregator cache ancestor {}: {e}",
            existing.display()
        ))
    })?;
    if !before.is_dir() || before.file_type().is_symlink() {
        return Err(crate::IndexError::Schema(format!(
            "aggregator cache ancestor must be a real directory: {}",
            existing.display()
        )));
    }

    let resolved_existing = std::fs::canonicalize(existing).map_err(|e| {
        crate::IndexError::Schema(format!(
            "resolve aggregator cache ancestor {}: {e}",
            existing.display()
        ))
    })?;
    let mut handle =
        cap_fs::open_ambient_dir(&resolved_existing, cap_primitives::ambient_authority()).map_err(
            |e| {
                crate::IndexError::Schema(format!(
                    "open aggregator cache ancestor {}: {e}",
                    resolved_existing.display()
                ))
            },
        )?;

    {
        use std::os::unix::fs::MetadataExt;
        let opened = handle.metadata().map_err(|e| {
            crate::IndexError::Schema(format!("inspect opened aggregator cache ancestor: {e}"))
        })?;
        if before.dev() != opened.dev() || before.ino() != opened.ino() {
            return Err(crate::IndexError::Schema(format!(
                "aggregator cache ancestor changed while opening: {}",
                existing.display()
            )));
        }
    }

    let mut resolved = resolved_existing;
    let remainder = path.strip_prefix(existing).map_err(|_| {
        crate::IndexError::Schema(format!(
            "derive cache path below ancestor: {}",
            path.display()
        ))
    })?;
    for component in remainder.components() {
        let std::path::Component::Normal(component) = component else {
            continue;
        };
        handle = match cap_fs::open_dir_nofollow(&handle, Path::new(component)) {
            Ok(child) => child,
            Err(_)
                if create_missing
                    && matches!(cap_fs::stat(&handle, Path::new(component), cap_fs::FollowSymlinks::No), Err(e) if e.kind() == std::io::ErrorKind::NotFound) =>
            {
                let parent = kio_core::store_dir::StoreDirectory::from_retained_with_control(
                    handle.try_clone().map_err(|error| {
                        crate::IndexError::Schema(format!("retain cache parent: {error}"))
                    })?,
                    resolved.clone(),
                    control.clone(),
                )
                .map_err(|error| crate::IndexError::Schema(error.to_string()))?;
                parent
                    .create_directory(Path::new(component))
                    .map_err(|error| {
                        crate::IndexError::Schema(format!(
                            "create private aggregator cache directory {}: {error}",
                            path.display()
                        ))
                    })?
            }
            Err(e) => {
                return Err(crate::IndexError::Schema(format!(
                    "open aggregator cache directory {} without following links: {e}",
                    path.display()
                )));
            }
        };
        resolved.push(component);
    }
    Ok((handle, resolved))
}

pub(super) fn cache_file_identity(file: &std::fs::File) -> Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file
        .metadata()
        .map_err(|e| crate::IndexError::Schema(format!("inspect opened aggregator cache: {e}")))?;
    Ok((metadata.dev(), metadata.ino()))
}

pub(crate) fn validate_bound_cache_file(file: &std::fs::File, path: &Path) -> Result<()> {
    kio_core::private_fs::verify_private_regular_file_handle(file)
        .map_err(|error| crate::IndexError::Schema(error.to_string()))?;
    let metadata = file.metadata().map_err(|e| {
        crate::IndexError::Schema(format!(
            "inspect opened aggregator cache {}: {e}",
            path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(crate::IndexError::Schema(format!(
            "aggregator cache target is not a regular file: {}",
            path.display()
        )));
    }

    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(crate::IndexError::Schema(format!(
                "aggregator cache target must have exactly one hard link (found {}): {}",
                metadata.nlink(),
                path.display()
            )));
        }
    }
    Ok(())
}

fn create_private_cache_file(parent_handle: &std::fs::File, path: &Path) -> Result<std::fs::File> {
    let leaf = path.file_name().expect("resolved cache has leaf");
    let retained = kio_core::store_dir::StoreDirectory::from_retained(
        parent_handle
            .try_clone()
            .map_err(|error| crate::IndexError::Schema(error.to_string()))?,
        path.parent().expect("cache parent").to_path_buf(),
    )
    .map_err(|error| crate::IndexError::Schema(error.to_string()))?;
    let file = retained
        .create_private_file(Path::new(leaf))
        .map_err(|error| crate::IndexError::Schema(error.to_string()))?;
    validate_bound_cache_file(&file, path)?;
    Ok(file)
}

pub(crate) fn bind_cache(
    path: &Path,
    create_missing: bool,
    control: &kio_core::control::ControlHandle,
) -> Result<BoundCache> {
    let BoundCacheParent {
        parent: parent_handle,
        public_path: path,
    } = bind_cache_parent(path, true, control)?;
    let leaf = path.file_name().expect("resolved cache has leaf");
    let before_leaf = cap_fs::stat(&parent_handle, Path::new(leaf), cap_fs::FollowSymlinks::No);
    if matches!(&before_leaf, Ok(metadata) if !metadata.is_file()) {
        return Err(crate::IndexError::Schema(format!(
            "aggregator cache target is not a regular file (possibly a symlink): {}",
            path.display()
        )));
    }
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);

    let file = if create_missing
        && matches!(&before_leaf, Err(error) if error.kind() == std::io::ErrorKind::NotFound)
    {
        create_private_cache_file(&parent_handle, &path)?
    } else {
        cap_fs::open(&parent_handle, Path::new(leaf), &options).map_err(|error| {
            crate::IndexError::Schema(format!("open aggregator cache {}: {error}", path.display()))
        })?
    };
    validate_bound_cache_file(&file, &path)?;

    if let Ok(before) = before_leaf {
        use cap_fs::MetadataExt;
        if (before.dev(), before.ino()) != cache_file_identity(&file)? {
            return Err(crate::IndexError::Schema(format!(
                "aggregator cache leaf changed while opening: {}",
                path.display()
            )));
        }
    }
    Ok(BoundCache {
        _parent: parent_handle,
        file,
        public_path: path,
    })
}

pub(crate) fn recreate_cache(
    path: &Path,
    control: &kio_core::control::ControlHandle,
) -> Result<BoundCache> {
    let BoundCacheParent {
        parent: parent_handle,
        public_path: path,
    } = bind_cache_parent(path, true, control)?;
    let leaf = path.file_name().expect("resolved cache has leaf");
    // Cap-relative deletion prevents a swapped public parent from becoming
    // the authority for any part of explicit recreation.
    for candidate in [
        leaf.to_os_string(),
        format!("{}-wal", leaf.to_string_lossy()).into(),
        format!("{}-shm", leaf.to_string_lossy()).into(),
    ] {
        let candidate = Path::new(&candidate);
        match cap_fs::stat(&parent_handle, candidate, cap_fs::FollowSymlinks::No) {
            Ok(metadata) if metadata.is_file() => {
                use cap_fs::MetadataExt;
                if metadata.nlink() != 1 {
                    return Err(crate::IndexError::Schema(format!(
                        "aggregator cache target must have exactly one hard link: {}",
                        candidate.display()
                    )));
                }
            }
            Ok(_) => {
                return Err(crate::IndexError::Schema(format!(
                    "refusing to recreate aggregator cache through symlink or non-file: {}",
                    candidate.display()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(crate::IndexError::Schema(format!(
                    "inspect aggregator cache {}: {e}",
                    candidate.display()
                )));
            }
        }
        let mut options = cap_fs::OpenOptions::new();
        options
            .read(true)
            ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
        let file = cap_fs::open(&parent_handle, candidate, &options)
            .map_err(|error| crate::IndexError::Schema(error.to_string()))?;
        validate_bound_cache_file(&file, candidate)?;
        drop(file);
        cap_fs::remove_file(&parent_handle, candidate).map_err(|e| {
            crate::IndexError::Schema(format!(
                "remove aggregator cache {}: {e}",
                candidate.display()
            ))
        })?;
    }
    let file = create_private_cache_file(&parent_handle, &path)?;
    let cache = BoundCache {
        _parent: parent_handle,
        file,
        public_path: path,
    };
    Ok(cache)
}

#[cfg(target_os = "macos")]
const BOUND_CACHE_VFS_NAME: &CStr = c"kio-bound-cache-unix";

#[cfg(target_os = "macos")]
static BOUND_CACHE_VFS_INIT: Once = Once::new();

#[cfg(target_os = "macos")]
static BOUND_CACHE_VFS_RESULT: OnceLock<std::result::Result<(), String>> = OnceLock::new();

#[cfg(target_os = "macos")]
pub(super) static BOUND_CACHE_DEFAULT_VFS: OnceLock<usize> = OnceLock::new();

#[cfg(target_os = "macos")]
pub(super) fn initialize_bound_cache_vfs() -> Result<()> {
    BOUND_CACHE_VFS_INIT.call_once(|| {
        let result = unsafe {
            let original = rusqlite::ffi::sqlite3_vfs_find(std::ptr::null());
            if original.is_null() {
                Err("SQLite has no default VFS".to_owned())
            } else {
                let _ = BOUND_CACHE_DEFAULT_VFS.set(original as usize);
                let mut wrapped = Box::new(*original);
                wrapped.zName = BOUND_CACHE_VFS_NAME.as_ptr();
                wrapped.xOpen = Some(bound_cache_x_open);
                wrapped.xFullPathname = Some(bound_cache_x_full_pathname);
                let code = rusqlite::ffi::sqlite3_vfs_register(Box::into_raw(wrapped), 0);
                if code == rusqlite::ffi::SQLITE_OK {
                    Ok(())
                } else {
                    Err(format!(
                        "register bound cache SQLite VFS: SQLite error {code}"
                    ))
                }
            }
        };
        let _ = BOUND_CACHE_VFS_RESULT.set(result);
    });
    BOUND_CACHE_VFS_RESULT
        .get()
        .expect("VFS initializer sets result")
        .as_ref()
        .map_err(|e| crate::IndexError::Schema(e.clone()))?;
    Ok(())
}

#[cfg(target_os = "macos")]
pub(super) unsafe extern "C" fn bound_cache_x_full_pathname(
    _: *mut rusqlite::ffi::sqlite3_vfs,
    name: *const std::ffi::c_char,
    output_len: std::ffi::c_int,
    output: *mut std::ffi::c_char,
) -> std::ffi::c_int {
    if name.is_null() || output.is_null() || output_len <= 0 {
        return rusqlite::ffi::SQLITE_CANTOPEN;
    }
    let bytes = unsafe { CStr::from_ptr(name).to_bytes() };
    if cache_internal_name_allowed(bytes) {
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
    let Some(default_vfs) = BOUND_CACHE_DEFAULT_VFS.get() else {
        return rusqlite::ffi::SQLITE_CANTOPEN;
    };
    let default_vfs = *default_vfs as *mut rusqlite::ffi::sqlite3_vfs;
    let Some(callback) = (unsafe { (*default_vfs).xFullPathname }) else {
        return rusqlite::ffi::SQLITE_CANTOPEN;
    };
    unsafe { callback(default_vfs, name, output_len, output) }
}

#[cfg(target_os = "macos")]
pub(super) fn is_bound_cache_fd_name(value: &[u8]) -> bool {
    value
        .strip_prefix(b"/dev/fd/")
        .is_some_and(|fd| !fd.is_empty() && fd.iter().all(u8::is_ascii_digit))
}
