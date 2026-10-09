use crate::export::unsafe_export_error;
use cap_primitives::fs as cap_fs;
use kio_core::{KioError, Result};
use std::fs::File;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DestinationIdentity {
    identity: kio_core::platform::DirectoryHandleIdentity,
}

pub(crate) fn destination_lstat_identity(path: &Path) -> Result<DestinationIdentity> {
    let Some(identity) = kio_core::platform::real_directory_identity(path)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?
    else {
        return Err(unsafe_export_error(
            path,
            "destination identity is unavailable for this filesystem",
        ));
    };
    Ok(DestinationIdentity { identity })
}

pub(crate) fn destination_identity_matches_opened(
    expected: &DestinationIdentity,
    opened: &File,
) -> bool {
    kio_core::platform::directory_handle_identity(opened) == Some(expected.identity)
}

pub(crate) fn validate_export_directory_native(path: &Path) -> Result<()> {
    if !kio_core::platform::directory_is_real(path)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?
    {
        return Err(unsafe_export_error(
            path,
            "destination ancestor is a reparse point",
        ));
    }
    Ok(())
}

pub(crate) fn validate_export_regular_native(
    path: &Path,
    _listed: &std::fs::Metadata,
) -> Result<()> {
    if !kio_core::platform::regular_file_is_safe(path)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?
    {
        return Err(unsafe_export_error(
            path,
            "export refuses a reparse or hard-linked destination file",
        ));
    }
    Ok(())
}

pub(crate) fn validate_export_regular_metadata_native(
    path: &Path,
    listed: &cap_fs::Metadata,
) -> Result<()> {
    use cap_fs::_WindowsByHandle;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    if listed.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || listed.number_of_links() != Some(1)
    {
        return Err(unsafe_export_error(
            path,
            "export refuses a reparse or hard-linked destination file",
        ));
    }
    Ok(())
}

pub(crate) fn validate_export_directory_metadata_native(
    path: &Path,
    listed: &cap_fs::Metadata,
) -> Result<()> {
    use cap_fs::_WindowsByHandle;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    if listed.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(unsafe_export_error(
            path,
            "destination ancestor is a reparse point",
        ));
    }
    Ok(())
}

pub(crate) fn configure_export_temp(options: &mut cap_fs::OpenOptions) {
    use cap_fs::OpenOptionsExt;
    use windows_sys::Win32::Foundation::GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_GENERIC_WRITE};
    options.access_mode(GENERIC_READ | FILE_GENERIC_WRITE | DELETE);
}

pub(crate) fn sync_export_directory(_handle: &File, _path: &Path) -> Result<()> {
    Ok(())
}
pub(crate) fn is_platform_root_alias(_path: &Path) -> bool {
    false
}
