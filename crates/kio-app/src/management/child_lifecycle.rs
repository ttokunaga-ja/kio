//! Durable reconciliation for direct-child membership and publication.
//!
//! The journals live in the parent's retained `.kio`. A replacement at the
//! child basename must never complete work for the old directory.

mod cancel;

use super::binding_for_repo;
use crate::grants::PrivateGrantStore;
use crate::index_to_kio;
use kio_core::management::{
    ChildEnrollment, DirectoryIdentity, ManagementBinding, ManagementRecord,
    compare_and_remove_child_enrollment, enroll_child, observe_direct_child,
    planned_child_management_record, read_record, read_registration_recovery_record,
    validate_controlled_root,
};
use kio_core::scope::{Repository, new_ulid};
use kio_core::store_dir::{
    ATOMIC_WORKSPACE_DIR, AtomicWorkspaceState, Publication, StoreDirectory,
};
use kio_core::{KioError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

const RETIREMENT: &str = "child-retirement.json";
const INITIALIZATIONS: &str = "child-initializations";
const STAGE_MARKER: &str = ".child-init-journal.json";
const MAX_JOURNAL: u64 = 64 * 1024;
const MAX_INITIALIZATIONS: usize = 1024;
const MAX_INITIALIZATION_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Retirement {
    version: u8,
    basename: String,
    enrollment: ChildEnrollment,
    old_kio_path: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Initialization {
    version: u8,
    operation_id: String,
    basename: String,
    retained_child_identity: DirectoryIdentity,
    planned_scope_id: String,
    enrollment_token: String,
    stage_leaf: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stage_identity: Option<DirectoryIdentity>,
    management: ManagementRecord,
}

fn control(parent: &ManagementBinding) -> Result<StoreDirectory> {
    StoreDirectory::from_retained(
        parent.kio_handle().try_clone().map_err(|e| {
            KioError::io(e.to_string(), parent.canonical_root().display().to_string())
        })?,
        parent.canonical_root().join(".kio"),
    )
}

fn encode<T: Serialize>(value: &T, what: &str) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| KioError::invalid_usage(format!("cannot serialize {what}")))?;
    if bytes.len() as u64 > MAX_JOURNAL {
        return Err(KioError::invalid_usage(format!("{what} exceeds limit")));
    }
    Ok(bytes)
}

fn decode<T: for<'a> Deserialize<'a> + Serialize>(bytes: &[u8], what: &str) -> Result<T> {
    if bytes.len() as u64 > MAX_JOURNAL {
        return Err(KioError::invalid_usage(format!("{what} exceeds limit")));
    }
    let value: T = serde_json::from_slice(bytes)
        .map_err(|_| KioError::invalid_usage(format!("invalid {what}")))?;
    if encode(&value, what)? != bytes {
        return Err(KioError::invalid_usage(format!("non-canonical {what}")));
    }
    Ok(value)
}

fn operation_leaf(operation_id: &str) -> Result<PathBuf> {
    if operation_id.len() != 26
        || !operation_id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z'))
        || operation_id.as_bytes()[0] > b'7'
    {
        return Err(KioError::invalid_usage(
            "child initialization operation ID is invalid",
        ));
    }
    Ok(PathBuf::from(format!("{operation_id}.json")))
}

fn initialization_directory(
    parent_control: &StoreDirectory,
    create: bool,
) -> Result<Option<StoreDirectory>> {
    if parent_control.contains_entry(Path::new(INITIALIZATIONS))? {
        let directory = StoreDirectory::from_retained(
            parent_control.open_directory(Path::new(INITIALIZATIONS))?,
            parent_control.path().join(INITIALIZATIONS),
        )?;
        kio_core::private_fs::verify_private_directory_handle(&directory)?;
        return Ok(Some(directory));
    }
    if !create {
        return Ok(None);
    }
    let directory = StoreDirectory::from_retained(
        parent_control.create_directory(Path::new(INITIALIZATIONS))?,
        parent_control.path().join(INITIALIZATIONS),
    )?;
    kio_core::private_fs::verify_private_directory_handle(&directory)?;
    Ok(Some(directory))
}

fn pending_initializations(control: &StoreDirectory) -> Result<Vec<(Initialization, Vec<u8>)>> {
    let Some(directory) = initialization_directory(control, false)? else {
        return Ok(Vec::new());
    };
    let atomic = directory.inspect_atomic()?;
    if atomic == AtomicWorkspaceState::Pending {
        return Err(KioError::invalid_usage(
            "child initialization journal atomic recovery is required",
        ));
    }
    let entries = directory
        .entries_optional(Path::new(""))?
        .unwrap_or_default();
    if entries.len() > MAX_INITIALIZATIONS {
        return Err(KioError::invalid_usage(
            "too many child initialization journals",
        ));
    }
    let mut pending = Vec::with_capacity(entries.len());
    let mut total = 0usize;
    for entry in entries {
        let name = entry.name.to_str().ok_or_else(|| {
            KioError::invalid_usage("child initialization journal name is invalid")
        })?;
        if name == ATOMIC_WORKSPACE_DIR && atomic == AtomicWorkspaceState::Clean {
            continue;
        }
        let operation_id = name.strip_suffix(".json").ok_or_else(|| {
            KioError::invalid_usage("child initialization journal leaf is invalid")
        })?;
        if !entry.is_regular_file
            || entry.is_directory
            || operation_leaf(operation_id)?
                .file_name()
                .and_then(|n| n.to_str())
                != Some(name)
        {
            return Err(KioError::invalid_usage(
                "child initialization journal inventory is invalid",
            ));
        }
        let bytes = directory
            .read_optional(Path::new(name), MAX_JOURNAL)?
            .ok_or_else(|| KioError::invalid_usage("child initialization journal disappeared"))?;
        total = total.checked_add(bytes.len()).ok_or_else(|| {
            KioError::invalid_usage("child initialization journal inventory exceeds limit")
        })?;
        if total > MAX_INITIALIZATION_BYTES {
            return Err(KioError::invalid_usage(
                "child initialization journal inventory exceeds limit",
            ));
        }
        let journal: Initialization = decode(&bytes, "child initialization journal")?;
        if journal.operation_id != operation_id {
            return Err(KioError::invalid_usage(
                "child initialization journal operation mismatch",
            ));
        }
        pending.push((journal, bytes));
    }
    pending.sort_by(|a, b| a.0.operation_id.cmp(&b.0.operation_id));
    ensure_unique_initialization_basenames(&pending)?;
    Ok(pending)
}

