use kio_core::Result;
use std::path::Path;
pub(crate) fn verified_inventory_directory_native(_path: &Path) -> Result<bool> {
    Ok(true)
}
