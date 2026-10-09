//! Unix publication and cleanup of marker-bound source index copies.
use crate::fts::{
    PreparedGcIndexCleanup, gc_leaf_identity, gc_leaf_state, open_existing_gc_internal_index_dir,
    require_gc_private_dir_identity, source_file_state_digest, validate_gc_temp_leaf,
};
use crate::platform::source::{
    canonical_gc_index_identity, sync_bound_gc_directory, validate_bound_source_file,
};
use crate::{IndexError, Result};
use cap_primitives::fs as cap_fs;
use std::path::Path;

pub(crate) fn exchange_gc_index_leaves(
    left_dir: &std::fs::File,
    left: &str,
    right_dir: &std::fs::File,
    right: &str,
) -> Result<()> {
    use std::ffi::CString;
    let left =
        CString::new(left).map_err(|_| IndexError::Schema("invalid GC index name".into()))?;
    let right =
        CString::new(right).map_err(|_| IndexError::Schema("invalid GC index name".into()))?;
    kio_core::platform::atomic_exchange(left_dir, &left, right_dir, &right)
        .map_err(|error| IndexError::Schema(format!("atomic GC index exchange: {error}")))
}

pub(crate) fn remove_prepared_bound_gc_index(
    kio_dir: &std::fs::File,
    temp_leaf: &str,
    expected_private_dir_identity: &str,
    expected_identity: &str,
) -> Result<PreparedGcIndexCleanup> {
    validate_gc_temp_leaf(temp_leaf)?;
    let private = open_existing_gc_internal_index_dir(kio_dir)?;
    require_gc_private_dir_identity(&private, expected_private_dir_identity)?;
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let file = match cap_fs::open(&private, Path::new(temp_leaf), &options) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PreparedGcIndexCleanup::AlreadyAbsent);
        }
        Err(error) => {
            return Err(IndexError::Schema(format!(
                "open GC private index copy for cleanup: {error}"
            )));
        }
    };
    validate_bound_source_file(&file, Path::new(temp_leaf))?;
    let identity = canonical_gc_index_identity(&file)?;
    if gc_leaf_identity(&private, temp_leaf)? != identity {
        return Err(IndexError::Schema(
            "GC private index cleanup input changed".to_owned(),
        ));
    }
    if identity != expected_identity {
        return Err(IndexError::Schema(
            "GC private index cleanup input changed".to_owned(),
        ));
    }
    match cap_fs::remove_file(&private, Path::new(temp_leaf)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PreparedGcIndexCleanup::AlreadyAbsent);
        }
        Err(error) => {
            return Err(IndexError::Schema(format!(
                "remove GC private index copy: {error}"
            )));
        }
    }
    sync_bound_gc_directory(&private, "fsync private GC index cleanup")?;
    Ok(PreparedGcIndexCleanup::Removed)
}

pub(crate) fn exchange_prepared_bound_gc_index(
    kio_dir: &std::fs::File,
    temp_leaf: &str,
    expected_private_dir_identity: &str,
    expected_source_identity: &str,
    expected_source_state_digest: &str,
    expected_target_identity: &str,
) -> Result<()> {
    validate_gc_temp_leaf(temp_leaf)?;
    let index = cap_fs::open_dir_nofollow(kio_dir, Path::new("index")).map_err(|error| {
        IndexError::Schema(format!("open GC index directory for exchange: {error}"))
    })?;
    let private = open_existing_gc_internal_index_dir(kio_dir)?;
    require_gc_private_dir_identity(&private, expected_private_dir_identity)?;
    let source = gc_leaf_identity(&index, "sqlite.db")?;
    let source_state = gc_leaf_state(&index, "sqlite.db")?;
    let temp = gc_leaf_identity(&private, temp_leaf)?;
    if source != expected_source_identity
        || source_file_state_digest(source_state) != expected_source_state_digest
        || temp != expected_target_identity
    {
        return Err(IndexError::Schema(
            "GC durable index exchange inputs changed".to_owned(),
        ));
    }
    exchange_gc_index_leaves(&index, "sqlite.db", &private, temp_leaf)?;
    // Exchange spans two directories.  Persist the destination namespace
    // first: after a power loss recovery may see both names, but never rely on
    // a source-directory fsync having made the target name durable first.
    sync_bound_gc_directory(&index, "fsync GC index exchange")?;
    sync_bound_gc_directory(&private, "fsync private GC index exchange")?;
    if gc_leaf_identity(&index, "sqlite.db")? != expected_target_identity
        || gc_leaf_identity(&private, temp_leaf)? != expected_source_identity
    {
        return Err(IndexError::Schema(
            "GC durable index exchange changed unexpectedly".to_owned(),
        ));
    }
    Ok(())
}
