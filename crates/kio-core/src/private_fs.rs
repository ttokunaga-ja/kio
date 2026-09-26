//! Read small device-local trust files without accepting a path that another
//! local user can replace while it is being inspected.
//!
//! This module deliberately has a narrower contract than general file I/O.
//! Callers use it for credentials that authenticate a local peer (for example,
//! a private CA certificate), so a convenient path based `read` would turn a
//! writable ancestor or a symlink race into a trust-boundary bypass.

use std::{ffi::OsString, path::Path};

#[cfg(unix)]
use std::path::Component;

use serde_json::json;

use crate::store_dir::StoreDirectory;
use crate::{ExitCode, KioError, Result};

const PRIVATE_FILE_ERROR: &str = "KIO-E-PRIVATE-FILE-UNSAFE-001";
const PRIVATE_FILE_OVERSIZE: &str = "KIO-E-PRIVATE-FILE-OVERSIZE-001";
const PRIVATE_FILE_RACE: &str = "KIO-E-PRIVATE-FILE-RACE-001";

/// Read an owner-private, single-link regular file through retained directory
/// capabilities.
///
/// `path` must be absolute. Every component is opened with no-follow semantics
/// from the retained root capability; the final leaf is never reopened by its
/// absolute pathname. The final parent and leaf must be owned by the current
/// user and owner-only. Ancestors must be owned by the current user or root and
/// not writable by another user, except for root-owned sticky directories such
/// as the OS temporary directory. No permissions are repaired by this API.
pub fn read_private_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    #[cfg(unix)]
    {
        unix::read_private_file(path, max_bytes)
    }
    #[cfg(windows)]
    {
        windows::read_private_file(path, max_bytes)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, max_bytes);
        Err(unsafe_file(
            "private trust files are unsupported on this platform",
            None,
        ))
    }
}

/// Read a direct owner-private regular-file child of an already retained
/// private directory capability.
///
/// `name` must be one ordinary path component. The directory is rechecked
/// through its retained handle before the leaf is opened; its diagnostic path
/// is never reopened.
pub fn read_private_file_at(
    parent: &StoreDirectory,
    name: &str,
    max_bytes: u64,
) -> Result<Vec<u8>> {
    #[cfg(unix)]
    {
        unix::read_private_file_at(parent, name, max_bytes)
    }
    #[cfg(windows)]
    {
        windows::read_private_file_at(parent, name, max_bytes)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (parent, name, max_bytes);
        Err(unsafe_file(
            "private trust files are unsupported on this platform",
            None,
        ))
    }
}

/// Resolve the sole supported inherited-directory spelling as a retained
/// capability root and a validated relative suffix.
///
/// This deliberately does not resolve `/dev/fd` as an ordinary path. The raw
/// Unix bytes must be exactly `/dev/fd/<canonical decimal fd>` followed by
/// optional ordinary name components. The descriptor is duplicated with
/// close-on-exec before it is inspected, so later operations retain the
/// duplicated directory rather than reopening the magic path.
#[cfg(unix)]
pub fn resolve_inherited_private_root(
    path: &Path,
) -> Result<Option<(StoreDirectory, Vec<OsString>)>> {
    unix::resolve_inherited_private_root(path)
}

#[cfg(not(unix))]
pub fn resolve_inherited_private_root(
    path: &Path,
) -> Result<Option<(StoreDirectory, Vec<OsString>)>> {
    let _ = path;
    Ok(None)
}

