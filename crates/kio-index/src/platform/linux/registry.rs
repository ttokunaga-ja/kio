use crate::Result;
use crate::platform::registry_unix::leaf_identity;
#[cfg(test)]
use crate::registry::run_parent_component_hook;
use crate::registry::{
    RegistrySnapshotError, SnapshotAttemptError, registry_parent_binding_error, validate_directory,
};
use cap_primitives::fs as cap_fs;
use std::{
    fs,
    path::{Component, Path, PathBuf},
};
pub(crate) struct RegistryParentBinding {
    pub(crate) parent: std::sync::Arc<fs::File>,
    pub(crate) leaf: String,
    pub(crate) parent_path: PathBuf,
    // Keep the core-duplicated inherited root for the complete snapshot
    // attempt. A later `/dev/fd/N` re-resolution is compared to this retained
    // capability before a stable read or missing result is accepted.
    inherited_root: Option<kio_core::store_dir::StoreDirectory>,
}

pub(crate) fn registry_parent_and_leaf(
    path: &Path,
    control: &kio_core::control::ControlHandle,
) -> std::result::Result<RegistryParentBinding, RegistrySnapshotError> {
    if let Some((root, suffix)) = kio_core::private_fs::resolve_inherited_private_root_with_control(
        path, control,
    )
    .map_err(|error| {
        RegistrySnapshotError::UnsafeIntegrity(format!(
            "resolve inherited registry directory capability: {error}"
        ))
    })? {
        return inherited_registry_parent_and_leaf(path, root, &suffix);
    }
    let path = normalized_absolute_registry_path(path)?;
    let filesystem_root = registry_filesystem_root(&path)?;
    let leaf = path
        .file_name()
        .and_then(|leaf| leaf.to_str())
        .ok_or_else(|| {
            RegistrySnapshotError::UnsafeIntegrity(format!(
                "scope registry path has no valid UTF-8 file name: {}",
                path.display()
            ))
        })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("/"));
    let mut handle =
        cap_fs::open_ambient_dir(&filesystem_root, cap_primitives::ambient_authority()).map_err(
            |error| RegistrySnapshotError::UnstableBusy(format!("open filesystem root: {error}")),
        )?;
    let mut bound_component_path = filesystem_root.clone();
    #[cfg(test)]
    let mut index = 0usize;
    for component in parent.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let before = match cap_fs::stat(&handle, Path::new(name), cap_fs::FollowSymlinks::No) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(RegistrySnapshotError::Missing);
            }
            Err(error) => {
                return Err(RegistrySnapshotError::UnstableBusy(format!(
                    "inspect registry parent component {}: {error}",
                    name.to_string_lossy()
                )));
            }
        };
        validate_directory(&before, &name.to_string_lossy())?;
        #[cfg(test)]
        run_parent_component_hook(index);
        let child = cap_fs::open_dir_nofollow(&handle, Path::new(name)).map_err(|error| {
            RegistrySnapshotError::UnsafeIntegrity(format!(
                "open registry parent component {}: {error}",
                name.to_string_lossy()
            ))
        })?;
        bound_component_path.push(name);
        let opened = cap_fs::Metadata::from_file(&child).map_err(|error| {
            RegistrySnapshotError::UnstableBusy(format!(
                "inspect opened registry parent component {}: {error}",
                name.to_string_lossy()
            ))
        })?;
        validate_directory(&opened, &name.to_string_lossy())?;
        let current = cap_fs::stat(&handle, Path::new(name), cap_fs::FollowSymlinks::No).map_err(
            |error| {
                RegistrySnapshotError::UnsafeIntegrity(format!(
                    "reinspect registry parent component {} after opening: {error}",
                    name.to_string_lossy()
                ))
            },
        )?;
        validate_directory(&current, &name.to_string_lossy())?;
        let before_identity = leaf_identity(&before).ok_or_else(|| {
            RegistrySnapshotError::UnsafeIntegrity(format!(
                "registry parent component has no usable identity: {}",
                name.to_string_lossy()
            ))
        })?;
        let opened_identity = leaf_identity(&opened).ok_or_else(|| {
            RegistrySnapshotError::UnsafeIntegrity(format!(
                "opened registry parent component has no usable identity: {}",
                name.to_string_lossy()
            ))
        })?;
        let current_identity = leaf_identity(&current).ok_or_else(|| {
            RegistrySnapshotError::UnsafeIntegrity(format!(
                "reinspected registry parent component has no usable identity: {}",
                name.to_string_lossy()
            ))
        })?;
        if before_identity != opened_identity
            || before_identity != current_identity
            || opened_identity != current_identity
        {
            return Err(RegistrySnapshotError::UnsafeIntegrity(format!(
                "registry parent component changed while opening: {}",
                name.to_string_lossy()
            )));
        }

        handle = child;
        #[cfg(test)]
        {
            index += 1;
        }
    }
    Ok(RegistryParentBinding {
        parent: std::sync::Arc::new(handle),
        leaf: leaf.to_owned(),
        parent_path: parent.to_owned(),

        inherited_root: None,
    })
}

