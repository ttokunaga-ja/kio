use super::gc_observation::{file_state, same_file_state};
use crate::cas::hash_bytes;
use crate::error::Result;
use crate::gc::{
    FileObservation, MAX_METADATA, SNAPSHOT_AUTO_STATE_LEAF, SnapshotAutoState, id_meta, ioerr,
    parse_snapshot_auto_state, snapshot_auto_state_changed, valid_file,
};
use crate::store_dir::StoreDirectory;
use cap_primitives::fs as cap_fs;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub(crate) fn directory(file: &File) -> Result<StoreDirectory> {
    StoreDirectory::from_retained(
        file.try_clone()
            .map_err(|e| ioerr(e, "snapshot state directory"))?,
        PathBuf::from("<retained snapshot state directory>"),
    )
}

pub(crate) fn open_snapshot_source(parent: &File, expected: &FileObservation) -> Result<File> {
    match crate::platform::open_gc_mutation(
        &directory(parent)?,
        Path::new(SNAPSHOT_AUTO_STATE_LEAF),
        MAX_METADATA,
    ) {
        Ok(source) => Ok(source),
        Err(acquisition_error) => {
            // A replacement's live writer can deny the exclusive mutation pin
            // before observe() compares identity. Metadata under the retained
            // parent can prove stale CAS authority, but cannot authorize mutation.
            // Missing, unsafe, unreadable, or unchanged leaves retain the original
            // acquisition error; a failed re-observation is not proof of a race.
            let changed = (|| -> Result<bool> {
                let metadata = cap_fs::stat(
                    parent,
                    Path::new(SNAPSHOT_AUTO_STATE_LEAF),
                    cap_fs::FollowSymlinks::No,
                )
                .map_err(|error| ioerr(error, "snapshot state"))?;
                valid_file(&metadata, MAX_METADATA)?;
                Ok(id_meta(&metadata)? != expected.identity
                    || (metadata.modified().is_ok() && file_state(&metadata) != expected.state))
            })();
            if matches!(changed, Ok(true)) {
                Err(snapshot_auto_state_changed())
            } else {
                Err(acquisition_error)
            }
        }
    }
}

pub(crate) fn observe(file: &File) -> Result<(SnapshotAutoState, FileObservation)> {
    let before = cap_fs::Metadata::from_file(file).map_err(|e| ioerr(e, "snapshot state"))?;
    valid_file(&before, MAX_METADATA)?;
    let mut cursor = file;
    cursor
        .seek(SeekFrom::Start(0))
        .map_err(|e| ioerr(e, "snapshot state"))?;
    let mut bytes = Vec::new();
    cursor
        .take(MAX_METADATA + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| ioerr(e, "snapshot state"))?;
    let after = cap_fs::Metadata::from_file(file).map_err(|e| ioerr(e, "snapshot state"))?;
    valid_file(&after, MAX_METADATA)?;
    if !same_file_state(&before, &after)? || bytes.len() as u64 != before.len() {
        return Err(snapshot_auto_state_changed());
    }
    Ok((
        parse_snapshot_auto_state(&bytes)?,
        FileObservation {
            identity: id_meta(&after)?,
            state: file_state(&after),
            digest: hash_bytes(&bytes),
        },
    ))
}
