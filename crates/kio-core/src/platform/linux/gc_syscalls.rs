use crate::error::Result;
use crate::gc::{corrupt, ioerr};
use cap_primitives::fs as cap_fs;
use std::path::Path;

pub(crate) fn remove_reserved_leaf(dir: &std::fs::File, leaf: &str) -> Result<()> {
    cap_fs::remove_file(dir, Path::new(leaf)).map_err(|error| ioerr(error, leaf))
}

pub(crate) fn rename_noreplace_bound(dir: &std::fs::File, from: &str, to: &str) -> Result<()> {
    rename_noreplace_between(dir, from, dir, to)
}

pub(crate) fn rename_noreplace_between(
    from_dir: &std::fs::File,
    from: &str,
    to_dir: &std::fs::File,
    to: &str,
) -> Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    let from = CString::new(from).map_err(|_| corrupt("invalid quarantine name"))?;
    let to = CString::new(to).map_err(|_| corrupt("invalid quarantine name"))?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            from_dir.as_raw_fd(),
            from.as_ptr(),
            to_dir.as_raw_fd(),
            to.as_ptr(),
            1_u32,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(ioerr(
            std::io::Error::last_os_error(),
            from.to_string_lossy().as_ref(),
        ))
    }
}

#[allow(dead_code)]
pub(crate) fn exchange_bound(dir: &std::fs::File, left: &str, right: &str) -> Result<()> {
    exchange_between(dir, left, dir, right)
}

pub(crate) fn exchange_between(
    left_dir: &std::fs::File,
    left: &str,
    right_dir: &std::fs::File,
    right: &str,
) -> Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    let left = CString::new(left).map_err(|_| corrupt("invalid marker archive name"))?;
    let right = CString::new(right).map_err(|_| corrupt("invalid marker archive name"))?;
    // Linux renameat2 RENAME_EXCHANGE is 0x2.
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
        Err(ioerr(
            std::io::Error::last_os_error(),
            left.to_string_lossy().as_ref(),
        ))
    }
}
