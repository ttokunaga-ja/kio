use crate::Result;
use crate::platform::BoundSqliteConnection;
use cap_primitives::fs as cap_fs;
use rusqlite::OpenFlags;
use std::path::{Path, PathBuf};
/// A cache leaf and its parent directory retained as capabilities.  The parent
/// must stay alive: a public cache pathname can be redirected after opening it.
pub(crate) struct BoundCache {
    _parent: std::fs::File,
    file: std::fs::File,
    public_path: PathBuf,
}

struct BoundCacheParent {
    parent: std::fs::File,
    public_path: PathBuf,
}

/// Bind the immediate cache parent without following it. Ancestors may be
/// OS-owned symlinks (such as `/var`), so only the direct parent is selected
/// relative to a canonicalized *outer* directory capability.
fn bind_cache_parent(
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

fn open_or_create_cache_parent(
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

    let before_identity = kio_core::platform::real_directory_identity(existing)
        .map_err(|error| {
            crate::IndexError::Schema(format!(
                "inspect aggregator cache ancestor reparse state {}: {error}",
                existing.display()
            ))
        })?
        .ok_or_else(|| {
            crate::IndexError::Schema(format!(
                "aggregator cache ancestor must not be a Windows reparse point: {}",
                existing.display()
            ))
        })?;
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
        if kio_core::platform::directory_handle_identity(&handle) != Some(before_identity) {
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

    Ok(())
}

fn create_private_cache_file(parent_handle: &std::fs::File, path: &Path) -> Result<std::fs::File> {
    let leaf = path.file_name().expect("resolved cache has leaf");
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    {
        use cap_fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    }
    let retained = kio_core::store_dir::StoreDirectory::from_retained(
        parent_handle
            .try_clone()
            .map_err(|error| crate::IndexError::Schema(error.to_string()))?,
        path.parent().expect("cache parent").to_path_buf(),
    )
    .map_err(|error| crate::IndexError::Schema(error.to_string()))?;
    let created = retained
        .create_private_file(Path::new(leaf))
        .map_err(|error| crate::IndexError::Schema(error.to_string()))?;
    let pinned = cap_fs::open(parent_handle, Path::new(leaf), &options)
        .map_err(|error| crate::IndexError::Schema(error.to_string()))?;
    if kio_core::platform::regular_file_handle_identity(&created)
        != kio_core::platform::regular_file_handle_identity(&pinned)
    {
        return Err(crate::IndexError::Schema(
            "private aggregator cache changed after creation".into(),
        ));
    }
    validate_bound_cache_file(&pinned, path)?;
    Ok(pinned)
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

    {
        use cap_fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    }
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

    Ok(BoundCache {
        _parent: parent_handle,
        file,
        public_path: path,
    })
}

pub(crate) fn open_bound_cache_connection(
    cache: &BoundCache,
    flags: OpenFlags,
) -> Result<BoundSqliteConnection> {
    BoundSqliteConnection::open::<()>(&cache.public_path, flags, None, None)
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
            Ok(metadata) if metadata.is_file() => {}
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

impl BoundCache {
    pub(crate) fn file(&self) -> &std::fs::File {
        &self.file
    }
    pub(crate) fn public_path(&self) -> &Path {
        &self.public_path
    }
}
