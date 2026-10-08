use crate::error::{KioError, Result};
use crate::platform::unix::lock::bound_lock_corrupt;
use std::fs::File;

pub(crate) fn exchange_bound_lock(
    left_dir: &File,
    left: &str,
    right_dir: &File,
    right: &str,
) -> Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    let left = CString::new(left).map_err(|_| bound_lock_corrupt("invalid GC lock name"))?;
    let right = CString::new(right).map_err(|_| bound_lock_corrupt("invalid GC lock name"))?;
    if unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            left_dir.as_raw_fd(),
            left.as_ptr(),
            right_dir.as_raw_fd(),
            right.as_ptr(),
            2_u32,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(KioError::io(
            std::io::Error::last_os_error().to_string(),
            ".lock",
        ))
    }
}
