//! Windows-only owner-private storage for registry snapshots.
//!
//! This deliberately does not use `tempfile`: the system temporary directory
//! is shared, and applying an ACL after creating an ordinary directory leaves
//! an attacker-controlled interval.  The directory and every fixed-name leaf
//! are created with an owner-only protected DACL, then inspected by handle.

#![cfg(windows)]

use std::{
    ffi::c_void,
    fs, io, mem,
    os::windows::{ffi::OsStrExt, io::FromRawHandle},
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
const PRIVATE_SQLITE_LEAVES: [&str; 3] = [
    "snapshot.sqlite",
    "snapshot.sqlite-wal",
    "snapshot.sqlite-shm",
];

#[derive(Clone, Copy)]
enum AclShape {
    ExactCreated { directory: bool },
    OwnerOnly,
}

/// A private, handle-pinned directory for one SQLite registry snapshot.
///
/// The handle intentionally does not allow delete sharing.  It stays open for
/// the entire snapshot lifetime. Cleanup is attempted only through verified
/// child/directory handles while that directory remains pinned.
pub(crate) struct RegistrySnapshotPrivateDir {
    path: PathBuf,
    handle: Option<fs::File>,
}

impl RegistrySnapshotPrivateDir {
    /// Create a fresh directory below the process temporary root.  Both the
    /// directory name and its ACL are chosen before it becomes observable.
    pub(crate) fn create() -> io::Result<Self> {
        let temp_root = std::env::temp_dir();
        let owner = CurrentUserSid::current()?;

        for _ in 0..MAX_CREATE_ATTEMPTS {
            let path = temp_root.join(random_leaf()?);
            let mut descriptor = OwnerOnlyDescriptor::new(&owner, true)?;
            let attributes = descriptor.security_attributes();
            let wide = wide_path(&path)?;
            // SAFETY: the UTF-16 path and descriptor remain live for the call.
            if unsafe { CreateDirectoryW(wide.as_ptr(), &attributes) } == 0 {
                let error = last_error();
                if error.raw_os_error() == Some(ERROR_ALREADY_EXISTS as i32) {
                    continue;
                }
                return Err(context(
                    error,
                    "create owner-private registry snapshot directory",
                ));
            }

            match Self::open_and_verify(path.clone(), &owner) {
                Ok(handle) => {
                    return Ok(Self {
                        path,
                        handle: Some(handle),
                    });
                }
                Err(error) => {
                    // `open_and_verify` closes its no-delete-share handle
                    // before returning an error. A path-based cleanup here
                    // could therefore remove a replacement chosen at the
                    // shared temp-root path. Leave the untrusted result for
                    // OS/user cleanup instead of deleting an unbound path.
                    return Err(error);
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique owner-private registry snapshot directory",
        ))
    }

    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The pinned, no-delete-share directory handle. Callers use this only to
    /// retain the already-validated capability; they must not reopen `path`.
    pub(crate) fn handle(&self) -> &fs::File {
        self.handle
            .as_ref()
            .expect("private registry snapshot directory handle is present until drop")
    }

    /// Clone the pinned directory handle for a capability-based operation.
    pub(crate) fn capability(&self) -> io::Result<fs::File> {
        self.handle().try_clone()
    }

    /// Create one fixed-basename, owner-private regular file.  `CREATE_NEW`
    /// makes pre-existing leaves an error; callers must never overwrite a
    /// private snapshot leaf by path.
    pub(crate) fn create_file(&self, basename: &str) -> io::Result<fs::File> {
        validate_basename(basename)?;
        let owner = CurrentUserSid::current()?;
        let path = self.path.join(basename);
        let mut descriptor = OwnerOnlyDescriptor::new(&owner, false)?;
        let attributes = descriptor.security_attributes();
        let wide = wide_path(&path)?;
        // SAFETY: path and security descriptor outlive this call.  The result
        // is transferred exactly once to `File` below.
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
                "create owner-private registry snapshot leaf",
            ));
        }
        // SAFETY: `CreateFileW` returned an owned valid handle.
        let file = unsafe { fs::File::from_raw_handle(raw as _) };
        if let Err(error) = verify_file_handle(&file, &path, &owner) {
            // Do not close and unlink by path: after closing the handle, a
            // concurrent replacement at this leaf would be unbound from the
            // object we verified. The private directory may leak, but source
            // state and any replacement remain untouched.
            drop(file);
            return Err(error);
        }
        Ok(file)
    }

    fn open_and_verify(path: PathBuf, owner: &CurrentUserSid) -> io::Result<fs::File> {
        let wide = wide_path(&path)?;
        // SAFETY: path lives for the call.  No delete sharing pins the
        // directory while the snapshot is active.
        let raw = unsafe {
            CreateFileW(
                wide.as_ptr(),
                DELETE | FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | READ_CONTROL,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                3, // OPEN_EXISTING
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                ptr::null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(context(
                last_error(),
                "open owner-private registry snapshot directory",
            ));
        }
        // SAFETY: `CreateFileW` returned an owned valid handle.
        let file = unsafe { fs::File::from_raw_handle(raw as _) };
        verify_directory_handle(&file, &path, owner)?;
        Ok(file)
    }
}

