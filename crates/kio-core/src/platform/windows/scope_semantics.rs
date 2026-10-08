use crate::error::{IoResultExt, KioError, Result};
use crate::platform::{PathLock, PathLockLease};
use std::fs::{File, Metadata};
use std::path::Path;
use std::rc::Rc;

pub(crate) fn normalize_repository_handles(
    scope: File,
    kio: File,
    canonical_root: &Path,
) -> Result<(File, File)> {
    let scope =
        crate::store_dir::StoreDirectory::from_retained(scope, canonical_root.to_path_buf())?;
    let kio = crate::store_dir::StoreDirectory::from_retained(kio, canonical_root.join(".kio"))?;
    Ok((
        scope
            .root_handle()
            .try_clone()
            .map_err(|error| KioError::io(error.to_string(), "."))?,
        kio.root_handle()
            .try_clone()
            .map_err(|error| KioError::io(error.to_string(), ".kio"))?,
    ))
}
pub(crate) fn validate_tag_directory_native(path: &Path) -> Result<()> {
    if !super::cas_identity::directory_is_real(path)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?
    {
        return Err(crate::scope::unsafe_store_error(
            path,
            "tag refs path must not be a Windows reparse point",
        ));
    }
    Ok(())
}
pub(crate) fn validate_commit_ref_links(_listed: &Metadata, _path: &Path) -> Result<()> {
    Ok(())
}
pub(crate) fn validate_orphan_file_native(path: &Path, _listed: &Metadata) -> Result<()> {
    if !super::cas_identity::regular_file_is_safe(path).kio_io(path)? {
        return Err(crate::scope::orphan_raw_ingest_error(
            path,
            "raw-ingest orphan is a reparse point or hard link",
        ));
    }
    Ok(())
}
pub(crate) fn validate_ingest_directory_native(path: &Path) -> Result<()> {
    if !super::cas_identity::directory_is_real(path).kio_io(path)? {
        return Err(crate::scope::orphan_raw_ingest_error(
            path,
            "raw-ingest namespace ancestor is a reparse point",
        ));
    }
    Ok(())
}
pub(crate) fn prepare_path_lock_lease(native: PathLock) -> (PathLockLease, Option<Rc<PathLock>>) {
    let owner = Rc::new(native);
    (Rc::clone(&owner), Some(owner))
}
pub(crate) fn reentrant_path_lock_lease(
    owner: Option<Rc<PathLock>>,
) -> Result<Option<PathLockLease>> {
    owner
        .map(Some)
        .ok_or_else(|| KioError::locked("Windows lock lease is unavailable"))
}
