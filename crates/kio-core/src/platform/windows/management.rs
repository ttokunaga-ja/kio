use crate::Result;
use crate::management::{DirectoryIdentity, err};
use std::fs::File;

pub(crate) fn same_case_probe_file(left: &File, right: &File) -> Result<bool> {
    Ok(crate::cas::same_windows_regular_file(left, right))
}

pub(crate) fn file_identity(f: &File) -> Result<DirectoryIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(f.as_raw_handle() as _, &mut information) } == 0 {
        return Err(err("cannot read Windows directory identity"));
    }
    if information.dwFileAttributes
        & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY
        == 0
        || information.dwFileAttributes
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    {
        return Err(err("retained handle is not normal directory"));
    }
    Ok(DirectoryIdentity::Windows {
        volume_serial_number: information.dwVolumeSerialNumber,
        file_index: ((information.nFileIndexHigh as u64) << 32) | information.nFileIndexLow as u64,
    })
}
