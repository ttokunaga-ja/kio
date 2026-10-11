// Tests-only source artifact; expected private owner API is listed in requirements.md.
use super::{BoundSqliteConnection, Hooks, RegisteredVfs, header};
use rusqlite::{OpenFlags, ffi};
use std::{
    cell::UnsafeCell,
    ffi::{CStr, CString, c_char, c_int, c_void},
    path::Path,
    ptr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    },
};

#[derive(Default)]
pub(crate) struct Trace {
    pub(crate) mask: AtomicU32,
    original: AtomicUsize,
    userdata: AtomicUsize,
    native: AtomicUsize,
    pub(crate) closed: AtomicUsize,
    pub(crate) dropped: AtomicUsize,
    pub(crate) close_registered: AtomicBool,
    pub(crate) drop_unregistered: AtomicBool,
    name: Mutex<Option<CString>>,
    methods: Mutex<Vec<std::pin::Pin<Box<CloseMethods>>>>,
    pub(crate) extra_path: Mutex<Option<std::path::PathBuf>>,
    pub(crate) extra_file: Mutex<Option<std::fs::File>>,
}
#[repr(C)]
struct CloseMethods {
    methods: ffi::sqlite3_io_methods,
    original: *const ffi::sqlite3_io_methods,
    trace: usize,
}
// Each methods table is used sequentially by its one movable Connection, not shared.
unsafe impl Send for CloseMethods {}

unsafe extern "C" fn close_file(file: *mut ffi::sqlite3_file) -> c_int {
    if file.is_null() {
        return ffi::SQLITE_IOERR_CLOSE;
    }
    let methods = unsafe { (*file).pMethods };
    if methods.is_null() {
        return ffi::SQLITE_IOERR_CLOSE;
    }
    let shim = unsafe { &*methods.cast::<CloseMethods>() };
    let trace = unsafe { &*(shim.trace as *const Trace) };
    trace.closed.fetch_add(1, Ordering::SeqCst);
    if let Ok(name) = trace.name.lock()
        && let Some(name) = name.as_ref()
    {
        trace.close_registered.store(
            !unsafe { ffi::sqlite3_vfs_find(name.as_ptr()) }.is_null(),
            Ordering::SeqCst,
        );
    }
    let original = shim.original;
    unsafe {
        (*file).pMethods = original;
    }
    match unsafe { (*original).xClose } {
        Some(close) => unsafe { close(file) },
        None => ffi::SQLITE_IOERR_CLOSE,
    }
}

