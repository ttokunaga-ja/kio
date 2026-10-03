//! Descriptor-bound local management authority.
use crate::scope::KIO_FORMAT_VERSION;
use crate::store_dir::{Publication, StoreDirectory};
use crate::{ExitCode, KioError, Result};
use cap_primitives::fs as cap_fs;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    path::{Component, Path, PathBuf},
    sync::Arc,
};
pub const MANAGEMENT_RECORD_LEAF: &str = "management.json";
pub const MANAGEMENT_RECORD_VERSION: u8 = 2;
pub const REGISTRATION_PENDING_LEAF: &str = "registration-pending.json";
pub const REGISTRATION_PENDING_ERROR: &str = "KIO-E-REGISTRATION-PENDING-001";
pub const CHILD_INITIALIZATION_PENDING_LEAF: &str = ".child-init-journal.json";
pub const CHILD_INITIALIZATION_PENDING_ERROR: &str = "KIO-E-CHILD-INITIALIZATION-PENDING-001";
pub const ROOT_INITIALIZATION_PENDING_LEAF: &str = ".root-init-journal.json";
pub const ROOT_INITIALIZATION_PENDING_ERROR: &str = "KIO-E-ROOT-INITIALIZATION-PENDING-001";
pub const CONTROLLED_ROOT_ERROR: &str = "KIO-E-MANAGEMENT-ROOT-UNSAFE-001";
/// Reserved initialization probe leaf. Planned-layout validation may permit
/// only this exact transient leaf when it contains [`CASE_PROBE_BYTES`].
pub const CASE_PROBE_LEAF: &str = ".kio-case-probe";
/// Fixed bytes make an interrupted case probe safely resumable and auditable.
pub const CASE_PROBE_BYTES: &[u8] = b"kio-case-probe-v1";
const CASE_PROBE_UPPER_LEAF: &str = ".KIO-CASE-PROBE";
pub const MAX_MANAGEMENT_RECORD_BYTES: u64 = 256 * 1024;
pub const MAX_MANAGEMENT_CHAIN_DEPTH: usize = 256;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "platform", rename_all = "snake_case", deny_unknown_fields)]
pub enum DirectoryIdentity {
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
impl DirectoryIdentity {
    /// Whether this persisted identity can name a directory on the current
    /// host. Foreign identities are valid recovery data, but never authorize a
    /// local filesystem probe.
    #[must_use]
    pub const fn is_native_platform(&self) -> bool {
        match self {
            Self::Unix { .. } => cfg!(unix),
            Self::Windows { .. } => cfg!(windows),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagementAuthority {
    Root,
    Child {
        parent_scope_id: String,
        root_scope_id: String,
        enrollment_token: String,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildEnrollment {
    pub scope_id: String,
    pub enrollment_token: String,
    pub directory_identity: DirectoryIdentity,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagementRecord {
    pub version: u8,
    pub registration_generation: u64,
    pub scope_id: String,
    pub canonical_root: PathBuf,
    pub directory_identity: DirectoryIdentity,
    pub case_insensitive: bool,
    pub authority: ManagementAuthority,
    pub children: BTreeMap<String, ChildEnrollment>,
}
#[derive(Debug, Clone)]
pub struct ManagementBinding {
    canonical_root: PathBuf,
    root: Arc<File>,
    kio: Arc<File>,
    control: StoreDirectory,
    directory_identity: DirectoryIdentity,
    kio_identity: DirectoryIdentity,
}
#[derive(Debug, Clone)]
pub struct ValidatedManagementScope {
    pub binding: ManagementBinding,
    pub record: ManagementRecord,
}
#[derive(Debug, Clone)]
pub struct ValidatedManagementChain {
    pub scopes: Vec<ValidatedManagementScope>,
    pub digest_input: Vec<u8>,
}
/// A direct child observed only through the parent's retained root handle.
/// The handle is retained so callers can create staged state without reopening
/// a public child path.
pub struct ObservedDirectChild {
    pub handle: File,
    pub canonical_root: PathBuf,
    pub directory_identity: DirectoryIdentity,
}
impl ManagementBinding {
    pub fn from_retained(root: File, kio: File, canonical_root: PathBuf) -> Result<Self> {
        if !canonical_root.is_absolute()
            || canonical_root
                .components()
                .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
        {
            return Err(err("canonical root path is invalid"));
        }
        controlled_root_directory(&root, &canonical_root)?;
        Self::from_validated_retained(root, kio, canonical_root)
    }
    fn from_validated_retained(root: File, kio: File, canonical_root: PathBuf) -> Result<Self> {
        let directory_identity = file_identity(&root)?;
        let kio_identity = file_identity(&kio)?;
        let named = cap_fs::open_dir_nofollow(&root, Path::new(".kio")).map_err(io)?;
        if file_identity(&named)? != kio_identity {
            return Err(err("retained .kio handle does not match root"));
        }
        #[cfg(windows)]
        let (root, kio) = {
            // Keep the proved objects while consuming the original Windows
            // handles: retaining their delete-share-denying clones would
            // prevent later namespace moves even after `control` normalizes.
            let root = StoreDirectory::from_retained(root, canonical_root.clone())?;
            let kio = StoreDirectory::from_retained(kio, canonical_root.join(".kio"))?;
            (
                root.root_handle().try_clone().map_err(io)?,
                kio.root_handle().try_clone().map_err(io)?,
            )
        };
        let control = StoreDirectory::from_retained(
            kio.try_clone().map_err(io)?,
            canonical_root.join(".kio"),
        )?;
        Ok(Self {
            canonical_root,
            root: Arc::new(root),
            kio: Arc::new(kio),
            control,
            directory_identity,
            kio_identity,
        })
    }
    pub fn bind(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
        {
            return Err(err("scope path must be absolute and canonical"));
        }
        let checked = controlled_root_directory_from_path(path)?;
        let root = checked.root_handle().try_clone().map_err(io)?;
        let kio = cap_fs::open_dir_nofollow(&root, Path::new(".kio")).map_err(io)?;
        Self::from_validated_retained(root, kio, path.to_path_buf())
    }
    #[must_use]
    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }
    #[must_use]
    pub fn directory_identity(&self) -> &DirectoryIdentity {
        &self.directory_identity
    }
    #[must_use]
    pub fn root_handle(&self) -> &File {
        &self.root
    }
    #[must_use]
    pub fn kio_handle(&self) -> &File {
        &self.kio
    }
    /// Revalidate the retained root and `.kio` bindings without reading a
    /// management record. Policy and egress callers use this before acting.
    pub fn revalidate(&self) -> Result<()> {
        self.recheck()
    }
    fn recheck(&self) -> Result<()> {
        validate_controlled_root(&self.root, &self.canonical_root)?;
        if file_identity(&self.root)? != self.directory_identity
            || file_identity(&self.kio)? != self.kio_identity
        {
            return Err(err("retained directory identity changed"));
        }
        let named = cap_fs::open_dir_nofollow(&self.root, Path::new(".kio")).map_err(io)?;
        if file_identity(&named)? != self.kio_identity {
            return Err(err("named .kio entry changed"));
        }
        Ok(())
    }
}

/// Require that a retained working root remains safe for Kio-managed writes.
///
/// The canonical path is walked through the existing platform-specific
/// shared-read-parent policy, then its retained identity is matched to `root`.
/// This only validates; it never repairs permissions or changes ACLs.
pub fn validate_controlled_root(root: &File, canonical_root: &Path) -> Result<()> {
    controlled_root_directory(root, canonical_root).map(|_| ())
}

fn controlled_root_directory(root: &File, canonical_root: &Path) -> Result<StoreDirectory> {
    let checked = controlled_root_directory_from_path(canonical_root)?;
    let expected = file_identity(root)
        .map_err(|error| controlled_root_err(canonical_root, error.to_string()))?;
    let checked_identity = file_identity(checked.root_handle().as_ref())
        .map_err(|error| controlled_root_err(canonical_root, error.to_string()))?;
    if expected != checked_identity {
        return Err(controlled_root_err(
            canonical_root,
            "retained root does not match canonical controlled root",
        ));
    }
    Ok(checked)
}

fn controlled_root_directory_from_path(canonical_root: &Path) -> Result<StoreDirectory> {
    crate::private_fs::verify_private_creation_parent(canonical_root)
        .map_err(|error| controlled_root_err(canonical_root, error.to_string()))
}
pub fn initialize_root(
    binding: &ManagementBinding,
    scope_id: impl Into<String>,
    case_insensitive: bool,
) -> Result<ManagementRecord> {
    binding.recheck()?;
    let scope_id = scope_id.into();
    validate_id(&scope_id, "scope_id")?;
    if current_scope_id(binding)? != scope_id {
        return Err(err("management scope_id differs from current scope.json"));
    }
    let record = ManagementRecord {
        version: MANAGEMENT_RECORD_VERSION,
        registration_generation: 1,
        scope_id,
        canonical_root: binding.canonical_root.clone(),
        directory_identity: binding.directory_identity.clone(),
        case_insensitive,
        authority: ManagementAuthority::Root,
        children: BTreeMap::new(),
    };
    write_record(binding, &record, false)?;
    Ok(record)
}

/// Measure this retained scope's case behavior during explicit initialization.
/// No read/search path calls this probe. The unique lower-case leaf is created
/// with CreateOnly and is removed only through byte-verified quarantine.
pub fn detect_case_insensitive(binding: &ManagementBinding) -> Result<bool> {
    let _lock = crate::scope::acquire_retained_store_lock(&binding.kio)?;
    let root = StoreDirectory::from_retained(
        binding.root.try_clone().map_err(io)?,
        binding.canonical_root.clone(),
    )?;
    detect_case_insensitive_with_recheck(&root, &binding.control, || binding.recheck())
}

/// Measure case behavior in this exact retained directory. This is an
/// initialization-only mutation: callers must not infer a child's behavior
/// from an ancestor control directory, because Windows permits it to differ
/// per directory.
/// `workspace_owner` supplies the private atomic workspace. It must remain a
/// retained private store for the duration of this mutation; no atomic state is
/// written into `directory`'s parent or elsewhere among user files.
pub fn detect_case_insensitive_in_directory(
    directory: &StoreDirectory,
    workspace_owner: &StoreDirectory,
) -> Result<bool> {
    detect_case_insensitive_with_recheck(directory, workspace_owner, || Ok(()))
}

fn detect_case_insensitive_with_recheck(
    directory: &StoreDirectory,
    workspace_owner: &StoreDirectory,
    recheck: impl Fn() -> Result<()>,
) -> Result<bool> {
    recheck()?;
    workspace_owner.recover_atomic(&[directory])?;
    recheck()?;
    if !case_probe_present_and_validated(directory)? {
        directory.write_atomic_with_owner(
            workspace_owner,
            Path::new(CASE_PROBE_LEAF),
            CASE_PROBE_BYTES,
            Publication::CreateOnly,
        )?;
    }
    let result = (|| -> Result<bool> {
        recheck()?;
        case_probe_uppercase_resolution(directory)
    })();
    let cleanup = directory.quarantine_then_remove_with_owner(
        workspace_owner,
        Path::new(CASE_PROBE_LEAF),
        CASE_PROBE_BYTES,
        CASE_PROBE_BYTES.len() as u64,
    );
    recheck()?;
    match (result, cleanup) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(value), Ok(())) => Ok(value),
    }
}

/// Remove a verified interrupted case probe without creating a new one.
///
/// The caller supplies the retained private owner for atomic recovery and
/// removal. `Ok(false)` means the reserved leaf was absent; malformed or
/// tampered leaves are rejected rather than removed.
pub fn cleanup_case_probe_in_directory(
    directory: &StoreDirectory,
    workspace_owner: &StoreDirectory,
) -> Result<bool> {
    workspace_owner.recover_atomic(&[directory])?;
    if !case_probe_present_and_validated(directory)? {
        return Ok(false);
    }
    directory.quarantine_then_remove_with_owner(
        workspace_owner,
        Path::new(CASE_PROBE_LEAF),
        CASE_PROBE_BYTES,
        CASE_PROBE_BYTES.len() as u64,
    )?;
    Ok(true)
}

/// Read and verify an existing reserved case probe without creating or
/// deleting anything. `true` means the verified probe also resolves through
/// its upper-case spelling; `false` means the leaf is absent or the directory
/// is case-sensitive. A malformed, linked, non-private, or tampered leaf is
/// rejected rather than ignored.
pub fn validate_case_probe(directory: &StoreDirectory) -> Result<bool> {
    if !case_probe_present_and_validated(directory)? {
        return Ok(false);
    }
    case_probe_uppercase_resolution(directory)
}

fn case_probe_present_and_validated(directory: &StoreDirectory) -> Result<bool> {
    let Some(bytes) =
        directory.read_optional(Path::new(CASE_PROBE_LEAF), CASE_PROBE_BYTES.len() as u64)?
    else {
        return Ok(false);
    };
    directory.ensure_owner_private(Path::new(CASE_PROBE_LEAF))?;
    if bytes != CASE_PROBE_BYTES {
        return Err(err(
            "reserved case probe bytes differ from the expected value",
        ));
    }
    Ok(true)
}

fn case_probe_uppercase_resolution(directory: &StoreDirectory) -> Result<bool> {
    match directory.read_optional(
        Path::new(CASE_PROBE_UPPER_LEAF),
        CASE_PROBE_BYTES.len() as u64,
    )? {
        None => Ok(false),
        Some(found) if found == CASE_PROBE_BYTES => {
            let lower = directory
                .open_regular_read(Path::new(CASE_PROBE_LEAF), CASE_PROBE_BYTES.len() as u64)?;
            let upper = directory.open_regular_read(
                Path::new(CASE_PROBE_UPPER_LEAF),
                CASE_PROBE_BYTES.len() as u64,
            )?;
            if same_case_probe_file(&lower, &upper)? {
                Ok(true)
            } else {
                Err(err(
                    "case probe upper-case lookup names a different regular file",
                ))
            }
        }
        Some(_) => Err(err(
            "case probe upper-case lookup did not name the reserved probe leaf",
        )),
    }
}

#[cfg(unix)]
fn same_case_probe_file(left: &File, right: &File) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let left = left.metadata().map_err(io)?;
    let right = right.metadata().map_err(io)?;
    Ok(left.dev() == right.dev() && left.ino() == right.ino() && left.nlink() == 1)
}

#[cfg(windows)]
fn same_case_probe_file(left: &File, right: &File) -> Result<bool> {
    Ok(crate::cas::same_windows_regular_file(left, right))
}
pub fn read_record(binding: &ManagementBinding) -> Result<ManagementRecord> {
    binding.recheck()?;
    if child_initialization_pending(binding)? {
        return Err(child_initialization_pending_err());
    }
    if root_initialization_pending(binding)? {
        return Err(root_initialization_pending_err());
    }
    if peek_registration_marker(binding)?.is_some() {
        return Err(registration_pending_err());
    }
    let bytes = read_leaf(
        &binding.control,
        MANAGEMENT_RECORD_LEAF,
        MAX_MANAGEMENT_RECORD_BYTES,
    )?;
    let record: ManagementRecord =
        serde_json::from_slice(&bytes).map_err(|_| err("management record is not strict JSON"))?;
    validate_record(binding, &record)?;
    if current_scope_id(binding)? != record.scope_id {
        return Err(err("management scope_id differs from current scope.json"));
    }
    binding.recheck()?;
    if child_initialization_pending(binding)? {
        return Err(child_initialization_pending_err());
    }
    if root_initialization_pending(binding)? {
        return Err(root_initialization_pending_err());
    }
    if peek_registration_marker(binding)?.is_some() {
        return Err(registration_pending_err());
    }
    Ok(record)
}

/// Read a registration-in-progress record only for recovery. Unlike
/// [`read_record`], this intentionally does not require the record's old
/// canonical location or old directory identity to match. It still validates
/// the binding's current named root/.kio relationship, strict record shape,
/// and current scope ID.
pub fn read_registration_recovery_record(binding: &ManagementBinding) -> Result<ManagementRecord> {
    binding.recheck()?;
    let bytes = read_leaf(
        &binding.control,
        MANAGEMENT_RECORD_LEAF,
        MAX_MANAGEMENT_RECORD_BYTES,
    )?;
    let record: ManagementRecord =
        serde_json::from_slice(&bytes).map_err(|_| err("management record is not strict JSON"))?;
    validate_record_shape(&record)?;
    if current_scope_id(binding)? != record.scope_id {
        return Err(err("management scope_id differs from current scope.json"));
    }
    binding.recheck()?;
    Ok(record)
}

/// Return the pending operation ID when a strict retained marker exists.
pub fn peek_registration_marker(binding: &ManagementBinding) -> Result<Option<String>> {
    binding.recheck()?;
    let bytes = binding
        .control
        .read_optional(Path::new(REGISTRATION_PENDING_LEAF), 1024)
        .map_err(|_| registration_pending_err())?;
    let Some(bytes) = bytes else { return Ok(None) };
    let marker: RegistrationPending =
        serde_json::from_slice(&bytes).map_err(|_| registration_pending_err())?;
    validate_operation_id(&marker.operation_id).map_err(|_| registration_pending_err())?;
    Ok(Some(marker.operation_id))
}

fn child_initialization_pending(binding: &ManagementBinding) -> Result<bool> {
    initialization_pending(
        binding,
        CHILD_INITIALIZATION_PENDING_LEAF,
        child_initialization_pending_err,
    )
}

fn root_initialization_pending(binding: &ManagementBinding) -> Result<bool> {
    initialization_pending(
        binding,
        ROOT_INITIALIZATION_PENDING_LEAF,
        root_initialization_pending_err,
    )
}

fn initialization_pending(
    binding: &ManagementBinding,
    leaf: &str,
    pending_error: fn() -> KioError,
) -> Result<bool> {
    binding
        .control
        .read_optional(Path::new(leaf), MAX_MANAGEMENT_RECORD_BYTES)
        .map(|marker| marker.is_some())
        .map_err(|_| pending_error())
}

pub fn begin_registration(binding: &ManagementBinding, operation_id: &str) -> Result<()> {
    validate_operation_id(operation_id)?;
    let _lock = crate::scope::acquire_retained_store_lock(&binding.kio)?;
    binding.recheck()?;
    let marker = registration_marker_bytes(operation_id)?;
    match binding
        .control
        .read_optional(Path::new(REGISTRATION_PENDING_LEAF), 1024)
    {
        Ok(Some(existing)) if existing == marker => return Ok(()),
        Ok(Some(_)) | Err(_) => return Err(registration_pending_err()),
        Ok(None) => {}
    }
    binding.control.write_atomic(
        Path::new(REGISTRATION_PENDING_LEAF),
        &marker,
        Publication::CreateOnly,
    )?;
    binding.recheck()
}

pub fn publish_registration_record(
    binding: &ManagementBinding,
    operation_id: &str,
    before: &ManagementRecord,
    after: &ManagementRecord,
) -> Result<()> {
    validate_operation_id(operation_id)?;
    let _lock = crate::scope::acquire_retained_store_lock(&binding.kio)?;
    if peek_registration_marker(binding)?.as_deref() != Some(operation_id) {
        return Err(registration_pending_err());
    }
    validate_registration_transition(before, after)?;
    validate_record(binding, after)?;
    let actual = read_registration_recovery_record(binding)?;
    if actual != *before && actual != *after {
        return Err(err("registration record differs from expected transition"));
    }
    if actual == *after {
        return Ok(());
    }
    write_registration_record(binding, after)
}

pub fn finish_registration(
    binding: &ManagementBinding,
    operation_id: &str,
    expected_after: &ManagementRecord,
) -> Result<()> {
    validate_operation_id(operation_id)?;
    let _lock = crate::scope::acquire_retained_store_lock(&binding.kio)?;
    if peek_registration_marker(binding)?.as_deref() != Some(operation_id) {
        return Err(registration_pending_err());
    }
    validate_record(binding, expected_after)?;
    if read_registration_recovery_record(binding)? != *expected_after {
        return Err(err(
            "registration record differs from expected published record",
        ));
    }
    let marker = registration_marker_bytes(operation_id)?;
    binding.control.quarantine_then_remove(
        Path::new(REGISTRATION_PENDING_LEAF),
        &marker,
        marker.len() as u64,
    )?;
    binding.recheck()
}

/// Validate a prospective direct child before creating `.kio` or probing its
/// filesystem. A retained handle alone must not authorize a substituted name
/// or an unregistered volume.
pub fn validate_prospective_child(
    parent: &ManagementBinding,
    child: &File,
    canonical_root: &Path,
) -> Result<()> {
    let basename = canonical_root
        .strip_prefix(parent.canonical_root())
        .ok()
        .and_then(|relative| relative.to_str())
        .ok_or_else(|| err("prospective child is not beneath its parent"))?;
    validate_basename(basename)?;
    validate_controlled_root(child, canonical_root)?;
    let observed = observe_direct_child(parent, basename)?
        .ok_or_else(|| err("prospective child is absent"))?;
    let identity = file_identity(child)?;
    if identity != observed.directory_identity {
        return Err(err(
            "prospective child name no longer matches the retained directory",
        ));
    }
    if !same_volume(parent.directory_identity(), &identity) {
        return Err(err("prospective child is on a different filesystem volume"));
    }
    Ok(())
}

/// Observe a named direct child twice through the retained parent. `None`
/// means only that the final leaf is absent; links, reparse points,
/// non-directories, and races fail closed.
pub fn observe_direct_child(
    parent: &ManagementBinding,
    basename: &str,
) -> Result<Option<ObservedDirectChild>> {
    validate_basename(basename)?;
    parent.revalidate()?;
    let first = observe_direct_child_once(parent, basename)?;
    parent.revalidate()?;
    let second = observe_direct_child_once(parent, basename)?;
    match (first, second) {
        (None, None) => Ok(None),
        (Some(first), Some(second)) if first.directory_identity == second.directory_identity => {
            parent.revalidate()?;
            Ok(Some(first))
        }
        _ => Err(err("prospective child entry changed while being observed")),
    }
}

fn observe_direct_child_once(
    parent: &ManagementBinding,
    basename: &str,
) -> Result<Option<ObservedDirectChild>> {
    let handle = match cap_fs::open_dir_nofollow(parent.root_handle(), Path::new(basename)) {
        Ok(handle) => handle,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io(error)),
    };
    let canonical_root = parent.canonical_root.join(basename);
    validate_controlled_root(&handle, &canonical_root)?;
    let directory_identity = file_identity(&handle)?;
    Ok(Some(ObservedDirectChild {
        handle,
        canonical_root,
        directory_identity,
    }))
}

pub fn enroll_child(
    parent: &ManagementBinding,
    basename: &str,
    scope_id: impl Into<String>,
    token: impl Into<String>,
    expected_directory_identity: &DirectoryIdentity,
) -> Result<()> {
    let _lock = crate::scope::acquire_retained_store_lock(&parent.kio)?;
    validate_live_chain(parent)?;
    validate_basename(basename)?;
    let (scope_id, token) = (scope_id.into(), token.into());
    validate_id(&scope_id, "child scope_id")?;
    validate_id(&token, "enrollment_token")?;
    let observed =
        observe_direct_child(parent, basename)?.ok_or_else(|| err("enrolled child is absent"))?;
    if !same_volume(parent.directory_identity(), &observed.directory_identity) {
        return Err(err("enrolled child is on a different filesystem volume"));
    }
    if &observed.directory_identity != expected_directory_identity {
        return Err(err(
            "enrolled child identity differs from expected identity",
        ));
    }
    let expected = ChildEnrollment {
        scope_id,
        enrollment_token: token,
        directory_identity: observed.directory_identity,
    };
    let mut r = read_record(parent)?;
    if let Some(existing) = r.children.get(basename) {
        if existing == &expected {
            return Ok(());
        }
        return Err(err("child basename already has a conflicting enrollment"));
    }
    r.children.insert(basename.into(), expected);
    write_record(parent, &r, true)
}

/// Remove a child enrollment only when every persisted enrollment field still
/// matches the journaled expected value. A conflicting row is never treated as
/// an already-completed retirement.
pub fn compare_and_remove_child_enrollment(
    parent: &ManagementBinding,
    basename: &str,
    expected: &ChildEnrollment,
) -> Result<bool> {
    let _lock = crate::scope::acquire_retained_store_lock(&parent.kio)?;
    validate_live_chain(parent)?;
    validate_basename(basename)?;
    validate_id(&expected.scope_id, "child scope_id")?;
    validate_id(&expected.enrollment_token, "enrollment_token")?;
    let mut record = read_record(parent)?;
    match record.children.get(basename) {
        None => Ok(false),
        Some(actual) if actual == expected => {
            record.children.remove(basename);
            write_record(parent, &record, true)?;
            Ok(true)
        }
        Some(_) => Err(err(
            "child enrollment differs from expected retirement entry",
        )),
    }
}
pub fn revoke_child(parent: &ManagementBinding, basename: &str) -> Result<bool> {
    let _lock = crate::scope::acquire_retained_store_lock(&parent.kio)?;
    validate_live_chain(parent)?;
    validate_basename(basename)?;
    let mut r = read_record(parent)?;
    let removed = r.children.remove(basename).is_some();
    if removed {
        write_record(parent, &r, true)?
    }
    Ok(removed)
}
pub fn initialize_child(
    parent: &ManagementBinding,
    child: &ManagementBinding,
    scope_id: impl Into<String>,
    token: impl Into<String>,
) -> Result<ManagementRecord> {
    let _lock = crate::scope::acquire_retained_store_lock(&parent.kio)?;
    validate_live_chain(parent)?;
    child.recheck()?;
    let name = child_name(parent, child)?;
    if !same_volume(parent.directory_identity(), child.directory_identity()) {
        return Err(err("child is on a different filesystem volume"));
    }
    let (scope_id, token) = (scope_id.into(), token.into());
    if current_scope_id(child)? != scope_id {
        return Err(err("child scope_id differs from current scope.json"));
    }
    let p = read_record(parent)?;
    let g = p
        .children
        .get(name)
        .ok_or_else(|| err("parent has no child enrollment grant"))?;
    if g.scope_id != scope_id
        || g.enrollment_token != token
        || g.directory_identity != *child.directory_identity()
    {
        return Err(err("child enrollment does not match parent grant"));
    }
    let root_scope_id = match &p.authority {
        ManagementAuthority::Root => p.scope_id.clone(),
        ManagementAuthority::Child { root_scope_id, .. } => root_scope_id.clone(),
    };
    let r = ManagementRecord {
        version: MANAGEMENT_RECORD_VERSION,
        registration_generation: 1,
        scope_id,
        canonical_root: child.canonical_root.clone(),
        directory_identity: child.directory_identity.clone(),
        case_insensitive: p.case_insensitive,
        authority: ManagementAuthority::Child {
            parent_scope_id: p.scope_id,
            root_scope_id,
            enrollment_token: token,
        },
        children: BTreeMap::new(),
    };
    write_record(child, &r, false)?;
    Ok(r)
}

/// Construct, but do not write, a child management record from retained
/// parent/child authority. This deliberately does not require a parent
/// enrollment: the caller can record the full expected value before allocating
/// a staged child `.kio` store. Planning performs only reads; a caller that
/// subsequently publishes the plan must hold the parent writer lease and
/// revalidate the retained bindings before publication.
pub fn planned_child_management_record(
    parent: &ManagementBinding,
    child: &ObservedDirectChild,
    scope_id: impl Into<String>,
    token: impl Into<String>,
) -> Result<ManagementRecord> {
    let basename = child
        .canonical_root
        .strip_prefix(parent.canonical_root())
        .ok()
        .and_then(|relative| relative.to_str())
        .ok_or_else(|| err("planned child is not beneath its parent"))?;
    validate_basename(basename)?;
    if file_identity(&child.handle)? != child.directory_identity {
        return Err(err("retained planned child identity changed"));
    }
    let observed =
        observe_direct_child(parent, basename)?.ok_or_else(|| err("planned child is absent"))?;
    if observed.directory_identity != child.directory_identity {
        return Err(err("planned child name no longer matches retained child"));
    }
    if !same_volume(parent.directory_identity(), &child.directory_identity) {
        return Err(err("planned child is on a different filesystem volume"));
    }
    let (scope_id, token) = (scope_id.into(), token.into());
    validate_id(&scope_id, "child scope_id")?;
    validate_id(&token, "enrollment_token")?;
    let parent_record = read_record(parent)?;
    let root_scope_id = match &parent_record.authority {
        ManagementAuthority::Root => parent_record.scope_id.clone(),
        ManagementAuthority::Child { root_scope_id, .. } => root_scope_id.clone(),
    };
    let record = ManagementRecord {
        version: MANAGEMENT_RECORD_VERSION,
        registration_generation: 1,
        scope_id,
        canonical_root: child.canonical_root.clone(),
        directory_identity: child.directory_identity.clone(),
        case_insensitive: parent_record.case_insensitive,
        authority: ManagementAuthority::Child {
            parent_scope_id: parent_record.scope_id,
            root_scope_id,
            enrollment_token: token,
        },
        children: BTreeMap::new(),
    };
    validate_record_shape(&record)?;
    Ok(record)
}

/// Create-only write for a previously planned child record. `stage` is a
/// retained staged `.kio` directory whose strict `scope.json` must name the
/// planned scope before management state is published.
pub fn write_planned_child_management_record(
    stage: &StoreDirectory,
    record: &ManagementRecord,
) -> Result<()> {
    validate_record_shape(record)?;
    if scope_id_from_store(stage)? != record.scope_id {
        return Err(err("staged child scope_id differs from planned record"));
    }
    let bytes = planned_management_record_bytes(record)?;
    stage.write_atomic(
        Path::new(MANAGEMENT_RECORD_LEAF),
        &bytes,
        Publication::CreateOnly,
    )
}

/// Serialize a planned management record only after its strict persisted shape
/// has been validated. Staging callers use these exact bytes for idempotence
/// checks before any create-only publication.
pub fn planned_management_record_bytes(record: &ManagementRecord) -> Result<Vec<u8>> {
    validate_record_shape(record)?;
    management_record_bytes(record)
}
pub fn validate_live_chain(target: &ManagementBinding) -> Result<ValidatedManagementChain> {
    let (mut current, mut seen, mut scopes) = (target.clone(), BTreeSet::new(), Vec::new());
    let mut r = read_record(&current)?;
    for _ in 0..MAX_MANAGEMENT_CHAIN_DEPTH {
        if !seen.insert(r.scope_id.clone()) {
            return Err(err(
                "management chain contains duplicate scope identity or cycle",
            ));
        }
        let a = r.authority.clone();
        scopes.push(ValidatedManagementScope {
            binding: current.clone(),
            record: r,
        });
        match a {
            ManagementAuthority::Root => {
                let digest_input =
                    serde_json::to_vec(&scopes.iter().map(|s| &s.record).collect::<Vec<_>>())
                        .map_err(|_| err("cannot serialize management chain"))?;
                return Ok(ValidatedManagementChain {
                    scopes,
                    digest_input,
                });
            }
            ManagementAuthority::Child {
                parent_scope_id,
                root_scope_id,
                enrollment_token,
            } => {
                let path = current
                    .canonical_root
                    .parent()
                    .ok_or_else(|| err("child has no physical parent"))?;
                let parent = ManagementBinding::bind(path)?;
                if !same_volume(parent.directory_identity(), current.directory_identity()) {
                    return Err(err("parent and child are on different filesystem volumes"));
                }
                let pr = read_record(&parent)?;
                let name = child_name(&parent, &current)?;
                let g = pr
                    .children
                    .get(name)
                    .ok_or_else(|| err("parent lacks reciprocal child grant"))?;
                if pr.scope_id != parent_scope_id
                    || g.scope_id != scopes.last().expect("pushed").record.scope_id
                    || g.enrollment_token != enrollment_token
                    || g.directory_identity != *current.directory_identity()
                {
                    return Err(err("parent and child management records do not match"));
                }
                let parent_root = match &pr.authority {
                    ManagementAuthority::Root => &pr.scope_id,
                    ManagementAuthority::Child { root_scope_id, .. } => root_scope_id,
                };
                if parent_root != &root_scope_id {
                    return Err(err("child root identity does not match parent chain"));
                }
                // Carry the same fresh record used for the reciprocal grant into
                // the next authority check and the returned chain digest.
                current = parent;
                r = pr;
            }
        }
    }
    Err(err("management chain exceeds bounded depth"))
}
fn validate_record(b: &ManagementBinding, r: &ManagementRecord) -> Result<()> {
    validate_record_shape(r)?;
    if !r.directory_identity.is_native_platform() {
        return Err(err(
            "foreign management directory identity cannot authorize a local binding",
        ));
    }
    if r.canonical_root != b.canonical_root || r.directory_identity != b.directory_identity {
        return Err(err("management creation binding differs from live scope"));
    }
    Ok(())
}
fn validate_record_shape(r: &ManagementRecord) -> Result<()> {
    if r.version != MANAGEMENT_RECORD_VERSION || r.registration_generation == 0 {
        return Err(err("unsupported management record version"));
    }
    validate_id(&r.scope_id, "scope_id")?;
    validate_recorded_root_path(&r.canonical_root, &r.directory_identity)?;
    for (n, c) in &r.children {
        validate_basename(n)?;
        validate_id(&c.scope_id, "child scope_id")?;
        validate_id(&c.enrollment_token, "enrollment_token")?;
        if !same_volume(&r.directory_identity, &c.directory_identity) {
            return Err(err("child enrollment is on a different filesystem volume"));
        }
    }
    if let ManagementAuthority::Child {
        parent_scope_id,
        root_scope_id,
        enrollment_token,
    } = &r.authority
    {
        validate_id(parent_scope_id, "parent_scope_id")?;
        validate_id(root_scope_id, "root_scope_id")?;
        validate_id(enrollment_token, "enrollment_token")?
    }
    Ok(())
}

/// Construct a direct child's recorded root with the syntax of the record's
/// platform. This is lexical only: callers must establish a fresh native
/// [`ManagementBinding`] before using a path as filesystem authority.
pub fn recorded_child_root(record: &ManagementRecord, name: &str) -> Result<PathBuf> {
    validate_recorded_root_path(&record.canonical_root, &record.directory_identity)?;
    validate_basename(name)?;
    let root = record
        .canonical_root
        .to_str()
        .ok_or_else(|| err("management record canonical_root is not UTF-8"))?;
    match &record.directory_identity {
        DirectoryIdentity::Unix { .. } => {
            let joined = if root == "/" {
                format!("/{name}")
            } else {
                format!("{root}/{name}")
            };
            Ok(PathBuf::from(joined))
        }
        DirectoryIdentity::Windows { .. } => {
            let joined = if root.ends_with('\\') {
                format!("{root}{name}")
            } else {
                format!("{root}\\{name}")
            };
            Ok(PathBuf::from(joined))
        }
    }
}

fn validate_recorded_root_path(path: &Path, identity: &DirectoryIdentity) -> Result<()> {
    let path = path
        .to_str()
        .ok_or_else(|| err("management record canonical_root is not UTF-8"))?;
    let valid = match identity {
        DirectoryIdentity::Unix { .. } => valid_unix_recorded_root(path),
        DirectoryIdentity::Windows { .. } => valid_windows_recorded_root(path),
    };
    if valid {
        Ok(())
    } else {
        Err(err("management record canonical_root is invalid"))
    }
}

fn valid_unix_recorded_root(path: &str) -> bool {
    if path == "/" {
        return true;
    }
    path.starts_with('/')
        && !path.ends_with('/')
        && path
            .split('/')
            .skip(1)
            .all(|component| !component.is_empty() && component != "." && component != "..")
}

fn valid_windows_recorded_root(path: &str) -> bool {
    let components = if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest
    } else {
        path
    };
    let bytes = components.as_bytes();
    if path.contains('/')
        || bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || bytes[2] != b'\\'
    {
        return false;
    }
    let tail = &components[3..];
    tail.is_empty()
        || (!tail.ends_with('\\')
            && tail
                .split('\\')
                .all(|component| !component.is_empty() && component != "." && component != ".."))
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistrationPending {
    operation_id: String,
}
fn registration_marker_bytes(operation_id: &str) -> Result<Vec<u8>> {
    serde_json::to_vec(&RegistrationPending {
        operation_id: operation_id.into(),
    })
    .map_err(|_| err("cannot serialize registration marker"))
}
fn validate_operation_id(operation_id: &str) -> Result<()> {
    if operation_id.is_empty()
        || operation_id.len() > 256
        || operation_id
            .bytes()
            .any(|b| b.is_ascii_control() || b == b'/' || b == b'\\')
    {
        Err(err("registration operation_id is invalid"))
    } else {
        Ok(())
    }
}
fn registration_pending_err() -> KioError {
    KioError::new(
        REGISTRATION_PENDING_ERROR,
        "registration recovery is required",
        json!({}),
        ExitCode::PermanentFailure,
    )
}
fn child_initialization_pending_err() -> KioError {
    KioError::new(
        CHILD_INITIALIZATION_PENDING_ERROR,
        "child initialization recovery is required",
        json!({}),
        ExitCode::PermanentFailure,
    )
}
fn root_initialization_pending_err() -> KioError {
    KioError::new(
        ROOT_INITIALIZATION_PENDING_ERROR,
        "root initialization recovery is required",
        json!({}),
        ExitCode::PermanentFailure,
    )
}
fn controlled_root_err(canonical_root: &Path, cause: impl Into<String>) -> KioError {
    KioError::new(
        CONTROLLED_ROOT_ERROR,
        "working root is unsafe for Kio-managed writes",
        json!({"canonical_root": canonical_root.display().to_string(), "cause": cause.into()}),
        ExitCode::PermanentFailure,
    )
}
fn validate_registration_transition(
    before: &ManagementRecord,
    after: &ManagementRecord,
) -> Result<()> {
    validate_record_shape(before)?;
    validate_record_shape(after)?;
    if before.scope_id != after.scope_id
        || after.registration_generation
            != before
                .registration_generation
                .checked_add(1)
                .ok_or_else(|| err("registration generation overflow"))?
    {
        return Err(err("registration record transition is invalid"));
    }
    Ok(())
}
fn current_scope_id(b: &ManagementBinding) -> Result<String> {
    scope_id_from_store(&b.control)
}
fn scope_id_from_store(store: &StoreDirectory) -> Result<String> {
    let bytes = read_leaf(store, "scope.json", MAX_MANAGEMENT_RECORD_BYTES)?;
    let v: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| err("scope.json is not JSON"))?;
    if v.get("kio_format_version")
        .and_then(serde_json::Value::as_str)
        != Some(KIO_FORMAT_VERSION)
    {
        return Err(err("scope.json format is not current"));
    }
    let id = v
        .get("scope_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| err("scope.json lacks scope_id"))?
        .to_owned();
    validate_id(&id, "scope_id")?;
    Ok(id)
}
fn child_name<'a>(p: &ManagementBinding, c: &'a ManagementBinding) -> Result<&'a str> {
    if c.canonical_root.parent() != Some(p.canonical_root.as_path()) {
        return Err(err("child is not physical direct descendant"));
    }
    let n = c
        .canonical_root
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| err("child basename invalid"))?;
    validate_basename(n)?;
    Ok(n)
}
fn validate_basename(n: &str) -> Result<()> {
    if n.is_empty()
        || n == "."
        || n == ".."
        || n == ".kio"
        || n.bytes()
            .any(|b| b.is_ascii_control() || b == b'/' || b == b'\\')
        || Path::new(n)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        Err(err("child basename is unsafe"))
    } else {
        Ok(())
    }
}
fn validate_id(v: &str, l: &str) -> Result<()> {
    if v.is_empty() || v.len() > 512 || v.bytes().any(|b| b.is_ascii_control()) {
        Err(err(format!("{l} is invalid")))
    } else {
        Ok(())
    }
}
fn read_leaf(d: &StoreDirectory, n: &str, max: u64) -> Result<Vec<u8>> {
    d.read_optional(Path::new(n), max)?
        .ok_or_else(|| err("management control file is absent"))
}
fn write_record(b: &ManagementBinding, r: &ManagementRecord, replace: bool) -> Result<()> {
    b.recheck()?;
    validate_record(b, r)?;
    let bytes = management_record_bytes(r)?;
    b.recheck()?;
    b.control.write_atomic(
        Path::new(MANAGEMENT_RECORD_LEAF),
        &bytes,
        if replace {
            Publication::Replace
        } else {
            Publication::CreateOnly
        },
    )?;
    b.recheck()
}
fn write_registration_record(binding: &ManagementBinding, record: &ManagementRecord) -> Result<()> {
    binding.recheck()?;
    let bytes = management_record_bytes(record)?;
    binding.control.write_atomic(
        Path::new(MANAGEMENT_RECORD_LEAF),
        &bytes,
        Publication::Replace,
    )?;
    binding.recheck()
}
fn management_record_bytes(record: &ManagementRecord) -> Result<Vec<u8>> {
    let bytes =
        serde_json::to_vec(record).map_err(|_| err("cannot serialize management record"))?;
    if bytes.len() as u64 > MAX_MANAGEMENT_RECORD_BYTES {
        return Err(err("management record exceeds byte limit"));
    }
    Ok(bytes)
}