/// Retain an existing owner-private directory after validating it without
/// following any path component. The returned [`StoreDirectory`] keeps the
/// final directory handle; its display path is diagnostic only and is never
/// reopened for later mutations.
///
/// `path` must be absolute. Every ancestor follows the same trusted-owner,
/// no-untrusted-write policy as [`read_private_file`], and the final directory
/// must be owned by the current user and owner-only (including macOS ACL or
/// Windows DACL checks). This function never creates, repairs, or locks an
/// entry.
pub fn verify_private_directory(path: &Path) -> Result<StoreDirectory> {
    #[cfg(unix)]
    {
        unix::verify_private_directory(path)
    }
    #[cfg(windows)]
    {
        windows::verify_private_directory(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(unsafe_file(
            "private directories are unsupported on this platform",
            None,
        ))
    }
}

/// Recheck that an already retained directory capability remains owner-private.
///
/// This validates the retained handle itself and never resolves its diagnostic
/// path. It is intended for callers which have traversed a validated suffix
/// beneath an inherited directory capability.
pub fn verify_private_directory_handle(directory: &StoreDirectory) -> Result<()> {
    #[cfg(unix)]
    {
        unix::verify_private_directory_handle(directory)
    }
    #[cfg(windows)]
    {
        windows::verify_private_directory_handle(directory)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = directory;
        Err(unsafe_file(
            "private directories are unsupported on this platform",
            None,
        ))
    }
}

/// Retain an existing directory that is safe to create owner-private children
/// beneath. Unlike [`verify_private_directory`], the final directory may be a
/// trusted shared-read parent such as an XDG data directory with mode `0755`.
/// It must still be absolute, non-symlinked, and not writable, deletable, or
/// ACL-mutable by an untrusted local principal. The retained handle prevents a
/// later path re-resolution before private children are created.
///
/// This function only verifies an existing parent. It never creates, repairs,
/// locks, or otherwise mutates filesystem state.
pub fn verify_private_creation_parent(path: &Path) -> Result<StoreDirectory> {
    #[cfg(unix)]
    {
        unix::verify_private_creation_parent(path)
    }
    #[cfg(windows)]
    {
        windows::verify_private_creation_parent(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(unsafe_file(
            "private directory creation is unsupported on this platform",
            None,
        ))
    }
}

/// Verify an already opened native-definition leaf without changing it.
/// Unix shared-read modes such as `0644` are allowed, but only the current
/// owner may mutate the single-link regular file. Windows uses the existing
/// owner-private DACL policy. Callers must separately admit its ancestry and
/// open the leaf without following links.
pub fn verify_trusted_owned_regular_file_handle(file: &std::fs::File) -> Result<()> {
    #[cfg(unix)]
    {
        unix::verify_trusted_owned_regular_file_handle(file)
    }
    #[cfg(windows)]
    {
        if crate::cas::windows_regular_file_handle_identity(file).is_none() {
            return Err(unsafe_file(
                "trusted leaf is not a single-link regular file",
                None,
            ));
        }
        windows::verify_owner_private_handle(file)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Err(unsafe_file(
            "trusted regular files are unsupported on this platform",
            None,
        ))
    }
}

/// Resolve and verify an executable that a persistent native scheduler may
/// launch later.
///
/// The returned path is canonical and must be used verbatim in the scheduler
/// definition.  The check rejects a file or an ancestor that another local
/// principal can replace, delete, or retake ownership of.  This is distinct
/// from [`read_private_file`]: trusted system installation directories may be
/// shared for reading and execution, but may not be writable by an untrusted
/// principal.
pub fn verify_trusted_executable(path: &Path) -> Result<std::path::PathBuf> {
    let canonical = path
        .canonicalize()
        .map_err(|_| unsafe_file("cannot resolve scheduler executable", Some(path)))?;
    #[cfg(unix)]
    unix::verify_trusted_executable(&canonical)?;
    #[cfg(windows)]
    windows::verify_trusted_executable(&canonical)?;
    #[cfg(not(any(unix, windows)))]
    {
        let _ = &canonical;
        return Err(unsafe_file(
            "scheduler executable verification is unsupported on this platform",
            Some(path),
        ));
    }
    Ok(canonical)
}

/// Create one direct child beneath a retained Windows directory with its
/// current-user owner and protected owner-only DACL installed by the kernel at
/// creation time. This never adopts or repairs an existing name.
#[cfg(windows)]
#[derive(Clone, Copy)]
pub(crate) enum NewPrivateObjectKind {
    Directory,
    RegularFile,
}

#[cfg(windows)]
pub(crate) fn create_new_private_at(
    parent: &std::fs::File,
    leaf: &std::ffi::OsStr,
    kind: NewPrivateObjectKind,
) -> std::io::Result<std::fs::File> {
    windows::create_new_private_at(parent, leaf, kind)
}

/// Restrict a retained, already-existing object only after proving that the
/// current user owns it. New objects must use `create_new_private_at` instead.
#[cfg(windows)]
pub(crate) fn restrict_existing_private_handle(file: &std::fs::File) -> Result<()> {
    windows::restrict_existing_private_handle(file)
}

#[cfg(windows)]
/// Verify a retained Windows handle's owner-private ACL before another crate
/// creates operational artifacts beneath it. This never repairs permissions.
pub fn verify_owner_private_handle(file: &std::fs::File) -> Result<()> {
    windows::verify_owner_private_handle(file)
}

/// Require that a retained Windows file or directory belongs to the current
/// user without changing its security descriptor.
#[cfg(windows)]
pub(crate) fn verify_current_owner_handle(file: &std::fs::File) -> Result<()> {
    windows::verify_current_owner_handle(file)
}

fn unsafe_file(message: &str, path: Option<&Path>) -> KioError {
    KioError::new(
        PRIVATE_FILE_ERROR,
        message,
        json!({ "path": path.map(|value| value.display().to_string()) }),
        ExitCode::PermanentFailure,
    )
}

fn oversized_file(max_bytes: u64, actual_bytes: u64, path: &Path) -> KioError {
    KioError::new(
        PRIVATE_FILE_OVERSIZE,
        "private trust file exceeds its byte limit",
        json!({ "path": path.display().to_string(), "max_bytes": max_bytes, "actual_bytes": actual_bytes }),
        ExitCode::PermanentFailure,
    )
}

fn raced_file(path: &Path) -> KioError {
    KioError::new(
        PRIVATE_FILE_RACE,
        "private trust file changed while it was read",
        json!({ "path": path.display().to_string() }),
        ExitCode::PermanentFailure,
    )
}

#[cfg(unix)]
fn path_components(path: &Path) -> Result<Vec<&std::ffi::OsStr>> {
    if !path.is_absolute() {
        return Err(unsafe_file(
            "private trust file path must be absolute",
            Some(path),
        ));
    }
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(value) => components.push(value),
            // Prefixes are handled by the Windows implementation before it
            // calls this helper. On Unix these components cannot occur in an
            // absolute path, and accepting `.` or `..` would defeat the
            // component-by-component security policy.
            Component::Prefix(_) | Component::CurDir | Component::ParentDir => {
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
    Ok(components)
}

#[cfg(unix)]
mod unix {
    #[cfg(target_os = "macos")]
    mod macos_acl {
        use std::{
            ffi::c_void,
            fs::File,
            os::{fd::AsRawFd, unix::fs::MetadataExt},
            path::Path,
        };

        use super::super::{Result, unsafe_file};

        type Acl = *mut c_void;
        type AclEntry = *mut c_void;
        type AclFlagset = *mut c_void;

        const ACL_TYPE_EXTENDED: i32 = 0x100;
        const ACL_FIRST_ENTRY: i32 = 0;
        const ACL_NEXT_ENTRY: i32 = -1;
        const MAX_ACL_ENTRIES: usize = 128;
        const ACL_EXTENDED_ALLOW: i32 = 1;
        const ACL_ENTRY_ONLY_INHERIT: i32 = 1 << 8;
        const ACL_WRITE_DATA: u64 = 1 << 2;
        const ACL_DELETE: u64 = 1 << 4;
        const ACL_APPEND_DATA: u64 = 1 << 5;
        const ACL_DELETE_CHILD: u64 = 1 << 6;
        const ACL_WRITE_ATTRIBUTES: u64 = 1 << 8;
        const ACL_WRITE_EXTATTRIBUTES: u64 = 1 << 10;
        const ACL_WRITE_SECURITY: u64 = 1 << 12;
        const ACL_CHANGE_OWNER: u64 = 1 << 13;
        const ACL_ALL_KNOWN_PERMISSIONS: u64 = (1 << 21) - 1;
        const MUTATE_FILE: u64 = ACL_WRITE_DATA
            | ACL_DELETE
            | ACL_APPEND_DATA
            | ACL_WRITE_ATTRIBUTES
            | ACL_WRITE_EXTATTRIBUTES
            | ACL_WRITE_SECURITY
            | ACL_CHANGE_OWNER;
        const MUTATE_DIRECTORY: u64 = MUTATE_FILE | ACL_DELETE_CHILD;

        unsafe extern "C" {
            fn acl_get_fd_np(fd: i32, ty: i32) -> Acl;
            fn acl_valid(acl: Acl) -> i32;
            fn acl_free(object: *mut c_void) -> i32;
            fn acl_get_entry(acl: Acl, entry_id: i32, entry: *mut AclEntry) -> i32;
            fn acl_get_tag_type(entry: AclEntry, tag: *mut i32) -> i32;
            fn acl_get_qualifier(entry: AclEntry) -> *mut c_void;
            fn acl_get_permset_mask_np(entry: AclEntry, mask: *mut u64) -> i32;
            fn acl_get_flagset_np(object: *mut c_void, flags: *mut AclFlagset) -> i32;
            fn acl_get_flag_np(flags: AclFlagset, flag: i32) -> i32;
            fn mbr_uid_to_uuid(uid: u32, uuid: *mut u8) -> i32;
        }

        #[derive(Clone, Copy)]
        pub(super) enum Scope {
            PrivateAncestor,
            PrivateParent,
            PrivateLeaf,
            ExecutableAncestor,
            ExecutableLeaf,
        }

        fn trusted_uuids(file: &File, current_uid: u32) -> std::result::Result<Vec<[u8; 16]>, ()> {
            // Derive the owner through the retained descriptor; permit the
            // current account, root, and the retained object owner.
            let owner_uid = file.metadata().map_err(|_| ())?.uid();
            let mut values = Vec::with_capacity(3);
            let uids = [current_uid, 0, owner_uid];
            for (index, uid) in uids.into_iter().enumerate() {
                if uids[..index].contains(&uid) {
                    continue;
                }
                let mut uuid = [0_u8; 16];
                if unsafe { mbr_uid_to_uuid(uid, uuid.as_mut_ptr()) } != 0 {
                    return Err(());
                }
                if !values.contains(&uuid) {
                    values.push(uuid);
                }
            }
            Ok(values)
        }

        pub(super) fn verify(
            file: &File,
            current_uid: u32,
            scope: Scope,
            path: &Path,
        ) -> Result<()> {
            let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) };
            if acl.is_null() {
                // ENOENT means that this descriptor has no extended ACL.
                return if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
                    Ok(())
                } else {
                    Err(unsafe_file("cannot inspect retained macOS ACL", Some(path)))
                };
            }
            let result = (|| {
                // macOS acl_get_entry returns 0 for an entry and -1/EINVAL
                // at the end. acl_valid validates the retained extended ACL;
                // acl_valid_fd_np does not support ACL_TYPE_EXTENDED here.
                if unsafe { acl_valid(acl) } != 0 {
                    return Err(());
                }
                let mut trusted = None;
                let mut entry = std::ptr::null_mut();
                let mut selector = ACL_FIRST_ENTRY;
                let mut reached_end = false;
                for _ in 0..MAX_ACL_ENTRIES {
                    let state = unsafe { acl_get_entry(acl, selector, &mut entry) };
                    if state == -1
                        && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL)
                    {
                        reached_end = true;
                        break;
                    }
                    if state != 0 || entry.is_null() {
                        return Err(());
                    }
                    selector = ACL_NEXT_ENTRY;
                    let mut tag = 0;
                    let mut flags = std::ptr::null_mut();
                    let mut mask = 0_u64;
                    if unsafe { acl_get_tag_type(entry, &mut tag) } != 0
                        || unsafe { acl_get_flagset_np(entry.cast(), &mut flags) } != 0
                        || flags.is_null()
                        || unsafe { acl_get_permset_mask_np(entry, &mut mask) } != 0
                    {
                        return Err(());
                    }
                    // INHERIT_ONLY is not effective for this retained object.
                    let inherit_only = unsafe { acl_get_flag_np(flags, ACL_ENTRY_ONLY_INHERIT) };
                    if inherit_only < 0 {
                        return Err(());
                    }
                    if inherit_only == 1 {
                        continue;
                    }
                    if tag != ACL_EXTENDED_ALLOW {
                        // Denies do not grant a right; this retains normal
                        // macOS deny-delete entries on private home directories.
                        continue;
                    }
                    let forbidden = match scope {
                        Scope::PrivateParent | Scope::PrivateLeaf => {
                            mask & ACL_ALL_KNOWN_PERMISSIONS
                        }
                        Scope::PrivateAncestor | Scope::ExecutableAncestor => {
                            mask & MUTATE_DIRECTORY
                        }
                        Scope::ExecutableLeaf => mask & MUTATE_FILE,
                    };
                    if forbidden == 0 {
                        continue;
                    }
                    // Resolve identities only for effective, relevant grants,
                    // once per verification, using fresh retained metadata.
                    // Resolve before allocating the qualifier so lookup errors
                    // cannot leak it.
                    if trusted.is_none() {
                        trusted = Some(trusted_uuids(file, current_uid)?);
                    }
                    let qualifier = unsafe { acl_get_qualifier(entry) };
                    if qualifier.is_null() {
                        return Err(());
                    }
                    let uuid = unsafe { std::slice::from_raw_parts(qualifier.cast::<u8>(), 16) };
                    let trusted_grant = trusted
                        .as_ref()
                        .is_some_and(|values| values.iter().any(|candidate| uuid == candidate));
                    unsafe { acl_free(qualifier) };
                    // A group UUID is untrusted: evaluating group membership
                    // safely would otherwise authorize another local account.
                    if !trusted_grant {
                        return Err(());
                    }
                }
                if !reached_end
                    && (unsafe { acl_get_entry(acl, ACL_NEXT_ENTRY, &mut entry) } != -1
                        || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINVAL))
                {
                    return Err(());
                }
                Ok(())
            })();
            unsafe { acl_free(acl) };
            result.map_err(|_| {
                unsafe_file(
                    "retained macOS ACL grants an untrusted principal",
                    Some(path),
                )
            })
        }
    }

    use std::{
        ffi::{CString, OsString},
        fs::File,
        io::Read,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{
                ffi::{OsStrExt, OsStringExt},
                fs::MetadataExt,
            },
        },
        path::{Component, Path, PathBuf},
    };

    use cap_primitives::{ambient_authority, fs as cap_fs};

    use super::{Result, StoreDirectory, oversized_file, path_components, raced_file, unsafe_file};

    /// Recognize the raw spelling before `Path::components` can normalize an
    /// alias such as `/dev//fd` into an apparent descriptor root.
    fn inherited_root_parts(path: &Path) -> Result<Option<(i32, Vec<OsString>)>> {
        let raw = path.as_os_str().as_bytes();
        const PREFIX: &[u8] = b"/dev/fd/";
        if !raw.starts_with(PREFIX) {
            // A component-normalized spelling which names `/dev/fd` is never
            // allowed to fall through to the descriptor capability bridge.
            // Inspect it only after rejecting the raw prefix so normalization
            // cannot grant authority to an alias.
            let mut components = path.components();
            let names_descriptor_root = components.next() == Some(Component::RootDir)
                && matches!(components.next(), Some(Component::Normal(name)) if name.as_bytes() == b"dev")
                && matches!(components.next(), Some(Component::Normal(name)) if name.as_bytes() == b"fd");
            if names_descriptor_root {
                return Err(unsafe_file(
                    "inherited private directory path is not canonical",
                    Some(path),
                ));
            }
            return Ok(None);
        }
        let remainder = &raw[PREFIX.len()..];
        if remainder.is_empty() || remainder.starts_with(b"/") || remainder.ends_with(b"/") {
            return Err(unsafe_file(
                "inherited private directory path is not canonical",
                Some(path),
            ));
        }
        let mut parts = remainder.split(|byte| *byte == b'/');
        let descriptor = parts.next().expect("nonempty inherited descriptor");
        if descriptor.is_empty()
            || !descriptor.iter().all(u8::is_ascii_digit)
            || (descriptor.len() > 1 && descriptor[0] == b'0')
        {
            return Err(unsafe_file(
                "inherited private directory descriptor is invalid",
                Some(path),
            ));
        }
        let fd = std::str::from_utf8(descriptor)
            .ok()
            .and_then(|value| value.parse::<i32>().ok())
            .filter(|fd| *fd >= 0)
            .ok_or_else(|| {
                unsafe_file(
                    "inherited private directory descriptor is invalid",
                    Some(path),
                )
            })?;
        let mut suffix = Vec::new();
        for component in parts {
            if component.is_empty() || component == b"." || component == b".." {
                return Err(unsafe_file(
                    "inherited private directory path is not canonical",
                    Some(path),
                ));
            }
            suffix.push(OsString::from_vec(component.to_vec()));
        }
        Ok(Some((fd, suffix)))
    }

    /// Duplicate the inherited descriptor before treating it as authority.
    fn duplicate_inherited_private_root(fd: i32, path: &Path) -> Result<File> {
        // SAFETY: fcntl receives an integer descriptor supplied by the caller;
        // F_DUPFD_CLOEXEC returns a new descriptor or -1 and has no aliasing
        // requirements.
        let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if duplicate < 0 {
            return Err(unsafe_file(
                "cannot duplicate inherited private directory descriptor",
                Some(path),
            ));
        }
        // SAFETY: F_DUPFD_CLOEXEC returned one newly owned descriptor.
        Ok(unsafe { File::from_raw_fd(duplicate) })
    }

    pub(super) fn resolve_inherited_private_root(
        path: &Path,
    ) -> Result<Option<(StoreDirectory, Vec<OsString>)>> {
        let Some((fd, suffix)) = inherited_root_parts(path)? else {
            return Ok(None);
        };
        let root = duplicate_inherited_private_root(fd, path)?;
        // SAFETY: geteuid has no arguments and no memory-safety preconditions.
        let uid = unsafe { libc::geteuid() };
        verify_private_parent(&root, uid, path)?;
        let logical = PathBuf::from(format!("/dev/fd/{fd}"));
        Ok(Some((
            StoreDirectory::from_retained(root, logical)?,
            suffix,
        )))
    }

    pub(super) fn verify_private_directory_handle(directory: &StoreDirectory) -> Result<()> {
        // SAFETY: geteuid has no arguments and no memory-safety preconditions.
        let uid = unsafe { libc::geteuid() };
        let handle = directory.root_handle();
        verify_private_parent(&handle, uid, directory.path())
    }

    pub(super) fn verify_private_directory(path: &Path) -> Result<StoreDirectory> {
        if let Some((root, suffix)) = resolve_inherited_private_root(path)? {
            return inherited_private_directory(root, &suffix, path, false);
        }
        let components = path_components(path)?;
        // SAFETY: geteuid has no arguments and no memory-safety preconditions.
        let uid = unsafe { libc::geteuid() };
        let root = cap_fs::open_ambient_dir(Path::new("/"), ambient_authority()).map_err(|_| {
            unsafe_file(
                "cannot open filesystem root for private directory",
                Some(path),
            )
        })?;
        verify_ancestor(&root, uid, path)?;
        let mut directory = root;
        for (index, component) in components.iter().enumerate() {
            let next =
                cap_fs::open_dir_nofollow(&directory, Path::new(component)).map_err(|_| {
                    unsafe_file(
                        "private directory has a missing, symlinked, or unsafe component",
                        Some(path),
                    )
                })?;
            if index + 1 < components.len() {
                verify_ancestor(&next, uid, path)?;
            }
            directory = next;
        }
        verify_private_parent(&directory, uid, path)?;
        StoreDirectory::from_retained(directory, path.to_path_buf())
    }

    pub(super) fn verify_private_creation_parent(path: &Path) -> Result<StoreDirectory> {
        if let Some((root, suffix)) = resolve_inherited_private_root(path)? {
            return inherited_private_directory(root, &suffix, path, true);
        }
        let components = path_components(path)?;
        // SAFETY: geteuid has no arguments and no memory-safety preconditions.
        let uid = unsafe { libc::geteuid() };
        let root = cap_fs::open_ambient_dir(Path::new("/"), ambient_authority()).map_err(|_| {
            unsafe_file(
                "cannot open filesystem root for private directory creation",
                Some(path),
            )
        })?;
        verify_ancestor(&root, uid, path)?;
        let mut directory = root;
        for component in &components {
            let next = cap_fs::open_dir_nofollow(&directory, Path::new(component)).map_err(|_| {
                unsafe_file(
                    "private directory creation parent has a missing, symlinked, or unsafe component",
                    Some(path),
                )
            })?;
            // The final creation parent intentionally follows the ancestor
            // policy: it may be owner-readable (for example 0755), but never
            // writable by another principal.
            verify_ancestor(&next, uid, path)?;
            directory = next;
        }
        verify_creation_parent(&directory, uid, path)?;
        StoreDirectory::from_retained(directory, path.to_path_buf())
    }

    pub(super) fn read_private_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
        if let Some((root, suffix)) = resolve_inherited_private_root(path)? {
            return read_inherited_private_file(&root, &suffix, path, max_bytes);
        }
        let components = path_components(path)?;
        // SAFETY: geteuid has no arguments and no memory-safety preconditions.
        let uid = unsafe { libc::geteuid() };
        let root = cap_fs::open_ambient_dir(Path::new("/"), ambient_authority()).map_err(|_| {
            unsafe_file(
                "cannot open filesystem root for private trust file",
                Some(path),
            )
        })?;
        verify_ancestor(&root, uid, path)?;

        let mut parent = root;
        for component in &components[..components.len() - 1] {
            let next = cap_fs::open_dir_nofollow(&parent, Path::new(component)).map_err(|_| {
                unsafe_file(
                    "private trust file has a missing, symlinked, or unsafe ancestor",
                    Some(path),
                )
            })?;
            verify_ancestor(&next, uid, path)?;
            parent = next;
        }
        verify_private_parent(&parent, uid, path)?;
        read_leaf(
            &parent,
            components.last().expect("nonempty components"),
            path,
            max_bytes,
            uid,
        )
    }

    pub(super) fn read_private_file_at(
        parent: &StoreDirectory,
        name: &str,
        max_bytes: u64,
    ) -> Result<Vec<u8>> {
        let leaf = direct_leaf(name, parent.path())?;
        // SAFETY: geteuid has no arguments and no memory-safety preconditions.
        let uid = unsafe { libc::geteuid() };
        let handle = parent.root_handle();
        verify_private_parent(&handle, uid, parent.path())?;
        let label = parent.path().join(leaf);
        read_leaf(&handle, leaf, &label, max_bytes, uid)
    }

    fn inherited_private_directory(
        root: StoreDirectory,
        suffix: &[OsString],
        path: &Path,
        creation_parent: bool,
    ) -> Result<StoreDirectory> {
        // SAFETY: geteuid has no arguments and no memory-safety preconditions.
        let uid = unsafe { libc::geteuid() };
        let mut directory = root.root_handle();
        for component in suffix {
            let next =
                cap_fs::open_dir_nofollow(&directory, Path::new(component)).map_err(|_| {
                    unsafe_file(
                        "inherited private directory has a missing, symlinked, or unsafe component",
                        Some(path),
                    )
                })?;
            verify_ancestor(&next, uid, path)?;
            directory = std::sync::Arc::new(next);
        }
        if creation_parent {
            verify_creation_parent(&directory, uid, path)?;
        } else {
            verify_private_parent(&directory, uid, path)?;
        }
        let logical = path.to_path_buf();
        let directory = duplicate_inherited_private_root(directory.as_raw_fd(), path)?;
        StoreDirectory::from_retained(directory, logical)
    }

    fn read_inherited_private_file(
        root: &StoreDirectory,
        suffix: &[OsString],
        path: &Path,
        max_bytes: u64,
    ) -> Result<Vec<u8>> {
        let (leaf, parents) = suffix
            .split_last()
            .ok_or_else(|| unsafe_file("inherited private trust file has no leaf", Some(path)))?;
        // SAFETY: geteuid has no arguments and no memory-safety preconditions.
        let uid = unsafe { libc::geteuid() };
        let mut parent = root.root_handle();
        for component in parents {
            let next = cap_fs::open_dir_nofollow(&parent, Path::new(component)).map_err(|_| {
                unsafe_file(
                    "inherited private trust file has a missing, symlinked, or unsafe ancestor",
                    Some(path),
                )
            })?;
            verify_ancestor(&next, uid, path)?;
            parent = std::sync::Arc::new(next);
        }
        verify_private_parent(&parent, uid, path)?;
        read_leaf(&parent, leaf, path, max_bytes, uid)
    }

    fn direct_leaf<'a>(name: &'a str, label: &Path) -> Result<&'a std::ffi::OsStr> {
        let path = Path::new(name);
        let mut components = path.components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(leaf)), None) => Ok(leaf),
            _ => Err(unsafe_file(
                "private trust file leaf must be one normal component",
                Some(label),
            )),
        }
    }

    fn metadata(file: &File, path: &Path) -> Result<std::fs::Metadata> {
        file.metadata()
            .map_err(|_| unsafe_file("cannot inspect private trust file component", Some(path)))
    }

    fn verify_ancestor(file: &File, uid: u32, path: &Path) -> Result<()> {
        let meta = metadata(file, path)?;
        if !meta.is_dir() {
            return Err(unsafe_file(
                "private trust file ancestor is not a directory",
                Some(path),
            ));
        }
        let mode = meta.mode();
        let owner_trusted = meta.uid() == uid || meta.uid() == 0;
        let writable_by_others = mode & 0o022 != 0;
        let root_owned_sticky = meta.uid() == 0 && mode & 0o1000 != 0;
        if !owner_trusted || (writable_by_others && !root_owned_sticky) {
            return Err(unsafe_file(
                "private trust file ancestor is not privately controlled",
                Some(path),
            ));
        }
        #[cfg(target_os = "macos")]
        macos_acl::verify(file, uid, macos_acl::Scope::PrivateAncestor, path)?;
        Ok(())
    }

    fn verify_private_parent(file: &File, uid: u32, path: &Path) -> Result<()> {
        let meta = metadata(file, path)?;
        if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
            return Err(unsafe_file(
                "private trust file parent is not owner-only",
                Some(path),
            ));
        }
        #[cfg(target_os = "macos")]
        macos_acl::verify(file, uid, macos_acl::Scope::PrivateParent, path)?;
        Ok(())
    }

    fn verify_creation_parent(file: &File, uid: u32, path: &Path) -> Result<()> {
        let meta = metadata(file, path)?;
        if !meta.is_dir() || (meta.uid() != uid && meta.uid() != 0) || meta.mode() & 0o022 != 0 {
            return Err(unsafe_file(
                "private directory creation parent is writable by another user",
                Some(path),
            ));
        }
        #[cfg(target_os = "macos")]
        macos_acl::verify(file, uid, macos_acl::Scope::PrivateAncestor, path)?;
        Ok(())
    }

    fn read_leaf(
        parent: &File,
        leaf: &std::ffi::OsStr,
        path: &Path,
        max_bytes: u64,
        uid: u32,
    ) -> Result<Vec<u8>> {
        let leaf = CString::new(leaf.as_encoded_bytes())
            .map_err(|_| unsafe_file("private trust file leaf contains NUL", Some(path)))?;
        // SAFETY: `parent` is a retained directory capability and `leaf` is a
        // single NUL-free path component. O_NOFOLLOW prevents the final leaf
        // from being resolved as a symlink.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                leaf.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(unsafe_file("cannot open private trust file", Some(path)));
        }
        // SAFETY: successful openat transferred exactly one owned descriptor.
        let mut file = unsafe { File::from_raw_fd(fd) };
        let before = file
            .metadata()
            .map_err(|_| unsafe_file("cannot inspect private trust file", Some(path)))?;
        verify_private_leaf(&before, uid, path)?;
        #[cfg(target_os = "macos")]
        macos_acl::verify(&file, uid, macos_acl::Scope::PrivateLeaf, path)?;
        if before.len() > max_bytes {
            return Err(oversized_file(max_bytes, before.len(), path));
        }
        let capacity = usize::try_from(before.len())
            .map_err(|_| oversized_file(max_bytes, before.len(), path))?;
        let mut bytes = Vec::with_capacity(capacity);
        file.by_ref()
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| unsafe_file("cannot read private trust file", Some(path)))?;
        if bytes.len() as u64 > max_bytes {
            return Err(oversized_file(max_bytes, bytes.len() as u64, path));
        }
        let after = file
            .metadata()
            .map_err(|_| unsafe_file("cannot recheck private trust file", Some(path)))?;
        verify_private_leaf(&after, uid, path)?;
        #[cfg(target_os = "macos")]
        macos_acl::verify(&file, uid, macos_acl::Scope::PrivateLeaf, path)?;
        if !same_file(&before, &after) || after.len() != bytes.len() as u64 {
            return Err(raced_file(path));
        }
        Ok(bytes)
    }

    fn verify_private_leaf(meta: &std::fs::Metadata, uid: u32, path: &Path) -> Result<()> {
        if !meta.is_file() || meta.nlink() != 1 || meta.uid() != uid || meta.mode() & 0o077 != 0 {
            return Err(unsafe_file(
                "private trust file is not an owner-only single-link regular file",
                Some(path),
            ));
        }
        Ok(())
    }

    fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
        left.dev() == right.dev()
            && left.ino() == right.ino()
            && left.len() == right.len()
            && left.mtime() == right.mtime()
            && left.mtime_nsec() == right.mtime_nsec()
            && left.ctime() == right.ctime()
            && left.ctime_nsec() == right.ctime_nsec()
    }

    pub(super) fn verify_trusted_owned_regular_file_handle(file: &File) -> Result<()> {
        let path = Path::new("<retained-native-definition>");
        let uid = unsafe { libc::geteuid() };
        let meta = metadata(file, path)?;
        if !meta.is_file() || meta.nlink() != 1 || meta.uid() != uid || meta.mode() & 0o022 != 0 {
            return Err(unsafe_file(
                "trusted leaf is not an owner-controlled single-link regular file",
                Some(path),
            ));
        }
        #[cfg(target_os = "macos")]
        macos_acl::verify(file, uid, macos_acl::Scope::ExecutableLeaf, path)?;
        Ok(())
    }

    pub(super) fn verify_trusted_executable(path: &Path) -> Result<()> {
        let uid = unsafe { libc::geteuid() };
        let components = path_components(path)?;
        let root = cap_fs::open_ambient_dir(Path::new("/"), ambient_authority())
            .map_err(|_| unsafe_file("cannot open scheduler executable root", Some(path)))?;
        verify_executable_ancestor(&root, uid, path)?;
        let mut parent = root;
        for component in &components[..components.len() - 1] {
            let next = cap_fs::open_dir_nofollow(&parent, Path::new(component)).map_err(|_| {
                unsafe_file(
                    "scheduler executable has a missing, symlinked, or unsafe ancestor",
                    Some(path),
                )
            })?;
            verify_executable_ancestor(&next, uid, path)?;
            parent = next;
        }
        let leaf = CString::new(
            components
                .last()
                .expect("nonempty components")
                .as_encoded_bytes(),
        )
        .map_err(|_| unsafe_file("scheduler executable leaf contains NUL", Some(path)))?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                leaf.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(unsafe_file("cannot open scheduler executable", Some(path)));
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = metadata(&file, path)?;
        if !metadata.is_file()
            || (metadata.uid() != uid && metadata.uid() != 0)
            || metadata.mode() & 0o022 != 0
            || metadata.mode() & 0o111 == 0
        {
            return Err(unsafe_file(
                "scheduler executable is not a trusted executable regular file",
                Some(path),
            ));
        }
        #[cfg(target_os = "macos")]
        macos_acl::verify(&file, uid, macos_acl::Scope::ExecutableLeaf, path)?;
        Ok(())
    }

    fn verify_executable_ancestor(file: &File, uid: u32, path: &Path) -> Result<()> {
        let metadata = metadata(file, path)?;
        let safe_root_sticky = metadata.uid() == 0 && metadata.mode() & 0o1000 != 0;
        if !metadata.is_dir()
            || (metadata.uid() != uid && metadata.uid() != 0)
            || (metadata.mode() & 0o022 != 0 && !safe_root_sticky)
        {
            return Err(unsafe_file(
                "scheduler executable ancestor is writable by an untrusted principal",
                Some(path),
            ));
        }
        #[cfg(target_os = "macos")]
        macos_acl::verify(file, uid, macos_acl::Scope::ExecutableAncestor, path)?;
        Ok(())
    }
}

