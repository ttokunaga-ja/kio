//! Owner-private Windows SQLite snapshot directories and ACL checks.

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
        AddAccessAllowedAce, AddAccessAllowedAceEx, CONTAINER_INHERIT_ACE,
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation, GetLengthSid,
        GetSecurityDescriptorControl, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner,
        GetTokenInformation, INHERITED_ACE, InitializeAcl, InitializeSecurityDescriptor,
        OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SetSecurityDescriptorControl,
        SetSecurityDescriptorDacl, SetSecurityDescriptorOwner, TOKEN_QUERY, TOKEN_USER, TokenUser,
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
const REGISTRY_LEAVES: [&str; 3] = [
    "snapshot.sqlite",
    "snapshot.sqlite-wal",
    "snapshot.sqlite-shm",
];

const LEDGER_LEAVES: [&str; 3] = ["ledger.sqlite", "ledger.sqlite-wal", "ledger.sqlite-shm"];

#[derive(Clone, Copy)]
enum SnapshotKind {
    Registry,
    Ledger,
}

impl SnapshotKind {
    fn name(self) -> &'static str {
        match self {
            Self::Registry => "registry",
            Self::Ledger => "ledger",
        }
    }

    fn directory_ace_flags(self) -> u8 {
        match self {
            Self::Registry => 0,
            Self::Ledger => (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE) as u8,
        }
    }

    fn leaves(self) -> &'static [&'static str; 3] {
        match self {
            Self::Registry => &REGISTRY_LEAVES,
            Self::Ledger => &LEDGER_LEAVES,
        }
    }

    fn acl_error_kind(self) -> io::ErrorKind {
        match self {
            Self::Registry => io::ErrorKind::PermissionDenied,
            Self::Ledger => io::ErrorKind::InvalidData,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum AclShape {
    ExactCreated(u8),
    OwnerOnly,
}

/// An owner-private SQLite snapshot directory pinned without delete sharing.
pub struct PrivateSnapshotDirectory {
    path: PathBuf,
    handle: fs::File,
    kind: SnapshotKind,
}

impl PrivateSnapshotDirectory {
    pub fn create_registry() -> io::Result<Self> {
        Self::create(SnapshotKind::Registry)
    }

    pub fn create_ledger() -> io::Result<Self> {
        Self::create(SnapshotKind::Ledger)
    }

    /// Create a fresh directory below the process temporary root.  Both the
    /// directory name and its ACL are chosen before it becomes observable.
    fn create(kind: SnapshotKind) -> io::Result<Self> {
        let temp_root = std::env::temp_dir();
        let owner = CurrentUserSid::current(SidProbe::RequireInsufficientBuffer)?;

        for _ in 0..MAX_CREATE_ATTEMPTS {
            let path = temp_root.join(random_leaf(kind)?);
            let mut descriptor =
                OwnerOnlyDescriptor::new(&owner, Some(kind.directory_ace_flags()))?;
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
                    &format!("create owner-private {} snapshot directory", kind.name()),
                ));
            }

            // On failure the pin has closed. A path-based cleanup in the shared
            // temporary root could delete a replacement.
            let handle = Self::open_and_verify(&path, &owner, kind)?;
            return Ok(Self { path, handle, kind });
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "could not allocate a unique owner-private {} snapshot directory",
                kind.name()
            ),
        ))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The pinned, no-delete-share directory handle. Callers use this only to
    /// retain the already-validated capability; they must not reopen `path`.
    pub fn handle(&self) -> &fs::File {
        &self.handle
    }

    /// Clone the pinned directory handle for a capability-based operation.
    pub fn capability(&self) -> io::Result<fs::File> {
        self.handle().try_clone()
    }

    /// Create one fixed-basename, owner-private regular file.  `CREATE_NEW`
    /// makes pre-existing leaves an error; callers must never overwrite a
    /// private snapshot leaf by path.
    pub fn create_file(&self, basename: &str) -> io::Result<fs::File> {
        validate_basename(basename)?;
        let owner = CurrentUserSid::current(SidProbe::RequireInsufficientBuffer)?;
        let path = self.path.join(basename);
        let mut descriptor = OwnerOnlyDescriptor::new(&owner, Some(0))?;
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
            return Err(context(last_error(), "create owner-private snapshot leaf"));
        }
        // SAFETY: `CreateFileW` returned an owned valid handle.
        let file = unsafe { fs::File::from_raw_handle(raw as _) };
        // Verification failure only closes the handle. The path could name
        // a replacement by the time a pathname-based cleanup ran.
        verify_file_handle(
            &file,
            &path,
            &owner,
            AclShape::ExactCreated(0),
            self.kind.acl_error_kind(),
        )?;
        Ok(file)
    }

    /// Check the exact private shape immediately before SQLite receives the
    /// private main path.  A source WAL is copied only when it was present;
    /// SHM must still be absent here because SQLite, not the source, may
    /// create a private SHM after this check.
    pub fn verify_before_sqlite(&self, has_wal: bool) -> io::Result<()> {
        let owner = CurrentUserSid::current(SidProbe::RequireInsufficientBuffer)?;
        verify_directory_handle(self.handle(), self.path(), &owner, self.kind)?;
        let expected = &LEDGER_LEAVES[..1 + usize::from(has_wal)];
        if private_dir_leaves(&self.path)? != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "owner-private ledger snapshot has an unexpected pre-SQLite leaf",
            ));
        }
        for leaf in expected {
            open_private_leaf(&self.path, leaf, &owner, LeafOpen::Verify, self.kind)?;
        }
        Ok(())
    }

    fn open_and_verify(
        path: &Path,
        owner: &CurrentUserSid,
        kind: SnapshotKind,
    ) -> io::Result<fs::File> {
        let wide = wide_path(path)?;
        // SAFETY: path lives for the call.  No delete sharing pins the
        // directory while the snapshot is active.
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
                &format!("open owner-private {} snapshot directory", kind.name()),
            ));
        }
        // SAFETY: `CreateFileW` returned an owned valid handle.
        let file = unsafe { fs::File::from_raw_handle(raw as _) };
        verify_directory_handle(&file, path, owner, kind)?;
        Ok(file)
    }
}

