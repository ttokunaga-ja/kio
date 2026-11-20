use crate::eg2::{WorkerError, runtime::VerifiedLibrary};
use std::ptr::null_mut;
use std::{
    ffi::{CStr, CString, c_void},
    fs::File,
    io,
    path::Path,
};

pub(crate) fn open_eg2_library(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ};
    File::options()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .share_mode(FILE_SHARE_READ)
        .open(path)
}

pub(crate) fn eg2_c_path(path: &Path) -> Result<CString, WorkerError> {
    CString::new(path.to_str().ok_or(WorkerError::Startup)?.as_bytes())
        .map_err(|_| WorkerError::Startup)
}

pub(crate) struct Eg2Library {
    handle: *mut c_void,
    _pin: File,
}

impl Eg2Library {
    pub(crate) fn open(verified: VerifiedLibrary) -> Result<Self, WorkerError> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::System::LibraryLoader::{
            LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
        };

        let path: Vec<_> = verified.path.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: path is fully qualified and NUL-terminated. The verification
        // handle denies writes/deletion; dependent libraries resolve in System32.
        let handle =
            unsafe { LoadLibraryExW(path.as_ptr(), null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
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
        let ptr = unsafe {
            windows_sys::Win32::System::LibraryLoader::GetProcAddress(
                self.handle,
                name.as_ptr().cast(),
            )
            .map_or(null_mut(), |symbol| symbol as *mut c_void)
        };
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
            windows_sys::Win32::Foundation::FreeLibrary(self.handle);
        }
    }
}
