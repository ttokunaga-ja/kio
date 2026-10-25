use std::{
    ffi::{OsStr, c_void},
    fs::File,
    io::Read,
    mem,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle},
    },
    path::{Component, Path, PathBuf, Prefix},
    ptr,
};

use cap_primitives::{ambient_authority, fs as cap_fs};
use windows_sys::Wdk::{
    Foundation::OBJECT_ATTRIBUTES,
    Storage::FileSystem::{
        FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN_REPARSE_POINT,
        FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
    },
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE, LocalFree, OBJ_CASE_INSENSITIVE,
        RtlNtStatusToDosError, UNICODE_STRING,
    },
    Security::Authorization::{ConvertStringSidToSidW, GetSecurityInfo, SE_FILE_OBJECT},
    Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_REVISION, ACL_SIZE_INFORMATION,
        AddAccessAllowedAce, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
        GetLengthSid, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
        GetSecurityDescriptorOwner, GetTokenInformation, INHERIT_ONLY_ACE, InitializeAcl,
        InitializeSecurityDescriptor, IsValidSid, IsWellKnownSid, OWNER_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, SECURITY_DESCRIPTOR,
        SetSecurityDescriptorControl, SetSecurityDescriptorDacl, SetSecurityDescriptorOwner,
        TOKEN_QUERY, TOKEN_USER, TokenUser, WinBuiltinAdministratorsSid, WinLocalSystemSid,
    },
    Storage::FileSystem::{
        FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, READ_CONTROL,
        SYNCHRONIZE,
    },
    System::{
        IO::IO_STATUS_BLOCK,
        Threading::{GetCurrentProcess, OpenProcessToken},
        WindowsProgramming::FILE_CREATED,
    },
};

use crate::Result;
use crate::private_fs::{oversized_file, raced_file, unsafe_file};
use crate::store_dir::StoreDirectory;

const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const ACL_SIZE_INFORMATION_CLASS: i32 = 2;
const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
const FILE_ALL_ACCESS: u32 = 0x001f_01ff;
const ACCESS_DENIED_ACE_TYPE: u8 = 1;
const GENERIC_ALL: u32 = 0x1000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const MAXIMUM_ALLOWED: u32 = 0x0200_0000;
const DELETE: u32 = 0x0001_0000;
const WRITE_DAC: u32 = 0x0004_0000;
const WRITE_OWNER: u32 = 0x0008_0000;
const FILE_WRITE_DATA: u32 = 0x0000_0002;
const FILE_APPEND_DATA: u32 = 0x0000_0004;
const FILE_WRITE_EA: u32 = 0x0000_0010;
const FILE_DELETE_CHILD: u32 = 0x0000_0040;
const FILE_WRITE_ATTRIBUTES: u32 = 0x0000_0100;

/// Create one direct child beneath a retained Windows directory with its
/// current-user owner and protected owner-only DACL installed by the kernel at
/// creation time. This never adopts or repairs an existing name.
#[derive(Clone, Copy)]
pub(crate) enum NewPrivateObjectKind {
    Directory,
    RegularFile,
    LockFile,
}

