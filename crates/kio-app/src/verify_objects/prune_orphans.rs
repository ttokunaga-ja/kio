//! Confirmed orphan removal: prove the complete live set, then consume exact pins.
use super::*;
use kio_core::management::{DirectoryIdentity, directory_identity_from_handle};
use kio_core::scope::RetainedStoreLock;
use kio_core::store_dir::StoreDirectory;
use serde::Deserialize;

mod proof;
use proof::{Blocker, Proof};

#[derive(Debug, Default, Serialize)]
pub struct PruneOrphansReport {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_by: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<Blocker>,
    pub pruned_prepared_count: u64,
    pub pruned_image_count: u64,
    pub pruned_open_cache_count: u64,
    pub pruned_staging_root_count: u64,
}

impl PruneOrphansReport {
    fn blocked(blocker: Blocker) -> Self {
        Self {
            status: "blocked".into(),
            blocked_by: Some(blocker.kind.clone()),
            blockers: vec![blocker],
            ..Self::default()
        }
    }
}

struct BoundScope {
    session: GcSweepSession,
    directory: StoreDirectory,
    identity: DirectoryIdentity,
    scope_id: String,
    _lease: RetainedStoreLock,
}
impl BoundScope {
    fn bind(repo: &Repository) -> Result<Self> {
        let lease = repo.lock_store()?;
        let session = GcSweepSession::bind_repository(repo)?;
        let directory = StoreDirectory::from_retained(
            session.retained_kio_handle()?,
            repo.kio_dir().to_path_buf(),
        )?;
        let identity = directory_identity_from_handle(&directory.root_handle())?;
        let scope_id = repo.scope_identity()?.scope_id;
        Ok(Self {
            session,
            directory,
            identity,
            scope_id,
            _lease: lease,
        })
    }
    fn recheck(&self, repo: &Repository) -> Result<()> {
        self.session.assert_public_identity()?;
        let current = repo
            .bound_kio_handle()
            .ok_or_else(|| unsafe_prune("repository has no retained store"))?;
        if directory_identity_from_handle(current)? != self.identity
            || repo.scope_identity()?.scope_id != self.scope_id
        {
            return Err(unsafe_prune("approved prune scope changed"));
        }
        Ok(())
    }
}

