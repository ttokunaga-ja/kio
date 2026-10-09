use crate::purge::store_corrupt;
use kio_core::{KioError, Result};
use std::fs::{self, File, OpenOptions};
use std::path::Path;

use kio_core::platform::{DirectoryHandleIdentity, RegularFileHandleIdentity};

pub(crate) struct PurgeRegularObservation {
    listed: fs::Metadata,
    identity: RegularFileHandleIdentity,
}
pub(crate) struct PurgeRemovalObservation {
    identity: RemovalIdentity,
}
enum RemovalIdentity {
    Directory(DirectoryHandleIdentity),
    Regular(PurgeRegularObservation),
}
pub(crate) struct PurgeParentObservation {
    identity: DirectoryHandleIdentity,
}

pub(crate) fn validate_purge_directory_native(path: &Path) -> Result<()> {
    if !kio_core::platform::directory_is_real(path)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?
    {
        return Err(store_corrupt(path, "expected a real non-reparse directory"));
    }
    Ok(())
}

fn windows_regular_file_identity(path: &Path) -> Result<RegularFileHandleIdentity> {
    kio_core::platform::real_regular_file_identity(path)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?
        .ok_or_else(|| store_corrupt(path, "expected a real single-link regular file"))
}

pub(crate) fn validate_purge_regular_native(path: &Path, _listed: &fs::Metadata) -> Result<()> {
    let _ = windows_regular_file_identity(path)?;
    Ok(())
}

pub(crate) fn observe_purge_regular(
    path: &Path,
    listed: &fs::Metadata,
) -> Result<PurgeRegularObservation> {
    let identity = windows_regular_file_identity(path)?;
    Ok(PurgeRegularObservation {
        listed: listed.clone(),
        identity,
    })
}

fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
        && left.file_type() == right.file_type()
}

pub(crate) fn purge_regular_matches_opened(
    observed: &PurgeRegularObservation,
    opened: &fs::Metadata,
    file: &File,
) -> bool {
    same_file_identity(&observed.listed, opened)
        && kio_core::platform::regular_file_handle_identity(file) == Some(observed.identity)
}

pub(crate) fn observe_purge_removal(
    path: &Path,
    listed: &fs::Metadata,
    is_directory: bool,
) -> Result<PurgeRemovalObservation> {
    if is_directory {
        let identity = kio_core::platform::real_directory_identity(path)
            .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?
            .ok_or_else(|| store_corrupt(path, "artifact tree contains a reparse directory"))?;
        Ok(PurgeRemovalObservation {
            identity: RemovalIdentity::Directory(identity),
        })
    } else {
        Ok(PurgeRemovalObservation {
            identity: RemovalIdentity::Regular(observe_purge_regular(path, listed)?),
        })
    }
}

pub(crate) fn purge_removal_matches(
    observed: &PurgeRemovalObservation,
    path: &Path,
    current: &fs::Metadata,
    _is_directory: bool,
) -> Result<bool> {
    match &observed.identity {
        RemovalIdentity::Directory(identity) => {
            Ok(kio_core::platform::real_directory_identity(path)
                .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?
                == Some(*identity))
        }
        RemovalIdentity::Regular(regular) => purge_regular_matches_named(regular, path, current),
    }
}

pub(crate) fn observe_purge_parent(
    path: &Path,
    _listed: &fs::Metadata,
) -> Result<PurgeParentObservation> {
    let identity = kio_core::platform::real_directory_identity(path)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?
        .ok_or_else(|| store_corrupt(path, "purge state parent is a reparse directory"))?;
    Ok(PurgeParentObservation { identity })
}

pub(crate) fn purge_parent_matches(observed: &PurgeParentObservation, path: &Path) -> Result<bool> {
    Ok(kio_core::platform::real_directory_identity(path)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?
        == Some(observed.identity))
}

pub(crate) fn purge_regular_matches_named(
    observed: &PurgeRegularObservation,
    path: &Path,
    current: &fs::Metadata,
) -> Result<bool> {
    Ok(same_file_identity(&observed.listed, current)
        && windows_regular_file_identity(path)? == observed.identity)
}

pub(crate) fn configure_purge_private_temp(_options: &mut OpenOptions) {}
