use crate::Result;
use crate::store_dir::StoreDirectory;
use crate::store_dir::atomic::{
    AtomicRemovalExpectation, FileSystemIdentity, REMOVE_READY, REMOVED, failure, file_id,
    hash_file, io_failure, remove_if_present,
};
use std::{
    ffi::OsStr,
    fs::{File, Metadata},
    path::Path,
};
pub(crate) fn ensure_private_workspace(workspace: &StoreDirectory) -> Result<()> {
    let metadata = workspace
        .root_handle()
        .metadata()
        .map_err(|e| io_failure("cannot inspect atomic workspace", e))?;
    if !metadata.is_dir() {
        return Err(failure("atomic workspace is not a directory"));
    }
    {
        use std::os::unix::fs::MetadataExt as _;
        let uid = unsafe { libc::geteuid() };
        if metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            return Err(failure("atomic workspace is not owner-private"));
        }
    }
    Ok(())
}

/// Remove only Kio's own private publication staging leaf.
pub(crate) fn remove_publication_residue(workspace: &StoreDirectory, name: &Path) -> Result<()> {
    workspace.remove_file(name)
}

pub(crate) fn atomic_file_identity(file: &File) -> Result<FileSystemIdentity> {
    let metadata = file
        .metadata()
        .map_err(|e| io_failure("cannot inspect retained file", e))?;
    if !metadata.is_file() {
        return Err(failure("atomic source is not a regular file"));
    }
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.nlink() != 1 {
            return Err(failure("atomic source is not a single-link regular file"));
        }
        Ok(FileSystemIdentity::Unix {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

pub(crate) fn validate_hash_metadata(before: &Metadata, after: &Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    if before.ctime() != after.ctime() || before.ctime_nsec() != after.ctime_nsec() {
        return Err(failure("atomic source changed while it was hashed"));
    }
    Ok(())
}

pub(crate) fn verify_removal_owner(_file: &File) -> Result<()> {
    Ok(())
}
use crate::store_dir::Publication;

pub(crate) fn finish_remove_retirement(
    parent: &StoreDirectory,
    leaf: &OsStr,
    workspace: &StoreDirectory,
    source: Option<File>,
    removed: bool,
    intent: AtomicRemovalExpectation,
) -> Result<bool> {
    match (source, removed) {
        (Some(mut source), false) => {
            if file_id(&source)? != intent.source_id
                || hash_file(&mut source, intent.max_bytes)?
                    != (intent.sha256.clone(), intent.length)
            {
                return Err(failure("removal source does not match its ready intent"));
            }
            drop(source);
            parent.rename_regular_between_raw(
                Path::new(leaf),
                workspace,
                Path::new(REMOVED),
                Publication::CreateOnly,
            )?;
            workspace
                .control()
                .checkpoint(crate::durability::DurabilityPoint::AtomicRemoveQuarantined)?;
        }
        (None, true) => {}
        (None, false) => {
            remove_if_present(workspace, REMOVE_READY)?;
            return Ok(true);
        }
        (Some(_), true) => return Err(failure("both removal source and quarantine are present")),
    }
    {
        let mut quarantined =
            workspace.open_regular_for_removal(Path::new(REMOVED), intent.max_bytes)?;
        if file_id(&quarantined)? != intent.source_id
            || hash_file(&mut quarantined, intent.max_bytes)? != (intent.sha256, intent.length)
        {
            return Err(failure(
                "removal quarantine does not match its ready intent",
            ));
        }
        drop(quarantined);
        remove_if_present(workspace, REMOVED)?;
        workspace
            .control()
            .checkpoint(crate::durability::DurabilityPoint::AtomicRemoveDeleted)?;
        remove_if_present(workspace, REMOVE_READY)?;
        Ok(true)
    }
}