/// Only this module can construct or change the approved target capabilities.
pub struct PruneOrphansPlan {
    blocked: Option<Blocker>,
    scope: Option<BoundScope>,
    #[cfg(test)]
    pub(super) prepared: Vec<String>,
    targets: Vec<ApprovedTarget>,
}
impl std::fmt::Debug for PruneOrphansPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PruneOrphansPlan")
            .field("blocked", &self.blocked)
            .field("targets", &self.target_lines())
            .finish()
    }
}
impl PruneOrphansPlan {
    #[cfg(test)]
    pub(super) fn blocked_for_test(kind: &str) -> Self {
        Self::blocked(Blocker {
            kind: kind.into(),
            target: "fixture".into(),
            next_action: "settle fixture".into(),
        })
    }
    pub fn is_blocked(&self) -> bool {
        self.blocked.is_some()
    }
    pub fn target_lines(&self) -> Vec<String> {
        self.targets
            .iter()
            .map(|target| target.label.clone())
            .collect()
    }
    fn blocked(blocker: Blocker) -> Self {
        Self {
            blocked: Some(blocker),
            scope: None,
            #[cfg(test)]
            prepared: vec![],
            targets: vec![],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum TargetKey {
    Prepared(String),
    Image(String),
    Staging(PathBuf),
    Cache(PathBuf),
}
struct ApprovedTarget {
    key: TargetKey,
    label: String,
    pin: RemovalPin,
}
// Exact mutation capsules are supplied by the core retained namespace boundary.
enum RemovalPin {
    Content(kio_core::cas::PlannedContentRemoval),
    Directory(kio_core::store_dir::PlannedDirectoryRemoval),
}
impl RemovalPin {
    fn revalidate(&self) -> Result<bool> {
        match self {
            Self::Content(pin) => pin.revalidate(),
            Self::Directory(pin) => pin.revalidate(),
        }
    }
    fn remove(self) -> Result<bool> {
        match self {
            Self::Content(pin) => pin.remove(),
            Self::Directory(pin) => pin.remove(),
        }
    }
}

fn unsafe_prune(message: impl Into<String>) -> KioError {
    KioError::new(
        "KIO-E-PRUNE-UNSAFE-001",
        message,
        json!({}),
        kio_core::ExitCode::PermanentFailure,
    )
}

/// Mandatory blockers are inspected before repair-capable verification begins.
pub fn prune_orphans_preflight(repo: &Repository) -> Result<Option<PruneOrphansReport>> {
    let scope = BoundScope::bind(repo)?;
    let result = proof::preflight(repo, &scope);
    scope.recheck(repo)?;
    Ok(result.err().map(PruneOrphansReport::blocked))
}

pub fn prune_orphans_plan(repo: &Repository) -> Result<PruneOrphansPlan> {
    let scope = BoundScope::bind(repo)?;
    let proof = match Proof::capture(repo, &scope) {
        Ok(proof) => proof,
        Err(blocker) => return Ok(PruneOrphansPlan::blocked(blocker)),
    };
    let mut budget = kio_core::store_dir::RemovalBudget::new();
    budget.reserve_pins(6)?;
    let store = ObjectStore::from_bound_kio(&scope.directory.root_handle())?;
    let mut targets = Vec::new();
    #[cfg(test)]
    let mut prepared = Vec::new();
    for candidate in &proof.candidates {
        let pin = (|| -> Result<Option<RemovalPin>> {
            Ok(match candidate {
                TargetKey::Prepared(hash) | TargetKey::Image(hash) => {
                    let kind = if matches!(candidate, TargetKey::Prepared(_)) {
                        ContentObjectKind::Prepared
                    } else {
                        ContentObjectKind::Image
                    };
                    store
                        .plan_content_removal(kind, hash, &mut budget)?
                        .map(RemovalPin::Content)
                }
                TargetKey::Staging(relative) => {
                    let parent_relative = relative
                        .parent()
                        .ok_or_else(|| unsafe_prune("staging root has no parent"))?;
                    let parent = StoreDirectory::from_retained(
                        scope
                            .directory
                            .open_maintenance_directory(parent_relative)?,
                        scope.directory.path().join(parent_relative),
                    )?;
                    scope.directory.require_same_filesystem(&parent)?;
                    let leaf = relative
                        .file_name()
                        .ok_or_else(|| unsafe_prune("staging root has no leaf"))?;
                    kio_core::store_dir::PlannedDirectoryRemoval::capture(
                        &parent,
                        Path::new(leaf),
                        &mut budget,
                    )?
                    .map(RemovalPin::Directory)
                }
                TargetKey::Cache(path) => {
                    let cache_root_path = crate::cache_home().join("kio/open");
                    let cache_root = StoreDirectory::open(&cache_root_path)?;
                    let parent_path = path
                        .parent()
                        .ok_or_else(|| unsafe_prune("cache root has no parent"))?;
                    let relative = parent_path.strip_prefix(&cache_root_path).map_err(|_| {
                        unsafe_prune("cache parent is outside the designated open cache")
                    })?;
                    let parent = if relative.as_os_str().is_empty() {
                        cache_root.clone()
                    } else {
                        StoreDirectory::from_retained(
                            cache_root.open_maintenance_directory(relative)?,
                            parent_path.to_path_buf(),
                        )?
                    };
                    cache_root.require_same_filesystem(&parent)?;
                    let leaf = path
                        .file_name()
                        .ok_or_else(|| unsafe_prune("cache root has no leaf"))?;
                    kio_core::store_dir::PlannedDirectoryRemoval::capture(
                        &parent,
                        Path::new(leaf),
                        &mut budget,
                    )?
                    .map(RemovalPin::Directory)
                }
            })
        })();
        let pin = match pin {
            Ok(pin) => pin,
            Err(error) => return Ok(PruneOrphansPlan::blocked(Blocker::unsafe_state(error))),
        };
        let Some(pin) = pin else {
            continue;
        };
        let label = match candidate {
            TargetKey::Prepared(hash) => {
                #[cfg(test)]
                prepared.push(hash.clone());
                format!("prepared  {hash}")
            }
            TargetKey::Image(hash) => {
                format!("image     {hash}")
            }
            TargetKey::Staging(path) => format!("staging   {}", path.display()),
            TargetKey::Cache(path) => format!("cache     {}", path.display()),
        };
        targets.push(ApprovedTarget {
            key: candidate.clone(),
            label,
            pin,
        });
    }
    scope.recheck(repo)?;
    Ok(PruneOrphansPlan {
        blocked: None,
        scope: Some(scope),
        #[cfg(test)]
        prepared,
        targets,
    })
}

/// Consume a confirmed plan. Recheck every blocker and every pin before deletion.
/// Newly orphaned entries are never added to the approved set.
pub fn prune_orphans_apply(
    repo: &Repository,
    plan: PruneOrphansPlan,
) -> Result<PruneOrphansReport> {
    if let Some(blocker) = plan.blocked {
        return Ok(PruneOrphansReport::blocked(blocker));
    }
    let scope = plan
        .scope
        .ok_or_else(|| unsafe_prune("prune plan has no retained scope"))?;
    scope.recheck(repo)?;
    let current = match Proof::capture(repo, &scope) {
        Ok(proof) => proof,
        Err(blocker) => return Ok(PruneOrphansReport::blocked(blocker)),
    };
    let mut eligible = Vec::new();
    for target in plan.targets {
        if current.candidates.contains(&target.key) && target.pin.revalidate()? {
            eligible.push(target);
        }
    }
    scope.recheck(repo)?;
    let mut report = PruneOrphansReport {
        status: "pruned".into(),
        ..PruneOrphansReport::default()
    };
    for target in eligible {
        if target.pin.remove()? {
            match target.key {
                TargetKey::Prepared(_) => report.pruned_prepared_count += 1,
                TargetKey::Image(_) => report.pruned_image_count += 1,
                TargetKey::Staging(_) => report.pruned_staging_root_count += 1,
                TargetKey::Cache(_) => report.pruned_open_cache_count += 1,
            }
        }
    }
    Ok(report)
}
