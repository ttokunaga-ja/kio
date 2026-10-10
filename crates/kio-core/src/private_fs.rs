//! Read small device-local trust files without accepting a path that another
//! local user can replace while it is being inspected.
//!
//! This module deliberately has a narrower contract than general file I/O.
//! Callers use it for credentials that authenticate a local peer (for example,
//! a private CA certificate), so a convenient path based `read` would turn a
//! writable ancestor or a symlink race into a trust-boundary bypass.

use std::{ffi::OsString, path::Path};

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
/// absolute pathname. The final parent and leaf must be owned by the current
/// user and owner-only. Ancestors must be owned by the current user or root and
/// not writable by another user, except for root-owned sticky directories such
/// as the OS temporary directory. No permissions are repaired by this API.
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
    native::read_private_file_at(parent, name, max_bytes)
}

/// Resolve the sole supported inherited-directory spelling as a retained
/// capability root and a validated relative suffix.
///
/// This deliberately does not resolve `/dev/fd` as an ordinary path. The raw
/// Unix bytes must be exactly `/dev/fd/<canonical decimal fd>` followed by
/// optional ordinary name components. The descriptor is duplicated with
/// close-on-exec before it is inspected, so later operations retain the
/// duplicated directory rather than reopening the magic path.
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
/// `path` must be absolute. Every ancestor follows the same trusted-owner,
/// no-untrusted-write policy as [`read_private_file`], and the final directory
/// must be owned by the current user and owner-only (including macOS ACL or
/// Windows DACL checks). This function never creates, repairs, or locks an
/// entry.
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