pub(crate) fn inherited_registry_parent_and_leaf(
    path: &Path,
    root: kio_core::store_dir::StoreDirectory,
    suffix: &[std::ffi::OsString],
) -> std::result::Result<RegistryParentBinding, RegistrySnapshotError> {
    use std::os::unix::fs::MetadataExt;

    let (leaf, parent_suffix) = suffix.split_last().ok_or_else(|| {
        RegistrySnapshotError::UnsafeIntegrity(
            "inherited scope registry path has no file name".to_owned(),
        )
    })?;
    let leaf = leaf.to_str().ok_or_else(|| {
        RegistrySnapshotError::UnsafeIntegrity(
            "inherited scope registry path has no valid UTF-8 file name".to_owned(),
        )
    })?;
    let parent_path = path.parent().ok_or_else(|| {
        RegistrySnapshotError::UnsafeIntegrity(
            "inherited scope registry path has no parent".to_owned(),
        )
    })?;
    let mut parent = root.root_handle();
    // SAFETY: geteuid has no arguments and no memory-safety preconditions.
    let uid = unsafe { libc::geteuid() };
    for name in parent_suffix {
        let label = name.to_string_lossy();
        let before =
            match cap_fs::stat(parent.as_ref(), Path::new(name), cap_fs::FollowSymlinks::No) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(RegistrySnapshotError::Missing);
                }
                Err(error) => {
                    return Err(RegistrySnapshotError::UnstableBusy(format!(
                        "inspect inherited registry parent component {label}: {error}"
                    )));
                }
            };
        validate_directory(&before, &label)?;
        let child =
            cap_fs::open_dir_nofollow(parent.as_ref(), Path::new(name)).map_err(|error| {
                RegistrySnapshotError::UnsafeIntegrity(format!(
                    "open inherited registry parent component {label}: {error}"
                ))
            })?;
        let opened = cap_fs::Metadata::from_file(&child).map_err(|error| {
            RegistrySnapshotError::UnstableBusy(format!(
                "inspect inherited registry parent component {label}: {error}"
            ))
        })?;
        let after = cap_fs::stat(parent.as_ref(), Path::new(name), cap_fs::FollowSymlinks::No)
            .map_err(|error| {
                RegistrySnapshotError::UnsafeIntegrity(format!(
                    "reinspect inherited registry parent component {label}: {error}"
                ))
            })?;
        validate_directory(&opened, &label)?;
        validate_directory(&after, &label)?;
        let before_identity = leaf_identity(&before).ok_or_else(|| {
            RegistrySnapshotError::UnsafeIntegrity(format!(
                "inherited registry parent component has no usable identity: {label}"
            ))
        })?;
        if leaf_identity(&opened).as_ref() != Some(&before_identity)
            || leaf_identity(&after).as_ref() != Some(&before_identity)
        {
            return Err(RegistrySnapshotError::UnsafeIntegrity(format!(
                "inherited registry parent component changed while opening: {label}"
            )));
        }
        let metadata = child.metadata().map_err(|error| {
            RegistrySnapshotError::UnstableBusy(format!(
                "inspect inherited registry parent permissions {label}: {error}"
            ))
        })?;
        if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            return Err(RegistrySnapshotError::UnsafeIntegrity(format!(
                "inherited registry parent is not owner-private: {label}"
            )));
        }
        parent = std::sync::Arc::new(child);
    }
    Ok(RegistryParentBinding {
        parent,
        leaf: leaf.to_owned(),
        parent_path: parent_path.to_owned(),
        inherited_root: Some(root),
    })
}

