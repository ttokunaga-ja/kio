//! Windows-only owner-private storage for ledger snapshots.
//!
//! This module deliberately avoids `tempfile`: a shared temporary directory
//! followed by a later ACL change has an attacker-controlled interval.  The
//! directory and each fixed SQLite leaf are created with an owner-only,
//! protected DACL and then bound to a no-delete-share handle.

#![cfg(windows)]

use std::{
    ffi::c_void,
    fs, io, mem,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle},
    },
    path::{Path, PathBuf},
    ptr,
};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, ERROR_INSUFFICIENT_BUFFER, GetLastError, HANDLE,
        INVALID_HANDLE_VALUE, LocalFree,
    },
    Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT},
    Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_REVISION, ACL_SIZE_INFORMATION,
        AddAccessAllowedAceEx, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
        GetAclInformation, GetLengthSid, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
        GetSecurityDescriptorOwner, GetTokenInformation, INHERITED_ACE, InitializeAcl,
        InitializeSecurityDescriptor, OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
        SetSecurityDescriptorControl, SetSecurityDescriptorDacl, SetSecurityDescriptorOwner,
        TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    Storage::FileSystem::{
        CREATE_NEW, CreateDirectoryW, CreateFileW, DELETE, FILE_ALL_ACCESS, FILE_ATTRIBUTE_NORMAL,
        FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
        FILE_DISPOSITION_INFO_EX, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_TRAVERSE, FILE_WRITE_DATA, FileDispositionInfoEx, OPEN_EXISTING,
        READ_CONTROL, SetFileInformationByHandle,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

const MAX_CREATE_ATTEMPTS: usize = 16;
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
const ACL_SIZE_INFORMATION_CLASS: i32 = 2;
const PRIVATE_SQLITE_LEAVES: [&str; 3] =
    ["ledger.sqlite", "ledger.sqlite-wal", "ledger.sqlite-shm"];

#[derive(Clone, Copy)]
enum AclShape {
    ExactCreated { directory: bool },
    OwnerOnly,
}

/// A private, handle-pinned directory for a single ledger snapshot.
///
/// The directory handle has no delete share and remains live until the
/// snapshot connection closes. Cleanup only ever marks verified child handles
/// for deletion; it never recursively deletes a pathname.
pub(crate) struct LedgerSnapshotPrivateDir {
    path: PathBuf,
    handle: Option<fs::File>,
}

impl LedgerSnapshotPrivateDir {
    pub(crate) fn create() -> io::Result<Self> {
        let root = std::env::temp_dir();
        let owner = CurrentUserSid::current()?;
        for _ in 0..MAX_CREATE_ATTEMPTS {
            let path = root.join(random_leaf()?);
            let mut descriptor = OwnerOnlyDescriptor::new(&owner, true)?;
            let attributes = descriptor.security_attributes();
            let wide = wide_path(&path)?;
            // SAFETY: arguments remain valid for the duration of CreateDirectoryW.
            if unsafe { CreateDirectoryW(wide.as_ptr(), &attributes) } == 0 {
                let error = last_error();
                if error.raw_os_error() == Some(ERROR_ALREADY_EXISTS as i32) {
                    continue;
                }
                return Err(context(
                    error,
                    "create owner-private ledger snapshot directory",
                ));
            }
            // On validation failure the handle has already closed. Never clean
            // up by the shared temporary-root pathname: it could now name a
            // replacement. Leave it for OS/user cleanup.
            let handle = Self::open_and_verify(path.clone(), &owner)?;
            return Ok(Self {
                path,
                handle: Some(handle),
            });
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate owner-private ledger snapshot directory",
        ))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
    pub(crate) fn handle(&self) -> &fs::File {
        self.handle
            .as_ref()
            .expect("private ledger directory remains pinned")
    }
    pub(crate) fn capability(&self) -> io::Result<fs::File> {
        self.handle().try_clone()
    }

    pub(crate) fn create_file(&self, basename: &str) -> io::Result<fs::File> {
        validate_basename(basename)?;
        let owner = CurrentUserSid::current()?;
        let path = self.path.join(basename);
        let mut descriptor = OwnerOnlyDescriptor::new(&owner, false)?;
        let attributes = descriptor.security_attributes();
        let wide = wide_path(&path)?;
        // SAFETY: path and descriptor stay live for the call; raw is owned on success.
        let raw = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | READ_CONTROL,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                &attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
                ptr::null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(context(
                last_error(),
                "create owner-private ledger snapshot leaf",
            ));
        }
        // SAFETY: CreateFileW returned an owned valid handle.
        let file = unsafe { fs::File::from_raw_handle(raw as _) };
        if let Err(error) = verify_file_handle(&file, &path, &owner) {
            drop(file);
            return Err(error);
        }
        Ok(file)
    }

    /// Check the exact private shape immediately before SQLite receives the
    /// private main path.  A source WAL is copied only when it was present;
    /// SHM must still be absent here because SQLite, not the source, may
    /// create a private SHM after this check.
    pub(crate) fn verify_before_sqlite(&self, has_wal: bool) -> io::Result<()> {
        let owner = CurrentUserSid::current()?;
        verify_directory_handle(self.handle(), self.path(), &owner)?;
        let expected: &[&str] = if has_wal {
            &["ledger.sqlite", "ledger.sqlite-wal"]
        } else {
            &["ledger.sqlite"]
        };
        let leaves = private_dir_leaves(&self.path)?;
        if leaves.len() != expected.len()
            || !leaves
                .iter()
                .zip(expected)
                .all(|(actual, expected)| actual == *expected)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "owner-private ledger snapshot has an unexpected pre-SQLite leaf",
            ));
        }
        for leaf in expected {
            let file = open_private_leaf_for_verify(&self.path, leaf, &owner)?;
            drop(file);
        }
        Ok(())
    }

    fn open_and_verify(path: PathBuf, owner: &CurrentUserSid) -> io::Result<fs::File> {
        let wide = wide_path(&path)?;
        // SAFETY: no delete sharing pins the directory while a snapshot uses it.
        let raw = unsafe {
            CreateFileW(
                wide.as_ptr(),
                DELETE | FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | READ_CONTROL,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                ptr::null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(context(
                last_error(),
                "open owner-private ledger snapshot directory",
            ));
        }
        // SAFETY: CreateFileW returned an owned valid handle.
        let file = unsafe { fs::File::from_raw_handle(raw as _) };
        verify_directory_handle(&file, &path, owner)?;
        Ok(file)
    }
}

