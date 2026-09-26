//! Application-owned initialization and child-management reconciliation.
//!
//! Core owns strict records and retained descriptor primitives.  This module
//! decides when an application command is allowed to create those records: an
//! explicit root or a just-created, directly planned child only.

mod child_lifecycle;
mod root_bootstrap;

use serde_json::Value;
use std::fs::File;
use std::path::Path;

use kio_core::management::{
    ManagementAuthority, ManagementBinding, read_record, validate_live_chain,
};
use kio_core::scope::Repository;
use kio_core::store_dir::StoreDirectory;
use kio_core::{KioError, Result};
use kio_pipeline::scan::BoundPlannedChild;

pub(crate) enum ExplicitRoot {
    Created(Repository),
    Existing(Repository),
}

pub(crate) enum ChildScope {
    Managed { repo: Box<Repository> },
    ExcludedIndependentRoot,
    ExcludedByPolicy,
}

/// Initialize an explicit root or resume only its identity-bound bootstrap.
/// Existing managed roots remain observable without starting a new bootstrap.
pub(crate) fn initialize_explicit_root(root: &Path) -> Result<ExplicitRoot> {
    root_bootstrap::initialize(root)
}

/// Open an existing root only after validating its management chain.
pub(crate) fn open_existing_managed_root(root: &Path) -> Result<Repository> {
    let canonical_root = root
        .canonicalize()
        .map_err(|error| KioError::io(error.to_string(), root.display().to_string()))?;
    let retained_root = StoreDirectory::open(&canonical_root)?;
    let repo = open_existing_bound(&canonical_root, &retained_root)?;
    validate_live_chain(&binding_for_repo(&repo)?)?;
    Ok(repo)
}

/// Reconcile parent-owned lifecycle journals once before indexing any of its
/// direct children.  The inner primitives take the same reentrant store lock,
/// so callers may hold their index writer lock around the whole parent pass.
pub(crate) fn reconcile_child_lifecycle(parent: &Repository) -> Result<()> {
    child_lifecycle::reconcile_stale_child_enrollments(parent)?;
    let _ = child_lifecycle::recover_pending_initializations(parent)?;
    Ok(())
}

pub(crate) fn cancel_child_initialization(
    parent: &Repository,
    preview: bool,
    operation: Option<&str>,
) -> Result<Value> {
    child_lifecycle::cancel_child_initialization(parent, preview, operation)
}

/// Reconcile a retained, direct child discovered by the parent. The parent
/// binding is deliberately supplied by the immediate caller, so grandchildren
/// are enrolled by their own direct parent rather than the top-level root.
pub(crate) fn reconcile_planned_child(
    parent: &Repository,
    child: BoundPlannedChild,
) -> Result<ChildScope> {
    let _parent_lock = parent.lock_store()?;
    let BoundPlannedChild {
        canonical_root,
        root,
        ..
    } = child;
    let child_name = child_basename(parent.canonical_root(), &canonical_root)?.to_owned();
    let parent_binding = binding_for_repo(parent)?;
    child_lifecycle::reconcile_stale_child_enrollment(parent, Some(&child_name))?;
    let parent_record = read_record(&parent_binding)?;
    validate_live_chain(&parent_binding)?;
    let policy = kio_pipeline::policy::CurrentPolicyEvaluator::load(
        &parent_binding,
        parent_record.case_insensitive,
    )
    .map_err(super::pipeline_to_kio)?;
    if !policy
        .allows_directory(&child_name)
        .map_err(super::pipeline_to_kio)?
    {
        return Ok(ChildScope::ExcludedByPolicy);
    }
    policy.revalidate().map_err(super::pipeline_to_kio)?;
    kio_core::management::validate_prospective_child(&parent_binding, &root, &canonical_root)?;

    if let Some(repo) =
        child_lifecycle::recover_pending_initialization_for_child(parent, &child_name)?
        && repo.canonical_root() == canonical_root
    {
        return Ok(ChildScope::Managed {
            repo: Box::new(repo),
        });
    }

    let child_directory = StoreDirectory::from_retained(
        root.try_clone().map_err(|error| {
            KioError::io(error.to_string(), canonical_root.display().to_string())
        })?,
        canonical_root.clone(),
    )?;
    if retained_kio(&child_directory)?.is_some() {
        let repo = open_existing_bound(&canonical_root, &child_directory)?;
        let child_binding = binding_for_repo(&repo)?;
        let child_record = read_record(&child_binding)?;
        validate_live_chain(&child_binding)?;
        return match child_record.authority {
            ManagementAuthority::Root => Ok(ChildScope::ExcludedIndependentRoot),
            ManagementAuthority::Child {
                parent_scope_id, ..
            } if parent_scope_id == parent_record.scope_id => Ok(ChildScope::Managed {
                repo: Box::new(repo),
            }),
            ManagementAuthority::Child { .. } => Err(KioError::invalid_usage(
                "planned child is not enrolled by its direct parent",
            )),
        };
    }

    if parent_record.children.contains_key(&child_name) {
        return Err(KioError::invalid_usage(
            "enrolled child is missing its .kio store; recovery is required before creation",
        ));
    }

    let repo = child_lifecycle::reconcile_or_initialize_child(parent, &child_name)?;
    validate_live_chain(&binding_for_repo(&repo)?)?;
    Ok(ChildScope::Managed {
        repo: Box::new(repo),
    })
}

