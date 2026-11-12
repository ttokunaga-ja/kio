use crate::{IndexError, Result};
use cap_primitives::fs as cap_fs;
use kio_core::store_dir::StoreDirectory;
use rusqlite::ffi;
use std::{
    ffi::{CStr, CString, c_int},
    fs::File,
    path::{Component, Path, PathBuf},
};
pub(super) fn configure_open(options: &mut cap_fs::OpenOptions) {
    use cap_fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
}
pub(super) fn release_shm_pin(pin: &mut Option<File>, delete: c_int) {
    if delete != 0 {
        drop(pin.take());
    }
}
pub(super) struct Binding {
    _ancestors: Vec<File>,
    locator: CString,
    path: PathBuf,
}
// Pin every named ancestor without admitting its ownership or ACL. Omitting
// FILE_SHARE_DELETE prevents a parent rename from redirecting native SHM opens.
pub(super) fn bind(path: &Path, directory: &StoreDirectory) -> Result<Binding> {
    bind_with_snapshot(path, directory, None)
}
pub(super) fn bind_snapshot(
    path: &Path,
    directory: &StoreDirectory,
    snapshot: &super::registry_security::RegistrySnapshotPrivateDir,
) -> Result<Binding> {
    bind_with_snapshot(path, directory, Some(snapshot))
}
fn bind_with_snapshot(
    path: &Path,
    directory: &StoreDirectory,
    snapshot: Option<&super::registry_security::RegistrySnapshotPrivateDir>,
) -> Result<Binding> {
    use cap_fs::OpenOptionsExt;
    use std::os::windows::fs::OpenOptionsExt as StdOpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, SYNCHRONIZE,
    };
    let parent = path
        .parent()
        .ok_or_else(|| IndexError::Schema("registry path has no parent".into()))?;
    let mut root = PathBuf::new();
    let mut names = Vec::new();
    for component in parent.components() {
        match component {
            Component::Prefix(prefix) => root.push(prefix.as_os_str()),
            Component::RootDir => root.push(component.as_os_str()),
            Component::Normal(name) => names.push(name.to_owned()),
            _ => {
                return Err(IndexError::Schema(
                    "registry native locator is not normalized".into(),
                ));
            }
        }
    }
    if !root.is_absolute() {
        return Err(IndexError::Schema(
            "registry native locator must be absolute".into(),
        ));
    }
    let access = FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
    let root = std::fs::OpenOptions::new()
        .read(true)
        .access_mode(access)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&root)
        .map_err(|error| IndexError::Schema(error.to_string()))?;
    if kio_core::platform::directory_handle_identity(&root).is_none() {
        return Err(IndexError::Schema(
            "registry native locator root is not a real directory".into(),
        ));
    }
    let mut ancestors = vec![root];
    let count = names.len();
    for (index, name) in names.into_iter().enumerate() {
        if index + 1 == count
            && let Some(snapshot) = snapshot
        {
            // This opaque storage type already owns a protected, no-delete
            // directory pin with DELETE access for later exact-handle cleanup.
            // Reopening another no-delete handle would conflict with that
            // access. Reuse the trusted pin, never relax sharing on a new open.
            let pinned = snapshot
                .capability()
                .map_err(|error| IndexError::Schema(error.to_string()))?;
            let named = cap_fs::stat(
                ancestors.last().expect("root pinned"),
                Path::new(&name),
                cap_fs::FollowSymlinks::No,
            )
            .map_err(|error| IndexError::Schema(error.to_string()))?;
            if !named.is_dir() || super::leaf_identity(&named) != identity(&pinned) {
                return Err(IndexError::Schema(
                    "private snapshot named root changed before binding".into(),
                ));
            }
            ancestors.push(pinned);
            continue;
        }
        let mut options = cap_fs::OpenOptions::new();
        options
            .read(true)
            .access_mode(access)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
        let next = cap_fs::open(
            ancestors.last().expect("root pinned"),
            Path::new(&name),
            &options,
        )
        .map_err(|error| IndexError::Schema(error.to_string()))?;
        if kio_core::platform::directory_handle_identity(&next).is_none() {
            return Err(IndexError::Schema(
                "registry native locator contains a reparse or non-directory component".into(),
            ));
        }
        ancestors.push(next);
    }
    if kio_core::platform::directory_handle_identity(ancestors.last().expect("root pinned"))
        != kio_core::platform::directory_handle_identity(&directory.root_handle())
    {
        return Err(IndexError::Schema(
            "registry native locator parent changed while binding".into(),
        ));
    }
    let locator = CString::new(
        path.to_str()
            .ok_or_else(|| IndexError::Schema("registry path is not Unicode".into()))?,
    )
    .map_err(|error| IndexError::Schema(error.to_string()))?;
    Ok(Binding {
        _ancestors: ancestors,
        locator,
        path: path.to_path_buf(),
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
        operation()
    }
}
fn identity(file: &File) -> Option<super::FileIdentity> {
    let metadata = cap_fs::Metadata::from_file(file).ok()?;
    super::leaf_identity(&metadata)
}
pub(super) unsafe fn native_matches(file: *mut ffi::sqlite3_file, pin: &File) -> bool {
    use std::{mem::ManuallyDrop, os::windows::io::FromRawHandle};
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    let Some(control) = (unsafe { (*(*file).pMethods).xFileControl }) else {
        return false;
    };
    let mut handle: HANDLE = std::ptr::null_mut();
    if unsafe {
        control(
            file,
            ffi::SQLITE_FCNTL_WIN32_GET_HANDLE,
            (&mut handle as *mut HANDLE).cast(),
        )
    } != ffi::SQLITE_OK
        || handle.is_null()
        || handle == INVALID_HANDLE_VALUE
    {
        return false;
    }
    // Borrow SQLite's live native handle without transferring ownership.
    let native = ManuallyDrop::new(unsafe { File::from_raw_handle(handle.cast()) });
    kio_core::private_fs::verify_private_regular_file_handle(&native).is_ok()
        && identity(&native) == identity(pin)
}