pub(crate) fn stable_registry_parent_binding(
    path: &Path,
    control: &kio_core::control::ControlHandle,
) -> std::result::Result<Option<RegistryParentBinding>, SnapshotAttemptError> {
    if let Some((root, suffix)) = kio_core::private_fs::resolve_inherited_private_root_with_control(
        path, control,
    )
    .map_err(|error| {
        SnapshotAttemptError::Unsafe(format!(
            "resolve inherited registry directory capability: {error}"
        ))
    })? {
        match inherited_registry_parent_and_leaf(path, root.clone(), &suffix) {
            Ok(binding) => return Ok(Some(binding)),
            Err(RegistrySnapshotError::Missing) => {
                match inherited_registry_parent_and_leaf(path, root.clone(), &suffix) {
                    Ok(_) => {
                        return Err(SnapshotAttemptError::Unstable(
                            "inherited registry parent appeared while confirming its absence"
                                .to_owned(),
                        ));
                    }
                    Err(RegistrySnapshotError::Missing) => {
                        let (fresh_root, fresh_suffix) =
                            kio_core::private_fs::resolve_inherited_private_root_with_control(
                                path, control,
                            )
                            .map_err(|error| {
                                SnapshotAttemptError::Unsafe(format!(
                                    "re-resolve inherited registry directory capability: {error}"
                                ))
                            })?
                            .ok_or_else(|| {
                                SnapshotAttemptError::Unstable(
                                    "inherited registry root disappeared while confirming absence"
                                        .to_owned(),
                                )
                            })?;
                        if fresh_suffix != suffix || !same_inherited_root(&root, &fresh_root)? {
                            return Err(SnapshotAttemptError::Unsafe(
                                "inherited registry root changed while confirming absence"
                                    .to_owned(),
                            ));
                        }
                        return Ok(None);
                    }
                    Err(error) => return Err(registry_parent_binding_error(error)),
                }
            }
            Err(error) => return Err(registry_parent_binding_error(error)),
        }
    }
    match registry_parent_and_leaf(path, control) {
        Ok(binding) => Ok(Some(binding)),
        Err(RegistrySnapshotError::Missing) => match registry_parent_and_leaf(path, control) {
            Err(RegistrySnapshotError::Missing) => Ok(None),
            Ok(_) => Err(SnapshotAttemptError::Unstable(
                "scope registry parent appeared while confirming its absence".to_owned(),
            )),
            Err(error) => Err(registry_parent_binding_error(error)),
        },
        Err(error) => Err(registry_parent_binding_error(error)),
    }
}

