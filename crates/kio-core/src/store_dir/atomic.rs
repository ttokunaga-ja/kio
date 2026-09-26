//! Bounded crash recovery for one-file publication and removal.
//!
//! This module deliberately owns only the private `.kio-atomic` namespace.
//! Callers retain both the owner and target directory capabilities; no recovery
//! decision is made from a diagnostic pathname.

use std::{
    fs::File,
    io::{Read, Write},
    path::{Component, Path},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{Publication, StoreDirectory};
use crate::management::DirectoryIdentity;
use crate::{ExitCode, KioError, Result};

pub const ATOMIC_WORKSPACE_DIR: &str = ".kio-atomic";

const GATE: &str = ".gate";
const WRITE: &str = "write";
const REMOVE_PENDING: &str = "remove.pending";
const REMOVE_READY: &str = "remove.json";
const REMOVED: &str = "removed";
const MAX_INTENT_BYTES: u64 = 64 * 1024;
const MAX_RELATIVE_DEPTH: usize = 128;
const MAX_RELATIVE_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtomicWorkspaceState {
    Absent,
    Clean,
    Pending,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveIntent {
    version: u8,
    owner_id: DirectoryIdentity,
    target_root_id: DirectoryIdentity,
    parent_chain_ids: Vec<DirectoryIdentity>,
    relative: String,
    source_id: FileSystemIdentity,
    sha256: String,
    length: u64,
    max_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "platform", rename_all = "snake_case", deny_unknown_fields)]
enum FileSystemIdentity {
    Unix {
        #[serde(with = "crate::identity_serde::u64_hex")]
        device: u64,
        #[serde(with = "crate::identity_serde::u64_hex")]
        inode: u64,
    },
    Windows {
        #[serde(with = "crate::identity_serde::u32_hex")]
        volume_serial_number: u32,
        #[serde(with = "crate::identity_serde::u64_hex")]
        file_index: u64,
    },
}

fn failure(message: impl Into<String>) -> KioError {
    KioError::new(
        "KIO-E-ATOMIC-WORKSPACE-UNSAFE-001",
        message,
        serde_json::json!({"workspace": ATOMIC_WORKSPACE_DIR}),
        ExitCode::PermanentFailure,
    )
}

fn io_failure(message: impl Into<String>, error: std::io::Error) -> KioError {
    KioError::new(
        "KIO-E-ATOMIC-WORKSPACE-IO-001",
        format!("{}: {error}", message.into()),
        serde_json::json!({"workspace": ATOMIC_WORKSPACE_DIR}),
        ExitCode::Failure,
    )
}

fn workspace_existing(owner: &StoreDirectory) -> Result<Option<StoreDirectory>> {
    let relative = Path::new(ATOMIC_WORKSPACE_DIR);
    if !owner.contains_entry(relative)? {
        return Ok(None);
    }
    let handle = owner.open_directory(relative)?;
    let workspace = StoreDirectory::from_retained(handle, owner.path().join(ATOMIC_WORKSPACE_DIR))?;
    ensure_private_workspace(&workspace)?;
    Ok(Some(workspace))
}

fn workspace_create(owner: &StoreDirectory) -> Result<StoreDirectory> {
    if let Some(existing) = workspace_existing(owner)? {
        return Ok(existing);
    }
    match owner.create_directory(Path::new(ATOMIC_WORKSPACE_DIR)) {
        Ok(handle) => {
            let workspace =
                StoreDirectory::from_retained(handle, owner.path().join(ATOMIC_WORKSPACE_DIR))?;
            ensure_private_workspace(&workspace)?;
            Ok(workspace)
        }
        // A concurrent initializer may have won the create-only race.  Reopen
        // only through the retained owner and then require the regular gate.
        Err(_) => {
            workspace_existing(owner)?.ok_or_else(|| failure("cannot create atomic workspace"))
        }
    }
}

fn known_leaf(name: &std::ffi::OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(GATE | WRITE | REMOVE_PENDING | REMOVE_READY | REMOVED)
    )
}

