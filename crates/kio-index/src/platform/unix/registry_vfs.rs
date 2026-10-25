use cap_primitives::fs as cap_fs;
use rusqlite::ffi;
use std::{ffi::c_int, fs::File};
pub(super) fn configure_open(_: &mut cap_fs::OpenOptions) {}
pub(super) fn release_shm_pin(_: &mut Option<File>, _: c_int) {}
pub(super) unsafe fn native_matches(file: *mut ffi::sqlite3_file, _: &File) -> bool {
    let Some(control) = (unsafe { (*(*file).pMethods).xFileControl }) else {
        return false;
    };
    let mut moved: c_int = 1;
    (unsafe {
        control(
            file,
            ffi::SQLITE_FCNTL_HAS_MOVED,
            (&mut moved as *mut c_int).cast(),
        )
    }) == ffi::SQLITE_OK
        && moved == 0
}
