use std::fs::{self, File, OpenOptions};
use std::path::Path;

use super::cas_identity::windows_file_information;
use crate::cas::{
    bound_io_or_not_found, ensure_bound_directory, non_regular_object_error,
    same_windows_cas_file_components, same_windows_repair_quarantine_file_components,
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

pub(crate) fn open_regular_nofollow(path: &Path) -> Result<File> {
    let before = fs::symlink_metadata(path).kio_io(path)?;
    if !before.file_type().is_file() || before.file_type().is_symlink() {
        return Err(non_regular_object_error(path));
    }
    #[cfg(unix)]
    if !is_single_link_regular_metadata(&before) {
        return Err(non_regular_object_error(path));
    }

    let mut options = OpenOptions::new();
    options.read(true);
    configure_no_follow(&mut options);
    #[cfg(windows)]
    let before_handle = options.open(path).kio_io(path)?;
    let file = options.open(path).kio_io(path)?;
    let opened = file.metadata().kio_io(path)?;
    let after = fs::symlink_metadata(path).kio_io(path)?;
    #[cfg(windows)]
    let same_identity = {
        let mut verification_options = OpenOptions::new();
        verification_options.read(true);
        configure_no_follow(&mut verification_options);
        let verification = verification_options.open(path).kio_io(path)?;
        verification.metadata().kio_io(path)?.is_file()
            && same_windows_cas_file_triplet(&before_handle, &file, &verification)
    };
    #[cfg(not(windows))]
    let same_identity = same_file_identity(&before, &opened) && same_file_identity(&opened, &after);
    if !opened.is_file()
        || !after.file_type().is_file()
        || after.file_type().is_symlink()
        || !same_identity
    {
        return Err(non_regular_object_error(path));
    }
    #[cfg(unix)]
    {
        if !is_single_link_regular_metadata(&opened) || !is_single_link_regular_metadata(&after) {
            return Err(non_regular_object_error(path));
        }
    }
    Ok(file)
}

/// Open an existing metadata file for reading and mutation through the same
/// no-follow, single-link, identity-checked boundary as [`open_regular_nofollow`].
pub(crate) fn open_regular_nofollow_read_write(path: &Path) -> Result<File> {
    let before = fs::symlink_metadata(path).kio_io(path)?;
    if !before.file_type().is_file() || before.file_type().is_symlink() {
        return Err(non_regular_object_error(path));
    }
    #[cfg(unix)]
    if !is_single_link_regular_metadata(&before) {
        return Err(non_regular_object_error(path));
    }

    let mut options = OpenOptions::new();
    options.read(true).write(true);
    configure_no_follow(&mut options);
    #[cfg(windows)]
    let before_handle = open_windows_regular_nofollow_observation(path)?;
    let file = options.open(path).kio_io(path)?;
    #[cfg(windows)]
    validate_open_regular_nofollow_with_windows_before(path, &file, &before_handle)?;
    #[cfg(not(windows))]
    validate_open_regular_nofollow_with_before(path, &file, &before)?;
    Ok(file)
}

/// Open an existing ledger for append, or atomically create a new real regular
/// single-link ledger.  In particular, a dangling symlink is an existing path,
/// not a missing ledger to be created through.
pub(crate) fn open_or_create_regular_nofollow_append(path: &Path) -> Result<File> {
    for _ in 0..4 {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                    return Err(non_regular_object_error(path));
                }
                #[cfg(unix)]
                if !is_single_link_regular_metadata(&metadata) {
                    return Err(non_regular_object_error(path));
                }
                let mut options = OpenOptions::new();
                options.append(true);
                configure_no_follow(&mut options);
                #[cfg(windows)]
                let before_handle = open_windows_regular_nofollow_observation(path)?;
                let file = options.open(path).kio_io(path)?;
                #[cfg(windows)]
                validate_open_regular_nofollow_with_windows_before(path, &file, &before_handle)?;
                #[cfg(not(windows))]
                validate_open_regular_nofollow_with_before(path, &file, &metadata)?;
                return Ok(file);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut options = OpenOptions::new();
                options.write(true).append(true).create_new(true);
                configure_no_follow(&mut options);
                match options.open(path) {
                    Ok(file) => {
                        validate_open_regular_nofollow(path, &file)?;
                        return Ok(file);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error).kio_io(path),
                }
            }
            Err(error) => return Err(error).kio_io(path),
        }
    }
    Err(KioError::io(
        "could not safely create ledger file after concurrent path changes",
        path.display().to_string(),
    ))
}

