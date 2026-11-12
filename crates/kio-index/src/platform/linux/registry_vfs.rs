pub(super) use super::registry_unix_native::{configure_open, native_matches, release_shm_pin};
use crate::{IndexError, Result};
use kio_core::store_dir::StoreDirectory;
use std::{
    ffi::{CStr, CString, c_int},
    fs::File,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

pub(super) struct Binding {
    directory: File,
    locator: CString,
    path: PathBuf,
}
pub(super) fn bind(path: &Path, directory: &StoreDirectory) -> Result<Binding> {
    use std::os::unix::ffi::OsStrExt;
    let file = directory
        .root_handle()
        .try_clone()
        .map_err(|error| IndexError::Schema(error.to_string()))?;
    let leaf = path
        .file_name()
        .ok_or_else(|| IndexError::Schema("registry path has no leaf".into()))?;
    let path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd())).join(leaf);
    let locator = CString::new(path.as_os_str().as_bytes())
        .map_err(|error| IndexError::Schema(error.to_string()))?;
    Ok(Binding {
        directory: file,
        locator,
        path,
    })
}
impl Binding {
    pub(super) fn locator(&self) -> &CStr {
        &self.locator
    }
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
    pub(super) fn execute(&self, _: c_int, operation: impl FnOnce() -> c_int) -> c_int {
        // Keep the directory descriptor alive through every native operation.
        let _retained = &self.directory;
        operation()
    }
}