// Do not cast wrong-owner userdata as Trace: original xOpen is an independent
// fixture marker. Bad delegation produces missing observations, never UB/panic.
unsafe fn observe(vfs: *mut ffi::sqlite3_vfs, slot: u32) -> Option<&'static Trace> {
    if vfs.is_null() {
        return None;
    }
    let open = unsafe { (*vfs).xOpen }?;
    if !ptr::fn_addr_eq(
        open,
        original_open as unsafe extern "C" fn(_, _, _, _, _) -> _,
    ) {
        return None;
    }
    let data = unsafe { (*vfs).pAppData };
    if data.is_null() {
        return None;
    }
    let trace = unsafe { &*data.cast::<Trace>() };
    if trace.original.load(Ordering::SeqCst) != vfs as usize {
        return None;
    }
    trace.userdata.store(data as usize, Ordering::SeqCst);
    trace.mask.fetch_or(1 << slot, Ordering::SeqCst);
    Some(trace)
}
unsafe fn native(trace: &Trace) -> *mut ffi::sqlite3_vfs {
    trace.native.load(Ordering::SeqCst) as *mut _
}
unsafe extern "C" fn original_open(
    vfs: *mut ffi::sqlite3_vfs,
    name: ffi::sqlite3_filename,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    let Some(trace) = (unsafe { observe(vfs, 0) }) else {
        return ffi::SQLITE_CANTOPEN;
    };
    let n = unsafe { native(trace) };
    if n.is_null() {
        if !out.is_null() {
            unsafe {
                *out = flags;
            }
        }
        return ffi::SQLITE_OK;
    }
    let Some(open) = (unsafe { (*n).xOpen }) else {
        return ffi::SQLITE_CANTOPEN;
    };
    let rc = unsafe { open(n, name, file, flags, out) };
    if rc != ffi::SQLITE_OK || file.is_null() {
        return rc;
    }
    let original = unsafe { (*file).pMethods };
    if original.is_null() {
        return ffi::SQLITE_IOERR;
    }
    let mut methods = Box::new(CloseMethods {
        methods: unsafe { ptr::read(original) },
        original,
        trace: trace as *const Trace as usize,
    });
    methods.methods.xClose = Some(close_file);
    let address = ptr::addr_of!(methods.methods);
    let Ok(mut owned) = trace.methods.lock() else {
        return ffi::SQLITE_IOERR;
    };
    owned.push(Box::into_pin(methods));
    unsafe {
        (*file).pMethods = address;
    }
    if let Ok(path) = trace.extra_path.lock()
        && let Some(path) = path.as_ref()
        && let Ok(file) = std::fs::File::open(path)
        && let Ok(mut extra) = trace.extra_file.lock()
    {
        *extra = Some(file);
    }
    rc
}
unsafe extern "C" fn original_delete(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    sync: c_int,
) -> c_int {
    let Some(t) = (unsafe { observe(vfs, 1) }) else {
        return ffi::SQLITE_IOERR_DELETE;
    };
    let n = unsafe { native(t) };
    if n.is_null() {
        return 101;
    }
    match unsafe { (*n).xDelete } {
        Some(f) => unsafe { f(n, name, sync) },
        None => ffi::SQLITE_IOERR_DELETE,
    }
}
unsafe extern "C" fn original_access(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    let Some(t) = (unsafe { observe(vfs, 2) }) else {
        return ffi::SQLITE_IOERR_ACCESS;
    };
    let n = unsafe { native(t) };
    if n.is_null() {
        if !out.is_null() {
            unsafe {
                *out = flags + 7;
            }
        }
        return 102;
    }
    match unsafe { (*n).xAccess } {
        Some(f) => unsafe { f(n, name, flags, out) },
        None => ffi::SQLITE_IOERR_ACCESS,
    }
}
unsafe extern "C" fn original_full(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    len: c_int,
    out: *mut c_char,
) -> c_int {
    let Some(t) = (unsafe { observe(vfs, 3) }) else {
        return ffi::SQLITE_CANTOPEN;
    };
    let n = unsafe { native(t) };
    if n.is_null() {
        if !out.is_null() && len >= 2 {
            unsafe {
                *out = b'f' as c_char;
                *out.add(1) = 0;
            }
        }
        return 103;
    }
    match unsafe { (*n).xFullPathname } {
        Some(f) => unsafe { f(n, name, len, out) },
        None => ffi::SQLITE_CANTOPEN,
    }
}
unsafe extern "C" fn symbol(_: *mut ffi::sqlite3_vfs, _: *mut c_void, _: *const c_char) {}
unsafe extern "C" fn syscall() {}
unsafe extern "C" fn original_dl_open(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
) -> *mut c_void {
    let Some(t) = (unsafe { observe(vfs, 4) }) else {
        return ptr::null_mut();
    };
    let n = unsafe { native(t) };
    if n.is_null() {
        return 17usize as *mut c_void;
    }
    match unsafe { (*n).xDlOpen } {
        Some(f) => unsafe { f(n, name) },
        None => ptr::null_mut(),
    }
}
unsafe extern "C" fn original_dl_error(vfs: *mut ffi::sqlite3_vfs, len: c_int, out: *mut c_char) {
    let Some(t) = (unsafe { observe(vfs, 5) }) else {
        return;
    };
    let n = unsafe { native(t) };
    if n.is_null() {
        if !out.is_null() && len > 0 {
            unsafe {
                *out = 0;
            }
        }
        return;
    }
    if let Some(f) = unsafe { (*n).xDlError } {
        unsafe { f(n, len, out) }
    }
}
unsafe extern "C" fn original_dl_sym(
    vfs: *mut ffi::sqlite3_vfs,
    handle: *mut c_void,
    name: *const c_char,
) -> Option<unsafe extern "C" fn(*mut ffi::sqlite3_vfs, *mut c_void, *const c_char)> {
    let t = (unsafe { observe(vfs, 6) })?;
    let n = unsafe { native(t) };
    if n.is_null() {
        return Some(symbol);
    }
    match unsafe { (*n).xDlSym } {
        Some(f) => unsafe { f(n, handle, name) },
        None => None,
    }
}
unsafe extern "C" fn original_dl_close(vfs: *mut ffi::sqlite3_vfs, handle: *mut c_void) {
    let Some(t) = (unsafe { observe(vfs, 7) }) else {
        return;
    };
    let n = unsafe { native(t) };
    if n.is_null() {
        return;
    }
    if let Some(f) = unsafe { (*n).xDlClose } {
        unsafe { f(n, handle) }
    }
}
unsafe extern "C" fn original_random(
    vfs: *mut ffi::sqlite3_vfs,
    len: c_int,
    out: *mut c_char,
) -> c_int {
    let Some(t) = (unsafe { observe(vfs, 8) }) else {
        return 0;
    };
    let n = unsafe { native(t) };
    if n.is_null() {
        if !out.is_null() && len > 0 {
            unsafe {
                *out = 42;
            }
        }
        return len;
    }
    match unsafe { (*n).xRandomness } {
        Some(f) => unsafe { f(n, len, out) },
        None => 0,
    }
}
unsafe extern "C" fn original_sleep(vfs: *mut ffi::sqlite3_vfs, duration: c_int) -> c_int {
    let Some(t) = (unsafe { observe(vfs, 9) }) else {
        return 0;
    };
    let n = unsafe { native(t) };
    if n.is_null() {
        return duration;
    }
    match unsafe { (*n).xSleep } {
        Some(f) => unsafe { f(n, duration) },
        None => 0,
    }
}
unsafe extern "C" fn original_time(vfs: *mut ffi::sqlite3_vfs, out: *mut f64) -> c_int {
    let Some(t) = (unsafe { observe(vfs, 10) }) else {
        return ffi::SQLITE_ERROR;
    };
    let n = unsafe { native(t) };
    if n.is_null() {
        if !out.is_null() {
            unsafe {
                *out = 123.5;
            }
        }
        return 110;
    }
    match unsafe { (*n).xCurrentTime } {
        Some(f) => unsafe { f(n, out) },
        None => ffi::SQLITE_ERROR,
    }
}
unsafe extern "C" fn original_last(
    vfs: *mut ffi::sqlite3_vfs,
    len: c_int,
    out: *mut c_char,
) -> c_int {
    let Some(t) = (unsafe { observe(vfs, 11) }) else {
        return 0;
    };
    let n = unsafe { native(t) };
    if n.is_null() {
        if !out.is_null() && len > 0 {
            unsafe {
                *out = 0;
            }
        }
        return 111;
    }
    match unsafe { (*n).xGetLastError } {
        Some(f) => unsafe { f(n, len, out) },
        None => 0,
    }
}
unsafe extern "C" fn original_time64(
    vfs: *mut ffi::sqlite3_vfs,
    out: *mut ffi::sqlite3_int64,
) -> c_int {
    let Some(t) = (unsafe { observe(vfs, 12) }) else {
        return ffi::SQLITE_ERROR;
    };
    let n = unsafe { native(t) };
    if n.is_null() {
        if !out.is_null() {
            unsafe {
                *out = 1234567;
            }
        }
        return 112;
    }
    match unsafe { (*n).xCurrentTimeInt64 } {
        Some(f) => unsafe { f(n, out) },
        None => ffi::SQLITE_ERROR,
    }
}
unsafe extern "C" fn original_set(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    pointer: ffi::sqlite3_syscall_ptr,
) -> c_int {
    let Some(t) = (unsafe { observe(vfs, 13) }) else {
        return ffi::SQLITE_NOTFOUND;
    };
    let n = unsafe { native(t) };
    if n.is_null() {
        return 113;
    }
    match unsafe { (*n).xSetSystemCall } {
        Some(f) => unsafe { f(n, name, pointer) },
        None => ffi::SQLITE_NOTFOUND,
    }
}
unsafe extern "C" fn original_get(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
) -> ffi::sqlite3_syscall_ptr {
    let t = (unsafe { observe(vfs, 14) })?;
    let n = unsafe { native(t) };
    if n.is_null() {
        return Some(syscall);
    }
    match unsafe { (*n).xGetSystemCall } {
        Some(f) => unsafe { f(n, name) },
        None => None,
    }
}
unsafe extern "C" fn original_next(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
) -> *const c_char {
    let Some(t) = (unsafe { observe(vfs, 15) }) else {
        return ptr::null();
    };
    let n = unsafe { native(t) };
    if n.is_null() {
        return c"next".as_ptr();
    }
    match unsafe { (*n).xNextSystemCall } {
        Some(f) => unsafe { f(n, name) },
        None => ptr::null(),
    }
}

