//! Stage mutations retain the verified, delete-capable handle throughout.
use crate::{
    Result,
    store_dir::{Publication, StageIdentity, StoreDirectory, atomic::io_failure},
};
use cap_primitives::fs::{self as cap_fs, OpenOptionsExt};
use std::{fs::File, os::windows::io::AsRawHandle, path::Path};
use windows_sys::Win32::{
    Foundation::GENERIC_READ,
    Storage::FileSystem::{
        DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, SYNCHRONIZE,
    },
};

pub(crate) fn create_file(directory: &StoreDirectory, leaf: &Path) -> Result<File> {
    let parts = crate::store_dir::relative_components(leaf, false, directory.path())?;
    if parts.len() != 1 {
        return Err(crate::KioError::invalid_usage(
            "stage must be a direct leaf",
        ));
    }
    super::private_fs::create_new_private_at(
        &directory.root_handle(),
        parts[0],
        super::private_fs::NewPrivateObjectKind::StageFile,
    )
    .map_err(|e| crate::store_dir::stage::stage_io("cannot create private stage", e))
}
pub(crate) fn open_file(directory: &StoreDirectory, leaf: &Path) -> Result<File> {
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        .access_mode(GENERIC_READ | DELETE | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    cap_fs::open(&directory.root_handle(), leaf, &options)
        .map_err(|e| io_failure("cannot retain stage for mutation", e))
}

pub(crate) fn publish(
    directory: &StoreDirectory,
    _source: &Path,
    held: &File,
    target: &StoreDirectory,
    leaf: &Path,
    publication: Publication,
    _identity: &StageIdentity,
) -> Result<()> {
    if publication == Publication::Replace
        || (publication == Publication::Upsert && target.contains_entry(leaf)?)
    {
        target.open_regular_read(leaf, u64::MAX)?;
    }
    super::store::rename_retained_handle(
        held,
        &target.root_handle(),
        leaf.as_os_str(),
        publication != Publication::CreateOnly,
        target.path(),
    )?;
    target.sync_stage_destination()?;
    directory.sync()
}

fn retire(held: &File) -> Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
        FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO_EX, FileDispositionInfoEx,
        SetFileInformationByHandle,
    };
    let info = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    if unsafe {
        SetFileInformationByHandle(
            held.as_raw_handle(),
            FileDispositionInfoEx,
            &info as *const _ as _,
            std::mem::size_of_val(&info) as u32,
        )
    } == 0
    {
        return Err(io_failure(
            "cannot retire retained stage handle",
            std::io::Error::last_os_error(),
        ));
    }
    Ok(())
}
pub(crate) fn discard_file(
    _directory: &StoreDirectory,
    _source: &Path,
    held: &File,
    _identity: Option<&StageIdentity>,
) -> Result<()> {
    retire(held)
}
pub(crate) fn discard_empty_directory(
    directory: &StoreDirectory,
    source: &Path,
    held: &File,
) -> Result<()> {
    // Persistent directory handles are read-only. Retain DELETE access to the
    // named child, then compare its identity before retiring that handle.
    let mut options = cap_fs::OpenOptions::new();
    options
        .access_mode(DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mutator = cap_fs::open(&directory.root_handle(), source, &options)
        .map_err(|e| io_failure("cannot retain stage directory for retirement", e))?;
    let Some(identity) = crate::platform::directory_handle_identity(held) else {
        return Err(crate::KioError::schema("invalid retained stage directory"));
    };
    if crate::platform::directory_handle_identity(&mutator) != Some(identity) {
        return Err(crate::KioError::schema(
            "stage directory no longer matches the retained handle; preserved",
        ));
    }
    retire(&mutator)
}

// Windows mutations use the retained handle directly and create no capture.
pub(crate) fn capture_name(_identity: &StageIdentity) -> Result<Option<std::path::PathBuf>> {
    Ok(None)
}
pub(crate) fn inspect_capture(
    _parent: &StoreDirectory,
    _name: &Path,
    _identity: &StageIdentity,
) -> Result<bool> {
    Ok(false)
}
pub(crate) fn recover_for_publication(
    _: &StoreDirectory,
    _: &Path,
    _: &StageIdentity,
) -> Result<()> {
    Ok(())
}

pub(crate) fn recover_file(
    _parent: &StoreDirectory,
    _source: &Path,
    _identity: &StageIdentity,
) -> Result<bool> {
    Ok(false)
}
