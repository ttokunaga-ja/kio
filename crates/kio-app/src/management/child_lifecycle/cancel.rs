//! Explicit cancellation for an unpublished, parent-owned child stage.
//!
//! This deliberately has its own durable marker: normal lifecycle recovery
//! must never decide that an initialization should be cancelled.

use super::{
    Initialization, MAX_JOURNAL, STAGE_MARKER, control, decode, encode, initialization_directory,
    operation_leaf, pending_initializations, policy_allows, recover_lifecycle_atomic,
};
use crate::management::binding_for_repo;
use kio_core::management::{
    DirectoryIdentity, ManagementBinding, directory_identity_from_handle, observe_direct_child,
    planned_child_management_record, read_registration_recovery_record, validate_controlled_root,
    validate_live_chain,
};
use kio_core::scope::{Repository, validate_planned_kio_layout};
use kio_core::store_dir::{AtomicWorkspaceState, Publication, StoreDirectory};
use kio_core::{KioError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

const CANCELLATION: &str = "child-initialization-cancel.json";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Prepared,
    Quarantined,
    Removed,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cancellation {
    version: u8,
    initialization: Initialization,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stage_identity: Option<DirectoryIdentity>,
    quarantine_leaf: String,
    phase: Phase,
}

struct CheckedInitialization {
    stage: Option<StoreDirectory>,
    stage_identity: Option<DirectoryIdentity>,
}

pub(super) fn run(parent: &Repository, preview: bool, operation: Option<&str>) -> Result<Value> {
    if preview {
        let binding = binding_for_repo(parent)?;
        let store = control(&binding)?;
        let pending = pending_initializations(&store)?;
        let cancellation = load_marker(&store)?.map(|(marker, _)| {
            json!({
                "operation": marker.initialization.operation_id,
                "phase": marker.phase,
            })
        });
        if let Some(operation) = operation {
            operation_leaf(operation)?;
        }
        let rows = pending
            .into_iter()
            .filter(|(journal, _)| operation.is_none_or(|id| id == journal.operation_id))
            .map(|(journal, bytes)| preview_row(&binding, &store, journal, bytes))
            .collect::<Result<Vec<_>>>()?;
        return Ok(json!({
            "preview": true,
            "operations": rows,
            "pending_cancellation": cancellation,
        }));
    }
    let operation = operation.ok_or_else(|| {
        KioError::invalid_usage("child initialization cancellation requires an operation ID")
    })?;
    operation_leaf(operation)?;
    let _lock = parent.lock_store()?;
    let binding = binding_for_repo(parent)?;
    let store = control(&binding)?;
    recover_lifecycle_atomic(&store)?;
    validate_live_chain(&binding)?;
    if let Some((marker, bytes)) = load_marker(&store)? {
        if marker.initialization.operation_id != operation {
            return Err(KioError::invalid_usage(
                "a different child initialization cancellation is pending",
            ));
        }
        finish(&binding, &store, marker, bytes)?;
        return Ok(json!({"operation": operation, "cancelled": true, "resumed": true}));
    }
    let (journal, bytes) = pending_initializations(&store)?
        .into_iter()
        .find(|(journal, _)| journal.operation_id == operation)
        .ok_or_else(|| KioError::invalid_usage("child initialization operation is absent"))?;
    let quarantine_leaf = format!(".child-init-cancel-{operation}");
    if store.contains_entry(Path::new(&quarantine_leaf))? {
        return Err(KioError::invalid_usage(
            "child cancellation quarantine already exists",
        ));
    }
    let checked = check_initialization(&binding, &store, &journal, &bytes, false)?;
    let marker = Cancellation {
        version: 1,
        initialization: journal,
        stage_identity: checked.stage_identity,
        quarantine_leaf,
        phase: Phase::Prepared,
    };
    validate_cancellation(&marker, &bytes)?;
    let marker_bytes = encode(&marker, "child initialization cancellation marker")?;
    store.write_atomic(
        Path::new(CANCELLATION),
        &marker_bytes,
        Publication::CreateOnly,
    )?;
    // The marker is durable before a stage can be moved. Re-read it through
    // the retained control handle before beginning the state machine.
    let (marker, marker_bytes) = load_marker(&store)?.ok_or_else(|| {
        KioError::invalid_usage("child initialization cancellation marker disappeared")
    })?;
    finish(&binding, &store, marker, marker_bytes)?;
    Ok(json!({"operation": operation, "cancelled": true, "resumed": false}))
}

/// Resume only an explicit cancellation marker. It never starts cancellation
/// for an ordinary initialization journal.
pub(super) fn recover(parent: &Repository) -> Result<()> {
    let _lock = parent.lock_store()?;
    let binding = binding_for_repo(parent)?;
    let store = control(&binding)?;
    recover_lifecycle_atomic(&store)?;
    let Some((marker, bytes)) = load_marker(&store)? else {
        return Ok(());
    };
    finish(&binding, &store, marker, bytes)
}

fn preview_row(
    parent: &ManagementBinding,
    control: &StoreDirectory,
    journal: Initialization,
    bytes: Vec<u8>,
) -> Result<Value> {
    validate_live_chain(parent)?;
    let operation = journal.operation_id.clone();
    let path = parent.canonical_root().join(&journal.basename);
    let stage = parent
        .canonical_root()
        .join(".kio")
        .join(&journal.stage_leaf);
    let policy = policy_allows(parent, &journal.basename).ok();
    match check_initialization(parent, control, &journal, &bytes, false) {
        Ok(_) => Ok(json!({
            "operation": operation,
            "path": path,
            "stage": stage,
            "cancelable": true,
            "reason": null,
            "current_policy_allows": policy,
        })),
        Err(error) => Ok(json!({
            "operation": operation,
            "path": path,
            "stage": stage,
            "cancelable": false,
            "reason": error.to_string(),
            "current_policy_allows": policy,
        })),
    }
}

fn load_marker(control: &StoreDirectory) -> Result<Option<(Cancellation, Vec<u8>)>> {
    let Some(bytes) = control.read_optional(Path::new(CANCELLATION), MAX_JOURNAL)? else {
        return Ok(None);
    };
    let marker: Cancellation = decode(&bytes, "child initialization cancellation marker")?;
    let initial_bytes = encode(&marker.initialization, "child initialization journal")?;
    validate_cancellation(&marker, &initial_bytes)?;
    Ok(Some((marker, bytes)))
}

fn validate_cancellation(marker: &Cancellation, journal_bytes: &[u8]) -> Result<()> {
    let journal = &marker.initialization;
    if marker.version != 1
        || journal.version != 1
        || journal.stage_leaf != format!(".child-init-{}", journal.operation_id)
        || marker.quarantine_leaf != format!(".child-init-cancel-{}", journal.operation_id)
        || operation_leaf(&journal.operation_id)?.file_name().is_none()
    {
        return Err(KioError::invalid_usage(
            "child initialization cancellation marker is inconsistent",
        ));
    }
    if encode(journal, "child initialization journal")? != journal_bytes {
        return Err(KioError::invalid_usage(
            "child initialization cancellation journal bytes differ",
        ));
    }
    if let Some(identity) = &journal.stage_identity
        && marker.stage_identity.as_ref() != Some(identity)
    {
        return Err(KioError::invalid_usage(
            "cancellation marker differs from pinned initialization stage",
        ));
    }
    Ok(())
}

fn require_current_journal(
    control: &StoreDirectory,
    marker: &Cancellation,
    expected: &[u8],
) -> Result<()> {
    let directory = initialization_directory(control, false)?.ok_or_else(|| {
        KioError::invalid_usage("child initialization journal directory disappeared")
    })?;
    let leaf = operation_leaf(&marker.initialization.operation_id)?;
    if directory.read_optional(&leaf, MAX_JOURNAL)? != Some(expected.to_vec()) {
        return Err(KioError::invalid_usage(
            "child initialization journal differs from cancellation marker",
        ));
    }
    Ok(())
}

fn check_initialization(
    parent: &ManagementBinding,
    control: &StoreDirectory,
    journal: &Initialization,
    bytes: &[u8],
    allow_pinned_stage_absent: bool,
) -> Result<CheckedInitialization> {
    if journal.version != 1
        || journal.stage_leaf != format!(".child-init-{}", journal.operation_id)
        || journal.management.scope_id != journal.planned_scope_id
        || journal.management.canonical_root != parent.canonical_root().join(&journal.basename)
        || journal.management.directory_identity != journal.retained_child_identity
        || encode(journal, "child initialization journal")? != bytes
    {
        return Err(KioError::invalid_usage(
            "child initialization journal is inconsistent",
        ));
    }
    parent.revalidate()?;
    let observed = observe_direct_child(parent, &journal.basename)?
        .ok_or_else(|| KioError::invalid_usage("planned child is absent during cancellation"))?;
    if observed.directory_identity != journal.retained_child_identity {
        return Err(KioError::invalid_usage(
            "planned child identity changed during cancellation",
        ));
    }
    validate_controlled_root(&observed.handle, &observed.canonical_root)?;
    let expected = planned_child_management_record(
        parent,
        &observed,
        &journal.planned_scope_id,
        &journal.enrollment_token,
    )?;
    if expected != journal.management {
        return Err(KioError::invalid_usage(
            "child initialization management plan differs from current parent authority",
        ));
    }
    if read_registration_recovery_record(parent)?
        .children
        .contains_key(&journal.basename)
    {
        return Err(KioError::invalid_usage(
            "child initialization already has a parent enrollment",
        ));
    }
    let child = StoreDirectory::from_retained(
        observed.handle.try_clone().map_err(|error| {
            KioError::io(
                error.to_string(),
                observed.canonical_root.display().to_string(),
            )
        })?,
        observed.canonical_root,
    )?;
    if child.contains_entry(Path::new(".kio"))? {
        return Err(KioError::invalid_usage(
            "published child initialization is not cancelable",
        ));
    }
    stage_from_source(parent, control, journal, bytes, allow_pinned_stage_absent)
}

fn stage_from_source(
    parent: &ManagementBinding,
    control: &StoreDirectory,
    journal: &Initialization,
    bytes: &[u8],
    allow_pinned_stage_absent: bool,
) -> Result<CheckedInitialization> {
    if !control.contains_entry(Path::new(&journal.stage_leaf))? {
        if journal.stage_identity.is_some() {
            if allow_pinned_stage_absent {
                return Ok(CheckedInitialization {
                    stage: None,
                    stage_identity: journal.stage_identity.clone(),
                });
            }
            return Err(KioError::invalid_usage(
                "pinned child initialization stage is absent",
            ));
        }
        return Ok(CheckedInitialization {
            stage: None,
            stage_identity: None,
        });
    }
    let stage = StoreDirectory::from_retained(
        control.open_directory(Path::new(&journal.stage_leaf))?,
        parent
            .canonical_root()
            .join(".kio")
            .join(&journal.stage_leaf),
    )?;
    kio_core::private_fs::verify_private_directory_handle(&stage)?;
    let identity = directory_identity_from_handle(stage.root_handle().as_ref())?;
    if let Some(expected) = &journal.stage_identity {
        if expected != &identity {
            return Err(KioError::invalid_usage(
                "child initialization stage identity changed",
            ));
        }
        let management =
            kio_core::management::planned_management_record_bytes(&journal.management)?;
        validate_planned_kio_layout(
            &stage,
            parent.canonical_root().join(&journal.basename).as_path(),
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
            "un-pinned child initialization stage is not empty",
        ));
    }
    Ok(CheckedInitialization {
        stage: Some(stage),
        stage_identity: Some(identity),
    })
}