impl Drop for LedgerSnapshotPrivateDir {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.as_ref() {
            let _ = cleanup_pinned_private_dir(handle, &self.path);
        }
        self.handle.take();
    }
}

struct CurrentUserSid {
    bytes: Vec<usize>,
}
impl CurrentUserSid {
    fn current() -> io::Result<Self> {
        let mut token = ptr::null_mut();
        // SAFETY: current process pseudo-handle is valid.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(context(last_error(), "open current process token"));
        }
        let result = Self::from_token(token);
        // SAFETY: token was returned by OpenProcessToken.
        unsafe { CloseHandle(token) };
        result
    }
    fn from_token(token: HANDLE) -> io::Result<Self> {
        let mut needed = 0_u32;
        // SAFETY: intentional size probe.
        let probe =
            unsafe { GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut needed) };
        if probe != 0 || needed == 0 || unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER {
            return Err(context(last_error(), "size current token SID"));
        }
        let mut storage = words_for(needed as usize);
        // SAFETY: storage has at least `needed` bytes.
        if unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                storage.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        } == 0
        {
            return Err(context(last_error(), "read current token SID"));
        }
        let sid = unsafe { (*storage.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        if sid.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "current token has no SID",
            ));
        }
        let len = unsafe { GetLengthSid(sid) } as usize;
        if len == 0 || len > needed as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid current token SID",
            ));
        }
        let mut bytes = words_for(len);
        // SAFETY: spans are valid and non-overlapping.
        unsafe { ptr::copy_nonoverlapping(sid.cast::<u8>(), bytes.as_mut_ptr().cast::<u8>(), len) };
        Ok(Self { bytes })
    }
    fn as_psid(&self) -> PSID {
        // Windows exposes `PSID` as a mutable opaque pointer even for APIs
        // that only read it. The allocation remains owned and immutable here.
        self.bytes.as_ptr().cast_mut().cast()
    }
    fn len(&self) -> usize {
        unsafe { GetLengthSid(self.as_psid()) as usize }
    }
}