pub(crate) fn create_new_private_at(
    parent: &File,
    leaf: &OsStr,
    kind: NewPrivateObjectKind,
) -> std::io::Result<File> {
    if crate::platform::directory_handle_identity(parent).is_none() {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    let name: Vec<u16> = leaf.encode_wide().collect();
    if name.is_empty()
        || name == [b'.' as u16]
        || name == [b'.' as u16, b'.' as u16]
        || name.len().saturating_mul(mem::size_of::<u16>()) > u16::MAX as usize
        || name.iter().any(|unit| {
            *unit == 0 || *unit == b'\\' as u16 || *unit == b'/' as u16 || *unit == b':' as u16
        })
    {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    let owner = CurrentUserSid::current()?;
    let (_acl, acl_ptr) = private_acl(&owner)?;
    let mut descriptor = SECURITY_DESCRIPTOR::default();
    let descriptor_ptr: PSECURITY_DESCRIPTOR = (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast();
    // SAFETY: `descriptor_ptr` is the Windows binding's erased mutable
    // pointer to the absolute descriptor. It, the aligned ACL, and the
    // owner SID remain live through NtCreateFile below.
    if unsafe { InitializeSecurityDescriptor(descriptor_ptr, SECURITY_DESCRIPTOR_REVISION) } == 0
        || unsafe { SetSecurityDescriptorOwner(descriptor_ptr, owner.as_psid(), 0) } == 0
        || unsafe { SetSecurityDescriptorDacl(descriptor_ptr, 1, acl_ptr, 0) } == 0
        || unsafe {
            SetSecurityDescriptorControl(descriptor_ptr, SE_DACL_PROTECTED, SE_DACL_PROTECTED)
        } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let mut unicode = UNICODE_STRING {
        Length: (name.len() * mem::size_of::<u16>()) as u16,
        MaximumLength: (name.len() * mem::size_of::<u16>()) as u16,
        Buffer: name.as_ptr().cast_mut(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw_handle() as HANDLE,
        ObjectName: &mut unicode,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: &descriptor,
        SecurityQualityOfService: ptr::null(),
    };
    let (desired_access, file_attributes, create_options) = match kind {
        NewPrivateObjectKind::Directory => (
            FILE_LIST_DIRECTORY
                | FILE_TRAVERSE
                | FILE_ADD_FILE
                | FILE_ADD_SUBDIRECTORY
                | READ_CONTROL
                | FILE_READ_ATTRIBUTES
                | SYNCHRONIZE,
            windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY,
            FILE_SYNCHRONOUS_IO_NONALERT | FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
        ),
        NewPrivateObjectKind::RegularFile => (
            GENERIC_READ | GENERIC_WRITE | SYNCHRONIZE | READ_CONTROL | FILE_READ_ATTRIBUTES,
            windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL,
            FILE_SYNCHRONOUS_IO_NONALERT | FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
        ),
        NewPrivateObjectKind::LockFile => (
            GENERIC_READ
                | GENERIC_WRITE
                | windows_sys::Win32::Storage::FileSystem::DELETE
                | SYNCHRONIZE
                | READ_CONTROL
                | FILE_READ_ATTRIBUTES,
            windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL,
            FILE_SYNCHRONOUS_IO_NONALERT | FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
        ),
    };
    let mut handle = INVALID_HANDLE_VALUE;
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: every input buffer above remains live for this synchronous
    // call, the root is a retained directory handle, and FILE_CREATE never
    // opens an existing leaf.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            desired_access,
            &attributes,
            &mut status_block,
            ptr::null_mut(),
            file_attributes,
            match kind {
                NewPrivateObjectKind::LockFile => FILE_SHARE_READ,
                _ => FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            },
            FILE_CREATE,
            create_options,
            ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
            // SAFETY: a failing NtCreateFile must not leak a returned handle.
            unsafe { CloseHandle(handle) };
        }
        return Err(std::io::Error::from_raw_os_error(unsafe {
            RtlNtStatusToDosError(status) as i32
        }));
    }
    if handle.is_null()
        || handle == INVALID_HANDLE_VALUE
        || status_block.Information != FILE_CREATED as usize
    {
        if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
            // SAFETY: NtCreateFile returned this owned handle despite an
            // unexpected completion result.
            unsafe { CloseHandle(handle) };
        }
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
    }
    // SAFETY: successful NtCreateFile returned one owned retained handle.
    let file = unsafe { File::from_raw_handle(handle as _) };
    let valid = match kind {
        NewPrivateObjectKind::Directory => {
            crate::platform::directory_handle_identity(&file).is_some()
        }
        NewPrivateObjectKind::RegularFile | NewPrivateObjectKind::LockFile => {
            crate::platform::regular_file_handle_identity(&file).is_some()
        }
    };
    if !valid {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
    }
    verify_owner_only(&file, &owner, Path::new("<retained>"))
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidData))?;
    Ok(file)
}

