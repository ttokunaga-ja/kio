use crate::eg2::{WorkerError, runtime::VerifiedLibrary};
use std::path::PathBuf;

#[cfg(not(test))]
pub(crate) const EG2_SLOT_ROOT: &str = "/tmp";

pub(crate) fn configure_eg2_worker_privacy() -> Result<(), WorkerError> {
    // SAFETY: this call only changes the current worker's dumpability.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(WorkerError::Startup);
    }
    Ok(())
}

pub(crate) fn eg2_library_load_path(verified: &VerifiedLibrary) -> PathBuf {
    // Load the retained descriptor even if the distribution entry is replaced.
    use std::os::fd::AsRawFd;
    PathBuf::from(format!("/proc/self/fd/{}", verified.file.as_raw_fd()))
}
