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
}

pub(crate) fn registry_parent_and_leaf(
    path: &Path,
    _control: &kio_core::control::ControlHandle,
    #[cfg(test)] hooks: Option<&crate::registry::RegistryTestHooks>,
) -> std::result::Result<RegistryParentBinding, RegistrySnapshotError> {
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
        run_parent_component_hook(hooks, index);
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
    crate::registry::verify_private_registry_parent(&handle, parent)?;
    Ok(RegistryParentBinding {
        parent: std::sync::Arc::new(handle),
        leaf: leaf.to_owned(),
        parent_path: parent.to_owned(),
    })
}

pub(crate) fn stable_registry_parent_binding(
    path: &Path,
    control: &kio_core::control::ControlHandle,
    #[cfg(test)] hooks: Option<&crate::registry::RegistryTestHooks>,
) -> std::result::Result<Option<RegistryParentBinding>, SnapshotAttemptError> {
    match registry_parent_and_leaf(
        path,
        control,
        #[cfg(test)]
        hooks,
    ) {
        Ok(binding) => Ok(Some(binding)),
        Err(RegistrySnapshotError::Missing) => match registry_parent_and_leaf(
            path,
            control,
            #[cfg(test)]
            hooks,
        ) {
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
    #[cfg(test)] hooks: Option<&crate::registry::RegistryTestHooks>,
) -> std::result::Result<std::sync::Arc<fs::File>, SnapshotAttemptError> {
    let canonical_path = held.parent_path.join(&held.leaf);
    let fresh = match registry_parent_and_leaf(
        &canonical_path,
        control,
        #[cfg(test)]
        hooks,
    ) {
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
                if normalized == Path::new("/") && name == "var" {
                    normalized.push("private");
                    normalized.push("var");
                } else {
                    normalized.push(name);
                }
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

use crate::platform::registry_unix::registry_filesystem_root;
pub(crate) fn prepare_registry_writer(
    path: &Path,
    _control: &kio_core::control::ControlHandle,
) -> Result<(PathBuf, Option<kio_core::store_dir::StoreDirectory>)> {
    crate::registry::prepare_private_registry_writer(path)
}
