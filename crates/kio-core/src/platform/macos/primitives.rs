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
    unsafe extern "C" {
        fn renameatx_np(
            fromfd: libc::c_int,
            from: *const libc::c_char,
            tofd: libc::c_int,
            to: *const libc::c_char,
            flags: libc::c_uint,
        ) -> libc::c_int;
    }
    let result = unsafe {
        renameatx_np(
            left_dir.as_raw_fd(),
            left.as_ptr(),
            right_dir.as_raw_fd(),
            right.as_ptr(),
            0x0000_0002,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