impl Drop for PrivateSnapshotDirectory {
    fn drop(&mut self) {
        // Never remove by path after releasing this no-delete-share handle.
        // A failed validation or unexpected leaf intentionally leaks the
        // private directory rather than deleting a replacement.
        let _ = cleanup_pinned_private_dir(&self.handle, &self.path, self.kind);
    }
}

#[derive(Clone, Copy)]
pub(super) enum SidProbe {
    RequireInsufficientBuffer,
    SizeOnly,
}

pub(super) struct CurrentUserSid {
    bytes: Vec<usize>,
}

impl CurrentUserSid {
    pub(super) fn current(probe_policy: SidProbe) -> io::Result<Self> {
        let mut token = ptr::null_mut();
        // SAFETY: current process pseudo-handle is valid; token is initialized
        // only on successful return.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(token_error(probe_policy, "open current process token"));
        }
        let result = Self::from_token(token, probe_policy);
        // SAFETY: `OpenProcessToken` returned this handle.
        unsafe { CloseHandle(token) };
        result
    }

    fn from_token(token: HANDLE, probe_policy: SidProbe) -> io::Result<Self> {
        let mut needed = 0_u32;
        // SAFETY: size probe intentionally passes a null buffer.
        let probe =
            unsafe { GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut needed) };
        if needed == 0
            || (matches!(probe_policy, SidProbe::RequireInsufficientBuffer)
                && (probe != 0 || unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER))
        {
            return Err(token_error(
                probe_policy,
                "size current process token user SID",
            ));
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
            return Err(token_error(
                probe_policy,
                "read current process token user SID",
            ));
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

    pub(super) fn as_psid(&self) -> PSID {
        // Windows exposes `PSID` as a mutable opaque pointer even for APIs
        // that only read it. The allocation remains owned and immutable here.
        self.bytes.as_ptr().cast_mut().cast()
    }

    pub(super) fn len(&self) -> usize {
        // SAFETY: this owns an initialized, validated SID.
        unsafe { GetLengthSid(self.as_psid()) as usize }
    }
}