fn recover_lifecycle_atomic(directory: &StoreDirectory) -> Result<()> {
    let allowed = [directory];
    let _ = directory.recover_atomic(&allowed)?;
    if let Some(operations) = initialization_directory(directory, false)? {
        let allowed = [&operations];
        let _ = operations.recover_atomic(&allowed)?;
    }
    Ok(())
}

fn ensure_unique_initialization_basenames(pending: &[(Initialization, Vec<u8>)]) -> Result<()> {
    let mut names = std::collections::BTreeSet::new();
    for (journal, _) in pending {
        if !names.insert(&journal.basename) {
            return Err(KioError::invalid_usage(
                "multiple child initialization journals name one child",
            ));
        }
    }
    Ok(())
}

fn write_initialization(
    control: &StoreDirectory,
    journal: &Initialization,
    bytes: &[u8],
    publication: Publication,
) -> Result<()> {
    let pending = pending_initializations(control)?;
    let old = pending
        .iter()
        .find(|(existing, _)| existing.operation_id == journal.operation_id);
    match publication {
        Publication::CreateOnly if old.is_some() => {
            return Err(KioError::invalid_usage(
                "child initialization operation already exists",
            ));
        }
        Publication::Replace if old.is_none() => {
            return Err(KioError::invalid_usage(
                "child initialization operation disappeared",
            ));
        }
        Publication::Upsert => {
            return Err(KioError::invalid_usage(
                "child initialization journal upsert is not allowed",
            ));
        }
        _ => {}
    }
    let existing: usize = pending.iter().map(|(_, value)| value.len()).sum();
    let replaced = old.map_or(0, |(_, value)| value.len());
    if existing
        .saturating_sub(replaced)
        .saturating_add(bytes.len())
        > MAX_INITIALIZATION_BYTES
    {
        return Err(KioError::invalid_usage(
            "child initialization journal inventory exceeds limit",
        ));
    }
    initialization_directory(control, true)?
        .ok_or_else(|| {
            KioError::invalid_usage("cannot create child initialization journal directory")
        })?
        .write_atomic(
            operation_leaf(&journal.operation_id)?.as_path(),
            bytes,
            publication,
        )
}

/// Retire stale memberships before discovery. The parent store lock is held
/// across planning and every durable mutation (core's locks are reentrant).
pub(crate) fn reconcile_stale_child_enrollments(parent: &Repository) -> Result<()> {
    reconcile_stale_child_enrollment(parent, None)
}

pub(crate) fn reconcile_stale_child_enrollment(
    parent: &Repository,
    target: Option<&str>,
) -> Result<()> {
    cancel::recover(parent)?;
    let _lock = parent.lock_store()?;
    let binding = binding_for_repo(parent)?;
    let store = control(&binding)?;
    recover_lifecycle_atomic(&store)?;
    if let Some(bytes) = store.read_optional(Path::new(RETIREMENT), MAX_JOURNAL)? {
        finish_retirement(
            &binding,
            &store,
            decode(&bytes, "child retirement journal")?,
            bytes,
        )?;
    }
    for (basename, enrollment) in read_record(&binding)?.children {
        if target.is_some_and(|name| name != basename) {
            continue;
        }
        match observe_direct_child(&binding, &basename)? {
            Some(observed) if observed.directory_identity == enrollment.directory_identity => {
                continue;
            }
            Some(_) | None => {}
        }
        let journal = Retirement {
            version: 1,
            basename: basename.clone(),
            enrollment: enrollment.clone(),
            old_kio_path: parent
                .canonical_root()
                .join(&basename)
                .join(".kio")
                .display()
                .to_string(),
        };
        let bytes = encode(&journal, "child retirement journal")?;
        store.write_atomic(Path::new(RETIREMENT), &bytes, Publication::CreateOnly)?;
        finish_retirement(&binding, &store, journal, bytes)?;
    }
    Ok(())
}

pub(super) fn cancel_child_initialization(
    parent: &Repository,
    preview: bool,
    operation: Option<&str>,
) -> Result<Value> {
    cancel::run(parent, preview, operation)
}

fn finish_retirement(
    parent: &ManagementBinding,
    store: &StoreDirectory,
    journal: Retirement,
    bytes: Vec<u8>,
) -> Result<()> {
    if journal.version != 1 {
        return Err(KioError::invalid_usage(
            "unsupported child retirement journal",
        ));
    }
    if !journal.enrollment.directory_identity.is_native_platform() {
        return Err(KioError::invalid_usage(
            "child retirement journal has a non-native directory identity",
        ));
    }
    if journal.old_kio_path
        != parent
            .canonical_root()
            .join(&journal.basename)
            .join(".kio")
            .display()
            .to_string()
    {
        return Err(KioError::invalid_usage(
            "child retirement journal path is inconsistent",
        ));
    }
    // Recheck before the authority cutoff. A live old identity is never retired.
    match observe_direct_child(parent, &journal.basename)? {
        Some(observed) if observed.directory_identity == journal.enrollment.directory_identity => {
            return Err(KioError::invalid_usage(
                "retirement journal child identity is live",
            ));
        }
        Some(_) | None => {}
    }
    // Missing means replay; a different enrollment is a conflicting new child.
    compare_and_remove_child_enrollment(parent, &journal.basename, &journal.enrollment)?;
    if PrivateGrantStore::open_readonly(&crate::approvals::store_path())?.is_some() {
        PrivateGrantStore::open_or_create(&crate::approvals::store_path())?.revoke_scope_instance(
            &journal.enrollment.scope_id,
            &parent.canonical_root().join(&journal.basename),
            &journal.enrollment.directory_identity,
            crate::now_utc_seconds(),
        )?;
    }
    let registry_path = kio_index::registry::default_registry_path().map_err(index_to_kio)?;
    match kio_index::registry::RegistryDb::open_read_only(&registry_path) {
        Ok(_) => {
            let _ = kio_index::registry::RegistryDb::open_default()
                .map_err(index_to_kio)?
                .remove(&journal.enrollment.scope_id, &journal.old_kio_path)
                .map_err(index_to_kio)?;
        }
        Err(kio_index::registry::RegistrySnapshotError::Missing) => {}
        Err(error) => return Err(KioError::invalid_usage(error.to_string())),
    }
    store.quarantine_then_remove(Path::new(RETIREMENT), &bytes, MAX_JOURNAL)
}

