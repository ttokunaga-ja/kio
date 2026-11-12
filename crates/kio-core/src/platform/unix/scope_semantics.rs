use crate::error::{KioError, Result};
use crate::platform::{PathLock, PathLockLease};
use std::fs::{File, Metadata};
use std::path::Path;
use std::rc::Rc;

pub(crate) fn normalize_repository_handles(
    scope: File,
    kio: File,
    _canonical_root: &Path,
    _control: crate::control::ControlHandle,
) -> Result<(File, File)> {
    Ok((scope, kio))
}
pub(crate) fn validate_tag_directory_native(_path: &Path) -> Result<()> {
    Ok(())
}
pub(crate) fn validate_commit_ref_links(listed: &Metadata, path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if listed.nlink() != 1 {
        return Err(crate::scope::tag_ref_corrupt(
            path,
            "commit ref has an unexpected hard-link count",
        ));
    }
    Ok(())
}
pub(crate) fn validate_orphan_file_native(path: &Path, listed: &Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if listed.nlink() != 1 {
        return Err(crate::scope::orphan_raw_ingest_error(
            path,
            "raw-ingest orphan has an unexpected hard-link count",
        ));
    }
    Ok(())
}
pub(crate) fn validate_ingest_directory_native(_path: &Path) -> Result<()> {
    Ok(())
}
pub(crate) fn prepare_path_lock_lease(native: PathLock) -> (PathLockLease, Option<Rc<PathLock>>) {
    (native, None)
}
pub(crate) fn reentrant_path_lock_lease(
    _owner: Option<Rc<PathLock>>,
) -> Result<Option<PathLockLease>> {
    Ok(None)
}
pub(crate) fn acquire_path_gate(parent: &File, lock_path: &Path) -> Result<File> {
    use std::os::fd::AsRawFd;
    let gate = super::lock::open_bound_directory_for_io(parent, lock_path)?;
    // SAFETY: owned gate outlives its flock; LOCK_NB keeps writer contention nonblocking.
    if unsafe { libc::flock(gate.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(libc::EWOULDBLOCK)) {
            return Err(KioError::locked(lock_path.display().to_string()));
        }
        return Err(KioError::io(
            error.to_string(),
            lock_path.display().to_string(),
        ));
    }
    Ok(gate)
}
pub(crate) fn unlock_path_gate(file: &File) {
    use std::os::fd::AsRawFd;
    // SAFETY: descriptor remains owned by the common gate until this call completes.
    let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
}
