//! Native retained scope source admission and no-follow configuration.

use std::fs::{File, OpenOptions};
use std::path::{Component, Path};

use crate::error::{IoResultExt, Result};
use crate::scope::scope_file_changed_path;
use crate::store_dir::StoreDirectory;

pub(crate) fn open_bound_scope_file_nofollow(
    root: &File,
    file_name: &str,
    display_path: &Path,
) -> Result<File> {
    let name = Path::new(file_name);
    if name.components().count() != 1
        || !matches!(name.components().next(), Some(Component::Normal(_)))
    {
        return Err(scope_file_changed_path(display_path));
    }
    let directory = StoreDirectory::from_retained(
        root.try_clone().kio_io(display_path)?,
        display_path
            .parent()
            .unwrap_or(Path::new("."))
            .to_path_buf(),
    )?;
    // Source enumeration and each read/staging caller enforce archive limits
    // from this descriptor's metadata before reading, then bound the stream.
    // Let those checks report the scope budget error for oversized inputs while
    // retaining StoreDirectory's no-follow and single-link validation here.
    directory.open_regular_read(name, u64::MAX)
}

pub(crate) fn configure_scope_no_follow(options: &mut OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;
    options.custom_flags(0x0020_0000);
}