/// Recover a planned initialization, or atomically publish a newly planned
/// direct child. It is called only after policy has admitted `basename`.
pub(crate) fn reconcile_or_initialize_child(
    parent: &Repository,
    basename: &str,
) -> Result<Repository> {
    let _lock = parent.lock_store()?;
    cancel::recover(parent)?;
    let parent_binding = binding_for_repo(parent)?;
    let store = control(&parent_binding)?;
    recover_lifecycle_atomic(&store)?;
    let pending = pending_initializations(&store)?;
    ensure_unique_initialization_basenames(&pending)?;
    let at_capacity = pending.len() >= MAX_INITIALIZATIONS;
    let matching: Vec<_> = pending
        .into_iter()
        .filter(|(j, _)| j.basename == basename)
        .collect();
    if let Some((journal, bytes)) = matching.into_iter().next() {
        return resume_initialization(parent, &parent_binding, &store, journal, bytes);
    }
    if at_capacity {
        return Err(KioError::invalid_usage(
            "too many child initialization journals",
        ));
    }
    let observed = observe_direct_child(&parent_binding, basename)?
        .ok_or_else(|| KioError::invalid_usage("planned child is absent"))?;
    validate_controlled_root(&observed.handle, &observed.canonical_root)?;
    let scope_id = new_ulid(&observed.canonical_root);
    let token = new_ulid(&observed.canonical_root);
    let management =
        planned_child_management_record(&parent_binding, &observed, &scope_id, &token)?;
    let operation_id = new_ulid(parent.canonical_root());
    let journal = Initialization {
        version: 1,
        stage_leaf: format!(".child-init-{operation_id}"),
        operation_id,
        basename: basename.to_owned(),
        retained_child_identity: observed.directory_identity,
        planned_scope_id: scope_id,
        enrollment_token: token,
        stage_identity: None,
        management,
    };
    let bytes = encode(&journal, "child initialization journal")?;
    write_initialization(&store, &journal, &bytes, Publication::CreateOnly)?;
    resume_initialization(parent, &parent_binding, &store, journal, bytes)
}

/// Complete only an already-durable initialization.  A caller uses this before
/// classifying an existing `.kio`, so publication after a crash is never
/// mistaken for an independently initialized root.
pub(crate) fn recover_pending_initializations(parent: &Repository) -> Result<Vec<Repository>> {
    let _lock = parent.lock_store()?;
    cancel::recover(parent)?;
    let parent_binding = binding_for_repo(parent)?;
    let store = control(&parent_binding)?;
    recover_lifecycle_atomic(&store)?;
    let pending = pending_initializations(&store)?;
    ensure_unique_initialization_basenames(&pending)?;
    let validated = pending
        .iter()
        .map(|(j, b)| validate_pending_before_policy(parent, &parent_binding, &store, j, b))
        .collect::<Result<Vec<_>>>()?;
    let mut repos = Vec::new();
    for ((journal, bytes), activated) in pending.into_iter().zip(validated) {
        if let Some(repo) = activated {
            remove_initialization_journal(&store, &journal, &bytes)?;
            repos.push(repo);
            continue;
        }
        if !policy_allows(&parent_binding, &journal.basename)? {
            continue;
        }
        repos.push(resume_initialization(
            parent,
            &parent_binding,
            &store,
            journal,
            bytes,
        )?);
    }
    Ok(repos)
}

