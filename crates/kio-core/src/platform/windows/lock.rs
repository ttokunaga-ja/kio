use crate::error::{KioError, Result};
use crate::scope::{
    LockFile, RELEASED_LOCK_PID, canonical_lock_bytes_for, canonical_lock_bytes_with_control,
    new_lock_token,
};
use crate::store_dir::StoreDirectory;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
pub(crate) fn windows_lock_leaf(path: &Path) -> Result<&Path> {
    let leaf = path
        .file_name()
        .filter(|leaf| !leaf.is_empty())
        .map(Path::new)
        .ok_or_else(|| KioError::invalid_usage("lock path must name a leaf"))?;
    Ok(leaf)
}

pub(crate) fn is_windows_lock_contention(error: &std::io::Error) -> bool {
    use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION};

    matches!(
        error.kind(),
        std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
    ) || matches!(
        error.raw_os_error(),
        Some(code) if code == ERROR_SHARING_VIOLATION as i32 || code == ERROR_LOCK_VIOLATION as i32
    )
}

pub(super) fn windows_lock_open_options(
    create_new: bool,
    access_mode: u32,
) -> cap_primitives::fs::OpenOptions {
    use cap_primitives::fs::{FollowSymlinks, OpenOptionsExt};
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ};

    let mut options = cap_primitives::fs::OpenOptions::new();
    options
        .read(true)
        .write(create_new)
        .create_new(create_new)
        .access_mode(access_mode)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        ._cap_fs_ext_follow(FollowSymlinks::No);
    options
}

pub(crate) fn create_windows_lock(
    parent: &File,
    leaf: &Path,
    bytes: &[u8],
) -> std::io::Result<File> {
    // Preserve the lock's DELETE rights and read-only sharing while installing
    // a protected owner-only DACL at creation, before publishing any bytes.
    let mut file = super::private_fs::create_new_private_at(
        parent,
        leaf.as_os_str(),
        super::private_fs::NewPrivateObjectKind::LockFile,
    )?;
    let published = (|| -> std::io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        let (record, _) = read_windows_lock(&file).map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
        })?;
        if canonical_lock_bytes_for(&record).map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
        })? != bytes
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "new Windows lock record differs from its canonical bytes",
            ));
        }
        Ok(())
    })();
    if let Err(error) = published {
        // The create_new handle is the exact object we just created. Do not
        // unlink a path here: a partial record must not wedge future writers,
        // and a competing replacement must never be removed.
        let _ = delete_windows_lock_handle(&file);
        return Err(error);
    }
    Ok(file)
}

pub(crate) fn read_windows_lock(
    file: &File,
) -> Result<(LockFile, crate::platform::RegularFileHandleIdentity)> {
    let identity = crate::platform::regular_file_handle_identity(file)
        .ok_or_else(|| KioError::locked("Windows lock leaf is not a single-link regular file"))?;
    super::private_fs::verify_owner_private_handle(file)
        .map_err(|_| KioError::locked("Windows lock leaf is not owner-private"))?;
    let length = file
        .metadata()
        .map_err(|error| KioError::io(error.to_string(), ".lock"))?
        .len();
    if length > MAX_WINDOWS_LOCK_BYTES {
        return Err(KioError::locked(
            "Windows lock leaf exceeds the bounded record size",
        ));
    }
    let mut reader = file
        .try_clone()
        .map_err(|error| KioError::io(error.to_string(), ".lock"))?;
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|error| KioError::io(error.to_string(), ".lock"))?;
    let mut bytes = Vec::with_capacity(length as usize);
    reader
        .read_to_end(&mut bytes)
        .map_err(|error| KioError::io(error.to_string(), ".lock"))?;
    if bytes.len() as u64 != length || bytes.len() as u64 > MAX_WINDOWS_LOCK_BYTES {
        return Err(KioError::locked(
            "Windows lock leaf changed while being read",
        ));
    }
    let lock: LockFile = serde_json::from_slice(&bytes)
        .map_err(|_| KioError::locked("Windows lock is malformed"))?;
    if canonical_lock_bytes_for(&lock)? != bytes
        || crate::platform::regular_file_handle_identity(file) != Some(identity)
    {
        return Err(KioError::locked(
            "Windows lock is not canonical or changed identity",
        ));
    }
    Ok((lock, identity))
}

