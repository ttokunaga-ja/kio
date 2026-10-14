//! Bind SQLite's *actual* main-file HANDLE before the pager can use it.
//!
//! Windows source connections use memory-only temporary B-trees. Named
//! sidecars and unnamed disk temporary files remain outside the retained
//! source capability and are refused, including if a caller changes the
//! temporary-storage pragma. SQLite reports memory exhaustion normally.
use crate::platform::BoundSqliteConnection;
use crate::platform::bound_sqlite::{Hooks, RegisteredVfs, header};
use crate::platform::source::{BoundSourceIndex, source_file_identity};
use crate::{IndexError, Result};
use cap_primitives::fs as cap_fs;
use rusqlite::OpenFlags;
use rusqlite::ffi;
use std::cell::Cell;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::sync::Arc;
use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_TYPE_DISK, GetFileInformationByHandle, GetFileType, GetFinalPathNameByHandleW,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity(u32, u64);
struct Opening {
    access: Arc<Access>,
    opened: Cell<bool>,
}
struct Access {
    expected: Identity,
    locator: CString,
    parent: std::fs::File,
    leaf: std::ffi::OsString,
}
const SIDECARS: [&str; 3] = ["-journal", "-wal", "-shm"];

fn sidecar_absent(access: &Access, suffix: &str) -> bool {
    let mut leaf = access.leaf.clone();
    leaf.push(suffix);
    matches!(cap_fs::stat(&access.parent, std::path::Path::new(&leaf), cap_fs::FollowSymlinks::No), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
}

fn identity(handle: HANDLE) -> Option<Identity> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileType(handle) } != FILE_TYPE_DISK
        || unsafe { GetFileInformationByHandle(handle, &mut info) } == 0
        || info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
        || info.nNumberOfLinks != 1
    {
        return None;
    }
    Some(Identity(
        info.dwVolumeSerialNumber,
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

fn locator(handle: HANDLE) -> Result<CString> {
    let required = unsafe { GetFinalPathNameByHandleW(handle, std::ptr::null_mut(), 0, 0) };
    if required == 0 {
        return Err(IndexError::Schema(
            "cannot resolve retained SQLite handle".into(),
        ));
    }
    let mut buffer = vec![0_u16; required as usize];
    let length = unsafe { GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), required, 0) };
    if length == 0 || length >= required {
        return Err(IndexError::Schema(
            "retained SQLite handle path changed".into(),
        ));
    }
    let path = String::from_utf16(&buffer[..length as usize])
        .map_err(|_| IndexError::Schema("SQLite handle path is not Unicode".into()))?;
    CString::new(path).map_err(|_| IndexError::Schema("SQLite handle path contains NUL".into()))
}

fn registration(access: Arc<Access>) -> Result<RegisteredVfs<Opening>> {
    let original = unsafe { ffi::sqlite3_vfs_find(std::ptr::null()) };
    if original.is_null() || unsafe { (*original).xOpen }.is_none() {
        return Err(IndexError::Schema(
            "SQLite has no usable default VFS".into(),
        ));
    }
    // SAFETY: bundled native default callbacks/userdata outlive every owner
    // and support sequential Connection use, including moves between threads.
    unsafe {
        RegisteredVfs::new(
            Opening {
                access,
                opened: Cell::new(false),
            },
            Hooks {
                open: x_open,
                full_path: x_full_pathname,
                access: Some(x_access),
                delete: Some(x_delete),
            },
            original,
        )
    }
}

pub(crate) fn open(source: &BoundSourceIndex, flags: OpenFlags) -> Result<BoundSqliteConnection> {
    let handle = source.file.as_raw_handle();
    let expected = identity(handle).ok_or_else(|| {
        IndexError::Schema("retained SQLite handle is not a single-link regular file".into())
    })?;
    let locator = locator(handle)?;
    let access = Arc::new(Access {
        expected,
        locator,
        parent: source
            ._parent
            .try_clone()
            .map_err(|error| IndexError::Schema(format!("retain SQLite parent: {error}")))?,
        leaf: source
            .public_path
            .file_name()
            .expect("bound source leaf")
            .to_owned(),
    });
    open_with_access(access, flags)
}

