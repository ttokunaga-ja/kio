use crate::Result;
use crate::management::{DirectoryIdentity, err};
use std::fs::File;

pub(crate) fn same_case_probe_file(left: &File, right: &File) -> Result<bool> {
    Ok(crate::platform::cas_identity::same_windows_regular_file(
        left, right,
    ))
}

pub(crate) fn file_identity(f: &File) -> Result<DirectoryIdentity> {
    let information = super::primitives::file_information(f)
        .map_err(|_| err("cannot read Windows directory identity"))?;
    if information.file_attributes
        & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY
        == 0
        || information.file_attributes
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    {
        return Err(err("retained handle is not normal directory"));
    }
    Ok(DirectoryIdentity::Windows {
        volume_serial_number: information.volume_serial_number,
        file_index: information.file_index,
    })
}

pub(crate) const NATIVE_IDENTITY_FAMILY: crate::platform::NativeIdentityFamily =
    crate::platform::NativeIdentityFamily::Windows;