fn validate_pending_before_policy(
    parent: &Repository,
    binding: &ManagementBinding,
    parent_control: &StoreDirectory,
    journal: &Initialization,
    bytes: &[u8],
) -> Result<Option<Repository>> {
    if journal.version != 1
        || journal.stage_leaf != format!(".child-init-{}", journal.operation_id)
        || journal.management.canonical_root != parent.canonical_root().join(&journal.basename)
        || journal.management.directory_identity != journal.retained_child_identity
    {
        return Err(KioError::invalid_usage(
            "child initialization journal is inconsistent",
        ));
    }
    let observed = observe_direct_child(binding, &journal.basename)?
        .ok_or_else(|| KioError::invalid_usage("pending child initialization is absent"))?;
    if observed.directory_identity != journal.retained_child_identity {
        return Err(KioError::invalid_usage(
            "pending child initialization identity changed",
        ));
    }
    validate_controlled_root(&observed.handle, &observed.canonical_root)?;
    let expected = planned_child_management_record(
        binding,
        &observed,
        &journal.planned_scope_id,
        &journal.enrollment_token,
    )?;
    if expected != journal.management {
        return Err(KioError::invalid_usage(
            "pending child initialization plan changed",
        ));
    }
    let child = StoreDirectory::from_retained(observed.handle, observed.canonical_root.clone())?;
    if child.contains_entry(Path::new(".kio"))? {
        let repo = super::open_existing_bound(&observed.canonical_root, &child)?;
        let child_binding = binding_for_repo(&repo)?;
        if Some(kio_core::management::directory_identity_from_handle(
            child_binding.kio_handle(),
        )?) != journal.stage_identity
        {
            return Err(KioError::invalid_usage(
                "pending published child store differs from journal",
            ));
        }
        let marker =
            control(&child_binding)?.read_optional(Path::new(STAGE_MARKER), MAX_JOURNAL)?;
        if marker.is_some() {
            if marker.as_deref() != Some(bytes)
                || read_registration_recovery_record(&child_binding)? != journal.management
            {
                return Err(KioError::invalid_usage(
                    "pending published child marker differs from journal",
                ));
            }
        } else {
            let parent_record = read_record(binding)?;
            let enrollment = ChildEnrollment {
                scope_id: journal.planned_scope_id.clone(),
                enrollment_token: journal.enrollment_token.clone(),
                directory_identity: journal.retained_child_identity.clone(),
            };
            if parent_record.children.get(&journal.basename) != Some(&enrollment)
                || !same_planned_child_record(&read_record(&child_binding)?, &journal.management)
            {
                return Err(KioError::invalid_usage(
                    "markerless pending child is not activated",
                ));
            }
            kio_core::management::validate_live_chain(&child_binding)?;
            return Ok(Some(repo));
        }
    } else if parent_control.contains_entry(Path::new(&journal.stage_leaf))? {
        let stage = StoreDirectory::from_retained(
            parent_control.open_directory(Path::new(&journal.stage_leaf))?,
            parent_control.path().join(&journal.stage_leaf),
        )?;
        kio_core::private_fs::verify_private_directory_handle(&stage)?;
        let identity =
            kio_core::management::directory_identity_from_handle(stage.root_handle().as_ref())?;
        if let Some(expected) = &journal.stage_identity {
            if expected != &identity {
                return Err(KioError::invalid_usage(
                    "pending child stage identity changed",
                ));
            }
            let management =
                kio_core::management::planned_management_record_bytes(&journal.management)?;
            kio_core::scope::validate_planned_kio_layout(
                &stage,
                &observed.canonical_root,
                &journal.planned_scope_id,
                &[
                    ("management.json", management.as_slice()),
                    (STAGE_MARKER, bytes),
                ],
            )?;
        } else if !stage
            .entries_optional(Path::new(""))?
            .unwrap_or_default()
            .is_empty()
        {
            return Err(KioError::invalid_usage(
                "un-pinned pending child stage is not empty",
            ));
        }
    } else if journal.stage_identity.is_some() {
        return Err(KioError::invalid_usage(
            "pinned child initialization stage is absent",
        ));
    }
    Ok(None)
}

fn remove_initialization_journal(
    control: &StoreDirectory,
    journal: &Initialization,
    bytes: &[u8],
) -> Result<()> {
    initialization_directory(control, false)?
        .ok_or_else(|| {
            KioError::invalid_usage("child initialization journal directory disappeared")
        })?
        .quarantine_then_remove(
            operation_leaf(&journal.operation_id)?.as_path(),
            bytes,
            MAX_JOURNAL,
        )
}

fn policy_allows(parent: &ManagementBinding, basename: &str) -> Result<bool> {
    let record = read_record(parent)?;
    let policy =
        kio_pipeline::policy::CurrentPolicyEvaluator::load(parent, record.case_insensitive)
            .map_err(crate::pipeline_to_kio)?;
    let allowed = policy
        .allows_directory(basename)
        .map_err(crate::pipeline_to_kio)?;
    policy.revalidate().map_err(crate::pipeline_to_kio)?;
    Ok(allowed)
}

pub(crate) fn recover_pending_initialization_for_child(
    parent: &Repository,
    basename: &str,
) -> Result<Option<Repository>> {
    let _lock = parent.lock_store()?;
    cancel::recover(parent)?;
    let binding = binding_for_repo(parent)?;
    let store = control(&binding)?;
    recover_lifecycle_atomic(&store)?;
    let pending = pending_initializations(&store)?;
    ensure_unique_initialization_basenames(&pending)?;
    let mut matching = pending.into_iter().filter(|(j, _)| j.basename == basename);
    let first = matching.next();
    first
        .map(|(j, b)| resume_initialization(parent, &binding, &store, j, b))
        .transpose()
}