struct OwnerOnlyDescriptor {
    _owner: CurrentUserSid,
    _acl: Vec<usize>,
    descriptor: SECURITY_DESCRIPTOR,
}
impl OwnerOnlyDescriptor {
    fn new(owner: &CurrentUserSid, directory: bool) -> io::Result<Self> {
        let owner = CurrentUserSid {
            bytes: owner.bytes.clone(),
        };
        let ace_size = mem::size_of::<ACCESS_ALLOWED_ACE>() - mem::size_of::<u32>() + owner.len();
        let acl_size = mem::size_of::<ACL>() + ace_size;
        let mut acl = words_for(acl_size);
        // SAFETY: correctly sized aligned ACL buffer.
        if unsafe { InitializeAcl(acl.as_mut_ptr().cast(), acl_size as u32, ACL_REVISION) } == 0 {
            return Err(context(last_error(), "initialize owner-private DACL"));
        }
        let flags = if directory {
            CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE
        } else {
            0
        };
        // SAFETY: ACL has room for exactly one ACE.
        if unsafe {
            AddAccessAllowedAceEx(
                acl.as_mut_ptr().cast(),
                ACL_REVISION,
                flags,
                FILE_ALL_ACCESS,
                owner.as_psid(),
            )
        } == 0
        {
            return Err(context(last_error(), "add owner-private DACL ACE"));
        }
        let mut descriptor = SECURITY_DESCRIPTOR::default();
        // SAFETY: descriptor is valid writable storage.
        if unsafe {
            InitializeSecurityDescriptor(
                (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                SECURITY_DESCRIPTOR_REVISION,
            )
        } == 0
            || unsafe {
                SetSecurityDescriptorOwner(
                    (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                    owner.as_psid(),
                    0,
                )
            } == 0
            || unsafe {
                SetSecurityDescriptorDacl(
                    (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                    1,
                    acl.as_ptr().cast(),
                    0,
                )
            } == 0
            || unsafe {
                SetSecurityDescriptorControl(
                    (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                    SE_DACL_PROTECTED,
                    SE_DACL_PROTECTED,
                )
            } == 0
        {
            return Err(context(
                last_error(),
                "construct owner-private security descriptor",
            ));
        }
        Ok(Self {
            _owner: owner,
            _acl: acl,
            descriptor,
        })
    }
    fn security_attributes(&mut self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: (&mut self.descriptor as *mut SECURITY_DESCRIPTOR).cast(),
            bInheritHandle: 0,
        }
    }
}

fn verify_directory_handle(file: &fs::File, path: &Path, owner: &CurrentUserSid) -> io::Result<()> {
    let handle = kio_core::cas::windows_directory_handle_identity(file).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "private ledger directory is reparse or non-directory",
        )
    })?;
    if kio_core::cas::windows_real_directory_identity(path)? != Some(handle) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "private ledger directory changed while opening",
        ));
    }
    verify_owner_only_security(file, owner, AclShape::ExactCreated { directory: true })
}
fn verify_file_handle(file: &fs::File, path: &Path, owner: &CurrentUserSid) -> io::Result<()> {
    let handle = kio_core::cas::windows_regular_file_handle_identity(file).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "private ledger leaf is reparse, nonregular, or linked",
        )
    })?;
    if kio_core::cas::windows_real_regular_file_identity(path)? != Some(handle) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "private ledger leaf changed while opening",
        ));
    }
    verify_owner_only_security(file, owner, AclShape::ExactCreated { directory: false })
}

fn cleanup_pinned_private_dir(directory: &fs::File, path: &Path) -> io::Result<()> {
    let owner = CurrentUserSid::current()?;
    verify_directory_handle(directory, path, &owner)?;
    let leaves = private_dir_leaves(path)?;
    if leaves
        .iter()
        .any(|leaf| !PRIVATE_SQLITE_LEAVES.contains(&leaf.as_str()))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "private ledger snapshot has unexpected cleanup leaf",
        ));
    }
    for leaf in leaves {
        let file = open_private_leaf_for_delete(path, &leaf, &owner)?;
        mark_handle_delete(&file)?;
        drop(file);
    }
    if !private_dir_leaves(path)?.is_empty() {
        return Err(io::Error::other(
            "private ledger snapshot changed during cleanup",
        ));
    }
    mark_handle_delete(directory)
}
fn private_dir_leaves(path: &Path) -> io::Result<Vec<String>> {
    let mut leaves = Vec::new();
    for entry in fs::read_dir(path).map_err(|e| context(e, "enumerate private ledger snapshot"))? {
        let entry = entry.map_err(|e| context(e, "read private ledger snapshot leaf"))?;
        leaves.push(
            entry
                .file_name()
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "private ledger leaf is non-UTF8",
                    )
                })?,
        );
    }
    leaves.sort_unstable();
    Ok(leaves)
}
fn open_private_leaf_for_delete(
    directory: &Path,
    basename: &str,
    owner: &CurrentUserSid,
) -> io::Result<fs::File> {
    validate_basename(basename)?;
    let path = directory.join(basename);
    let wide = wide_path(&path)?;
    // SAFETY: directory remains pinned without delete sharing.
    let raw = unsafe {
        CreateFileW(
            wide.as_ptr(),
            DELETE | FILE_READ_ATTRIBUTES | READ_CONTROL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(context(last_error(), "open private ledger cleanup leaf"));
    }
    // SAFETY: CreateFileW returned an owned valid handle.
    let file = unsafe { fs::File::from_raw_handle(raw as _) };
    let handle = kio_core::cas::windows_regular_file_handle_identity(&file).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "private ledger cleanup leaf unsafe",
        )
    })?;
    if kio_core::cas::windows_real_regular_file_identity(&path)? != Some(handle) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "private ledger cleanup leaf changed",
        ));
    }
    verify_owner_only_security(&file, owner, AclShape::OwnerOnly)?;
    Ok(file)
}

