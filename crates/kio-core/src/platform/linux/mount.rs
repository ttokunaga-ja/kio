use crate::Result;
use crate::store_dir::{err, ioerr};
use std::{fs::File, path::Path};

pub(crate) fn mount_id(file: &File, label: &Path) -> Result<u64> {
    use std::os::fd::AsRawFd;
    let mut info: libc::statx = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::statx(
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            libc::STATX_MNT_ID,
            &mut info,
        )
    } != 0
    {
        return Err(ioerr(label, std::io::Error::last_os_error()));
    }
    if info.stx_mask & libc::STATX_MNT_ID == 0 {
        return Err(err(
            label,
            "planned removal requires retained-handle mount IDs",
        ));
    }
    Ok(info.stx_mnt_id)
}