pub(crate) fn validate_open_regular_nofollow(path: &Path, file: &File) -> Result<()> {
    let opened = file.metadata().kio_io(path)?;
    let after = fs::symlink_metadata(path).kio_io(path)?;
    #[cfg(windows)]
    let same_identity = {
        let mut verification_options = OpenOptions::new();
        verification_options.read(true);
        configure_no_follow(&mut verification_options);
        let verification = verification_options.open(path).kio_io(path)?;
        verification.metadata().kio_io(path)?.is_file()
            && same_windows_cas_file(file, &verification)
    };
    #[cfg(not(windows))]
    let same_identity = same_file_identity(&opened, &after);
    if !opened.is_file()
        || !after.file_type().is_file()
        || after.file_type().is_symlink()
        || !same_identity
    {
        return Err(non_regular_object_error(path));
    }
    #[cfg(unix)]
    {
        if !is_single_link_regular_metadata(&opened) || !is_single_link_regular_metadata(&after) {
            return Err(non_regular_object_error(path));
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub(crate) fn validate_open_regular_nofollow_with_before(
    path: &Path,
    file: &File,
    before: &fs::Metadata,
) -> Result<()> {
    let opened = file.metadata().kio_io(path)?;
    if !same_file_identity(before, &opened) {
        return Err(non_regular_object_error(path));
    }
    #[cfg(unix)]
    if !is_single_link_regular_metadata(before) || !is_single_link_regular_metadata(&opened) {
        return Err(non_regular_object_error(path));
    }
    validate_open_regular_nofollow(path, file)
}

#[cfg(windows)]
fn validate_open_regular_nofollow_with_windows_before(
    path: &Path,
    file: &File,
    before: &File,
) -> Result<()> {
    let mut options = OpenOptions::new();
    options.read(true);
    configure_no_follow(&mut options);
    let after = options.open(path).kio_io(path)?;
    if !same_windows_cas_file_triplet(before, file, &after) {
        return Err(non_regular_object_error(path));
    }
    validate_open_regular_nofollow(path, file)
}

pub(crate) fn bound_metadata_single_link(metadata: &cap_primitives::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use cap_primitives::fs::MetadataExt;
        metadata.nlink() == 1
    }
    #[cfg(windows)]
    {
        let _ = metadata;
        // The Windows check is done against the opened handles below, where
        // link count and reparse attributes are available by handle.
        true
    }
}

pub(crate) fn same_bound_regular_entry(
    _parent: &File,
    leaf: &Path,
    opened: &File,
    before: &cap_primitives::fs::Metadata,
    after: &cap_primitives::fs::Metadata,
) -> Result<bool> {
    #[cfg(unix)]
    {
        use cap_primitives::fs::MetadataExt;
        let opened = cap_primitives::fs::Metadata::from_file(opened)
            .map_err(|error| KioError::io(error.to_string(), leaf.display().to_string()))?;
        Ok(opened.dev() == before.dev()
            && opened.ino() == before.ino()
            && opened.dev() == after.dev()
            && opened.ino() == after.ino())
    }
    #[cfg(windows)]
    {
        let _ = (before, after);
        let mut options = cap_primitives::fs::OpenOptions::new();
        options
            .read(true)
            ._cap_fs_ext_follow(cap_primitives::fs::FollowSymlinks::No);
        let verification = cap_primitives::fs::open(_parent, leaf, &options)
            .map_err(|error| bound_io_or_not_found(error, &leaf.display().to_string()))?;
        Ok(same_windows_cas_file(opened, &verification))
    }
}

pub(crate) fn observe_regular_file_stability(
    metadata: &fs::Metadata,
    path: &Path,
) -> Result<crate::platform::RegularFileStability> {
    Ok(crate::platform::RegularFileStability {
        len: metadata.len(),
        modified: metadata.modified().kio_io(path)?,
        changed: None,
    })
}