fn private_acl(owner: &CurrentUserSid) -> std::io::Result<(Vec<u32>, *mut ACL)> {
    let bytes = mem::size_of::<ACL>()
        .saturating_add(mem::size_of::<ACCESS_ALLOWED_ACE>() - mem::size_of::<u32>())
        .saturating_add(owner.len());
    let mut acl = vec![0_u32; bytes.div_ceil(mem::size_of::<u32>())];
    let acl_ptr = acl.as_mut_ptr().cast::<ACL>();
    // SAFETY: DWORD-aligned storage has at least `bytes` bytes for the
    // ACL and owner SID, and both remain live for the caller's use.
    if unsafe { InitializeAcl(acl_ptr, bytes as u32, ACL_REVISION) } == 0
        || unsafe { AddAccessAllowedAce(acl_ptr, ACL_REVISION, FILE_ALL_ACCESS, owner.as_psid()) }
            == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok((acl, acl_ptr))
}

/// Verify a retained Windows handle's owner-private ACL before another crate
/// creates operational artifacts beneath it. This never repairs permissions.
pub fn verify_owner_private_handle(file: &File) -> Result<()> {
    let owner = CurrentUserSid::current()
        .map_err(|_| unsafe_file("cannot determine current Windows user", None))?;
    verify_owner_only(file, &owner, Path::new("<retained>"))
}

/// Require that a retained Windows file or directory belongs to the current
/// user without changing its security descriptor.
pub(crate) fn verify_current_owner_handle(file: &File) -> Result<()> {
    let owner = CurrentUserSid::current()
        .map_err(|_| unsafe_file("cannot determine current Windows user", None))?;
    verify_current_owner(file, &owner, Path::new("<retained>"))
}

pub(crate) fn verify_private_directory_handle(directory: &StoreDirectory) -> Result<()> {
    let handle = directory.root_handle();
    verify_real_directory(&handle, directory.path())?;
    let owner = CurrentUserSid::current().map_err(|_| {
        unsafe_file(
            "cannot determine current Windows user",
            Some(directory.path()),
        )
    })?;
    verify_owner_only(&handle, &owner, directory.path())
}

pub(crate) fn verify_private_directory(
    path: &Path,
    control: crate::control::ControlHandle,
) -> Result<StoreDirectory> {
    let (root_path, components) = windows_absolute_components(path)?;
    let owner = CurrentUserSid::current()
        .map_err(|_| unsafe_file("cannot determine current Windows user", Some(path)))?;
    let root = cap_fs::open_ambient_dir(&root_path, ambient_authority())
        .map_err(|_| unsafe_file("cannot open Windows filesystem root", Some(path)))?;
    verify_real_directory(&root, path)?;
    let mut directory = root;
    for component in &components {
        let next = cap_fs::open_dir_nofollow(&directory, Path::new(component)).map_err(|_| {
            unsafe_file(
                "private directory has a missing, reparse, or unsafe component",
                Some(path),
            )
        })?;
        verify_real_directory(&next, path)?;
        directory = next;
    }
    verify_owner_only(&directory, &owner, path)?;
    StoreDirectory::from_retained_with_control(directory, path.to_path_buf(), control)
}

pub(crate) fn verify_private_creation_parent(
    path: &Path,
    control: crate::control::ControlHandle,
) -> Result<StoreDirectory> {
    let (root_path, components) = windows_absolute_components(path)?;
    let root = cap_fs::open_ambient_dir(&root_path, ambient_authority())
        .map_err(|_| unsafe_file("cannot open Windows filesystem root", Some(path)))?;
    verify_real_directory(&root, path)?;
    let mut directory = root;
    for component in &components {
        let next = cap_fs::open_dir_nofollow(&directory, Path::new(component)).map_err(|_| {
            unsafe_file(
                "private directory creation parent has a missing, reparse, or unsafe component",
                Some(path),
            )
        })?;
        verify_real_directory(&next, path)?;
        directory = next;
    }
    StoreDirectory::from_retained_with_control(directory, path.to_path_buf(), control)
}

