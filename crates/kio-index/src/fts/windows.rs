//! Bind SQLite's *actual* main-file HANDLE before the pager can use it.
//!
//! Windows source connections use memory-only temporary B-trees. Named
//! sidecars and unnamed disk temporary files remain outside the retained
//! source capability and are refused, including if a caller changes the
//! temporary-storage pragma. SQLite reports memory exhaustion normally.
use super::{BoundSourceIndex, Connection, IndexError, OpenFlags, Result};
use rusqlite::ffi;
use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::os::windows::io::AsRawHandle;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_TYPE_DISK, GetFileInformationByHandle, GetFileType, GetFinalPathNameByHandleW,
};

const NAME: &CStr = c"kio-bound-source-windows";
static VFS: OnceLock<std::result::Result<usize, String>> = OnceLock::new();
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity(u32, u64);
struct Opening {
    locator: CString,
    expected: Identity,
    access: Arc<Access>,
    opened: bool,
}
struct Access {
    expected: Identity,
    locator: CString,
    parent: std::fs::File,
    leaf: std::ffi::OsString,
}
static ACCESS: Mutex<Vec<(CString, Weak<Access>)>> = Mutex::new(Vec::new());
impl Drop for Access {
    fn drop(&mut self) {
        if let Ok(mut entries) = ACCESS.lock() {
            entries.retain(|(_, entry)| !std::ptr::eq(entry.as_ptr(), self));
        }
    }
}
const SIDECARS: [&str; 3] = ["-journal", "-wal", "-shm"];

fn sidecar_absent(access: &Access, suffix: &str) -> bool {
    let mut leaf = access.leaf.clone();
    leaf.push(suffix);
    matches!(super::cap_fs::stat(&access.parent, std::path::Path::new(&leaf), super::cap_fs::FollowSymlinks::No), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
}

thread_local! {
    static OPENING: RefCell<Option<Opening>> = const { RefCell::new(None) };
}
struct OpeningGuard;
impl Drop for OpeningGuard {
    fn drop(&mut self) {
        OPENING.with(|slot| *slot.borrow_mut() = None);
    }
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

fn default_vfs() -> Result<*mut ffi::sqlite3_vfs> {
    let result = VFS.get_or_init(|| unsafe {
        let original = ffi::sqlite3_vfs_find(std::ptr::null());
        if original.is_null() || (*original).xOpen.is_none() {
            return Err("SQLite has no usable default VFS".into());
        }
        let mut wrapped = Box::new(*original);
        wrapped.zName = NAME.as_ptr();
        wrapped.xOpen = Some(x_open);
        wrapped.xFullPathname = Some(x_full_pathname);
        wrapped.xDelete = Some(x_delete);
        wrapped.xAccess = Some(x_access);
        let wrapped = Box::into_raw(wrapped);
        let code = ffi::sqlite3_vfs_register(wrapped, 0);
        if code != ffi::SQLITE_OK {
            drop(Box::from_raw(wrapped));
            return Err(format!("register bound Windows SQLite VFS: {code}"));
        }
        Ok(original as usize)
    });
    result
        .as_ref()
        .copied()
        .map(|ptr| ptr as *mut ffi::sqlite3_vfs)
        .map_err(|error| IndexError::Schema(error.clone()))
}

pub(super) fn open(source: &BoundSourceIndex, flags: OpenFlags) -> Result<Connection> {
    default_vfs()?;
    let handle = source.file.as_raw_handle();
    let expected = identity(handle).ok_or_else(|| {
        IndexError::Schema("retained SQLite handle is not a single-link regular file".into())
    })?;
    let locator = locator(handle)?;
    let access = Arc::new(Access {
        expected,
        locator: locator.clone(),
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
    open_with_access(locator, expected, flags, access)
}

fn open_with_access(
    locator: CString,
    expected: Identity,
    flags: OpenFlags,
    access: Arc<Access>,
) -> Result<Connection> {
    if !SIDECARS
        .iter()
        .all(|suffix| sidecar_absent(&access, suffix))
    {
        return Err(IndexError::Schema(
            "source SQLite sidecar exists or cannot be inspected".into(),
        ));
    }
    {
        let mut entries = ACCESS
            .lock()
            .map_err(|_| IndexError::Schema("SQLite access registry poisoned".into()))?;
        entries.retain(|(_, entry)| entry.strong_count() != 0);
        entries.push((access.locator.clone(), Arc::downgrade(&access)));
    }
    let path = locator
        .to_str()
        .map_err(|_| IndexError::Schema("invalid SQLite locator".into()))?
        .to_owned();
    OPENING.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_some() {
            return Err(IndexError::Schema("nested bound SQLite open".into()));
        }
        *slot = Some(Opening {
            locator,
            expected,
            access,
            opened: false,
        });
        Ok(())
    })?;
    let _guard = OpeningGuard;
    let conn =
        Connection::open_with_flags_and_vfs(path, flags, NAME.to_str().expect("ASCII VFS name"))?;
    // xOpen has verified the actual native main-file handle. Keep sorting,
    // materialization and temporary tables usable without granting SQLite
    // authority to create files in an ambient temporary directory.
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    Ok(conn)
}

unsafe extern "C" fn x_full_pathname(
    _: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    len: c_int,
    output: *mut c_char,
) -> c_int {
    if name.is_null() || output.is_null() || len <= 0 {
        return ffi::SQLITE_CANTOPEN;
    }
    let name = unsafe { CStr::from_ptr(name) };
    let allowed = OPENING.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|context| !context.opened && context.locator.as_c_str() == name)
    });
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