fn token_error(probe_policy: SidProbe, operation: &str) -> io::Error {
    let error = io::Error::last_os_error();
    if matches!(probe_policy, SidProbe::RequireInsufficientBuffer) {
        context(error, operation)
    } else {
        error
    }
}

pub(super) struct OwnerOnlyDescriptor<'a> {
    _owner: &'a CurrentUserSid,
    _acl: Vec<usize>,
    descriptor: SECURITY_DESCRIPTOR,
}

impl<'a> OwnerOnlyDescriptor<'a> {
    /// Snapshot creation specifies ACE flags; ordinary private objects use
    /// the non-Ex Win32 call and propagate its raw OS errors.
    pub(super) fn new(owner: &'a CurrentUserSid, snapshot_flags: Option<u8>) -> io::Result<Self> {
        let error = |operation| {
            let error = last_error();
            if snapshot_flags.is_some() {
                context(error, operation)
            } else {
                error
            }
        };
        let ace_size = mem::size_of::<ACCESS_ALLOWED_ACE>() - mem::size_of::<u32>() + owner.len();
        let acl_size = mem::size_of::<ACL>() + ace_size;
        let mut acl = words_for(acl_size);
        let acl_ptr = acl.as_mut_ptr().cast();
        // SAFETY: aligned ACL storage has sufficient space for one ACE.
        if unsafe { InitializeAcl(acl_ptr, acl_size as u32, ACL_REVISION) } == 0 {
            return Err(error("initialize owner-private DACL"));
        }
        // SAFETY: the initialized ACL has room for the ACE and SID.
        let added = unsafe {
            match snapshot_flags {
                Some(flags) => AddAccessAllowedAceEx(
                    acl_ptr,
                    ACL_REVISION,
                    u32::from(flags),
                    FILE_ALL_ACCESS,
                    owner.as_psid(),
                ),
                None => {
                    AddAccessAllowedAce(acl_ptr, ACL_REVISION, FILE_ALL_ACCESS, owner.as_psid())
                }
            }
        };
        if added == 0 {
            return Err(error("add owner-private DACL ACE"));
        }
        let mut descriptor = SECURITY_DESCRIPTOR::default();
        let raw = (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast();
        // SAFETY: descriptor is writable and the referenced SID and ACL outlive it.
        if unsafe { InitializeSecurityDescriptor(raw, SECURITY_DESCRIPTOR_REVISION) } == 0 {
            return Err(error("initialize owner-private security descriptor"));
        }
        if unsafe { SetSecurityDescriptorOwner(raw, owner.as_psid(), 0) } == 0
            || unsafe { SetSecurityDescriptorDacl(raw, 1, acl_ptr, 0) } == 0
            || unsafe { SetSecurityDescriptorControl(raw, SE_DACL_PROTECTED, SE_DACL_PROTECTED) }
                == 0
        {
            return Err(error("construct owner-private security descriptor"));
        }
        Ok(Self {
            _owner: owner,
            _acl: acl,
            descriptor,
        })
    }

    pub(super) fn as_raw(&mut self) -> PSECURITY_DESCRIPTOR {
        (&mut self.descriptor as *mut SECURITY_DESCRIPTOR).cast()
    }

    fn security_attributes(&mut self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.as_raw(),
            bInheritHandle: 0,
        }
    }
}