pub(crate) fn read_private_file(
    path: &Path,
    max_bytes: u64,
    _control: crate::control::ControlHandle,
) -> Result<Vec<u8>> {
    let (root_path, components) = windows_absolute_components(path)?;
    let owner = CurrentUserSid::current()
        .map_err(|_| unsafe_file("cannot determine current Windows user", Some(path)))?;
    let root = cap_fs::open_ambient_dir(&root_path, ambient_authority())
        .map_err(|_| unsafe_file("cannot open Windows filesystem root", Some(path)))?;
    verify_real_directory(&root, path)?;
    let mut parent = root;
    for component in &components[..components.len() - 1] {
        let next = cap_fs::open_dir_nofollow(&parent, Path::new(component)).map_err(|_| {
            unsafe_file(
                "private trust file has a missing, reparse, or unsafe ancestor",
                Some(path),
            )
        })?;
        verify_real_directory(&next, path)?;
        parent = next;
    }
    read_private_leaf(
        &parent,
        components.last().expect("nonempty components"),
        path,
        max_bytes,
        &owner,
    )
}

pub(crate) fn read_private_file_at(
    parent: &StoreDirectory,
    name: &str,
    max_bytes: u64,
) -> Result<Vec<u8>> {
    let leaf = direct_leaf(name, parent.path())?;
    let owner = CurrentUserSid::current().map_err(|_| {
        crate::private_fs::unsafe_file("cannot determine current Windows user", Some(parent.path()))
    })?;
    let handle = parent.root_handle();
    verify_real_directory(&handle, parent.path())?;
    let label = parent.path().join(leaf);
    read_private_leaf(&handle, leaf, &label, max_bytes, &owner)
}

fn read_private_leaf(
    parent: &File,
    leaf: &std::ffi::OsStr,
    path: &Path,
    max_bytes: u64,
    owner: &CurrentUserSid,
) -> Result<Vec<u8>> {
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut file = cap_fs::open(parent, Path::new(leaf), &options)
        .map_err(|_| unsafe_file("cannot open private trust file", Some(path)))?;
    let before_identity =
        crate::platform::regular_file_handle_identity(&file).ok_or_else(|| {
            unsafe_file(
                "private trust file is reparse, nonregular, or hardlinked",
                Some(path),
            )
        })?;
    verify_owner_only(&file, owner, path)?;
    let before = file
        .metadata()
        .map_err(|_| unsafe_file("cannot inspect private trust file", Some(path)))?;
    let before_change_time = change_time(&file, path)?;
    if before.len() > max_bytes {
        return Err(oversized_file(max_bytes, before.len(), path));
    }
    let mut body = Vec::with_capacity(
        usize::try_from(before.len()).map_err(|_| oversized_file(max_bytes, before.len(), path))?,
    );
    file.by_ref()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut body)
        .map_err(|_| unsafe_file("cannot read private trust file", Some(path)))?;
    if body.len() as u64 > max_bytes {
        return Err(oversized_file(max_bytes, body.len() as u64, path));
    }
    let after_identity = crate::platform::regular_file_handle_identity(&file).ok_or_else(|| {
        unsafe_file(
            "private trust file changed type while it was read",
            Some(path),
        )
    })?;
    let after = file
        .metadata()
        .map_err(|_| unsafe_file("cannot recheck private trust file", Some(path)))?;
    let after_change_time = change_time(&file, path)?;
    verify_owner_only(&file, owner, path)?;
    if before_identity != after_identity
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
        || before_change_time != after_change_time
        || after.len() != body.len() as u64
    {
        return Err(raced_file(path));
    }
    Ok(body)
}

fn direct_leaf<'a>(name: &'a str, label: &Path) -> Result<&'a std::ffi::OsStr> {
    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(leaf)), None) => {
            validate_windows_component(leaf, label)?;
            Ok(leaf)
        }
        _ => Err(unsafe_file(
            "private trust file leaf must be one normal component",
            Some(label),
        )),
    }
}

