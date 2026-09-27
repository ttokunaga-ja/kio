//! Read-only blocker, staging-residue, and complete immutable live-set proof.
use super::*;
use kio_core::store_dir::{ATOMIC_WORKSPACE_DIR, AtomicWorkspaceState};
use kio_pipeline::markdownize::{NormalizedUnitObject, UnitStatus};
use kio_pipeline::task::{
    TaskDescriptor, TaskOutputRef, TaskStatus, TaskStore, task_failure_is_terminal,
    validate_task_output_ref,
};
use std::ffi::OsString;

const MAX_SCAN_ENTRIES: usize = 1_000_000;
const MAX_SCAN_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_MANIFEST: u64 = 8 * 1024 * 1024;
const MAX_UNIT: u64 = 64 * 1024 * 1024;
const DIRECTORY_QUARANTINE: &str = ".kio-prune-directory-";
const CAS_QUARANTINE: &str = ".kio-cas-remove-";

type Checked<T> = std::result::Result<T, Blocker>;
#[derive(Debug, Clone, Serialize)]
pub struct Blocker {
    pub kind: String,
    pub target: String,
    pub next_action: String,
}
impl Blocker {
    fn new(kind: &str, target: impl Into<String>, action: &str) -> Self {
        Self {
            kind: kind.into(),
            target: target.into(),
            next_action: action.into(),
        }
    }
    pub(super) fn unsafe_state(error: impl std::fmt::Display) -> Self {
        let detail = error.to_string();
        if detail.contains("planned removal exceeds") {
            return Self::new(
                "prune_limit",
                detail,
                "The maintenance plan exceeds the retained-handle, depth, or byte limit. Inspect the listed residue set; automatic pruning made no changes",
            );
        }
        Self::new(
            "unsafe_prune_authority",
            detail,
            "Run kio repair verify-objects and resolve the reported unsafe or corrupt state before pruning",
        )
    }
}
fn checked<T>(value: Result<T>) -> Checked<T> {
    value.map_err(Blocker::unsafe_state)
}

// This budget charges direct reads and successfully validated logical bodies.
// Shared loaders and shallow-receipt verification retain their own bounds;
// failed loader attempts and repeated verification passes are not a global
// physical-I/O ceiling.
#[derive(Default)]
struct Budget {
    entries: usize,
    bytes: u64,
}
impl Budget {
    fn entry(&mut self) -> Checked<()> {
        self.entries = self.entries.saturating_add(1);
        if self.entries > MAX_SCAN_ENTRIES {
            return Err(Blocker::new(
                "prune_limit",
                "directory entry budget",
                "Reduce the maintenance scope before retrying; no entries were deleted",
            ));
        }
        Ok(())
    }
    fn bytes(&mut self, n: u64) -> Checked<()> {
        self.bytes = self.bytes.saturating_add(n);
        if self.bytes > MAX_SCAN_BYTES {
            return Err(Blocker::new(
                "prune_limit",
                "verification byte budget",
                "Reduce the maintenance scope before retrying; no entries were deleted",
            ));
        }
        Ok(())
    }
}
fn clone_directory(directory: &StoreDirectory) -> Result<StoreDirectory> {
    StoreDirectory::from_retained(
        directory
            .root_handle()
            .try_clone()
            .map_err(|e| KioError::io(e.to_string(), directory.path().display().to_string()))?,
        directory.path().to_path_buf(),
    )
}
fn child(directory: &StoreDirectory, leaf: &Path) -> Result<StoreDirectory> {
    StoreDirectory::from_retained(
        directory.open_maintenance_directory(leaf)?,
        directory.path().join(leaf),
    )
}
fn optional_child(directory: &StoreDirectory, relative: &Path) -> Result<Option<StoreDirectory>> {
    let mut current = directory.clone();
    for component in relative.components() {
        let std::path::Component::Normal(leaf) = component else {
            return Err(unsafe_prune(
                "maintenance directory must be a relative normal path",
            ));
        };
        if !current.contains_entry(Path::new(leaf))? {
            return Ok(None);
        }
        current = child(&current, Path::new(leaf))?;
    }
    Ok(Some(current))
}