fn open_private_leaf_for_verify(
    directory: &Path,
    basename: &str,
    owner: &CurrentUserSid,
) -> io::Result<fs::File> {
    validate_basename(basename)?;
    let path = directory.join(basename);
    let wide = wide_path(&path)?;
    // SAFETY: the private directory remains pinned while this fixed leaf is
    // opened. `FILE_FLAG_OPEN_REPARSE_POINT` prevents a reparse traversal.
    let raw = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES | READ_CONTROL,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(context(
            last_error(),
            "open private ledger snapshot verification leaf",
        ));
    }
    // SAFETY: CreateFileW returned an owned valid handle.
    let file = unsafe { fs::File::from_raw_handle(raw as _) };
    verify_file_handle(&file, &path, owner)?;
    Ok(file)
}
fn mark_handle_delete(file: &fs::File) -> io::Result<()> {
    let info = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    };
    // SAFETY: file owns DELETE access and info has Win32 layout.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as _,
            FileDispositionInfoEx,
            (&info as *const FILE_DISPOSITION_INFO_EX).cast(),
            mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    } == 0
    {
        return Err(context(last_error(), "mark private ledger handle delete"));
    }
    Ok(())
}

fn verify_owner_only_security(
    file: &fs::File,
    owner: &CurrentUserSid,
    shape: AclShape,
) -> io::Result<()> {
    let mut returned = ptr::null_mut();
    // SAFETY: file owns a valid handle; OS allocated descriptor is LocalFree'd below.
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut returned,
        )
    };
    if status != 0 || returned.is_null() {
        if !returned.is_null() {
            unsafe { LocalFree(returned as _) };
        }
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let result = verify_descriptor(returned, owner, shape);
    // SAFETY: descriptor returned by GetSecurityInfo.
    unsafe { LocalFree(returned as _) };
    result
}
fn verify_descriptor(
    descriptor: PSECURITY_DESCRIPTOR,
    owner: &CurrentUserSid,
    shape: AclShape,
) -> io::Result<()> {
    let mut actual_owner = ptr::null_mut();
    let mut owner_defaulted = 0;
    let mut dacl_present = 0;
    let mut dacl = ptr::null_mut();
    let mut dacl_defaulted = 0;
    let mut control = 0_u16;
    let mut revision = 0_u32;
    // SAFETY: descriptor was returned by the OS.
    if unsafe { GetSecurityDescriptorOwner(descriptor, &mut actual_owner, &mut owner_defaulted) }
        == 0
        || unsafe {
            GetSecurityDescriptorDacl(
                descriptor,
                &mut dacl_present,
                &mut dacl,
                &mut dacl_defaulted,
            )
        } == 0
        || unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0
    {
        return Err(context(
            last_error(),
            "inspect private ledger security descriptor",
        ));
    }
    let strict = matches!(shape, AclShape::ExactCreated { .. });
    if actual_owner.is_null()
        || unsafe { EqualSid(actual_owner, owner.as_psid()) } == 0
        || dacl_present == 0
        || dacl.is_null()
        || revision != SECURITY_DESCRIPTOR_REVISION
        || (strict
            && (owner_defaulted != 0 || dacl_defaulted != 0 || control & SE_DACL_PROTECTED == 0))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "private ledger DACL is not owner-only protected",
        ));
    }
    let mut info = ACL_SIZE_INFORMATION::default();
    // SAFETY: DACL is OS-owned; info has correct layout.
    if unsafe {
        GetAclInformation(
            dacl,
            (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
            mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            ACL_SIZE_INFORMATION_CLASS,
        )
    } == 0
    {
        return Err(context(last_error(), "inspect private ledger DACL"));
    }
    if info.AceCount != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "private ledger DACL has extra ACEs",
        ));
    }
    let mut ace = ptr::null_mut();
    // SAFETY: DACL has one ACE.
    if unsafe { GetAce(dacl, 0, &mut ace) } == 0 || ace.is_null() {
        return Err(context(last_error(), "read private ledger DACL ACE"));
    }
    let header = ace.cast::<ACE_HEADER>();
    let allowed = ace.cast::<ACCESS_ALLOWED_ACE>();
    // SAFETY: ACE layout verified by mask/size checks below.
    let flags = unsafe { (*header).AceFlags };
    let expected = match shape {
        AclShape::ExactCreated { directory: true } => {
            (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE) as u8
        }
        AclShape::ExactCreated { directory: false } => 0,
        AclShape::OwnerOnly => flags,
    };
    let owner_only = (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE | INHERITED_ACE) as u8;
    if unsafe { (*header).AceType } != ACCESS_ALLOWED_ACE_TYPE
        || flags != expected
        || (matches!(shape, AclShape::OwnerOnly) && flags & !owner_only != 0)
        || unsafe { (*allowed).Mask } != FILE_ALL_ACCESS
        || unsafe { (*header).AceSize as usize }
            != mem::size_of::<ACCESS_ALLOWED_ACE>() - mem::size_of::<u32>() + owner.len()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "private ledger DACL ACE shape unsafe",
        ));
    }
    // SAFETY: SID begins at SidStart and is bounded by the checked ACE size.
    let ace_sid = unsafe {
        (&(*allowed).SidStart as *const u32)
            .cast_mut()
            .cast::<c_void>()
    };
    if unsafe { EqualSid(ace_sid, owner.as_psid()) } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "private ledger DACL owner differs",
        ));
    }
    Ok(())
}

