use crate::Result;
use crate::registry::{
    FileObservation, LeafObservation, MAX_REGISTRY_LEAF_BYTES, RegistrySnapshotError,
    SnapshotAttemptError, index_error_from_snapshot, registry_parent_binding_error,
    validate_directory,
};
#[cfg(test)]
use crate::registry::{SnapshotTestHook, SnapshotTestPhase, run_parent_component_hook};
use cap_primitives::fs as cap_fs;
use sha2::{Digest, Sha256};
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

        verify_windows_directory_binding(&bound_component_path, &child, &name.to_string_lossy())?;
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

pub(crate) fn verify_windows_directory_binding(
    path: &Path,
    opened: &fs::File,
    label: &str,
) -> std::result::Result<(), RegistrySnapshotError> {
    let path_identity = kio_core::platform::real_directory_identity(path).map_err(|error| {
        RegistrySnapshotError::UnstableBusy(format!(
            "inspect Windows registry parent component {label}: {error}"
        ))
    })?;
    let handle_identity = kio_core::platform::directory_handle_identity(opened);
    if path_identity.is_none() || path_identity != handle_identity {
        return Err(RegistrySnapshotError::UnsafeIntegrity(format!(
            "Windows registry parent component is a reparse point or changed while opening: {label}"
        )));
    }
    Ok(())
}

pub(crate) fn verify_windows_regular_binding(
    path: &Path,
    opened: &fs::File,
    label: &str,
) -> std::result::Result<(), SnapshotAttemptError> {
    let path_identity = kio_core::platform::real_regular_file_identity(path).map_err(|error| {
        SnapshotAttemptError::Unstable(format!("inspect Windows registry {label}: {error}"))
    })?;
    let handle_identity = kio_core::platform::regular_file_handle_identity(opened);
    if path_identity.is_none() || handle_identity.is_none() {
        return Err(SnapshotAttemptError::Unsafe(format!(
            "Windows registry {label} is a reparse point or has no usable identity"
        )));
    }
    if path_identity != handle_identity {
        return Err(SnapshotAttemptError::Unstable(format!(
            "Windows registry {label} changed while opening"
        )));
    }
    Ok(())
}

pub(crate) fn normalized_absolute_registry_path(
    path: &Path,
) -> std::result::Result<PathBuf, RegistrySnapshotError> {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Prefix;

    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return Err(RegistrySnapshotError::UnsafeIntegrity(format!(
            "scope registry path must use an absolute drive or UNC path: {}",
            path.display()
        )));
    };
    let verbatim_disk = matches!(prefix.kind(), Prefix::VerbatimDisk(_));
    if !matches!(
        prefix.kind(),
        Prefix::Disk(_) | Prefix::UNC(_, _) | Prefix::VerbatimDisk(_)
    ) || !matches!(components.next(), Some(Component::RootDir))
    {
        return Err(RegistrySnapshotError::UnsafeIntegrity(format!(
            "scope registry path must use a normalized absolute drive or UNC path: {}",
            path.display()
        )));
    }
    let mut normalized = PathBuf::from(prefix.as_os_str());
    normalized.push("\\");
    for component in components {
        match component {
            Component::Normal(name) => {
                // Canonical Windows drive paths retain their verbatim prefix.
                // That namespace only treats backslashes as separators; do
                // not pass a disguised slash separator to the parent walk.
                if verbatim_disk && name.encode_wide().any(|unit| unit == b'/' as u16) {
                    return Err(RegistrySnapshotError::UnsafeIntegrity(format!(
                        "scope registry verbatim drive path contains a slash: {}",
                        path.display()
                    )));
                }
                normalized.push(name);
            }
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(RegistrySnapshotError::UnsafeIntegrity(format!(
                    "scope registry path is not normalized: {}",
                    path.display()
                )));
            }
        }
    }
    Ok(normalized)
}