fn retained_kio(root: &StoreDirectory) -> Result<Option<File>> {
    if !root.contains_entry(Path::new(".kio"))? {
        return Ok(None);
    }
    root.open_directory(Path::new(".kio")).map(Some)
}

fn open_existing_bound(canonical_root: &Path, root: &StoreDirectory) -> Result<Repository> {
    let kio = retained_kio(root)?.ok_or_else(|| {
        KioError::invalid_usage("existing managed scope is missing its .kio directory")
    })?;
    Repository::open_bound_existing(canonical_root.to_path_buf(), clone_root(root)?, kio)
}

pub(crate) fn binding_for_repo(repo: &Repository) -> Result<ManagementBinding> {
    let root = repo
        .bound_root_handle()
        .ok_or_else(|| {
            KioError::invalid_usage("managed operation requires a retained root handle")
        })?
        .try_clone()
        .map_err(|error| {
            KioError::io(
                error.to_string(),
                repo.canonical_root().display().to_string(),
            )
        })?;
    let kio = repo
        .bound_kio_handle()
        .ok_or_else(|| {
            KioError::invalid_usage("managed operation requires a retained .kio handle")
        })?
        .try_clone()
        .map_err(|error| KioError::io(error.to_string(), repo.kio_dir().display().to_string()))?;
    ManagementBinding::from_retained(root, kio, repo.canonical_root().to_path_buf())
}

fn clone_root(root: &StoreDirectory) -> Result<File> {
    root.root_handle()
        .as_ref()
        .try_clone()
        .map_err(|error| KioError::io(error.to_string(), root.path().display().to_string()))
}

