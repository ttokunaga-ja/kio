use crate::eg2::{WorkerError, runtime::VerifiedLibrary};
use std::path::PathBuf;

#[cfg(not(test))]
pub(crate) const EG2_SLOT_ROOT: &str = "/private/tmp";

pub(crate) fn configure_eg2_worker_privacy() -> Result<(), WorkerError> {
    Ok(())
}

pub(crate) fn eg2_library_load_path(verified: &VerifiedLibrary) -> PathBuf {
    verified.path.clone()
}