fn verify_directory_handle(
    file: &fs::File,
    path: &Path,
    owner: &CurrentUserSid,
    kind: SnapshotKind,
) -> io::Result<()> {
    let handle_identity = crate::platform::directory_handle_identity(file).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "private snapshot directory is reparse or not a directory",
        )
    })?;
    let path_identity = crate::platform::real_directory_identity(path)?;
    if path_identity != Some(handle_identity) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "private snapshot directory changed while opening",
        ));
    }
    verify_owner_only_security(
        file,
        owner,
        AclShape::ExactCreated(kind.directory_ace_flags()),
        kind.acl_error_kind(),
    )
}

fn verify_file_handle(
    file: &fs::File,
    path: &Path,
    owner: &CurrentUserSid,
    shape: AclShape,
    error_kind: io::ErrorKind,
) -> io::Result<()> {
    let handle_identity = crate::platform::regular_file_handle_identity(file).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "private snapshot leaf is reparse, non-regular, or linked",
        )
    })?;
    let path_identity = crate::platform::real_regular_file_identity(path)?;
    if path_identity != Some(handle_identity) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "private snapshot leaf changed while opening",
        ));
    }
    verify_owner_only_security(file, owner, shape, error_kind)
}

/// Delete only the exact objects opened through the still-pinned private
/// directory. No deletion in this routine is path-based: paths are used solely
/// to open an object, then the handle's no-follow identity/ACL is checked
/// before its delete disposition is set.
fn cleanup_pinned_private_dir(
    directory: &fs::File,
    path: &Path,
    kind: SnapshotKind,
) -> io::Result<()> {
    let owner = CurrentUserSid::current(SidProbe::RequireInsufficientBuffer)?;
    verify_directory_handle(directory, path, &owner, kind)?;
    let first = private_dir_leaves(path)?;
    if first
        .iter()
        .any(|leaf| !kind.leaves().contains(&leaf.as_str()))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "owner-private snapshot has an unexpected cleanup leaf",
        ));
    }

    for leaf in first {
        let file = open_private_leaf(path, &leaf, &owner, LeafOpen::Delete, kind)?;
        mark_handle_delete(&file)?;
        // Closing this exact marked handle completes leaf deletion before the
        // second enumeration. The directory remains pinned throughout.
        drop(file);
    }

    if !private_dir_leaves(path)?.is_empty() {
        return Err(io::Error::other(
            "owner-private snapshot changed during cleanup",
        ));
    }
    mark_handle_delete(directory)
}

fn private_dir_leaves(path: &Path) -> io::Result<Vec<String>> {
    let entries =
        fs::read_dir(path).map_err(|error| context(error, "enumerate owner-private snapshot"))?;
    let mut leaves = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| context(error, "read owner-private snapshot leaf"))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "owner-private snapshot has a non-UTF-8 cleanup leaf",
            ));
        };
        leaves.push(name);
    }
    leaves.sort_unstable();
    Ok(leaves)
}

enum LeafOpen {
    Delete,
    Verify,
}

fn open_private_leaf(
    directory: &Path,
    basename: &str,
    owner: &CurrentUserSid,
    mode: LeafOpen,
    kind: SnapshotKind,
) -> io::Result<fs::File> {
    validate_basename(basename)?;
    let path = directory.join(basename);
    let wide = wide_path(&path)?;
    let (access, sharing, shape) = match mode {
        LeafOpen::Delete => (DELETE, FILE_SHARE_DELETE, AclShape::OwnerOnly),
        LeafOpen::Verify => (0, 0, AclShape::ExactCreated(0)),
    };
    // SAFETY: the directory is pinned and the path lives through the call.
    let raw = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access | FILE_READ_ATTRIBUTES | READ_CONTROL,
            sharing | FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(context(last_error(), "open owner-private snapshot leaf"));
    }
    // SAFETY: CreateFileW returned an owned valid handle.
    let file = unsafe { fs::File::from_raw_handle(raw as _) };
    verify_file_handle(&file, &path, owner, shape, kind.acl_error_kind())?;
    Ok(file)
}