fn child_basename<'a>(parent: &Path, child: &'a Path) -> Result<&'a str> {
    let relative = child.strip_prefix(parent).map_err(|_| {
        KioError::invalid_usage("planned child is not contained by its direct parent")
    })?;
    let mut parts = relative.components();
    let name = parts
        .next()
        .and_then(|part| part.as_os_str().to_str())
        .ok_or_else(|| KioError::invalid_usage("planned child name is invalid"))?;
    if parts.next().is_some() {
        return Err(KioError::invalid_usage(
            "planned child is not an immediate descendant of its parent",
        ));
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::{
        ChildScope, ExplicitRoot, binding_for_repo, initialize_explicit_root,
        reconcile_planned_child,
    };
    use crate::repository_scan_policy_allows_file;
    use kio_core::management::{revoke_child, validate_live_chain};
    use kio_core::scope::Repository;
    use kio_pipeline::scan::BoundPlannedChild;
    use std::fs;

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

    #[test]
    fn explicit_init_creates_once_and_existing_missing_management_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let repo = match initialize_explicit_root(root.path()).unwrap() {
            ExplicitRoot::Created(repo) => repo,
            ExplicitRoot::Existing(_) => panic!("new root was classified as existing"),
        };
        assert_eq!(repo.canonical_root(), root.path().canonicalize().unwrap());
        assert!(matches!(
            initialize_explicit_root(root.path()).unwrap(),
            ExplicitRoot::Existing(_)
        ));

        let unmanaged = tempfile::tempdir().unwrap();
        Repository::init(unmanaged.path()).unwrap();
        assert!(initialize_explicit_root(unmanaged.path()).is_err());
    }

    #[test]
    fn child_reconciliation_enrolls_direct_child_and_excludes_independent_root() {
        let root = tempfile::tempdir().unwrap();
        let parent = match initialize_explicit_root(root.path()).unwrap() {
            ExplicitRoot::Created(repo) => repo,
            ExplicitRoot::Existing(_) => panic!("new parent was classified as existing"),
        };

        let child_path = root.path().join("child");
        fs::create_dir(&child_path).unwrap();
        let child = reconcile_planned_child(&parent, bound(&child_path)).unwrap();
        let ChildScope::Managed { repo: child, .. } = child else {
            panic!("new direct child was not enrolled")
        };
        assert_eq!(child.canonical_root(), child_path.canonicalize().unwrap());

        let independent_path = root.path().join("independent");
        fs::create_dir(&independent_path).unwrap();
        assert!(matches!(
            initialize_explicit_root(&independent_path).unwrap(),
            ExplicitRoot::Created(_)
        ));
        assert!(matches!(
            reconcile_planned_child(&parent, bound(&independent_path)).unwrap(),
            ChildScope::ExcludedIndependentRoot
        ));
    }

    #[test]
    fn direct_parent_enrolls_grandchild_only_after_parent_is_managed() {
        let root = tempfile::tempdir().unwrap();
        let parent = match initialize_explicit_root(root.path()).unwrap() {
            ExplicitRoot::Created(repo) => repo,
            ExplicitRoot::Existing(_) => panic!("new parent was classified as existing"),
        };
        let child_path = root.path().join("child");
        fs::create_dir(&child_path).unwrap();
        let ChildScope::Managed { repo: child, .. } =
            reconcile_planned_child(&parent, bound(&child_path)).unwrap()
        else {
            panic!("child was not managed")
        };
        let grandchild_path = child_path.join("grandchild");
        fs::create_dir(&grandchild_path).unwrap();
        let ChildScope::Managed {
            repo: grandchild, ..
        } = reconcile_planned_child(&child, bound(&grandchild_path)).unwrap()
        else {
            panic!("grandchild was not managed by its immediate parent")
        };
        assert_eq!(
            validate_live_chain(&binding_for_repo(&grandchild).unwrap())
                .unwrap()
                .scopes
                .len(),
            3
        );
    }

    #[test]
    fn unmanaged_or_revoked_child_fails_closed_without_reinitialization() {
        let root = tempfile::tempdir().unwrap();
        let parent = match initialize_explicit_root(root.path()).unwrap() {
            ExplicitRoot::Created(repo) => repo,
            ExplicitRoot::Existing(_) => panic!("new parent was classified as existing"),
        };
        let unmanaged_path = root.path().join("unmanaged");
        fs::create_dir(&unmanaged_path).unwrap();
        Repository::init(&unmanaged_path).unwrap();
        assert!(reconcile_planned_child(&parent, bound(&unmanaged_path)).is_err());
        assert!(unmanaged_path.join(".kio").is_dir());

        let child_path = root.path().join("child");
        fs::create_dir(&child_path).unwrap();
        let ChildScope::Managed { repo: child, .. } =
            reconcile_planned_child(&parent, bound(&child_path)).unwrap()
        else {
            panic!("child was not managed")
        };
        let parent_binding = binding_for_repo(&parent).unwrap();
        assert!(revoke_child(&parent_binding, "child").unwrap());
        assert!(reconcile_planned_child(&parent, bound(&child_path)).is_err());
        assert!(validate_live_chain(&binding_for_repo(&child).unwrap()).is_err());
    }

    #[test]
    fn independent_root_exclusion_does_not_enroll_its_descendant() {
        let root = tempfile::tempdir().unwrap();
        let parent = match initialize_explicit_root(root.path()).unwrap() {
            ExplicitRoot::Created(repo) => repo,
            ExplicitRoot::Existing(_) => panic!("new parent was classified as existing"),
        };
        let independent_path = root.path().join("independent");
        fs::create_dir(&independent_path).unwrap();
        assert!(matches!(
            initialize_explicit_root(&independent_path).unwrap(),
            ExplicitRoot::Created(_)
        ));
        let descendant = independent_path.join("grandchild");
        fs::create_dir(&descendant).unwrap();
        assert!(matches!(
            reconcile_planned_child(&parent, bound(&independent_path)).unwrap(),
            ChildScope::ExcludedIndependentRoot
        ));
        assert!(!descendant.join(".kio").exists());
    }

    #[test]
    fn retained_child_binding_rejects_a_replaced_directory_identity() {
        let root = tempfile::tempdir().unwrap();
        let parent = match initialize_explicit_root(root.path()).unwrap() {
            ExplicitRoot::Created(repo) => repo,
            ExplicitRoot::Existing(_) => panic!("new parent was classified as existing"),
        };
        let child_path = root.path().join("child");
        fs::create_dir(&child_path).unwrap();
        let ChildScope::Managed { repo: child, .. } =
            reconcile_planned_child(&parent, bound(&child_path)).unwrap()
        else {
            panic!("child was not managed")
        };
        let moved = root.path().join("moved-child");
        fs::rename(&child_path, &moved).unwrap();
        fs::create_dir(&child_path).unwrap();
        assert!(binding_for_repo(&child).is_err());
    }

    #[test]
    fn removed_child_management_record_fails_without_reinitialization() {
        let root = tempfile::tempdir().unwrap();
        let parent = match initialize_explicit_root(root.path()).unwrap() {
            ExplicitRoot::Created(repo) => repo,
            ExplicitRoot::Existing(_) => panic!("new parent was classified as existing"),
        };
        let child_path = root.path().join("child");
        fs::create_dir(&child_path).unwrap();
        let ChildScope::Managed { .. } =
            reconcile_planned_child(&parent, bound(&child_path)).unwrap()
        else {
            panic!("child was not managed")
        };
        fs::remove_file(child_path.join(".kio/management.json")).unwrap();
        assert!(reconcile_planned_child(&parent, bound(&child_path)).is_err());
        assert!(!child_path.join(".kio/management.json").exists());
    }

    #[test]
    fn parent_ignore_change_denies_child_input_without_a_watcher_event() {
        let root = tempfile::tempdir().unwrap();
        let parent = match initialize_explicit_root(root.path()).unwrap() {
            ExplicitRoot::Created(repo) => repo,
            ExplicitRoot::Existing(_) => panic!("new parent was classified as existing"),
        };
        let child_path = root.path().join("child");
        fs::create_dir(&child_path).unwrap();
        let ChildScope::Managed { repo: child, .. } =
            reconcile_planned_child(&parent, bound(&child_path)).unwrap()
        else {
            panic!("child was not managed")
        };
        fs::write(child_path.join("blocked.txt"), b"content").unwrap();
        assert!(repository_scan_policy_allows_file(&child, "blocked.txt").unwrap());
        fs::write(root.path().join(".kioignore"), "child/blocked.txt\n").unwrap();
        assert!(!repository_scan_policy_allows_file(&child, "blocked.txt").unwrap());
    }
}