pub(crate) struct Original {
    vfs: UnsafeCell<ffi::sqlite3_vfs>,
}
// Only moved with its sequential owner; allocation and immutable callbacks stay stable.
unsafe impl Send for Original {}
pub(crate) struct State {
    pub(crate) original: Box<Original>,
    pub(crate) trace: Arc<Trace>,
}
impl Drop for State {
    fn drop(&mut self) {
        self.trace.dropped.fetch_add(1, Ordering::SeqCst);
        if let Ok(name) = self.trace.name.lock()
            && let Some(name) = name.as_ref()
        {
            self.trace.drop_unregistered.store(
                unsafe { ffi::sqlite3_vfs_find(name.as_ptr()) }.is_null(),
                Ordering::SeqCst,
            );
        }
    }
}
impl State {
    pub(crate) fn new(use_native: bool) -> Self {
        let trace = Arc::new(Trace::default());
        let native = unsafe { ffi::sqlite3_vfs_find(ptr::null()) };
        assert!(!native.is_null());
        if use_native {
            trace.native.store(native as usize, Ordering::SeqCst);
        }
        let original = Box::new(Original {
            vfs: UnsafeCell::new(ffi::sqlite3_vfs {
                iVersion: 3,
                szOsFile: unsafe { (*native).szOsFile },
                mxPathname: unsafe { (*native).mxPathname },
                pNext: ptr::null_mut(),
                zName: c"test-explicit-original".as_ptr(),
                pAppData: Arc::as_ptr(&trace).cast_mut().cast(),
                xOpen: Some(original_open),
                xDelete: Some(original_delete),
                xAccess: Some(original_access),
                xFullPathname: Some(original_full),
                xDlOpen: Some(original_dl_open),
                xDlError: Some(original_dl_error),
                xDlSym: Some(original_dl_sym),
                xDlClose: Some(original_dl_close),
                xRandomness: Some(original_random),
                xSleep: Some(original_sleep),
                xCurrentTime: Some(original_time),
                xGetLastError: Some(original_last),
                xCurrentTimeInt64: Some(original_time64),
                xSetSystemCall: Some(original_set),
                xGetSystemCall: Some(original_get),
                xNextSystemCall: Some(original_next),
            }),
        });
        trace
            .original
            .store(original.vfs.get() as usize, Ordering::SeqCst);
        Self { original, trace }
    }
    pub(crate) fn original_ptr(&self) -> *mut ffi::sqlite3_vfs {
        self.original.vfs.get()
    }
}

