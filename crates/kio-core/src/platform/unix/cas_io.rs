use std::fs::{self, File, OpenOptions};
use std::path::Path;

use crate::cas::ensure_bound_directory;
use crate::error::{IoResultExt, KioError, Result};

pub(crate) fn sync_bound_directory(directory: &File, label: &Path) -> Result<()> {
    use cap_primitives::fs::{self as cap_fs, MetadataExt};

    let expected = cap_fs::Metadata::from_file(directory).kio_io(label)?;
    ensure_bound_directory(directory, label)?;
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let syncable = cap_fs::open(directory, Path::new("."), &options).kio_io(label)?;
    let observed = cap_fs::Metadata::from_file(&syncable).kio_io(label)?;
    if !observed.is_dir() || observed.dev() != expected.dev() || observed.ino() != expected.ino() {
        return Err(KioError::io(
            "bound CAS directory changed while reopening for fsync",
            label.display().to_string(),
        ));
    }
    syncable.sync_all().kio_io(label)
}

pub(crate) fn same_open_file(left: &File, right: &File) -> Result<bool> {
    Ok(same_file_identity(
        &left
            .metadata()
            .map_err(|error| KioError::io(error.to_string(), "raw repair source"))?,
        &right
            .metadata()
            .map_err(|error| KioError::io(error.to_string(), "raw repair quarantine"))?,
    ))
}

pub(crate) fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination)
        .map_err(|error| KioError::io(error.to_string(), destination.display().to_string()))
}

pub(crate) fn is_single_link_regular_metadata(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.is_file() && metadata.nlink() == 1
}

pub(crate) fn configure_no_follow(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    #[cfg(target_os = "linux")]
    options.custom_flags(0x20_800);
    #[cfg(target_os = "macos")]
    options.custom_flags(0x104);
    let _ = options;
}

pub(crate) fn same_file_identity(opened: &fs::Metadata, path: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    opened.dev() == path.dev() && opened.ino() == path.ino()
}
