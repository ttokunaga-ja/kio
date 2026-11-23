use crate::Result;
use crate::platform::removal_semantics::{annotate, attributes};
use crate::platform::store as platform;
use crate::store_dir::planned_removal::{Identity, identity};
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
        .map_err(|error| annotate(error, "rename to or from quarantine", attributes(held)))
}

pub(crate) fn retire_by_disposition(
    file: &File,
    directory: bool,
    expected: &Identity,
    ignore_read_only: bool,
    label: &Path,
) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
        FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO_EX, FileDispositionInfoEx,
        SetFileInformationByHandle,
    };
    if &identity(file, directory, label)? != expected {
        return Err(err(label, "retirement handle identity changed"));
    }
    let mut flags = FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS;
    if ignore_read_only {
        flags |= FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE;
    }
    let info = FILE_DISPOSITION_INFO_EX { Flags: flags };
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfoEx,
            &info as *const _ as _,
            std::mem::size_of_val(&info) as u32,
        )
    } == 0
    {
        let error = std::io::Error::last_os_error();
        return Err(annotate(
            ioerr(label, error),
            "set delete disposition",
            attributes(file),
        ));
    }
    if directory && &identity(file, true, label)? != expected {
        return Err(err(label, "retired directory handle identity changed"));
    }
    Ok(())
}