fn open_quarantine(
    parent: &ManagementBinding,
    control: &StoreDirectory,
    marker: &Cancellation,
    journal_bytes: &[u8],
) -> Result<Option<StoreDirectory>> {
    if !control.contains_entry(Path::new(&marker.quarantine_leaf))? {
        return Ok(None);
    }
    let stage = StoreDirectory::from_retained(
        control.open_directory(Path::new(&marker.quarantine_leaf))?,
        parent
            .canonical_root()
            .join(".kio")
            .join(&marker.quarantine_leaf),
    )?;
    kio_core::private_fs::verify_private_directory_handle(&stage)?;
    let identity = directory_identity_from_handle(stage.root_handle().as_ref())?;
    if marker.stage_identity.as_ref() != Some(&identity) {
        return Err(KioError::invalid_usage("cancelled stage identity changed"));
    }
    if stage.inspect_atomic()? == AtomicWorkspaceState::Pending {
        return Err(KioError::invalid_usage(
            "cancelled stage has an unrecovered atomic workspace",
        ));
    }
    let management =
        kio_core::management::planned_management_record_bytes(&marker.initialization.management)?;
    validate_planned_kio_layout(
        &stage,
        parent
            .canonical_root()
            .join(&marker.initialization.basename)
            .as_path(),
        &marker.initialization.planned_scope_id,
        &[
            ("management.json", management.as_slice()),
            (STAGE_MARKER, journal_bytes),
        ],
    )?;
    Ok(Some(stage))
}

