use crate::{IndexError, Result};
use rusqlite::{Connection, OpenFlags, Transaction, ffi};
use std::{
    cell::UnsafeCell,
    ffi::{CString, c_char, c_int, c_void},
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    ptr,
};

type Open = unsafe extern "C" fn(
    *mut ffi::sqlite3_vfs,
    ffi::sqlite3_filename,
    *mut ffi::sqlite3_file,
    c_int,
    *mut c_int,
) -> c_int;
type OpenHook = unsafe fn(
    *mut c_void,
    *mut ffi::sqlite3_vfs,
    ffi::sqlite3_filename,
    *mut ffi::sqlite3_file,
    c_int,
    *mut c_int,
) -> c_int;
type FullPath =
    unsafe fn(*mut c_void, *mut ffi::sqlite3_vfs, *const c_char, c_int, *mut c_char) -> c_int;
type Access =
    unsafe fn(*mut c_void, *mut ffi::sqlite3_vfs, *const c_char, c_int, *mut c_int) -> c_int;
type Delete = unsafe fn(*mut c_void, *mut ffi::sqlite3_vfs, *const c_char, c_int) -> c_int;
type Symbol = Option<unsafe extern "C" fn(*mut ffi::sqlite3_vfs, *mut c_void, *const c_char)>;

pub(crate) struct Hooks {
    pub(crate) open: OpenHook,
    pub(crate) full_path: FullPath,
    pub(crate) access: Option<Access>,
    pub(crate) delete: Option<Delete>,
}
#[repr(C)]
pub(crate) struct Header {
    pub(crate) vfs: UnsafeCell<ffi::sqlite3_vfs>,
    pub(crate) original: *mut ffi::sqlite3_vfs,
    pub(crate) state: *mut c_void,
    hooks: Hooks,
}
#[repr(C)]
struct VfsOwner<S> {
    header: Header,
    name: CString,
    state: S,
}
pub(crate) struct RegisteredVfs<S> {
    owner: Option<Box<VfsOwner<S>>>,
}
// The allocation never moves. SQLite use is sequential under Connection's Send
// contract; native original outlives registration (bundled library lifetime),
// and private names are not exposed for independent connection reuse.
unsafe impl<S: Send> Send for RegisteredVfs<S> {}
trait Registration: Send {
    fn vfs(&self) -> Option<*mut ffi::sqlite3_vfs>;
}
impl<S: Send> Registration for RegisteredVfs<S> {
    fn vfs(&self) -> Option<*mut ffi::sqlite3_vfs> {
        self.owner.as_ref().map(|o| o.header.vfs.get())
    }
}