pub(crate) fn windows_registry_writer_parent(
    path: &Path,
    control: &kio_core::control::ControlHandle,
) -> Result<(PathBuf, kio_core::store_dir::StoreDirectory)> {
    use kio_core::{private_fs, store_dir::StoreDirectory};
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        // Only plain relative custom paths inherit the current directory.
        // Drive-relative and rooted-relative paths have a different authority.
        if path.as_os_str().is_empty()
            || !path
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
        {
            return Err(crate::IndexError::Schema(format!(
                "registry writer path is not a normalized relative path: {}",
                path.display()
            )));
        }
        std::env::current_dir()
            .map_err(|error| crate::IndexError::Schema(error.to_string()))?
            .join(path)
    };
    let absolute =
        normalized_absolute_registry_path(&absolute).map_err(index_error_from_snapshot)?;
    if absolute.file_name().is_none() {
        return Err(crate::IndexError::Schema(
            "registry writer path has no file name".to_owned(),
        ));
    }
    let parent_path = absolute.parent().ok_or_else(|| {
        crate::IndexError::Schema("registry writer path has no parent".to_owned())
    })?;
    let mut ancestor = parent_path.to_path_buf();
    let mut missing = Vec::new();
    let mut parent = loop {
        match fs::symlink_metadata(&ancestor) {
            Ok(metadata) => {
                if !metadata.is_dir()
                    || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
                {
                    return Err(crate::IndexError::Schema(format!(
                        "registry writer ancestor is not a real directory: {}",
                        ancestor.display()
                    )));
                }
                // An occupied unsafe ancestor is a refusal, never a reason to
                // back off farther or repair that ancestor's ACL.
                break private_fs::verify_private_creation_parent_with_control(&ancestor, control)
                    .map_err(|error| crate::IndexError::Schema(error.to_string()))?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let component = ancestor.file_name().ok_or_else(|| {
                    crate::IndexError::Schema("registry writer has no existing ancestor".to_owned())
                })?;
                missing.push(component.to_os_string());
                if !ancestor.pop() {
                    return Err(crate::IndexError::Schema(
                        "registry writer has no existing ancestor".to_owned(),
                    ));
                }
            }
            Err(error) => return Err(crate::IndexError::Schema(error.to_string())),
        }
    };
    let mut logical = ancestor;
    for component in missing.into_iter().rev() {
        let leaf = Path::new(&component);
        let next = if parent
            .contains_entry(leaf)
            .map_err(|error| crate::IndexError::Schema(error.to_string()))?
        {
            parent
                .open_directory(leaf)
                .map_err(|error| crate::IndexError::Schema(error.to_string()))?
        } else {
            // The core primitive installs a protected owner-only DACL before
            // exposing this create-only entry. Creation errors stay errors.
            parent
                .create_directory(leaf)
                .map_err(|error| crate::IndexError::Schema(error.to_string()))?
        };
        logical.push(&component);
        parent = StoreDirectory::from_retained_with_control(next, logical.clone(), control.clone())
            .map_err(|error| crate::IndexError::Schema(error.to_string()))?;
        // A component that appeared after the missing suffix was observed must
        // also be private; neither that entry nor a new one is ACL-repaired.
        private_fs::verify_private_directory_handle(&parent)
            .map_err(|error| crate::IndexError::Schema(error.to_string()))?;
    }
    Ok((absolute, parent))
}

use std::io::{Read, Write};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileIdentity {
    volume_serial_number: u32,
    file_index: u64,
}

pub(crate) fn registry_filesystem_root(
    path: &Path,
) -> std::result::Result<PathBuf, RegistrySnapshotError> {
    {
        let mut components = path.components();
        use std::path::Prefix;
        let Some(Component::Prefix(prefix)) = components.next() else {
            return Err(RegistrySnapshotError::UnsafeIntegrity(format!(
                "scope registry path must have an absolute Windows volume/share root: {}",
                path.display()
            )));
        };
        if !matches!(
            prefix.kind(),
            Prefix::Disk(_) | Prefix::UNC(_, _) | Prefix::VerbatimDisk(_)
        ) || !matches!(components.next(), Some(Component::RootDir))
        {
            return Err(RegistrySnapshotError::UnsafeIntegrity(format!(
                "scope registry path must be absolute on Windows: {}",
                path.display()
            )));
        }
        let mut root = PathBuf::from(prefix.as_os_str());
        root.push("\\");
        Ok(root)
    }
}

pub(crate) fn leaf_identity(metadata: &cap_fs::Metadata) -> Option<FileIdentity> {
    {
        use cap_fs::_WindowsByHandle;
        Some(FileIdentity {
            volume_serial_number: metadata.volume_serial_number()?,
            file_index: metadata.file_index()?,
        })
    }
}

pub(crate) fn validate_regular(
    metadata: &cap_fs::Metadata,
    label: &str,
) -> std::result::Result<(), SnapshotAttemptError> {
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(SnapshotAttemptError::Unsafe(format!(
            "scope registry {label} must be a regular non-symlink file"
        )));
    }
    if metadata.len() > MAX_REGISTRY_LEAF_BYTES {
        return Err(SnapshotAttemptError::Unsafe(format!(
            "scope registry {label} exceeds {} byte limit",
            MAX_REGISTRY_LEAF_BYTES
        )));
    }

    {
        use cap_fs::_WindowsByHandle;
        if metadata.number_of_links() != Some(1) {
            return Err(SnapshotAttemptError::Unsafe(format!(
                "scope registry {label} must have exactly one hard link"
            )));
        }
    }
    Ok(())
}

