//! Connection-owned admission for registry SQLite files, including native SHM opens.
use super::bound_sqlite::{Hooks, header};
use super::{BoundSqliteConnection, leaf_identity, registry_native};
use crate::{IndexError, Result};
use cap_primitives::fs as cap_fs;
use kio_core::store_dir::StoreDirectory;
use rusqlite::{OpenFlags, ffi};
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    fs::File,
    path::Path,
    sync::{Arc, Mutex},
};

#[cfg(test)]
thread_local! {
    static BEFORE_NATIVE_OPEN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
    static BEFORE_NATIVE_SHM: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
}
#[cfg(test)]
type HookSlot = std::thread::LocalKey<std::cell::RefCell<Option<Box<dyn FnOnce()>>>>;
#[cfg(test)]
fn run_hook(slot: &'static HookSlot) {
    let hook = slot.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}

struct Access {
    directory: StoreDirectory,
    locator: CString,
    binding: registry_native::Binding,
    leaf: String,
    // Windows' SHM open bypasses xOpen. Retain a no-delete-share pin for the
    // connection lifetime before delegating the first native xShmMap.
    shm: Mutex<Option<File>>,
}

fn identity(file: &File) -> Result<super::FileIdentity> {
    let metadata =
        cap_fs::Metadata::from_file(file).map_err(|error| IndexError::Schema(error.to_string()))?;
    leaf_identity(&metadata)
        .ok_or_else(|| IndexError::Schema("registry file has no usable identity".into()))
}

impl Access {
    fn name(&self, suffix: &str) -> String {
        format!("{}{suffix}", self.leaf)
    }
    fn suffix<'a>(&self, name: &'a CStr) -> Option<&'a str> {
        let suffix = name.to_bytes().strip_prefix(self.locator.to_bytes())?;
        match suffix {
            b"" => Some(""),
            b"-journal" => Some("-journal"),
            b"-wal" => Some("-wal"),
            b"-shm" => Some("-shm"),
            _ => None,
        }
    }
    fn private_file(&self, suffix: &str, create: bool) -> Result<File> {
        kio_core::private_fs::verify_private_directory_handle(&self.directory)
            .map_err(|error| IndexError::Schema(error.to_string()))?;
        let name = self.name(suffix);
        let leaf = Path::new(&name);
        let created = if !self
            .directory
            .contains_entry(leaf)
            .map_err(|error| IndexError::Schema(error.to_string()))?
            && create
        {
            match self.directory.create_private_file(leaf) {
                Ok(file) => Some(file),
                Err(error)
                    if error
                        .context()
                        .get("io_error_kind")
                        .and_then(serde_json::Value::as_str)
                        == Some("already_exists") =>
                {
                    None
                }
                Err(error) => return Err(IndexError::Schema(error.to_string())),
            }
        } else {
            None
        };
        let mut options = cap_fs::OpenOptions::new();
        options
            .read(true)
            .write(true)
            ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
        registry_native::configure_open(&mut options);
        let file = cap_fs::open(self.directory.root_handle().as_ref(), leaf, &options)
            .map_err(|error| IndexError::Schema(error.to_string()))?;
        kio_core::private_fs::verify_private_regular_file_handle(&file)
            .map_err(|error| IndexError::Schema(error.to_string()))?;
        if let Some(created) = created
            && identity(&created)? != identity(&file)?
        {
            return Err(IndexError::Schema(
                "private registry leaf changed after creation".into(),
            ));
        }
        Ok(file)
    }
    fn recheck(&self, suffix: &str, file: &File) -> Result<()> {
        kio_core::private_fs::verify_private_regular_file_handle(file)
            .map_err(|error| IndexError::Schema(error.to_string()))?;
        let metadata = cap_fs::stat(
            self.directory.root_handle().as_ref(),
            Path::new(&self.name(suffix)),
            cap_fs::FollowSymlinks::No,
        )
        .map_err(|error| IndexError::Schema(error.to_string()))?;
        if !metadata.is_file() || leaf_identity(&metadata).as_ref() != Some(&identity(file)?) {
            return Err(IndexError::Schema(
                "registry leaf changed during native SQLite access".into(),
            ));
        }
        Ok(())
    }
}

