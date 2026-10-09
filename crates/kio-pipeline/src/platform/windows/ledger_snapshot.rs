use crate::ledger::snapshot::{AttemptError, LedgerSnapshotError};
use cap_primitives::fs as cap_fs;
use std::{
    fs,
    path::{Component, Path, PathBuf},
};
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Identity {
    volume_serial_number: Option<u32>,
    file_index: Option<u64>,
}
pub(crate) struct PrivateStorage(super::snapshot_windows_security::LedgerSnapshotPrivateDir);
impl PrivateStorage {
    pub(crate) fn create() -> Result<Self, AttemptError> {
        #[cfg(test)]
        crate::ledger::snapshot::run_private_storage_create_hook()?;
        super::snapshot_windows_security::LedgerSnapshotPrivateDir::create()
            .map(Self)
            .map_err(|e| AttemptError::Unstable(format!("create private ledger snapshot dir: {e}")))
    }
    pub(crate) fn path(&self) -> &Path {
        self.0.path()
    }
    pub(crate) fn create_file(&self, name: &str) -> Result<fs::File, AttemptError> {
        self.0.create_file(name).map_err(|e| {
            AttemptError::Unstable(format!("create private ledger snapshot {name}: {e}"))
        })
    }
}
pub(crate) fn normalized_absolute(path: &Path) -> Result<PathBuf, LedgerSnapshotError> {
    if !path.is_absolute() {
        return Err(LedgerSnapshotError::UnsafeIntegrity(format!(
            "ledger path must be absolute: {}",
            path.display()
        )));
    }

    use std::os::windows::ffi::OsStrExt;
    use std::path::Prefix;
    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return Err(LedgerSnapshotError::UnsafeIntegrity(format!(
            "ledger path must use an absolute drive or UNC path: {}",
            path.display()
        )));
    };
    let verbatim_disk = matches!(prefix.kind(), Prefix::VerbatimDisk(_));
    if !matches!(
        prefix.kind(),
        Prefix::Disk(_) | Prefix::UNC(_, _) | Prefix::VerbatimDisk(_)
    ) || !matches!(components.next(), Some(Component::RootDir))
    {
        return Err(LedgerSnapshotError::UnsafeIntegrity(format!(
            "ledger path must use a normalized absolute drive or UNC path: {}",
            path.display()
        )));
    }
    let mut normalized = PathBuf::from(prefix.as_os_str());
    normalized.push("\\");
    for component in components {
        match component {
            Component::Normal(name) => {
                // Preserve canonical drive paths in their verbatim
                // namespace, whose only separator is a backslash.
                if verbatim_disk && name.encode_wide().any(|unit| unit == b'/' as u16) {
                    return Err(LedgerSnapshotError::UnsafeIntegrity(format!(
                        "ledger verbatim drive path contains a slash: {}",
                        path.display()
                    )));
                }
                normalized.push(name);
            }
            _ => {
                return Err(LedgerSnapshotError::UnsafeIntegrity(format!(
                    "ledger path is not normalized: {}",
                    path.display()
                )));
            }
        }
    }
    Ok(normalized)
}
pub(crate) fn normalize_snapshot_entry_path(path: &Path) -> Result<PathBuf, LedgerSnapshotError> {
    normalized_absolute(path)
}
pub(crate) use crate::ledger::snapshot::{
    bound_ambient_parent as bound_parent, stable_ambient_parent_binding as stable_parent_binding,
};
pub(crate) fn filesystem_root(_path: &Path) -> Result<PathBuf, LedgerSnapshotError> {
    use std::path::Prefix;
    let mut components = _path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return Err(LedgerSnapshotError::UnsafeIntegrity(
            "ledger path has no volume root".into(),
        ));
    };
    if !matches!(
        prefix.kind(),
        Prefix::Disk(_) | Prefix::UNC(_, _) | Prefix::VerbatimDisk(_)
    ) || !matches!(components.next(), Some(Component::RootDir))
    {
        return Err(LedgerSnapshotError::UnsafeIntegrity(
            "ledger path has no volume root".into(),
        ));
    }
    let mut root = PathBuf::from(prefix.as_os_str());
    root.push("\\");
    Ok(root)
}
pub(crate) fn verify_directory_binding(
    path: &Path,
    opened: &fs::File,
    label: &str,
) -> Result<(), LedgerSnapshotError> {
    let path_identity = kio_core::platform::real_directory_identity(path).map_err(|e| {
        LedgerSnapshotError::UnstableBusy(format!("inspect Windows ledger parent {label}: {e}"))
    })?;
    let handle_identity = kio_core::platform::directory_handle_identity(opened);
    if path_identity.is_none() || path_identity != handle_identity {
        return Err(LedgerSnapshotError::UnsafeIntegrity(format!(
            "Windows ledger parent is a reparse point or changed: {label}"
        )));
    }
    Ok(())
}
pub(crate) fn verify_regular_binding(
    path: &Path,
    opened: &fs::File,
    label: &str,
) -> Result<(), AttemptError> {
    let path_identity = kio_core::platform::real_regular_file_identity(path)
        .map_err(|e| AttemptError::Unstable(format!("inspect Windows ledger {label}: {e}")))?;
    let handle_identity = kio_core::platform::regular_file_handle_identity(opened);
    if path_identity.is_none() || path_identity != handle_identity {
        return Err(AttemptError::Unsafe(format!(
            "Windows ledger {label} is a reparse point or changed while opening"
        )));
    }
    Ok(())
}
pub(crate) fn identity(m: &cap_fs::Metadata) -> Identity {
    use cap_fs::_WindowsByHandle;
    Identity {
        volume_serial_number: m.volume_serial_number(),
        file_index: m.file_index(),
    }
}
/// Compare platform file identities without treating an unavailable Windows
/// identity field as a stable value. A missing volume serial or file index
/// cannot establish that two handles name the same object, so it fails closed.
pub(crate) fn same_identity(left: &cap_fs::Metadata, right: &cap_fs::Metadata) -> bool {
    let left = identity(left);
    let right = identity(right);
    left.volume_serial_number.is_some() && left.file_index.is_some() && left == right
}
pub(crate) fn validate_file_links(m: &cap_fs::Metadata, label: &str) -> Result<(), AttemptError> {
    use cap_fs::_WindowsByHandle;
    if m.number_of_links() != Some(1) {
        return Err(AttemptError::Unsafe(format!(
            "ledger {label} has hard links"
        )));
    }
    Ok(())
}
pub(crate) fn validate_test_barrier_parent(parent: &fs::File) -> Result<(), AttemptError> {
    let metadata = cap_fs::Metadata::from_file(parent)
        .map_err(|e| AttemptError::Unstable(format!("inspect test ledger barrier parent: {e}")))?;
    if !metadata.is_dir() {
        return Err(AttemptError::Unsafe(
            "test ledger barrier parent is not a directory".into(),
        ));
    }

    Err(AttemptError::Unsafe(
        "test ledger barrier is unsupported on Windows without owner-DACL binding".into(),
    ))
}
use crate::ledger::snapshot::{Leaf, Manifest};
pub(crate) fn verify_private_storage(
    storage: &PrivateStorage,
    manifest: &Manifest,
) -> Result<(), AttemptError> {
    let private = &storage.0;
    private
        .verify_before_sqlite(matches!(manifest.wal, Leaf::Present(_)))
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::InvalidData {
                AttemptError::Unsafe(format!("verify owner-private ledger snapshot: {error}"))
            } else {
                AttemptError::Unstable(format!("verify owner-private ledger snapshot: {error}"))
            }
        })
}
