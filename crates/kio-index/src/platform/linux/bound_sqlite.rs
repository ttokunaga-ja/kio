use super::bound_sqlite::{BoundSqliteConnection, Hooks, header};
use crate::{IndexError, Result};
use rusqlite::{OpenFlags, ffi};
use std::{
    cell::Cell,
    collections::BTreeMap,
    ffi::{CStr, OsStr, c_char, c_int, c_void},
    fs::File,
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::PathBuf,
    ptr,
    sync::Mutex,
};

pub(super) struct OpenState {
    pub(super) expected: (u64, u64),
    pub(super) opening: Cell<bool>,
    pub(super) parent: File,
}
// Serializes descriptor observations only; never carries owner/current context.
static OPEN: Mutex<()> = Mutex::new(());
pub(super) fn internal_parent_path(value: &[u8]) -> bool {
    let Some(value) = value.strip_prefix(b"/proc/self/fd/") else {
        return false;
    };
    let Some(separator) = value.iter().position(|byte| *byte == b'/') else {
        return false;
    };
    let (fd, leaf) = value.split_at(separator);
    let leaf = &leaf[1..];
    !fd.is_empty()
        && fd.iter().all(u8::is_ascii_digit)
        && !leaf.is_empty()
        && !leaf.contains(&b'/')
        && leaf != b"."
        && leaf != b".."
}
fn internal_name(value: &[u8]) -> bool {
    internal_parent_path(value)
        || value
            .strip_prefix(b"/dev/fd/")
            .is_some_and(|fd| !fd.is_empty() && fd.iter().all(u8::is_ascii_digit))
}
fn regular_fd_inventory() -> Result<BTreeMap<i32, (u64, u64)>> {
    let entries = std::fs::read_dir("/proc/self/fd")
        .map_err(|e| IndexError::Schema(format!("inspect process descriptor inventory: {e}")))?;
    let mut result = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|e| {
            IndexError::Schema(format!("read process descriptor inventory entry: {e}"))
        })?;
        let Ok(fd) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Ok(metadata) = std::fs::metadata(entry.path()) else {
            continue;
        };
        if metadata.is_file() {
            result.insert(fd, (metadata.dev(), metadata.ino()));
        }
    }
    Ok(result)
}
struct NativeOpenGuard {
    file: *mut ffi::sqlite3_file,
    active: bool,
}
impl NativeOpenGuard {
    // SAFETY: file is valid writable storage for a successful native xOpen;
    // methods and backing allocation outlive this guard.
    unsafe fn new(file: *mut ffi::sqlite3_file) -> Self {
        Self { file, active: true }
    }
    fn disarm(&mut self) {
        self.active = false;
    }
}
impl Drop for NativeOpenGuard {
    fn drop(&mut self) {
        if !self.active || self.file.is_null() {
            return;
        }
        let methods = unsafe { (*self.file).pMethods };
        if !methods.is_null() {
            if let Some(close) = unsafe { (*methods).xClose } {
                let _ = unsafe { close(self.file) };
            }
            // SQLite must not close a successfully delegated but refused file again.
            unsafe {
                (*self.file).pMethods = ptr::null();
            }
        }
    }
}
pub(super) fn connection(
    parent: &File,
    leaf: &OsStr,
    expected: (u64, u64),
    flags: OpenFlags,
) -> Result<BoundSqliteConnection> {
    let state = OpenState {
        expected,
        opening: Cell::new(true),
        parent: parent
            .try_clone()
            .map_err(|e| IndexError::Schema(format!("retain bound SQLite parent: {e}")))?,
    };
    // The path names THIS owned descriptor, which survives BUSY retention.
    // Cloning a parent while naming the caller's original FD is insufficient.
    let path = PathBuf::from(format!("/proc/self/fd/{}", state.parent.as_raw_fd())).join(leaf);
    if !internal_parent_path(path.as_os_str().as_encoded_bytes()) {
        return Err(IndexError::Schema(
            "bound SQLite path is not an internal descriptor-root path".into(),
        ));
    }
    let hooks = Hooks {
        open,
        full_path,
        access: None,
        delete: None,
    };
    // Linux source/cache ALWAYS Some; failure never falls back to ambient open.
    let conn = BoundSqliteConnection::open(&path, flags, None, Some((state, hooks)))?;
    let vfs = conn
        .registered_vfs()
        .ok_or_else(|| IndexError::Schema("bound SQLite owner missing".into()))?;
    // SAFETY: own stable registration passed prefix checks; its exact state is
    // OpenState, allocated by the Some constructor above, and conn retains it.
    let owner = unsafe { header(vfs) }
        .ok_or_else(|| IndexError::Schema("bound SQLite owner invalid".into()))?;
    unsafe {
        (*owner.state.cast::<OpenState>()).opening.set(false);
    }
    // Error/unwind has no surviving connection owner; its state is torn down.
    Ok(conn)
}
unsafe fn open(
    state: *mut c_void,
    vfs: *mut ffi::sqlite3_vfs,
    name: ffi::sqlite3_filename,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    let Some(owner) = (unsafe { header(vfs) }) else {
        return ffi::SQLITE_CANTOPEN;
    };
    let original = owner.original;
    let Some(callback) = (unsafe { (*original).xOpen }) else {
        return ffi::SQLITE_CANTOPEN;
    };
    if !name.is_null()
        && internal_parent_path(unsafe { CStr::from_ptr(name).to_bytes() })
        && flags & ffi::SQLITE_OPEN_MAIN_DB != 0
    {
        if state.is_null() || file.is_null() {
            return ffi::SQLITE_CANTOPEN;
        }
        let state = unsafe { &*state.cast::<OpenState>() };
        if !state.opening.get() {
            return ffi::SQLITE_CANTOPEN;
        }
        let Ok(_guard) = OPEN.lock() else {
            return ffi::SQLITE_CANTOPEN;
        };
        let Ok(before) = regular_fd_inventory() else {
            return ffi::SQLITE_CANTOPEN;
        };
        let code = unsafe { callback(original, name, file, flags, out) };
        if code != ffi::SQLITE_OK {
            return code;
        }
        // Installed immediately after native success: verification panic or
        // refusal closes once before shared FFI catch returns a closed error.
        let mut native = unsafe { NativeOpenGuard::new(file) };
        let verified = regular_fd_inventory().is_ok_and(|after| {
            after
                .iter()
                .filter(|(fd, identity)| {
                    before.get(fd) != Some(*identity) && **identity == state.expected
                })
                .count()
                == 1
        });
        if !verified {
            return ffi::SQLITE_CANTOPEN;
        }
        native.disarm();
        return ffi::SQLITE_OK;
    }
    // Preserve noninternal MAIN and all non-main original delegation.
    unsafe { callback(original, name, file, flags, out) }
}
unsafe fn full_path(
    _: *mut c_void,
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    len: c_int,
    out: *mut c_char,
) -> c_int {
    if name.is_null() || out.is_null() || len <= 0 {
        return ffi::SQLITE_CANTOPEN;
    }
    let bytes = unsafe { CStr::from_ptr(name).to_bytes() };
    if internal_name(bytes) {
        if bytes.len() + 1 > len as usize {
            return ffi::SQLITE_CANTOPEN;
        }
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr().cast(), out, bytes.len());
            *out.add(bytes.len()) = 0;
        }
        return ffi::SQLITE_OK;
    }
    let Some(owner) = (unsafe { header(vfs) }) else {
        return ffi::SQLITE_CANTOPEN;
    };
    match unsafe { (*owner.original).xFullPathname } {
        Some(callback) => unsafe { callback(owner.original, name, len, out) },
        None => ffi::SQLITE_CANTOPEN,
    }
}