fn mark_handle_delete(file: &fs::File) -> io::Result<()> {
    use std::mem::size_of;

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
            "mark owner-private snapshot handle delete",
        ));
    }
    Ok(())
}

fn verify_owner_only_security(
    file: &fs::File,
    owner: &CurrentUserSid,
    shape: AclShape,
    error_kind: io::ErrorKind,
) -> io::Result<()> {
    verify_descriptor(&SecurityDescriptor::read(file)?, owner, shape, error_kind)
}

pub(super) struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    pub(super) fn read(file: &fs::File) -> io::Result<Self> {
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
        Ok(Self(returned))
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: the descriptor was allocated by GetSecurityInfo.
        unsafe { LocalFree(self.0 as _) };
    }
}

pub(super) fn verify_descriptor(
    descriptor: &SecurityDescriptor,
    owner: &CurrentUserSid,
    shape: AclShape,
    error_kind: io::ErrorKind,
) -> io::Result<()> {
    let descriptor = descriptor.0;
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
    let strict = matches!(shape, AclShape::ExactCreated(_));
    if actual_owner.is_null()
        || unsafe { EqualSid(actual_owner, owner.as_psid()) } == 0
        || dacl_present == 0
        || dacl.is_null()
        || revision != SECURITY_DESCRIPTOR_REVISION
        || (strict
            && (owner_defaulted != 0 || dacl_defaulted != 0 || control & SE_DACL_PROTECTED == 0))
    {
        return Err(io::Error::new(
            error_kind,
            "private snapshot ACL is not owner-only protected",
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
            error_kind,
            "private snapshot DACL has extra ACEs",
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
    let valid_flags = match shape {
        AclShape::ExactCreated(expected) => flags == expected,
        // Cleanup also accepts SQLite sidecars with inherited owner-only ACEs.
        AclShape::OwnerOnly => {
            flags & !((CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE | INHERITED_ACE) as u8) == 0
        }
    };
    if unsafe { (*header).AceType } != ACCESS_ALLOWED_ACE_TYPE
        || !valid_flags
        || unsafe { (*allowed).Mask } != FILE_ALL_ACCESS
        || unsafe { (*header).AceSize as usize }
            != mem::size_of::<ACCESS_ALLOWED_ACE>() - mem::size_of::<u32>() + owner.len()
    {
        return Err(io::Error::new(
            error_kind,
            "private snapshot DACL ACE shape is unsafe",
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
            error_kind,
            "private snapshot DACL ACE owner differs from token user",
        ));
    }
    Ok(())
}

fn random_leaf(kind: SnapshotKind) -> io::Result<String> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|error| {
        io::Error::other(format!(
            "sample {} snapshot directory nonce: {error}",
            kind.name()
        ))
    })?;
    let mut out = format!("kio-{}-snapshot-", kind.name());
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
            "snapshot child must be a fixed ASCII basename",
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
            "snapshot path contains NUL",
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
        let private = PrivateSnapshotDirectory::create_registry().expect("private directory");
        let path = private.path().to_owned();
        let child = private
            .create_file("snapshot.sqlite")
            .expect("private leaf");
        let owner =
            CurrentUserSid::current(SidProbe::RequireInsufficientBuffer).expect("token user");
        verify_directory_handle(private.handle(), private.path(), &owner, private.kind)
            .expect("directory remains owner-only");
        verify_file_handle(
            &child,
            &private.path().join("snapshot.sqlite"),
            &owner,
            AclShape::ExactCreated(0),
            private.kind.acl_error_kind(),
        )
        .expect("leaf remains owner-only");
        let retained = crate::store_dir::StoreDirectory::from_retained(
            private.capability().expect("retained snapshot directory"),
            private.path().to_path_buf(),
        )
        .expect("ordinary retained snapshot directory");
        crate::private_fs::verify_private_directory_handle(&retained)
            .expect("snapshot directory matches common private-object policy");
        crate::private_fs::verify_private_regular_file_handle(&child)
            .expect("snapshot leaf matches common private-object policy");
        drop(retained);
        drop(child);
        drop(private);
        assert!(
            !path.exists(),
            "normal private snapshot cleanup removes the bound directory"
        );
    }

    #[test]
    fn unexpected_private_leaf_blocks_cleanup_without_path_deletion() {
        let private = PrivateSnapshotDirectory::create_registry().expect("private directory");
        let path = private.path().to_owned();
        let unexpected = private
            .create_file("unexpected.sqlite")
            .expect("unexpected private leaf");
        drop(unexpected);
        assert!(
            cleanup_pinned_private_dir(private.handle(), &path, private.kind).is_err(),
            "unknown leaf must stop the cleanup before any recursive deletion"
        );
        assert!(
            path.exists(),
            "unknown leaf leaves the bound directory intact"
        );
        let owner =
            CurrentUserSid::current(SidProbe::RequireInsufficientBuffer).expect("token user");
        let unexpected = open_private_leaf(
            &path,
            "unexpected.sqlite",
            &owner,
            LeafOpen::Delete,
            private.kind,
        )
        .expect("exact cleanup handle");
        mark_handle_delete(&unexpected).expect("mark unexpected test leaf delete");
        drop(unexpected);
        cleanup_pinned_private_dir(private.handle(), &path, private.kind)
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

#[cfg(test)]
mod ledger_tests {
    use super::*;

    fn create_stage(private: &PrivateSnapshotDirectory, name: &str) -> fs::File {
        let directory = crate::store_dir::StoreDirectory::from_retained(
            private.handle().try_clone().unwrap(),
            private.path().to_owned(),
        )
        .unwrap();
        let writer = crate::store_dir::StageWriter::create(&directory, Path::new(name)).unwrap();
        writer.sync().unwrap();
        drop(writer);
        open_private_leaf(
            private.path(),
            name,
            &CurrentUserSid::current(SidProbe::RequireInsufficientBuffer).unwrap(),
            LeafOpen::Verify,
            private.kind,
        )
        .unwrap()
    }

    #[test]
    fn private_directory_and_fixed_leaf_are_owner_only() {
        let private = PrivateSnapshotDirectory::create_ledger().expect("private directory");
        let path = private.path().to_owned();
        let child = create_stage(&private, "ledger.sqlite");
        let owner =
            CurrentUserSid::current(SidProbe::RequireInsufficientBuffer).expect("token user");
        verify_directory_handle(private.handle(), private.path(), &owner, private.kind)
            .expect("directory remains owner-only");
        verify_file_handle(
            &child,
            &path.join("ledger.sqlite"),
            &owner,
            AclShape::ExactCreated(0),
            private.kind.acl_error_kind(),
        )
        .expect("leaf remains owner-only");
        drop(child);
        drop(private);
        assert!(!path.exists(), "normal cleanup removes the bound directory");
    }

    #[test]
    fn unexpected_private_leaf_blocks_cleanup() {
        let private = PrivateSnapshotDirectory::create_ledger().expect("private directory");
        let path = private.path().to_owned();
        let unexpected = create_stage(&private, "unexpected.sqlite");
        drop(unexpected);
        assert!(cleanup_pinned_private_dir(private.handle(), &path, private.kind).is_err());
        assert!(path.exists(), "unknown leaf leaves directory intact");
        let owner =
            CurrentUserSid::current(SidProbe::RequireInsufficientBuffer).expect("token user");
        let unexpected = open_private_leaf(
            &path,
            "unexpected.sqlite",
            &owner,
            LeafOpen::Delete,
            private.kind,
        )
        .expect("verified cleanup handle");
        mark_handle_delete(&unexpected).expect("mark test leaf delete");
        drop(unexpected);
        cleanup_pinned_private_dir(private.handle(), &path, private.kind)
            .expect("cleanup bound dir");
        drop(private);
        assert!(!path.exists());
    }
}