pub(crate) fn observe_leaf(
    parent: &fs::File,
    parent_path: &Path,
    name: &str,
    copy_to: Option<(&PrivateSnapshotStorage, &str)>,
    #[cfg(test)] hook: Option<&SnapshotTestHook>,
    #[cfg(test)] attempt: usize,
) -> std::result::Result<LeafObservation, SnapshotAttemptError> {
    let before = match cap_fs::stat(parent, Path::new(name), cap_fs::FollowSymlinks::No) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LeafObservation::Absent);
        }
        Err(error) => {
            return Err(SnapshotAttemptError::Unstable(format!(
                "stat scope registry {name}: {error}"
            )));
        }
    };
    validate_regular(&before, name)?;
    #[cfg(test)]
    if let Some(hook) = hook {
        hook(
            SnapshotTestPhase::BeforeSourceLeafOpen,
            attempt,
            Some(&parent_path.join(name)),
        );
    }
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut input = cap_fs::open(parent, Path::new(name), &options).map_err(|error| {
        SnapshotAttemptError::Unstable(format!("open scope registry {name}: {error}"))
    })?;
    let opened = cap_fs::Metadata::from_file(&input).map_err(|error| {
        SnapshotAttemptError::Unstable(format!("inspect opened registry {name}: {error}"))
    })?;
    validate_regular(&opened, name)?;

    verify_windows_regular_binding(&parent_path.join(name), &input, name)?;
    let before_identity = leaf_identity(&before).ok_or_else(|| {
        SnapshotAttemptError::Unsafe(format!("scope registry {name} has no usable identity"))
    })?;
    let opened_identity = leaf_identity(&opened).ok_or_else(|| {
        SnapshotAttemptError::Unsafe(format!(
            "opened scope registry {name} has no usable identity"
        ))
    })?;
    if before_identity != opened_identity || before.len() != opened.len() {
        return Err(SnapshotAttemptError::Unstable(format!(
            "scope registry {name} changed while opening"
        )));
    }

    let mut output = match copy_to {
        Some((storage, basename)) => Some(storage.create_file(basename)?),
        None => None,
    };
    let mut hash = Sha256::new();
    let mut total = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = input.read(&mut buf).map_err(|error| {
            SnapshotAttemptError::Unstable(format!("read scope registry {name}: {error}"))
        })?;
        if read == 0 {
            break;
        }
        total = total.checked_add(read as u64).ok_or_else(|| {
            SnapshotAttemptError::Unsafe(format!("scope registry {name} size overflow"))
        })?;
        if total > MAX_REGISTRY_LEAF_BYTES {
            return Err(SnapshotAttemptError::Unsafe(format!(
                "scope registry {name} exceeded byte limit while reading"
            )));
        }
        hash.update(&buf[..read]);
        if let Some(output) = output.as_mut() {
            output.write_all(&buf[..read]).map_err(|error| {
                SnapshotAttemptError::Unstable(format!("write registry snapshot {name}: {error}"))
            })?;
        }
    }
    if let Some(output) = output.as_mut() {
        output.flush().map_err(|error| {
            SnapshotAttemptError::Unstable(format!("flush registry snapshot {name}: {error}"))
        })?;
    }
    #[cfg(test)]
    if let Some(hook) = hook {
        hook(
            SnapshotTestPhase::BeforeSourceLeafRestat,
            attempt,
            Some(&parent_path.join(name)),
        );
    }
    let after =
        cap_fs::stat(parent, Path::new(name), cap_fs::FollowSymlinks::No).map_err(|error| {
            SnapshotAttemptError::Unstable(format!("restat scope registry {name}: {error}"))
        })?;
    validate_regular(&after, name)?;

    verify_windows_regular_binding(&parent_path.join(name), &input, name)?;
    let after_identity = leaf_identity(&after).ok_or_else(|| {
        SnapshotAttemptError::Unsafe(format!(
            "reinspected scope registry {name} has no usable identity"
        ))
    })?;
    if opened_identity != after_identity || opened.len() != after.len() || total != opened.len() {
        return Err(SnapshotAttemptError::Unstable(format!(
            "scope registry {name} changed while reading"
        )));
    }
    Ok(LeafObservation::Present(FileObservation {
        identity: opened_identity,
        bytes: total,
        sha256: hash.finalize().into(),
    }))
}

pub(crate) struct PrivateSnapshotStorage(
    crate::platform::registry_security::RegistrySnapshotPrivateDir,
);
impl PrivateSnapshotStorage {
    pub(crate) fn create() -> std::result::Result<Self, SnapshotAttemptError> {
        crate::platform::registry_security::RegistrySnapshotPrivateDir::create()
            .map(Self)
            .map_err(|error| {
                SnapshotAttemptError::Unstable(format!(
                    "create owner-private registry snapshot directory: {error}"
                ))
            })
    }
    pub(crate) fn path(&self) -> &Path {
        self.0.path()
    }
    pub(crate) fn create_file(
        &self,
        basename: &str,
    ) -> std::result::Result<fs::File, SnapshotAttemptError> {
        self.0.create_file(basename).map_err(|error| {
            SnapshotAttemptError::Unstable(format!(
                "create owner-private registry snapshot {basename}: {error}"
            ))
        })
    }
    pub(crate) fn capability(&self) -> std::result::Result<fs::File, SnapshotAttemptError> {
        self.0.capability().map_err(|error| {
            SnapshotAttemptError::Unstable(format!(
                "clone owner-private registry snapshot capability: {error}"
            ))
        })
    }
}
pub(crate) fn prepare_registry_writer(
    path: &Path,
    control: &kio_core::control::ControlHandle,
) -> Result<(PathBuf, Option<kio_core::store_dir::StoreDirectory>)> {
    let (sqlite_path, parent) = windows_registry_writer_parent(path, control)?;
    Ok((sqlite_path, Some(parent)))
}
