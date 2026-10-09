use std::{
    fs::{File, OpenOptions},
    io,
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    path::Path,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FileBasicInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
};

/// Raw handle information returned by one Windows file-information query.
///
/// Available on Windows. No file-type or link-count policy is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileInformation {
    pub volume_serial_number: u32,
    pub file_index: u64,
    pub number_of_links: u32,
    pub file_attributes: u32,
    pub file_size: u64,
    pub last_write_time: u64,
}

/// Query the raw signed Windows ChangeTime without applying time policy.
/// Available on Windows.
pub fn file_change_time(file: &File) -> io::Result<i64> {
    let mut basic = FILE_BASIC_INFO::default();
    // SAFETY: `file` owns a live handle and `basic` is writable storage with
    // the exact layout and size required by FileBasicInfo.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileBasicInfo,
            (&mut basic as *mut FILE_BASIC_INFO).cast(),
            std::mem::size_of::<FILE_BASIC_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(basic.ChangeTime)
}

/// Query raw Windows handle attributes, identity, links, size and write time.
/// Available on Windows.
pub fn file_information(file: &File) -> io::Result<FileInformation> {
    let mut raw = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `file` owns a live handle and `raw` is writable storage with the
    // Win32-required layout. The call does not retain the buffer.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut raw) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(FileInformation {
        volume_serial_number: raw.dwVolumeSerialNumber,
        file_index: (u64::from(raw.nFileIndexHigh) << 32) | u64::from(raw.nFileIndexLow),
        number_of_links: raw.nNumberOfLinks,
        file_attributes: raw.dwFileAttributes,
        file_size: (u64::from(raw.nFileSizeHigh) << 32) | u64::from(raw.nFileSizeLow),
        last_write_time: (u64::from(raw.ftLastWriteTime.dwHighDateTime) << 32)
            | u64::from(raw.ftLastWriteTime.dwLowDateTime),
    })
}

/// Open a Windows metadata handle without following the final reparse point.
///
/// Available on Windows. Uses READ_ATTRIBUTES, READ/WRITE/DELETE sharing, and
/// BACKUP_SEMANTICS with OPEN_REPARSE_POINT; no type or link policy is applied.
pub fn open_metadata_nofollow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
    options.open(path)
}
