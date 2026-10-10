use std::{ffi::OsStr, fs::File, path::Path};

use crate::Result;

pub(crate) fn validate_store_component(_component: &OsStr, _label: &Path) -> Result<()> {
    Ok(())
}

pub(crate) fn materializable_component(_path: &str) -> bool {
    true
}

pub(crate) fn normalize_management_handles(
    root: File,
    kio: File,
    _canonical_root: &Path,
    _control: &crate::control::ControlHandle,
) -> Result<(File, File)> {
    Ok((root, kio))
}
