//! Crash-resumable initialization of an explicitly named root.
//!
//! This is deliberately narrower than normal repository opening.  The
//! bootstrap journal is the only authority to complete an unborn `.kio`:
//! without it, a non-empty store is left untouched.

use super::{ExplicitRoot, clone_root, retained_kio};
use kio_core::management::{
    DirectoryIdentity, MANAGEMENT_RECORD_VERSION, ManagementAuthority, ManagementBinding,
    ManagementRecord, detect_case_insensitive_in_directory, directory_identity_from_handle,
    observe_direct_child, planned_management_record_bytes, read_record,
    read_registration_recovery_record, validate_controlled_root, validate_live_chain,
};
use kio_core::scope::{Repository, RetainedStoreLock, new_ulid};
use kio_core::store_dir::{AtomicWorkspaceState, Publication, StoreDirectory};
use kio_core::{KioError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const JOURNAL: &str = ".root-init-journal.json";
const GATE: &str = ".root-init-gate";
const JOURNAL_VERSION: u8 = 1;
const MAX_JOURNAL_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BootstrapJournal {
    version: u8,
    operation_id: String,
    planned_scope_id: String,
    canonical_root: PathBuf,
    root_identity: DirectoryIdentity,
    kio_identity: DirectoryIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    case_insensitive: Option<bool>,
}

/// Keeps a direct, authenticated parent's writer lease alive while an
/// otherwise-independent direct child receives its first `.kio` directory.
/// A parent with no enrollment is allowed to coexist; one cannot enroll the
/// same child until this admission either publishes or aborts.
struct ParentAdmission {
    _parent: Repository,
    _lock: RetainedStoreLock,
    binding: ManagementBinding,
    basename: String,
    child_identity: DirectoryIdentity,
}

/// Initialize, or resume initialization of, an explicitly supplied root.
///
/// The caller has already decided that this is an explicit root request. This
/// function makes no discovery decision and never adopts a non-empty `.kio`
/// without its strict bootstrap journal.
pub(super) fn initialize(root: &Path) -> Result<ExplicitRoot> {
    let canonical_root = root
        .canonicalize()
        .map_err(|error| KioError::io(error.to_string(), root.display().to_string()))?;
    let retained_root = StoreDirectory::open(&canonical_root)?;
    // Root control is an authority boundary, not a permission-repair step.
    // Refuse an unsafe root before creating `.kio`, a gate, or a journal.
    validate_controlled_root(retained_root.root_handle().as_ref(), &canonical_root)?;

    // Existing complete stores only validate and reopen. In particular, an
    // already-enrolled Child remains a Child; it must not be forced through
    // this new-root admission path.
    let existing_kio = retained_kio(&retained_root)?;
    let existing_complete = match &existing_kio {
        Some(handle) => {
            let directory = StoreDirectory::from_retained(
                handle
                    .try_clone()
                    .map_err(|e| KioError::io(e.to_string(), ".kio"))?,
                canonical_root.join(".kio"),
            )?;
            directory.contains_entry(Path::new("management.json"))?
                && !directory.contains_entry(Path::new(JOURNAL))?
        }
        None => false,
    };
    let _parent_admission = if existing_complete {
        None
    } else {
        admit_direct_parent(&canonical_root, &retained_root)?
    };
    validate_controlled_root(retained_root.root_handle().as_ref(), &canonical_root)?;

    let kio = match existing_kio {
        Some(kio) => StoreDirectory::from_retained(kio, canonical_root.join(".kio"))?,
        None => {
            let handle = retained_root
                .create_directory(Path::new(".kio"))
                .map_err(|_| {
                    KioError::invalid_usage("refusing to create over an existing .kio store")
                })?;
            StoreDirectory::from_retained(handle, canonical_root.join(".kio"))?
        }
    };

    // An explicit request may resume only a directory controlled by this OS
    // user. Do not repair the permissions of a pre-existing empty directory.
    kio_core::private_fs::verify_private_directory_handle(&kio)?;

    let binding = ManagementBinding::from_retained(
        clone_root(&retained_root)?,
        kio.root_handle().as_ref().try_clone().map_err(|e| {
            KioError::io(
                e.to_string(),
                canonical_root.join(".kio").display().to_string(),
            )
        })?,
        canonical_root.clone(),
    )?;

    // Read and validate an extant journal before creating any bootstrap gate.
    // Thus old-format, malformed, or unmanaged stores are observationally
    // rejected rather than being changed by an attempted recovery.
    let observed_journal = read_journal(&kio)?;
    let root_identity = directory_identity_from_handle(retained_root.root_handle().as_ref())?;
    let kio_identity = directory_identity_from_handle(kio.root_handle().as_ref())?;
    if let Some(journal) = &observed_journal {
        verify_journal(journal, &canonical_root, &root_identity, &kio_identity)?;
    }
    binding.revalidate()?;
    validate_controlled_root(retained_root.root_handle().as_ref(), &canonical_root)?;
    if kio.contains_entry(Path::new("management.json"))? && observed_journal.is_none() {
        validate_live_chain(&binding)?;
        if kio.inspect_atomic()? == AtomicWorkspaceState::Pending {
            // A live, validated root may clean only its own validated atomic
            // residue, serialized by the bootstrap gate.
            let _gate = kio.lock_private_gate(Path::new(GATE))?;
            let _ = kio.recover_atomic(&[&kio, &retained_root])?;
            validate_live_chain(&binding)?;
        }
        let repo = Repository::open_bound_existing(
            canonical_root,
            clone_root(&retained_root)?,
            kio.root_handle()
                .as_ref()
                .try_clone()
                .map_err(|e| KioError::io(e.to_string(), ".kio"))?,
        )?;
        verify_parent_admission(_parent_admission.as_ref(), &retained_root)?;
        return Ok(ExplicitRoot::Existing(repo));
    }

    if observed_journal.is_none() {
        preflight_unmarked_retry(&kio)?;
    }

    // Unlike a normal store lease this gate is part of the bootstrap's planned
    // inventory. It is retained after completion so a concurrent explicit
    // initialization serializes before it can create a second journal.
    let _gate = kio.lock_private_gate(Path::new(GATE))?;
    binding.revalidate()?;

    let after_gate_journal = read_journal(&kio)?;
    if kio.contains_entry(Path::new("management.json"))? && after_gate_journal.is_none() {
        validate_live_chain(&binding)?;
        let repo = Repository::open_bound_existing(
            canonical_root,
            clone_root(&retained_root)?,
            kio.root_handle()
                .as_ref()
                .try_clone()
                .map_err(|e| KioError::io(e.to_string(), ".kio"))?,
        )?;
        verify_parent_admission(_parent_admission.as_ref(), &retained_root)?;
        return Ok(ExplicitRoot::Existing(repo));
    }

    let mut journal = match after_gate_journal {
        Some(journal) => {
            verify_journal(&journal, &canonical_root, &root_identity, &kio_identity)?;
            journal
        }
        None => {
            permit_unmarked_retry(&kio)?;
            let journal = BootstrapJournal {
                version: JOURNAL_VERSION,
                operation_id: new_ulid(&canonical_root),
                planned_scope_id: new_ulid(&canonical_root),
                canonical_root: canonical_root.clone(),
                root_identity: root_identity.clone(),
                kio_identity: kio_identity.clone(),
                case_insensitive: None,
            };
            binding.revalidate()?;
            validate_controlled_root(retained_root.root_handle().as_ref(), &canonical_root)?;
            write_journal(&kio, &journal, Publication::CreateOnly)?;
            journal
        }
    };

    // Do not let an abandoned atomic writer from a prior attempt affect this
    // store until the journal has bound its exact directory identity.
    binding.revalidate()?;
    // The case probe targets the working root while its private atomic
    // workspace belongs to `.kio`; both retained targets are explicit.
    let _ = kio.recover_atomic(&[&kio, &retained_root])?;
    verify_journal(&journal, &canonical_root, &root_identity, &kio_identity)?;

    let case_insensitive = match journal.case_insensitive {
        Some(value) => value,
        None => {
            let journal_bytes = journal_bytes(&journal)?;
            kio_core::scope::complete_planned_kio_layout(
                &kio,
                &canonical_root,
                &journal.planned_scope_id,
                &[(GATE, b""), (JOURNAL, journal_bytes.as_slice())],
            )?;
            validate_controlled_root(retained_root.root_handle().as_ref(), &canonical_root)?;
            // Case behavior belongs to the user-visible root directory, not
            // its private store. The core helper keeps transient artifacts in
            // `.kio` and resumes any verified interrupted probe itself.
            let value = detect_case_insensitive_in_directory(&retained_root, &kio)?;
            journal.case_insensitive = Some(value);
            write_journal(&kio, &journal, Publication::Replace)?;
            value
        }
    };
    let record = planned_root_record(&journal, case_insensitive)?;
    let record_bytes = planned_management_record_bytes(&record)?;
    let journal_bytes = journal_bytes(&journal)?;
    validate_controlled_root(retained_root.root_handle().as_ref(), &canonical_root)?;
    kio_core::scope::complete_planned_kio_layout(
        &kio,
        &canonical_root,
        &journal.planned_scope_id,
        &[
            (GATE, b""),
            (JOURNAL, journal_bytes.as_slice()),
            ("management.json", record_bytes.as_slice()),
        ],
    )?;

    // The record was written through the planned layout only after its exact
    // bytes were bound to this journal. The recovery reader bypasses the
    // bootstrap gate but still validates strict record shape and scope ID.
    let recovered = read_registration_recovery_record(&binding)?;
    if recovered != record {
        return Err(KioError::invalid_usage(
            "published root management record differs from bootstrap plan",
        ));
    }

    // Normal readers reject the marker. Once the exact recovery record has
    // been proven, remove only this journal and then use the normal chain.
    binding.revalidate()?;
    kio.quarantine_then_remove(Path::new(JOURNAL), &journal_bytes, MAX_JOURNAL_BYTES)?;
    let repo = Repository::open_bound_existing(
        canonical_root,
        clone_root(&retained_root)?,
        kio.root_handle()
            .as_ref()
            .try_clone()
            .map_err(|e| KioError::io(e.to_string(), ".kio"))?,
    )?;
    validate_live_chain(&binding)?;
    verify_parent_admission(_parent_admission.as_ref(), &retained_root)?;
    Ok(ExplicitRoot::Created(repo))
}

fn admit_direct_parent(
    canonical_root: &Path,
    retained_root: &StoreDirectory,
) -> Result<Option<ParentAdmission>> {
    let Some(parent_path) = canonical_root.parent() else {
        return Ok(None);
    };
    let parent = StoreDirectory::open(parent_path)?;
    if !parent.contains_entry(Path::new(".kio"))? {
        return Ok(None);
    }
    let Some(basename) = canonical_root.file_name().and_then(|name| name.to_str()) else {
        return Err(KioError::invalid_usage(
            "explicit root basename is invalid beneath a managed parent",
        ));
    };

    // A present `.kio` that cannot authenticate a live management chain is
    // ambiguous authority, not permission to mint an independent root.
    let parent_kio = parent.open_directory(Path::new(".kio")).map_err(|_| {
        KioError::invalid_usage(
            "immediate parent has an unreadable or unsafe .kio; refusing ambiguous root creation",
        )
    })?;
    let parent_repo = Repository::open_bound_existing(
        parent_path.to_path_buf(),
        clone_root(&parent)?,
        parent_kio,
    )
    .map_err(|_| {
        KioError::invalid_usage(
            "immediate parent is not a complete managed scope; refusing ambiguous root creation",
        )
    })?;
    let parent_binding = super::binding_for_repo(&parent_repo)?;
    // This is observational: it authenticates the parent before `lock_store`
    // can create any parent-private lease material.
    validate_live_chain(&parent_binding)?;
    let child_identity = directory_identity_from_handle(retained_root.root_handle().as_ref())?;
    let lock = parent_repo.lock_store()?;
    let admission = ParentAdmission {
        _parent: parent_repo,
        _lock: lock,
        binding: parent_binding,
        basename: basename.to_owned(),
        child_identity,
    };
    // Read after the lease and observation. Any membership, even a stale
    // different identity, belongs to the parent lifecycle and is never
    // retired or converted by an explicit child-root request.
    admission.verify(retained_root)?;
    Ok(Some(admission))
}

fn verify_parent_admission(
    admission: Option<&ParentAdmission>,
    retained_root: &StoreDirectory,
) -> Result<()> {
    if let Some(admission) = admission {
        admission.verify(retained_root)?;
    }
    Ok(())
}

impl ParentAdmission {
    fn verify(&self, retained_root: &StoreDirectory) -> Result<()> {
        self.binding.revalidate()?;
        validate_live_chain(&self.binding)?;
        let observed = observe_direct_child(&self.binding, &self.basename)?.ok_or_else(|| {
            KioError::invalid_usage("explicit root disappeared from its authenticated parent")
        })?;
        let retained_identity =
            directory_identity_from_handle(retained_root.root_handle().as_ref())?;
        if observed.directory_identity != self.child_identity
            || retained_identity != self.child_identity
        {
            return Err(KioError::invalid_usage(
                "explicit root no longer matches its authenticated parent entry",
            ));
        }
        if read_record(&self.binding)?
            .children
            .contains_key(&self.basename)
        {
            return Err(KioError::invalid_usage(
                "explicit root is enrolled by its immediate parent; parent recovery is required",
            ));
        }
        self.binding.revalidate()
    }
}

fn planned_root_record(
    journal: &BootstrapJournal,
    case_insensitive: bool,
) -> Result<ManagementRecord> {
    if journal.case_insensitive != Some(case_insensitive) {
        return Err(KioError::invalid_usage(
            "bootstrap case behavior is not durably pinned",
        ));
    }
    Ok(ManagementRecord {
        version: MANAGEMENT_RECORD_VERSION,
        registration_generation: 1,
        scope_id: journal.planned_scope_id.clone(),
        canonical_root: journal.canonical_root.clone(),
        directory_identity: journal.root_identity.clone(),
        case_insensitive,
        authority: ManagementAuthority::Root,
        children: BTreeMap::new(),
    })
}

fn read_journal(kio: &StoreDirectory) -> Result<Option<BootstrapJournal>> {
    let Some(bytes) = kio.read_optional(Path::new(JOURNAL), MAX_JOURNAL_BYTES)? else {
        return Ok(None);
    };
    kio.ensure_owner_private(Path::new(JOURNAL))?;
    let journal: BootstrapJournal = serde_json::from_slice(&bytes)
        .map_err(|_| KioError::invalid_usage("root bootstrap journal is invalid"))?;
    if journal_bytes(&journal)? != bytes {
        return Err(KioError::invalid_usage(
            "root bootstrap journal is not canonical",
        ));
    }
    if journal.version != JOURNAL_VERSION
        || !is_ulid(&journal.operation_id)
        || !is_ulid(&journal.planned_scope_id)
    {
        return Err(KioError::invalid_usage(
            "root bootstrap journal has an invalid envelope",
        ));
    }
    Ok(Some(journal))
}

fn write_journal(
    kio: &StoreDirectory,
    journal: &BootstrapJournal,
    publication: Publication,
) -> Result<()> {
    let bytes = journal_bytes(journal)?;
    kio.write_atomic(Path::new(JOURNAL), &bytes, publication)
}

fn journal_bytes(journal: &BootstrapJournal) -> Result<Vec<u8>> {
    serde_json::to_vec(journal)
        .map_err(|_| KioError::invalid_usage("cannot serialize root bootstrap journal"))
        .and_then(|bytes| {
            if bytes.len() as u64 > MAX_JOURNAL_BYTES {
                Err(KioError::invalid_usage(
                    "root bootstrap journal exceeds its size limit",
                ))
            } else {
                Ok(bytes)
            }
        })
}

fn verify_journal(
    journal: &BootstrapJournal,
    canonical_root: &Path,
    root_identity: &DirectoryIdentity,
    kio_identity: &DirectoryIdentity,
) -> Result<()> {
    if journal.canonical_root != canonical_root
        || &journal.root_identity != root_identity
        || &journal.kio_identity != kio_identity
    {
        return Err(KioError::invalid_usage(
            "root bootstrap journal does not bind the current root and .kio identities",
        ));
    }
    Ok(())
}

fn preflight_unmarked_retry(kio: &StoreDirectory) -> Result<()> {
    validate_unmarked_retry(kio, false)
}

fn permit_unmarked_retry(kio: &StoreDirectory) -> Result<()> {
    validate_unmarked_retry(kio, true)
}

// Both admission passes observe the same exact crash residue. Before taking
// the bootstrap gate it may be absent; after serialization it must be present.
// This helper never creates locks, repairs permissions, or recovers artifacts.
fn validate_unmarked_retry(kio: &StoreDirectory, require_gate: bool) -> Result<()> {
    let entries = kio.entries(Path::new(""))?;
    let has_gate = entries
        .iter()
        .any(|entry| entry.is_regular_file && entry.name == GATE);
    let has_atomic = entries
        .iter()
        .any(|entry| entry.is_directory && entry.name == ".kio-atomic");
    if entries.len() != usize::from(has_gate) + usize::from(has_atomic)
        || (require_gate && !has_gate)
    {
        return Err(KioError::invalid_usage(
            "refusing to adopt a non-empty unmanaged .kio store without a root bootstrap journal",
        ));
    }
    if has_gate {
        // The retained regular-file open checks size and hardlinks without a
        // ReadFile request against the Windows gate's exclusively locked range.
        let _gate = kio.open_regular_read(Path::new(GATE), 0)?;
        kio.ensure_owner_private(Path::new(GATE))?;
    }
    if has_atomic && !exact_abandoned_atomic_workspace(kio)? {
        return Err(KioError::invalid_usage(
            "refusing to adopt a non-empty unmanaged .kio store without a root bootstrap journal",
        ));
    }
    Ok(())
}

fn exact_abandoned_atomic_workspace(kio: &StoreDirectory) -> Result<bool> {
    if !matches!(
        kio.inspect_atomic()?,
        AtomicWorkspaceState::Clean | AtomicWorkspaceState::Pending
    ) {
        return Ok(false);
    }
    let atomic = StoreDirectory::from_retained(
        kio.open_directory(Path::new(".kio-atomic"))?,
        kio.path().join(".kio-atomic"),
    )?;
    let names = atomic.entries(Path::new(""))?;
    Ok(names.iter().all(|entry| {
        entry.is_regular_file && matches!(entry.name.to_str(), Some(".gate" | "write"))
    }) && names.iter().any(|entry| entry.name == ".gate"))
}

fn is_ulid(value: &str) -> bool {
    value.len() == 26
        && value.as_bytes()[0] <= b'7'
        && value.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::management::{binding_for_repo, reconcile_planned_child};
    use kio_core::management::read_record;
    use kio_pipeline::scan::BoundPlannedChild;
    use std::fs;

    fn canonical_tempdir() -> tempfile::TempDir {
        // Strict store ancestry checks require the resolved macOS temporary root.
        tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap()
    }

    fn bound(path: &Path) -> BoundPlannedChild {
        let canonical_root = path.canonicalize().unwrap();
        let root = StoreDirectory::open(&canonical_root)
            .unwrap()
            .root_handle()
            .as_ref()
            .try_clone()
            .unwrap();
        BoundPlannedChild {
            canonical_root,
            root,
            inherited_rules: Vec::new(),
        }
    }

    fn tree_fingerprint(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        fn visit(base: &Path, current: &Path, output: &mut Vec<(PathBuf, Vec<u8>)>) {
            let mut entries: Vec<_> = fs::read_dir(current)
                .unwrap()
                .map(|entry| entry.unwrap())
                .collect();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                if entry.file_type().unwrap().is_dir() {
                    output.push((path.strip_prefix(base).unwrap().to_path_buf(), Vec::new()));
                    visit(base, &path, output);
                } else {
                    output.push((
                        path.strip_prefix(base).unwrap().to_path_buf(),
                        fs::read(path).unwrap(),
                    ));
                }
            }
        }
        let mut output = Vec::new();
        visit(root, root, &mut output);
        output
    }

    #[test]
    fn empty_store_is_bootstrapped_and_reopens_as_existing() {
        let root = canonical_tempdir();
        let created = initialize(root.path()).unwrap();
        assert!(matches!(created, ExplicitRoot::Created(_)));
        assert!(matches!(
            initialize(root.path()).unwrap(),
            ExplicitRoot::Existing(_)
        ));
        assert!(!root.path().join(".kio").join(JOURNAL).exists());
    }

    #[cfg(unix)]
    #[test]
    fn read_shared_root_control_is_admitted() {
        use std::os::unix::fs::PermissionsExt;
        let root = canonical_tempdir();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            initialize(root.path()).unwrap(),
            ExplicitRoot::Created(_)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_root_control_is_rejected_before_bootstrap_writes() {
        use std::os::unix::fs::PermissionsExt;
        let root = canonical_tempdir();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o777)).unwrap();
        let before = tree_fingerprint(root.path());
        assert!(initialize(root.path()).is_err());
        assert_eq!(tree_fingerprint(root.path()), before);
        assert!(!root.path().join(".kio").exists());
    }

    #[cfg(unix)]
    #[test]
    fn preexisting_nonprivate_empty_store_is_rejected_without_changes() {
        use std::os::unix::fs::PermissionsExt;
        let root = canonical_tempdir();
        let path = root.path().join(".kio");
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap();
        let before = tree_fingerprint(root.path());
        assert!(initialize(root.path()).is_err());
        assert_eq!(tree_fingerprint(root.path()), before);
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o777
        );
    }

    #[cfg(windows)]
    #[test]
    fn preexisting_permissive_dacl_is_rejected_without_changes() {
        let root = canonical_tempdir();
        let path = root.path().join(".kio");
        fs::create_dir(&path).unwrap();
        let status = std::process::Command::new("icacls.exe")
            .arg(&path)
            .args(["/grant", "*S-1-1-0:(OI)(CI)F"])
            .status()
            .unwrap();
        assert!(status.success());
        let before = tree_fingerprint(root.path());
        assert!(initialize(root.path()).is_err());
        assert_eq!(tree_fingerprint(root.path()), before);
        let directory = StoreDirectory::open(&path).unwrap();
        assert!(kio_core::private_fs::verify_private_directory_handle(&directory).is_err());
    }

    #[test]
    fn unmarked_nonempty_store_is_preserved() {
        let root = canonical_tempdir();
        fs::create_dir(root.path().join(".kio")).unwrap();
        fs::write(root.path().join(".kio").join("legacy"), b"do not touch").unwrap();
        let before = tree_fingerprint(root.path());
        assert!(initialize(root.path()).is_err());
        assert_eq!(tree_fingerprint(root.path()), before);
    }

    #[test]
    fn malformed_journal_is_preserved() {
        let root = canonical_tempdir();
        fs::create_dir(root.path().join(".kio")).unwrap();
        let marker = root.path().join(".kio").join(JOURNAL);
        fs::write(&marker, b"{}").unwrap();
        let before = tree_fingerprint(root.path());
        assert!(initialize(root.path()).is_err());
        assert_eq!(tree_fingerprint(root.path()), before);
    }

    #[test]
    fn empty_initial_directory_is_the_only_unmarked_fresh_state() {
        let root = canonical_tempdir();
        fs::create_dir(root.path().join(".kio")).unwrap();
        let kio = StoreDirectory::open(&root.path().join(".kio")).unwrap();
        assert!(preflight_unmarked_retry(&kio).is_ok());
        fs::write(root.path().join(".kio").join("old-format"), b"v0").unwrap();
        assert!(preflight_unmarked_retry(&kio).is_err());
    }

    fn unmarked_atomic_residue(root: &Path, staged: bool) -> StoreDirectory {
        let root_dir = StoreDirectory::open(root).unwrap();
        let kio = StoreDirectory::from_retained(
            root_dir.create_directory(Path::new(".kio")).unwrap(),
            root.join(".kio"),
        )
        .unwrap();
        drop(kio.lock_private_gate(Path::new(GATE)).unwrap());
        let atomic = StoreDirectory::from_retained(
            kio.create_directory(Path::new(".kio-atomic")).unwrap(),
            kio.path().join(".kio-atomic"),
        )
        .unwrap();
        drop(atomic.lock_private_gate(Path::new(".gate")).unwrap());
        if staged {
            drop(atomic.lock_private_gate(Path::new("write")).unwrap());
            fs::write(
                atomic.path().join("write"),
                b"unpublished bootstrap journal",
            )
            .unwrap();
        }
        kio
    }

    #[test]
    fn unmarked_bootstrap_gate_and_atomic_write_residue_resume() {
        for staged in [false, true] {
            let root = canonical_tempdir();
            let kio = unmarked_atomic_residue(root.path(), staged);
            let before = tree_fingerprint(root.path());
            preflight_unmarked_retry(&kio).unwrap();
            assert_eq!(tree_fingerprint(root.path()), before);
            assert!(matches!(
                initialize(root.path()).unwrap(),
                ExplicitRoot::Created(_)
            ));
            assert!(matches!(
                initialize(root.path()).unwrap(),
                ExplicitRoot::Existing(_)
            ));
            assert!(!kio.path().join(".kio-atomic/write").exists());
        }
    }

    #[test]
    fn unmarked_bootstrap_residue_rejects_foreign_leaves_without_changes() {
        for leaf in ["foreign", ".kio-atomic/foreign"] {
            let root = canonical_tempdir();
            let kio = unmarked_atomic_residue(root.path(), true);
            fs::write(kio.path().join(leaf), b"preserve").unwrap();
            let before = tree_fingerprint(root.path());
            assert!(initialize(root.path()).is_err());
            assert_eq!(tree_fingerprint(root.path()), before);
        }
    }

    #[test]
    fn unmarked_bootstrap_residue_rejects_malformed_gates_without_changes() {
        for leaf in [GATE, ".kio-atomic/.gate"] {
            let root = canonical_tempdir();
            let kio = unmarked_atomic_residue(root.path(), true);
            fs::write(kio.path().join(leaf), b"not empty").unwrap();
            let before = tree_fingerprint(root.path());
            assert!(initialize(root.path()).is_err());
            assert_eq!(tree_fingerprint(root.path()), before);
        }
    }

    #[cfg(unix)]
    #[test]
    fn unmarked_bootstrap_residue_rejects_nonprivate_and_hardlinked_gates() {
        use std::os::unix::fs::PermissionsExt;
        for hardlinked in [false, true] {
            let root = canonical_tempdir();
            let kio = unmarked_atomic_residue(root.path(), true);
            let gate = kio.path().join(GATE);
            if hardlinked {
                fs::hard_link(&gate, root.path().join("gate-alias")).unwrap();
            } else {
                fs::set_permissions(&gate, fs::Permissions::from_mode(0o644)).unwrap();
            }
            let before = tree_fingerprint(root.path());
            let mode = fs::metadata(&gate).unwrap().permissions().mode();
            assert!(initialize(root.path()).is_err());
            assert_eq!(tree_fingerprint(root.path()), before);
            assert_eq!(fs::metadata(&gate).unwrap().permissions().mode(), mode);
        }
    }

    #[test]
    fn unmarked_bootstrap_residue_rejects_ready_removal_without_changes() {
        let root = canonical_tempdir();
        let kio = unmarked_atomic_residue(root.path(), false);
        let atomic = StoreDirectory::from_retained(
            kio.open_directory(Path::new(".kio-atomic")).unwrap(),
            kio.path().join(".kio-atomic"),
        )
        .unwrap();
        let identity = directory_identity_from_handle(kio.root_handle().as_ref()).unwrap();
        // A canonical owner-bound ready intent passes inspect_atomic, but it
        // must never authorize adoption or removal in an unmarked store.
        let intent = serde_json::json!({
            "version": 1,
            "owner_id": identity,
            "target_root_id": identity,
            "parent_chain_ids": [identity],
            "relative": "victim",
            "source_id": identity,
            "sha256": "0".repeat(64),
            "length": 0,
            "max_bytes": 0
        });
        drop(atomic.lock_private_gate(Path::new("remove.json")).unwrap());
        fs::write(
            atomic.path().join("remove.json"),
            serde_jcs::to_vec(&intent).unwrap(),
        )
        .unwrap();
        assert_eq!(kio.inspect_atomic().unwrap(), AtomicWorkspaceState::Pending);
        let before = tree_fingerprint(root.path());
        assert!(initialize(root.path()).is_err());
        assert_eq!(tree_fingerprint(root.path()), before);
    }

    #[test]
    fn partial_planned_layout_resumes_with_its_original_scope_id() {
        let root = canonical_tempdir();
        let canonical = root.path().canonicalize().unwrap();
        let root_dir = StoreDirectory::open(&canonical).unwrap();
        let kio = StoreDirectory::from_retained(
            root_dir.create_directory(Path::new(".kio")).unwrap(),
            canonical.join(".kio"),
        )
        .unwrap();
        let _gate = kio.lock_private_gate(Path::new(GATE)).unwrap();
        let journal = BootstrapJournal {
            version: JOURNAL_VERSION,
            operation_id: new_ulid(&canonical),
            planned_scope_id: new_ulid(&canonical),
            canonical_root: canonical.clone(),
            root_identity: directory_identity_from_handle(root_dir.root_handle().as_ref()).unwrap(),
            kio_identity: directory_identity_from_handle(kio.root_handle().as_ref()).unwrap(),
            case_insensitive: None,
        };
        write_journal(&kio, &journal, Publication::CreateOnly).unwrap();
        let bytes = journal_bytes(&journal).unwrap();
        kio_core::scope::complete_planned_kio_layout(
            &kio,
            &canonical,
            &journal.planned_scope_id,
            &[(GATE, b""), (JOURNAL, bytes.as_slice())],
        )
        .unwrap();
        drop(_gate);

        assert!(matches!(
            initialize(&canonical).unwrap(),
            ExplicitRoot::Created(_)
        ));
        let scope: serde_json::Value =
            serde_json::from_slice(&fs::read(canonical.join(".kio/scope.json")).unwrap()).unwrap();
        assert_eq!(scope["scope_id"], journal.planned_scope_id);
        assert!(!canonical.join(".kio").join(JOURNAL).exists());
    }

    #[cfg(unix)]
    #[test]
    fn changed_root_control_refuses_resume_and_preserves_journal() {
        use std::os::unix::fs::PermissionsExt;
        let root = canonical_tempdir();
        let canonical = root.path().canonicalize().unwrap();
        let root_dir = StoreDirectory::open(&canonical).unwrap();
        let kio = StoreDirectory::from_retained(
            root_dir.create_directory(Path::new(".kio")).unwrap(),
            canonical.join(".kio"),
        )
        .unwrap();
        let gate = kio.lock_private_gate(Path::new(GATE)).unwrap();
        let journal = BootstrapJournal {
            version: JOURNAL_VERSION,
            operation_id: new_ulid(&canonical),
            planned_scope_id: new_ulid(&canonical),
            canonical_root: canonical.clone(),
            root_identity: directory_identity_from_handle(root_dir.root_handle().as_ref()).unwrap(),
            kio_identity: directory_identity_from_handle(kio.root_handle().as_ref()).unwrap(),
            case_insensitive: None,
        };
        write_journal(&kio, &journal, Publication::CreateOnly).unwrap();
        drop(gate);
        let marker = canonical.join(".kio").join(JOURNAL);
        let before = fs::read(&marker).unwrap();
        fs::set_permissions(&canonical, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(initialize(&canonical).is_err());
        assert_eq!(fs::read(marker).unwrap(), before);
    }

    #[test]
    fn journal_identity_refuses_a_replaced_root_or_store() {
        let root = canonical_tempdir();
        let canonical = root.path().canonicalize().unwrap();
        let root_dir = StoreDirectory::open(&canonical).unwrap();
        let kio = StoreDirectory::from_retained(
            root_dir.create_directory(Path::new(".kio")).unwrap(),
            canonical.join(".kio"),
        )
        .unwrap();
        let journal = BootstrapJournal {
            version: JOURNAL_VERSION,
            operation_id: new_ulid(&canonical),
            planned_scope_id: new_ulid(&canonical),
            canonical_root: canonical.clone(),
            root_identity: directory_identity_from_handle(root_dir.root_handle().as_ref()).unwrap(),
            kio_identity: directory_identity_from_handle(kio.root_handle().as_ref()).unwrap(),
            case_insensitive: None,
        };
        let other = canonical_tempdir();
        let other_identity = directory_identity_from_handle(
            StoreDirectory::open(other.path())
                .unwrap()
                .root_handle()
                .as_ref(),
        )
        .unwrap();
        assert!(
            verify_journal(&journal, &canonical, &other_identity, &journal.kio_identity).is_err()
        );
        assert!(
            verify_journal(
                &journal,
                &canonical,
                &journal.root_identity,
                &other_identity
            )
            .is_err()
        );
    }

    #[test]
    fn enrolled_same_identity_child_without_kio_cannot_become_a_root() {
        let parent_path = canonical_tempdir();
        let parent = match initialize(parent_path.path()).unwrap() {
            ExplicitRoot::Created(repo) => repo,
            ExplicitRoot::Existing(_) => panic!("fresh parent unexpectedly existed"),
        };
        let child = parent_path.path().join("child");
        fs::create_dir(&child).unwrap();
        reconcile_planned_child(&parent, bound(&child)).unwrap();
        let before = read_record(&binding_for_repo(&parent).unwrap())
            .unwrap()
            .children["child"]
            .clone();
        fs::remove_dir_all(child.join(".kio")).unwrap();
        assert!(initialize(&child).is_err());
        assert!(!child.join(".kio").exists());
        assert_eq!(
            read_record(&binding_for_repo(&parent).unwrap())
                .unwrap()
                .children["child"],
            before
        );
    }

    #[test]
    fn healthy_parent_allows_unenrolled_sibling_and_preserves_existing_child() {
        let parent_path = canonical_tempdir();
        let parent = match initialize(parent_path.path()).unwrap() {
            ExplicitRoot::Created(repo) => repo,
            ExplicitRoot::Existing(_) => panic!("fresh parent unexpectedly existed"),
        };
        let child = parent_path.path().join("child");
        fs::create_dir(&child).unwrap();
        reconcile_planned_child(&parent, bound(&child)).unwrap();
        assert!(matches!(
            initialize(&child).unwrap(),
            ExplicitRoot::Existing(_)
        ));

        let sibling = parent_path.path().join("sibling");
        fs::create_dir(&sibling).unwrap();
        assert!(matches!(
            initialize(&sibling).unwrap(),
            ExplicitRoot::Created(_)
        ));
        assert!(
            !read_record(&binding_for_repo(&parent).unwrap())
                .unwrap()
                .children
                .contains_key("sibling")
        );
    }

    #[test]
    fn corrupt_parent_kio_refuses_before_child_writes() {
        let parent = canonical_tempdir();
        let child = parent.path().join("child");
        fs::create_dir(&child).unwrap();
        fs::write(parent.path().join(".kio"), b"not a directory").unwrap();
        let before = tree_fingerprint(&child);
        assert!(initialize(&child).is_err());
        assert_eq!(tree_fingerprint(&child), before);
        assert!(!child.join(".kio").exists());
    }
}
