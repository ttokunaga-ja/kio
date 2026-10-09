use std::fs::File;
use std::path::Path;

use kio_core::Result;
use kio_core::private_fs::read_private_file_at;
use kio_core::store_dir::{Publication, StoreDirectory};

const LOCK: &str = "instance.lock";

pub(crate) fn open_watch_instance_lock(
    directory: &StoreDirectory,
    create: bool,
) -> Result<Option<File>> {
    if directory.read_optional(Path::new(LOCK), 0)?.is_none() {
        if !create {
            return Ok(None);
        }
        if let Err(error) = directory.write_atomic(Path::new(LOCK), &[], Publication::CreateOnly)
            && directory.read_optional(Path::new(LOCK), 0)?.is_none()
        {
            return Err(error);
        }
    }
    // Revalidates the retained private parent/DACL. State from a
    // different local user cannot serve as a stop request or queue authority.
    read_private_file_at(directory, LOCK, 0)?;
    directory.open_regular_read(Path::new(LOCK), 0).map(Some)
}
