use crate::export::unsafe_export_error;
use cap_primitives::fs as cap_fs;
use kio_core::{KioError, Result};
use std::fs::{self, File};
use std::path::Path;

use std::os::unix::fs::MetadataExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DestinationIdentity {
    dev: u64,
    ino: u64,
}

pub(crate) fn destination_lstat_identity(path: &Path) -> Result<DestinationIdentity> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
    Ok(DestinationIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    })
}

pub(crate) fn destination_identity_matches_opened(
    expected: &DestinationIdentity,
    opened: &File,
) -> bool {
    opened
        .metadata()
        .is_ok_and(|opened| expected.dev == opened.dev() && expected.ino == opened.ino())
}

pub(crate) fn validate_export_directory_native(_path: &Path) -> Result<()> {
    Ok(())
}

pub(crate) fn validate_export_regular_native(path: &Path, listed: &fs::Metadata) -> Result<()> {
    if listed.nlink() != 1 {
        return Err(unsafe_export_error(
            path,
            "export refuses a hard-linked destination file",
        ));
    }
    Ok(())
}

pub(crate) fn validate_export_regular_metadata_native(
    path: &Path,
    listed: &cap_fs::Metadata,
) -> Result<()> {
    use cap_fs::MetadataExt;
    if listed.nlink() != 1 {
        return Err(unsafe_export_error(
            path,
            "export refuses a hard-linked destination file",
        ));
    }
    Ok(())
}

pub(crate) fn validate_export_directory_metadata_native(
    _path: &Path,
    _listed: &cap_fs::Metadata,
) -> Result<()> {
    Ok(())
}

pub(crate) fn configure_export_temp(options: &mut cap_fs::OpenOptions) {
    use cap_fs::OpenOptionsExt;
    options.mode(0o600);
}

pub(crate) fn sync_export_directory(handle: &File, path: &Path) -> Result<()> {
    // Linux capability directory handles use O_PATH: reopen the held directory for fsync.
    let mut options = cap_fs::OpenOptions::new();
    options.read(true);
    let syncable = cap_fs::open(handle, Path::new("."), &options)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
    syncable
        .sync_all()
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))
}