// SAFETY: non-null input must point to a live readable SQLite VFS. The private
// marker/self-address checks reject foreign and copied headers before tail
// access. Forged marker+self headers lie outside this FFI pointer contract.
pub(crate) unsafe fn header<'a>(vfs: *mut ffi::sqlite3_vfs) -> Option<&'a Header> {
    if vfs.is_null() || unsafe { (*vfs).pAppData } != vfs.cast() {
        return None;
    }
    let open = unsafe { (*vfs).xOpen }?;
    if !ptr::fn_addr_eq(open, x_open as Open) {
        return None;
    }
    Some(unsafe { &*vfs.cast::<Header>() })
}
macro_rules! forward {
    ($name:ident, $slot:ident, ($($arg:ident : $ty:ty),*), (), ()) => {
        unsafe extern "C" fn $name(vfs: *mut ffi::sqlite3_vfs, $($arg:$ty),*) {
            let _=catch_unwind(AssertUnwindSafe(|| {
                let Some(owner)=(unsafe { header(vfs) }) else { return; };
                if let Some(callback)=unsafe { (*owner.original).$slot } { unsafe { callback(owner.original,$($arg),*); } }
            }));
        }
    };
    ($name:ident, $slot:ident, ($($arg:ident : $ty:ty),*), $ret:ty, None) => {
        unsafe extern "C" fn $name(vfs: *mut ffi::sqlite3_vfs, $($arg:$ty),*) -> $ret {
            catch_unwind(AssertUnwindSafe(|| {
                let owner = (unsafe { header(vfs) })?;
                let callback = (unsafe { (*owner.original).$slot })?;
                unsafe { callback(owner.original, $($arg),*) }
            })).unwrap_or(None)
        }
    };
    ($name:ident, $slot:ident, ($($arg:ident : $ty:ty),*), $ret:ty, $error:expr) => {
        unsafe extern "C" fn $name(vfs: *mut ffi::sqlite3_vfs, $($arg:$ty),*) -> $ret {
            catch_unwind(AssertUnwindSafe(|| {
                let Some(owner) = (unsafe { header(vfs) }) else { return $error; };
                match unsafe { (*owner.original).$slot } {
                    Some(callback) => unsafe { callback(owner.original, $($arg),*) },
                    None => $error,
                }
            })).unwrap_or($error)
        }
    };
}
forward!(x_dl_open,xDlOpen,(name:*const c_char),*mut c_void,ptr::null_mut());
forward!(x_dl_error,xDlError,(len:c_int,out:*mut c_char),(),());
forward!(x_dl_sym,xDlSym,(handle:*mut c_void,name:*const c_char),Symbol,None);
forward!(x_dl_close,xDlClose,(handle:*mut c_void),(),());
forward!(x_randomness,xRandomness,(len:c_int,out:*mut c_char),c_int,0);
forward!(x_sleep,xSleep,(duration:c_int),c_int,0);
forward!(x_time,xCurrentTime,(out:*mut f64),c_int,ffi::SQLITE_ERROR);
forward!(x_last_error,xGetLastError,(len:c_int,out:*mut c_char),c_int,0);
forward!(x_time64,xCurrentTimeInt64,(out:*mut ffi::sqlite3_int64),c_int,ffi::SQLITE_ERROR);
forward!(x_set_system_call,xSetSystemCall,(name:*const c_char,call:ffi::sqlite3_syscall_ptr),c_int,ffi::SQLITE_NOTFOUND);
forward!(x_get_system_call,xGetSystemCall,(name:*const c_char),ffi::sqlite3_syscall_ptr,None);
forward!(x_next_system_call,xNextSystemCall,(name:*const c_char),*const c_char,ptr::null());

unsafe extern "C" fn x_open(
    vfs: *mut ffi::sqlite3_vfs,
    name: ffi::sqlite3_filename,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        let Some(owner) = (unsafe { header(vfs) }) else {
            return ffi::SQLITE_CANTOPEN;
        };
        unsafe { (owner.hooks.open)(owner.state, vfs, name, file, flags, out) }
    }))
    .unwrap_or(ffi::SQLITE_CANTOPEN)
}
unsafe extern "C" fn x_full_path(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    len: c_int,
    out: *mut c_char,
) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        let Some(owner) = (unsafe { header(vfs) }) else {
            return ffi::SQLITE_CANTOPEN;
        };
        unsafe { (owner.hooks.full_path)(owner.state, vfs, name, len, out) }
    }))
    .unwrap_or(ffi::SQLITE_CANTOPEN)
}
macro_rules! override_or_forward {
    ($name:ident,$hook:ident,$slot:ident,($($arg:ident:$ty:ty),*),$error:expr) => {
        unsafe extern "C" fn $name(vfs:*mut ffi::sqlite3_vfs,$($arg:$ty),*)->c_int {
            catch_unwind(AssertUnwindSafe(|| {
                let Some(owner)=(unsafe { header(vfs) }) else { return $error; };
                if let Some(hook)=owner.hooks.$hook { return unsafe { hook(owner.state,vfs,$($arg),*) }; }
                match unsafe { (*owner.original).$slot } {
                    Some(callback)=>unsafe { callback(owner.original,$($arg),*) }, None=>$error,
                }
            })).unwrap_or($error)
        }
    };
}
override_or_forward!(x_access,access,xAccess,(name:*const c_char,flags:c_int,out:*mut c_int),ffi::SQLITE_IOERR_ACCESS);
override_or_forward!(x_delete,delete,xDelete,(name:*const c_char,sync:c_int),ffi::SQLITE_IOERR_DELETE);