pub(crate) unsafe fn delegate_open(
    _: *mut c_void,
    vfs: *mut ffi::sqlite3_vfs,
    name: ffi::sqlite3_filename,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    let Some(h) = (unsafe { header(vfs) }) else {
        return ffi::SQLITE_CANTOPEN;
    };
    let original = h.original;
    match unsafe { (*original).xOpen } {
        Some(f) => unsafe { f(original, name, file, flags, out) },
        None => ffi::SQLITE_CANTOPEN,
    }
}
pub(crate) unsafe fn delegate_full(
    _: *mut c_void,
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    len: c_int,
    out: *mut c_char,
) -> c_int {
    let Some(h) = (unsafe { header(vfs) }) else {
        return ffi::SQLITE_CANTOPEN;
    };
    let original = h.original;
    match unsafe { (*original).xFullPathname } {
        Some(f) => unsafe { f(original, name, len, out) },
        None => ffi::SQLITE_CANTOPEN,
    }
}
pub(crate) fn hooks() -> Hooks {
    Hooks {
        open: delegate_open,
        full_path: delegate_full,
        access: None,
        delete: None,
    }
}
pub(crate) fn registration<S: Send + 'static>(
    state: S,
    hooks: Hooks,
    original: *mut ffi::sqlite3_vfs,
) -> RegisteredVfs<S> {
    // SAFETY: each test S owns the stable original backing, which survives registration and SQLite close.
    unsafe { RegisteredVfs::new(state, hooks, original) }.unwrap()
}
pub(crate) fn vfs<S>(r: &RegisteredVfs<S>) -> *mut ffi::sqlite3_vfs {
    r.owner.as_ref().unwrap().header.vfs.get()
}
pub(crate) fn name<S>(r: &RegisteredVfs<S>) -> CString {
    r.owner.as_ref().unwrap().name.clone()
}
pub(crate) fn set_name(trace: &Trace, name: &CStr) {
    *trace.name.lock().unwrap() = Some(name.to_owned());
}
pub(crate) fn open_owned<S: Send + 'static>(
    r: RegisteredVfs<S>,
    path: &Path,
) -> crate::Result<BoundSqliteConnection> {
    BoundSqliteConnection::open_registered(
        r,
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )
}
fn fixture() -> (
    tempfile::TempDir,
    BoundSqliteConnection,
    CString,
    Arc<Trace>,
) {
    let directory = tempfile::tempdir().unwrap();
    let state = State::new(true);
    let trace = Arc::clone(&state.trace);
    let original = state.original_ptr();
    let r = registration(state, hooks(), original);
    let name = self::name(&r);
    set_name(&trace, &name);
    let conn = open_owned(r, &directory.path().join("test.sqlite")).unwrap();
    conn.execute_batch("PRAGMA journal_mode=MEMORY; PRAGMA temp_store=MEMORY; CREATE TABLE values_(v); INSERT INTO values_ VALUES(41);").unwrap();
    (directory, conn, name, trace)
}
fn cleaned(name: &CStr, trace: &Trace) {
    assert!(unsafe { ffi::sqlite3_vfs_find(name.as_ptr()) }.is_null());
    assert_eq!(trace.closed.load(Ordering::SeqCst), 1);
    assert_eq!(trace.dropped.load(Ordering::SeqCst), 1);
    assert!(trace.close_registered.load(Ordering::SeqCst));
    assert!(trace.drop_unregistered.load(Ordering::SeqCst));
}

