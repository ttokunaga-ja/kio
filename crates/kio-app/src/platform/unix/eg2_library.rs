use crate::eg2::{WorkerError, runtime::VerifiedLibrary};
use std::{
    ffi::{CStr, CString, c_void},
    fs::File,
    io,
    path::Path,
};

pub(crate) fn open_eg2_library(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

pub(crate) fn eg2_c_path(path: &Path) -> Result<CString, WorkerError> {
    CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_| WorkerError::Startup)
}

pub(crate) struct Eg2Library {
    handle: *mut c_void,
    _pin: File,
}

impl Eg2Library {
    pub(crate) fn open(verified: VerifiedLibrary) -> Result<Self, WorkerError> {
        use std::os::unix::fs::MetadataExt;
        let bound = verified
            .file
            .metadata()
            .map_err(|_| WorkerError::RuntimeUnavailable)?;
        let named = verified
            .path
            .symlink_metadata()
            .map_err(|_| WorkerError::RuntimeUnavailable)?;
        if bound.dev() != named.dev() || bound.ino() != named.ino() {
            return Err(WorkerError::RuntimeUnavailable);
        }
        let path = eg2_c_path(&crate::platform::eg2_library_load_path(&verified))?;
        // SAFETY: path is a live C string for the verified image.
        let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return Err(WorkerError::RuntimeUnavailable);
        }
        Ok(Self {
            handle,
            _pin: verified.file,
        })
    }

    pub(crate) fn symbol(&self, name: &CStr) -> Result<*mut c_void, WorkerError> {
        // SAFETY: the loaded image and terminated export name are both live.
        let ptr = unsafe { libc::dlsym(self.handle, name.as_ptr()) };
        if ptr.is_null() {
            Err(WorkerError::RuntimeUnavailable)
        } else {
            Ok(ptr)
        }
    }
}

impl Drop for Eg2Library {
    fn drop(&mut self) {
        // SAFETY: the library owns one loader reference to a non-null image.
        unsafe {
            libc::dlclose(self.handle);
        }
    }
}