/// Freshly bind the canonical parent at the acceptance boundary and require it
/// to be the same directory as the descriptor used to capture the snapshot.
pub(crate) fn fresh_registry_parent_binding(
    held: &RegistryParentBinding,
    control: &kio_core::control::ControlHandle,
) -> std::result::Result<std::sync::Arc<fs::File>, SnapshotAttemptError> {
    let canonical_path = held.parent_path.join(&held.leaf);
    let fresh = match registry_parent_and_leaf(&canonical_path, control) {
        Ok(binding) => binding,
        Err(RegistrySnapshotError::Missing) => {
            return Err(SnapshotAttemptError::Unstable(
                "scope registry canonical parent disappeared while snapshotting".to_owned(),
            ));
        }
        Err(error) => return Err(registry_parent_binding_error(error)),
    };
    if fresh.leaf != held.leaf || fresh.parent_path != held.parent_path {
        return Err(SnapshotAttemptError::Unsafe(
            "scope registry canonical parent rebound to an unexpected path".to_owned(),
        ));
    }

    match (&held.inherited_root, &fresh.inherited_root) {
        (Some(held_root), Some(fresh_root)) => {
            if !same_inherited_root(held_root, fresh_root)? {
                return Err(SnapshotAttemptError::Unsafe(
                    "inherited registry root changed while snapshotting".to_owned(),
                ));
            }
        }
        (None, None) => {}
        _ => {
            return Err(SnapshotAttemptError::Unsafe(
                "registry binding changed inherited-root mode while snapshotting".to_owned(),
            ));
        }
    }
    let held_metadata = cap_fs::Metadata::from_file(held.parent.as_ref()).map_err(|error| {
        SnapshotAttemptError::Unstable(format!("inspect retained registry parent: {error}"))
    })?;
    let fresh_metadata = cap_fs::Metadata::from_file(fresh.parent.as_ref()).map_err(|error| {
        SnapshotAttemptError::Unstable(format!("inspect freshly bound registry parent: {error}"))
    })?;
    let held_identity = leaf_identity(&held_metadata).ok_or_else(|| {
        SnapshotAttemptError::Unsafe(
            "retained registry parent has no usable identity while snapshotting".to_owned(),
        )
    })?;
    let fresh_identity = leaf_identity(&fresh_metadata).ok_or_else(|| {
        SnapshotAttemptError::Unsafe(
            "freshly bound registry parent has no usable identity while snapshotting".to_owned(),
        )
    })?;
    if held_identity != fresh_identity {
        return Err(SnapshotAttemptError::Unsafe(
            "scope registry canonical parent identity changed while snapshotting".to_owned(),
        ));
    }
    Ok(fresh.parent)
}

pub(crate) fn same_inherited_root(
    held: &kio_core::store_dir::StoreDirectory,
    fresh: &kio_core::store_dir::StoreDirectory,
) -> std::result::Result<bool, SnapshotAttemptError> {
    let held_metadata =
        cap_fs::Metadata::from_file(held.root_handle().as_ref()).map_err(|error| {
            SnapshotAttemptError::Unstable(format!(
                "inspect retained inherited registry root: {error}"
            ))
        })?;
    let fresh_metadata =
        cap_fs::Metadata::from_file(fresh.root_handle().as_ref()).map_err(|error| {
            SnapshotAttemptError::Unstable(format!(
                "inspect fresh inherited registry root: {error}"
            ))
        })?;
    Ok(leaf_identity(&held_metadata) == leaf_identity(&fresh_metadata))
}

pub(crate) fn normalized_absolute_registry_path(
    path: &Path,
) -> std::result::Result<PathBuf, RegistrySnapshotError> {
    if !path.is_absolute() {
        return Err(RegistrySnapshotError::UnsafeIntegrity(format!(
            "scope registry path must be absolute: {}",
            path.display()
        )));
    }
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                normalized.push(name);
            }
            Component::CurDir | Component::ParentDir => {
                return Err(RegistrySnapshotError::UnsafeIntegrity(format!(
                    "scope registry path is not normalized: {}",
                    path.display()
                )));
            }
            Component::Prefix(_) => {
                return Err(RegistrySnapshotError::UnsafeIntegrity(format!(
                    "scope registry path prefix is unsupported: {}",
                    path.display()
                )));
            }
        }
    }
    Ok(normalized)
}