fn open_with_access(access: Arc<Access>, flags: OpenFlags) -> Result<BoundSqliteConnection> {
    if !SIDECARS
        .iter()
        .all(|suffix| sidecar_absent(&access, suffix))
    {
        return Err(IndexError::Schema(
            "source SQLite sidecar exists or cannot be inspected".into(),
        ));
    }
    let path = access
        .locator
        .to_str()
        .map_err(|_| IndexError::Schema("invalid SQLite locator".into()))?
        .to_owned();
    let conn =
        BoundSqliteConnection::open_registered(registration(access)?, Path::new(&path), flags)?;
    // xOpen has verified the actual native main-file handle. Keep sorting,
    // materialization and temporary tables usable without granting SQLite
    // authority to create files in an ambient temporary directory.
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    Ok(conn)
}

unsafe fn x_full_pathname(
    state: *mut c_void,
    _: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    len: c_int,
    output: *mut c_char,
) -> c_int {
    if name.is_null() || output.is_null() || len <= 0 {
        return ffi::SQLITE_CANTOPEN;
    }
    let name = unsafe { CStr::from_ptr(name) };
    // Shared trampoline validates the private owner prefix before dispatch.
    let context = unsafe { &*state.cast::<Opening>() };
    let allowed = !context.opened.get() && context.access.locator.as_c_str() == name;
    if !allowed || name.to_bytes_with_nul().len() > len as usize {
        return ffi::SQLITE_CANTOPEN;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(name.as_ptr(), output, name.to_bytes_with_nul().len());
    }
    ffi::SQLITE_OK
}

unsafe fn native_identity(file: *mut ffi::sqlite3_file) -> Option<Identity> {
    if file.is_null() {
        return None;
    }
    let methods = unsafe { (*file).pMethods.as_ref() }?;
    let control = methods.xFileControl?;
    let mut handle: HANDLE = std::ptr::null_mut();
    if unsafe {
        control(
            file,
            ffi::SQLITE_FCNTL_WIN32_GET_HANDLE,
            (&mut handle as *mut HANDLE).cast(),
        )
    } != ffi::SQLITE_OK
    {
        return None;
    }
    identity(handle)
}

unsafe fn reject_opened(file: *mut ffi::sqlite3_file) -> c_int {
    if let Some(methods) = unsafe { (*file).pMethods.as_ref() } {
        if let Some(close) = methods.xClose {
            let _ = unsafe { close(file) };
        }
        // SQLite may call xClose on a failed xOpen when pMethods remains set.
        unsafe {
            (*file).pMethods = std::ptr::null();
        }
    }
    ffi::SQLITE_CANTOPEN
}

unsafe fn x_open(
    state: *mut c_void,
    vfs: *mut ffi::sqlite3_vfs,
    name: ffi::sqlite3_filename,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    if file.is_null() {
        return ffi::SQLITE_CANTOPEN;
    }
    unsafe {
        (*file).pMethods = std::ptr::null();
    }
    // No named journals, WALs, ATTACH targets or temporary database names.
    if name.is_null() || flags & ffi::SQLITE_OPEN_MAIN_DB == 0 {
        return ffi::SQLITE_CANTOPEN;
    }
    let context = unsafe { &*state.cast::<Opening>() };
    if context.opened.get() || context.access.locator.as_c_str() != unsafe { CStr::from_ptr(name) }
    {
        return ffi::SQLITE_CANTOPEN;
    }
    context.opened.set(true);
    let access = Arc::clone(&context.access);
    let Some(owner) = (unsafe { header(vfs) }) else {
        return ffi::SQLITE_CANTOPEN;
    };
    let original_vfs = owner.original;
    let Some(callback) = (unsafe { (*original_vfs).xOpen }) else {
        return ffi::SQLITE_CANTOPEN;
    };
    // Pass SQLite-owned filename storage unchanged; win32 VFS retains it.
    // The retained capability already created the file, so a raced-away name
    // must never grant authority to create a replacement.
    let code = unsafe {
        callback(
            original_vfs,
            name,
            file,
            flags & !ffi::SQLITE_OPEN_CREATE,
            out,
        )
    };
    if code != ffi::SQLITE_OK {
        return code;
    }
    if unsafe { native_identity(file) } != Some(access.expected) {
        return unsafe { reject_opened(file) };
    }
    let original = unsafe { (*file).pMethods };
    let mut methods = unsafe { *original };
    if methods.xClose.is_none() {
        return unsafe { reject_opened(file) };
    }
    methods.xClose = Some(x_close);
    // win32 opens -shm directly from xShmMap, bypassing VFS xOpen.
    methods.xShmMap = Some(x_shm_map);
    methods.xShmUnmap = Some(x_shm_unmap);
    let wrapped = Box::new(Methods {
        methods,
        original,
        _access: access,
    });
    unsafe {
        (*file).pMethods = Box::into_raw(wrapped).cast();
    }
    ffi::SQLITE_OK
}

