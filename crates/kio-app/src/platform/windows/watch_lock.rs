use std::fs::File;
use std::path::Path;

use kio_core::Result;
use kio_core::store_dir::{Publication, StoreDirectory};

const LOCK: &str = "instance.lock";

pub(crate) fn open_watch_instance_lock(
    directory: &StoreDirectory,
    create: bool,
) -> Result<Option<File>> {
    let leaf = Path::new(LOCK);
    // Even an empty marker must not be read through a second handle while
    // Windows holds its exclusive byte-range lock. Absence and fixed length
    // are metadata invariants; unsafe occupied names remain errors below.
    if !directory.contains_entry(leaf)? {
        if !create {
            return Ok(None);
        }
        if let Err(error) = directory.write_atomic(leaf, &[], Publication::CreateOnly)
            && !directory.contains_entry(leaf)?
        {
            return Err(error);
        }
    }
    let file = directory.open_regular_read(leaf, 0)?;
    kio_core::private_fs::verify_private_directory_handle(directory)?;
    kio_core::platform::verify_owner_private_handle(&file)?;
    Ok(Some(file))
}
