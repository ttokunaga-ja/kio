use crate::Result;
use crate::platform::store as platform;
use std::{ffi::OsStr, fs::File, path::Path};

pub(crate) fn rename(
    parent: &File,
    source: &OsStr,
    destination: &OsStr,
    _held: &File,
    label: &Path,
) -> Result<()> {
    platform::rename_regular_noreplace(parent, source, parent, destination, label)
}
