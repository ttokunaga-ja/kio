use crate::error::Result;
use crate::gc::{corrupt, ioerr};

pub(crate) fn remove_reserved_leaf(dir: &std::fs::File, leaf: &str) -> Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;

    // Darwin private/full-level flags from <sys/fcntl.h>.  They are accepted
    // by the public unlinkat(2) entry point. AT_UNIQUE performs the link-count
    // check in the same kernel operation as removal.
    const AT_SYMLINK_NOFOLLOW_ANY: libc::c_int = 0x0800;
    const AT_RESOLVE_BENEATH: libc::c_int = 0x2000;
    const AT_UNIQUE: libc::c_int = 0x8000;

    let leaf = CString::new(leaf).map_err(|_| corrupt("invalid GC retirement leaf"))?;
    // SAFETY: `leaf` is NUL-terminated, `dir` remains open for the call, and
    // unlinkat does not retain either pointer or descriptor.
    let result = unsafe {
        libc::unlinkat(
            dir.as_raw_fd(),
            leaf.as_ptr(),
            AT_SYMLINK_NOFOLLOW_ANY | AT_RESOLVE_BENEATH | AT_UNIQUE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(ioerr(std::io::Error::last_os_error(), "GC retirement leaf"))
    }
}

/// Descriptor-relative atomic move which refuses to clobber the destination.
/// This is the critical quarantine primitive: an attacker cannot pre-create a
/// victim name and have it replaced by a tree object.
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
    unsafe extern "C" {
        fn renameatx_np(
            fromfd: libc::c_int,
            from: *const libc::c_char,
            tofd: libc::c_int,
            to: *const libc::c_char,
            flags: libc::c_uint,
        ) -> libc::c_int;
    }
    // Darwin's RENAME_EXCL is documented as 0x00000004.
    let result = unsafe {
        renameatx_np(
            from_dir.as_raw_fd(),
            from.as_ptr(),
            to_dir.as_raw_fd(),
            to.as_ptr(),
            0x0000_0004,
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
    let left = CString::new(left).map_err(|_| corrupt("invalid marker archive name"))?;
    let right = CString::new(right).map_err(|_| corrupt("invalid marker archive name"))?;
    crate::platform::atomic_exchange(left_dir, &left, right_dir, &right)
        .map_err(|error| ioerr(error, left.to_string_lossy().as_ref()))
}
