//! Read protected Kio objects through retained, no-follow directory capabilities.
//!
//! Owner-private checks apply to Kio objects, while external ancestor permissions
//! are outside the storage admission policy. Retained handles prevent a path
//! replacement from redirecting an operation into another tree.

use std::{
    ffi::OsString,
    path::{Component, Path},
};

use serde_json::json;

use crate::platform::private_fs as native;
use crate::{ExitCode, KioError, Result};
use crate::{control::ControlHandle, store_dir::StoreDirectory};

const PRIVATE_FILE_ERROR: &str = "KIO-E-PRIVATE-FILE-UNSAFE-001";
const PRIVATE_FILE_OVERSIZE: &str = "KIO-E-PRIVATE-FILE-OVERSIZE-001";
const PRIVATE_FILE_RACE: &str = "KIO-E-PRIVATE-FILE-RACE-001";

/// Read an owner-private, single-link regular file through retained directory
/// capabilities.
///
/// `path` must be absolute. Every component is opened with no-follow semantics
/// from the retained root capability; the final leaf is never reopened by its
/// absolute pathname. The leaf must be owned by the current user and owner-only.
/// External ancestors, including the parent, have no owner/mode/ACL admission
/// requirement. No permissions are repaired by this API.
pub fn read_private_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    read_private_file_with_control(path, max_bytes, &ControlHandle::default())
}

pub fn read_private_file_with_control(
    path: &Path,
    max_bytes: u64,
    control: &ControlHandle,
) -> Result<Vec<u8>> {
    native::read_private_file(path, max_bytes, control.clone())
}

/// Read a direct owner-private regular-file child of a retained directory.
///
/// `name` must be one ordinary path component. The parent may be external; its
/// permissions are not admission authority. Callers using a Kio-owned parent
/// must separately verify it with [`verify_private_directory_handle`]. The leaf
/// is opened through the retained parent; its diagnostic path is never reopened.
pub fn read_private_file_at(
    parent: &StoreDirectory,
    name: &str,
    max_bytes: u64,
) -> Result<Vec<u8>> {
    read_private_file_at_path(parent, Path::new(name), max_bytes)
}

/// Preserve native OS filename bytes while opening exactly one private leaf
/// through the retained parent. No pathname ancestor is resolved here.
pub fn read_private_file_at_path(
    parent: &StoreDirectory,
    name: &Path,
    max_bytes: u64,
) -> Result<Vec<u8>> {
    native::read_private_file_at_path(parent, name, max_bytes)
}

/// Resolve the sole supported inherited-directory spelling as a retained
/// capability root and a validated relative suffix.
///
/// This deliberately does not resolve `/dev/fd` as an ordinary path. The raw
/// Unix bytes must be exactly `/dev/fd/<canonical decimal fd>` followed by
/// optional ordinary name components. The descriptor is duplicated with
/// close-on-exec before it is inspected, so later operations retain the
/// duplicated directory rather than reopening the magic path. This resolver
/// verifies only the directory capability: callers that use it as a Kio-owned
/// object must verify its protection, including when the suffix is empty.
pub fn resolve_inherited_private_root(
    path: &Path,
) -> Result<Option<(StoreDirectory, Vec<OsString>)>> {
    resolve_inherited_private_root_with_control(path, &ControlHandle::default())
}

pub fn resolve_inherited_private_root_with_control(
    path: &Path,
    control: &ControlHandle,
) -> Result<Option<(StoreDirectory, Vec<OsString>)>> {
    native::resolve_inherited_private_root(path, control.clone())
}

/// Retain an existing owner-private directory after validating it without
/// following any path component. The returned [`StoreDirectory`] keeps the
/// final directory handle; its display path is diagnostic only and is never
/// reopened for later mutations.
///
/// `path` must be absolute. External ancestor permissions are not admission
/// authority. The final directory must be owned by the current user and
/// owner-only (including macOS ACL or Windows DACL checks). This function never
/// creates, repairs, or locks an entry.
pub fn verify_private_directory(path: &Path) -> Result<StoreDirectory> {
    verify_private_directory_with_control(path, &ControlHandle::default())
}

pub fn verify_private_directory_with_control(
    path: &Path,
    control: &ControlHandle,
) -> Result<StoreDirectory> {
    native::verify_private_directory(path, control.clone())
}

/// Recheck that an already retained directory capability remains owner-private.
///
/// This validates the retained handle itself and never resolves its diagnostic
/// path. It is intended for callers which have traversed a validated suffix
/// beneath an inherited directory capability.
pub fn verify_private_directory_handle(directory: &StoreDirectory) -> Result<()> {
    native::verify_private_directory_handle(directory)
}

/// Retain a caller-declared external directory without changing it. Its path
/// may contain aliases; the followed result is compared with a strict open of
/// its canonical name before its handle becomes the private-child boundary.
/// Callers must never pass a Kio-created directory as this external boundary.
pub fn retain_external_directory(path: &Path) -> Result<StoreDirectory> {
    retain_external_directory_with_control(path, &ControlHandle::default())
}

pub fn retain_external_directory_with_control(
    path: &Path,
    control: &ControlHandle,
) -> Result<StoreDirectory> {
    native::retain_external_directory(path, control.clone())
}

