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
    let left = CString::new(left).map_err(|_| bound_lock_corrupt("invalid GC lock name"))?;
    let right = CString::new(right).map_err(|_| bound_lock_corrupt("invalid GC lock name"))?;
    crate::platform::atomic_exchange(left_dir, &left, right_dir, &right)
        .map_err(|error| KioError::io(error.to_string(), ".lock"))
}