pub(crate) fn verify_trusted_executable(path: &Path) -> Result<()> {
    let (root_path, components) = windows_absolute_components(path)?;
    let current = CurrentUserSid::current()
        .map_err(|_| unsafe_file("cannot determine current Windows user", Some(path)))?;
    let root = cap_fs::open_ambient_dir(&root_path, ambient_authority())
        .map_err(|_| unsafe_file("cannot open scheduler executable root", Some(path)))?;
    verify_real_directory(&root, path)?;
    verify_trusted_acl(&root, &current, path, true)?;
    let mut parent = root;
    for component in &components[..components.len() - 1] {
        let next = cap_fs::open_dir_nofollow(&parent, Path::new(component)).map_err(|_| {
            unsafe_file(
                "scheduler executable has a missing or reparse ancestor",
                Some(path),
            )
        })?;
        verify_real_directory(&next, path)?;
        verify_trusted_acl(&next, &current, path, true)?;
        parent = next;
    }
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let file = cap_fs::open(
        &parent,
        Path::new(components.last().expect("nonempty components")),
        &options,
    )
    .map_err(|_| unsafe_file("cannot open scheduler executable", Some(path)))?;
    if crate::platform::regular_file_handle_identity(&file).is_none() {
        return Err(unsafe_file(
            "scheduler executable is reparse, nonregular, or hardlinked",
            Some(path),
        ));
    }
    verify_trusted_acl(&file, &current, path, false)
}

fn windows_absolute_components(path: &Path) -> Result<(PathBuf, Vec<&std::ffi::OsStr>)> {
    let mut iter = path.components();
    let Some(Component::Prefix(prefix)) = iter.next() else {
        return Err(unsafe_file(
            "private trust file path must be an absolute Windows path",
            Some(path),
        ));
    };
    if !matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
        || !matches!(iter.next(), Some(Component::RootDir))
    {
        return Err(unsafe_file(
            "private trust file path must include a Windows root",
            Some(path),
        ));
    }
    let mut root = PathBuf::from(prefix.as_os_str());
    root.push(Path::new(r"\"));
    let mut components = Vec::new();
    for component in iter {
        match component {
            Component::Normal(value) => {
                validate_windows_component(value, path)?;
                components.push(value);
            }
            _ => {
                return Err(unsafe_file(
                    "private trust file path contains an unsafe component",
                    Some(path),
                ));
            }
        }
    }
    if components.is_empty() {
        return Err(unsafe_file(
            "private trust file path has no file leaf",
            Some(path),
        ));
    }
    Ok((root, components))
}

fn validate_windows_component(value: &OsStr, path: &Path) -> Result<()> {
    if value
        .encode_wide()
        .any(|unit| unit == 0 || unit == b':' as u16)
    {
        return Err(unsafe_file(
            "private trust file path contains an ADS or NUL component",
            Some(path),
        ));
    }
    Ok(())
}

fn change_time(file: &File, path: &Path) -> Result<i64> {
    super::primitives::file_change_time(file).map_err(|_| {
        unsafe_file(
            "cannot inspect private trust file Windows change time",
            Some(path),
        )
    })
}

fn verify_real_directory(directory: &File, path: &Path) -> Result<()> {
    if crate::platform::directory_handle_identity(directory).is_none() {
        return Err(unsafe_file(
            "private trust file path contains a reparse or non-directory component",
            Some(path),
        ));
    }
    Ok(())
}

fn verify_current_owner(file: &File, owner: &CurrentUserSid, path: &Path) -> Result<()> {
    let mut returned = ptr::null_mut();
    // SAFETY: `file` retains the object whose owner is inspected, and
    // Windows allocates the descriptor returned through `returned`.
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut returned,
        )
    };
    if status != 0 || returned.is_null() {
        if !returned.is_null() {
            // SAFETY: GetSecurityInfo allocated the partial descriptor.
            unsafe { LocalFree(returned as _) };
        }
        return Err(unsafe_file(
            "cannot inspect private object Windows owner",
            Some(path),
        ));
    }
    let mut actual_owner = ptr::null_mut();
    let mut owner_defaulted = 0;
    // SAFETY: `returned` is a GetSecurityInfo descriptor and both output
    // pointers address writable storage for this call.
    let owner_ok =
        unsafe { GetSecurityDescriptorOwner(returned, &mut actual_owner, &mut owner_defaulted) }
            != 0
            && !actual_owner.is_null()
            && owner_defaulted == 0
            && unsafe { IsValidSid(actual_owner) } != 0
            && unsafe { EqualSid(actual_owner, owner.as_psid()) } != 0;
    // SAFETY: GetSecurityInfo allocated `returned` exactly once.
    unsafe { LocalFree(returned as _) };
    if owner_ok {
        Ok(())
    } else {
        Err(unsafe_file(
            "private object Windows owner is not the current user",
            Some(path),
        ))
    }
}