impl Drop for RegistrySnapshotPrivateDir {
    fn drop(&mut self) {
        // Never remove by path after releasing this no-delete-share handle.
        // A failed validation or unexpected leaf intentionally leaks the
        // private directory rather than deleting a replacement.
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
        // SAFETY: current process pseudo-handle is valid; token is initialized
        // only on successful return.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(context(last_error(), "open current process token"));
        }
        let result = Self::from_token(token);
        // SAFETY: `OpenProcessToken` returned this handle.
        unsafe { CloseHandle(token) };
        result
    }

    fn from_token(token: HANDLE) -> io::Result<Self> {
        let mut needed = 0_u32;
        // SAFETY: size probe intentionally passes a null buffer.
        let probe =
            unsafe { GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut needed) };
        if probe != 0 || needed == 0 || unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER {
            return Err(context(last_error(), "size current process token user SID"));
        }
        let mut storage = words_for(needed as usize);
        // SAFETY: allocation is at least `needed` bytes and correctly aligned.
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
            return Err(context(last_error(), "read current process token user SID"));
        }
        let user = storage.as_ptr().cast::<TOKEN_USER>();
        // SAFETY: successful `TokenUser` query returned a TOKEN_USER buffer.
        let sid = unsafe { (*user).User.Sid };
        if sid.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "current process token has no user SID",
            ));
        }
        // SAFETY: SID is owned by `storage` and valid after successful query.
        let sid_len = unsafe { GetLengthSid(sid) } as usize;
        if sid_len == 0 || sid_len > needed as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "current process token returned invalid user SID",
            ));
        }
        let mut bytes = words_for(sid_len);
        // SAFETY: both spans are valid for sid_len bytes and non-overlapping.
        unsafe {
            ptr::copy_nonoverlapping(sid.cast::<u8>(), bytes.as_mut_ptr().cast::<u8>(), sid_len)
        };
        Ok(Self { bytes })
    }

    fn as_psid(&self) -> PSID {
        // Windows exposes `PSID` as a mutable opaque pointer even for APIs
        // that only read it. The allocation remains owned and immutable here.
        self.bytes.as_ptr().cast_mut().cast()
    }

    fn len(&self) -> usize {
        // SAFETY: this owns an initialized, validated SID.
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
        // SAFETY: ACL storage is aligned and exactly sized for its one ACE.
        if unsafe { InitializeAcl(acl.as_mut_ptr().cast(), acl_size as u32, ACL_REVISION) } == 0 {
            return Err(context(last_error(), "initialize owner-private DACL"));
        }
        let ace_flags = if directory {
            CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE
        } else {
            0
        };
        // SAFETY: initialized ACL has sufficient room for exactly this ACE.
        if unsafe {
            AddAccessAllowedAceEx(
                acl.as_mut_ptr().cast(),
                ACL_REVISION,
                ace_flags,
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
        {
            return Err(context(
                last_error(),
                "initialize owner-private security descriptor",
            ));
        }
        // SAFETY: descriptor, SID, and ACL stay live until Create* returns.
        if unsafe {
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
    let handle_identity =
        kio_core::cas::windows_directory_handle_identity(file).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "private registry snapshot directory is reparse or not a directory",
            )
        })?;
    let path_identity = kio_core::cas::windows_real_directory_identity(path)?;
    if path_identity != Some(handle_identity) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "private registry snapshot directory changed while opening",
        ));
    }
    verify_owner_only_security(file, owner, AclShape::ExactCreated { directory: true })
}

fn verify_file_handle(file: &fs::File, path: &Path, owner: &CurrentUserSid) -> io::Result<()> {
    let handle_identity =
        kio_core::cas::windows_regular_file_handle_identity(file).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "private registry snapshot leaf is reparse, non-regular, or linked",
            )
        })?;
    let path_identity = kio_core::cas::windows_real_regular_file_identity(path)?;
    if path_identity != Some(handle_identity) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "private registry snapshot leaf changed while opening",
        ));
    }
    verify_owner_only_security(file, owner, AclShape::ExactCreated { directory: false })
}