#[cfg(test)]
mod tests {
    // Tests-only source artifact. Production Linux helper is intentionally absent.
    use super::{NativeOpenGuard, OpenState, full_path, open, regular_fd_inventory};
    use crate::platform::bound_sqlite::tests as shared;
    use crate::platform::bound_sqlite::{Hooks, RegisteredVfs};
    use rusqlite::{Connection, OpenFlags, ffi};
    use std::{
        cell::Cell,
        ffi::{CString, c_int, c_void},
        fs::File,
        os::{fd::AsRawFd, unix::fs::MetadataExt},
        path::{Path, PathBuf},
        ptr,
        sync::{Arc, atomic::Ordering},
    };

    struct State {
        linux: OpenState,
        _backing: shared::State,
    }
    fn identity(file: &File) -> (u64, u64) {
        let m = file.metadata().unwrap();
        (m.dev(), m.ino())
    }
    unsafe fn linux_open(
        state: *mut c_void,
        vfs: *mut ffi::sqlite3_vfs,
        name: ffi::sqlite3_filename,
        file: *mut ffi::sqlite3_file,
        flags: c_int,
        out: *mut c_int,
    ) -> c_int {
        let state = unsafe { &mut *state.cast::<State>() };
        unsafe {
            open(
                ptr::addr_of_mut!(state.linux).cast(),
                vfs,
                name,
                file,
                flags,
                out,
            )
        }
    }
    unsafe fn linux_full(
        _: *mut c_void,
        vfs: *mut ffi::sqlite3_vfs,
        name: *const std::ffi::c_char,
        len: c_int,
        out: *mut std::ffi::c_char,
    ) -> c_int {
        unsafe { full_path(ptr::null_mut(), vfs, name, len, out) }
    }
    fn hooks() -> Hooks {
        Hooks {
            open: linux_open,
            full_path: linux_full,
            access: None,
            delete: None,
        }
    }
    fn seed(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=MEMORY; CREATE TABLE values_(v); INSERT INTO values_ VALUES(41);",
        )
        .unwrap();
    }
    struct Filename(ffi::sqlite3_filename);
    impl Filename {
        fn new(path: &Path) -> Self {
            use std::os::unix::ffi::OsStrExt;
            let path = CString::new(path.as_os_str().as_bytes()).unwrap();
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
            unsafe {
                ffi::sqlite3_free_filename(self.0);
            }
        }
    }
    fn internal_path(parent: &File, leaf: &str) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd())).join(leaf)
    }
    fn fixture(parent: &File, expected: (u64, u64)) -> (RegisteredVfs<State>, Arc<shared::Trace>) {
        let backing = shared::State::new(true);
        let original = backing.original_ptr();
        let trace = Arc::clone(&backing.trace);
        let state = State {
            linux: OpenState {
                expected,
                opening: Cell::new(true),
                parent: parent.try_clone().unwrap(),
            },
            _backing: backing,
        };
        let r = shared::registration(state, hooks(), original);
        shared::set_name(&trace, &shared::name(&r));
        (r, trace)
    }
    fn file_buffer(vfs: *mut ffi::sqlite3_vfs) -> Vec<u64> {
        let bytes = usize::try_from(unsafe { (*vfs).szOsFile }).unwrap();
        vec![0; bytes.div_ceil(std::mem::size_of::<u64>())]
    }
    fn invoke(vfs: *mut ffi::sqlite3_vfs, path: &Path, storage: &mut [u64], flags: c_int) -> c_int {
        let name = Filename::new(path);
        let file = storage.as_mut_ptr().cast();
        let mut out = 0;
        unsafe { (*vfs).xOpen.unwrap()(vfs, name.0, file, flags, &mut out) }
    }
    fn main_flags() -> c_int {
        ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_MAIN_DB
    }
    fn refused_and_closed(vfs: *mut ffi::sqlite3_vfs, path: &Path, trace: &shared::Trace) {
        let mut storage = file_buffer(vfs);
        let file = storage.as_mut_ptr().cast::<ffi::sqlite3_file>();
        assert_ne!(
            invoke(vfs, path, &mut storage, main_flags()),
            ffi::SQLITE_OK
        );
        assert_eq!(trace.closed.load(Ordering::SeqCst), 1);
        assert!(unsafe { (*file).pMethods }.is_null());
    }

    #[test]
    fn source_and_cache_parallel_owners_use_native_identity() {
        for cache_first in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let source_path = directory.path().join("source.sqlite");
            let cache_path = directory.path().join("cache.sqlite");
            seed(&source_path);
            seed(&cache_path);
            let source = crate::platform::BoundSourceIndex {
                _root: File::open(directory.path()).unwrap(),
                _parent: File::open(directory.path()).unwrap(),
                file: File::open(&source_path).unwrap(),
                public_path: source_path,
            };
            let cache = crate::platform::BoundCache {
                _parent: File::open(directory.path()).unwrap(),
                file: File::open(&cache_path).unwrap(),
                public_path: cache_path,
            };
            let source_identity = identity(&source.file);
            let cache_identity = identity(cache.file());
            let before = regular_fd_inventory().unwrap();
            let source_conn = crate::platform::open_bound_source_connection(
                &source,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )
            .unwrap();
            let cache_conn = crate::platform::open_bound_cache_connection(
                &cache,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )
            .unwrap();
            let after = regular_fd_inventory().unwrap();
            for expected in [source_identity, cache_identity] {
                assert_eq!(
                    after
                        .iter()
                        .filter(|(fd, id)| before.get(fd) != Some(*id) && **id == expected)
                        .count(),
                    1
                );
            }
            // Test-only accessor uses the owner-held pointer; no cast of erased registration storage.
            let source_vfs = source_conn.registered_vfs_for_test().unwrap();
            let cache_vfs = cache_conn.registered_vfs_for_test().unwrap();
            assert_ne!(source_vfs, cache_vfs);
            let source_name = unsafe { std::ffi::CStr::from_ptr((*source_vfs).zName) }.to_owned();
            let cache_name = unsafe { std::ffi::CStr::from_ptr((*cache_vfs).zName) }.to_owned();
            assert_ne!(source_name, cache_name);
            source_conn
                .execute_batch("PRAGMA journal_mode=MEMORY; PRAGMA temp_store=MEMORY;")
                .unwrap();
            cache_conn
                .execute_batch("PRAGMA journal_mode=MEMORY; PRAGMA temp_store=MEMORY;")
                .unwrap();
            source_conn.execute("UPDATE values_ SET v=42", []).unwrap();
            cache_conn.execute("UPDATE values_ SET v=43", []).unwrap();
            if cache_first {
                drop(cache_conn);
                assert!(unsafe { ffi::sqlite3_vfs_find(cache_name.as_ptr()) }.is_null());
                assert_eq!(
                    source_conn
                        .query_row("SELECT v FROM values_", [], |r| r.get::<_, i64>(0))
                        .unwrap(),
                    42
                );
                drop(source_conn);
            } else {
                drop(source_conn);
                assert!(unsafe { ffi::sqlite3_vfs_find(source_name.as_ptr()) }.is_null());
                assert_eq!(
                    cache_conn
                        .query_row("SELECT v FROM values_", [], |r| r.get::<_, i64>(0))
                        .unwrap(),
                    43
                );
                drop(cache_conn);
            }
            assert!(unsafe { ffi::sqlite3_vfs_find(source_name.as_ptr()) }.is_null());
            assert!(unsafe { ffi::sqlite3_vfs_find(cache_name.as_ptr()) }.is_null());
        }
    }
    #[test]
    fn wrong_native_identity_is_refused_and_closed_once() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a.sqlite");
        let b = d.path().join("b.sqlite");
        seed(&a);
        seed(&b);
        let retained = File::open(&a).unwrap();
        let parent = File::open(d.path()).unwrap();
        let (r, trace) = fixture(&parent, identity(&retained));
        refused_and_closed(shared::vfs(&r), &internal_path(&parent, "b.sqlite"), &trace);
    }
    #[test]
    fn two_new_matching_native_fds_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("a.sqlite");
        seed(&path);
        let retained = File::open(&path).unwrap();
        let parent = File::open(d.path()).unwrap();
        let (r, trace) = fixture(&parent, identity(&retained));
        *trace.extra_path.lock().unwrap() = Some(path);
        refused_and_closed(shared::vfs(&r), &internal_path(&parent, "a.sqlite"), &trace);
        assert!(trace.extra_file.lock().unwrap().is_some());
    }
    #[test]
    fn internal_main_authority_ends_after_open() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("a.sqlite");
        seed(&path);
        let source = crate::platform::BoundSourceIndex {
            _root: File::open(d.path()).unwrap(),
            _parent: File::open(d.path()).unwrap(),
            file: File::open(&path).unwrap(),
            public_path: path.clone(),
        };
        let conn = crate::platform::open_bound_source_connection(
            &source,
            OpenFlags::SQLITE_OPEN_READ_WRITE,
        )
        .unwrap();
        // Test-only accessor on this explicit connection owner, never global lookup.
        let vfs = conn.registered_vfs_for_test().unwrap();
        let mut storage = file_buffer(vfs);
        assert_ne!(
            invoke(
                vfs,
                &internal_path(&source._parent, "a.sqlite"),
                &mut storage,
                main_flags()
            ),
            ffi::SQLITE_OK
        );
        assert!(unsafe { (*storage.as_ptr().cast::<ffi::sqlite3_file>()).pMethods }.is_null());
        // Existing noninternal MAIN delegation remains usable after designated open.
        let ordinary = d.path().join("ordinary.sqlite");
        seed(&ordinary);
        let mut delegated = file_buffer(vfs);
        assert_eq!(
            invoke(vfs, &ordinary, &mut delegated, main_flags()),
            ffi::SQLITE_OK
        );
        let file = delegated.as_mut_ptr().cast::<ffi::sqlite3_file>();
        unsafe {
            assert_eq!((*(*file).pMethods).xClose.unwrap()(file), ffi::SQLITE_OK);
        }
        let temp = d.path().join("temp.sqlite");
        let mut nonmain = file_buffer(vfs);
        assert_eq!(
            invoke(
                vfs,
                &temp,
                &mut nonmain,
                ffi::SQLITE_OPEN_READWRITE
                    | ffi::SQLITE_OPEN_CREATE
                    | ffi::SQLITE_OPEN_TEMP_DB
                    | ffi::SQLITE_OPEN_DELETEONCLOSE
            ),
            ffi::SQLITE_OK
        );
        let file = nonmain.as_mut_ptr().cast::<ffi::sqlite3_file>();
        unsafe {
            assert_eq!((*(*file).pMethods).xClose.unwrap()(file), ffi::SQLITE_OK);
        }
        assert_eq!(
            conn.query_row("SELECT v FROM values_", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            41
        );
        // Failure/unwind paths have no returned owner; shared lifecycle tests prove registration removal.
    }
    unsafe fn native_then_unwind(
        _: *mut c_void,
        vfs: *mut ffi::sqlite3_vfs,
        name: ffi::sqlite3_filename,
        file: *mut ffi::sqlite3_file,
        flags: c_int,
        out: *mut c_int,
    ) -> c_int {
        let rc = unsafe { shared::delegate_open(ptr::null_mut(), vfs, name, file, flags, out) };
        if rc != ffi::SQLITE_OK {
            return rc;
        }
        // Actual guard type under test. No original C callback panics.
        let _guard = unsafe { NativeOpenGuard::new(file) };
        panic!("Rust unwind after native success");
    }
    #[test]
    fn native_open_unwind_closes_once_and_clears_methods() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("a.sqlite");
        seed(&path);
        let parent = File::open(d.path()).unwrap();
        let retained = File::open(&path).unwrap();
        let backing = shared::State::new(true);
        let original = backing.original_ptr();
        let trace = Arc::clone(&backing.trace);
        let state = State {
            linux: OpenState {
                expected: identity(&retained),
                opening: Cell::new(true),
                parent: parent.try_clone().unwrap(),
            },
            _backing: backing,
        };
        let mut h = hooks();
        h.open = native_then_unwind;
        let r = shared::registration(state, h, original);
        shared::set_name(&trace, &shared::name(&r));
        refused_and_closed(shared::vfs(&r), &internal_path(&parent, "a.sqlite"), &trace);
    }

    #[test]
    fn forgotten_statement_retains_descriptor_parent_after_outer_drop() {
        use std::ffi::CStr;
        for is_cache in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("busy.sqlite");
            seed(&path);
            let source = (!is_cache).then(|| crate::platform::BoundSourceIndex {
                _root: File::open(directory.path()).unwrap(),
                _parent: File::open(directory.path()).unwrap(),
                file: File::open(&path).unwrap(),
                public_path: path.clone(),
            });
            let cache = is_cache.then(|| crate::platform::BoundCache {
                _parent: File::open(directory.path()).unwrap(),
                file: File::open(&path).unwrap(),
                public_path: path.clone(),
            });
            let (conn, outer_parent) = if let Some(source) = source.as_ref() {
                (
                    crate::platform::open_bound_source_connection(
                        source,
                        OpenFlags::SQLITE_OPEN_READ_WRITE,
                    )
                    .unwrap(),
                    &source._parent,
                )
            } else {
                let cache = cache.as_ref().unwrap();
                (
                    crate::platform::open_bound_cache_connection(
                        cache,
                        OpenFlags::SQLITE_OPEN_READ_WRITE,
                    )
                    .unwrap(),
                    &cache._parent,
                )
            };
            let vfs = conn.registered_vfs_for_test().unwrap();
            let name = unsafe { CStr::from_ptr((*vfs).zName) }.to_owned();
            // Read SQLite's actual path while this explicit connection is live.
            // Its descriptor must be owned independently of the outer capability.
            let filename = conn.path().unwrap().to_owned();
            let (descriptor, leaf) = filename
                .strip_prefix("/proc/self/fd/")
                .unwrap()
                .split_once('/')
                .unwrap();
            assert_eq!(leaf, "busy.sqlite");
            let parent_fd: i32 = descriptor.parse().unwrap();
            assert_ne!(parent_fd, outer_parent.as_raw_fd());
            let expected_parent = identity(outer_parent);
            let parent_path = PathBuf::from(format!("/proc/self/fd/{parent_fd}"));
            let parent = std::fs::metadata(&parent_path).unwrap();
            assert!(parent.is_dir());
            assert_eq!((parent.dev(), parent.ino()), expected_parent);
            let expected_file = identity(&File::open(&path).unwrap());
            let sqlite_path = PathBuf::from(filename);
            let statement = conn.prepare("SELECT v FROM values_").unwrap();
            std::mem::forget(statement);
            drop(conn);
            drop(source);
            drop(cache);
            // Accepted BUSY retention keeps this test directory and native owner
            // for process lifetime. Do not sweep statements or reclaim the leak.
            let _retained_directory = directory.keep();
            assert_eq!(unsafe { ffi::sqlite3_vfs_find(name.as_ptr()) }, vfs);
            // Only stat recorded paths after drop. Never dereference old state or
            // a forgotten statement/Connection to test the retention guarantee.
            let parent = std::fs::metadata(&parent_path).unwrap();
            assert!(parent.is_dir());
            assert_eq!((parent.dev(), parent.ino()), expected_parent);
            let main = std::fs::metadata(&sqlite_path).unwrap();
            assert_eq!((main.dev(), main.ino()), expected_file);
        }
    }
}