fn delete_windows_lock_handle(file: &File) -> std::io::Result<()> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
        FILE_DISPOSITION_INFO_EX, FileDispositionInfoEx, SetFileInformationByHandle,
    };

    let info = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    };
    // SAFETY: `info` has the exact Win32 layout and `file` retains DELETE access.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfoEx,
            std::ptr::addr_of!(info).cast(),
            size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    } == 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn reclaim_windows_stale_lock(
    parent: &File,
    leaf: &Path,
    replacement: &[u8],
) -> Result<Option<File>> {
    use cap_primitives::fs as cap_fs;
    use windows_sys::Win32::Foundation::GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::{DELETE, SYNCHRONIZE};

    let options = windows_lock_open_options(false, GENERIC_READ | DELETE | SYNCHRONIZE);
    let stale = match cap_fs::open(parent, leaf, &options) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) if is_windows_lock_contention(&error) => return Ok(None),
        Err(error) => return Err(KioError::io(error.to_string(), ".lock")),
    };
    let (old, identity) = read_windows_lock(&stale)?;
    if crate::platform::process_is_alive(old.pid) {
        return Ok(None);
    }
    if crate::platform::regular_file_handle_identity(&stale) != Some(identity) {
        return Err(KioError::locked("Windows stale lock changed identity"));
    }
    delete_windows_lock_handle(&stale).map_err(|error| KioError::io(error.to_string(), ".lock"))?;
    drop(stale);
    match create_windows_lock(parent, leaf, replacement) {
        Ok(file) => Ok(Some(file)),
        Err(error) if is_windows_lock_contention(&error) => Ok(None),
        Err(error) => Err(KioError::io(error.to_string(), ".lock")),
    }
}

pub(crate) fn release_windows_owned_lock(file: &File, pid: u32, token: &str) -> Result<()> {
    let (owned, _) = read_windows_lock(file)?;
    if owned.pid != pid || owned.token != token {
        return Ok(());
    }
    delete_windows_lock_handle(file).map_err(|error| KioError::io(error.to_string(), ".lock"))
}

pub(crate) fn process_is_alive(pid: u32) -> bool {
    if pid == RELEASED_LOCK_PID {
        return false;
    }
    if pid == std::process::id() {
        return true;
    }

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, GetLastError, STILL_ACTIVE,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: `OpenProcess` is called with a PID read from the lock file and no
    // inheritable handle. A null handle is never passed to the query/close calls.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        // Windows documents ERROR_INVALID_PARAMETER for a PID that does not name
        // a process (including PID 0). Access denial and every other query error
        // are ambiguous, so keep the lock rather than reclaiming a live owner's.
        return unsafe { GetLastError() } != ERROR_INVALID_PARAMETER;
    }

    let mut exit_code = 0_u32;
    // SAFETY: `process` is a valid handle returned by `OpenProcess`, and
    // `exit_code` points to writable storage for the duration of the call.
    let queried = unsafe { GetExitCodeProcess(process, &mut exit_code) } != 0;
    // SAFETY: this function owns the process handle and closes it exactly once.
    let _ = unsafe { CloseHandle(process) };

    // A query failure is ambiguous. Conservatively retain the lock; otherwise,
    // STILL_ACTIVE is the documented marker for a process that has not exited.
    !queried || exit_code == STILL_ACTIVE as u32
}

#[cfg(test)]
#[cfg(test)]
pub(crate) fn open_windows_lock_parent(path: &Path) -> Result<File> {
    let parent = path
        .parent()
        .ok_or_else(|| KioError::invalid_usage("lock path has no parent"))?;
    StoreDirectory::open_or_create_directories(parent)?
        .root_handle()
        .try_clone()
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))
}

pub(crate) struct BoundLock {
    _kio: File,
    owner: Option<File>,
    gate: Option<super::store_gate::Gate>,
    pid: u32,
    token: String,
}

impl Drop for BoundLock {
    fn drop(&mut self) {
        // The owner handle denies replacement and retains DELETE access. The
        // release routine checks its token and deletes that exact object.
        if let Some(owner) = self.owner.take() {
            let _ = release_windows_owned_lock(&owner, self.pid, &self.token);
            drop(owner);
        }
        drop(self.gate.take());
    }
}

pub(crate) struct ReadGate {
    kio: File,
    gate: super::store_gate::Gate,
    initial_lock: Option<super::store_gate::IdleLock>,
}

impl ReadGate {
    pub(crate) fn recheck_idle(&self) -> Result<()> {
        self.gate.recheck(&self.kio)?;
        if super::store_gate::idle_lock(&self.kio)? != self.initial_lock {
            return Err(KioError::locked(".kio/.lock"));
        }
        Ok(())
    }
}

