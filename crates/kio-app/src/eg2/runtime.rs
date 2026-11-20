use super::*;
use std::path::{Path, PathBuf};

use kio_core::litert::{Bundle, sha256};

pub(super) use crate::platform::EG2_TARGET as TARGET;
use crate::platform::open_eg2_library;

#[derive(Debug)]
pub(crate) struct VerifiedLibrary {
    pub path: PathBuf,
    pub file: File, // Retained through native loading.
}

pub(super) fn verify(executable: &Path) -> Result<VerifiedLibrary, WorkerError> {
    let bundle = Bundle::for_target(TARGET).ok_or(WorkerError::RuntimeUnsupported)?;
    let root = executable
        .parent()
        .and_then(Path::parent)
        .filter(|path| path.is_absolute())
        .ok_or(WorkerError::RuntimeUnavailable)?;
    let path = root.join(bundle.directory()).join(bundle.library.name());
    let mut file = open_eg2_library(&path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => WorkerError::RuntimeMissing,
        _ => WorkerError::RuntimeUnavailable,
    })?;
    let metadata = file
        .metadata()
        .map_err(|_| WorkerError::RuntimeUnavailable)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(WorkerError::RuntimeUnavailable);
    }

    const MAX_LIBRARY_BYTES: u64 = 256 * 1024 * 1024;
    if metadata.len() > MAX_LIBRARY_BYTES
        || sha256(&mut (&mut file).take(MAX_LIBRARY_BYTES + 1))
            .map_err(|_| WorkerError::RuntimeUnavailable)?
            != bundle.library.sha256
    {
        return Err(WorkerError::RuntimeHashMismatch);
    }
    Ok(VerifiedLibrary { path, file })
}