#[cfg(windows)]
mod windows {
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
            CloseHandle, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
            OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING,
        },
        Security::Authorization::{
            ConvertStringSidToSidW, GetSecurityInfo, SE_FILE_OBJECT, SetSecurityInfo,
        },
        Security::{
            ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_REVISION, ACL_SIZE_INFORMATION,
            AddAccessAllowedAce, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
            GetLengthSid, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
            GetSecurityDescriptorOwner, GetTokenInformation, INHERIT_ONLY_ACE, InitializeAcl,
            InitializeSecurityDescriptor, IsValidSid, IsWellKnownSid, OWNER_SECURITY_INFORMATION,
            PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED,
            SECURITY_DESCRIPTOR, SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
            SetSecurityDescriptorOwner, TOKEN_QUERY, TOKEN_USER, TokenUser,
            WinBuiltinAdministratorsSid, WinLocalSystemSid,
        },
        Storage::FileSystem::{
            FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_BASIC_INFO, FILE_LIST_DIRECTORY,
            FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            FILE_TRAVERSE, FileBasicInfo, GetFileInformationByHandleEx, READ_CONTROL, SYNCHRONIZE,
        },
        System::{
            IO::IO_STATUS_BLOCK,
            Threading::{GetCurrentProcess, OpenProcessToken},
            WindowsProgramming::FILE_CREATED,
        },
    };

    use super::{
        NewPrivateObjectKind, Result, StoreDirectory, oversized_file, raced_file, unsafe_file,
    };

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

    pub(super) fn create_new_private_at(
        parent: &File,
        leaf: &OsStr,
        kind: NewPrivateObjectKind,
    ) -> std::io::Result<File> {
        if crate::cas::windows_directory_handle_identity(parent).is_none() {
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
        let descriptor_ptr: PSECURITY_DESCRIPTOR =
            (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast();
        // SAFETY: `descriptor_ptr` is the Windows binding's erased mutable
        // pointer to the absolute descriptor. It, the aligned ACL, and the
        // owner SID remain live through NtCreateFile below.
        if unsafe { InitializeSecurityDescriptor(descriptor_ptr, SECURITY_DESCRIPTOR_REVISION) }
            == 0
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
        let mut attributes = OBJECT_ATTRIBUTES {
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
                &mut attributes,
                &mut status_block,
                ptr::null_mut(),
                file_attributes,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
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
                crate::cas::windows_directory_handle_identity(&file).is_some()
            }
            NewPrivateObjectKind::RegularFile => {
                crate::cas::windows_regular_file_handle_identity(&file).is_some()
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
            || unsafe {
                AddAccessAllowedAce(acl_ptr, ACL_REVISION, FILE_ALL_ACCESS, owner.as_psid())
            } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok((acl, acl_ptr))
    }

    pub(super) fn verify_owner_private_handle(file: &File) -> Result<()> {
        let owner = CurrentUserSid::current()
            .map_err(|_| unsafe_file("cannot determine current Windows user", None))?;
        verify_owner_only(file, &owner, Path::new("<retained>"))
    }

    pub(super) fn verify_current_owner_handle(file: &File) -> Result<()> {
        let owner = CurrentUserSid::current()
            .map_err(|_| unsafe_file("cannot determine current Windows user", None))?;
        verify_current_owner(file, &owner, Path::new("<retained>"))
    }

    pub(super) fn verify_private_directory_handle(directory: &StoreDirectory) -> Result<()> {
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

    pub(super) fn restrict_existing_private_handle(file: &File) -> Result<()> {
        let owner = CurrentUserSid::current()
            .map_err(|_| unsafe_file("cannot determine current Windows user", None))?;
        let directory = crate::cas::windows_directory_handle_identity(file);
        let regular = crate::cas::windows_regular_file_handle_identity(file);
        match (directory, regular) {
            (Some(_), None) | (None, Some(_)) => {}
            _ => {
                return Err(unsafe_file(
                    "private object is not a retained directory or regular file",
                    None,
                ));
            }
        }
        verify_current_owner(file, &owner, Path::new("<retained>"))?;
        let (_acl, acl_ptr) = private_acl(&owner)
            .map_err(|_| unsafe_file("cannot construct private Windows ACL", None))?;
        // SAFETY: `file` is the retained, owner-validated object opened with
        // READ_CONTROL|WRITE_DAC|FILE_READ_ATTRIBUTES by its caller; no path
        // is reopened or adopted before this DACL-only update.
        let status = unsafe {
            SetSecurityInfo(
                file.as_raw_handle() as HANDLE,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                acl_ptr,
                ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err(unsafe_file("cannot set private Windows ACL", None));
        }
        verify_owner_only(file, &owner, Path::new("<retained>"))
    }

    pub(super) fn verify_private_directory(path: &Path) -> Result<StoreDirectory> {
        let (root_path, components) = windows_absolute_components(path)?;
        let owner = CurrentUserSid::current()
            .map_err(|_| unsafe_file("cannot determine current Windows user", Some(path)))?;
        let root = cap_fs::open_ambient_dir(&root_path, ambient_authority())
            .map_err(|_| unsafe_file("cannot open Windows filesystem root", Some(path)))?;
        verify_real_directory(&root, path)?;
        verify_trusted_acl(&root, &owner, path, true)?;
        let mut directory = root;
        for (index, component) in components.iter().enumerate() {
            let next =
                cap_fs::open_dir_nofollow(&directory, Path::new(component)).map_err(|_| {
                    unsafe_file(
                        "private directory has a missing, reparse, or unsafe component",
                        Some(path),
                    )
                })?;
            verify_real_directory(&next, path)?;
            if index + 1 < components.len() {
                verify_trusted_acl(&next, &owner, path, true)?;
            }
            directory = next;
        }
        verify_owner_only(&directory, &owner, path)?;
        StoreDirectory::from_retained(directory, path.to_path_buf())
    }

    pub(super) fn verify_private_creation_parent(path: &Path) -> Result<StoreDirectory> {
        let (root_path, components) = windows_absolute_components(path)?;
        let owner = CurrentUserSid::current()
            .map_err(|_| unsafe_file("cannot determine current Windows user", Some(path)))?;
        let root = cap_fs::open_ambient_dir(&root_path, ambient_authority())
            .map_err(|_| unsafe_file("cannot open Windows filesystem root", Some(path)))?;
        verify_real_directory(&root, path)?;
        verify_trusted_acl(&root, &owner, path, true)?;
        let mut directory = root;
        for component in &components {
            let next = cap_fs::open_dir_nofollow(&directory, Path::new(component)).map_err(|_| {
                unsafe_file(
                    "private directory creation parent has a missing, reparse, or unsafe component",
                    Some(path),
                )
            })?;
            verify_real_directory(&next, path)?;
            verify_trusted_acl(&next, &owner, path, true)?;
            directory = next;
        }
        StoreDirectory::from_retained(directory, path.to_path_buf())
    }

    pub(super) fn read_private_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
        let (root_path, components) = windows_absolute_components(path)?;
        let owner = CurrentUserSid::current()
            .map_err(|_| unsafe_file("cannot determine current Windows user", Some(path)))?;
        let root = cap_fs::open_ambient_dir(&root_path, ambient_authority())
            .map_err(|_| unsafe_file("cannot open Windows filesystem root", Some(path)))?;
        verify_real_directory(&root, path)?;
        verify_trusted_acl(&root, &owner, path, true)?;
        let mut parent = root;
        for component in &components[..components.len() - 1] {
            let next = cap_fs::open_dir_nofollow(&parent, Path::new(component)).map_err(|_| {
                unsafe_file(
                    "private trust file has a missing, reparse, or unsafe ancestor",
                    Some(path),
                )
            })?;
            verify_real_directory(&next, path)?;
            verify_trusted_acl(&next, &owner, path, true)?;
            parent = next;
        }
        verify_owner_only(&parent, &owner, path)?;
        read_private_leaf(
            &parent,
            components.last().expect("nonempty components"),
            path,
            max_bytes,
            &owner,
        )
    }

    pub(super) fn read_private_file_at(
        parent: &StoreDirectory,
        name: &str,
        max_bytes: u64,
    ) -> Result<Vec<u8>> {
        let leaf = direct_leaf(name, parent.path())?;
        let owner = CurrentUserSid::current().map_err(|_| {
            unsafe_file("cannot determine current Windows user", Some(parent.path()))
        })?;
        let handle = parent.root_handle();
        verify_real_directory(&handle, parent.path())?;
        verify_owner_only(&handle, &owner, parent.path())?;
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
            crate::cas::windows_regular_file_handle_identity(&file).ok_or_else(|| {
                unsafe_file(
                    "private trust file is reparse, nonregular, or hardlinked",
                    Some(path),
                )
            })?;
        verify_owner_only(&file, &owner, path)?;
        let before = file
            .metadata()
            .map_err(|_| unsafe_file("cannot inspect private trust file", Some(path)))?;
        let before_change_time = change_time(&file, path)?;
        if before.len() > max_bytes {
            return Err(oversized_file(max_bytes, before.len(), path));
        }
        let mut body = Vec::with_capacity(
            usize::try_from(before.len())
                .map_err(|_| oversized_file(max_bytes, before.len(), path))?,
        );
        file.by_ref()
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut body)
            .map_err(|_| unsafe_file("cannot read private trust file", Some(path)))?;
        if body.len() as u64 > max_bytes {
            return Err(oversized_file(max_bytes, body.len() as u64, path));
        }
        let after_identity =
            crate::cas::windows_regular_file_handle_identity(&file).ok_or_else(|| {
                unsafe_file(
                    "private trust file changed type while it was read",
                    Some(path),
                )
            })?;
        let after = file
            .metadata()
            .map_err(|_| unsafe_file("cannot recheck private trust file", Some(path)))?;
        let after_change_time = change_time(&file, path)?;
        verify_owner_only(&file, &owner, path)?;
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

    pub(super) fn verify_trusted_executable(path: &Path) -> Result<()> {
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
        if crate::cas::windows_regular_file_handle_identity(&file).is_none() {
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
        let mut basic = FILE_BASIC_INFO::default();
        // SAFETY: `file` retains a valid Windows handle; `basic` is writable
        // storage exactly matching FileBasicInfo's required buffer layout.
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle() as HANDLE,
                FileBasicInfo,
                (&mut basic as *mut FILE_BASIC_INFO).cast(),
                mem::size_of::<FILE_BASIC_INFO>() as u32,
            )
        } == 0
        {
            return Err(unsafe_file(
                "cannot inspect private trust file Windows change time",
                Some(path),
            ));
        }
        Ok(basic.ChangeTime)
    }

    fn verify_real_directory(directory: &File, path: &Path) -> Result<()> {
        if crate::cas::windows_directory_handle_identity(directory).is_none() {
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
        let owner_ok = unsafe {
            GetSecurityDescriptorOwner(returned, &mut actual_owner, &mut owner_defaulted)
        } != 0
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
        if unsafe {
            GetSecurityDescriptorOwner(descriptor, &mut actual_owner, &mut owner_defaulted)
        } == 0
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
            if unsafe { GetSecurityDescriptorOwner(descriptor, &mut owner, &mut owner_defaulted) }
                == 0
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
                    && untrusted_effective_mutation(
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
        let converted =
            unsafe { ConvertStringSidToSidW(text.as_mut_ptr(), &mut trusted_installer) };
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
            let _ =
                unsafe { GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut needed) };
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
}