pub(crate) fn acquire_bound_lock(
    kio: &File,
    store_gate: bool,
    control: &crate::control::ControlHandle,
) -> Result<BoundLock> {
    let gate = if store_gate {
        Some(super::store_gate::Gate::acquire(kio, true)?)
    } else {
        None
    };
    if crate::platform::directory_handle_identity(kio).is_none() {
        return Err(KioError::locked(
            "retained .kio is not an ordinary directory",
        ));
    }
    let retained = kio
        .try_clone()
        .map_err(|error| KioError::io(error.to_string(), ".kio"))?;
    let pid = std::process::id();
    let token = new_lock_token(pid);
    let bytes = canonical_lock_bytes_with_control(control, pid, &token)?;
    let leaf = Path::new(".lock");
    let owner = match create_windows_lock(&retained, leaf, &bytes) {
        Ok(owner) => owner,
        Err(error) if is_windows_lock_contention(&error) => {
            reclaim_windows_stale_lock(&retained, leaf, &bytes)?
                .ok_or_else(|| KioError::locked(".kio/.lock"))?
        }
        Err(error) => return Err(KioError::io(error.to_string(), ".kio/.lock")),
    };
    Ok(BoundLock {
        _kio: retained,
        owner: Some(owner),
        gate,
        pid,
        token,
    })
}

pub(crate) fn acquire_bound_read_gate(kio: &File) -> Result<ReadGate> {
    let gate = super::store_gate::Gate::acquire(kio, false)?;
    let initial_lock = super::store_gate::idle_lock(kio)?;
    let guard = ReadGate {
        kio: kio
            .try_clone()
            .map_err(|e| KioError::io(e.to_string(), ".kio"))?,
        gate,
        initial_lock,
    };
    guard.recheck_idle()?;
    Ok(guard)
}

const MAX_WINDOWS_LOCK_BYTES: u64 = 4096;
pub(crate) struct PathLock {
    _parent: File,
    owner: Option<File>,
    gate: Option<super::store_gate::Gate>,
    pid: u32,
    token: String,
}
impl Drop for PathLock {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.take() {
            let _ = release_windows_owned_lock(&owner, self.pid, &self.token);
            // Windows disposition is not namespace completion until close.
            drop(owner);
        }
        drop(self.gate.take());
    }
}
pub(crate) fn acquire_path_lock(
    _session: &crate::scope::LockSession,
    target: crate::scope::locks::NamedLockTarget,
    control: &crate::control::ControlHandle,
    pid: u32,
    store_gate: bool,
    validate_authority: fn(&File, &crate::control::ControlHandle) -> Result<()>,
    _acquire_gate: crate::scope::locks::AcquireLockGate,
) -> Result<PathLock> {
    let crate::scope::locks::NamedLockTarget { parent, path } = target;
    let gate = if store_gate {
        validate_authority(&parent, control)?;
        Some(super::store_gate::Gate::acquire(&parent, true)?)
    } else {
        None
    };
    let leaf = windows_lock_leaf(&path)?;
    let token = new_lock_token(pid);
    let canonical = canonical_lock_bytes_with_control(control, pid, &token)?;
    let owner = match create_windows_lock(&parent, leaf, &canonical) {
        Ok(owner) => owner,
        Err(error) if is_windows_lock_contention(&error) => {
            reclaim_windows_stale_lock(&parent, leaf, &canonical)?
                .ok_or_else(|| KioError::locked(path.display().to_string()))?
        }
        Err(error) => return Err(KioError::io(error.to_string(), path.display().to_string())),
    };
    Ok(PathLock {
        _parent: parent,
        owner: Some(owner),
        gate,
        pid,
        token,
    })
}
pub(crate) fn validate_store_gate(directory: &StoreDirectory) -> Result<()> {
    super::store_gate::validate(directory.root_handle().as_ref())
}
pub(crate) fn directory_lease_identity(
    kio: &File,
) -> Result<crate::platform::DirectoryHandleIdentity> {
    crate::platform::directory_handle_identity(kio).ok_or_else(|| KioError::locked(".kio"))
}

pub(crate) type PathLockLease = std::rc::Rc<PathLock>;
pub(crate) fn finish_path_lock(lease: &mut Option<PathLockLease>, _release_outer: bool) {
    // The last common Rc lease owns disposition, close, then gate unlock.
    drop(lease.take());
}
