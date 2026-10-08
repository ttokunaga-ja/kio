//! Native retained scope reads and POSIX no-follow configuration.

use std::fs::{self as fs, File, OpenOptions};
use std::io::Read;
use std::path::{Component, Path};

use sha2::{Digest, Sha256};

use crate::cas::lower_hex;
use crate::error::{IoResultExt, KioError, Result};
use crate::scope::{scope_file_changed_path, unsafe_store_error};

/// Read a bounded regular metadata leaf without following any public path and
/// retain enough evidence to reject same-byte replacement at a later writer
/// boundary.
pub(crate) fn read_bound_regular_text_observed_at(
    kio: &File,
    relative: &str,
    max_bytes: u64,
) -> Result<(String, BoundMetadataObservation)> {
    use cap_primitives::fs::{self as cap_fs, MetadataExt};

    let path = Path::new(relative);
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(KioError::invalid_usage(
            "bound metadata path must contain only normal components",
        ));
    }
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let leaf = path
        .file_name()
        .ok_or_else(|| KioError::invalid_usage("bound metadata path must name a regular file"))?;
    let mut directory = kio
        .try_clone()
        .map_err(|error| KioError::io(error.to_string(), relative))?;
    for component in parent.components() {
        let Component::Normal(component) = component else {
            continue;
        };
        directory = cap_fs::open_dir_nofollow(&directory, Path::new(component))
            .map_err(|error| KioError::io(error.to_string(), relative))?;
    }
    let before = cap_fs::stat(&directory, Path::new(leaf), cap_fs::FollowSymlinks::No)
        .map_err(|error| KioError::io(error.to_string(), relative))?;
    if !before.is_file() || before.nlink() != 1 || before.len() > max_bytes {
        return Err(KioError::schema(
            "bound metadata must be a bounded single-link regular file",
        ));
    }
    let mut options = cap_fs::OpenOptions::new();
    options.read(true);
    options._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut file = cap_fs::open(&directory, Path::new(leaf), &options)
        .map_err(|error| KioError::io(error.to_string(), relative))?;
    let opened = cap_fs::Metadata::from_file(&file)
        .map_err(|error| KioError::io(error.to_string(), relative))?;
    if !opened.is_file()
        || opened.nlink() != 1
        || opened.len() > max_bytes
        || opened.dev() != before.dev()
        || opened.ino() != before.ino()
        || opened.len() != before.len()
    {
        return Err(KioError::schema("bound metadata changed while opening"));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(opened.len()).unwrap_or(0));
    (&mut file)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| KioError::io(error.to_string(), relative))?;
    let after = cap_fs::stat(&directory, Path::new(leaf), cap_fs::FollowSymlinks::No)
        .map_err(|error| KioError::io(error.to_string(), relative))?;
    if bytes.len() as u64 != opened.len()
        || after.dev() != opened.dev()
        || after.ino() != opened.ino()
        || after.nlink() != opened.nlink()
        || after.len() != opened.len()
    {
        return Err(KioError::schema("bound metadata changed while reading"));
    }
    let text =
        String::from_utf8(bytes.clone()).map_err(|error| KioError::schema(error.to_string()))?;
    Ok((
        text,
        BoundMetadataObservation {
            dev: opened.dev(),
            ino: opened.ino(),
            len: opened.len(),
            nlink: opened.nlink(),
            digest: lower_hex(&Sha256::digest(&bytes)),
        },
    ))
}

pub(crate) fn effective_uid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid has no preconditions and only returns process metadata.
    unsafe { geteuid() }
}

/// Open one direct source child using the scope descriptor retained by an
/// internal child index. The public path is diagnostic-only and never used for
/// lookup, so a post-bind rename cannot redirect a source read.
pub(crate) fn open_bound_scope_file_nofollow(
    root: &File,
    file_name: &str,
    display_path: &Path,
) -> Result<File> {
    use cap_fs::MetadataExt;
    use cap_primitives::fs as cap_fs;

    let name = Path::new(file_name);
    if name.components().count() != 1
        || !matches!(name.components().next(), Some(Component::Normal(_)))
    {
        return Err(scope_file_changed_path(display_path));
    }
    let before = cap_fs::stat(root, name, cap_fs::FollowSymlinks::No)
        .map_err(|error| KioError::io(error.to_string(), display_path.display().to_string()))?;
    if !before.is_file() {
        return Err(scope_file_changed_path(display_path));
    }
    let mut options = cap_fs::OpenOptions::new();
    options.read(true);
    options._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let file = cap_fs::open(root, name, &options)
        .map_err(|error| KioError::io(error.to_string(), display_path.display().to_string()))?;
    let opened = cap_fs::Metadata::from_file(&file)
        .map_err(|error| KioError::io(error.to_string(), display_path.display().to_string()))?;
    let after = cap_fs::stat(root, name, cap_fs::FollowSymlinks::No)
        .map_err(|error| KioError::io(error.to_string(), display_path.display().to_string()))?;
    if !opened.is_file()
        || !after.is_file()
        || opened.dev() != before.dev()
        || opened.ino() != before.ino()
        || opened.dev() != after.dev()
        || opened.ino() != after.ino()
    {
        return Err(scope_file_changed_path(display_path));
    }
    Ok(file)
}

pub(crate) fn configure_scope_no_follow(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    #[cfg(target_os = "linux")]
    options.custom_flags(0x20_800);
    #[cfg(target_os = "macos")]
    options.custom_flags(0x104);
    let _ = options;
}

/// Scheduled source authority preserves the existing dev/inode observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScheduledSourceIdentity(u64, u64);

/// Retained metadata authority; separate from scheduled source admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundMetadataObservation {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) len: u64,
    pub(crate) nlink: u64,
    pub(crate) digest: String,
}

pub(crate) fn scheduled_source_identity(
    file: &File,
) -> Result<crate::platform::ScheduledSourceIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file
        .metadata()
        .map_err(|error| KioError::io(error.to_string(), "scheduled source"))?;
    Ok(ScheduledSourceIdentity(metadata.dev(), metadata.ino()))
}

fn same_scope_file_identity(opened: &fs::Metadata, path: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    opened.dev() == path.dev() && opened.ino() == path.ino()
}

pub(crate) fn configure_private_stage(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

pub(crate) fn set_scope_directory_permissions(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).kio_io(dir)?;
    Ok(())
}

pub(crate) fn validate_scope_path_private(metadata: &fs::Metadata, kio_dir: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(unsafe_store_error(
            kio_dir,
            ".kio must not be accessible to group or other principals",
        ));
    }
    if metadata.uid() != effective_uid() {
        return Err(unsafe_store_error(
            kio_dir,
            ".kio must be owned by the current effective user",
        ));
    }
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
    let same_identity = same_scope_file_identity(&opened, &after);
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
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(unsafe_store_error(
                label,
                ".kio must not be accessible to group or other principals",
            ));
        }
        if metadata.uid() != effective_uid() {
            return Err(unsafe_store_error(
                label,
                ".kio must be owned by the current effective user",
            ));
        }
    }
    Ok(())
}
