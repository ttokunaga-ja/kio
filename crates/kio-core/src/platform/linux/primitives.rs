use std::{ffi::CStr, fs::File, io, os::fd::AsRawFd};

/// Atomically exchange two descriptor-relative names.
///
/// Available on Linux and macOS. Name encoding and domain validation belong
/// to the caller; this operation performs only the native exchange syscall.
pub fn atomic_exchange(
    left_dir: &File,
    left: &CStr,
    right_dir: &File,
    right: &CStr,
) -> io::Result<()> {
    // SAFETY: both files remain open and both C strings remain valid for the
    // call; the kernel retains neither the pointers nor the descriptors.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            left_dir.as_raw_fd(),
            left.as_ptr(),
            right_dir.as_raw_fd(),
            right.as_ptr(),
            2_u32,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