pub(crate) fn open(
    path: &Path,
    directory: &StoreDirectory,
    flags: OpenFlags,
) -> Result<BoundSqliteConnection> {
    let binding = registry_native::bind(path, directory)?;
    open_with_binding(path, directory, flags, binding)
}

pub(crate) fn open_snapshot(
    path: &Path,
    directory: &StoreDirectory,
    storage: &super::PrivateSnapshotStorage,
) -> Result<BoundSqliteConnection> {
    let original = storage
        .capability()
        .map_err(|error| IndexError::Schema(format!("retain private snapshot root: {error:?}")))?;
    if identity(&original)? != identity(directory.root_handle().as_ref())? {
        return Err(IndexError::Schema(
            "private snapshot root changed before native binding".into(),
        ));
    }
    let binding = storage.native_binding(path, directory)?;
    open_with_binding(path, directory, OpenFlags::SQLITE_OPEN_READ_ONLY, binding)
}

fn open_with_binding(
    path: &Path,
    directory: &StoreDirectory,
    flags: OpenFlags,
    binding: registry_native::Binding,
) -> Result<BoundSqliteConnection> {
    let locator = binding.locator().to_owned();
    let sqlite_path = binding.path().to_path_buf();
    let leaf = path
        .file_name()
        .and_then(|leaf| leaf.to_str())
        .ok_or_else(|| IndexError::Schema("registry path has no UTF-8 leaf".into()))?
        .to_owned();
    let access = Arc::new(Access {
        directory: directory.clone(),
        locator,
        binding,
        leaf,
        shm: Mutex::new(None),
    });
    // Refuse unsafe occupied sidecars even if this SQLite open would not use them.
    for suffix in ["", "-journal", "-wal", "-shm"] {
        if access
            .directory
            .contains_entry(Path::new(&access.name(suffix)))
            .map_err(|error| IndexError::Schema(error.to_string()))?
        {
            access.private_file(suffix, false)?;
        }
    }
    let conn = BoundSqliteConnection::open(
        &sqlite_path,
        flags,
        None,
        Some((
            access,
            Hooks {
                open: x_open,
                full_path: x_full_path,
                access: Some(x_access),
                delete: Some(x_delete),
            },
        )),
    )?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    Ok(conn)
}

unsafe fn x_full_path(
    state: *mut c_void,
    _: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    len: c_int,
    out: *mut c_char,
) -> c_int {
    if name.is_null() || out.is_null() || len <= 0 {
        return ffi::SQLITE_CANTOPEN;
    }
    let access = unsafe { &*state.cast::<Arc<Access>>() };
    let name = unsafe { CStr::from_ptr(name) };
    if access.suffix(name).is_none() || name.to_bytes_with_nul().len() > len as usize {
        return ffi::SQLITE_CANTOPEN;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(name.as_ptr(), out, name.to_bytes_with_nul().len());
    }
    ffi::SQLITE_OK
}

unsafe fn reject_opened(file: *mut ffi::sqlite3_file) -> c_int {
    if let Some(methods) = unsafe { (*file).pMethods.as_ref() }
        && let Some(close) = methods.xClose
    {
        unsafe {
            close(file);
        }
    }
    unsafe {
        (*file).pMethods = std::ptr::null();
    }
    ffi::SQLITE_CANTOPEN
}