#[test]
fn all_vfs_callbacks_delegate_original_pointer() {
    let state = State::new(false);
    let trace = Arc::clone(&state.trace);
    let original = state.original_ptr();
    let userdata = unsafe { (*original).pAppData };
    let r = registration(state, hooks(), original);
    let v = vfs(&r);
    let mut file = ffi::sqlite3_file {
        pMethods: ptr::null(),
    };
    let mut value = 0;
    let mut buf = [0 as c_char; 16];
    unsafe {
        assert_eq!(
            (*v).xOpen.unwrap()(v, c"name".as_ptr(), &mut file, 7, &mut value),
            ffi::SQLITE_OK
        );
        assert_eq!(value, 7);
        assert_eq!((*v).xDelete.unwrap()(v, c"name".as_ptr(), 0), 101);
        assert_eq!(
            (*v).xAccess.unwrap()(v, c"name".as_ptr(), 2, &mut value),
            102
        );
        assert_eq!(value, 9);
        assert_eq!(
            (*v).xFullPathname.unwrap()(v, c"name".as_ptr(), 16, buf.as_mut_ptr()),
            103
        );
        assert_eq!(buf[0], b'f' as c_char);
        assert_eq!(
            (*v).xDlOpen.unwrap()(v, c"name".as_ptr()),
            17usize as *mut c_void
        );
        (*v).xDlError.unwrap()(v, 16, buf.as_mut_ptr());
        let sym = (*v).xDlSym.unwrap()(v, ptr::null_mut(), c"name".as_ptr()).unwrap();
        assert!(ptr::fn_addr_eq(
            sym,
            symbol as unsafe extern "C" fn(_, _, _)
        ));
        (*v).xDlClose.unwrap()(v, ptr::null_mut());
        assert_eq!((*v).xRandomness.unwrap()(v, 16, buf.as_mut_ptr()), 16);
        assert_eq!(buf[0], 42);
        assert_eq!((*v).xSleep.unwrap()(v, 11), 11);
        let mut time = 0.;
        assert_eq!((*v).xCurrentTime.unwrap()(v, &mut time), 110);
        assert_eq!(time, 123.5);
        assert_eq!((*v).xGetLastError.unwrap()(v, 16, buf.as_mut_ptr()), 111);
        let mut time64 = 0;
        assert_eq!((*v).xCurrentTimeInt64.unwrap()(v, &mut time64), 112);
        assert_eq!(time64, 1234567);
        assert_eq!(
            (*v).xSetSystemCall.unwrap()(v, c"name".as_ptr(), Some(syscall)),
            113
        );
        assert!(ptr::fn_addr_eq(
            (*v).xGetSystemCall.unwrap()(v, c"name".as_ptr()).unwrap(),
            syscall as unsafe extern "C" fn()
        ));
        assert_eq!(
            CStr::from_ptr((*v).xNextSystemCall.unwrap()(v, c"name".as_ptr())),
            c"next"
        );
        assert_eq!((*original).pAppData, userdata);
    }
    assert_eq!(trace.userdata.load(Ordering::SeqCst), userdata as usize);
    assert_eq!(trace.mask.load(Ordering::SeqCst), 0xffff);
}