pub(crate) fn registry_writer_parent(
    path: &Path,
    control: &kio_core::control::ControlHandle,
) -> Result<Option<(PathBuf, kio_core::store_dir::StoreDirectory)>> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    let Some((mut parent, suffix)) =
        kio_core::private_fs::resolve_inherited_private_root_with_control(path, control).map_err(
            |error| {
                crate::IndexError::Schema(format!(
                    "resolve inherited registry directory capability: {error}"
                ))
            },
        )?
    else {
        return Ok(None);
    };
    let (leaf, parent_suffix) = suffix.split_last().ok_or_else(|| {
        crate::IndexError::Schema("inherited scope registry path has no file name".to_owned())
    })?;
    let verify_owner_private_directory =
        |directory: &kio_core::store_dir::StoreDirectory| -> Result<()> {
            let metadata = directory.root_handle().metadata().map_err(|error| {
                crate::IndexError::Schema(format!("inspect inherited registry parent: {error}"))
            })?;
            // SAFETY: geteuid has no arguments and no memory-safety preconditions.
            let uid = unsafe { libc::geteuid() };
            if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
                return Err(crate::IndexError::Schema(format!(
                    "inherited registry parent is not owner-private: {}",
                    directory.path().display()
                )));
            }
            Ok(())
        };

    verify_owner_private_directory(&parent)?;
    let mut logical = parent.path().to_path_buf();
    for name in parent_suffix {
        let component = Path::new(name);
        logical.push(name);
        let next = if parent
            .contains_entry(component)
            .map_err(|error| crate::IndexError::Schema(error.to_string()))?
        {
            parent
                .open_directory(component)
                .map_err(|error| crate::IndexError::Schema(error.to_string()))?
        } else {
            parent
                .create_directory(component)
                .map_err(|error| crate::IndexError::Schema(error.to_string()))?
        };
        parent = kio_core::store_dir::StoreDirectory::from_retained_with_control(
            next,
            logical.clone(),
            control.clone(),
        )
        .map_err(|error| crate::IndexError::Schema(error.to_string()))?;
        // Existing directories are only verified, never chmod-repaired. New
        // directories were restricted by StoreDirectory before this adoption.
        verify_owner_private_directory(&parent)?;
    }

    let leaf = Path::new(leaf);
    let name = CString::new(leaf.as_os_str().as_bytes()).map_err(|_| {
        crate::IndexError::Schema("inherited scope registry leaf contains NUL".to_owned())
    })?;
    // Create a missing SQLite main leaf descriptor-relatively and owner-only
    // before SQLite sees it. Existing state is opened with O_NOFOLLOW and
    // validated, never chmod-repaired.
    if !parent
        .contains_entry(leaf)
        .map_err(|error| crate::IndexError::Schema(error.to_string()))?
    {
        // SAFETY: `parent` is a retained directory capability and `name` is a
        // single NUL-free component supplied by core's inherited-root parser.
        let fd = unsafe {
            libc::openat(
                parent.root_handle().as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd >= 0 {
            // SAFETY: openat returned an owned descriptor above.
            let created = unsafe { fs::File::from_raw_fd(fd) };
            let metadata = created.metadata().map_err(|error| {
                crate::IndexError::Schema(format!(
                    "inspect created inherited registry leaf: {error}"
                ))
            })?;
            // SAFETY: geteuid has no arguments and no memory-safety preconditions.
            let uid = unsafe { libc::geteuid() };
            if !metadata.is_file()
                || metadata.uid() != uid
                || metadata.mode() & 0o077 != 0
                || metadata.nlink() != 1
            {
                return Err(crate::IndexError::Schema(
                    "created inherited registry leaf is not owner-private".to_owned(),
                ));
            }
        } else {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EEXIST) {
                parent
                    .ensure_owner_private(leaf)
                    .map_err(|error| crate::IndexError::Schema(error.to_string()))?;
            } else {
                return Err(crate::IndexError::Schema(format!(
                    "create inherited scope registry leaf: {error}"
                )));
            }
        }
    } else {
        parent
            .ensure_owner_private(leaf)
            .map_err(|error| crate::IndexError::Schema(error.to_string()))?;
    }
    let sqlite_path = PathBuf::from(format!(
        "/proc/self/fd/{}",
        parent.root_handle().as_raw_fd()
    ))
    .join(leaf);
    Ok(Some((sqlite_path, parent)))
}

use crate::platform::registry_unix::registry_filesystem_root;
pub(crate) fn prepare_registry_writer(
    path: &Path,
    control: &kio_core::control::ControlHandle,
) -> Result<(PathBuf, Option<kio_core::store_dir::StoreDirectory>)> {
    let inherited = registry_writer_parent(path, control)?;
    let (sqlite_path, inherited_parent) = if let Some((sqlite_path, parent)) = inherited {
        (sqlite_path, Some(parent))
    } else {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| crate::IndexError::Schema(err.to_string()))?;
            // P2: the device data dir (`~/.local/share/kio`) that holds this
            // registry, the cost ledger, logs and the open-cache carries usage
            // patterns and the scope map — restrict it to the owner (0700) so a
            // multi-user host cannot read another user's data. Best-effort (the
            // registry is a recoverable cache); no-op on non-unix.

            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }
        (path.to_path_buf(), None)
    };

    Ok((sqlite_path, inherited_parent))
}