fn random_leaf() -> io::Result<String> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)
        .map_err(|e| io::Error::other(format!("sample ledger snapshot nonce: {e}")))?;
    let mut out = String::from("kio-ledger-snapshot-");
    for byte in random {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    Ok(out)
}
fn validate_basename(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ledger snapshot child must be fixed ASCII basename",
        ));
    }
    Ok(())
}
fn words_for(bytes: usize) -> Vec<usize> {
    vec![0; bytes.div_ceil(mem::size_of::<usize>())]
}
fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ledger snapshot path contains NUL",
        ));
    }
    wide.push(0);
    Ok(wide)
}
fn last_error() -> io::Error {
    io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
}
fn context(error: io::Error, operation: &str) -> io::Error {
    io::Error::new(error.kind(), format!("{operation}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_directory_and_fixed_leaf_are_owner_only() {
        let private = LedgerSnapshotPrivateDir::create().expect("private directory");
        let path = private.path().to_owned();
        let child = private.create_file("ledger.sqlite").expect("private leaf");
        let owner = CurrentUserSid::current().expect("token user");
        verify_directory_handle(private.handle(), private.path(), &owner)
            .expect("directory remains owner-only");
        verify_file_handle(&child, &path.join("ledger.sqlite"), &owner)
            .expect("leaf remains owner-only");
        drop(child);
        drop(private);
        assert!(!path.exists(), "normal cleanup removes the bound directory");
    }

    #[test]
    fn unexpected_private_leaf_blocks_cleanup() {
        let private = LedgerSnapshotPrivateDir::create().expect("private directory");
        let path = private.path().to_owned();
        let unexpected = private
            .create_file("unexpected.sqlite")
            .expect("unexpected private leaf");
        drop(unexpected);
        assert!(cleanup_pinned_private_dir(private.handle(), &path).is_err());
        assert!(path.exists(), "unknown leaf leaves directory intact");
        let owner = CurrentUserSid::current().expect("token user");
        let unexpected = open_private_leaf_for_delete(&path, "unexpected.sqlite", &owner)
            .expect("verified cleanup handle");
        mark_handle_delete(&unexpected).expect("mark test leaf delete");
        drop(unexpected);
        cleanup_pinned_private_dir(private.handle(), &path).expect("cleanup bound dir");
        drop(private);
        assert!(!path.exists());
    }

    #[test]
    fn fixed_child_names_reject_path_syntax() {
        for name in ["", ".", "..", "a/b", "a\\b", "a:b", "a\0b"] {
            assert!(validate_basename(name).is_err(), "{name:?}");
        }
        assert!(validate_basename("ledger.sqlite-wal").is_ok());
    }
}
