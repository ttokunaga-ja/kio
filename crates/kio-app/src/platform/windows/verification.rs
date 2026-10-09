use kio_core::{KioError, Result};
use std::path::Path;
pub(crate) fn verified_inventory_directory_native(path: &Path) -> Result<bool> {
    kio_core::platform::directory_is_real(path)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))
}