unsafe fn reject_bound(access: &Access, file: *mut ffi::sqlite3_file) -> c_int {
    let result = access
        .binding
        .execute(ffi::SQLITE_CANTOPEN, || unsafe { reject_opened(file) });
    // A failed native-directory restore poisons the binding. Do not let SQLite
    // invoke an unscoped native close in that exceptional failure path.
    unsafe {
        (*file).pMethods = std::ptr::null();
    }
    result
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
    if name.is_null() {
        return ffi::SQLITE_CANTOPEN;
    }
    let access = unsafe { &*state.cast::<Arc<Access>>() };
    let Some(suffix) = access.suffix(unsafe { CStr::from_ptr(name) }) else {
        return ffi::SQLITE_CANTOPEN;
    };
    let permitted = match suffix {
        "" => flags & ffi::SQLITE_OPEN_MAIN_DB != 0,
        "-wal" => flags & ffi::SQLITE_OPEN_WAL != 0,
        "-journal" => flags & ffi::SQLITE_OPEN_MAIN_JOURNAL != 0,
        _ => false,
    };
    if !permitted {
        return ffi::SQLITE_CANTOPEN;
    }
    let Ok(pin) = access.private_file(suffix, flags & ffi::SQLITE_OPEN_CREATE != 0) else {
        return ffi::SQLITE_CANTOPEN;
    };
    let Some(owner) = (unsafe { header(vfs) }) else {
        return ffi::SQLITE_CANTOPEN;
    };
    let Some(open) = (unsafe { (*owner.original).xOpen }) else {
        return ffi::SQLITE_CANTOPEN;
    };
    // Only the retained capability creates files. Default SQLite must not create
    // a replacement if the already-admitted public name disappears.
    #[cfg(test)]
    run_hook(&BEFORE_NATIVE_OPEN);
    let result = access.binding.execute(ffi::SQLITE_CANTOPEN, || unsafe {
        open(
            owner.original,
            name,
            file,
            flags & !ffi::SQLITE_OPEN_CREATE,
            out,
        )
    });
    if result != ffi::SQLITE_OK {
        if !unsafe { (*file).pMethods }.is_null() {
            unsafe {
                reject_bound(access, file);
            }
        }
        return result;
    }
    let original = unsafe { (*file).pMethods };
    if original.is_null() || unsafe { (*original).xClose }.is_none() {
        return unsafe { reject_bound(access, file) };
    }
    if access.recheck(suffix, &pin).is_err()
        || access.binding.execute(0, || {
            i32::from(unsafe { registry_native::native_matches(file, &pin) })
        }) == 0
    {
        return unsafe { reject_bound(access, file) };
    }
    let mut methods = unsafe { *original };
    methods.xClose = Some(x_close);
    methods.xSync = Some(x_sync);
    methods.xFileControl = Some(x_file_control);
    if suffix.is_empty() {
        methods.xShmMap = Some(x_shm_map);
        methods.xShmUnmap = Some(x_shm_unmap);
    }
    let wrapped = Box::new(Methods {
        methods,
        original,
        access: Arc::clone(access),
        _pin: pin,
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
    access: Arc<Access>,
    _pin: File,
}
unsafe extern "C" fn x_close(file: *mut ffi::sqlite3_file) -> c_int {
    let methods = unsafe { Box::from_raw((*file).pMethods.cast_mut().cast::<Methods>()) };
    unsafe {
        (*file).pMethods = methods.original;
    }
    let result = methods
        .access
        .binding
        .execute(ffi::SQLITE_IOERR_CLOSE, || unsafe {
            ((*methods.original).xClose.expect("verified native close"))(file)
        });
    unsafe {
        (*file).pMethods = std::ptr::null();
    }
    result
}
unsafe extern "C" fn x_sync(file: *mut ffi::sqlite3_file, flags: c_int) -> c_int {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let methods = unsafe { &*(*file).pMethods.cast::<Methods>() };
        let Some(sync) = (unsafe { (*methods.original).xSync }) else {
            return ffi::SQLITE_IOERR_FSYNC;
        };
        methods
            .access
            .binding
            .execute(ffi::SQLITE_IOERR_FSYNC, || unsafe { sync(file, flags) })
    }))
    .unwrap_or(ffi::SQLITE_IOERR_FSYNC)
}
unsafe extern "C" fn x_file_control(
    file: *mut ffi::sqlite3_file,
    operation: c_int,
    argument: *mut c_void,
) -> c_int {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let methods = unsafe { &*(*file).pMethods.cast::<Methods>() };
        let Some(control) = (unsafe { (*methods.original).xFileControl }) else {
            return ffi::SQLITE_NOTFOUND;
        };
        methods
            .access
            .binding
            .execute(ffi::SQLITE_IOERR, || unsafe {
                control(file, operation, argument)
            })
    }))
    .unwrap_or(ffi::SQLITE_IOERR)
}
unsafe extern "C" fn x_shm_unmap(file: *mut ffi::sqlite3_file, delete: c_int) -> c_int {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let methods = unsafe { &*(*file).pMethods.cast::<Methods>() };
        let Some(unmap) = (unsafe { (*methods.original).xShmUnmap }) else {
            return ffi::SQLITE_OK;
        };
        let Ok(mut shm) = methods.access.shm.lock() else {
            return ffi::SQLITE_IOERR_SHMMAP;
        };
        if let Some(pin) = shm.as_ref()
            && methods.access.recheck("-shm", pin).is_err()
        {
            return ffi::SQLITE_IOERR_SHMMAP;
        }
        // Native SHM mapping/lock handles still retain this exact object.
        // Release a Windows no-delete pin only at native deletion, so the
        // last unmap can perform its normal verified-name cleanup.
        registry_native::release_shm_pin(&mut shm, delete);
        drop(shm);
        methods
            .access
            .binding
            .execute(ffi::SQLITE_IOERR_SHMMAP, || unsafe { unmap(file, delete) })
    }))
    .unwrap_or(ffi::SQLITE_IOERR_SHMMAP)
}
unsafe extern "C" fn x_shm_map(
    file: *mut ffi::sqlite3_file,
    page: c_int,
    size: c_int,
    extend: c_int,
    out: *mut *mut c_void,
) -> c_int {
    // SQLite invokes this outside the VFS trampoline. Do not unwind across C.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let methods = unsafe { &*(*file).pMethods.cast::<Methods>() };
        let Some(map) = (unsafe { (*methods.original).xShmMap }) else {
            return ffi::SQLITE_IOERR_SHMMAP;
        };
        let Ok(mut shm) = methods.access.shm.lock() else {
            return ffi::SQLITE_IOERR_SHMMAP;
        };
        if shm.is_none() {
            // bExtend controls mapping/file-size extension, not whether the
            // native VFS must initialize its SHM coordination handle. These
            // connections own a writer target or an isolated private copy;
            // source read-only snapshots never pass their source to SQLite.
            match methods.access.private_file("-shm", true) {
                Ok(pin) => *shm = Some(pin),
                Err(_error) => {
                    #[cfg(test)]
                    eprintln!("registry SHM private creation/admission failed: {_error}");
                    return ffi::SQLITE_IOERR_SHMMAP;
                }
            }
        }
        let pin = shm.as_ref().expect("SHM pin retained");
        if let Err(_error) = methods.access.recheck("-shm", pin) {
            #[cfg(test)]
            eprintln!("registry SHM retained identity check failed: {_error}");
            return ffi::SQLITE_IOERR_SHMMAP;
        }
        #[cfg(test)]
        run_hook(&BEFORE_NATIVE_SHM);
        let result = methods
            .access
            .binding
            .execute(ffi::SQLITE_IOERR_SHMMAP, || unsafe {
                map(file, page, size, extend, out)
            });
        if let Err(_error) = methods.access.recheck("-shm", pin) {
            #[cfg(test)]
            eprintln!("registry SHM retained identity check failed: {_error}");
            return ffi::SQLITE_IOERR_SHMMAP;
        }
        result
    }))
    .unwrap_or(ffi::SQLITE_IOERR_SHMMAP)
}
unsafe fn x_access(
    state: *mut c_void,
    _: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    _: c_int,
    out: *mut c_int,
) -> c_int {
    if name.is_null() || out.is_null() {
        return ffi::SQLITE_IOERR_ACCESS;
    }
    unsafe {
        *out = 0;
    }
    let access = unsafe { &*state.cast::<Arc<Access>>() };
    let Some(suffix) = access.suffix(unsafe { CStr::from_ptr(name) }) else {
        return ffi::SQLITE_IOERR_ACCESS;
    };
    if suffix == "-shm"
        && let Ok(shm) = access.shm.lock()
        && let Some(pin) = shm.as_ref()
    {
        if access.recheck(suffix, pin).is_err() {
            return ffi::SQLITE_IOERR_ACCESS;
        }
        unsafe {
            *out = 1;
        }
        return ffi::SQLITE_OK;
    }
    match access
        .directory
        .contains_entry(Path::new(&access.name(suffix)))
    {
        Ok(false) => ffi::SQLITE_OK,
        Ok(true) if access.private_file(suffix, false).is_ok() => {
            unsafe {
                *out = 1;
            }
            ffi::SQLITE_OK
        }
        _ => ffi::SQLITE_IOERR_ACCESS,
    }
}
unsafe fn x_delete(
    state: *mut c_void,
    _: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    _: c_int,
) -> c_int {
    if name.is_null() {
        return ffi::SQLITE_IOERR_DELETE;
    }
    let access = unsafe { &*state.cast::<Arc<Access>>() };
    let Some(suffix) = access.suffix(unsafe { CStr::from_ptr(name) }) else {
        return ffi::SQLITE_IOERR_DELETE;
    };
    if suffix.is_empty() {
        return ffi::SQLITE_IOERR_DELETE;
    }
    let leaf = access.name(suffix);
    match access.directory.contains_entry(Path::new(&leaf)) {
        Ok(false) => ffi::SQLITE_OK,
        Ok(true) => {
            if access.private_file(suffix, false).is_err() {
                return ffi::SQLITE_IOERR_DELETE;
            }
            if access.directory.remove_file(Path::new(&leaf)).is_ok() {
                ffi::SQLITE_OK
            } else {
                ffi::SQLITE_IOERR_DELETE
            }
        }
        Err(_) => ffi::SQLITE_IOERR_DELETE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{RegistryDb, RegistryEntry};
    use std::{cell::Cell, fs, path::PathBuf, rc::Rc};

    fn external_ancestor() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o777)).unwrap();
        }
        directory
    }
    fn seed(path: &Path, tag: &str) -> RegistryDb {
        let db = RegistryDb::open(path).unwrap();
        db.upsert(&RegistryEntry {
            scope_id: tag.to_owned(),
            kio_path: format!("/{tag}/.kio"),
            root_path: format!("/{tag}"),
            participates_in_global_search: true,
            indexed: true,
            last_seen_at: "2026-10-22T00:00:00Z".into(),
        })
        .unwrap();
        db
    }
    fn swap_hook(
        a: PathBuf,
        b: PathBuf,
        parked: PathBuf,
        swapped: Rc<Cell<bool>>,
    ) -> Box<dyn FnOnce()> {
        Box::new(move || {
            // Windows may already prevent renaming a pinned directory. A refused
            // replacement is the intended safe outcome, not a test failure.
            if fs::rename(&a, &parked).is_ok() {
                fs::rename(&b, &a).unwrap();
                swapped.set(true);
            }
        })
    }
    fn assert_original_or_refused(result: crate::Result<RegistryDb>) {
        if let Ok(db) = result {
            let rows = db.all_entries().unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0].scope_id, "original-a",
                "native SQLite entered replacement B"
            );
        }
    }
    #[test]
    fn registry_native_open_never_enters_replaced_parent() {
        let base = external_ancestor();
        let a = base.path().join("a");
        let b = base.path().join("b");
        let a_path = a.join("scope-registry.sqlite");
        let b_path = b.join("scope-registry.sqlite");
        drop(seed(&a_path, "original-a"));
        let _b = seed(&b_path, "replacement-b");
        let swapped = Rc::new(Cell::new(false));
        BEFORE_NATIVE_OPEN.with(|hook| {
            *hook.borrow_mut() = Some(swap_hook(
                a.clone(),
                b,
                base.path().join("parked-a"),
                Rc::clone(&swapped),
            ))
        });
        let result = RegistryDb::open(&a_path);
        BEFORE_NATIVE_OPEN.with(|hook| *hook.borrow_mut() = None);
        assert_original_or_refused(result);
    }
    #[test]
    fn registry_native_shm_never_mutates_replacement_parent() {
        let base = external_ancestor();
        let a = base.path().join("a");
        let b = base.path().join("b");
        let a_path = a.join("scope-registry.sqlite");
        let b_path = b.join("scope-registry.sqlite");
        drop(seed(&a_path, "original-a"));
        let _b = seed(&b_path, "replacement-b");
        let shm_name = "scope-registry.sqlite-shm";
        let before = fs::read(b.join(shm_name)).unwrap();
        let swapped = Rc::new(Cell::new(false));
        BEFORE_NATIVE_SHM.with(|hook| {
            *hook.borrow_mut() = Some(swap_hook(
                a.clone(),
                b.clone(),
                base.path().join("parked-a"),
                Rc::clone(&swapped),
            ))
        });
        let result = RegistryDb::open(&a_path);
        BEFORE_NATIVE_SHM.with(|hook| *hook.borrow_mut() = None);
        let replacement = if swapped.get() { &a } else { &b };
        let after = fs::read(replacement.join(shm_name)).unwrap();
        assert!(
            after == before,
            "native SHM accessed replacement B (before {} bytes, after {} bytes)",
            before.len(),
            after.len()
        );
        assert_original_or_refused(result);
    }
}