fn children(directory: &StoreDirectory, budget: &mut Budget) -> Checked<Vec<OsString>> {
    let handle = directory.root_handle();
    let iterator =
        cap_primitives::fs::read_dir(&handle, Path::new(".")).map_err(Blocker::unsafe_state)?;
    let mut result = Vec::new();
    for entry in iterator {
        budget.entry()?;
        let entry = entry.map_err(Blocker::unsafe_state)?;
        result.push(entry.file_name());
    }
    result.sort();
    Ok(result)
}
fn text_leaf(name: &OsString) -> Checked<&str> {
    name.to_str()
        .ok_or_else(|| Blocker::unsafe_state("non-UTF8 maintenance namespace"))
}
fn read(
    directory: &StoreDirectory,
    path: &Path,
    max: u64,
    budget: &mut Budget,
) -> Checked<Vec<u8>> {
    let bytes = checked(directory.read_optional(path, max))?.ok_or_else(|| {
        Blocker::unsafe_state(format!("missing {}", directory.path().join(path).display()))
    })?;
    budget.bytes(bytes.len() as u64)?;
    Ok(bytes)
}
fn hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn hex2(value: &str) -> bool {
    value.len() == 2
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn logical_directory_leaf(leaf: &str) -> Checked<&str> {
    let logical = leaf.strip_prefix(DIRECTORY_QUARANTINE).unwrap_or(leaf);
    if logical.is_empty() || logical.starts_with(DIRECTORY_QUARANTINE) {
        return Err(Blocker::unsafe_state("malformed directory quarantine name"));
    }
    Ok(logical)
}
fn private_stage(leaf: &str) -> bool {
    leaf.strip_prefix(".staged-").is_some_and(|id| {
        id.len() == 26
            && id.as_bytes()[0] <= b'7'
            && id
                .bytes()
                .all(|b| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&b))
    })
}

#[derive(Default)]
pub(super) struct Preflight {
    tasks: Vec<TaskDescriptor>,
    instances: BTreeMap<(String, String, u64), NormalizedInstanceManifest>,
    private_stages: BTreeSet<PathBuf>,
    protected_prepared: BTreeSet<String>,
    protected_images: BTreeSet<String>,
    preserve_all_content: bool,
    budget: Budget,
}