fn finish(
    parent: &ManagementBinding,
    control: &StoreDirectory,
    mut marker: Cancellation,
    mut marker_bytes: Vec<u8>,
) -> Result<()> {
    let journal_bytes = encode(&marker.initialization, "child initialization journal")?;
    validate_cancellation(&marker, &journal_bytes)?;
    validate_live_chain(parent)?;
    if marker.phase == Phase::Prepared {
        require_current_journal(control, &marker, &journal_bytes)?;
        let source_present =
            control.contains_entry(Path::new(&marker.initialization.stage_leaf))?;
        let quarantine_present = control.contains_entry(Path::new(&marker.quarantine_leaf))?;
        if source_present && quarantine_present {
            return Err(KioError::invalid_usage(
                "source and cancellation quarantine are both present",
            ));
        }
        let checked = check_initialization(
            parent,
            control,
            &marker.initialization,
            &journal_bytes,
            quarantine_present,
        )?;
        if source_present {
            let stage = checked.stage.ok_or_else(|| {
                KioError::invalid_usage("initialization source stage disappeared")
            })?;
            if marker.stage_identity != checked.stage_identity
                || marker.stage_identity.as_ref()
                    != Some(&directory_identity_from_handle(
                        stage.root_handle().as_ref(),
                    )?)
            {
                return Err(KioError::invalid_usage(
                    "stage identity changed before cancellation",
                ));
            }
            let observed = observe_direct_child(parent, &marker.initialization.basename)?
                .ok_or_else(|| {
                    KioError::invalid_usage(
                        "planned child disappeared before cancellation probe cleanup",
                    )
                })?;
            if observed.directory_identity != marker.initialization.retained_child_identity {
                return Err(KioError::invalid_usage(
                    "planned child identity changed before cancellation probe cleanup",
                ));
            }
            validate_controlled_root(&observed.handle, &observed.canonical_root)?;
            let child = StoreDirectory::from_retained(
                observed.handle.try_clone().map_err(|error| {
                    KioError::io(
                        error.to_string(),
                        observed.canonical_root.display().to_string(),
                    )
                })?,
                observed.canonical_root,
            )?;
            let _ = kio_core::management::cleanup_case_probe_in_directory(&child, &stage)?;
            control.rename_directory_create_only(
                Path::new(&marker.initialization.stage_leaf),
                Path::new(&marker.quarantine_leaf),
            )?;
        } else if quarantine_present {
            open_quarantine(parent, control, &marker, &journal_bytes)?
                .ok_or_else(|| KioError::invalid_usage("cancel quarantine disappeared"))?;
        } else if marker.stage_identity.is_some() {
            return Err(KioError::invalid_usage(
                "pinned child initialization stage is absent during cancellation",
            ));
        }
        marker.phase = Phase::Quarantined;
        marker_bytes = encode(&marker, "child initialization cancellation marker")?;
        control.write_atomic(Path::new(CANCELLATION), &marker_bytes, Publication::Replace)?;
    }
    if marker.phase == Phase::Quarantined {
        require_current_journal(control, &marker, &journal_bytes)?;
        check_initialization(
            parent,
            control,
            &marker.initialization,
            &journal_bytes,
            true,
        )?;
        if control.contains_entry(Path::new(&marker.initialization.stage_leaf))? {
            return Err(KioError::invalid_usage(
                "initialization source reappeared during cancellation",
            ));
        }
        if let Some(stage) = open_quarantine(parent, control, &marker, &journal_bytes)? {
            let identity = directory_identity_from_handle(stage.root_handle().as_ref())?;
            if marker.stage_identity.as_ref() != Some(&identity) {
                return Err(KioError::invalid_usage("cancelled stage identity changed"));
            }
            control.remove_directory_all(Path::new(&marker.quarantine_leaf))?;
        }
        marker.phase = Phase::Removed;
        marker_bytes = encode(&marker, "child initialization cancellation marker")?;
        control.write_atomic(Path::new(CANCELLATION), &marker_bytes, Publication::Replace)?;
    }
    if marker.phase != Phase::Removed {
        return Err(KioError::invalid_usage(
            "child cancellation phase is invalid",
        ));
    }
    if control.contains_entry(Path::new(&marker.initialization.stage_leaf))?
        || control.contains_entry(Path::new(&marker.quarantine_leaf))?
    {
        return Err(KioError::invalid_usage("cancelled stage remains present"));
    }
    if let Some(directory) = initialization_directory(control, false)? {
        let leaf = operation_leaf(&marker.initialization.operation_id)?;
        match directory.read_optional(&leaf, MAX_JOURNAL)? {
            Some(found) if found == journal_bytes => {
                directory.quarantine_then_remove(&leaf, &journal_bytes, MAX_JOURNAL)?;
            }
            Some(_) => {
                return Err(KioError::invalid_usage(
                    "child initialization journal differs from cancellation marker",
                ));
            }
            None => {}
        }
    }
    control.quarantine_then_remove(Path::new(CANCELLATION), &marker_bytes, MAX_JOURNAL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::management::{ExplicitRoot, binding_for_repo, initialize_explicit_root};
    use kio_core::management::{
        CASE_PROBE_BYTES, CASE_PROBE_LEAF, directory_identity_from_handle, observe_direct_child,
        planned_child_management_record,
    };
    use kio_core::scope::new_ulid;
    use kio_core::store_dir::Publication;
    use std::fs;

    fn parent(temp: &tempfile::TempDir) -> Repository {
        match initialize_explicit_root(temp.path()).unwrap() {
            ExplicitRoot::Created(repo) => repo,
            ExplicitRoot::Existing(_) => panic!("test parent unexpectedly existed"),
        }
    }

    fn interrupted(parent: &Repository, basename: &str, pinned: bool, partial: bool) -> String {
        fs::create_dir(parent.canonical_root().join(basename)).unwrap();
        fs::write(
            parent.canonical_root().join(basename).join("user.txt"),
            b"keep",
        )
        .unwrap();
        let binding = binding_for_repo(parent).unwrap();
        let observed = observe_direct_child(&binding, basename).unwrap().unwrap();
        let scope = new_ulid(&observed.canonical_root);
        let token = new_ulid(&observed.canonical_root);
        let management =
            planned_child_management_record(&binding, &observed, &scope, &token).unwrap();
        let operation = new_ulid(parent.canonical_root());
        let mut journal = Initialization {
            version: 1,
            operation_id: operation.clone(),
            basename: basename.into(),
            retained_child_identity: observed.directory_identity,
            planned_scope_id: scope,
            enrollment_token: token,
            stage_leaf: format!(".child-init-{operation}"),
            stage_identity: None,
            management,
        };
        let store = control(&binding).unwrap();
        let stage = StoreDirectory::from_retained(
            store
                .create_directory(Path::new(&journal.stage_leaf))
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
        }
        let bytes = encode(&journal, "child initialization journal").unwrap();
        initialization_directory(&store, true)
            .unwrap()
            .unwrap()
            .write_atomic(
                &operation_leaf(&operation).unwrap(),
                &bytes,
                Publication::CreateOnly,
            )
            .unwrap();
        if partial {
            stage
                .write_atomic(Path::new("HEAD"), b"unborn\n", Publication::CreateOnly)
                .unwrap();
        }
        operation
    }

    #[test]
    fn preview_is_read_only_and_empty_or_partial_stages_cancel_without_touching_child_files() {
        let temp = tempfile::tempdir().unwrap();
        let parent = parent(&temp);
        let empty = interrupted(&parent, "empty", false, false);
        let preview = run(&parent, true, None).unwrap();
        assert_eq!(preview["preview"], true);
        assert!(!parent.kio_dir().join(CANCELLATION).exists());
        assert!(
            parent
                .kio_dir()
                .join(format!(".child-init-{empty}"))
                .exists()
        );
        let wrong = new_ulid(parent.canonical_root());
        assert!(run(&parent, false, Some(&wrong)).is_err());
        assert!(
            parent
                .kio_dir()
                .join(format!(".child-init-{empty}"))
                .exists()
        );

        run(&parent, false, Some(&empty)).unwrap();
        assert_eq!(
            fs::read(parent.canonical_root().join("empty/user.txt")).unwrap(),
            b"keep"
        );
        assert!(
            !parent
                .kio_dir()
                .join(format!(".child-init-{empty}"))
                .exists()
        );

        let partial = interrupted(&parent, "partial", true, true);
        run(&parent, false, Some(&partial)).unwrap();
        assert_eq!(
            fs::read(parent.canonical_root().join("partial/user.txt")).unwrap(),
            b"keep"
        );
        assert!(
            !parent
                .kio_dir()
                .join(format!(".child-init-{partial}"))
                .exists()
        );

        let published = interrupted(&parent, "published", true, false);
        fs::create_dir(parent.canonical_root().join("published/.kio")).unwrap();
        assert!(run(&parent, false, Some(&published)).is_err());
        assert!(
            parent
                .kio_dir()
                .join(format!(".child-init-{published}"))
                .exists()
        );
    }

    #[test]
    fn cancellation_cleans_a_verified_child_case_probe_before_removing_its_stage() {
        let temp = tempfile::tempdir().unwrap();
        let parent = parent(&temp);
        let operation = interrupted(&parent, "child", true, true);
        let binding = binding_for_repo(&parent).unwrap();
        let store = control(&binding).unwrap();
        let (journal, _) = pending_initializations(&store)
            .unwrap()
            .into_iter()
            .find(|(journal, _)| journal.operation_id == operation)
            .unwrap();
        let stage = StoreDirectory::from_retained(
            store
                .open_directory(Path::new(&journal.stage_leaf))
                .unwrap(),
            parent.kio_dir().join(&journal.stage_leaf),
        )
        .unwrap();
        let child_path = parent.canonical_root().join("child");
        let child = StoreDirectory::open(&child_path).unwrap();
        child
            .write_atomic_with_owner(
                &stage,
                Path::new(CASE_PROBE_LEAF),
                CASE_PROBE_BYTES,
                Publication::CreateOnly,
            )
            .unwrap();
        assert!(child_path.join(CASE_PROBE_LEAF).is_file());
        assert!(!child_path.join(".kio-atomic").exists());

        run(&parent, false, Some(&operation)).unwrap();

        assert!(!child_path.join(CASE_PROBE_LEAF).exists());
        assert!(!child_path.join(".kio-atomic").exists());
        assert_eq!(fs::read(child_path.join("user.txt")).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_pending_child_refuses_cancellation_without_mutating_lifecycle_artifacts() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let parent = parent(&temp);
        let operation = interrupted(&parent, "child", true, true);
        let binding = binding_for_repo(&parent).unwrap();
        let store = control(&binding).unwrap();
        let (journal, bytes) = pending_initializations(&store)
            .unwrap()
            .into_iter()
            .find(|(journal, _)| journal.operation_id == operation)
            .unwrap();
        let child_path = parent.canonical_root().join("child");
        fs::set_permissions(&child_path, fs::Permissions::from_mode(0o777)).unwrap();

        assert!(run(&parent, false, Some(&operation)).is_err());

        assert!(!store.contains_entry(Path::new(CANCELLATION)).unwrap());
        assert!(
            store
                .contains_entry(Path::new(&journal.stage_leaf))
                .unwrap()
        );
        assert_eq!(
            initialization_directory(&store, false)
                .unwrap()
                .unwrap()
                .read_optional(&operation_leaf(&operation).unwrap(), MAX_JOURNAL)
                .unwrap(),
            Some(bytes)
        );
        assert!(!child_path.join(".kio").exists());
        assert!(!child_path.join(CASE_PROBE_LEAF).exists());
        assert!(!child_path.join(".kio-atomic").exists());
    }

    #[test]
    fn recovery_finishes_a_rename_that_preceded_the_quarantined_phase_write() {
        let temp = tempfile::tempdir().unwrap();
        let parent = parent(&temp);
        let operation = interrupted(&parent, "crash", true, true);
        let binding = binding_for_repo(&parent).unwrap();
        let store = control(&binding).unwrap();
        let (journal, bytes) = pending_initializations(&store)
            .unwrap()
            .into_iter()
            .find(|(journal, _)| journal.operation_id == operation)
            .unwrap();
        let marker = Cancellation {
            version: 1,
            initialization: journal.clone(),
            stage_identity: journal.stage_identity.clone(),
            quarantine_leaf: format!(".child-init-cancel-{operation}"),
            phase: Phase::Prepared,
        };
        let marker_bytes = encode(&marker, "child initialization cancellation marker").unwrap();
        store
            .write_atomic(
                Path::new(CANCELLATION),
                &marker_bytes,
                Publication::CreateOnly,
            )
            .unwrap();
        store
            .rename_directory_create_only(
                Path::new(&journal.stage_leaf),
                Path::new(&marker.quarantine_leaf),
            )
            .unwrap();

        recover(&parent).unwrap();
        assert!(!parent.kio_dir().join(CANCELLATION).exists());
        assert!(
            !parent
                .kio_dir()
                .join(format!(".child-init-cancel-{operation}"))
                .exists()
        );
        let directory = initialization_directory(&store, false).unwrap().unwrap();
        assert_eq!(
            directory
                .read_optional(&operation_leaf(&operation).unwrap(), MAX_JOURNAL)
                .unwrap(),
            None
        );
        assert_eq!(
            fs::read(parent.canonical_root().join("crash/user.txt")).unwrap(),
            b"keep"
        );
        assert!(!bytes.is_empty());
    }

    #[test]
    fn quarantined_unpinned_stage_recovers_after_partial_or_completed_deletion() {
        let temp = tempfile::tempdir().unwrap();
        let parent = parent(&temp);
        let operation = interrupted(&parent, "partial-delete", false, false);
        let binding = binding_for_repo(&parent).unwrap();
        let store = control(&binding).unwrap();
        let (journal, bytes) = pending_initializations(&store)
            .unwrap()
            .into_iter()
            .find(|(journal, _)| journal.operation_id == operation)
            .unwrap();
        let stage = StoreDirectory::from_retained(
            store
                .open_directory(Path::new(&journal.stage_leaf))
                .unwrap(),
            parent
                .canonical_root()
                .join(".kio")
                .join(&journal.stage_leaf),
        )
        .unwrap();
        let stage_identity = directory_identity_from_handle(stage.root_handle().as_ref()).unwrap();
        stage
            .write_atomic(Path::new("HEAD"), b"unborn\n", Publication::CreateOnly)
            .unwrap();
        let marker = Cancellation {
            version: 1,
            initialization: journal.clone(),
            stage_identity: Some(stage_identity),
            quarantine_leaf: format!(".child-init-cancel-{operation}"),
            phase: Phase::Quarantined,
        };
        let marker_bytes = encode(&marker, "child initialization cancellation marker").unwrap();
        store
            .write_atomic(
                Path::new(CANCELLATION),
                &marker_bytes,
                Publication::CreateOnly,
            )
            .unwrap();
        store
            .rename_directory_create_only(
                Path::new(&journal.stage_leaf),
                Path::new(&marker.quarantine_leaf),
            )
            .unwrap();
        let quarantine = StoreDirectory::from_retained(
            store
                .open_directory(Path::new(&marker.quarantine_leaf))
                .unwrap(),
            parent.kio_dir().join(&marker.quarantine_leaf),
        )
        .unwrap();
        quarantine.remove_file(Path::new("HEAD")).unwrap();

        recover(&parent).unwrap();
        assert!(!parent.kio_dir().join(CANCELLATION).exists());
        assert!(!parent.kio_dir().join(&marker.quarantine_leaf).exists());
        assert_eq!(
            fs::read(parent.canonical_root().join("partial-delete/user.txt")).unwrap(),
            b"keep"
        );
        assert!(!bytes.is_empty());
    }

    #[test]
    fn removed_phase_allows_an_already_deleted_initialization_journal() {
        let temp = tempfile::tempdir().unwrap();
        let parent = parent(&temp);
        let operation = interrupted(&parent, "terminal", true, false);
        let binding = binding_for_repo(&parent).unwrap();
        let store = control(&binding).unwrap();
        let (journal, bytes) = pending_initializations(&store)
            .unwrap()
            .into_iter()
            .find(|(journal, _)| journal.operation_id == operation)
            .unwrap();
        let marker = Cancellation {
            version: 1,
            initialization: journal.clone(),
            stage_identity: journal.stage_identity.clone(),
            quarantine_leaf: format!(".child-init-cancel-{operation}"),
            phase: Phase::Removed,
        };
        let marker_bytes = encode(&marker, "child initialization cancellation marker").unwrap();
        store
            .write_atomic(
                Path::new(CANCELLATION),
                &marker_bytes,
                Publication::CreateOnly,
            )
            .unwrap();
        store
            .remove_directory_all(Path::new(&journal.stage_leaf))
            .unwrap();
        let directory = initialization_directory(&store, false).unwrap().unwrap();
        directory
            .quarantine_then_remove(&operation_leaf(&operation).unwrap(), &bytes, MAX_JOURNAL)
            .unwrap();

        recover(&parent).unwrap();
        assert!(!parent.kio_dir().join(CANCELLATION).exists());
        assert_eq!(
            fs::read(parent.canonical_root().join("terminal/user.txt")).unwrap(),
            b"keep"
        );
    }

    #[test]
    fn replaced_child_refuses_without_creating_a_cancellation_marker() {
        let temp = tempfile::tempdir().unwrap();
        let parent = parent(&temp);
        let operation = interrupted(&parent, "replace", true, false);
        let original = parent.canonical_root().join("original");
        fs::rename(parent.canonical_root().join("replace"), &original).unwrap();
        fs::create_dir(parent.canonical_root().join("replace")).unwrap();

        assert!(run(&parent, false, Some(&operation)).is_err());
        assert!(!parent.kio_dir().join(CANCELLATION).exists());
        assert!(
            parent
                .kio_dir()
                .join(format!(".child-init-{operation}"))
                .exists()
        );
        assert_eq!(fs::read(original.join("user.txt")).unwrap(), b"keep");
    }
}