fn same_volume(left: &DirectoryIdentity, right: &DirectoryIdentity) -> bool {
    match (left, right) {
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
    }
}
#[cfg(unix)]
fn file_identity(f: &File) -> Result<DirectoryIdentity> {
    use cap_primitives::fs::MetadataExt;
    let m = cap_fs::Metadata::from_file(f).map_err(io)?;
    if !m.is_dir() {
        return Err(err("retained handle is not a directory"));
    }
    Ok(DirectoryIdentity::Unix {
        device: m.dev(),
        inode: m.ino(),
    })
}
/// Read the strict platform identity of an already-retained real directory.
/// This rejects regular files and Windows reparse points; it does not adopt a
/// path or attempt to repair permissions on an arbitrary file.
pub fn directory_identity_from_handle(directory: &File) -> Result<DirectoryIdentity> {
    file_identity(directory)
}
#[cfg(windows)]
fn file_identity(f: &File) -> Result<DirectoryIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(f.as_raw_handle() as _, &mut information) } == 0 {
        return Err(err("cannot read Windows directory identity"));
    }
    if information.dwFileAttributes
        & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY
        == 0
        || information.dwFileAttributes
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    {
        return Err(err("retained handle is not normal directory"));
    }
    Ok(DirectoryIdentity::Windows {
        volume_serial_number: information.dwVolumeSerialNumber,
        file_index: ((information.nFileIndexHigh as u64) << 32) | information.nFileIndexLow as u64,
    })
}
fn err(m: impl Into<String>) -> KioError {
    KioError::new(
        "KIO-E-MANAGEMENT-AUTHORITY-001",
        m,
        json!({}),
        ExitCode::PermanentFailure,
    )
}
fn io(e: std::io::Error) -> KioError {
    KioError::new(
        "KIO-E-MANAGEMENT-AUTHORITY-001",
        "management authority I/O failed",
        json!({"cause":e.to_string()}),
        ExitCode::PermanentFailure,
    )
}
