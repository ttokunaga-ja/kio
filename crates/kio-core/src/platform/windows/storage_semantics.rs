use std::{ffi::OsStr, fs::File, path::Path};

use crate::Result;
use crate::management::io;
use crate::store_dir::{StoreDirectory, err};

pub(crate) fn validate_store_component(component: &OsStr, label: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    if component.encode_wide().any(|unit| unit == b':' as u16) {
        return Err(err(
            label,
            "Windows store paths cannot contain ADS separators",
        ));
    }
    Ok(())
}

pub(crate) fn materializable_component(path: &str) -> bool {
    crate::portable::portable_leaf_error(path).is_none()
}

pub(crate) fn normalize_management_handles(
    root: File,
    kio: File,
    canonical_root: &Path,
    control: &crate::control::ControlHandle,
) -> Result<(File, File)> {
    // Keep the proved objects while consuming the original Windows
    // handles: retaining their delete-share-denying clones would
    // prevent later namespace moves even after `control` normalizes.
    let root = StoreDirectory::from_retained_with_control(
        root,
        canonical_root.to_path_buf(),
        control.clone(),
    )?;
    let kio = StoreDirectory::from_retained_with_control(
        kio,
        canonical_root.join(".kio"),
        control.clone(),
    )?;
    Ok((
        root.root_handle().try_clone().map_err(io)?,
        kio.root_handle().try_clone().map_err(io)?,
    ))
}
