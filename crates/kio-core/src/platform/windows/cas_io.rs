use std::fs::{File, OpenOptions};
use std::path::Path;

use super::cas_identity::windows_file_information;
use crate::cas::{
    ensure_bound_directory, non_regular_object_error, same_windows_cas_file_components,
    same_windows_repair_quarantine_file_components,
};
use crate::error::{IoResultExt, KioError, Result};

/// Windows cannot flush a directory handle. The file body has already been
/// flushed before publication; validate the retained capability instead of
/// claiming POSIX directory-entry durability that Windows cannot provide.
pub(crate) fn sync_bound_directory(directory: &File, label: &Path) -> Result<()> {
    ensure_bound_directory(directory, label)
}

pub(crate) fn same_open_file(left: &File, right: &File) -> Result<bool> {
    Ok(same_windows_repair_quarantine_file_components(
        windows_file_information(left),
        windows_file_information(right),
    ))
}

pub(crate) fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: both UTF-16 buffers are NUL-terminated and live for the call.
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        Err(KioError::io(
            std::io::Error::last_os_error().to_string(),
            "raw repair destination",
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn open_windows_regular_nofollow_observation(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    configure_no_follow(&mut options);
    let file = options.open(path).kio_io(path)?;
    if !windows_file_information(&file)
        .is_some_and(|information| information.is_regular_file() && information.is_single_link())
    {
        return Err(non_regular_object_error(path));
    }
    Ok(file)
}

pub(crate) fn configure_no_follow(options: &mut OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;
    options.custom_flags(0x0020_0000);
}

pub(crate) fn same_windows_cas_file(opened: &File, path: &File) -> bool {
    same_windows_cas_file_components(
        windows_file_information(opened),
        windows_file_information(path),
    )
}

pub(crate) fn same_windows_cas_file_triplet(before: &File, opened: &File, after: &File) -> bool {
    same_windows_cas_file(before, opened) && same_windows_cas_file(opened, after)
}