fn inventory(workspace: &StoreDirectory) -> Result<Vec<String>> {
    let entries = workspace.entries(Path::new(""))?;
    if entries.len() > 5 {
        return Err(failure("atomic workspace exceeds its entry bound"));
    }
    let mut names = Vec::with_capacity(entries.len());
    for entry in entries {
        if !entry.is_regular_file || !known_leaf(&entry.name) {
            return Err(failure(
                "atomic workspace contains an unknown or unsafe entry",
            ));
        }
        let name = entry
            .name
            .to_str()
            .ok_or_else(|| failure("atomic workspace leaf is not UTF-8"))?;
        // `removed` is an already-public working file moved into the private
        // namespace after a ready intent. Its original mode is evidence, not a
        // workspace-journal permission failure. Every other artifact is
        // created in this workspace and must remain owner-private.
        if name != REMOVED {
            workspace.ensure_owner_private(Path::new(name))?;
        }
        if name == GATE {
            // Do not call `read_optional` here: on Windows the exclusive gate
            // lock makes a ReadFile range request fail with ERROR_LOCK_VIOLATION.
            // Opening the retained regular file and inspecting its metadata is
            // sufficient for the fixed zero-length gate invariant.
            let gate = workspace.open_regular_read(Path::new(GATE), 0)?;
            if gate
                .metadata()
                .map_err(|error| io_failure("cannot inspect atomic workspace gate", error))?
                .len()
                != 0
            {
                return Err(failure("atomic workspace gate is not empty"));
            }
        }
        names.push(name.to_owned());
    }
    names.sort();
    Ok(names)
}

fn ensure_private_workspace(workspace: &StoreDirectory) -> Result<()> {
    let metadata = workspace
        .root_handle()
        .metadata()
        .map_err(|e| io_failure("cannot inspect atomic workspace", e))?;
    if !metadata.is_dir() {
        return Err(failure("atomic workspace is not a directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let uid = unsafe { libc::geteuid() };
        if metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            return Err(failure("atomic workspace is not owner-private"));
        }
    }
    #[cfg(windows)]
    crate::private_fs::verify_owner_private_handle(&workspace.root_handle())?;
    Ok(())
}

fn valid_relative(relative: &Path) -> Result<String> {
    let raw = relative
        .to_str()
        .ok_or_else(|| failure("atomic target path must be UTF-8"))?;
    if raw.len() > MAX_RELATIVE_BYTES || raw.as_bytes().contains(&0) {
        return Err(failure(
            "atomic target path exceeds its byte or encoding bound",
        ));
    }
    if raw
        .split(|ch| ch == '/' || (cfg!(windows) && ch == '\\'))
        .any(|part| part.is_empty() || part == "." || part == "..")
        || (!cfg!(windows) && raw.contains('\\'))
    {
        return Err(failure("atomic target path must be normalized"));
    }
    let mut parts = Vec::new();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(failure("atomic target path must be relative"));
        };
        parts.push(
            part.to_str()
                .ok_or_else(|| failure("atomic target path must be UTF-8"))?,
        );
    }
    if parts.is_empty() || parts.len() > MAX_RELATIVE_DEPTH {
        return Err(failure("atomic target path depth is outside the bound"));
    }
    Ok(parts.join("/"))
}

fn validate_target_namespace(
    owner: &StoreDirectory,
    target: &StoreDirectory,
    relative: &str,
) -> Result<()> {
    if directory_id(owner)? == directory_id(target)?
        && relative
            .split('/')
            .next()
            .is_some_and(|leaf| leaf.eq_ignore_ascii_case(ATOMIC_WORKSPACE_DIR))
    {
        return Err(failure("atomic target overlaps its private workspace"));
    }
    Ok(())
}

fn directory_id(directory: &StoreDirectory) -> Result<DirectoryIdentity> {
    crate::management::directory_identity_from_handle(&directory.root_handle())
}