/// Delete only the exact objects opened through the still-pinned private
/// directory. No deletion in this routine is path-based: paths are used solely
/// to open an object, then the handle's no-follow identity/ACL is checked
/// before its delete disposition is set.
fn cleanup_pinned_private_dir(directory: &fs::File, path: &Path) -> io::Result<()> {
    let owner = CurrentUserSid::current()?;
    verify_directory_handle(directory, path, &owner)?;
    let first = private_dir_leaves(path)?;
    if first
        .iter()
        .any(|leaf| !PRIVATE_SQLITE_LEAVES.contains(&leaf.as_str()))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "owner-private registry snapshot has an unexpected cleanup leaf",
        ));
    }

    for leaf in first {
        let file = open_private_leaf_for_delete(path, &leaf, &owner)?;
        mark_handle_delete(&file)?;
        // Closing this exact marked handle completes leaf deletion before the
        // second enumeration. The directory remains pinned throughout.
        drop(file);
    }

    if !private_dir_leaves(path)?.is_empty() {
        return Err(io::Error::other(
            "owner-private registry snapshot changed during cleanup",
        ));
    }
    mark_handle_delete(directory)
}

fn private_dir_leaves(path: &Path) -> io::Result<Vec<String>> {
    let entries = fs::read_dir(path)
        .map_err(|error| context(error, "enumerate owner-private registry snapshot"))?;
    let mut leaves = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|error| context(error, "read owner-private registry snapshot leaf"))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "owner-private registry snapshot has a non-UTF-8 cleanup leaf",
            ));
        };
        leaves.push(name);
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
    // SAFETY: the directory itself remains pinned without delete sharing while
    // this path is opened. The returned raw handle is converted exactly once.
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
        return Err(context(
            last_error(),
            "open owner-private registry snapshot cleanup leaf",
        ));
    }
    // SAFETY: `CreateFileW` returned an owned valid handle.
    let file = unsafe { fs::File::from_raw_handle(raw as _) };
    let handle_identity =
        kio_core::cas::windows_regular_file_handle_identity(&file).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "owner-private registry snapshot cleanup leaf is unsafe",
            )
        })?;
    let path_identity = kio_core::cas::windows_real_regular_file_identity(&path)?;
    if path_identity != Some(handle_identity) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "owner-private registry snapshot cleanup leaf changed while opening",
        ));
    }
    verify_owner_only_security(&file, owner, AclShape::OwnerOnly)?;
    Ok(file)
}

fn mark_handle_delete(file: &fs::File) -> io::Result<()> {
    use std::{mem::size_of, os::windows::io::AsRawHandle};

    let info = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    };
    // SAFETY: `file` retains DELETE access and `info` has the exact Win32
    // layout expected by FileDispositionInfoEx.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as _,
            FileDispositionInfoEx,
            (&info as *const FILE_DISPOSITION_INFO_EX).cast(),
            size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    } == 0
    {
        return Err(context(
            last_error(),
            "mark owner-private registry snapshot handle delete",
        ));
    }
    Ok(())
}

fn verify_owner_only_security(
    file: &fs::File,
    owner: &CurrentUserSid,
    shape: AclShape,
) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;

    let mut returned = ptr::null_mut();
    // SAFETY: `file` owns a valid file handle. GetSecurityInfo allocates
    // `returned`, released by LocalFree below on every successful call.
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
            // SAFETY: GetSecurityInfo allocated this partial result too.
            unsafe { LocalFree(returned as _) };
        }
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let result = verify_descriptor(returned, owner, shape);
    // SAFETY: returned by GetSecurityInfo above.
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
    // SAFETY: descriptor is a valid self-relative descriptor returned by the OS.
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
            "inspect owner-private security descriptor",
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
            io::ErrorKind::PermissionDenied,
            "private registry snapshot ACL is not owner-only protected",
        ));
    }
    let mut info = ACL_SIZE_INFORMATION::default();
    // SAFETY: DACL was returned by the OS and output buffer is correctly sized.
    if unsafe {
        GetAclInformation(
            dacl,
            (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
            mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            ACL_SIZE_INFORMATION_CLASS,
        )
    } == 0
    {
        return Err(context(last_error(), "inspect owner-private DACL"));
    }
    if info.AceCount != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private registry snapshot DACL has extra ACEs",
        ));
    }
    let mut ace = ptr::null_mut();
    // SAFETY: DACL contains exactly one ACE according to the previous call.
    if unsafe { GetAce(dacl, 0, &mut ace) } == 0 || ace.is_null() {
        return Err(context(last_error(), "read owner-private DACL ACE"));
    }
    let header = ace.cast::<ACE_HEADER>();
    let allowed = ace.cast::<ACCESS_ALLOWED_ACE>();
    // SAFETY: ACE type/size are checked before interpreting SID bytes.
    let flags = unsafe { (*header).AceFlags };
    let expected_flags = match shape {
        AclShape::ExactCreated { directory: true } => {
            (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE) as u8
        }
        AclShape::ExactCreated { directory: false } => 0,
        // SQLite may create sidecars by inheriting this directory's ACE. It
        // remains owner-only, but the OS marks the resulting ACE inherited.
        AclShape::OwnerOnly => flags,
    };
    let owner_only_flags = (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE | INHERITED_ACE) as u8;
    if unsafe { (*header).AceType } != ACCESS_ALLOWED_ACE_TYPE
        || flags != expected_flags
        || (matches!(shape, AclShape::OwnerOnly) && flags & !owner_only_flags != 0)
        || unsafe { (*allowed).Mask } != FILE_ALL_ACCESS
        || unsafe { (*header).AceSize as usize }
            != mem::size_of::<ACCESS_ALLOWED_ACE>() - mem::size_of::<u32>() + owner.len()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private registry snapshot DACL ACE shape is unsafe",
        ));
    }
    // SAFETY: the ACE's variable SID starts at SidStart and has already been
    // bounded by the ACE size check above.
    let ace_sid = unsafe {
        (&(*allowed).SidStart as *const u32)
            .cast_mut()
            .cast::<c_void>()
    };
    if unsafe { EqualSid(ace_sid, owner.as_psid()) } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private registry snapshot DACL ACE owner differs from token user",
        ));
    }
    Ok(())
}

