use crate::Result;
use crate::platform::store as platform;
use crate::store_dir::planned_removal::identity;
use crate::store_dir::{err, ioerr};
use std::{ffi::OsStr, fs::File, path::Path};

pub(crate) fn rename(
    parent: &File,
    _source: &OsStr,
    destination: &OsStr,
    held: &File,
    label: &Path,
) -> Result<()> {
    platform::rename_retained_handle(held, parent, destination, false, label)
}

pub(crate) fn retire_directory(file: &File, label: &Path) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
        FILE_DISPOSITION_INFO_EX, FileDispositionInfoEx, SetFileInformationByHandle,
    };
    let expected = identity(file, true, label)?;
    let info = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    };
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfoEx,
            &info as *const _ as _,
            std::mem::size_of_val(&info) as u32,
        )
    } == 0
    {
        return Err(ioerr(label, std::io::Error::last_os_error()));
    }
    if identity(file, true, label)? != expected {
        return Err(err(label, "retired directory handle identity changed"));
    }
    Ok(())
}