fn verify_owner_only(file: &File, owner: &CurrentUserSid, path: &Path) -> Result<()> {
    let mut returned = ptr::null_mut();
    // SAFETY: the handle is retained by `file`; Windows allocates the
    // returned self-relative descriptor, which is freed below.
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
            // SAFETY: GetSecurityInfo allocated the partial descriptor.
            unsafe { LocalFree(returned as _) };
        }
        return Err(unsafe_file(
            "cannot inspect private trust file Windows ACL",
            Some(path),
        ));
    }
    let result = verify_descriptor(returned, owner);
    // SAFETY: returned by GetSecurityInfo above.
    unsafe { LocalFree(returned as _) };
    result.map_err(|_| {
        unsafe_file(
            "private trust file Windows ACL is not protected owner-only",
            Some(path),
        )
    })
}

fn verify_descriptor(
    descriptor: PSECURITY_DESCRIPTOR,
    owner: &CurrentUserSid,
) -> std::io::Result<()> {
    let mut actual_owner = ptr::null_mut();
    let mut owner_defaulted = 0;
    let mut dacl_present = 0;
    let mut dacl = ptr::null_mut();
    let mut dacl_defaulted = 0;
    let mut control = 0_u16;
    let mut revision = 0_u32;
    // SAFETY: descriptor came from GetSecurityInfo and all out-pointers
    // reference writable storage of the required layouts.
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
        return Err(std::io::Error::last_os_error());
    }
    if actual_owner.is_null()
        || unsafe { EqualSid(actual_owner, owner.as_psid()) } == 0
        || dacl_present == 0
        || dacl.is_null()
        || owner_defaulted != 0
        || dacl_defaulted != 0
        || revision != SECURITY_DESCRIPTOR_REVISION
        || control & SE_DACL_PROTECTED == 0
    {
        return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
    }
    let mut info = ACL_SIZE_INFORMATION::default();
    // SAFETY: DACL came from the validated descriptor; output size matches.
    if unsafe {
        GetAclInformation(
            dacl,
            (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
            mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            ACL_SIZE_INFORMATION_CLASS,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    if info.AceCount != 1 {
        return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
    }
    let mut ace = ptr::null_mut();
    // SAFETY: index zero exists because AceCount is exactly one.
    if unsafe { GetAce(dacl, 0, &mut ace) } == 0 || ace.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let header = ace.cast::<ACE_HEADER>();
    let allowed = ace.cast::<ACCESS_ALLOWED_ACE>();
    if unsafe { (*header).AceType } != ACCESS_ALLOWED_ACE_TYPE
        || unsafe { (*header).AceFlags } != 0
        || unsafe { (*allowed).Mask } != FILE_ALL_ACCESS
        || unsafe { (*header).AceSize as usize }
            != mem::size_of::<ACCESS_ALLOWED_ACE>() - mem::size_of::<u32>() + owner.len()
    {
        return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
    }
    // SAFETY: the ACE size above bounds the variable SID at SidStart.
    let ace_sid = unsafe {
        (&(*allowed).SidStart as *const u32)
            .cast_mut()
            .cast::<c_void>()
    };
    if unsafe { EqualSid(ace_sid, owner.as_psid()) } == 0 {
        return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
    }
    Ok(())
}

fn verify_trusted_acl(
    file: &File,
    current: &CurrentUserSid,
    path: &Path,
    directory: bool,
) -> Result<()> {
    let mut descriptor = ptr::null_mut();
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 || descriptor.is_null() {
        if !descriptor.is_null() {
            unsafe { LocalFree(descriptor as _) };
        }
        return Err(unsafe_file(
            "cannot inspect scheduler executable ACL",
            Some(path),
        ));
    }
    let result = (|| {
        let mut owner = ptr::null_mut();
        let mut owner_defaulted = 0;
        let mut dacl_present = 0;
        let mut dacl = ptr::null_mut();
        let mut dacl_defaulted = 0;
        if unsafe { GetSecurityDescriptorOwner(descriptor, &mut owner, &mut owner_defaulted) } == 0
            || unsafe {
                GetSecurityDescriptorDacl(
                    descriptor,
                    &mut dacl_present,
                    &mut dacl,
                    &mut dacl_defaulted,
                )
            } == 0
            || owner.is_null()
            || dacl_present == 0
            || dacl.is_null()
            || owner_defaulted != 0
            || dacl_defaulted != 0
            || unsafe { IsValidSid(owner) } == 0
            || !trusted_sid(owner, current)
        {
            return Err(());
        }
        let mut info = ACL_SIZE_INFORMATION::default();
        if unsafe {
            GetAclInformation(
                dacl,
                (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
                mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                ACL_SIZE_INFORMATION_CLASS,
            )
        } == 0
        {
            return Err(());
        }
        for index in 0..info.AceCount {
            let mut ace = ptr::null_mut();
            if unsafe { GetAce(dacl, index, &mut ace) } == 0 || ace.is_null() {
                return Err(());
            }
            let header = ace.cast::<ACE_HEADER>();
            let ace_type = unsafe { (*header).AceType };
            if ace_type != ACCESS_ALLOWED_ACE_TYPE && ace_type != ACCESS_DENIED_ACE_TYPE {
                return Err(());
            }
            let ace_flags = unsafe { (*header).AceFlags };
            let ace_size = unsafe { (*header).AceSize as usize };
            let fixed = mem::size_of::<ACCESS_ALLOWED_ACE>() - mem::size_of::<u32>();
            const SID_HEADER_BYTES: usize = 8;
            if ace_size < fixed.saturating_add(SID_HEADER_BYTES) {
                return Err(());
            }
            let allowed = ace.cast::<ACCESS_ALLOWED_ACE>();
            let sid_bytes = unsafe { (&(*allowed).SidStart as *const u32).cast::<u8>() };
            let payload = ace_size - fixed;
            let sub_authorities = unsafe { *sid_bytes.add(1) as usize };
            let sid_size = SID_HEADER_BYTES.saturating_add(sub_authorities.saturating_mul(4));
            if sid_size > payload || unsafe { IsValidSid(sid_bytes.cast_mut().cast()) } == 0 {
                return Err(());
            }
            let sid = sid_bytes.cast_mut().cast::<c_void>();
            if ace_type == ACCESS_ALLOWED_ACE_TYPE
                && !trusted_sid(sid, current)
                && crate::platform::windows::private_fs::untrusted_effective_mutation(
                    ace_flags,
                    unsafe { (*allowed).Mask },
                    directory,
                )
            {
                return Err(());
            }
        }
        Ok(())
    })();
    unsafe { LocalFree(descriptor as _) };
    result.map_err(|_| {
        unsafe_file(
            "scheduler executable ACL permits untrusted mutation",
            Some(path),
        )
    })
}

fn dangerous_access(mask: u32, directory: bool) -> bool {
    let mut dangerous = GENERIC_ALL
        | GENERIC_WRITE
        | MAXIMUM_ALLOWED
        | DELETE
        | WRITE_DAC
        | WRITE_OWNER
        | FILE_WRITE_EA
        | FILE_WRITE_ATTRIBUTES;
    if directory {
        dangerous |= FILE_DELETE_CHILD;
    } else {
        dangerous |= FILE_WRITE_DATA | FILE_APPEND_DATA;
    }
    mask & dangerous != 0
}

fn untrusted_effective_mutation(ace_flags: u8, mask: u32, directory: bool) -> bool {
    // INHERIT_ONLY_ACE affects descendants, never this object. Every
    // descendant is opened and checked in its own iteration.
    ace_flags as u32 & INHERIT_ONLY_ACE == 0 && dangerous_access(mask, directory)
}

fn trusted_sid(sid: PSID, current: &CurrentUserSid) -> bool {
    if unsafe { EqualSid(sid, current.as_psid()) } != 0
        || unsafe { IsWellKnownSid(sid, WinLocalSystemSid) } != 0
        || unsafe { IsWellKnownSid(sid, WinBuiltinAdministratorsSid) } != 0
    {
        return true;
    }
    let mut trusted_installer = ptr::null_mut();
    let mut text = "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let converted = unsafe { ConvertStringSidToSidW(text.as_mut_ptr(), &mut trusted_installer) };
    let trusted = converted != 0
        && !trusted_installer.is_null()
        && unsafe { EqualSid(sid, trusted_installer) } != 0;
    if !trusted_installer.is_null() {
        unsafe { LocalFree(trusted_installer as _) };
    }
    trusted
}

struct CurrentUserSid {
    bytes: Vec<usize>,
}
impl CurrentUserSid {
    fn current() -> std::io::Result<Self> {
        let mut token = ptr::null_mut();
        // SAFETY: current-process pseudo handle and output token pointer are valid.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let result = Self::from_token(token);
        // SAFETY: OpenProcessToken returned this handle.
        unsafe { CloseHandle(token) };
        result
    }
    fn from_token(token: HANDLE) -> std::io::Result<Self> {
        let mut needed = 0_u32;
        // SAFETY: null buffer is the documented size probe.
        let _ = unsafe { GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut needed) };
        if needed == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut words = vec![0_usize; (needed as usize).div_ceil(mem::size_of::<usize>())];
        // SAFETY: allocation has at least `needed` bytes and correct alignment.
        if unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                words.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let sid = unsafe { (*(words.as_ptr().cast::<TOKEN_USER>())).User.Sid };
        if sid.is_null() {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
        }
        let length = unsafe { GetLengthSid(sid) } as usize;
        if length == 0 || length > needed as usize {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
        }
        let mut bytes = vec![0_usize; length.div_ceil(mem::size_of::<usize>())];
        // SAFETY: both regions are valid, non-overlapping SID buffers.
        unsafe {
            ptr::copy_nonoverlapping(sid.cast::<u8>(), bytes.as_mut_ptr().cast::<u8>(), length)
        };
        Ok(Self { bytes })
    }
    fn as_psid(&self) -> PSID {
        self.bytes.as_ptr().cast_mut().cast()
    }
    fn len(&self) -> usize {
        // SAFETY: `self` owns the validated SID buffer returned by the
        // current-process token query.
        unsafe { GetLengthSid(self.as_psid()) as usize }
    }
}

pub(crate) fn resolve_inherited_private_root(
    path: &Path,
    _control: crate::control::ControlHandle,
) -> Result<Option<(StoreDirectory, Vec<std::ffi::OsString>)>> {
    let _ = path;
    Ok(None)
}

pub(crate) fn verify_private_regular_file_handle(file: &File) -> Result<()> {
    verify_trusted_owned_regular_file_handle(file)
}

pub(crate) fn verify_trusted_owned_regular_file_handle(file: &std::fs::File) -> Result<()> {
    if crate::platform::regular_file_handle_identity(file).is_none() {
        return Err(unsafe_file(
            "trusted leaf is not a single-link regular file",
            None,
        ));
    }
    verify_owner_private_handle(file)
}

#[cfg(test)]
mod acl_vectors {
    use super::{GENERIC_ALL, INHERIT_ONLY_ACE, untrusted_effective_mutation};

    #[test]
    fn inherited_only_creator_owner_grant_is_not_effective_on_parent() {
        assert!(!untrusted_effective_mutation(
            INHERIT_ONLY_ACE as u8,
            GENERIC_ALL,
            true,
        ));
        assert!(untrusted_effective_mutation(0, GENERIC_ALL, true));
    }
}