fn random_leaf() -> io::Result<String> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|error| {
        io::Error::other(format!("sample registry snapshot directory nonce: {error}"))
    })?;
    let mut out = String::from("kio-registry-snapshot-");
    for byte in random {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    Ok(out)
}

fn validate_basename(basename: &str) -> io::Result<()> {
    if basename.is_empty()
        || basename == "."
        || basename == ".."
        || !basename
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "registry snapshot child must be a fixed ASCII basename",
        ));
    }
    Ok(())
}

fn words_for(bytes: usize) -> Vec<usize> {
    let words = bytes.div_ceil(mem::size_of::<usize>());
    vec![0; words]
}

fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "registry snapshot path contains NUL",
        ));
    }
    wide.push(0);
    Ok(wide)
}

fn last_error() -> io::Error {
    // SAFETY: GetLastError reads the calling thread's last-error state.
    io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
}

fn context(error: io::Error, operation: &str) -> io::Error {
    io::Error::new(error.kind(), format!("{operation}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_directory_and_child_have_owner_only_shape() {
        let private = RegistrySnapshotPrivateDir::create().expect("private directory");
        let path = private.path().to_owned();
        let child = private
            .create_file("snapshot.sqlite")
            .expect("private leaf");
        let owner = CurrentUserSid::current().expect("token user");
        verify_directory_handle(
            private.handle.as_ref().expect("pinned directory"),
            private.path(),
            &owner,
        )
        .expect("directory remains owner-only");
        verify_file_handle(&child, &private.path().join("snapshot.sqlite"), &owner)
            .expect("leaf remains owner-only");
        drop(child);
        drop(private);
        assert!(
            !path.exists(),
            "normal private snapshot cleanup removes the bound directory"
        );
    }

    #[test]
    fn unexpected_private_leaf_blocks_cleanup_without_path_deletion() {
        let private = RegistrySnapshotPrivateDir::create().expect("private directory");
        let path = private.path().to_owned();
        let unexpected = private
            .create_file("unexpected.sqlite")
            .expect("unexpected private leaf");
        drop(unexpected);
        assert!(
            cleanup_pinned_private_dir(private.handle(), &path).is_err(),
            "unknown leaf must stop the cleanup before any recursive deletion"
        );
        assert!(
            path.exists(),
            "unknown leaf leaves the bound directory intact"
        );
        let owner = CurrentUserSid::current().expect("token user");
        let unexpected = open_private_leaf_for_delete(&path, "unexpected.sqlite", &owner)
            .expect("exact cleanup handle");
        mark_handle_delete(&unexpected).expect("mark unexpected test leaf delete");
        drop(unexpected);
        cleanup_pinned_private_dir(private.handle(), &path)
            .expect("cleanup after test leaf removal");
        drop(private);
        assert!(!path.exists(), "test cleanup removes the marked directory");
    }

    #[test]
    fn fixed_child_names_reject_path_syntax() {
        for name in ["", ".", "..", "a/b", "a\\b", "a:b", "a\0b"] {
            assert!(validate_basename(name).is_err(), "{name:?}");
        }
        assert!(validate_basename("snapshot.sqlite-wal").is_ok());
    }
}
