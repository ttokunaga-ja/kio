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
    unsafe extern "C" {
        fn renameatx_np(
            a: libc::c_int,
            b: *const libc::c_char,
            c: libc::c_int,
            d: *const libc::c_char,
            flags: libc::c_uint,
        ) -> libc::c_int;
    }
    if unsafe {
        renameatx_np(
            left_dir.as_raw_fd(),
            left.as_ptr(),
            right_dir.as_raw_fd(),
            right.as_ptr(),
            0x0000_0002,
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
