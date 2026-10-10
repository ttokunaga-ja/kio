use crate::ledger::lifecycle_fs::{
    FsSession, LIFECYCLE_LOCK, LOCK_BYTES, MAX_LOCK_BYTES, SqliteFiles, boundary_changed, contract,
    core_private,
};
use crate::{PipelineError, Result};
use kio_core::store_dir::StoreDirectory;
use rusqlite::OpenFlags;
use std::{
    fs::File,
    path::{Component, Path, PathBuf},
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
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata().map_err(|error| PipelineError::Io {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    // SAFETY: geteuid has no arguments and no memory-safety preconditions.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(contract(
            "KIO-E-LEDGER-PRIVATE-001",
            format!("ledger parent is not owner-private: {}", path.display()),
        ));
    }
    Ok(())
}

pub(crate) fn verify_existing_ancestor_path(
    path: &Path,
    control: &kio_core::control::ControlHandle,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    // Validate every already-existing component before creating anything
    // below it. Root-owned sticky directories (for example /private/tmp) are
    // the sole writable-by-others exception, matching private_fs.
    // SAFETY: geteuid has no arguments and no memory-safety preconditions.
    let uid = unsafe { libc::geteuid() };
    let mut prefix = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(value) => prefix.push(value),
            _ => {
                return Err(contract(
                    "KIO-E-LEDGER-PATH-001",
                    "ledger parent contains an unsafe component",
                ));
            }
        }
        let directory =
            StoreDirectory::open_with_control(&prefix, control.clone()).map_err(core_private)?;
        let metadata = directory
            .root_handle()
            .metadata()
            .map_err(|error| PipelineError::Io {
                path: prefix.display().to_string(),
                message: error.to_string(),
            })?;
        let owner_trusted = metadata.uid() == uid || metadata.uid() == 0;
        let writable_by_others = metadata.mode() & 0o022 != 0;
        let root_owned_sticky = metadata.uid() == 0 && metadata.mode() & 0o1000 != 0;
        if !metadata.is_dir() || !owner_trusted || (writable_by_others && !root_owned_sticky) {
            return Err(contract(
                "KIO-E-LEDGER-PRIVATE-001",
                format!(
                    "ledger creation ancestor is not privately controlled: {}",
                    prefix.display()
                ),
            ));
        }
    }
    Ok(())
}

pub(crate) fn path_component_is_unsafe(component: Component<'_>) -> bool {
    matches!(
        component,
        Component::CurDir | Component::ParentDir | Component::Prefix(_)
    )
}
pub(crate) fn validate_lock_contents_before_acquire(parent: &StoreDirectory) -> Result<()> {
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
