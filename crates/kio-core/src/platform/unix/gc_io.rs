use crate::cas::MAX_TREE_OBJECT_BYTES;
use crate::error::Result;
use crate::gc::{
    FileObservation, MAX_METADATA, corrupt, id_meta, ioerr, valid_file, validate_directory_handle,
};
use cap_primitives::fs as cap_fs;
use std::path::Path;

pub(crate) fn open_gc_tree_writable(
    directory: &std::fs::File,
    leaf: &str,
    expected: &FileObservation,
) -> Result<std::fs::File> {
    let mut options = cap_fs::OpenOptions::new();
    options.read(true).write(true);
    options._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let file =
        cap_fs::open(directory, Path::new(leaf), &options).map_err(|error| ioerr(error, leaf))?;
    let metadata = cap_fs::Metadata::from_file(&file).map_err(|error| ioerr(error, leaf))?;
    valid_file(&metadata, MAX_TREE_OBJECT_BYTES)?;
    if id_meta(&metadata)? != expected.identity || metadata.len() != expected.state.len {
        return Err(corrupt("tree changed before writable handle bind"));
    }
    Ok(file)
}

pub(crate) fn open_verified_file_handle(
    directory: &std::fs::File,
    leaf: &str,
    expected: &FileObservation,
    max: u64,
    label: &str,
) -> Result<std::fs::File> {
    let mut options = cap_fs::OpenOptions::new();
    options.read(true);
    options._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let file =
        cap_fs::open(directory, Path::new(leaf), &options).map_err(|error| ioerr(error, leaf))?;
    let metadata = cap_fs::Metadata::from_file(&file).map_err(|error| ioerr(error, leaf))?;
    valid_file(&metadata, max)?;
    if id_meta(&metadata)? != expected.identity || metadata.len() != expected.state.len {
        return Err(corrupt(&format!("{label} changed while binding handle")));
    }
    Ok(file)
}

pub(crate) fn open_verified_receipt_durable_handle(
    directory: &std::fs::File,
    leaf: &str,
    expected: &FileObservation,
) -> Result<std::fs::File> {
    open_verified_file_handle(
        directory,
        leaf,
        expected,
        MAX_METADATA,
        "existing GC shallow receipt",
    )
}

/// Capability directory handles are intentionally opened as `O_PATH` on
/// Linux, which is valid for descriptor-relative traversal but cannot be
/// passed to `fsync`. Reopen exactly `.` below the retained capability with
/// no-follow read access, verify that it is the same directory, then flush
/// that syncable descriptor. No ambient pathname is reconstructed.
pub(crate) fn sync_bound_directory(
    directory: &std::fs::File,
    label: impl AsRef<Path>,
) -> Result<()> {
    let label = label.as_ref();
    let expected = cap_fs::Metadata::from_file(directory).map_err(|error| ioerr(error, label))?;
    validate_directory_handle(directory, label)?;
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let syncable =
        cap_fs::open(directory, Path::new("."), &options).map_err(|error| ioerr(error, label))?;
    let observed = cap_fs::Metadata::from_file(&syncable).map_err(|error| ioerr(error, label))?;
    if !observed.is_dir() || id_meta(&observed)? != id_meta(&expected)? {
        return Err(corrupt(
            "retained directory changed while reopening for fsync",
        ));
    }
    syncable.sync_all().map_err(|error| ioerr(error, label))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn publish_staged(
    from_dir: &std::fs::File,
    staged: &str,
    to_dir: &std::fs::File,
    leaf: &str,
    bytes: &[u8],
    max: u64,
    staged_checkpoint: impl FnOnce() -> Result<()>,
    _control: &crate::control::ControlHandle,
) -> Result<()> {
    crate::gc::create_new_bound(from_dir, staged, bytes, max)?;
    staged_checkpoint()?;
    crate::platform::gc_rename_noreplace_between(from_dir, staged, to_dir, leaf)
}

pub(crate) fn verify_retained_receipt_observation(
    _file: &std::fs::File,
    _leaf: &str,
    _expected: &FileObservation,
) -> Result<()> {
    Ok(())
}