#[test]
fn optional_vfs_callbacks_preserve_absence() {
    for version in 1..=3 {
        let state = State::new(false);
        let original = state.original_ptr();
        unsafe {
            (*original).iVersion = version;
            (*original).xDlOpen = None;
            (*original).xDlError = None;
            (*original).xDlSym = None;
            (*original).xDlClose = None;
            (*original).xRandomness = None;
            (*original).xSleep = None;
            (*original).xCurrentTime = None;
            (*original).xGetLastError = None;
            (*original).xCurrentTimeInt64 = None;
            (*original).xSetSystemCall = None;
            (*original).xGetSystemCall = None;
            (*original).xNextSystemCall = None;
        }
        let r = registration(state, hooks(), original);
        let v = vfs(&r);
        unsafe {
            assert_eq!((*v).iVersion, version);
            assert!((*v).xDlOpen.is_none());
            assert!((*v).xDlError.is_none());
            assert!((*v).xDlSym.is_none());
            assert!((*v).xDlClose.is_none());
            assert!((*v).xRandomness.is_none());
            assert!((*v).xSleep.is_none());
            assert!((*v).xCurrentTime.is_none());
            assert!((*v).xGetLastError.is_none());
            assert!((*v).xCurrentTimeInt64.is_none());
            assert!((*v).xSetSystemCall.is_none());
            assert!((*v).xGetSystemCall.is_none());
            assert!((*v).xNextSystemCall.is_none());
        }
    }
}

