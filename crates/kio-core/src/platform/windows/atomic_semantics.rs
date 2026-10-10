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
    super::private_fs::verify_owner_private_handle(&workspace.root_handle())?;
    Ok(())
}

pub(crate) fn atomic_file_identity(file: &File) -> Result<FileSystemIdentity> {
    let metadata = file
        .metadata()
        .map_err(|e| io_failure("cannot inspect retained file", e))?;
    if !metadata.is_file() {
        return Err(failure("atomic source is not a regular file"));
    }
    {
        crate::platform::regular_file_handle_identity(file)
            .map(|id| {
                let (volume_serial_number, file_index) = id.atomic_recovery_components();
                FileSystemIdentity::Windows {
                    volume_serial_number,
                    file_index,
                }
            })
            .ok_or_else(|| failure("atomic source has no single-link identity"))
    }
}

pub(crate) fn validate_hash_metadata(_before: &Metadata, _after: &Metadata) -> Result<()> {
    Ok(())
}

pub(crate) fn verify_removal_owner(file: &File) -> Result<()> {
    super::private_fs::verify_current_owner_handle(file)
}

pub(crate) fn finish_remove_retirement(
    parent: &StoreDirectory,
    leaf: &OsStr,
    workspace: &StoreDirectory,
    source: Option<File>,
    removed: bool,
    intent: AtomicRemovalExpectation,
) -> Result<bool> {
    let _ = leaf;
    let (mut quarantined, contents_already_verified) = match (source, removed) {
        (Some(mut source), false) => {
            super::private_fs::verify_current_owner_handle(&source)?;
            if file_id(&source)? != intent.source_id
                || hash_file(&mut source, intent.max_bytes)?
                    != (intent.sha256.clone(), intent.length)
            {
                return Err(failure("removal source does not match its ready intent"));
            }
            crate::platform::move_verified_regular_handle_to_raw(
                parent,
                &source,
                workspace,
                Path::new(REMOVED),
            )?;
            if file_id(&source)? != intent.source_id {
                return Err(failure(
                    "moved removal source no longer matches its ready intent",
                ));
            }
            workspace
                .control()
                .checkpoint(crate::durability::DurabilityPoint::AtomicRemoveQuarantined)?;
            (source, true)
        }
        (None, true) => (
            workspace.open_regular_for_removal(Path::new(REMOVED), intent.max_bytes)?,
            false,
        ),
        (None, false) => {
            remove_if_present(workspace, REMOVE_READY)?;
            return Ok(true);
        }
        (Some(_), true) => return Err(failure("both removal source and quarantine are present")),
    };
    {
        super::private_fs::verify_current_owner_handle(&quarantined)?;
        if file_id(&quarantined)? != intent.source_id
            || (!contents_already_verified
                && hash_file(&mut quarantined, intent.max_bytes)? != (intent.sha256, intent.length))
        {
            return Err(failure(
                "removal quarantine does not match its ready intent",
            ));
        }
        super::private_fs::restrict_existing_private_handle(&quarantined)?;
        drop(quarantined);
        remove_if_present(workspace, REMOVED)?;
        workspace
            .control()
            .checkpoint(crate::durability::DurabilityPoint::AtomicRemoveDeleted)?;
        remove_if_present(workspace, REMOVE_READY)?;
        Ok(true)
    }
}