unsafe extern "C" fn x_open(
    _: *mut ffi::sqlite3_vfs,
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
    let expected = OPENING.with(|slot| {
        let mut slot = slot.borrow_mut();
        let context = slot.as_mut()?;
        if context.opened || context.locator.as_c_str() != unsafe { CStr::from_ptr(name) } {
            return None;
        }
        context.opened = true;
        Some((context.expected, Arc::clone(&context.access)))
    });
    let Some((expected, access)) = expected else {
        return ffi::SQLITE_CANTOPEN;
    };
    let Ok(vfs) = default_vfs() else {
        return ffi::SQLITE_CANTOPEN;
    };
    let Some(callback) = (unsafe { (*vfs).xOpen }) else {
        return ffi::SQLITE_CANTOPEN;
    };
    // Pass SQLite-owned filename storage unchanged; win32 VFS retains it.
    // The retained capability already created the file, so a raced-away name
    // must never grant authority to create a replacement.
    let code = unsafe { callback(vfs, name, file, flags & !ffi::SQLITE_OPEN_CREATE, out) };
    if code != ffi::SQLITE_OK {
        return code;
    }
    if unsafe { native_identity(file) } != Some(expected) {
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
unsafe extern "C" fn x_delete(_: *mut ffi::sqlite3_vfs, _: *const c_char, _: c_int) -> c_int {
    ffi::SQLITE_IOERR_DELETE
}
unsafe extern "C" fn x_access(
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
    // Snapshot matching weak references under the registry lock. Upgrade and
    // perform capability I/O after unlocking; unrelated connection closes
    // cannot invalidate this callback.
    let contexts = {
        let Ok(entries) = ACCESS.lock() else {
            return ffi::SQLITE_IOERR_ACCESS;
        };
        entries
            .iter()
            .filter(|(locator, _)| {
                name.strip_prefix(locator.to_bytes()).is_some_and(|suffix| {
                    SIDECARS
                        .iter()
                        .any(|candidate| candidate.as_bytes() == suffix)
                })
            })
            .map(|(_, entry)| entry.clone())
            .collect::<Vec<_>>()
    };
    let Some(contexts) = contexts
        .iter()
        .map(Weak::upgrade)
        .collect::<Option<Vec<_>>>()
    else {
        return ffi::SQLITE_IOERR_ACCESS;
    };
    let mut matched = false;
    let mut binding = None;
    for access in contexts {
        let Some(suffix) = name.strip_prefix(access.locator.to_bytes()) else {
            continue;
        };
        let Some(suffix) = SIDECARS
            .iter()
            .find(|candidate| candidate.as_bytes() == suffix)
        else {
            continue;
        };
        let Ok(parent) = super::source_file_identity(&access.parent) else {
            return ffi::SQLITE_IOERR_ACCESS;
        };
        if parent.volume_serial_number.is_none() || parent.file_index.is_none() {
            return ffi::SQLITE_IOERR_ACCESS;
        }
        let current = (parent, access.expected);
        if binding.is_some_and(|previous| previous != current) {
            return ffi::SQLITE_IOERR_ACCESS;
        }
        binding = Some(current);
        matched = true;
        if !sidecar_absent(&access, suffix) {
            return ffi::SQLITE_IOERR_ACCESS;
        }
    }
    if matched {
        ffi::SQLITE_OK
    } else {
        ffi::SQLITE_IOERR_ACCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{File, OpenOptions};
    use std::sync::atomic::{AtomicUsize, Ordering};

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
    fn open_locator(locator: CString, expected: Identity, flags: OpenFlags) -> Result<Connection> {
        let access = test_access(&locator, expected);
        open_with_access(locator, expected, flags, access)
    }

    fn fixture() -> (tempfile::TempDir, File, CString, Identity) {
        default_vfs().unwrap();
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
        // A rejected open must also clear the per-thread context.
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
        OPENING.with(|slot| {
            *slot.borrow_mut() = Some(Opening {
                access: test_access(&path, expected),
                locator: path,
                expected,
                opened: false,
            })
        });
        let _guard = OpeningGuard;
        let mut output = [0_i8; 512];
        assert_eq!(
            unsafe {
                x_full_pathname(
                    std::ptr::null_mut(),
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
            let name = CString::new(format!("{}{suffix}", path.to_str().unwrap())).unwrap();
            let mut exists = -1;
            assert_eq!(
                unsafe {
                    x_access(
                        std::ptr::null_mut(),
                        name.as_ptr(),
                        ffi::SQLITE_ACCESS_EXISTS,
                        &mut exists,
                    )
                },
                ffi::SQLITE_OK
            );
            assert_eq!(exists, 0);
            std::fs::write(&sidecar, b"late recovery data").unwrap();
            assert_eq!(
                unsafe {
                    x_access(
                        std::ptr::null_mut(),
                        name.as_ptr(),
                        ffi::SQLITE_ACCESS_EXISTS,
                        &mut exists,
                    )
                },
                ffi::SQLITE_IOERR_ACCESS
            );
            drop(conn);
        }
        let mut exists = -1;
        assert_eq!(
            unsafe {
                x_access(
                    std::ptr::null_mut(),
                    c"unknown.db-journal".as_ptr(),
                    ffi::SQLITE_ACCESS_EXISTS,
                    &mut exists,
                )
            },
            ffi::SQLITE_IOERR_ACCESS
        );
    }

    #[test]
    fn conflicting_live_binding_for_same_locator_is_rejected() {
        let (_directory, _file, path, expected) = fixture();
        let conn = open_locator(path.clone(), expected, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
        let other = test_access(&path, Identity(expected.0, expected.1 ^ 1));
        ACCESS
            .lock()
            .unwrap()
            .push((other.locator.clone(), Arc::downgrade(&other)));
        let name = CString::new(format!("{}-journal", path.to_str().unwrap())).unwrap();
        let mut exists = -1;
        assert_eq!(
            unsafe {
                x_access(
                    std::ptr::null_mut(),
                    name.as_ptr(),
                    ffi::SQLITE_ACCESS_EXISTS,
                    &mut exists,
                )
            },
            ffi::SQLITE_IOERR_ACCESS
        );
        drop(other);
        assert_eq!(
            unsafe {
                x_access(
                    std::ptr::null_mut(),
                    name.as_ptr(),
                    ffi::SQLITE_ACCESS_EXISTS,
                    &mut exists,
                )
            },
            ffi::SQLITE_OK
        );
        drop(conn);
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
        assert_eq!(
            unsafe { x_delete(std::ptr::null_mut(), c"foreign.db".as_ptr(), 0) },
            ffi::SQLITE_IOERR_DELETE
        );
        drop(conn);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
