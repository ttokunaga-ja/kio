//! Native retained scope source admission and no-follow configuration.

use std::fs::{self as fs, File, OpenOptions};
use std::path::{Component, Path};

use crate::error::{IoResultExt, KioError, Result};
use crate::scope::{scope_file_changed_path, unsafe_store_error};
use crate::store_dir::StoreDirectory;

pub(crate) fn open_bound_scope_file_nofollow(
    root: &File,
    file_name: &str,
    display_path: &Path,
) -> Result<File> {
    let name = Path::new(file_name);
    if name.components().count() != 1
        || !matches!(name.components().next(), Some(Component::Normal(_)))
    {
        return Err(scope_file_changed_path(display_path));
    }
    let directory = StoreDirectory::from_retained(
        root.try_clone().kio_io(display_path)?,
        display_path
            .parent()
            .unwrap_or(Path::new("."))
            .to_path_buf(),
    )?;
    // Source enumeration and each read/staging caller enforce archive limits
    // from this descriptor's metadata before reading, then bound the stream.
    // Let those checks report the scope budget error for oversized inputs while
    // retaining StoreDirectory's no-follow and single-link validation here.
    directory.open_regular_read(name, u64::MAX)
}

pub(crate) fn configure_scope_no_follow(options: &mut OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;
    options.custom_flags(0x0020_0000);
}

/// Scheduled source authority retains Windows single-link admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScheduledSourceIdentity(crate::platform::RegularFileHandleIdentity);

/// Retained metadata authority; separate from scheduled source admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundMetadataObservation {
    pub(crate) identity: ScheduledSourceIdentity,
    pub(crate) len: u64,
    pub(crate) nlink: u64,
    pub(crate) digest: String,
}

pub(crate) fn scheduled_source_identity(
    file: &File,
) -> Result<crate::platform::ScheduledSourceIdentity> {
    crate::platform::regular_file_handle_identity(file)
        .map(ScheduledSourceIdentity)
        .ok_or_else(|| KioError::schema("scheduled file is nonregular, reparse, or hardlinked"))
}

pub(crate) fn same_scope_file_identity(opened: &File, path: &File) -> bool {
    crate::platform::cas_identity::same_windows_regular_file(opened, path)
}

pub(crate) fn configure_private_stage(options: &mut OpenOptions) {
    let _ = options;
}

pub(crate) fn set_scope_directory_permissions(dir: &Path) -> Result<()> {
    let _ = dir;
    Ok(())
}

pub(crate) fn validate_scope_path_private(metadata: &fs::Metadata, kio_dir: &Path) -> Result<()> {
    let _ = (metadata, kio_dir);
    Ok(())
}

pub(crate) fn open_scope_file_nofollow(path: &Path) -> Result<File> {
    let before = fs::symlink_metadata(path).kio_io(path)?;
    if before.file_type().is_symlink() || !before.file_type().is_file() {
        return Err(scope_file_changed_path(path));
    }

    let mut options = OpenOptions::new();
    options.read(true);
    configure_scope_no_follow(&mut options);
    let file = options.open(path).kio_io(path)?;
    let opened = file.metadata().kio_io(path)?;
    let after = fs::symlink_metadata(path).kio_io(path)?;
    let same_identity = {
        let mut verification_options = OpenOptions::new();
        verification_options.read(true);
        configure_scope_no_follow(&mut verification_options);
        let verification = verification_options.open(path).kio_io(path)?;
        verification.metadata().kio_io(path)?.is_file()
            && same_scope_file_identity(&file, &verification)
    };
    if !opened.is_file()
        || after.file_type().is_symlink()
        || !after.file_type().is_file()
        || !same_identity
    {
        return Err(scope_file_changed_path(path));
    }
    Ok(file)
}

pub(crate) fn validate_scope_store_private(kio: &File, label: &Path) -> Result<()> {
    let metadata = kio
        .metadata()
        .map_err(|error| KioError::io(error.to_string(), label.display().to_string()))?;
    if !metadata.is_dir() {
        return Err(unsafe_store_error(
            label,
            "retained .kio handle is not a directory",
        ));
    }
    super::private_fs::verify_owner_private_handle(kio)?;
    Ok(())
}
