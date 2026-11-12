use crate::ledger::lifecycle_fs::{
    FsSession, LIFECYCLE_LOCK, LOCK_BYTES, MAX_LOCK_BYTES, SqliteFiles, boundary_changed, contract,
    core_private,
};
use crate::{PipelineError, Result};
use kio_core::store_dir::StoreDirectory;
use rusqlite::OpenFlags;
use std::{
    fs::File,
    path::{Component, Path},
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RegularFileIdentity {
    device: u64,
    inode: u64,
}

pub(crate) fn directory_identity(file: &File, path: &Path) -> Result<DirectoryIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata().map_err(|error| PipelineError::Io {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    if !metadata.is_dir() {
        return Err(contract(
            "KIO-E-LEDGER-PRIVATE-001",
            "ledger parent handle is not a directory",
        ));
    }
    Ok(DirectoryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

pub(crate) fn regular_file_identity(file: &File, path: &Path) -> Result<RegularFileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata().map_err(|error| PipelineError::Io {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(contract(
            "KIO-E-LEDGER-PRIVATE-001",
            format!(
                "ledger leaf is not a single-link regular file: {}",
                path.display()
            ),
        ));
    }
    Ok(RegularFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

pub(crate) fn verify_private_parent_handle(file: &File, path: &Path) -> Result<()> {
    let retained = file.try_clone().map_err(|error| PipelineError::Io {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    let directory =
        StoreDirectory::from_retained(retained, path.to_path_buf()).map_err(core_private)?;
    kio_core::private_fs::verify_private_directory_handle(&directory).map_err(core_private)
}

pub(crate) fn verify_existing_ancestor_path(
    path: &Path,
    control: &kio_core::control::ControlHandle,
) -> Result<()> {
    // This is an external creation parent, not a Kio-owned ledger directory.
    // Retain no-follow identity without judging or repairing its permissions.
    kio_core::private_fs::verify_private_creation_parent_with_control(path, control)
        .map(|_| ())
        .map_err(core_private)
}

pub(crate) fn path_component_is_unsafe(component: Component<'_>) -> bool {
    matches!(
        component,
        Component::CurDir | Component::ParentDir | Component::Prefix(_)
    )
}
pub(crate) fn validate_lock_contents_before_acquire(parent: &StoreDirectory) -> Result<()> {
    kio_core::private_fs::verify_private_directory_handle(parent).map_err(core_private)?;
    let observed =
        kio_core::private_fs::read_private_file_at(parent, LIFECYCLE_LOCK, MAX_LOCK_BYTES)
            .map_err(core_private)?;
    if observed != LOCK_BYTES {
        return Err(contract(
            "KIO-E-LEDGER-PRIVATE-001",
            "ledger lifecycle lock has unexpected contents",
        ));
    }
    Ok(())
}
pub(crate) fn validate_open_lock_before_acquire(_lock: &File) -> Result<()> {
    Ok(())
}
pub(crate) fn revalidate_lock_contents(
    parent: &StoreDirectory,
    _lock: &File,
    _lock_identity: &RegularFileIdentity,
    _parent_path: &Path,
) -> Result<()> {
    kio_core::private_fs::verify_private_directory_handle(parent).map_err(core_private)?;
    let observed =
        kio_core::private_fs::read_private_file_at(parent, LIFECYCLE_LOCK, MAX_LOCK_BYTES)
            .map_err(core_private)?;
    if observed != LOCK_BYTES {
        return Err(boundary_changed("ledger lifecycle lock contents changed"));
    }
    Ok(())
}
pub(crate) fn prepare_sqlite_sidecars(
    _session: &FsSession,
    _files: &mut SqliteFiles,
    _flags: OpenFlags,
) -> Result<()> {
    Ok(())
}