fn resume_initialization(
    parent: &Repository,
    parent_binding: &ManagementBinding,
    parent_control: &StoreDirectory,
    mut journal: Initialization,
    mut bytes: Vec<u8>,
) -> Result<Repository> {
    if journal.version != 1
        || journal.operation_id.is_empty()
        || journal.stage_leaf != format!(".child-init-{}", journal.operation_id)
        || journal.management.scope_id != journal.planned_scope_id
        || journal.management.canonical_root != parent.canonical_root().join(&journal.basename)
        || journal.management.directory_identity != journal.retained_child_identity
    {
        return Err(KioError::invalid_usage(
            "child initialization journal is inconsistent",
        ));
    }
    let observed = observe_direct_child(parent_binding, &journal.basename)?.ok_or_else(|| {
        KioError::invalid_usage("planned child is absent during initialization recovery")
    })?;
    if observed.directory_identity != journal.retained_child_identity {
        return Err(KioError::invalid_usage(
            "planned child identity changed during initialization recovery",
        ));
    }
    validate_controlled_root(&observed.handle, &observed.canonical_root)?;
    let expected_management = planned_child_management_record(
        parent_binding,
        &observed,
        &journal.planned_scope_id,
        &journal.enrollment_token,
    )?;
    if expected_management != journal.management {
        return Err(KioError::invalid_usage(
            "child initialization management plan differs from current parent authority",
        ));
    }
    let parent_record = read_record(parent_binding)?;
    let policy = kio_pipeline::policy::CurrentPolicyEvaluator::load(
        parent_binding,
        parent_record.case_insensitive,
    )
    .map_err(crate::pipeline_to_kio)?;
    if !policy
        .allows_directory(&journal.basename)
        .map_err(crate::pipeline_to_kio)?
    {
        return Err(KioError::invalid_usage(
            "planned child is no longer allowed by parent policy",
        ));
    }
    policy.revalidate().map_err(crate::pipeline_to_kio)?;
    let child_root = StoreDirectory::from_retained(
        observed.handle.try_clone().map_err(|e| {
            KioError::io(e.to_string(), observed.canonical_root.display().to_string())
        })?,
        observed.canonical_root.clone(),
    )?;
    if child_root.contains_entry(Path::new(".kio"))? {
        let repo = super::open_existing_bound(&observed.canonical_root, &child_root)?;
        let child_binding = binding_for_repo(&repo)?;
        if child_binding.directory_identity() != &journal.retained_child_identity
            || Some(kio_core::management::directory_identity_from_handle(
                child_binding.kio_handle(),
            )?) != journal.stage_identity
        {
            return Err(KioError::invalid_usage(
                "published child store differs from initialization journal",
            ));
        }
        let child_control = control(&child_binding)?;
        if let Some(marker) = child_control.read_optional(Path::new(STAGE_MARKER), MAX_JOURNAL)? {
            if marker != bytes
                || read_registration_recovery_record(&child_binding)? != journal.management
            {
                return Err(KioError::invalid_usage(
                    "published child recovery state differs from journal",
                ));
            }
            revalidate_policy(parent_binding, &journal.basename)?;
            enroll_child(
                parent_binding,
                &journal.basename,
                &journal.planned_scope_id,
                &journal.enrollment_token,
                &journal.retained_child_identity,
            )?;
            child_control.quarantine_then_remove(Path::new(STAGE_MARKER), &bytes, MAX_JOURNAL)?;
        } else {
            let parent_record = read_record(parent_binding)?;
            let exact_enrollment = parent_record.children.get(&journal.basename)
                == Some(&ChildEnrollment {
                    scope_id: journal.planned_scope_id.clone(),
                    enrollment_token: journal.enrollment_token.clone(),
                    directory_identity: journal.retained_child_identity.clone(),
                });
            let actual = read_record(&child_binding)?;
            if !exact_enrollment || !same_planned_child_record(&actual, &journal.management) {
                return Err(KioError::invalid_usage(
                    "markerless published child is not an enrolled journal replay",
                ));
            }
            kio_core::management::validate_live_chain(&child_binding)?;
        }
        initialization_directory(parent_control, false)?
            .ok_or_else(|| {
                KioError::invalid_usage("child initialization journal directory disappeared")
            })?
            .quarantine_then_remove(
                operation_leaf(&journal.operation_id)?.as_path(),
                &bytes,
                MAX_JOURNAL,
            )?;
        return Ok(repo);
    }
    let stage = if parent_control.contains_entry(Path::new(&journal.stage_leaf))? {
        let stage = StoreDirectory::from_retained(
            parent_control.open_directory(Path::new(&journal.stage_leaf))?,
            parent
                .canonical_root()
                .join(".kio")
                .join(&journal.stage_leaf),
        )?;
        kio_core::private_fs::verify_private_directory_handle(&stage)?;
        let identity =
            kio_core::management::directory_identity_from_handle(stage.root_handle().as_ref())?;
        match &journal.stage_identity {
            Some(expected) if expected == &identity => {}
            Some(_) => {
                return Err(KioError::invalid_usage(
                    "child initialization stage identity changed",
                ));
            }
            None => {
                if !stage
                    .entries_optional(Path::new(""))?
                    .unwrap_or_default()
                    .is_empty()
                {
                    return Err(KioError::invalid_usage(
                        "un-pinned child initialization stage is not empty",
                    ));
                }
                journal.stage_identity = Some(identity);
                bytes = encode(&journal, "child initialization journal")?;
                write_initialization(parent_control, &journal, &bytes, Publication::Replace)?;
            }
        }
        stage
    } else {
        if journal.stage_identity.is_some() {
            return Err(KioError::invalid_usage(
                "pinned child initialization stage is absent",
            ));
        }
        let stage = StoreDirectory::from_retained(
            parent_control.create_directory(Path::new(&journal.stage_leaf))?,
            parent
                .canonical_root()
                .join(".kio")
                .join(&journal.stage_leaf),
        )?;
        kio_core::private_fs::verify_private_directory_handle(&stage)?;
        journal.stage_identity = Some(kio_core::management::directory_identity_from_handle(
            stage.root_handle().as_ref(),
        )?);
        bytes = encode(&journal, "child initialization journal")?;
        write_initialization(parent_control, &journal, &bytes, Publication::Replace)?;
        stage
    };
    let child_case =
        kio_core::management::detect_case_insensitive_in_directory(&child_root, &stage)?;
    if child_case != parent_record.case_insensitive {
        return Err(KioError::invalid_usage(
            "planned child filesystem case capability differs from its parent",
        ));
    }
    let after_case = observe_direct_child(parent_binding, &journal.basename)?.ok_or_else(|| {
        KioError::invalid_usage("planned child disappeared during case capability probe")
    })?;
    if after_case.directory_identity != journal.retained_child_identity {
        return Err(KioError::invalid_usage(
            "planned child identity changed during case capability probe",
        ));
    }
    validate_controlled_root(&after_case.handle, &after_case.canonical_root)?;
    let management_bytes =
        kio_core::management::planned_management_record_bytes(&journal.management)?;
    kio_core::scope::complete_planned_kio_layout(
        &stage,
        &observed.canonical_root,
        &journal.planned_scope_id,
        &[
            ("management.json", management_bytes.as_slice()),
            (STAGE_MARKER, bytes.as_slice()),
        ],
    )?;
    if stage.read_optional(Path::new(STAGE_MARKER), MAX_JOURNAL)? != Some(bytes.clone()) {
        return Err(KioError::invalid_usage(
            "child initialization stage marker differs from journal",
        ));
    }
    stage.sync()?;
    revalidate_policy(parent_binding, &journal.basename)?;
    let rechecked = observe_direct_child(parent_binding, &journal.basename)?
        .ok_or_else(|| KioError::invalid_usage("planned child disappeared before publication"))?;
    if rechecked.directory_identity != journal.retained_child_identity {
        return Err(KioError::invalid_usage(
            "planned child changed before publication",
        ));
    }
    validate_controlled_root(&rechecked.handle, &rechecked.canonical_root)?;
    parent_control.rename_directory_between_create_only(
        Path::new(&journal.stage_leaf),
        &child_root,
        Path::new(".kio"),
    )?;
    resume_initialization(parent, parent_binding, parent_control, journal, bytes)
}