impl<S> RegisteredVfs<S> {
    // SAFETY: original and its immutable callback fields/userdata stay live
    // through connection close and successful unregister. The original supports
    // sequential use when Connection moves between threads. Do not copy pNext.
    pub(crate) unsafe fn new(
        state: S,
        hooks: Hooks,
        original: *mut ffi::sqlite3_vfs,
    ) -> Result<Self> {
        if original.is_null() {
            return Err(IndexError::Schema("SQLite has no default VFS".into()));
        }
        let version = unsafe { (*original).iVersion };
        let vfs = ffi::sqlite3_vfs {
            iVersion: version,
            szOsFile: unsafe { (*original).szOsFile },
            mxPathname: unsafe { (*original).mxPathname },
            pNext: ptr::null_mut(),
            zName: ptr::null(),
            pAppData: ptr::null_mut(),
            xOpen: Some(x_open),
            xFullPathname: Some(x_full_path),
            xDelete: if hooks.delete.is_some() || unsafe { (*original).xDelete }.is_some() {
                Some(x_delete)
            } else {
                None
            },
            xAccess: if hooks.access.is_some() || unsafe { (*original).xAccess }.is_some() {
                Some(x_access)
            } else {
                None
            },
            xDlOpen: unsafe { (*original).xDlOpen }.map(|_| x_dl_open as _),
            xDlError: unsafe { (*original).xDlError }.map(|_| x_dl_error as _),
            xDlSym: unsafe { (*original).xDlSym }.map(|_| x_dl_sym as _),
            xDlClose: unsafe { (*original).xDlClose }.map(|_| x_dl_close as _),
            xRandomness: unsafe { (*original).xRandomness }.map(|_| x_randomness as _),
            xSleep: unsafe { (*original).xSleep }.map(|_| x_sleep as _),
            xCurrentTime: unsafe { (*original).xCurrentTime }.map(|_| x_time as _),
            xGetLastError: unsafe { (*original).xGetLastError }.map(|_| x_last_error as _),
            xCurrentTimeInt64: if version >= 2 {
                unsafe { (*original).xCurrentTimeInt64 }.map(|_| x_time64 as _)
            } else {
                None
            },
            xSetSystemCall: if version >= 3 {
                unsafe { (*original).xSetSystemCall }.map(|_| x_set_system_call as _)
            } else {
                None
            },
            xGetSystemCall: if version >= 3 {
                unsafe { (*original).xGetSystemCall }.map(|_| x_get_system_call as _)
            } else {
                None
            },
            xNextSystemCall: if version >= 3 {
                unsafe { (*original).xNextSystemCall }.map(|_| x_next_system_call as _)
            } else {
                None
            },
        };
        let mut owner = Box::new(VfsOwner {
            header: Header {
                vfs: UnsafeCell::new(vfs),
                original,
                state: ptr::null_mut(),
                hooks,
            },
            name: CString::default(),
            state,
        });
        let address = owner.header.vfs.get();
        owner.name = CString::new(format!("kio-owned-{address:p}"))
            .map_err(|e| IndexError::Schema(e.to_string()))?;
        owner.header.state = ptr::addr_of_mut!(owner.state).cast();
        unsafe {
            (*address).zName = owner.name.as_ptr();
            (*address).pAppData = address.cast();
        }
        let code = unsafe { ffi::sqlite3_vfs_register(address, 0) };
        if code != ffi::SQLITE_OK {
            return Err(IndexError::Schema(format!(
                "register bound SQLite VFS: SQLite error {code}"
            )));
        }
        Ok(Self { owner: Some(owner) })
    }
    fn unregister_with(
        &mut self,
        unregister: impl FnOnce(*mut ffi::sqlite3_vfs) -> c_int,
    ) -> c_int {
        let Some(owner) = self.owner.take() else {
            return ffi::SQLITE_OK;
        };
        let code = catch_unwind(AssertUnwindSafe(|| unregister(owner.header.vfs.get())))
            .unwrap_or(ffi::SQLITE_ERROR);
        if code != ffi::SQLITE_OK {
            std::mem::forget(owner);
        }
        code
    }
}
impl<S> Drop for RegisteredVfs<S> {
    fn drop(&mut self) {
        self.unregister_with(|vfs| unsafe { ffi::sqlite3_vfs_unregister(vfs) });
    }
}