#[test]
fn invalid_owner_vfs_callbacks_fail_closed() {
    let state = State::new(false);
    let original = state.original_ptr();
    let trace = Arc::clone(&state.trace);
    let r = registration(state, hooks(), original);
    let valid = vfs(&r);
    // Only relevant individual immutable header fields copied; do not read pNext.
    let mut copied: ffi::sqlite3_vfs = unsafe { std::mem::zeroed() };
    unsafe {
        copied.pAppData = (*valid).pAppData;
        copied.xOpen = (*valid).xOpen;
    }
    let default = unsafe { ffi::sqlite3_vfs_find(ptr::null()) };
    assert!(!default.is_null());
    for wrong in [ptr::null_mut(), default, ptr::addr_of_mut!(copied)] {
        let mut file = ffi::sqlite3_file {
            pMethods: ptr::null(),
        };
        let mut out = 0;
        let mut buf = [0 as c_char; 32];
        unsafe {
            assert_ne!(
                (*valid).xOpen.unwrap()(
                    wrong,
                    c"name".as_ptr(),
                    &mut file,
                    ffi::SQLITE_OPEN_MAIN_DB,
                    &mut out
                ),
                ffi::SQLITE_OK
            );
            assert_ne!(
                (*valid).xAccess.unwrap()(
                    wrong,
                    c"name".as_ptr(),
                    ffi::SQLITE_ACCESS_EXISTS,
                    &mut out
                ),
                ffi::SQLITE_OK
            );
            assert_ne!(
                (*valid).xFullPathname.unwrap()(wrong, c"name".as_ptr(), 32, buf.as_mut_ptr()),
                ffi::SQLITE_OK
            );
        }
    }
    assert_eq!(trace.mask.load(Ordering::SeqCst), 0);
}