fn revalidate_policy(parent: &ManagementBinding, basename: &str) -> Result<()> {
    let record = read_record(parent)?;
    let policy =
        kio_pipeline::policy::CurrentPolicyEvaluator::load(parent, record.case_insensitive)
            .map_err(crate::pipeline_to_kio)?;
    if !policy
        .allows_directory(basename)
        .map_err(crate::pipeline_to_kio)?
    {
        return Err(KioError::invalid_usage(
            "planned child is no longer allowed by parent policy",
        ));
    }
    policy.revalidate().map_err(crate::pipeline_to_kio)
}

fn same_planned_child_record(actual: &ManagementRecord, planned: &ManagementRecord) -> bool {
    actual.version == planned.version
        && actual.registration_generation == planned.registration_generation
        && actual.scope_id == planned.scope_id
        && actual.canonical_root == planned.canonical_root
        && actual.directory_identity == planned.directory_identity
        && actual.case_insensitive == planned.case_insensitive
        && actual.authority == planned.authority
}

#[cfg(test)]
mod tests {
    use super::super::{
        ChildScope, ExplicitRoot, binding_for_repo, initialize_explicit_root,
        reconcile_planned_child,
    };
    use super::{
        Initialization, MAX_JOURNAL, control, encode, initialization_directory, operation_leaf,
        pending_initializations, reconcile_or_initialize_child,
    };
    use kio_core::management::{
        directory_identity_from_handle, observe_direct_child, planned_child_management_record,
        read_record,
    };
    use kio_core::scope::new_ulid;
    use kio_core::store_dir::{Publication, StoreDirectory};
    use kio_pipeline::scan::BoundPlannedChild;
    use std::fs;

    fn canonical_tempdir() -> tempfile::TempDir {
        // Strict store ancestry checks require the resolved macOS temporary root.
        tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap()
    }