pub(crate) struct BoundSqliteConnection {
    conn: Option<Connection>,
    registration: Option<Box<dyn Registration>>,
}
impl BoundSqliteConnection {
    pub(crate) fn from_connection(conn: Connection) -> Self {
        Self {
            conn: Some(conn),
            registration: None,
        }
    }
    pub(crate) fn open<S: Send + 'static>(
        path: &Path,
        flags: OpenFlags,
        existing_vfs: Option<&str>,
        owned: Option<(S, Hooks)>,
    ) -> Result<Self> {
        match owned {
            Some((state, hooks)) => {
                let original = unsafe { ffi::sqlite3_vfs_find(ptr::null()) };
                // SAFETY: bundled native default outlives every private owner.
                let registration = unsafe { RegisteredVfs::new(state, hooks, original) }?;
                Self::open_registered(registration, path, flags)
            }
            None => {
                let conn = match existing_vfs {
                    Some(name) => Connection::open_with_flags_and_vfs(path, flags, name)?,
                    None => Connection::open_with_flags(path, flags)?,
                };
                Ok(Self::from_connection(conn))
            }
        }
    }
    pub(crate) fn open_registered<S: Send + 'static>(
        registration: RegisteredVfs<S>,
        path: &Path,
        flags: OpenFlags,
    ) -> Result<Self> {
        // Store registration before open, so its allocation/name stay guarded
        // through SQLite errors and construction unwind. Box before native open.
        let mut owned = Self {
            conn: None,
            registration: Some(Box::new(registration)),
        };
        let vfs = owned
            .registered_vfs()
            .ok_or_else(|| IndexError::Schema("SQLite VFS already unregistered".into()))?;
        let name = unsafe { std::ffi::CStr::from_ptr((*vfs).zName) }
            .to_str()
            .map_err(|e| IndexError::Schema(e.to_string()))?;
        owned.conn = Some(Connection::open_with_flags_and_vfs(path, flags, name)?);
        Ok(owned)
    }
    pub(crate) fn registered_vfs(&self) -> Option<*mut ffi::sqlite3_vfs> {
        self.registration.as_ref().and_then(|r| r.vfs())
    }
    pub(crate) fn transaction_with_behavior(
        &mut self,
        behavior: rusqlite::TransactionBehavior,
    ) -> rusqlite::Result<Transaction<'_>> {
        self.conn
            .as_mut()
            .expect("live bound SQLite connection")
            .transaction_with_behavior(behavior)
    }
    pub(crate) fn transaction(&mut self) -> rusqlite::Result<Transaction<'_>> {
        self.conn
            .as_mut()
            .expect("live bound SQLite connection")
            .transaction()
    }
    #[cfg(test)]
    pub(crate) fn registered_vfs_for_test(&self) -> Option<*mut ffi::sqlite3_vfs> {
        self.registered_vfs()
    }
}
impl std::ops::Deref for BoundSqliteConnection {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn.as_ref().expect("live bound SQLite connection")
    }
}
impl Drop for BoundSqliteConnection {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take()
            && let Err((conn, _)) = conn.close()
        {
            // Keep SQLite and interrupt bookkeeping plus registered owner
            // alive on BUSY. No statement sweep or changed Drop reporting.
            std::mem::forget(conn);
            if let Some(registration) = self.registration.take() {
                std::mem::forget(registration);
            }
        }
        // Only after successful close does registration Drop unregister/free.
    }
}

#[cfg(test)]
#[path = "bound_sqlite_tests.rs"]
pub(crate) mod tests;