fn file_id(file: &File) -> Result<FileSystemIdentity> {
    let metadata = file
        .metadata()
        .map_err(|e| io_failure("cannot inspect retained file", e))?;
    if !metadata.is_file() {
        return Err(failure("atomic source is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.nlink() != 1 {
            return Err(failure("atomic source is not a single-link regular file"));
        }
        Ok(FileSystemIdentity::Unix {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(windows)]
    {
        crate::cas::windows_regular_file_handle_identity(file)
            .map(|id| {
                let (volume_serial_number, file_index) = id.atomic_recovery_components();
                FileSystemIdentity::Windows {
                    volume_serial_number,
                    file_index,
                }
            })
            .ok_or_else(|| failure("atomic source has no single-link identity"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        Err(failure(
            "atomic file identity is unsupported on this platform",
        ))
    }
}

fn hash_file(file: &mut File, max_bytes: u64) -> Result<(String, u64)> {
    let before_id = file_id(file)?;
    let before = file
        .metadata()
        .map_err(|e| io_failure("cannot inspect source length", e))?;
    if before.len() > max_bytes {
        return Err(failure("atomic source exceeds the declared byte limit"));
    }
    let mut digest = Sha256::new();
    let mut reader = file.take(max_bytes.saturating_add(1));
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|e| io_failure("cannot read atomic source", e))?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| failure("atomic source length overflow"))?;
        if total > max_bytes {
            return Err(failure("atomic source exceeds the declared byte limit"));
        }
        digest.update(&buffer[..read]);
    }
    let after = file
        .metadata()
        .map_err(|e| io_failure("cannot re-inspect atomic source", e))?;
    if before_id != file_id(file)? || before.len() != after.len() || after.len() != total {
        return Err(failure("atomic source changed while it was hashed"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if before.ctime() != after.ctime() || before.ctime_nsec() != after.ctime_nsec() {
            return Err(failure("atomic source changed while it was hashed"));
        }
    }
    Ok((crate::cas::lower_hex(&digest.finalize()), total))
}

fn parent_chain(
    target: &StoreDirectory,
    relative: &Path,
) -> Result<(StoreDirectory, std::ffi::OsString, Vec<DirectoryIdentity>)> {
    let (parent, leaf) = target.retained_parent(relative)?;
    let mut ids = vec![directory_id(target)?];
    let mut current = target.clone();
    let components: Vec<_> = relative.components().collect();
    for component in &components[..components.len() - 1] {
        let Component::Normal(part) = component else {
            return Err(failure("atomic target path is not normalized"));
        };
        let handle = current.open_directory(Path::new(part))?;
        current = StoreDirectory::from_retained(handle, current.path().join(part))?;
        ids.push(directory_id(&current)?);
    }
    if directory_id(&parent)? != *ids.last().expect("target root is present") {
        return Err(failure(
            "atomic target parent changed while it was resolved",
        ));
    }
    Ok((parent, leaf, ids))
}

fn same_volume(left: &StoreDirectory, right: &StoreDirectory) -> Result<()> {
    let left = directory_id(left)?;
    let right = directory_id(right)?;
    let same = match (&left, &right) {
        (
            DirectoryIdentity::Unix { device: left, .. },
            DirectoryIdentity::Unix { device: right, .. },
        ) => left == right,
        (
            DirectoryIdentity::Windows {
                volume_serial_number: left,
                ..
            },
            DirectoryIdentity::Windows {
                volume_serial_number: right,
                ..
            },
        ) => left == right,
        _ => false,
    };
    if !same {
        return Err(failure("atomic operation crosses filesystem volumes"));
    }
    Ok(())
}

fn parse_ready(bytes: &[u8]) -> Result<RemoveIntent> {
    if bytes.len() as u64 > MAX_INTENT_BYTES {
        return Err(failure("atomic removal intent exceeds its byte limit"));
    }
    let intent: RemoveIntent = serde_json::from_slice(bytes)
        .map_err(|_| failure("atomic removal intent is not canonical JSON"))?;
    let depth = Path::new(&intent.relative).components().count();
    if intent.version != 1
        || intent.relative.len() > MAX_RELATIVE_BYTES
        || intent.parent_chain_ids.len() != depth
        || intent.sha256.len() != 64
        || intent.length > intent.max_bytes
        || !intent
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(failure("atomic removal intent has an invalid shape"));
    }
    if valid_relative(Path::new(&intent.relative))? != intent.relative
        || (intent.owner_id == intent.target_root_id
            && intent
                .relative
                .split('/')
                .next()
                .is_some_and(|leaf| leaf.eq_ignore_ascii_case(ATOMIC_WORKSPACE_DIR)))
    {
        return Err(failure(
            "atomic removal target is not canonical or overlaps its workspace",
        ));
    }
    if intent_bytes(&intent)? != bytes {
        return Err(failure("atomic removal intent is not canonical JSON"));
    }
    Ok(intent)
}

fn intent_bytes(intent: &RemoveIntent) -> Result<Vec<u8>> {
    let bytes = serde_jcs::to_vec(intent)
        .map_err(|_| failure("cannot canonicalize atomic removal intent"))?;
    if bytes.len() as u64 > MAX_INTENT_BYTES {
        return Err(failure("atomic removal intent exceeds its byte limit"));
    }
    Ok(bytes)
}

fn lock_workspace(workspace: &StoreDirectory) -> Result<File> {
    let gate = workspace.open_private_gate_raw(Path::new(GATE))?;
    gate.lock()
        .map_err(|e| io_failure("cannot acquire atomic workspace gate", e))?;
    if gate
        .metadata()
        .map_err(|e| io_failure("cannot inspect atomic workspace gate", e))?
        .len()
        != 0
    {
        return Err(failure("atomic workspace gate is not empty"));
    }
    Ok(gate)
}

fn remove_if_present(workspace: &StoreDirectory, name: &str) -> Result<bool> {
    if workspace.contains_entry(Path::new(name))? {
        workspace.remove_file(Path::new(name))?;
        return Ok(true);
    }
    Ok(false)
}

fn ready_intent_for_owner(
    owner: &StoreDirectory,
    workspace: &StoreDirectory,
) -> Result<Option<RemoveIntent>> {
    let Some(bytes) = workspace.read_optional(Path::new(REMOVE_READY), MAX_INTENT_BYTES)? else {
        return Ok(None);
    };
    let intent = parse_ready(&bytes)?;
    if intent.owner_id != directory_id(owner)? {
        return Err(failure(
            "removal intent owner does not match workspace owner",
        ));
    }
    Ok(Some(intent))
}

/// Read-only workspace validation.  It never creates a directory or gate,
/// locks a file, or removes crash residue.
pub(super) fn inspect(owner: &StoreDirectory) -> Result<AtomicWorkspaceState> {
    let Some(workspace) = workspace_existing(owner)? else {
        return Ok(AtomicWorkspaceState::Absent);
    };
    let names = inventory(&workspace)?;
    let ready = ready_intent_for_owner(owner, &workspace)?;
    if ready.is_some()
        && (names.iter().any(|name| name == WRITE)
            || names.iter().any(|name| name == REMOVE_PENDING))
    {
        return Err(failure(
            "ready removal intent conflicts with unpublishable residue",
        ));
    }
    if ready.is_none() && names.iter().any(|name| name == REMOVED) {
        return Err(failure("removal quarantine exists without a ready intent"));
    }
    if names.len() == 1 && names[0] == GATE {
        Ok(AtomicWorkspaceState::Clean)
    } else {
        Ok(AtomicWorkspaceState::Pending)
    }
}

/// Recover only residue owned by `owner`.  `false` means no workspace existed;
/// `true` means the workspace is clean after the operation.
pub(super) fn recover(owner: &StoreDirectory, allowed_targets: &[&StoreDirectory]) -> Result<bool> {
    let Some(workspace) = workspace_existing(owner)? else {
        return Ok(false);
    };
    let _gate = lock_workspace(&workspace)?;
    recover_locked(owner, &workspace, allowed_targets)
}

pub(super) fn write(
    owner: &StoreDirectory,
    target: &StoreDirectory,
    relative: &Path,
    bytes: &[u8],
    publication: Publication,
) -> Result<()> {
    let normalized = valid_relative(relative)?;
    validate_target_namespace(owner, target, &normalized)?;
    let (preflight_parent, _, _) = parent_chain(target, relative)?;
    same_volume(owner, &preflight_parent)?;
    let workspace = workspace_create(owner)?;
    let _gate = lock_workspace(&workspace)?;
    let _ = recover_locked(owner, &workspace, &[target])?;
    let (parent, leaf, _) = parent_chain(target, relative)?;
    same_volume(&workspace, &parent)?;
    let mut staged = workspace.create_private_file_raw(Path::new(WRITE))?;
    staged
        .write_all(bytes)
        .map_err(|e| io_failure("cannot write atomic publication artifact", e))?;
    staged
        .sync_all()
        .map_err(|e| io_failure("cannot sync atomic publication artifact", e))?;
    drop(staged);
    workspace.sync()?;
    crate::durability::checkpoint(crate::durability::DurabilityPoint::AtomicWriteStaged)?;
    workspace.rename_regular_between_raw(
        Path::new(WRITE),
        &parent,
        Path::new(&leaf),
        publication,
    )?;
    workspace.sync()?;
    parent.sync()?;
    crate::durability::checkpoint(crate::durability::DurabilityPoint::AtomicWritePublished)
}

pub(super) fn remove(
    owner: &StoreDirectory,
    target: &StoreDirectory,
    relative: &Path,
    expected: &[u8],
    max_bytes: u64,
) -> Result<()> {
    let relative_text = valid_relative(relative)?;
    validate_target_namespace(owner, target, &relative_text)?;
    if expected.len() as u64 > max_bytes {
        return Err(failure("atomic expected bytes exceed removal limit"));
    }
    let (preflight_parent, _, _) = parent_chain(target, relative)?;
    same_volume(owner, &preflight_parent)?;
    let workspace = workspace_create(owner)?;
    let _gate = lock_workspace(&workspace)?;
    let _ = recover_locked(owner, &workspace, &[target])?;
    let (parent, leaf, chain) = parent_chain(target, relative)?;
    same_volume(&workspace, &parent)?;
    let mut source = parent.open_regular_for_removal(Path::new(&leaf), max_bytes)?;
    #[cfg(windows)]
    crate::private_fs::verify_current_owner_handle(&source)?;
    let source_id = file_id(&source)?;
    let (sha256, length) = hash_file(&mut source, max_bytes)?;
    let expected_hash = crate::cas::lower_hex(&Sha256::digest(expected));
    if sha256 != expected_hash || length != expected.len() as u64 {
        return Err(failure("atomic source differs from expected bytes"));
    }
    drop(source);
    let intent = RemoveIntent {
        version: 1,
        owner_id: directory_id(owner)?,
        target_root_id: directory_id(target)?,
        parent_chain_ids: chain,
        relative: relative_text,
        source_id,
        sha256,
        length,
        max_bytes,
    };
    let bytes = intent_bytes(&intent)?;
    let mut pending = workspace.create_private_file_raw(Path::new(REMOVE_PENDING))?;
    pending
        .write_all(&bytes)
        .map_err(|e| io_failure("cannot write removal boot intent", e))?;
    pending
        .sync_all()
        .map_err(|e| io_failure("cannot sync removal boot intent", e))?;
    drop(pending);
    workspace.sync()?;
    workspace.rename_regular_between_raw(
        Path::new(REMOVE_PENDING),
        &workspace,
        Path::new(REMOVE_READY),
        Publication::CreateOnly,
    )?;
    workspace.sync()?;
    crate::durability::checkpoint(crate::durability::DurabilityPoint::AtomicRemoveReady)?;
    let _ = recover_locked(owner, &workspace, &[target])?;
    Ok(())
}

fn recover_locked(
    owner: &StoreDirectory,
    workspace: &StoreDirectory,
    allowed_targets: &[&StoreDirectory],
) -> Result<bool> {
    let names = inventory(workspace)?;
    let ready_intent = ready_intent_for_owner(owner, workspace)?;
    let has_write = names.iter().any(|name| name == WRITE);
    let has_pending = names.iter().any(|name| name == REMOVE_PENDING);
    let has_removed = names.iter().any(|name| name == REMOVED);
    if ready_intent.is_some() && (has_write || has_pending) {
        return Err(failure(
            "ready removal intent conflicts with unpublishable residue",
        ));
    }
    if ready_intent.is_none() && has_removed {
        return Err(failure("removal quarantine exists without a ready intent"));
    }
    let Some(intent) = ready_intent else {
        let mut changed = false;
        if has_write {
            changed |= remove_if_present(workspace, WRITE)?;
        }
        if has_pending {
            changed |= remove_if_present(workspace, REMOVE_PENDING)?;
        }
        return Ok(changed);
    };
    // Drop the outer gate only after all recovery paths finish; this nested
    // implementation mirrors `recover` without attempting a second file lock.
    let mut target = None;
    for candidate in allowed_targets {
        if directory_id(candidate)? == intent.target_root_id && target.replace(*candidate).is_some()
        {
            return Err(failure(
                "removal intent target is duplicated in recovery authority",
            ));
        }
    }
    let target =
        target.ok_or_else(|| failure("removal intent target is not explicitly allowed"))?;
    let (parent, leaf, chain) = parent_chain(target, Path::new(&intent.relative))?;
    if chain != intent.parent_chain_ids {
        return Err(failure("removal intent target parent chain does not match"));
    }
    same_volume(workspace, &parent)?;
    let source = if parent.contains_entry(Path::new(&leaf))? {
        Some(parent.open_regular_for_removal(Path::new(&leaf), intent.max_bytes)?)
    } else {
        None
    };
    let removed = has_removed;
    #[cfg(windows)]
    let (mut quarantined, contents_already_verified) = match (source, removed) {
        (Some(mut source), false) => {
            crate::private_fs::verify_current_owner_handle(&source)?;
            if file_id(&source)? != intent.source_id
                || hash_file(&mut source, intent.max_bytes)?
                    != (intent.sha256.clone(), intent.length)
            {
                return Err(failure("removal source does not match its ready intent"));
            }
            parent.move_verified_regular_handle_to_raw(&source, workspace, Path::new(REMOVED))?;
            if file_id(&source)? != intent.source_id {
                return Err(failure(
                    "moved removal source no longer matches its ready intent",
                ));
            }
            crate::durability::checkpoint(
                crate::durability::DurabilityPoint::AtomicRemoveQuarantined,
            )?;
            (source, true)
        }
        (None, true) => (
            workspace.open_regular_for_removal(Path::new(REMOVED), intent.max_bytes)?,
            false,
        ),
        (None, false) => {
            remove_if_present(workspace, REMOVE_READY)?;
            return Ok(true);
        }
        (Some(_), true) => return Err(failure("both removal source and quarantine are present")),
    };
    #[cfg(windows)]
    {
        crate::private_fs::verify_current_owner_handle(&quarantined)?;
        if file_id(&quarantined)? != intent.source_id
            || (!contents_already_verified
                && hash_file(&mut quarantined, intent.max_bytes)? != (intent.sha256, intent.length))
        {
            return Err(failure(
                "removal quarantine does not match its ready intent",
            ));
        }
        crate::private_fs::restrict_existing_private_handle(&quarantined)?;
        drop(quarantined);
        remove_if_present(workspace, REMOVED)?;
        crate::durability::checkpoint(crate::durability::DurabilityPoint::AtomicRemoveDeleted)?;
        remove_if_present(workspace, REMOVE_READY)?;
        Ok(true)
    }
    #[cfg(not(windows))]
    match (source, removed) {
        (Some(mut source), false) => {
            if file_id(&source)? != intent.source_id
                || hash_file(&mut source, intent.max_bytes)?
                    != (intent.sha256.clone(), intent.length)
            {
                return Err(failure("removal source does not match its ready intent"));
            }
            drop(source);
            parent.rename_regular_between_raw(
                Path::new(&leaf),
                workspace,
                Path::new(REMOVED),
                Publication::CreateOnly,
            )?;
            crate::durability::checkpoint(
                crate::durability::DurabilityPoint::AtomicRemoveQuarantined,
            )?;
        }
        (None, true) => {}
        (None, false) => {
            remove_if_present(workspace, REMOVE_READY)?;
            return Ok(true);
        }
        (Some(_), true) => return Err(failure("both removal source and quarantine are present")),
    }
    #[cfg(not(windows))]
    {
        let mut quarantined =
            workspace.open_regular_for_removal(Path::new(REMOVED), intent.max_bytes)?;
        if file_id(&quarantined)? != intent.source_id
            || hash_file(&mut quarantined, intent.max_bytes)? != (intent.sha256, intent.length)
        {
            return Err(failure(
                "removal quarantine does not match its ready intent",
            ));
        }
        drop(quarantined);
        remove_if_present(workspace, REMOVED)?;
        crate::durability::checkpoint(crate::durability::DurabilityPoint::AtomicRemoveDeleted)?;
        remove_if_present(workspace, REMOVE_READY)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Write, path::Path};

    use tempfile::tempdir_in;

    use super::{
        AtomicWorkspaceState, Publication, RemoveIntent, StoreDirectory, directory_id, hash_file,
        inspect, intent_bytes, parent_chain, recover, remove, workspace_create, write,
    };

    #[test]
    fn remove_intent_jcs_preserves_all_identity_bits() {
        use super::{DirectoryIdentity, FileSystemIdentity};
        for value in [
            (1_u64 << 53) + 1,
            0x001d_0000_0001_2345,
            9_851_624_185_183_609,
            u64::MAX,
        ] {
            for (directory, source) in [
                (
                    DirectoryIdentity::Unix {
                        device: value,
                        inode: value,
                    },
                    FileSystemIdentity::Unix {
                        device: value,
                        inode: value,
                    },
                ),
                (
                    DirectoryIdentity::Windows {
                        volume_serial_number: u32::MAX,
                        file_index: value,
                    },
                    FileSystemIdentity::Windows {
                        volume_serial_number: u32::MAX,
                        file_index: value,
                    },
                ),
            ] {
                let mut intent = RemoveIntent {
                    version: 1,
                    owner_id: directory.clone(),
                    target_root_id: directory.clone(),
                    parent_chain_ids: vec![directory],
                    relative: "source".into(),
                    source_id: source,
                    sha256: "0".repeat(64),
                    length: 1,
                    max_bytes: 1,
                };
                let bytes = intent_bytes(&intent).unwrap();
                let recovered: RemoveIntent = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(recovered.owner_id, intent.owner_id);
                assert_eq!(recovered.target_root_id, intent.target_root_id);
                assert_eq!(recovered.parent_chain_ids, intent.parent_chain_ids);
                assert_eq!(recovered.source_id, intent.source_id);
                assert_eq!(intent_bytes(&recovered).unwrap(), bytes);
                let source_field = match &mut intent.source_id {
                    FileSystemIdentity::Unix { inode, .. } => {
                        *inode -= 1;
                        "inode"
                    }
                    FileSystemIdentity::Windows { file_index, .. } => {
                        *file_index -= 1;
                        "file_index"
                    }
                };
                assert_ne!(intent_bytes(&intent).unwrap(), bytes);
                for invalid in [
                    serde_json::json!(value),
                    serde_json::json!("FFFFFFFFFFFFFFFF"),
                    serde_json::json!("1"),
                    serde_json::json!("+000000000000001"),
                ] {
                    let mut json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    json["source_id"][source_field] = invalid;
                    assert!(serde_json::from_value::<RemoveIntent>(json).is_err());
                }
            }
        }
    }

    fn directories() -> (tempfile::TempDir, StoreDirectory, StoreDirectory) {
        let root = tempdir_in(
            std::env::temp_dir()
                .canonicalize()
                .expect("canonical temporary root"),
        )
        .expect("temporary root");
        fs::create_dir(root.path().join("owner")).expect("owner directory");
        fs::create_dir(root.path().join("target")).expect("target directory");
        let owner = StoreDirectory::open(&root.path().join("owner")).expect("retained owner");
        let target = StoreDirectory::open(&root.path().join("target")).expect("retained target");
        (root, owner, target)
    }

    fn stage_ready(
        owner: &StoreDirectory,
        target: &StoreDirectory,
        relative: &Path,
        bytes: &[u8],
    ) -> StoreDirectory {
        let workspace = workspace_create(owner).expect("workspace");
        let _gate = workspace
            .open_private_gate_raw(Path::new(".gate"))
            .expect("gate");
        let (parent, leaf, chain) = parent_chain(target, relative).expect("target parent");
        private_leaf(&parent, leaf.to_str().expect("utf8 leaf"), bytes);
        let mut source = parent
            .open_regular_read(Path::new(&leaf), bytes.len() as u64)
            .expect("retained source");
        let source_id = super::file_id(&source).expect("source identity");
        let (sha256, length) = hash_file(&mut source, bytes.len() as u64).expect("source hash");
        let intent = RemoveIntent {
            version: 1,
            owner_id: directory_id(owner).expect("owner identity"),
            target_root_id: directory_id(target).expect("target identity"),
            parent_chain_ids: chain,
            relative: relative.to_str().expect("utf8 path").to_owned(),
            source_id,
            sha256,
            length,
            max_bytes: bytes.len() as u64,
        };
        private_leaf(
            &workspace,
            "remove.json",
            &intent_bytes(&intent).expect("intent bytes"),
        );
        workspace
    }

    fn private_leaf(workspace: &StoreDirectory, leaf: &str, bytes: &[u8]) {
        let mut file = workspace
            .create_private_file_raw(Path::new(leaf))
            .expect("private residue");
        file.write_all(bytes).expect("residue bytes");
        file.sync_all().expect("residue sync");
    }

    #[test]
    fn target_names_are_canonical_and_cannot_mutate_the_gate() {
        assert_eq!(
            super::valid_relative(&Path::new("dir").join("leaf")).unwrap(),
            "dir/leaf"
        );
        for name in ["dir/./leaf", "dir//leaf", "dir/../leaf", "/leaf", "leaf/"] {
            assert!(super::valid_relative(Path::new(name)).is_err(), "{name}");
        }
        let (_temp, owner, _target) = directories();
        assert!(
            write(
                &owner,
                &owner,
                Path::new(".kio-atomic/.gate"),
                b"corrupt",
                Publication::Upsert
            )
            .is_err()
        );
        assert_eq!(inspect(&owner).unwrap(), AtomicWorkspaceState::Absent);
    }

    #[test]
    fn inspect_is_read_only_when_the_workspace_is_absent() {
        let (_root, owner, _target) = directories();
        assert_eq!(
            inspect(&owner).expect("inspection"),
            AtomicWorkspaceState::Absent
        );
        assert!(
            !owner
                .contains_entry(Path::new(".kio-atomic"))
                .expect("workspace lookup")
        );
    }

    #[test]
    fn write_then_remove_returns_to_a_clean_workspace() {
        let (_root, owner, target) = directories();
        write(
            &owner,
            &target,
            Path::new("leaf"),
            b"published",
            Publication::CreateOnly,
        )
        .expect("atomic publication");
        assert_eq!(
            target
                .read_optional(Path::new("leaf"), 64)
                .expect("published read"),
            Some(b"published".to_vec())
        );
        remove(&owner, &target, Path::new("leaf"), b"published", 64).expect("atomic removal");
        assert!(
            !target
                .contains_entry(Path::new("leaf"))
                .expect("target lookup")
        );
        assert_eq!(
            inspect(&owner).expect("inspection"),
            AtomicWorkspaceState::Clean
        );
    }

    #[test]
    fn remove_accepts_an_empty_expected_file_with_a_zero_limit() {
        let (_root, owner, target) = directories();
        write(
            &owner,
            &target,
            Path::new("empty"),
            b"",
            Publication::CreateOnly,
        )
        .expect("empty publication");
        remove(&owner, &target, Path::new("empty"), b"", 0).expect("empty removal");
        assert!(
            !target
                .contains_entry(Path::new("empty"))
                .expect("empty lookup")
        );
    }

    #[test]
    fn orphan_write_is_discarded_without_replaying_it() {
        let (_root, owner, target) = directories();
        private_leaf(&target, "leaf", b"old");
        let workspace = workspace_create(&owner).expect("workspace");
        let _gate = workspace
            .open_private_gate_raw(Path::new(".gate"))
            .expect("gate");
        private_leaf(&workspace, "write", b"new");
        assert!(recover(&owner, &[&target]).expect("recovery"));
        assert_eq!(
            target.read_optional(Path::new("leaf"), 16).expect("target"),
            Some(b"old".to_vec())
        );
        assert_eq!(
            inspect(&owner).expect("inspection"),
            AtomicWorkspaceState::Clean
        );
    }

    #[test]
    fn ready_intent_recovers_source_then_quarantine_and_cleans() {
        let (_root, owner, target) = directories();
        let _workspace = stage_ready(&owner, &target, Path::new("leaf"), b"remove");
        assert!(recover(&owner, &[&target]).expect("recovery"));
        assert!(
            !target
                .contains_entry(Path::new("leaf"))
                .expect("target lookup")
        );
        assert_eq!(
            inspect(&owner).expect("inspection"),
            AtomicWorkspaceState::Clean
        );
    }

    #[cfg(unix)]
    #[test]
    fn ordinary_mode_source_survives_ready_quarantine_recovery() {
        use std::os::unix::fs::PermissionsExt as _;

        let (_root, owner, target) = directories();
        let _workspace = stage_ready(&owner, &target, Path::new("leaf"), b"remove");
        fs::set_permissions(
            target.path().join("leaf"),
            fs::Permissions::from_mode(0o644),
        )
        .expect("ordinary source mode");
        assert!(recover(&owner, &[&target]).expect("recovery"));
        assert_eq!(
            inspect(&owner).expect("inspection"),
            AtomicWorkspaceState::Clean
        );
    }

    #[cfg(unix)]
    #[test]
    fn normal_removal_accepts_an_ordinary_mode_source() {
        use std::os::unix::fs::PermissionsExt as _;

        let (_root, owner, target) = directories();
        private_leaf(&target, "leaf", b"remove");
        fs::set_permissions(
            target.path().join("leaf"),
            fs::Permissions::from_mode(0o644),
        )
        .expect("ordinary source mode");
        remove(&owner, &target, Path::new("leaf"), b"remove", 64).expect("remove");
        assert!(
            !target
                .contains_entry(Path::new("leaf"))
                .expect("target lookup")
        );
    }

    #[test]
    fn ready_intent_with_quarantine_and_no_source_completes() {
        let (_root, owner, target) = directories();
        let workspace = stage_ready(&owner, &target, Path::new("leaf"), b"remove");
        let (parent, leaf, _) = parent_chain(&target, Path::new("leaf")).expect("parent");
        parent
            .rename_regular_between_raw(
                Path::new(&leaf),
                &workspace,
                Path::new("removed"),
                Publication::CreateOnly,
            )
            .expect("quarantine");
        assert!(recover(&owner, &[&target]).expect("recovery"));
        assert_eq!(
            inspect(&owner).expect("inspection"),
            AtomicWorkspaceState::Clean
        );
    }

    #[test]
    fn ready_intent_with_both_absent_cleans_only_the_intent() {
        let (_root, owner, target) = directories();
        let workspace = stage_ready(&owner, &target, Path::new("leaf"), b"remove");
        let (parent, leaf, _) = parent_chain(&target, Path::new("leaf")).expect("parent");
        parent.remove_file(Path::new(&leaf)).expect("remove source");
        assert!(recover(&owner, &[&target]).expect("recovery"));
        assert!(
            !workspace
                .contains_entry(Path::new("remove.json"))
                .expect("ready lookup")
        );
    }

    #[test]
    fn orphan_quarantine_is_left_untouched() {
        let (_root, owner, target) = directories();
        let workspace = workspace_create(&owner).expect("workspace");
        let _gate = workspace
            .open_private_gate_raw(Path::new(".gate"))
            .expect("gate");
        private_leaf(&workspace, "removed", b"do not delete");
        assert!(recover(&owner, &[&target]).is_err());
        assert_eq!(
            workspace
                .read_optional(Path::new("removed"), 64)
                .expect("quarantine"),
            Some(b"do not delete".to_vec())
        );
    }

    #[test]
    fn invalid_or_copied_ready_intent_leaves_workspace_unchanged() {
        let (_root, owner, target) = directories();
        let workspace = workspace_create(&owner).expect("workspace");
        let _gate = workspace
            .open_private_gate_raw(Path::new(".gate"))
            .expect("gate");
        private_leaf(&workspace, "write", b"unpublished");
        private_leaf(&workspace, "remove.json", br#"{"owner_id":"other"}"#);
        assert!(recover(&owner, &[&target]).is_err());
        assert!(
            workspace
                .contains_entry(Path::new("write"))
                .expect("write lookup")
        );
        assert!(
            workspace
                .contains_entry(Path::new("remove.json"))
                .expect("ready lookup")
        );
    }

    #[test]
    fn parent_replacement_refuses_ready_recovery() {
        let (root, owner, target) = directories();
        fs::create_dir(root.path().join("target/child")).expect("child");
        let workspace = stage_ready(&owner, &target, Path::new("child/leaf"), b"remove");
        fs::remove_dir_all(root.path().join("target/child")).expect("replace old child");
        fs::create_dir(root.path().join("target/child")).expect("replacement child");
        fs::write(root.path().join("target/child/leaf"), b"replacement")
            .expect("replacement source");
        assert!(recover(&owner, &[&target]).is_err());
        assert!(
            workspace
                .contains_entry(Path::new("remove.json"))
                .expect("intent retained")
        );
    }
}