    fn bound(path: &std::path::Path) -> BoundPlannedChild {
        let canonical_root = path.canonicalize().unwrap();
        let root = kio_core::store_dir::StoreDirectory::open(&canonical_root)
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

    fn parent(temp: &tempfile::TempDir) -> kio_core::scope::Repository {
        match initialize_explicit_root(temp.path()).unwrap() {
            ExplicitRoot::Created(repo) => repo,
            ExplicitRoot::Existing(_) => panic!("test root unexpectedly exists"),
        }
    }

    fn test_journal(store: &StoreDirectory) -> (Initialization, Vec<u8>) {
        pending_initializations(store)
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
    }

    fn interrupted_stage(
        parent: &kio_core::scope::Repository,
        _child: &std::path::Path,
        pinned: bool,
        head_only: bool,
    ) {
        let binding = binding_for_repo(parent).unwrap();
        let observed = observe_direct_child(&binding, "child").unwrap().unwrap();
        let scope = new_ulid(&observed.canonical_root);
        let token = new_ulid(&observed.canonical_root);
        let management =
            planned_child_management_record(&binding, &observed, &scope, &token).unwrap();
        let operation = new_ulid(parent.canonical_root());
        let mut journal = Initialization {
            version: 1,
            operation_id: operation.clone(),
            basename: "child".into(),
            retained_child_identity: observed.directory_identity,
            planned_scope_id: scope,
            enrollment_token: token,
            stage_leaf: format!(".child-init-{operation}"),
            stage_identity: None,
            management,
        };
        let store = control(&binding).unwrap();
        let mut bytes = encode(&journal, "child initialization journal").unwrap();
        initialization_directory(&store, true)
            .unwrap()
            .unwrap()
            .write_atomic(
                operation_leaf(&journal.operation_id).unwrap().as_path(),
                &bytes,
                Publication::CreateOnly,
            )
            .unwrap();
        let stage = StoreDirectory::from_retained(
            store
                .create_directory(std::path::Path::new(&journal.stage_leaf))
                .unwrap(),
            parent
                .canonical_root()
                .join(".kio")
                .join(&journal.stage_leaf),
        )
        .unwrap();
        if pinned {
            journal.stage_identity =
                Some(directory_identity_from_handle(stage.root_handle().as_ref()).unwrap());
            bytes = encode(&journal, "child initialization journal").unwrap();
            initialization_directory(&store, false)
                .unwrap()
                .unwrap()
                .write_atomic(
                    operation_leaf(&journal.operation_id).unwrap().as_path(),
                    &bytes,
                    Publication::Replace,
                )
                .unwrap();
        }
        if head_only {
            stage
                .write_atomic(
                    std::path::Path::new("HEAD"),
                    b"unborn\n",
                    Publication::CreateOnly,
                )
                .unwrap();
        }
        assert!(bytes.len() as u64 <= MAX_JOURNAL);
    }

    fn publish_without_enrollment(parent: &kio_core::scope::Repository, child: &std::path::Path) {
        interrupted_stage(parent, child, true, false);
        let binding = binding_for_repo(parent).unwrap();
        let parent_store = control(&binding).unwrap();
        let (journal, bytes) = test_journal(&parent_store);
        let stage = StoreDirectory::from_retained(
            parent_store
                .open_directory(std::path::Path::new(&journal.stage_leaf))
                .unwrap(),
            parent
                .canonical_root()
                .join(".kio")
                .join(&journal.stage_leaf),
        )
        .unwrap();
        let management =
            kio_core::management::planned_management_record_bytes(&journal.management).unwrap();
        kio_core::scope::complete_planned_kio_layout(
            &stage,
            child,
            &journal.planned_scope_id,
            &[
                ("management.json", management.as_slice()),
                (super::STAGE_MARKER, bytes.as_slice()),
            ],
        )
        .unwrap();
        let child_root = StoreDirectory::open(child).unwrap();
        parent_store
            .rename_directory_between_create_only(
                std::path::Path::new(&journal.stage_leaf),
                &child_root,
                std::path::Path::new(".kio"),
            )
            .unwrap();
    }

    #[test]
    fn empty_unpinned_stage_is_adopted_and_completed() {
        let temp = canonical_tempdir();
        let parent = parent(&temp);
        let child = temp.path().join("child");
        fs::create_dir(&child).unwrap();
        interrupted_stage(&parent, &child, false, false);
        assert!(reconcile_or_initialize_child(&parent, "child").is_ok());
        assert!(child.join(".kio/management.json").is_file());
        assert!(
            !child.join(".kio-atomic").exists(),
            "the child-root case probe must keep its atomic workspace in the private stage"
        );
    }

    #[test]
    fn pinned_partial_stage_is_completed_without_replacement() {
        let temp = canonical_tempdir();
        let parent = parent(&temp);
        let child = temp.path().join("child");
        fs::create_dir(&child).unwrap();
        interrupted_stage(&parent, &child, true, true);
        assert!(reconcile_or_initialize_child(&parent, "child").is_ok());
        assert!(child.join(".kio/HEAD").is_file());
    }

    #[test]
    fn pinned_missing_stage_is_refused_without_new_allocation() {
        let temp = canonical_tempdir();
        let parent = parent(&temp);
        let child = temp.path().join("child");
        fs::create_dir(&child).unwrap();
        interrupted_stage(&parent, &child, true, false);
        let binding = binding_for_repo(&parent).unwrap();
        let store = control(&binding).unwrap();
        let (journal, _) = test_journal(&store);
        store
            .remove_directory_all(std::path::Path::new(&journal.stage_leaf))
            .unwrap();
        assert!(reconcile_or_initialize_child(&parent, "child").is_err());
        assert!(
            !store
                .contains_entry(std::path::Path::new(&journal.stage_leaf))
                .unwrap()
        );
        assert_eq!(test_journal(&store).0.operation_id, journal.operation_id);
    }

    #[test]
    fn ignored_private_pending_child_does_not_block_other_child_and_resumes_same_scope() {
        let temp = canonical_tempdir();
        let parent = parent(&temp);
        let child = temp.path().join("child");
        fs::create_dir(&child).unwrap();
        interrupted_stage(&parent, &child, true, false);
        let parent_binding = binding_for_repo(&parent).unwrap();
        let (journal, _) = test_journal(&control(&parent_binding).unwrap());
        fs::write(temp.path().join(".kioignore"), "child\n").unwrap();
        assert!(
            super::recover_pending_initializations(&parent)
                .unwrap()
                .is_empty()
        );
        assert!(!child.join(".kio").exists());
        let other = temp.path().join("other");
        fs::create_dir(&other).unwrap();
        assert!(matches!(
            reconcile_planned_child(&parent, bound(&other)).unwrap(),
            ChildScope::Managed { .. }
        ));
        fs::remove_file(temp.path().join(".kioignore")).unwrap();
        let recovered = super::recover_pending_initializations(&parent).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(
            recovered[0].scope_identity().unwrap().scope_id,
            journal.planned_scope_id
        );
    }

    #[test]
    fn ignored_published_pending_child_remains_inert_then_resumes() {
        let temp = canonical_tempdir();
        let parent = parent(&temp);
        let child = temp.path().join("child");
        fs::create_dir(&child).unwrap();
        publish_without_enrollment(&parent, &child);
        fs::write(temp.path().join(".kioignore"), "child\n").unwrap();
        assert!(
            super::recover_pending_initializations(&parent)
                .unwrap()
                .is_empty()
        );
        let repo =
            super::super::open_existing_bound(&child, &StoreDirectory::open(&child).unwrap())
                .unwrap();
        assert!(read_record(&binding_for_repo(&repo).unwrap()).is_err());
        fs::remove_file(temp.path().join(".kioignore")).unwrap();
        assert_eq!(
            super::recover_pending_initializations(&parent)
                .unwrap()
                .len(),
            1
        );
        assert!(!child.join(".kio/.child-init-journal.json").exists());
    }

    #[test]
    fn nonempty_unpinned_stage_is_refused_without_cleanup() {
        let temp = canonical_tempdir();
        let parent = parent(&temp);
        let child = temp.path().join("child");
        fs::create_dir(&child).unwrap();
        interrupted_stage(&parent, &child, false, true);
        assert!(reconcile_or_initialize_child(&parent, "child").is_err());
        let binding = binding_for_repo(&parent).unwrap();
        let (journal, _) = test_journal(&control(&binding).unwrap());
        assert!(
            temp.path()
                .join(".kio")
                .join(journal.stage_leaf)
                .join("HEAD")
                .is_file()
        );
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_child_refuses_initialization_without_creating_lifecycle_artifacts() {
        use std::os::unix::fs::PermissionsExt;

        let temp = canonical_tempdir();
        let parent = parent(&temp);
        let child = temp.path().join("child");
        fs::create_dir(&child).unwrap();
        fs::set_permissions(&child, fs::Permissions::from_mode(0o777)).unwrap();

        assert!(reconcile_or_initialize_child(&parent, "child").is_err());

        let store = control(&binding_for_repo(&parent).unwrap()).unwrap();
        assert!(pending_initializations(&store).unwrap().is_empty());
        assert!(
            store
                .entries_optional(std::path::Path::new(""))
                .unwrap()
                .unwrap()
                .iter()
                .all(|entry| !entry.name.to_string_lossy().starts_with(".child-init-"))
        );
        assert!(!child.join(".kio").exists());
        assert!(!child.join(".kio-case-probe").exists());
        assert!(!child.join(".kio-atomic").exists());
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_pending_child_refuses_resume_without_changing_journal_or_stage() {
        use std::os::unix::fs::PermissionsExt;

        let temp = canonical_tempdir();
        let parent = parent(&temp);
        let child = temp.path().join("child");
        fs::create_dir(&child).unwrap();
        interrupted_stage(&parent, &child, true, true);
        let store = control(&binding_for_repo(&parent).unwrap()).unwrap();
        let (journal, bytes) = test_journal(&store);
        fs::set_permissions(&child, fs::Permissions::from_mode(0o777)).unwrap();

        assert!(reconcile_or_initialize_child(&parent, "child").is_err());

        let (after, after_bytes) = test_journal(&store);
        assert_eq!(after.operation_id, journal.operation_id);
        assert_eq!(after_bytes, bytes);
        assert!(
            store
                .contains_entry(std::path::Path::new(&journal.stage_leaf))
                .unwrap()
        );
        assert!(!child.join(".kio").exists());
        assert!(!child.join(".kio-case-probe").exists());
        assert!(!child.join(".kio-atomic").exists());
    }

    #[test]
    fn published_marker_blocks_normal_read_until_enrollment_recovery() {
        let temp = canonical_tempdir();
        let parent = parent(&temp);
        let child = temp.path().join("child");
        fs::create_dir(&child).unwrap();
        publish_without_enrollment(&parent, &child);
        let child_store = StoreDirectory::open(&child).unwrap();
        let child_repo = super::super::open_existing_bound(&child, &child_store).unwrap();
        assert!(read_record(&binding_for_repo(&child_repo).unwrap()).is_err());
        assert!(reconcile_or_initialize_child(&parent, "child").is_ok());
        assert!(!child.join(".kio/.child-init-journal.json").exists());
        assert!(
            pending_initializations(&control(&binding_for_repo(&parent).unwrap()).unwrap())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn enrolled_markerless_publication_only_cleans_parent_journal() {
        let temp = canonical_tempdir();
        let parent = parent(&temp);
        let child = temp.path().join("child");
        fs::create_dir(&child).unwrap();
        publish_without_enrollment(&parent, &child);
        let parent_binding = binding_for_repo(&parent).unwrap();
        let (journal, bytes) = test_journal(&control(&parent_binding).unwrap());
        kio_core::management::enroll_child(
            &parent_binding,
            "child",
            &journal.planned_scope_id,
            &journal.enrollment_token,
            &journal.retained_child_identity,
        )
        .unwrap();
        let child_repo =
            super::super::open_existing_bound(&child, &StoreDirectory::open(&child).unwrap())
                .unwrap();
        control(&binding_for_repo(&child_repo).unwrap())
            .unwrap()
            .quarantine_then_remove(
                std::path::Path::new(super::STAGE_MARKER),
                &bytes,
                MAX_JOURNAL,
            )
            .unwrap();
        fs::write(temp.path().join(".kioignore"), "child\n").unwrap();
        assert_eq!(
            super::recover_pending_initializations(&parent)
                .unwrap()
                .len(),
            1
        );
        assert!(
            pending_initializations(&control(&parent_binding).unwrap())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn retirement_replay_after_authority_cutoff_cleans_journal() {
        let temp = canonical_tempdir();
        let parent = parent(&temp);
        let child = temp.path().join("child");
        fs::create_dir(&child).unwrap();
        let ChildScope::Managed { repo } = reconcile_planned_child(&parent, bound(&child)).unwrap()
        else {
            panic!("child was not managed")
        };
        let parent_binding = binding_for_repo(&parent).unwrap();
        let enrollment = read_record(&parent_binding).unwrap().children["child"].clone();
        fs::rename(&child, temp.path().join("retired-child")).unwrap();
        let journal = super::Retirement {
            version: 1,
            basename: "child".into(),
            enrollment: enrollment.clone(),
            old_kio_path: child.join(".kio").display().to_string(),
        };
        let bytes = encode(&journal, "child retirement journal").unwrap();
        control(&parent_binding)
            .unwrap()
            .write_atomic(
                std::path::Path::new(super::RETIREMENT),
                &bytes,
                Publication::CreateOnly,
            )
            .unwrap();
        assert!(
            kio_core::management::compare_and_remove_child_enrollment(
                &parent_binding,
                "child",
                &enrollment
            )
            .unwrap()
        );
        super::reconcile_stale_child_enrollments(&parent).unwrap();
        assert!(
            !control(&parent_binding)
                .unwrap()
                .contains_entry(std::path::Path::new(super::RETIREMENT))
                .unwrap()
        );
        assert!(
            !read_record(&parent_binding)
                .unwrap()
                .children
                .contains_key("child")
        );
        drop(repo);
    }

    #[test]
    fn same_identity_missing_metadata_keeps_enrollment_and_refuses_child_write() {
        let temp = canonical_tempdir();
        let parent = parent(&temp);
        let child = temp.path().join("child");
        fs::create_dir(&child).unwrap();
        let ChildScope::Managed { .. } = reconcile_planned_child(&parent, bound(&child)).unwrap()
        else {
            panic!("child was not managed");
        };
        let before = read_record(&binding_for_repo(&parent).unwrap())
            .unwrap()
            .children["child"]
            .clone();
        fs::remove_dir_all(child.join(".kio")).unwrap();
        assert!(reconcile_planned_child(&parent, bound(&child)).is_err());
        assert_eq!(
            read_record(&binding_for_repo(&parent).unwrap())
                .unwrap()
                .children["child"],
            before
        );
        assert!(!child.join(".kio").exists());
    }

    #[test]
    fn replacement_directory_retires_old_membership_then_mints_fresh_scope() {
        let temp = canonical_tempdir();
        let parent = parent(&temp);
        let child = temp.path().join("child");
        fs::create_dir(&child).unwrap();
        let ChildScope::Managed { repo } = reconcile_planned_child(&parent, bound(&child)).unwrap()
        else {
            panic!("child was not managed");
        };
        let old_scope = repo.scope_identity().unwrap().scope_id;
        fs::rename(&child, temp.path().join("old-child")).unwrap();
        fs::create_dir(&child).unwrap();
        let ChildScope::Managed { repo } = reconcile_planned_child(&parent, bound(&child)).unwrap()
        else {
            panic!("replacement child was not managed");
        };
        assert_ne!(repo.scope_identity().unwrap().scope_id, old_scope);
    }
}