#[repr(C)]
struct Methods {
    methods: ffi::sqlite3_io_methods,
    original: *const ffi::sqlite3_io_methods,
    _access: Arc<Access>,
}
unsafe extern "C" fn x_close(file: *mut ffi::sqlite3_file) -> c_int {
    let methods = unsafe { Box::from_raw((*file).pMethods.cast_mut().cast::<Methods>()) };
    unsafe {
        (*file).pMethods = methods.original;
    }
    let code = unsafe { ((*methods.original).xClose.expect("verified xClose"))(file) };
    unsafe {
        (*file).pMethods = std::ptr::null();
    }
    code
}
unsafe extern "C" fn x_shm_map(
    _: *mut ffi::sqlite3_file,
    _: c_int,
    _: c_int,
    _: c_int,
    _: *mut *mut c_void,
) -> c_int {
    ffi::SQLITE_IOERR_SHMMAP
}
unsafe extern "C" fn x_shm_unmap(_: *mut ffi::sqlite3_file, _: c_int) -> c_int {
    ffi::SQLITE_OK
}
unsafe fn x_delete(_: *mut c_void, _: *mut ffi::sqlite3_vfs, _: *const c_char, _: c_int) -> c_int {
    ffi::SQLITE_IOERR_DELETE
}
unsafe fn x_access(
    state: *mut c_void,
    _: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    _: c_int,
    output: *mut c_int,
) -> c_int {
    if output.is_null() || name.is_null() {
        return ffi::SQLITE_IOERR_ACCESS;
    }
    unsafe {
        *output = 0;
    }
    let name = unsafe { CStr::from_ptr(name) }.to_bytes();
    let context = unsafe { &*state.cast::<Opening>() };
    let access = &context.access;
    let Some(suffix) = name.strip_prefix(access.locator.to_bytes()) else {
        return ffi::SQLITE_IOERR_ACCESS;
    };
    let Some(suffix) = SIDECARS
        .iter()
        .find(|candidate| candidate.as_bytes() == suffix)
    else {
        return ffi::SQLITE_IOERR_ACCESS;
    };
    let Ok(parent) = source_file_identity(&access.parent) else {
        return ffi::SQLITE_IOERR_ACCESS;
    };
    if parent.volume_serial_number.is_none()
        || parent.file_index.is_none()
        || !sidecar_absent(access, suffix)
    {
        return ffi::SQLITE_IOERR_ACCESS;
    }
    ffi::SQLITE_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{File, OpenOptions};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::platform::bound_sqlite::tests::{name as owner_name, vfs as owner_vfs};
    use std::ptr;

    unsafe fn owner_access(
        vfs: *mut ffi::sqlite3_vfs,
        name: *const c_char,
        flags: c_int,
        output: *mut c_int,
    ) -> c_int {
        unsafe { (*vfs).xAccess.unwrap()(vfs, name, flags, output) }
    }
    unsafe fn owner_full_path(
        vfs: *mut ffi::sqlite3_vfs,
        name: *const c_char,
        len: c_int,
        output: *mut c_char,
    ) -> c_int {
        unsafe { (*vfs).xFullPathname.unwrap()(vfs, name, len, output) }
    }
    unsafe fn owner_delete(vfs: *mut ffi::sqlite3_vfs, name: *const c_char, sync: c_int) -> c_int {
        unsafe { (*vfs).xDelete.unwrap()(vfs, name, sync) }
    }
    struct Filename(ffi::sqlite3_filename);
    impl Filename {
        fn new(path: &CStr) -> Self {
            let name = unsafe {
                ffi::sqlite3_create_filename(
                    path.as_ptr(),
                    c"".as_ptr(),
                    c"".as_ptr(),
                    0,
                    ptr::null_mut(),
                )
            };
            assert!(!name.is_null());
            Self(name)
        }
    }
    impl Drop for Filename {
        fn drop(&mut self) {
            unsafe { ffi::sqlite3_free_filename(self.0) }
        }
    }
    struct NativeFile {
        storage: Vec<u64>,
        filename: Filename,
    }
    impl NativeFile {
        fn new(vfs: *mut ffi::sqlite3_vfs, path: &CStr) -> Self {
            let bytes = usize::try_from(unsafe { (*vfs).szOsFile }).unwrap();
            Self {
                storage: vec![0; bytes.div_ceil(std::mem::size_of::<u64>())],
                filename: Filename::new(path),
            }
        }
        fn file(&mut self) -> *mut ffi::sqlite3_file {
            self.storage.as_mut_ptr().cast()
        }
        fn open(
            &mut self,
            dispatch: *mut ffi::sqlite3_vfs,
            argument: *mut ffi::sqlite3_vfs,
            flags: c_int,
        ) -> c_int {
            let filename = self.filename.0;
            let file = self.file();
            let mut out = 0;
            unsafe { (*dispatch).xOpen.unwrap()(argument, filename, file, flags, &mut out) }
        }
        fn close(&mut self) {
            let file = self.file();
            let methods = unsafe { (*file).pMethods };
            if !methods.is_null() {
                let close = unsafe { (*methods).xClose }.unwrap();
                assert_eq!(unsafe { close(file) }, ffi::SQLITE_OK);
                assert!(unsafe { (*file).pMethods }.is_null());
            }
        }
    }
    impl Drop for NativeFile {
        fn drop(&mut self) {
            // Filename/storage and owner registration are still live through close.
            // A refused native open has already cleared pMethods and is not closed twice.
            self.close();
        }
    }
    fn main_flags() -> c_int {
        ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_MAIN_DB
    }
    fn access_answer(vfs: *mut ffi::sqlite3_vfs, name: &CStr) -> (c_int, c_int) {
        let mut exists = -1;
        let code =
            unsafe { owner_access(vfs, name.as_ptr(), ffi::SQLITE_ACCESS_EXISTS, &mut exists) };
        (code, exists)
    }

    fn test_access(locator: &CStr, expected: Identity) -> Arc<Access> {
        use std::os::windows::fs::OpenOptionsExt;
        let path = std::path::Path::new(locator.to_str().unwrap());
        Arc::new(Access {
            expected,
            locator: locator.to_owned(),
            parent: OpenOptions::new()
                .read(true)
                .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS)
                .open(path.parent().unwrap())
                .unwrap(),
            leaf: path.file_name().unwrap().to_owned(),
        })
    }
    fn open_locator(
        locator: CString,
        expected: Identity,
        flags: OpenFlags,
    ) -> Result<BoundSqliteConnection> {
        let access = test_access(&locator, expected);
        open_with_access(access, flags)
    }

    fn fixture() -> (tempfile::TempDir, File, CString, Identity) {
        let directory = tempfile::tempdir().unwrap();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(directory.path().join("source.db"))
            .unwrap();
        let path = locator(file.as_raw_handle()).unwrap();
        let expected = identity(file.as_raw_handle()).unwrap();
        (directory, file, path, expected)
    }

    #[test]
    fn retained_handle_matches_and_memory_writes_succeed() {
        let (directory, _file, path, expected) = fixture();
        let conn = open_locator(path, expected, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
        conn.execute_batch("PRAGMA journal_mode=MEMORY; CREATE TABLE retained(value); INSERT INTO retained VALUES (42);").unwrap();
        assert_eq!(
            conn.query_row("SELECT value FROM retained", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            42
        );
        drop(conn);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn materialization_and_large_sort_use_memory_only_temporaries() {
        let (directory, _file, path, expected) = fixture();
        let conn = open_locator(path, expected, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
        assert_eq!(
            conn.query_row("PRAGMA temp_store", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            2
        );
        conn.execute_batch(
            "PRAGMA journal_mode=MEMORY;
            PRAGMA cache_size=8;
            CREATE TABLE source(value);
            INSERT INTO source VALUES (42);
            CREATE TEMP TABLE materialized AS
            WITH RECURSIVE input(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM input WHERE n<12000)
            SELECT n, printf('%08d',12001-n)||hex(zeroblob(256)) AS payload FROM input;",
        )
        .unwrap();
        let plan = conn
            .prepare("EXPLAIN QUERY PLAN SELECT n FROM materialized ORDER BY payload")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            plan.iter().any(|detail| detail.contains("TEMP B-TREE")),
            "the fixture must exercise SQLite's temporary sorter: {plan:?}"
        );
        let sorted = conn
            .prepare("SELECT n FROM materialized ORDER BY payload")
            .unwrap()
            .query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(sorted, (1..=12000).rev().collect::<Vec<_>>());
        assert_eq!(
            conn.query_row("SELECT value FROM source", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            42
        );
        drop(conn);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn wrong_existing_file_is_rejected_before_sql_use() {
        let (_directory, _file, _path, expected) = fixture();
        let (_other_directory, _other_file, other_path, _) = fixture();
        assert!(open_locator(other_path, expected, OpenFlags::SQLITE_OPEN_READ_WRITE).is_err());
        // A rejected open must also tear down its connection owner.
        let (_directory, _file, path, expected) = fixture();
        assert!(open_locator(path, expected, OpenFlags::SQLITE_OPEN_READ_WRITE).is_ok());
    }

    #[test]
    fn create_flag_cannot_create_a_foreign_file() {
        let (directory, _file, _path, expected) = fixture();
        let missing = directory.path().join("foreign.db");
        let missing_locator = CString::new(missing.to_str().unwrap()).unwrap();
        assert!(
            open_locator(
                missing_locator,
                expected,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
            )
            .is_err()
        );
        assert!(!missing.exists());
    }

    #[test]
    fn relative_name_cannot_replace_handle_locator() {
        let (_directory, _file, path, expected) = fixture();
        let owner = registration(test_access(&path, expected)).unwrap();
        let vfs = owner_vfs(&owner);
        let mut output = [0_i8; 512];
        assert_eq!(
            unsafe {
                owner_full_path(
                    vfs,
                    c"source.db".as_ptr(),
                    output.len() as c_int,
                    output.as_mut_ptr(),
                )
            },
            ffi::SQLITE_CANTOPEN
        );
    }

    #[test]
    fn hardlinked_or_directory_handles_are_rejected() {
        let (directory, file, _path, _expected) = fixture();
        std::fs::hard_link(
            directory.path().join("source.db"),
            directory.path().join("alias.db"),
        )
        .unwrap();
        assert_eq!(identity(file.as_raw_handle()), None);
        use std::os::windows::fs::OpenOptionsExt;
        let directory_handle = OpenOptions::new()
            .read(true)
            .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS)
            .open(directory.path())
            .unwrap();
        assert_eq!(identity(directory_handle.as_raw_handle()), None);
    }

    #[test]
    fn concurrent_connections_keep_independent_contexts() {
        let threads: Vec<_> = (0..4)
            .map(|_| {
                std::thread::spawn(|| {
                    let (_directory, _file, path, expected) = fixture();
                    let conn =
                        open_locator(path, expected, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
                    conn.execute_batch("PRAGMA journal_mode=MEMORY; CREATE TABLE t(v);")
                        .unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
    }

    static CLOSES: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn fake_close(_: *mut ffi::sqlite3_file) -> c_int {
        CLOSES.fetch_add(1, Ordering::SeqCst);
        ffi::SQLITE_OK
    }
    unsafe extern "C" fn refuse_control(
        _: *mut ffi::sqlite3_file,
        _: c_int,
        _: *mut c_void,
    ) -> c_int {
        ffi::SQLITE_NOTFOUND
    }
    unsafe extern "C" fn unknown_handle(
        _: *mut ffi::sqlite3_file,
        _: c_int,
        _: *mut c_void,
    ) -> c_int {
        ffi::SQLITE_OK
    }

    #[test]
    fn missing_refused_and_unknown_handle_fail_closed_and_close_once() {
        for control in [
            None,
            Some(refuse_control as unsafe extern "C" fn(_, _, _) -> _),
            Some(unknown_handle),
        ] {
            let mut methods: ffi::sqlite3_io_methods = unsafe { std::mem::zeroed() };
            methods.xClose = Some(fake_close);
            methods.xFileControl = control;
            let mut file = ffi::sqlite3_file { pMethods: &methods };
            assert_eq!(unsafe { native_identity(&mut file) }, None);
            let before = CLOSES.load(Ordering::SeqCst);
            assert_eq!(unsafe { reject_opened(&mut file) }, ffi::SQLITE_CANTOPEN);
            assert!(file.pMethods.is_null());
            assert_eq!(CLOSES.load(Ordering::SeqCst), before + 1);
            unsafe {
                reject_opened(&mut file);
            }
            assert_eq!(CLOSES.load(Ordering::SeqCst), before + 1);
        }
    }

    #[test]
    fn existing_and_late_sidecars_fail_closed() {
        for suffix in SIDECARS {
            let (directory, _file, path, expected) = fixture();
            let sidecar = directory.path().join(format!("source.db{suffix}"));
            std::fs::write(&sidecar, b"untrusted recovery data").unwrap();
            assert!(
                open_locator(path.clone(), expected, OpenFlags::SQLITE_OPEN_READ_WRITE).is_err()
            );
            std::fs::remove_file(&sidecar).unwrap();
            let conn =
                open_locator(path.clone(), expected, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
            let vfs = conn.registered_vfs_for_test().unwrap();
            let name = CString::new(format!("{}{suffix}", path.to_str().unwrap())).unwrap();
            let mut exists = -1;
            assert_eq!(
                unsafe { owner_access(vfs, name.as_ptr(), ffi::SQLITE_ACCESS_EXISTS, &mut exists) },
                ffi::SQLITE_OK
            );
            assert_eq!(exists, 0);
            std::fs::write(&sidecar, b"late recovery data").unwrap();
            assert_eq!(
                unsafe { owner_access(vfs, name.as_ptr(), ffi::SQLITE_ACCESS_EXISTS, &mut exists) },
                ffi::SQLITE_IOERR_ACCESS
            );
            drop(conn);
        }
        let (_directory, _file, path, expected) = fixture();
        let unknown_owner = registration(test_access(&path, expected)).unwrap();
        let vfs = owner_vfs(&unknown_owner);
        let mut exists = -1;
        assert_eq!(
            unsafe {
                owner_access(
                    vfs,
                    c"unknown.db-journal".as_ptr(),
                    ffi::SQLITE_ACCESS_EXISTS,
                    &mut exists,
                )
            },
            ffi::SQLITE_IOERR_ACCESS
        );
    }

    #[test]
    fn connection_owners_isolate_same_locator_authority() {
        for drop_correct_first in [false, true] {
            let (_directory, _file, path, expected) = fixture();
            let conn =
                open_locator(path.clone(), expected, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
            let correct = conn.registered_vfs_for_test().unwrap();
            let journal = CString::new(format!("{}-journal", path.to_str().unwrap())).unwrap();
            let before = access_answer(correct, &journal);
            assert_eq!(before, (ffi::SQLITE_OK, 0));

            let bad_access = test_access(&path, Identity(expected.0, expected.1 ^ 1));
            let other = registration(Arc::clone(&bad_access)).unwrap();
            let bad = owner_vfs(&other);
            let bad_name = owner_name(&other);
            assert_ne!(correct, bad);
            assert_eq!(access_answer(correct, &journal), before);
            assert_eq!(access_answer(bad, &journal), before);

            // Same actual native xOpen identity rejection as existing behaviour.
            // The registration remains live while the refused file is inspected.
            let mut rejected = NativeFile::new(bad, &path);
            assert_eq!(rejected.open(bad, bad, main_flags()), ffi::SQLITE_CANTOPEN);
            assert!(unsafe { (*rejected.file()).pMethods }.is_null());
            drop(rejected);
            // A fresh owner with the identical Arc<Access> also cannot become a
            // rusqlite connection; the direct check above proves native refusal
            // clears pMethods, rather than merely observing an SQL-level error.
            assert!(
                BoundSqliteConnection::open_registered(
                    registration(Arc::clone(&bad_access)).unwrap(),
                    Path::new(path.to_str().unwrap()),
                    OpenFlags::SQLITE_OPEN_READ_WRITE,
                )
                .is_err()
            );
            assert_eq!(access_answer(correct, &journal), before);

            // Read only immutable marker/userdata fields. Never copy/read pNext.
            let mut copied: ffi::sqlite3_vfs = unsafe { std::mem::zeroed() };
            copied.pAppData = unsafe { (*correct).pAppData };
            copied.xOpen = unsafe { (*correct).xOpen };
            let foreign = unsafe { ffi::sqlite3_vfs_find(ptr::null()) };
            assert!(!foreign.is_null());
            for wrong in [ptr::null_mut(), foreign, ptr::addr_of_mut!(copied)] {
                let mut exists = -1;
                let mut output = vec![0 as c_char; path.to_bytes_with_nul().len()];
                let mut closed = NativeFile::new(correct, &path);
                assert_eq!(
                    unsafe {
                        (*correct).xAccess.unwrap()(
                            wrong,
                            journal.as_ptr(),
                            ffi::SQLITE_ACCESS_EXISTS,
                            &mut exists,
                        )
                    },
                    ffi::SQLITE_IOERR_ACCESS
                );
                assert_eq!(
                    closed.open(correct, wrong, main_flags()),
                    ffi::SQLITE_CANTOPEN
                );
                assert!(unsafe { (*closed.file()).pMethods }.is_null());
                assert_eq!(
                    unsafe {
                        (*correct).xFullPathname.unwrap()(
                            wrong,
                            path.as_ptr(),
                            output.len() as c_int,
                            output.as_mut_ptr(),
                        )
                    },
                    ffi::SQLITE_CANTOPEN
                );
            }

            if drop_correct_first {
                drop(conn);
                assert_eq!(access_answer(bad, &journal), before);
                assert_eq!(unsafe { ffi::sqlite3_vfs_find(bad_name.as_ptr()) }, bad);
                drop(other);
            } else {
                drop(other);
                assert!(unsafe { ffi::sqlite3_vfs_find(bad_name.as_ptr()) }.is_null());
                assert_eq!(access_answer(correct, &journal), before);
                assert_eq!(
                    conn.query_row("SELECT 42", [], |r| r.get::<_, i64>(0))
                        .unwrap(),
                    42
                );
                drop(conn);
            }
            assert!(unsafe { ffi::sqlite3_vfs_find(bad_name.as_ptr()) }.is_null());
        }
    }

    #[test]
    fn sidecar_open_delete_and_shared_memory_are_refused() {
        let (directory, _file, path, expected) = fixture();
        let conn = open_locator(path, expected, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
        assert!(
            conn.execute_batch("PRAGMA journal_mode=DELETE; CREATE TABLE forbidden(v);")
                .is_err()
        );
        assert!(
            conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE forbidden_wal(v);")
                .is_err()
        );
        let vfs = conn.registered_vfs_for_test().unwrap();
        assert_eq!(
            unsafe { owner_delete(vfs, c"foreign.db".as_ptr(), 0) },
            ffi::SQLITE_IOERR_DELETE
        );
        drop(conn);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn main_file_authority_is_single_use_and_locator_bound() {
        let (_directory, _file, path, expected) = fixture();
        let owner = registration(test_access(&path, expected)).unwrap();
        let vfs = owner_vfs(&owner);
        let mut relative = NativeFile::new(vfs, c"source.db");
        assert_eq!(relative.open(vfs, vfs, main_flags()), ffi::SQLITE_CANTOPEN);
        assert!(unsafe { (*relative.file()).pMethods }.is_null());
        let mut nonmain = NativeFile::new(vfs, &path);
        assert_eq!(
            nonmain.open(
                vfs,
                vfs,
                ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_MAIN_JOURNAL
            ),
            ffi::SQLITE_CANTOPEN
        );
        assert!(unsafe { (*nonmain.file()).pMethods }.is_null());
        let mut null_name = NativeFile::new(vfs, &path);
        let mut out = 0;
        assert_eq!(
            unsafe {
                (*vfs).xOpen.unwrap()(vfs, ptr::null(), null_name.file(), main_flags(), &mut out)
            },
            ffi::SQLITE_CANTOPEN
        );
        assert!(unsafe { (*null_name.file()).pMethods }.is_null());

        let mut main = NativeFile::new(vfs, &path);
        assert_eq!(main.open(vfs, vfs, main_flags()), ffi::SQLITE_OK);
        assert_eq!(unsafe { native_identity(main.file()) }, Some(expected));
        let mut repeated = NativeFile::new(vfs, &path);
        assert_eq!(repeated.open(vfs, vfs, main_flags()), ffi::SQLITE_CANTOPEN);
        assert!(unsafe { (*repeated.file()).pMethods }.is_null());
        let mut output = vec![0 as c_char; path.to_bytes_with_nul().len()];
        assert_eq!(
            unsafe {
                owner_full_path(
                    vfs,
                    path.as_ptr(),
                    output.len() as c_int,
                    output.as_mut_ptr(),
                )
            },
            ffi::SQLITE_CANTOPEN
        );
        main.close();
        // Owner declared before every manual file, so unwind also closes first.
        drop(repeated);
        drop(main);
        drop(null_name);
        drop(nonmain);
        drop(relative);
        drop(owner);
    }

    #[test]
    fn native_methods_retain_access_until_close_and_refuse_shm() {
        let (directory, _file, path, expected) = fixture();
        let access = test_access(&path, expected);
        let weak = Arc::downgrade(&access);
        let owner = registration(access).unwrap();
        let vfs = owner_vfs(&owner);
        let mut main = NativeFile::new(vfs, &path);
        assert_eq!(main.open(vfs, vfs, main_flags()), ffi::SQLITE_OK);
        let file = main.file();
        let methods = unsafe { (*file).pMethods };
        assert!(!methods.is_null());
        // Verified native xOpen installs this private repr(C) Methods table.
        let retained = unsafe { &*methods.cast::<Methods>() };
        assert!(std::sync::Weak::ptr_eq(
            &weak,
            &Arc::downgrade(&retained._access)
        ));
        let before_close = weak.strong_count();
        assert!(before_close >= 2);
        let mut mapped: *mut c_void = ptr::null_mut();
        assert_eq!(
            unsafe { (*methods).xShmMap.unwrap()(file, 0, 4096, 0, &mut mapped) },
            ffi::SQLITE_IOERR_SHMMAP
        );
        assert!(mapped.is_null());
        assert_eq!(
            unsafe { (*methods).xShmUnmap.unwrap()(file, 0) },
            ffi::SQLITE_OK
        );
        assert!(!directory.path().join("source.db-shm").exists());
        main.close();
        assert_eq!(weak.strong_count(), before_close - 1);
        assert!(weak.upgrade().is_some());
        drop(main);
        drop(owner);
        assert!(weak.upgrade().is_none());
    }
}
