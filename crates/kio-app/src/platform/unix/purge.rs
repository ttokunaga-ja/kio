use crate::purge::store_corrupt;
use kio_core::Result;
use std::fs::{self, File};
use std::path::Path;

use std::os::unix::fs::MetadataExt;

pub(crate) struct PurgeRegularObservation {
    dev: u64,
    ino: u64,
}
pub(crate) struct PurgeRemovalObservation {
    dev: u64,
    ino: u64,
}

pub(crate) fn validate_purge_directory_native(_path: &Path) -> Result<()> {
    Ok(())
}

pub(crate) fn validate_purge_regular_native(path: &Path, listed: &fs::Metadata) -> Result<()> {
    if listed.nlink() != 1 {
        return Err(store_corrupt(
            path,
            "regular file has an unexpected hard-link count",
        ));
    }
    Ok(())
}

pub(crate) fn observe_purge_regular(
    _path: &Path,
    listed: &fs::Metadata,
) -> Result<PurgeRegularObservation> {
    Ok(PurgeRegularObservation {
        dev: listed.dev(),
        ino: listed.ino(),
    })
}

pub(crate) fn purge_regular_matches_opened(
    observed: &PurgeRegularObservation,
    opened: &fs::Metadata,
    _file: &File,
) -> bool {
    observed.dev == opened.dev() && observed.ino == opened.ino()
}

pub(crate) fn observe_purge_removal(
    path: &Path,
    listed: &fs::Metadata,
    is_directory: bool,
) -> Result<PurgeRemovalObservation> {
    if !is_directory && listed.nlink() != 1 {
        return Err(store_corrupt(
            path,
            "artifact has an unexpected hard-link count",
        ));
    }
    Ok(PurgeRemovalObservation {
        dev: listed.dev(),
        ino: listed.ino(),
    })
}

pub(crate) fn purge_removal_matches(
    observed: &PurgeRemovalObservation,
    _path: &Path,
    current: &fs::Metadata,
    _is_directory: bool,
) -> Result<bool> {
    Ok(observed.dev == current.dev() && observed.ino == current.ino())
}

pub(crate) fn purge_regular_matches_named(
    observed: &PurgeRegularObservation,
    _path: &Path,
    current: &fs::Metadata,
) -> Result<bool> {
    Ok(observed.dev == current.dev() && observed.ino == current.ino())
}