pub(super) fn preflight(repo: &Repository, scope: &BoundScope) -> Checked<Preflight> {
    checked(scope.recheck(repo))?;
    let purge = PurgeState::from_directory(checked(clone_directory(&scope.directory))?);
    if let Some(journal) = checked(purge.read_journal())? {
        return Err(Blocker::new(
            "active_purge_journal",
            journal.purge_id,
            "Resume the active purge journal before pruning",
        ));
    }
    if checked(
        scope
            .directory
            .contains_entry(Path::new("publication-v1.json")),
    )? || checked(scope.directory.inspect_atomic())? == AtomicWorkspaceState::Pending
    {
        return Err(Blocker::new(
            "unfinalized_manifest",
            "store publication journal",
            "Run kio repair verify-objects to recover the interrupted publication before pruning",
        ));
    }
    let ledger = checked(crate::open_optional_ledger_db())?;
    if let Some(ledger) = ledger {
        let rows = kio_pipeline::ledger::ops::inflight_requests_for_scope(&ledger, &scope.scope_id)
            .map_err(Blocker::unsafe_state)?;
        if let Some(row) = rows.first() {
            let key = &row.key;
            let target = row.intent_token.clone().unwrap_or_else(|| {
                format!(
                    "{}/{}/{}/{}",
                    key.scope_id, key.adapter_kind, key.input_hash, key.tool_profile_hash
                )
            });
            return Err(Blocker::new(
                "inflight_request",
                target,
                "Run kio batch resume or kio batch abandon for this request before pruning",
            ));
        }
    }
    let task_store = TaskStore::new(scope.directory.path());
    let tasks = task_store
        .all_bound(&scope.directory)
        .map_err(Blocker::unsafe_state)?;
    for task in &tasks {
        if matches!(
            task.status,
            TaskStatus::Pending | TaskStatus::Running | TaskStatus::Paused
        ) || (task.status == TaskStatus::Failed && !task_failure_is_terminal(task))
        {
            return Err(Blocker::new(
                "non_terminal_task",
                task.task_id.clone(),
                "Run kio batch resume or kio batch abandon, and finish or settle this task before pruning",
            ));
        }
    }
    let store = checked(ObjectStore::from_bound_kio(&scope.directory.root_handle()))?;
    let mut result = Preflight {
        tasks,
        ..Preflight::default()
    };
    let mut budget = Budget::default();
    inspect_projections(&scope.directory, &store, &mut result, &mut budget)?;
    // The current projection must represent the checkpoint selected by HEAD.
    if let Some(head) = checked(repo.head_commit_hash())? {
        let commit = checked(repo.read_commit(&head))?;
        let tree = checked(repo.read_tree(&commit.tree))?;
        for entry in &tree.entries {
            let Some(reference) = &entry.normalize else {
                continue;
            };
            if let Some(manifest) = result.instances.get(&(
                entry.raw_hash.clone(),
                reference.tool_profile_hash.clone(),
                reference.r#gen,
            )) {
                let bytes = checked(canonical_json_bytes(
                    &serde_json::to_value(manifest).map_err(Blocker::unsafe_state)?,
                ))?;
                if hash_bytes(&bytes) != reference.manifest_hash {
                    return Err(Blocker::new(
                        "unfinalized_manifest",
                        reference.manifest_hash.clone(),
                        "Run kio repair verify-objects, then reindex the current normalized projection before pruning",
                    ));
                }
            }
        }
    }
    for task in &result.tasks {
        if task.status == TaskStatus::Partial {
            let reference = validate_task_output_ref(task_store.kio_dir(), task)
                .map_err(Blocker::unsafe_state)?;
            let TaskOutputRef::NormalizedInstance {
                raw_hash,
                tool_profile_hash,
                r#gen,
                ..
            } = reference
            else {
                return Err(Blocker::new(
                    "non_terminal_task",
                    task.task_id.clone(),
                    "Repair the partial task's normalized output binding before pruning",
                ));
            };
            let manifest = result
                .instances
                .get(&(raw_hash, tool_profile_hash, r#gen))
                .ok_or_else(|| {
                    Blocker::new(
                        "unfinalized_manifest",
                        task.output_ref.clone(),
                        "Resume or repair the partial normalized instance before pruning",
                    )
                })?;
            if !crate::partial_retry_plan_from_manifest(manifest, task.attempts)
                .retryable_units
                .is_empty()
            {
                return Err(Blocker::new(
                    "non_terminal_task",
                    task.task_id.clone(),
                    "Run kio batch resume or kio batch abandon to settle retryable partial units",
                ));
            }
        }
    }
    checked(scope.recheck(repo))?;
    result.budget = budget;
    Ok(result)
}

fn protect_manifest(
    manifest: &NormalizedInstanceManifest,
    units: &[NormalizedUnitObject],
    state: &mut Preflight,
    budget: &mut Budget,
) -> Checked<()> {
    state
        .protected_prepared
        .extend(manifest.units.iter().map(|unit| unit.prepared_hash.clone()));
    for unit in units {
        let bytes = checked(canonical_json_bytes(
            &serde_json::to_value(unit).map_err(Blocker::unsafe_state)?,
        ))?;
        // The immutable loader inspects and then reads each canonical body.
        budget.bytes((bytes.len() as u64).saturating_mul(2))?;
        state
            .protected_images
            .extend(unit.owned_image_hashes.iter().cloned());
    }
    Ok(())
}

fn inspect_projections(
    kio: &StoreDirectory,
    store: &ObjectStore,
    state: &mut Preflight,
    budget: &mut Budget,
) -> Checked<()> {
    let Some(base) = checked(optional_child(kio, Path::new("objects/normalized_units")))? else {
        return Ok(());
    };
    for first in children(&base, budget)? {
        let a = text_leaf(&first)?;
        if !hex2(a) {
            return Err(Blocker::unsafe_state("invalid normalized first fanout"));
        }
        let first_dir = checked(child(&base, Path::new(&first)))?;
        for second in children(&first_dir, budget)? {
            let b = text_leaf(&second)?;
            if !hex2(b) {
                return Err(Blocker::unsafe_state("invalid normalized second fanout"));
            }
            let parent = checked(child(&first_dir, Path::new(&second)))?;
            for name in children(&parent, budget)? {
                let actual = text_leaf(&name)?;
                let leaf = logical_directory_leaf(actual)?;
                if actual != leaf && checked(parent.contains_entry(Path::new(leaf)))? {
                    return Err(Blocker::unsafe_state(
                        "normalized stage and quarantine coexist",
                    ));
                }
                let directory = checked(child(&parent, Path::new(actual)))?;
                let relative = Path::new("objects/normalized_units")
                    .join(a)
                    .join(b)
                    .join(leaf);
                if private_stage(leaf) {
                    // Private publication stages cannot authorize deletion of CAS.
                    // Preserve any valid manifest closure during this invocation.
                    if let Some(bytes) =
                        checked(directory.read_optional(Path::new("manifest.json"), MAX_MANIFEST))?
                    {
                        budget.bytes(bytes.len() as u64)?;
                        if let Ok(manifest) =
                            serde_json::from_slice::<NormalizedInstanceManifest>(&bytes)
                        {
                            match load_validated_normalized_units_from_manifest(store, &manifest) {
                                Ok(units) => protect_manifest(&manifest, &units, state, budget)?,
                                // The whole private stage can be retired, but incomplete
                                // references cannot authorize any content deletion this time.
                                Err(_) => state.preserve_all_content = true,
                            }
                        }
                    }
                    state.private_stages.insert(relative);
                    continue;
                }
                if actual != leaf {
                    return Err(Blocker::unsafe_state("unrecognized normalized quarantine"));
                }
                let tuple=parse_instance_leaf(leaf).ok_or_else(||Blocker::new("unfinalized_manifest",relative.display().to_string(),"Repair or reindex this normalized instance; its canonical publication is incomplete"))?;
                if &tuple.0[7..9] != a || &tuple.0[9..11] != b {
                    return Err(Blocker::unsafe_state(
                        "normalized instance is in the wrong fanout",
                    ));
                }
                let result = inspect_instance(&directory, &tuple, store, budget);
                let (manifest,units)=result.map_err(|detail|Blocker::new("unfinalized_manifest",format!("{}: {}",relative.display(),detail.target),"Run kio repair verify-objects, then reindex or resume the corresponding normalized task to finalize its projection"))?;
                protect_manifest(&manifest, &units, state, budget)?;
                if state.instances.insert(tuple, manifest).is_some() {
                    return Err(Blocker::unsafe_state("duplicate normalized instance"));
                }
            }
        }
    }
    Ok(())
}
fn parse_instance_leaf(leaf: &str) -> Option<(String, String, u64)> {
    let parts: Vec<_> = leaf.split('.').collect();
    if parts.len() != 3 || !hex64(parts[0]) || !hex64(parts[1]) {
        return None;
    }
    let value = parts[2].strip_prefix('g')?;
    let generation = value.parse::<u64>().ok()?;
    if generation.to_string() != value {
        return None;
    }
    Some((
        format!("sha256:{}", parts[0]),
        format!("sha256:{}", parts[1]),
        generation,
    ))
}
fn inspect_instance(
    directory: &StoreDirectory,
    tuple: &(String, String, u64),
    store: &ObjectStore,
    budget: &mut Budget,
) -> Checked<(NormalizedInstanceManifest, Vec<NormalizedUnitObject>)> {
    if checked(directory.inspect_atomic())? == AtomicWorkspaceState::Pending {
        return Err(Blocker::unsafe_state("pending instance atomic publication"));
    }
    let bytes = read(directory, Path::new("manifest.json"), MAX_MANIFEST, budget)?;
    let manifest: NormalizedInstanceManifest =
        serde_json::from_slice(&bytes).map_err(Blocker::unsafe_state)?;
    if (
        &manifest.raw_hash,
        &manifest.tool_profile_hash,
        manifest.r#gen,
    ) != (&tuple.0, &tuple.1, tuple.2)
    {
        return Err(Blocker::unsafe_state("projection identity mismatch"));
    }
    let canonical = checked(canonical_json_bytes(
        &serde_json::to_value(&manifest).map_err(Blocker::unsafe_state)?,
    ))?;
    let hash = hash_bytes(&canonical);
    let immutable =
        checked(store.read_content_object_bytes(ContentObjectKind::Manifest, &hash, MAX_MANIFEST))?;
    budget.bytes(immutable.len() as u64)?;
    if immutable != canonical {
        return Err(Blocker::unsafe_state(
            "projection does not match immutable manifest",
        ));
    }
    let units = load_validated_normalized_units_from_manifest(store, &manifest)
        .map_err(Blocker::unsafe_state)?;
    let mut expected = BTreeSet::from(["manifest.json".to_owned()]);
    for (entry, unit) in manifest
        .units
        .iter()
        .filter(|entry| entry.status == UnitStatus::Done)
        .zip(&units)
    {
        let leaf = format!("{}.json", entry.unit_ref);
        expected.insert(leaf.clone());
        let bytes = read(directory, Path::new(&leaf), MAX_UNIT, budget)?;
        let projected: NormalizedUnitObject =
            serde_json::from_slice(&bytes).map_err(Blocker::unsafe_state)?;
        if projected != *unit {
            return Err(Blocker::unsafe_state(
                "projection unit differs from pinned immutable unit",
            ));
        }
    }
    for name in children(directory, budget)? {
        let leaf = text_leaf(&name)?;
        if leaf == ATOMIC_WORKSPACE_DIR {
            continue;
        }
        if !expected.remove(leaf) {
            return Err(Blocker::unsafe_state(
                "unexpected normalized projection entry",
            ));
        }
    }
    if !expected.is_empty() {
        return Err(Blocker::unsafe_state("incomplete normalized projection"));
    }
    Ok((manifest, units))
}

pub(super) struct Proof {
    pub candidates: BTreeSet<TargetKey>,
}
impl Proof {
    pub fn capture(repo: &Repository, scope: &BoundScope) -> Checked<Self> {
        let mut progress = preflight(repo, scope)?;
        let store = checked(ObjectStore::from_bound_kio(&scope.directory.root_handle()))?;
        let purge = PurgeState::from_directory(checked(clone_directory(&scope.directory))?);
        let now = now_utc_seconds();
        let mut budget = std::mem::take(&mut progress.budget);
        let roots = checked(repo.current_ref_targets())?;
        let head = checked(repo.head_commit_hash())?;
        let shallow = checked(scope.session.validated_final_shallow_receipts())?;
        let mut commits = BTreeMap::<String, CommitObject>::new();
        let mut trees = BTreeMap::<String, TreeObject>::new();
        let mut reachable = BTreeSet::new();
        let mut queue: VecDeque<_> = roots.into_iter().collect();
        while let Some(hash) = queue.pop_front() {
            if !reachable.insert(hash.clone()) {
                continue;
            }
            budget.entry()?;
            let commit = checked(repo.read_commit(&hash))?;
            budget.bytes(
                checked(canonical_json_bytes(
                    &serde_json::to_value(&commit).map_err(Blocker::unsafe_state)?,
                ))?
                .len() as u64,
            )?;
            queue.extend(commit.parent.iter().cloned());
            match repo.read_tree(&commit.tree) {
                Ok(tree) => {
                    budget.bytes(
                        checked(canonical_json_bytes(
                            &serde_json::to_value(&tree).map_err(Blocker::unsafe_state)?,
                        ))?
                        .len() as u64,
                    )?;
                    if shallow.contains_key(&hash) {
                        return Err(Blocker::unsafe_state("shallow receipt coexists with tree"));
                    }
                    trees.insert(commit.tree.clone(), tree);
                }
                Err(error)
                    if is_store_not_found(&error) && shallow.get(&hash) == Some(&commit.tree) => {}
                Err(error) => return Err(Blocker::unsafe_state(error)),
            }
            commits.insert(hash, commit);
        }
        let explained_authority = explained_historical_manifests(
            &purge,
            &commits,
            &trees,
            &reachable,
            head.as_deref(),
            &now,
        );
        let mut verifier = State::default();
        let mut explained_missing_manifests = BTreeSet::new();
        for commit in commits.values() {
            let Some(tree) = trees.get(&commit.tree) else {
                continue;
            };
            for entry in &tree.entries {
                let Some(reference) = &entry.normalize else {
                    continue;
                };
                let lifecycle =
                    canonical_lookup(&purge, &entry.raw_hash, &commits, &trees, &reachable, &now);
                if lifecycle.tombstone_error.is_some() || lifecycle.receipt_error.is_some() {
                    return Err(Blocker::unsafe_state("invalid raw lifecycle authority"));
                }
                let retired = matches!(
                    lifecycle.canonical.as_ref().map(|value| value.event.kind),
                    Some(EventKind::Purged | EventKind::Erased)
                );
                if retired {
                    continue;
                }
                match load_pinned_normalized_instance(
                    &store,
                    repo.kio_dir(),
                    &entry.raw_hash,
                    reference,
                    &mut verifier,
                ) {
                    Ok(instance) => protect_manifest(
                        &instance.manifest,
                        &instance.units,
                        &mut progress,
                        &mut budget,
                    )?,
                    Err(PinnedNormalizedError::Missing)
                        if explained_authority
                            .get(&reference.manifest_hash)
                            .is_some_and(|(raw, exact)| {
                                raw == &entry.raw_hash && exact == reference
                            }) =>
                    {
                        explained_missing_manifests.insert(reference.manifest_hash.clone());
                    }
                    Err(error) => {
                        return Err(Blocker::new(
                            "live_manifest_unverified",
                            format!("{}: {error}", reference.manifest_hash),
                            "Run kio repair verify-objects and restore or repair the immutable manifest closure before pruning",
                        ));
                    }
                }
            }
        }
        // Every retained immutable manifest is a potential owner, including
        // shallow historical closures and pre-projection crash publications.
        for hash in content_inventory(&scope.directory, ContentObjectKind::Manifest, &mut budget)? {
            let bytes = checked(store.read_content_object_bytes(
                ContentObjectKind::Manifest,
                &hash,
                MAX_MANIFEST,
            ))?;
            budget.bytes(bytes.len() as u64)?;
            let manifest: NormalizedInstanceManifest =
                serde_json::from_slice(&bytes).map_err(Blocker::unsafe_state)?;
            if hash_bytes(&bytes) != hash
                || checked(canonical_json_bytes(
                    &serde_json::to_value(&manifest).map_err(Blocker::unsafe_state)?,
                ))? != bytes
            {
                return Err(Blocker::unsafe_state("noncanonical immutable manifest"));
            }
            let lifecycle = canonical_lookup(
                &purge,
                &manifest.raw_hash,
                &commits,
                &trees,
                &reachable,
                &now,
            );
            if lifecycle.tombstone_error.is_some() || lifecycle.receipt_error.is_some() {
                return Err(Blocker::unsafe_state(
                    "invalid manifest lifecycle authority",
                ));
            }
            if matches!(
                lifecycle.canonical.as_ref().map(|value| value.event.kind),
                Some(EventKind::Purged | EventKind::Erased)
            ) {
                continue;
            }
            let units = if explained_missing_manifests.contains(&hash) {
                load_surviving_normalized_units(&store, &manifest, &mut verifier)
                    .map_err(Blocker::unsafe_state)?
                    .0
            } else {
                load_validated_normalized_units_from_manifest(&store, &manifest)
                    .map_err(Blocker::unsafe_state)?
            };
            protect_manifest(&manifest, &units, &mut progress, &mut budget)?;
        }
        let mut candidates = BTreeSet::new();
        for hash in content_inventory(&scope.directory, ContentObjectKind::Prepared, &mut budget)? {
            if !progress.preserve_all_content && !progress.protected_prepared.contains(&hash) {
                candidates.insert(TargetKey::Prepared(hash));
            }
        }
        for hash in content_inventory(&scope.directory, ContentObjectKind::Image, &mut budget)? {
            if !progress.preserve_all_content && !progress.protected_images.contains(&hash) {
                candidates.insert(TargetKey::Image(hash));
            }
        }
        candidates.extend(
            progress
                .private_stages
                .iter()
                .cloned()
                .map(TargetKey::Staging),
        );
        classify_staging(scope, &mut candidates, &mut budget)?;
        cache_candidates(
            scope,
            &purge,
            &commits,
            &trees,
            &reachable,
            &now,
            &progress.protected_images,
            &mut candidates,
            &mut budget,
        )?;
        checked(scope.recheck(repo))?;
        Ok(Self { candidates })
    }
}

fn content_inventory(
    kio: &StoreDirectory,
    kind: ContentObjectKind,
    budget: &mut Budget,
) -> Checked<BTreeSet<String>> {
    let mut hashes = BTreeSet::new();
    let Some(base) = checked(optional_child(
        kio,
        &Path::new("objects").join(kind.directory()),
    ))?
    else {
        return Ok(hashes);
    };
    for first in children(&base, budget)? {
        let a = text_leaf(&first)?;
        if !hex2(a) {
            return Err(Blocker::unsafe_state("invalid CAS first fanout"));
        }
        let first = checked(child(&base, Path::new(&first)))?;
        for second in children(&first, budget)? {
            let b = text_leaf(&second)?;
            if !hex2(b) {
                return Err(Blocker::unsafe_state("invalid CAS second fanout"));
            }
            let parent = checked(child(&first, Path::new(&second)))?;
            for name in children(&parent, budget)? {
                let leaf = text_leaf(&name)?;
                let digest =
                    if matches!(kind, ContentObjectKind::Prepared | ContentObjectKind::Image) {
                        leaf.strip_prefix(CAS_QUARANTINE).unwrap_or(leaf)
                    } else {
                        leaf
                    };
                if !hex64(digest) || &digest[..2] != a || &digest[2..4] != b {
                    return Err(Blocker::unsafe_state("invalid CAS content leaf"));
                }
                if digest != leaf && checked(parent.contains_entry(Path::new(digest)))? {
                    return Err(Blocker::unsafe_state("CAS source and quarantine coexist"));
                }
                // This open checks only type/link safety; it reads no body.
                // Candidate hashing has the separate aggregate removal budget.
                checked(parent.open_regular_read(Path::new(leaf), u64::MAX))?;
                hashes.insert(format!("sha256:{digest}"));
            }
        }
    }
    Ok(hashes)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StagingDescriptor {
    scope_id: String,
    raw_hash: String,
    tool_profile_hash: String,
    adapter_kind: String,
}
fn classify_staging(
    scope: &BoundScope,
    candidates: &mut BTreeSet<TargetKey>,
    budget: &mut Budget,
) -> Checked<()> {
    let Some(base) = checked(optional_child(&scope.directory, Path::new("staging")))? else {
        return Ok(());
    };
    for name in children(&base, budget)? {
        let actual = text_leaf(&name)?;
        let leaf = logical_directory_leaf(actual)?;
        if leaf != actual && checked(base.contains_entry(Path::new(leaf)))? {
            return Err(Blocker::unsafe_state("staging root and quarantine coexist"));
        }
        let directory = checked(child(&base, Path::new(actual)))?;
        let candidate = TargetKey::Staging(Path::new("staging").join(leaf));
        let Some(bytes) =
            checked(directory.read_optional(Path::new("descriptor.json"), 64 * 1024))?
        else {
            candidates.insert(candidate);
            continue;
        };
        budget.bytes(bytes.len() as u64)?;
        let descriptor: StagingDescriptor = serde_json::from_slice(&bytes).map_err(|error| {
            Blocker::new(
                "unsafe_prune_authority",
                directory
                    .path()
                    .join("descriptor.json")
                    .display()
                    .to_string(),
                &format!("Inspect the malformed staging descriptor before pruning: {error}"),
            )
        })?;
        if descriptor.scope_id != scope.scope_id {
            return Err(Blocker::new(
                "unsafe_prune_authority",
                directory
                    .path()
                    .join("descriptor.json")
                    .display()
                    .to_string(),
                "This descriptor belongs to another scope; inspect its ownership before pruning",
            ));
        }
        if !is_hash(&descriptor.raw_hash)
            || !is_hash(&descriptor.tool_profile_hash)
            || descriptor.adapter_kind.is_empty()
            || descriptor.adapter_kind.len() > 64
            || !descriptor
                .adapter_kind
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err(Blocker::unsafe_state("invalid staging descriptor identity"));
        }
        let expected = format!(
            "{}.{}.{}",
            &descriptor.raw_hash[7..],
            &descriptor.tool_profile_hash[7..],
            descriptor.adapter_kind
        );
        if expected != leaf {
            candidates.insert(candidate);
            continue;
        }
        // PB16(1) permits unknown tasks when a nonempty set of generations
        // is terminal. This explicitly confirmed, writer-locked operation
        // also satisfies PB16(2) after all global blockers have cleared, so
        // it includes that case and the zero-generation recovery case.
        candidates.insert(candidate);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cache_candidates(
    scope: &BoundScope,
    purge: &PurgeState,
    commits: &BTreeMap<String, CommitObject>,
    trees: &BTreeMap<String, TreeObject>,
    reachable: &BTreeSet<String>,
    now: &str,
    live_images: &BTreeSet<String>,
    candidates: &mut BTreeSet<TargetKey>,
    budget: &mut Budget,
) -> Checked<()> {
    let cache = crate::cache_home().join("kio/open");
    match fs::symlink_metadata(&cache) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Blocker::unsafe_state(error)),
        Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
            return Err(Blocker::unsafe_state("open cache root is unsafe"));
        }
        Ok(_) => {}
    }
    let base = checked(StoreDirectory::open(&cache))?;
    for name in children(&base, budget)? {
        let actual = text_leaf(&name)?;
        if actual == "image" {
            continue;
        }
        let leaf = logical_directory_leaf(actual)?;
        if !hex64(leaf) {
            return Err(Blocker::unsafe_state("invalid raw cache root"));
        }
        let _directory = checked(child(&base, Path::new(actual)))?;
        let hash = format!("sha256:{leaf}");
        let lifecycle = canonical_lookup(purge, &hash, commits, trees, reachable, now);
        if lifecycle.tombstone_error.is_some() || lifecycle.receipt_error.is_some() {
            return Err(Blocker::unsafe_state("cache lifecycle proof is invalid"));
        }
        if matches!(
            lifecycle.canonical.map(|value| value.event.kind),
            Some(EventKind::Purged | EventKind::Erased)
        ) {
            candidates.insert(TargetKey::Cache(cache.join(leaf)));
        }
    }
    if let Some(images) = checked(optional_child(&base, Path::new("image")))? {
        for name in children(&images, budget)? {
            let actual = text_leaf(&name)?;
            let leaf = logical_directory_leaf(actual)?;
            if !hex64(leaf) {
                return Err(Blocker::unsafe_state("invalid image cache root"));
            }
            let _directory = checked(child(&images, Path::new(actual)))?;
            if !live_images.contains(&format!("sha256:{leaf}")) {
                candidates.insert(TargetKey::Cache(cache.join("image").join(leaf)));
            }
        }
    }
    checked(scope.session.assert_public_identity())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn historical_missing_unit_does_not_hide_later_corruption_or_duplicate_keys() {
        let directory = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(directory.path());
        let raw = hash_bytes(b"raw");
        let profile = hash_bytes(b"profile");
        let prepared = hash_bytes(b"prepared");
        let unit: NormalizedUnitObject = serde_json::from_value(json!({
            "unit_key":"file:2", "unit_type":"file", "raw_hash":raw,
            "prepared_hash":prepared, "preparation_profile_hash":profile,
            "tool_profile_hash":profile, "gen":0, "mode":"full",
            "markdown":"retained body", "owned_image_hashes":[], "metadata":{},
            "reused_from":null, "generated_at":"2026-09-27T00:00:00Z"
        }))
        .unwrap();
        let bytes = canonical_json_bytes(&serde_json::to_value(&unit).unwrap()).unwrap();
        let present_hash = store
            .write_content_object(ContentObjectKind::NormalizedUnit, &bytes)
            .unwrap();
        let mut manifest: NormalizedInstanceManifest = serde_json::from_value(json!({
            "raw_hash":raw,"tool_profile_hash":profile,"gen":0,"parent_gen":null,
            "run_id":"test","generated_at":"2026-09-27T00:00:00Z",
            "units":[
                {"order":0,"unit_key":"file:1","unit_ref":kio_pipeline::prepare::unit_ref("file:1"),
                 "unit_type":"file","status":"done","prepared_hash":prepared,
                 "preparation_profile_hash":profile,"unit_object_hash":hash_bytes(b"missing"),"error_kind":null},
                {"order":1,"unit_key":"file:2","unit_ref":kio_pipeline::prepare::unit_ref("file:2"),
                 "unit_type":"file","status":"done","prepared_hash":prepared,
                 "preparation_profile_hash":profile,"unit_object_hash":present_hash,"error_kind":null}
            ]
        })).unwrap();
        let units =
            load_surviving_normalized_units(&store, &manifest, &mut State::default()).unwrap();
        assert_eq!(units, (vec![unit], true));
        let path = store
            .content_path(ContentObjectKind::NormalizedUnit, &present_hash)
            .unwrap();
        fs::write(&path, b"corrupt later body").unwrap();
        assert!(load_surviving_normalized_units(&store, &manifest, &mut State::default()).is_err());
        fs::write(path, bytes).unwrap();
        manifest.units.push(manifest.units[0].clone());
        assert!(load_surviving_normalized_units(&store, &manifest, &mut State::default()).is_err());
    }
}