#[test]
fn concurrent_owners_have_private_unique_names() {
    let (_a, a, an, at) = fixture();
    let (_b, b, bn, bt) = fixture();
    assert_ne!(an, bn);
    let av = a.registered_vfs_for_test().unwrap();
    let bv = b.registered_vfs_for_test().unwrap();
    assert_ne!(av, bv);
    drop(a);
    cleaned(&an, &at);
    assert_eq!(
        b.query_row("SELECT v FROM values_", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        41
    );
    assert_eq!(unsafe { ffi::sqlite3_vfs_find(bn.as_ptr()) }, bv);
    drop(b);
    cleaned(&bn, &bt);
    let (_a, a, an, at) = fixture();
    let (_b, b, bn, bt) = fixture();
    drop(b);
    cleaned(&bn, &bt);
    assert_eq!(
        a.query_row("SELECT v FROM values_", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        41
    );
    drop(a);
    cleaned(&an, &at);
}
#[test]
fn normal_drop_closes_before_unregister_and_free() {
    let (_d, conn, name, trace) = fixture();
    drop(conn);
    cleaned(&name, &trace);
    // Exercise non-OK unregister storage cleanup without mutating SQLite init.
    let state = State::new(false);
    let original = state.original_ptr();
    let trace = Arc::clone(&state.trace);
    let mut r = registration(state, hooks(), original);
    let v = vfs(&r);
    let name = self::name(&r);
    set_name(&trace, &name);
    // Rust-ABI explicit cleanup helper, not a runtime injection field/FFI panic.
    let rc = r.unregister_with(|_| ffi::SQLITE_ERROR);
    assert_eq!(rc, ffi::SQLITE_ERROR);
    drop(r);
    assert_eq!(unsafe { ffi::sqlite3_vfs_find(name.as_ptr()) }, v);
    assert_eq!(trace.dropped.load(Ordering::SeqCst), 0);
}
#[test]
fn error_after_open_unregisters_and_frees_owner() {
    let (_d, conn, name, trace) = fixture();
    let initialize = |_conn: BoundSqliteConnection| -> crate::Result<()> {
        Err(crate::IndexError::Schema("test initializer error".into()))
    };
    assert!(initialize(conn).is_err());
    cleaned(&name, &trace);
}
#[test]
fn unwind_after_open_unregisters_and_frees_owner() {
    let (_d, conn, name, trace) = fixture();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _conn = conn;
        panic!("test caller unwind");
    }));
    assert!(result.is_err());
    cleaned(&name, &trace);
}
#[test]
fn reopen_after_drop_uses_live_private_registration() {
    let (directory, conn, name, trace) = fixture();
    drop(conn);
    cleaned(&name, &trace);
    let state = State::new(true);
    let trace = Arc::clone(&state.trace);
    let original = state.original_ptr();
    let r = registration(state, hooks(), original);
    let name = self::name(&r);
    set_name(&trace, &name);
    let conn = open_owned(r, &directory.path().join("test.sqlite")).unwrap();
    assert!(!unsafe { ffi::sqlite3_vfs_find(name.as_ptr()) }.is_null());
    assert_eq!(
        conn.query_row("SELECT v FROM values_", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        41
    );
    drop(conn);
    cleaned(&name, &trace);
}
#[test]
fn forgotten_statement_retains_connection_and_registration() {
    let (directory, conn, name, trace) = fixture();
    let raw = unsafe { conn.handle() };
    let statement = conn.prepare("SELECT v FROM values_").unwrap();
    std::mem::forget(statement);
    drop(conn);
    assert!(!unsafe { ffi::sqlite3_vfs_find(name.as_ptr()) }.is_null());
    assert_eq!(trace.closed.load(Ordering::SeqCst), 0);
    assert_eq!(trace.dropped.load(Ordering::SeqCst), 0);
    assert!(!unsafe { ffi::sqlite3_db_filename(raw, c"main".as_ptr()) }.is_null());
    // Keep the temp path as well as the intentional SQLite/owner process-lifetime retention.
    std::mem::forget(directory);
}
unsafe fn panic_open(
    _: *mut c_void,
    _: *mut ffi::sqlite3_vfs,
    _: ffi::sqlite3_filename,
    _: *mut ffi::sqlite3_file,
    _: c_int,
    _: *mut c_int,
) -> c_int {
    panic!("Rust hook panic");
}
#[test]
fn rust_hook_unwind_is_caught_at_ffi_boundary() {
    let state = State::new(false);
    let original = state.original_ptr();
    let mut h = hooks();
    h.open = panic_open;
    let r = registration(state, h, original);
    let v = vfs(&r);
    let mut file = ffi::sqlite3_file {
        pMethods: ptr::null(),
    };
    assert_ne!(
        unsafe { (*v).xOpen.unwrap()(v, c"name".as_ptr(), &mut file, 0, ptr::null_mut()) },
        ffi::SQLITE_OK
    );
}
#[test]
fn connection_owner_remains_send() {
    fn send<T: Send>() {}
    send::<BoundSqliteConnection>();
    send::<crate::fts::SourceIndexConnection>();
    send::<crate::fts::SqliteFtsIndex>();
    let (_d, conn, name, trace) = fixture();
    std::thread::spawn(move || {
        assert_eq!(
            conn.query_row("SELECT v FROM values_", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            41
        );
        drop(conn);
    })
    .join()
    .unwrap();
    cleaned(&name, &trace);
}