/// Retain a configured external base, creating only its absent components.
/// Existing external names and permissions are left untouched; each component
/// this call creates is private and verified through its retained handle.
pub fn retain_or_create_external_directory_with_control(
    base: &Path,
    control: &ControlHandle,
) -> Result<StoreDirectory> {
    if !base.is_absolute()
        || base
            .components()
            .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
    {
        return Err(unsafe_file(
            "external base must be absolute and normalized",
            Some(base),
        ));
    }
    let mut current = base.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match std::fs::symlink_metadata(&current) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let leaf = current.file_name().ok_or_else(|| {
                    unsafe_file("external base has no existing ancestor", Some(base))
                })?;
                missing.push(leaf.to_os_string());
                current = current
                    .parent()
                    .ok_or_else(|| {
                        unsafe_file("external base has no existing ancestor", Some(base))
                    })?
                    .to_path_buf();
            }
            Err(error) => {
                return Err(crate::KioError::io(
                    error.to_string(),
                    current.display().to_string(),
                ));
            }
        }
    }
    let mut directory = retain_external_directory_with_control(&current, control)?;
    for leaf in missing.into_iter().rev() {
        let name = Path::new(&leaf);
        let (handle, created) = if directory.contains_entry(name)? {
            (directory.open_directory(name)?, false)
        } else {
            match directory.create_directory(name) {
                Ok(handle) => (handle, true),
                Err(error) => (directory.open_directory(name).map_err(|_| error)?, false),
            }
        };
        let next = StoreDirectory::from_retained_with_control(
            handle,
            directory.path().join(&leaf),
            control.clone(),
        )?;
        if created {
            verify_private_directory_handle(&next)?;
        }
        directory = next;
    }
    Ok(directory)
}

/// Traverse a Kio-created suffix beneath a retained external or private
/// directory. Every component is opened without following links and checked
/// for current-owner private protection before the next component is used.
pub fn verify_private_directory_at(
    parent: &StoreDirectory,
    relative: &Path,
) -> Result<StoreDirectory> {
    let mut directory = parent.clone();
    let mut count = 0;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(unsafe_file(
                "private directory suffix contains an unsafe component",
                Some(relative),
            ));
        };
        count += 1;
        let child = directory.open_directory(Path::new(name))?;
        directory = StoreDirectory::from_retained_with_control(
            child,
            directory.path().join(name),
            parent.control().clone(),
        )?;
        verify_private_directory_handle(&directory)?;
    }
    if count == 0 {
        return Err(unsafe_file(
            "private directory suffix is empty",
            Some(relative),
        ));
    }
    Ok(directory)
}

/// Verify owner-only protection on an already retained single-link regular file.
/// Callers must separately bind its parent and open without following links.
/// This never repairs permissions or reopens the diagnostic path.
pub fn verify_private_regular_file_handle(file: &std::fs::File) -> Result<()> {
    native::verify_private_regular_file_handle(file)
}

/// Retain an existing external directory for creating owner-private children.
///
/// The path must be absolute and non-symlinked. Its owner, mode and ACL are not
/// admission authority. The retained handle prevents later path re-resolution
/// before private children are created. Callers must separately protect and
/// verify the Kio-owned children.
///
/// This function only verifies an existing parent. It never creates, repairs,
/// locks, or otherwise mutates filesystem state.
pub fn verify_private_creation_parent(path: &Path) -> Result<StoreDirectory> {
    verify_private_creation_parent_with_control(path, &ControlHandle::default())
}

pub fn verify_private_creation_parent_with_control(
    path: &Path,
    control: &ControlHandle,
) -> Result<StoreDirectory> {
    native::verify_private_creation_parent(path, control.clone())
}

/// Verify an already opened native-definition leaf without changing it.
/// Unix shared-read modes such as `0644` are allowed, but only the current
/// owner may mutate the single-link regular file. Windows uses the existing
/// owner-private DACL policy. Callers must separately admit its ancestry and
/// open the leaf without following links.
pub fn verify_trusted_owned_regular_file_handle(file: &std::fs::File) -> Result<()> {
    native::verify_trusted_owned_regular_file_handle(file)
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
    native::verify_trusted_executable(&canonical)?;
    Ok(canonical)
}

pub(crate) fn unsafe_file(message: &str, path: Option<&Path>) -> KioError {
    KioError::new(
        PRIVATE_FILE_ERROR,
        message,
        json!({ "path": path.map(|value| value.display().to_string()) }),
        ExitCode::PermanentFailure,
    )
}

pub(crate) fn oversized_file(max_bytes: u64, actual_bytes: u64, path: &Path) -> KioError {
    KioError::new(
        PRIVATE_FILE_OVERSIZE,
        "private trust file exceeds its byte limit",
        json!({ "path": path.display().to_string(), "max_bytes": max_bytes, "actual_bytes": actual_bytes }),
        ExitCode::PermanentFailure,
    )
}

pub(crate) fn raced_file(path: &Path) -> KioError {
    KioError::new(
        PRIVATE_FILE_RACE,
        "private trust file changed while it was read",
        json!({ "path": path.display().to_string() }),
        ExitCode::PermanentFailure,
    )
}
