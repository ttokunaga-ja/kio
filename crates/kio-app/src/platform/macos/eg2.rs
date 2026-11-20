use crate::eg2::{MEMORY_BYTES, WorkerError, runtime::VerifiedLibrary};
use std::path::PathBuf;

#[cfg(not(test))]
pub(crate) const EG2_SLOT_ROOT: &str = "/private/tmp";

pub(crate) fn cap_eg2_worker_address_space() -> Result<(), WorkerError> {
    // Darwin rejects RLIMIT_AS below the current VM map size. Measure after
    // exec so the ceiling covers the worker's own initial mappings (including
    // the dyld shared region) plus its budget for loading and inference.
    let mut info = std::mem::MaybeUninit::<libc::mach_task_basic_info>::uninit();
    let mut count = libc::MACH_TASK_BASIC_INFO_COUNT;
    // SAFETY: the flavor and count match the local output struct.
    let status = unsafe {
        libc::task_info(
            libc::mach_task_self(),
            libc::MACH_TASK_BASIC_INFO,
            info.as_mut_ptr().cast(),
            &mut count,
        )
    };
    if status != libc::KERN_SUCCESS || count != libc::MACH_TASK_BASIC_INFO_COUNT {
        return Err(WorkerError::Startup);
    }
    let bytes = unsafe { info.assume_init() }
        .virtual_size
        .checked_add(MEMORY_BYTES)
        .ok_or(WorkerError::Startup)?;
    crate::platform::eg2_worker::cap_address_space(bytes).map_err(|_| WorkerError::Startup)
}

pub(crate) fn eg2_library_load_path(verified: &VerifiedLibrary) -> PathBuf {
    verified.path.clone()
}
