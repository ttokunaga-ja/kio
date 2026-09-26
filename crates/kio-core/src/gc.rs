//! Read-only, capability-bound garbage collection planning.
//!
//! Public paths are used only to bind and diagnose a scope.  Every store read
//! below is relative to a retained directory descriptor.

mod unreachable_inventory;

pub use unreachable_inventory::{UnreachableInventoryLimits, UnreachableObjectInventory};

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use cap_primitives::{ambient_authority, fs as cap_fs};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::ExitCode;
use crate::cas::{
    MAX_COMMIT_OBJECT_BYTES, MAX_RAW_OBJECT_BYTES, MAX_TREE_OBJECT_BYTES, canonical_json_bytes,
    hash_bytes, is_hash,
};
use crate::dag::{CommitObject, CommitType, MAX_TREE_ENTRIES, TreeObject};
use crate::error::{KioError, Result};
use crate::schema::{SchemaKind, validate_json_schema};
use crate::scope::acquire_bound_reentrant_store_lock;
use crate::scope::{
    BoundReentrantStoreLock, BoundStoreLock, KIO_FORMAT_VERSION, Repository,
    acquire_bound_store_lock, enforce_scope_config_semantics, format_utc_seconds,
    parse_utc_seconds, reject_pending_managed_restore,
};
use crate::store_dir::StoreDirectory;

const MAX_METADATA: u64 = 1024 * 1024;
const MAX_REF: u64 = 4096;
const MAX_MARKER_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SWEEP_CANDIDATES: usize = 100_000;
const MAX_SWEEP_ESTIMATED_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const GC_RETIRE_SENTINEL: &[u8] = b"kio gc retirement sentinel\n";
const SNAPSHOT_AUTO_STATE_LEAF: &str = "snapshot-auto.json";
const SNAPSHOT_AUTO_STATE_VERSION: u32 = 2;
const SNAPSHOT_AUTO_STATE_TEMP_PREFIX: &str = ".snapshot-auto-state-";
/// Scheduler state publishing may leave a private create-new file behind if
/// the process dies after fsync and before the final rename/exchange.  Keep
/// recovery bounded even when a scheduler repeatedly dies at that seam.
const MAX_SNAPSHOT_AUTO_STATE_TEMPORARIES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GcSweepPhase {
    Prepared,
    Receipting,
    Sweeping,
    Finalizing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GcIndexState {
    Absent,
    Present {
        generation: String,
        /// Canonical platform identity of `index/sqlite.db`, obtained from
        /// the descriptor-bound index coordinator.  This binds recovery to
        /// the physical database as well as its logical generation.
        identity: String,
    },
}

fn validate_gc_index_state(state: &GcIndexState) -> bool {
    match state {
        GcIndexState::Absent => true,
        GcIndexState::Present {
            generation,
            identity,
        } => is_canonical_ulid(generation) && is_canonical_gc_index_identity(identity),
    }
}

/// Which marker transition owns a durable private index replacement.  Keeping
/// this explicit prevents a recovery record prepared for tree safety from
/// being mistaken for a finalization-only rotation (or vice versa).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GcIndexRotationRole {
    PreSweep,
    Final,
}

/// A durable two-name index replacement.  `temp_leaf` is a private regular
/// file below `.kio/gc/internal/index`; once this record is in the marker, the public
/// `sqlite.db` is allowed to name either `source` (before exchange) or
/// `target` (after exchange), never an arbitrary third file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GcIndexRotation {
    pub role: GcIndexRotationRole,
    pub temp_leaf: String,
    /// Canonical identity of the operation-reserved `gc/internal/index`
    /// directory. Recovery must not silently adopt a replacement namespace.
    pub private_dir_identity: String,
    pub source: GcIndexState,
    /// Digest of the complete source-file state captured after the private
    /// copy was fsynced. Recovery must re-establish this exact state before
    /// exchanging the clone, not merely the same generation and inode.
    pub source_state_digest: String,
    pub target: GcIndexState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GcMarkerCandidate {
    pub commit_hash: String,
    pub tree_hash: String,
    /// Exact verified length of the immutable tree object.  This is frozen
    /// with the operation so recovery cannot turn a low estimate into a
    /// larger destructive operation after the initial plan has disappeared.
    pub size_bytes: u64,
}

/// Frozen, bounded operation state. It is strict JCS+LF so it can safely act
/// as the crash-recovery authority only after a fresh current-truth check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GcInProgressMarker {
    pub version: u32,
    pub sweep_id: String,
    pub started_at: String,
    pub phase: GcSweepPhase,
    pub plan_digest: String,
    pub truth_digest: String,
    /// Immutable recovery truth: scope/config/policy/refs/commits only. Unlike
    /// `truth_digest`, it deliberately survives this operation's receipts and
    /// tree removals and is compared on every resume.
    pub stable_truth_digest: String,
    /// Digest of receipts that predate this operation (frozen candidates
    /// excluded). This detects external receipt mutation while allowing own
    /// receipt-first progress.
    pub baseline_receipts_digest: String,
    /// Exact file-observation digest of marker-owned receipts, frozen only
    /// once all receipts are durable and before entering Sweeping.
    pub operation_receipts_digest: Option<String>,
    pub candidates: Vec<GcMarkerCandidate>,
    pub trees: Vec<String>,
    pub estimated_bytes: u64,
    pub index_initial: GcIndexState,
    /// Durable target for the pre-delete rotation.  It is written before the
    /// SQLite mutation so recovery can distinguish a completed first rotation
    /// from a replacement of the marker's initial generation.
    pub index_pre_sweep: Option<GcIndexState>,
    pub index_final: Option<GcIndexState>,
    pub index_rotation: Option<GcIndexRotation>,
}

/// Exact, bounded observation of the marker's durable recovery state. This is
/// deliberately derived from retained descriptors; marker progress is never a
/// source of truth by itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GcSweepProgress {
    pub receipt_count: usize,
    pub receipts_complete: bool,
    pub any_tree_missing: bool,
    pub all_trees_missing: bool,
}

/// Process-local proof that trusted index coordination validated the durable
/// pre-sweep generation attestation for this exact marker and retained store.
/// Its fields are private so safe callers cannot turn marker JSON into delete
/// authority without going through the index verifier.
#[derive(Debug)]
pub struct GcTreeRemovalPermit {
    kio_identity: Identity,
    marker_digest: String,
    index_file: Option<std::fs::File>,
    index_state: Option<FileState>,
}

impl GcInProgressMarker {
    pub fn from_plan(
        plan: &GcPlan,
        sweep_id: String,
        started_at: String,
        index_initial: GcIndexState,
    ) -> Result<Self> {
        let candidates: Vec<_> = plan
            .candidates
            .iter()
            .map(|candidate| GcMarkerCandidate {
                commit_hash: candidate.commit_hash.clone(),
                tree_hash: candidate.tree_hash.clone(),
                size_bytes: candidate.size_bytes,
            })
            .collect();
        let trees = candidates
            .iter()
            .map(|candidate| candidate.tree_hash.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let marker = Self {
            version: 1,
            sweep_id,
            started_at,
            phase: GcSweepPhase::Prepared,
            plan_digest: plan.plan_digest.clone(),
            truth_digest: plan.truth_digest.clone(),
            stable_truth_digest: plan.stable_truth_digest.clone(),
            baseline_receipts_digest: plan.baseline_receipts_digest.clone(),
            operation_receipts_digest: None,
            candidates,
            trees,
            estimated_bytes: plan.estimated_bytes,
            index_initial,
            index_pre_sweep: None,
            index_final: None,
            index_rotation: None,
        };
        marker.validate()?;
        Ok(marker)
    }
    pub fn validate(&self) -> Result<()> {
        if self.version != 1
            || !is_hash(&self.plan_digest)
            || !is_hash(&self.truth_digest)
            || !is_hash(&self.stable_truth_digest)
            || !is_hash(&self.baseline_receipts_digest)
            || !is_canonical_utc_timestamp(&self.started_at)
            || self.sweep_id.is_empty()
            || !is_canonical_ulid(&self.sweep_id)
            || self.candidates.len() > MAX_SWEEP_CANDIDATES
            || self.trees.len() > MAX_SWEEP_CANDIDATES
            || self.estimated_bytes > MAX_SWEEP_ESTIMATED_BYTES
        {
            return Err(corrupt("invalid GC in-progress marker"));
        }
        if self
            .operation_receipts_digest
            .as_ref()
            .is_some_and(|digest| !is_hash(digest))
        {
            return Err(corrupt("invalid GC operation receipt digest"));
        }
        let mut previous = "";
        let mut candidate_trees = BTreeSet::new();
        let mut tree_sizes = BTreeMap::new();
        for candidate in &self.candidates {
            if !is_hash(&candidate.commit_hash)
                || !is_hash(&candidate.tree_hash)
                || candidate.commit_hash.as_str() <= previous
                || candidate.size_bytes > MAX_TREE_OBJECT_BYTES
            {
                return Err(corrupt("GC marker candidates are invalid or unsorted"));
            }
            previous = &candidate.commit_hash;
            candidate_trees.insert(candidate.tree_hash.clone());
            match tree_sizes.insert(candidate.tree_hash.clone(), candidate.size_bytes) {
                Some(previous_size) if previous_size != candidate.size_bytes => {
                    return Err(corrupt("GC marker shared tree sizes differ"));
                }
                _ => {}
            }
        }
        let trees: BTreeSet<_> = self.trees.iter().cloned().collect();
        if trees.len() != self.trees.len()
            || self.trees.windows(2).any(|x| x[0] >= x[1])
            || trees != candidate_trees
            || trees.iter().any(|tree| !is_hash(tree))
        {
            return Err(corrupt("GC marker tree set is invalid"));
        }
        let committed_bytes = tree_sizes.values().try_fold(0u64, |total, size| {
            total
                .checked_add(*size)
                .ok_or_else(|| corrupt("GC marker estimated bytes overflow"))
        })?;
        if committed_bytes != self.estimated_bytes {
            return Err(corrupt("GC marker estimated bytes do not bind candidates"));
        }
        let worst_case = (tree_sizes.len() as u64)
            .checked_mul(MAX_TREE_OBJECT_BYTES)
            .ok_or_else(|| corrupt("GC marker tree bound overflow"))?;
        if worst_case > MAX_SWEEP_ESTIMATED_BYTES {
            return Err(corrupt("GC marker tree count exceeds sweep byte bound"));
        }
        if let GcIndexState::Present {
            generation,
            identity,
        } = &self.index_initial
            && (!is_canonical_ulid(generation) || !is_canonical_gc_index_identity(identity))
        {
            return Err(corrupt("invalid GC marker initial index generation"));
        }
        if let Some(GcIndexState::Present {
            generation,
            identity,
        }) = &self.index_pre_sweep
            && (!is_canonical_ulid(generation) || !is_canonical_gc_index_identity(identity))
        {
            return Err(corrupt("invalid GC marker pre-sweep index generation"));
        }
        if let Some(GcIndexState::Present {
            generation,
            identity,
        }) = &self.index_final
            && (!is_canonical_ulid(generation) || !is_canonical_gc_index_identity(identity))
        {
            return Err(corrupt("invalid GC marker final index generation"));
        }
        let valid_pre_sweep = match (&self.index_initial, &self.index_pre_sweep) {
            (_, None) => matches!(
                self.phase,
                GcSweepPhase::Prepared | GcSweepPhase::Receipting | GcSweepPhase::Sweeping
            ),
            (
                GcIndexState::Present {
                    generation: initial,
                    identity: initial_identity,
                },
                Some(GcIndexState::Present {
                    generation: target,
                    identity: target_identity,
                }),
            ) => initial != target && initial_identity != target_identity,
            (GcIndexState::Absent, Some(GcIndexState::Absent)) => true,
            _ => false,
        };
        let valid_final = matches!(
            (&self.index_initial, &self.index_final),
            (_, None)
                | (GcIndexState::Absent, Some(GcIndexState::Absent))
                | (
                    GcIndexState::Present { .. },
                    Some(GcIndexState::Present { .. })
                )
        );
        let valid_phase_index_state = match self.phase {
            GcSweepPhase::Prepared | GcSweepPhase::Receipting => {
                self.index_pre_sweep.is_none() && self.index_final.is_none()
            }
            GcSweepPhase::Sweeping => self.index_final.is_none(),
            GcSweepPhase::Finalizing => self.index_pre_sweep.is_some(),
        };
        let valid_receipt_binding = match self.phase {
            GcSweepPhase::Prepared | GcSweepPhase::Receipting => {
                self.operation_receipts_digest.is_none()
            }
            GcSweepPhase::Sweeping | GcSweepPhase::Finalizing => {
                self.operation_receipts_digest.is_some()
            }
        };
        if !valid_pre_sweep || !valid_final || !valid_phase_index_state || !valid_receipt_binding {
            return Err(corrupt("GC marker index rotation state is invalid"));
        }
        if let Some(rotation) = &self.index_rotation {
            if !is_valid_gc_index_temp_leaf(&rotation.temp_leaf)
                || !is_canonical_gc_index_identity(&rotation.private_dir_identity)
                || !is_hash(&rotation.source_state_digest)
                || matches!(rotation.source, GcIndexState::Absent)
                || matches!(rotation.target, GcIndexState::Absent)
                || rotation.source == rotation.target
                || !validate_gc_index_state(&rotation.source)
                || !validate_gc_index_state(&rotation.target)
            {
                return Err(corrupt("GC marker index rotation is invalid"));
            }
            let role_matches_phase = matches!(
                (&rotation.role, &self.phase),
                (GcIndexRotationRole::PreSweep, GcSweepPhase::Sweeping)
                    | (GcIndexRotationRole::Final, GcSweepPhase::Finalizing)
            );
            if !role_matches_phase {
                return Err(corrupt("GC marker index rotation role is invalid"));
            }
            let expected_source = match rotation.role {
                GcIndexRotationRole::PreSweep => {
                    self.index_pre_sweep.as_ref().or(Some(&self.index_initial))
                }
                GcIndexRotationRole::Final => {
                    self.index_final.as_ref().or(self.index_pre_sweep.as_ref())
                }
            };
            if expected_source != Some(&rotation.source) {
                return Err(corrupt("GC marker index rotation source is invalid"));
            }
        }
        // Bound the actual canonical on-disk representation, not merely the
        // cardinalities which can vary in JSON escaping/number encoding.
        if serde_jcs::to_vec(self)
            .map_err(|error| corrupt(&error.to_string()))?
            .len()
            .saturating_add(1) as u64
            > MAX_MARKER_BYTES
        {
            return Err(corrupt("GC in-progress marker exceeds byte limit"));
        }
        Ok(())
    }
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut b = canonical_json_bytes(
            &serde_json::to_value(self).map_err(|e| corrupt(&e.to_string()))?,
        )?;
        b.push(b'\n');
        Ok(b)
    }
    pub fn parse_canonical(bytes: &[u8]) -> Result<Self> {
        if bytes.len() as u64 > MAX_MARKER_BYTES
            || !bytes.ends_with(b"\n")
            || bytes[..bytes.len().saturating_sub(1)].contains(&b'\n')
        {
            return Err(corrupt("GC marker is not canonical JCS+LF"));
        }
        let marker: Self = serde_json::from_slice(bytes)
            .map_err(|_| corrupt("malformed GC in-progress marker"))?;
        marker.validate()?;
        if marker.canonical_bytes()? != bytes {
            return Err(corrupt("GC marker is not canonical JCS+LF"));
        }
        Ok(marker)
    }
    #[must_use]
    pub fn is_frozen_pair(&self, commit_hash: &str, tree_hash: &str) -> bool {
        self.candidates
            .iter()
            .any(|c| c.commit_hash == commit_hash && c.tree_hash == tree_hash)
    }
}

/// Strict no-follow inventory for consumers that need to distinguish legitimate
/// shallow objects from corruption without reimplementing the receipt format.
pub fn read_shallow_receipts(kio_dir: &Path) -> Result<BTreeMap<String, ShallowReceipt>> {
    let kio = open_bound_absolute(&absolute_lexical_path(kio_dir)?)?;
    read_shallow_receipts_bound(&kio)
}
fn read_shallow_receipts_bound(kio: &std::fs::File) -> Result<BTreeMap<String, ShallowReceipt>> {
    let Some(gc) = open_optional_dir(kio, "gc")? else {
        return Ok(BTreeMap::new());
    };
    let Some(dir) = open_optional_dir(&gc, "shallowed")? else {
        return Ok(BTreeMap::new());
    };
    let mut stats = GcPlanStats::default();
    let limits = GcPlanLimits::default();
    let mut result = BTreeMap::new();
    for leaf in names(&dir, &mut stats, &limits, 3)? {
        if leaf.len() != 64 || !hex(&leaf) {
            return Err(corrupt("invalid shallow receipt leaf"));
        }
        let (bytes, _) = read_regular_observed(&dir, &leaf, MAX_METADATA)?;
        account(&mut stats, bytes.len() as u64, &limits)?;
        let receipt = ShallowReceipt::parse_canonical(&bytes, &leaf)?;
        if result
            .insert(receipt.commit_hash.clone(), receipt)
            .is_some()
        {
            return Err(corrupt("duplicate shallow receipt"));
        }
        stats.receipts = checked(stats.receipts, 1, "receipts")?;
        if stats.receipts > limits.max_receipts {
            return Err(limit("receipts"));
        }
    }
    Ok(result)
}

pub fn read_active_marker(kio_dir: &Path) -> Result<Option<GcInProgressMarker>> {
    let kio = open_bound_absolute(&absolute_lexical_path(kio_dir)?)?;
    read_active_marker_bound(&kio)
}
fn read_active_marker_bound(kio: &std::fs::File) -> Result<Option<GcInProgressMarker>> {
    let Some(gc) = open_optional_dir(kio, "gc")? else {
        return Ok(None);
    };
    let (bytes, _) = match read_regular_observed(&gc, "in_progress", MAX_MARKER_BYTES) {
        Ok(value) => value,
        Err(error) if is_io_not_found(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    GcInProgressMarker::parse_canonical(&bytes).map(Some)
}

pub fn ensure_no_active_sweep(kio_dir: &Path) -> Result<()> {
    if let Some(marker) = read_active_marker(kio_dir)? {
        return Err(active_sweep_error(&marker));
    }
    Ok(())
}

/// Retained-authority counterpart for repository and scheduler writers which
/// already own the exact `.kio` directory and must not reopen its pathname.
pub(crate) fn ensure_no_active_sweep_bound(kio: &std::fs::File) -> Result<()> {
    if let Some(marker) = read_active_marker_bound(kio)? {
        return Err(active_sweep_error(&marker));
    }
    Ok(())
}

fn active_sweep_error(marker: &GcInProgressMarker) -> KioError {
    KioError::new(
        "KIO-E-GC-SWEEP-ACTIVE-001",
        "a GC shallow sweep is active; resume it with kio gc --yes",
        json!({"sweep_id":marker.sweep_id}),
        ExitCode::PartialFailure,
    )
}

/// Inventory every markerless final shallow boundary in one bounded pass.
///
/// The returned map is the only stable reason a present commit may have an
/// absent tree.  In particular, a tree shared by multiple commits is admitted
/// only when every sharer has its own exact final receipt.  Keeping this as one
/// inventory prevents callers that need all receipts from multiplying the
/// bounded commit walk by the number of receipts.
pub fn validated_final_shallow_receipts(kio_dir: &Path) -> Result<BTreeMap<String, String>> {
    // Bind the supplied `.kio` spelling before any canonical diagnostic
    // lookup.  This rejects a symlink/reparse replacement at the public
    // boundary; `GcPlanner::bind` may subsequently canonicalize the already
    // verified scope only to normalize macOS's `/var` spelling.
    let bound_kio_path = absolute_lexical_path(kio_dir)?;
    let root = bound_kio_path
        .parent()
        .ok_or_else(|| corrupt("Kio directory has no scope root"))?;
    let planner = GcPlanner::bind(root.to_path_buf())?;
    let supplied_kio = open_bound_absolute(&bound_kio_path)?;
    if id_file(&supplied_kio)? != id_file(&planner.kio)? {
        return Err(corrupt(
            "Kio directory changed while binding shallow inventory",
        ));
    }
    let root_id = id_file(&planner.scope)?;
    let kio_id = id_file(&planner.kio)?;
    planner.require_layout()?;
    let first = inventory_final_shallow_receipts(&planner)?;
    let second = inventory_final_shallow_receipts(&planner)?;
    if first != second {
        return Err(corrupt(
            "store truth changed while validating final shallow receipts",
        ));
    }
    planner.recheck(root_id, kio_id)?;
    Ok(first.receipts)
}

/// `/var` is the platform-owned macOS spelling of `/private/var`.  Normalize
/// only that lexical alias before descriptor binding; do not call
/// `canonicalize` on arbitrary user-controlled components.
fn normalize_macos_var_alias(path: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        if let Ok(rest) = path.strip_prefix("/var") {
            return Path::new("/private/var").join(rest);
        }
    }
    path.to_path_buf()
}

/// Make a supplied relative spelling absolute without resolving any
/// user-controlled component.  Joining to the already-open process cwd keeps
/// the no-follow component walk authoritative while preserving callers that
/// deliberately open child scopes by relative path.
fn absolute_lexical_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| ioerr(error, path))?
            .join(path)
    };
    Ok(normalize_macos_var_alias(&absolute))
}

/// Complete descriptor-bound observation used by the final shallow inventory.
/// Directory observations deliberately include absent tree slots: an atomic
/// replacement or later creation changes the retained parent identity/state,
/// so the independent second pass cannot bless a stale absence.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FinalShallowInventory {
    receipts: BTreeMap<String, String>,
    observations: BTreeMap<String, FileObservation>,
}

fn inventory_final_shallow_receipts(planner: &GcPlanner) -> Result<FinalShallowInventory> {
    let mut stats = GcPlanStats::default();
    let mut observations = BTreeMap::new();
    observe_directory(&planner.kio, "dir:.kio", &mut observations)?;
    if let Some(marker) = read_active_marker_bound(&planner.kio)? {
        return Err(KioError::new(
            "KIO-E-GC-SWEEP-ACTIVE-001",
            "a GC shallow sweep is active; resume it with kio gc --yes",
            json!({"sweep_id":marker.sweep_id}),
            ExitCode::PartialFailure,
        ));
    }
    let refs = planner.read_refs(&mut stats, &mut observations)?;
    let receipts = planner.read_receipts(&mut stats, &mut observations)?;
    let commits = planner.inventory_commits(&mut stats, &mut observations)?;
    validate_commit_links(&commits)?;
    validate_receipt_links(&receipts, &commits)?;

    let mut sharers = HashMap::<&str, Vec<&str>>::new();
    for (commit_hash, commit) in &commits {
        sharers
            .entry(&commit.tree)
            .or_default()
            .push(commit_hash.as_str());
    }

    // Tree absence is shared by every commit naming that immutable tree.  A
    // receipt for only one sharer cannot turn the other commits' missing tree
    // into a legitimate final boundary.  Require the exact markerless receipt
    // relation for every sharer, including commit-type and ref-tip protection.
    let mut receipt_trees = BTreeSet::new();
    for (commit_hash, tree_hash) in &receipts {
        if refs.values().any(|tip| tip == commit_hash) {
            return Err(corrupt("shallow receipt commit is a current ref tip"));
        }
        for sharer_hash in sharers
            .get(tree_hash.as_str())
            .ok_or_else(|| corrupt("shallow receipt commit is missing"))?
        {
            if receipts.get(*sharer_hash) != Some(tree_hash) {
                return Err(corrupt("shared shallow tree is missing a commit receipt"));
            }
        }
        receipt_trees.insert(tree_hash.as_str());
    }

    let trees = open_path(&planner.kio, "objects/trees")?;
    observe_directory(&trees, "dir:objects/trees", &mut observations)?;
    for tree_hash in receipt_trees {
        let raw = tree_hash
            .strip_prefix("sha256:")
            .ok_or_else(|| corrupt("invalid shallow receipt tree"))?;
        let present = match open_optional_dir(&trees, &raw[..2])? {
            Some(first) => {
                observe_directory(
                    &first,
                    format!("dir:objects/trees/{}", &raw[..2]),
                    &mut observations,
                )?;
                match open_optional_dir(&first, &raw[2..4])? {
                    Some(second) => {
                        observe_directory(
                            &second,
                            format!("dir:objects/trees/{}/{}", &raw[..2], &raw[2..4]),
                            &mut observations,
                        )?;
                        match read_regular_observed(&second, raw, MAX_TREE_OBJECT_BYTES) {
                            Ok((_, observation)) => {
                                insert_observation(
                                    &mut observations,
                                    format!("tree/{}/{}/{}", &raw[..2], &raw[2..4], raw),
                                    observation,
                                )?;
                                true
                            }
                            Err(error) if is_io_not_found(&error) => false,
                            Err(error) => return Err(error),
                        }
                    }
                    None => false,
                }
            }
            None => false,
        };
        if present {
            return Err(corrupt("shallow receipt coexists with its tree object"));
        }
    }
    Ok(FinalShallowInventory {
        receipts: receipts.into_iter().collect(),
        observations,
    })
}

/// Validate one exact member of [`validated_final_shallow_receipts`].
///
/// Call this only after the tree read has returned STORE-NOT-FOUND.
pub fn validate_final_shallow_tree(
    kio_dir: &Path,
    commit_hash: &str,
    tree_hash: &str,
) -> Result<()> {
    let receipts = validated_final_shallow_receipts(kio_dir)?;
    match receipts.get(commit_hash) {
        Some(receipt_tree) if receipt_tree == tree_hash => Ok(()),
        Some(receipt_tree) => Err(KioError::new(
            "KIO-E-STORE-CORRUPT-001",
            "shallow receipt tree does not match its commit",
            json!({
                "commit_hash": commit_hash,
                "receipt_tree_hash": receipt_tree,
                "commit_tree_hash": tree_hash,
            }),
            ExitCode::PermanentFailure,
        )),
        None => Err(KioError::new(
            "KIO-E-STORE-CORRUPT-001",
            "missing commit tree has no canonical shallow receipt",
            json!({"commit_hash": commit_hash, "tree_hash": tree_hash}),
            ExitCode::PermanentFailure,
        )),
    }
}

/// Capability-bound mutation half of shallow GC. It deliberately owns only
/// `.kio/gc` and `objects/trees`; no method accepts an object kind or an
/// ambient victim path, making accidental commit/raw/index deletion impossible.
#[derive(Debug)]
pub struct GcSweepSession {
    root: PathBuf,
    scope: std::fs::File,
    kio: std::fs::File,
    #[cfg(debug_assertions)]
    test_control: crate::test_control::CoreTestControl,
}

/// How the CLI may invoke bounded automatic shallow GC.
///
/// The default remains [`GcAutomationMode::ManualOnly`]; callers must not
/// infer automatic deletion from the presence of a GC configuration table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcAutomationMode {
    ManualOnly,
    AfterIndex,
    OnIdle,
}

/// Strictly validated GC automation settings read from `.kio/config.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcAutomationConfig {
    pub mode: GcAutomationMode,
    pub max_runtime_seconds: Option<u64>,
    pub idle_threshold_seconds: Option<u64>,
}

/// A capability-bound observation of the complete validated `[gc]` authority
/// subtree that authorized an automatic GC handoff. The canonical semantic
/// digest covers retention policy and future schema-allowed GC controls, while
/// intentionally excluding unrelated config such as network approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcAutomationBinding {
    pub config: GcAutomationConfig,
    gc_config_digest: String,
    // Keep the retained scope capabilities in the handoff authority as well
    // as the GC policy. These fields stay private because callers only need
    // equality; exposing platform-specific dev/inode identities would invite
    // ambient-path reconstruction instead of capability reuse.
    scope_identity: Identity,
    kio_identity: Identity,
}

/// Strict scheduler settings from the optional `[snapshot.auto]` table.
/// Absence of the table is represented by `None`; there are deliberately no
/// field defaults or legacy aliases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotAutoConfig {
    pub enabled: bool,
    pub interval_seconds: u64,
    pub on_change_threshold: u64,
}

/// Capability-bound snapshot of the configuration authority used by one
/// scheduler invocation.  The private observation fields make equality bind
/// the exact regular-file identity/state/digest as well as the parsed policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotAutoBinding {
    pub config: Option<SnapshotAutoConfig>,
    config_observation: FileObservation,
    // The root ignore file is part of the effective snapshot policy.  Bind its
    // exact optional observation too: a same-UID edit must not turn a preview
    // that excluded a path into a later publication that archives it.
    root_ignore_observation: Option<FileObservation>,
    scope_identity: Identity,
    kio_identity: Identity,
}

/// Durable scheduler checkpoint. It records prepared eligible attempts,
/// including deterministic no-ops, before a scheduled ref can advance. A
/// later ref-publication failure may therefore defer the next attempt; that is
/// the intended fail-closed alternative to advancing a ref without its state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotAutoState {
    pub version: u32,
    pub last_successful_eligible_attempt_at: String,
    pub working_set_digest: String,
    pub idle_observed_since: String,
}

/// Exact optional state-file observation.  Callers compare this value before
/// and after taking the writer lock; same-byte inode/timestamp replacement is
/// therefore observable rather than treated as an unchanged checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotAutoStateBinding {
    pub state: Option<SnapshotAutoState>,
    observation: Option<FileObservation>,
}

impl Default for GcAutomationConfig {
    fn default() -> Self {
        Self {
            mode: GcAutomationMode::ManualOnly,
            max_runtime_seconds: None,
            idle_threshold_seconds: None,
        }
    }
}

/// Whether a receipt was newly made durable by this invocation or already
/// existed as the exact frozen receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcReceiptPublication {
    NewlyPublished,
    AlreadyPresent,
}

impl GcSweepSession {
    /// Recheck the managed-restore exclusion using the retained `.kio`
    /// capability.  Public mutation entry points call this after their caller
    /// holds the bound writer lock, closing the bind-to-first-write race.
    fn ensure_no_pending_managed_restore_under_lock(&self) -> Result<()> {
        let directory = StoreDirectory::from_retained(
            self.kio
                .try_clone()
                .map_err(|error| ioerr(error, &self.root))?,
            self.root.join(".kio"),
        )?;
        reject_pending_managed_restore(&directory)
    }

    pub fn bind(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        if !root.is_absolute() {
            return Err(KioError::invalid_usage(
                "GC sweep scope root must be absolute",
            ));
        }
        let scope = open_bound_absolute(&root)?;
        // A genuinely absent `.kio` is ordinary CLI misuse, not store
        // corruption.  Keep every other no-follow/open failure observable so
        // a symlink, reparse point, or non-directory can never masquerade as
        // an uninitialized scope.
        let kio = open_optional_dir(&scope, ".kio")?
            .ok_or_else(|| KioError::invalid_usage("current directory is not a Kio scope"))?;
        let canonical = root.canonicalize().map_err(|e| ioerr(e, &root))?;
        if id_file(&scope)? != id_path(&canonical)? || id_file(&kio)? != id_child(&scope, ".kio")? {
            return Err(corrupt("scope changed while binding GC sweep"));
        }
        let kio_directory = StoreDirectory::from_retained(
            kio.try_clone().map_err(|error| ioerr(error, &canonical))?,
            canonical.join(".kio"),
        )?;
        reject_pending_managed_restore(&kio_directory)?;
        Ok(Self {
            root: canonical,
            scope,
            kio,
            #[cfg(debug_assertions)]
            // Binding a sweep is a library operation root. Capture once here so
            // every later checkpoint observes the same controls, even when no
            // CLI composition root installed a snapshot.
            test_control: crate::test_control::capture_for_operation().core,
        })
    }
    /// Bind an automatic sweep to the exact repository capability already
    /// used by an internal child index. Ordinary repositories have no retained
    /// handles and use the same public no-follow bind as [`Self::bind`].
    pub fn bind_repository(repository: &Repository) -> Result<Self> {
        let session = Self::bind(repository.canonical_root().to_path_buf())?;
        #[cfg(unix)]
        match (
            repository.bound_root_handle(),
            repository.bound_kio_handle(),
        ) {
            (Some(scope), Some(kio)) => {
                if id_file(&session.scope)? != id_file(scope)?
                    || id_file(&session.kio)? != id_file(kio)?
                {
                    return Err(corrupt(
                        "automatic GC scope differs from bound child repository",
                    ));
                }
            }
            (None, None) => {}
            _ => return Err(corrupt("incomplete bound child repository capability")),
        }
        Ok(session)
    }

    fn inject_gc_tree_fault(&self, _point: GcTreeFaultPoint) -> Result<()> {
        #[cfg(debug_assertions)]
        if self
            .test_control
            .gc_fault
            .known()
            .is_some_and(|fault| _point.matches(*fault))
        {
            return Err(KioError::new(
                "KIO-E-GC-TEST-INTERRUPTED-001",
                "GC test fault injection interrupted the sweep",
                json!({"point": _point.label()}),
                ExitCode::Interrupted,
            ));
        }
        Ok(())
    }

    fn wait_at_gc_tree_quarantine_barrier(&self) {
        #[cfg(debug_assertions)]
        wait_at_gc_test_barrier(self.test_control.gc_tree_quarantine_ready.as_deref());
    }

    fn wait_at_gc_before_state_write_barrier(&self) {
        #[cfg(debug_assertions)]
        wait_at_gc_test_barrier(
            self.test_control
                .snapshot_before_state_write_ready
                .as_deref(),
        );
    }

    fn wait_at_gc_pre_quarantine_barrier(&self) {
        #[cfg(debug_assertions)]
        wait_at_gc_test_barrier(self.test_control.gc_pre_quarantine_ready.as_deref());
    }

    pub fn read_marker(&self) -> Result<Option<GcInProgressMarker>> {
        read_active_marker_bound(&self.kio)
    }
    pub fn acquire_store_lock(&self) -> Result<BoundStoreLock> {
        self.recheck_binding()?;
        acquire_bound_store_lock(&self.kio)
    }
    /// Acquire the scheduler writer barrier relative to the retained `.kio`
    /// descriptor, then make nested [`Repository`] store-lock acquisitions
    /// reentrant on this thread. There is deliberately no ambient fallback:
    /// platforms that cannot prove the descriptor-relative lock boundary fail
    /// closed through the underlying bound-lock primitive.
    pub fn acquire_snapshot_auto_store_lock(&self) -> Result<BoundReentrantStoreLock> {
        self.recheck_binding()?;
        let lock = acquire_bound_reentrant_store_lock(&self.kio)?;
        if let Err(error) = self.recheck_binding() {
            drop(lock);
            return Err(error);
        }
        if let Err(error) = ensure_no_active_sweep_bound(&self.kio) {
            drop(lock);
            return Err(error);
        }
        Ok(lock)
    }
    /// Read the validated complete GC authority subtree and retain a canonical
    /// semantic digest for the publication-to-GC handoff. Both reads are
    /// descriptor-bound and bracketed by scope identity checks.
    pub fn automation_binding(&self) -> Result<GcAutomationBinding> {
        self.recheck_binding()?;
        let (bytes, _) = read_regular_observed(&self.kio, "config.toml", MAX_METADATA)?;
        let config = read_automation_config_bytes(&bytes)?;
        self.recheck_binding()?;
        let parsed = parse_config_bytes(&bytes)?;
        let gc = parsed
            .as_ref()
            .and_then(|value| value.get("gc"))
            .map(serde_json::to_value)
            .transpose()
            .map_err(|error| KioError::schema(error.to_string()))?
            .unwrap_or(serde_json::Value::Null);
        Ok(GcAutomationBinding {
            config,
            gc_config_digest: hash_bytes(&canonical_json_bytes(&gc)?),
            scope_identity: id_file(&self.scope)?,
            kio_identity: id_file(&self.kio)?,
        })
    }

    /// Read the complete validated configuration through the retained `.kio`
    /// capability and bind the exact file observation used for scheduled
    /// snapshot authorization.
    pub fn snapshot_auto_binding(&self) -> Result<SnapshotAutoBinding> {
        self.recheck_binding()?;
        let (bytes, config_observation) =
            read_regular_observed(&self.kio, "config.toml", MAX_METADATA)?;
        let config = read_snapshot_auto_config_bytes(&bytes)?;
        let root_ignore_observation =
            match read_regular_observed(&self.scope, ".kioignore", MAX_METADATA) {
                Ok((_, observation)) => Some(observation),
                Err(error) if is_io_not_found(&error) => None,
                Err(error) => return Err(error),
            };
        self.recheck_binding()?;
        Ok(SnapshotAutoBinding {
            config,
            config_observation,
            root_ignore_observation,
            scope_identity: id_file(&self.scope)?,
            kio_identity: id_file(&self.kio)?,
        })
    }

    /// Read the optional strict scheduler checkpoint relative to the retained
    /// store handle.  A missing file is the only representation of a first
    /// scheduled attempt.
    pub fn snapshot_auto_state(&self) -> Result<SnapshotAutoStateBinding> {
        self.recheck_binding()?;
        let (state, observation) =
            match read_regular_observed(&self.kio, SNAPSHOT_AUTO_STATE_LEAF, MAX_METADATA) {
                Ok((bytes, observation)) => {
                    (Some(parse_snapshot_auto_state(&bytes)?), Some(observation))
                }
                Err(error) if is_io_not_found(&error) => (None, None),
                Err(error) => return Err(error),
            };
        self.recheck_binding()?;
        Ok(SnapshotAutoStateBinding { state, observation })
    }

    /// Atomically and durably publish the scheduler checkpoint at the prepared
    /// eligible-attempt boundary. `expected` must be the exact state observation
    /// re-read under the writer lock. A replacement between eligibility and
    /// publication fails closed before a scheduled ref can advance.
    pub fn publish_snapshot_auto_state(
        &self,
        expected: &SnapshotAutoStateBinding,
        now: &str,
        working_set_digest: &str,
        record_eligible_attempt: bool,
    ) -> Result<SnapshotAutoStateBinding> {
        self.prepare_snapshot_auto_state_publication(expected)?;
        let seconds = parse_utc_seconds(now)
            .filter(|seconds| format_utc_seconds(*seconds) == now)
            .ok_or_else(|| {
                snapshot_auto_clock_error("current time is not canonical UTC seconds")
            })?;
        if !is_hash(working_set_digest) {
            return Err(corrupt(
                "snapshot auto working-set digest is not canonical sha256",
            ));
        }
        if expected.state.is_none() && !record_eligible_attempt {
            return Err(corrupt(
                "first snapshot auto state publication must record an eligible attempt",
            ));
        }
        if let Some(previous) = expected.state.as_ref() {
            let previous_seconds = parse_utc_seconds(&previous.last_successful_eligible_attempt_at)
                .ok_or_else(|| corrupt("snapshot auto state time is invalid"))?;
            let observed_since_seconds = parse_utc_seconds(&previous.idle_observed_since)
                .ok_or_else(|| corrupt("snapshot auto state idle time is invalid"))?;
            if seconds < previous_seconds || seconds < observed_since_seconds {
                return Err(snapshot_auto_clock_error(
                    "scheduled snapshot clock moved backwards",
                ));
            }
        }
        let previous = expected.state.as_ref();
        let state = SnapshotAutoState {
            version: SNAPSHOT_AUTO_STATE_VERSION,
            last_successful_eligible_attempt_at: match previous {
                Some(state) if !record_eligible_attempt => {
                    state.last_successful_eligible_attempt_at.clone()
                }
                _ => format_utc_seconds(seconds),
            },
            working_set_digest: working_set_digest.to_owned(),
            idle_observed_since: match previous {
                Some(state) if state.working_set_digest == working_set_digest => {
                    state.idle_observed_since.clone()
                }
                _ => format_utc_seconds(seconds),
            },
        };
        let bytes = canonical_snapshot_auto_state_bytes(&state)?;
        let temporary = unique_internal_name(".snapshot-auto-state");
        create_new_bound(&self.kio, &temporary, &bytes, MAX_METADATA)?;
        let (_, temporary_observation) =
            read_regular_observed(&self.kio, &temporary, MAX_METADATA)?;
        if let Err(error) = self.recheck_binding() {
            retire_snapshot_state_temporary(&self.kio, &temporary, &bytes, &temporary_observation)?;
            return Err(error);
        }
        let observed_state = match self.snapshot_auto_state() {
            Ok(state) => state,
            Err(error) => {
                retire_snapshot_state_temporary(
                    &self.kio,
                    &temporary,
                    &bytes,
                    &temporary_observation,
                )?;
                return Err(error);
            }
        };
        if &observed_state != expected {
            retire_snapshot_state_temporary(&self.kio, &temporary, &bytes, &temporary_observation)?;
            return Err(snapshot_auto_state_changed());
        }
        self.wait_at_gc_before_state_write_barrier();
        match expected.observation.as_ref() {
            None => {
                if let Err(error) =
                    rename_snapshot_state_noreplace(&self.kio, &temporary, SNAPSHOT_AUTO_STATE_LEAF)
                {
                    let state_changed = self
                        .snapshot_auto_state()
                        .map(|state| &state != expected)
                        .unwrap_or(true);
                    retire_snapshot_state_temporary(
                        &self.kio,
                        &temporary,
                        &bytes,
                        &temporary_observation,
                    )?;
                    if state_changed {
                        return Err(snapshot_auto_state_changed());
                    }
                    return Err(error);
                }
            }
            Some(observation) => replace_snapshot_state_expected(
                &self.kio,
                SNAPSHOT_AUTO_STATE_LEAF,
                &temporary,
                &bytes,
                observation,
            )?,
        }
        sync_bound_directory(&self.kio, SNAPSHOT_AUTO_STATE_LEAF)?;
        let published = self.snapshot_auto_state()?;
        if published.state.as_ref() != Some(&state) {
            return Err(corrupt("snapshot auto state changed during publication"));
        }
        Ok(published)
    }

    /// Establish the scheduler-state publication precondition while the
    /// caller owns the scheduler writer barrier.  In particular, malformed or
    /// substituted private checkpoint crash residue is rejected *before* a
    /// scheduled snapshot can publish any CAS object or ref.  A process that
    /// died after fsyncing a valid private checkpoint is recovered by removing
    /// that exact, descriptor-relative record.
    ///
    /// This deliberately does not publish the new checkpoint; callers use it
    /// before immutable snapshot preparation and then call
    /// [`Self::publish_snapshot_auto_state`] at the pre-ref prepared-attempt
    /// boundary.
    pub fn prepare_snapshot_auto_state_publication(
        &self,
        expected: &SnapshotAutoStateBinding,
    ) -> Result<()> {
        self.recheck_binding()?;
        if &self.snapshot_auto_state()? != expected {
            return Err(snapshot_auto_state_changed());
        }
        cleanup_snapshot_auto_state_temporaries(&self.kio)?;
        // Cleanup has no reason to modify the public state leaf.  Re-observe
        // it anyway so a competing namespace writer cannot race the cleanup
        // precondition and leave a commit published before its checkpoint
        // conflict is discovered.
        if &self.snapshot_auto_state()? != expected {
            return Err(snapshot_auto_state_changed());
        }
        self.recheck_binding()
    }

    /// Retained scope-root capability for the scheduler's direct-file scan.
    pub fn retained_scope_handle(&self) -> Result<std::fs::File> {
        self.recheck_binding()?;
        self.scope
            .try_clone()
            .map_err(|error| ioerr(error, "scope"))
    }

    /// Reject unsafe direct entries before a scheduled scan. Direct child
    /// directories remain outside this scope's working tree; symlinks,
    /// reparse points, hardlinks and other special leaves are never silently
    /// interpreted as document inputs.
    pub fn validate_snapshot_auto_direct_entries(&self) -> Result<BTreeSet<String>> {
        self.recheck_binding()?;
        let names = validated_snapshot_auto_direct_entries(&self.scope)?;
        self.recheck_binding()?;
        Ok(names)
    }
    pub fn ensure_index_rotation_supported(&self) -> Result<()> {
        self.recheck_binding()?;
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            return Err(corrupt("GC index rotation is unsupported on this platform"));
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            Ok(())
        }
    }
    /// Mint the process-local capability required by
    /// [`Self::remove_candidate_tree`].
    ///
    /// # Safety
    ///
    /// Immediately before this call, the caller must descriptor-bind the
    /// public SQLite leaf and verify its strict durable GC attestation against
    /// this marker's sweep ID, plan digest, initial generation, pre-sweep
    /// generation, physical identity, and rotation role.  `kio-index` is the
    /// trusted coordinator that performs that verification; marker fields
    /// alone are not sufficient.
    pub unsafe fn authorize_tree_removal_after_index_attestation(
        &self,
        marker: &GcInProgressMarker,
        attested_index_file: Option<&std::fs::File>,
    ) -> Result<GcTreeRemovalPermit> {
        self.recheck_binding()?;
        if self.read_marker()?.as_ref() != Some(marker) {
            return Err(corrupt("GC marker changed before index authorization"));
        }
        let (index_file, index_state) = match (marker.index_pre_sweep.as_ref(), attested_index_file)
        {
            (Some(GcIndexState::Absent), None) => (None, None),
            (Some(GcIndexState::Present { identity, .. }), Some(file)) => {
                let metadata = cap_fs::Metadata::from_file(file)
                    .map_err(|error| ioerr(error, "index/sqlite.db"))?;
                valid_file(&metadata, u64::MAX)?;
                if canonical_gc_index_identity_from_metadata(&metadata)? != *identity {
                    return Err(corrupt("attested GC index identity differs from marker"));
                }
                (
                    Some(
                        file.try_clone()
                            .map_err(|error| ioerr(error, "index/sqlite.db"))?,
                    ),
                    Some(file_state(&metadata)),
                )
            }
            _ => return Err(corrupt("GC index attestation handle/state mismatch")),
        };
        Ok(GcTreeRemovalPermit {
            kio_identity: id_file(&self.kio)?,
            marker_digest: hash_bytes(&marker.canonical_bytes()?),
            index_file,
            index_state,
        })
    }
    /// Re-plan through the same retained scope/.kio descriptors used for a
    /// subsequent mutation, closing the public-path rebind gap after locking.
    pub fn plan_at(&self, now: i64) -> Result<GcPlan> {
        self.recheck_binding()?;
        let plan = self.bound_planner()?.plan_at(now)?;
        self.recheck_binding()?;
        Ok(plan)
    }
    /// Validate the marker phase against exact durable receipts and tree
    /// presence. Impossible combinations are corruption, not resumable state.
    pub fn validate_recovery_state(&self, marker: &GcInProgressMarker) -> Result<GcSweepProgress> {
        if self.read_marker()?.as_ref() != Some(marker) {
            return Err(corrupt("GC marker changed during recovery validation"));
        }
        let receipts = read_shallow_receipts_bound(&self.kio)?;
        let frozen: BTreeMap<_, _> = marker
            .candidates
            .iter()
            .map(|candidate| (&candidate.commit_hash, &candidate.tree_hash))
            .collect();
        let progress = self.validate_marker_phase_state(marker, &receipts, &frozen)?;
        if let Some(expected) = &marker.operation_receipts_digest {
            let actual = operation_receipt_observation_digest_bound(&self.kio, marker)?;
            if &actual != expected {
                return Err(corrupt("marker-owned receipt identity/content changed"));
            }
        }
        Ok(progress)
    }
    /// Structural phase validation for fsck/read paths. It intentionally does
    /// not compare live baseline identity digests, which are an executor-only
    /// authorization check performed by `validate_frozen_marker_current_truth`.
    pub fn validate_marker_phase_state(
        &self,
        marker: &GcInProgressMarker,
        receipts: &BTreeMap<String, ShallowReceipt>,
        frozen: &BTreeMap<&String, &String>,
    ) -> Result<GcSweepProgress> {
        let mut receipt_count = 0;
        for (commit, receipt) in receipts {
            if let Some(tree) = frozen.get(commit) {
                let expected = ShallowReceipt::new(
                    (*commit).clone(),
                    (**tree).clone(),
                    marker.started_at.clone(),
                )?;
                if receipt != &expected {
                    return Err(corrupt("recovery receipt tree differs from frozen marker"));
                }
                receipt_count += 1;
            }
        }
        let (any_tree_missing, all_trees_missing) = self.marker_tree_presence(marker)?;
        let progress = GcSweepProgress {
            receipt_count,
            receipts_complete: receipt_count == marker.candidates.len(),
            any_tree_missing,
            all_trees_missing,
        };
        // Tree disappearance is irreversible.  It is never a valid durable
        // state until the pre-sweep index generation has been committed.
        if any_tree_missing && marker.index_pre_sweep.is_none() {
            return Err(corrupt(
                "GC tree is missing before durable pre-sweep index rotation",
            ));
        }
        if any_tree_missing {
            self.require_live_pre_sweep_index_binding(marker)?;
        }
        let all_trees_present = !any_tree_missing;
        let index_final_is_none = marker.index_final.is_none();
        let valid = match marker.phase {
            GcSweepPhase::Prepared => {
                receipt_count == 0 && all_trees_present && index_final_is_none
            }
            GcSweepPhase::Receipting => all_trees_present && index_final_is_none,
            GcSweepPhase::Sweeping => progress.receipts_complete && index_final_is_none,
            // A crash immediately after persisting `finalizing` but before the
            // caller rotates the index is resumable. `Some` records a completed
            // final rotation; both forms require the physical sweep complete.
            GcSweepPhase::Finalizing => progress.receipts_complete && all_trees_missing,
        };
        if !valid {
            return Err(corrupt(
                "GC marker phase contradicts durable receipt/tree/index state",
            ));
        }
        Ok(progress)
    }
    /// Alias kept intentionally terse for orchestrators deciding whether an
    /// intact marker can be discarded after a fresh locked preview.
    pub fn progress(&self, marker: &GcInProgressMarker) -> Result<GcSweepProgress> {
        self.validate_recovery_state(marker)
    }
    /// True only before any irreversible receipt/tree work. A caller may remove
    /// this marker after a fresh locked plan differs; all other states retain
    /// their receipts and must either resume or fail closed.
    pub fn marker_can_be_discarded_after_fresh_replan(
        &self,
        marker: &GcInProgressMarker,
    ) -> Result<bool> {
        let progress = self.validate_recovery_state(marker)?;
        Ok(progress.receipt_count == 0 && !progress.any_tree_missing)
    }
    /// Re-read refs, commits and sharing from the currently bound store.  This
    /// never trusts the marker's old truth digest as permission to continue.
    /// It permits progress that the marker itself caused (new matching receipts)
    /// but rejects a new ref or non-frozen commit sharing a victim tree.
    pub fn validate_frozen_marker_current_truth(&self, marker: &GcInProgressMarker) -> Result<()> {
        let _ = self.validate_recovery_state(marker)?;
        let planner = self.bound_planner()?;
        let mut stats = GcPlanStats::default();
        let mut observations = BTreeMap::new();
        let scope_bytes = read_regular_observed(&self.kio, "scope.json", MAX_METADATA)?;
        observations.insert("scope.json".into(), scope_bytes.1.clone());
        let config_bytes = read_regular_observed(&self.kio, "config.toml", MAX_METADATA)?;
        observations.insert("config.toml".into(), config_bytes.1.clone());
        let refs = planner.read_refs(&mut stats, &mut observations)?;
        let commits = planner.inventory_commits(&mut stats, &mut observations)?;
        validate_commit_links(&commits)?;
        let scope_digest = validate_scope_bytes(&scope_bytes.0)?;
        let (policy, config_digest) = read_policy_bytes(&config_bytes.0)?;
        if semantic_stable_truth_digest(
            &scope_digest,
            &config_digest,
            &policy,
            &refs,
            &commits,
            &observations,
        )? != marker.stable_truth_digest
        {
            return Err(corrupt("immutable GC recovery truth changed"));
        }
        let frozen: BTreeMap<_, _> = marker
            .candidates
            .iter()
            .map(|candidate| (&candidate.commit_hash, &candidate.tree_hash))
            .collect();
        if read_receipt_observation_digest_bound(&self.kio, &frozen)?
            != marker.baseline_receipts_digest
        {
            return Err(corrupt(
                "non-operation shallow receipts changed during recovery",
            ));
        }
        for (commit, tree) in &frozen {
            let current = commits
                .get(*commit)
                .ok_or_else(|| corrupt("frozen GC commit is missing"))?;
            if &current.tree != *tree {
                return Err(corrupt("frozen GC commit tree changed"));
            }
        }
        if refs.values().any(|tip| frozen.contains_key(tip)) {
            return Err(corrupt("current ref points to a frozen shallow candidate"));
        }
        if read_receipt_observation_digest_bound(&self.kio, &frozen)?
            != marker.baseline_receipts_digest
        {
            return Err(corrupt(
                "non-operation shallow receipts changed during recovery",
            ));
        }
        for tree in &marker.trees {
            for (hash, commit) in &commits {
                if &commit.tree == tree && !frozen.contains_key(hash) {
                    return Err(corrupt("non-frozen commit shares GC victim tree"));
                }
            }
        }
        // A marker is recovery state, never an authorization token.  In
        // particular, do not merely check that its old digests still look
        // plausible: a same-user writer could manufacture a canonical marker
        // and matching receipts for a protected tree.  Re-run the retention
        // selection from the current retained config/refs/commit graph and
        // require its *entire* frozen pair set to match.  Marker-owned
        // receipts are deliberately treated as virtual pre-sweep state, so a
        // legitimate crash after receipt publication (or tree removal) can
        // resume; all other receipts remain already-shallow input.
        let current_receipts = read_shallow_receipts_bound(&self.kio)?;
        let mut baseline_receipts = HashMap::new();
        for (commit, receipt) in current_receipts {
            if !frozen.contains_key(&commit) {
                baseline_receipts.insert(commit, receipt.tree_hash);
            }
        }
        validate_receipt_links(&baseline_receipts, &commits)?;
        let mut eligibility_stats = GcPlanStats::default();
        let mut exclusions = BTreeMap::new();
        let expected = retention_candidate_pairs(
            &policy,
            &refs,
            &commits,
            &baseline_receipts,
            parse_utc_seconds(&marker.started_at)
                .ok_or_else(|| corrupt("invalid marker timestamp"))?,
            &mut eligibility_stats,
            &planner.limits,
            &mut exclusions,
        )?;
        let actual: Vec<_> = marker
            .candidates
            .iter()
            .map(|candidate| (candidate.commit_hash.clone(), candidate.tree_hash.clone()))
            .collect();
        if actual != expected {
            return Err(corrupt(
                "GC marker candidates are not exactly authorized by current retention policy",
            ));
        }
        Ok(())
    }
    pub fn publish_marker(&self, marker: &GcInProgressMarker) -> Result<()> {
        self.recheck_binding()?;
        self.ensure_no_pending_managed_restore_under_lock()?;
        let gc = ensure_child_dir(&self.kio, "gc")?;
        let internal = ensure_child_dir(&gc, "internal")?;
        let markers = ensure_child_dir(&internal, "markers")?;
        let staged = unique_internal_name(&format!("prepared-{}", marker.sweep_id));
        let bytes = marker.canonical_bytes()?;
        // Never expose a partially written public marker. The private stage is
        // create-new, fully written and fsynced before a no-clobber atomic
        // rename publishes `in_progress` in one namespace operation.
        create_new_bound(&markers, &staged, &bytes, MAX_MARKER_BYTES)?;
        self.inject_gc_tree_fault(GcTreeFaultPoint::MarkerStageFsync)?;
        rename_noreplace_between(&markers, &staged, &gc, "in_progress")?;
        sync_bound_directory(&gc, "in_progress")?;
        sync_bound_directory(&markers, &staged)?;
        let (published, _) = read_regular_observed(&gc, "in_progress", MAX_MARKER_BYTES)?;
        if published != bytes || GcInProgressMarker::parse_canonical(&published)? != *marker {
            return Err(corrupt("GC marker changed during atomic publication"));
        }
        Ok(())
    }
    pub fn advance_marker(&self, marker: &GcInProgressMarker) -> Result<()> {
        self.recheck_binding()?;
        self.ensure_no_pending_managed_restore_under_lock()?;
        let (current, observed) = self.read_marker_observed()?;
        if current.sweep_id != marker.sweep_id
            || phase_rank(&marker.phase) < phase_rank(&current.phase)
            || phase_rank(&marker.phase) > phase_rank(&current.phase) + 1
            || current.started_at != marker.started_at
            || current.plan_digest != marker.plan_digest
            || current.truth_digest != marker.truth_digest
            || current.stable_truth_digest != marker.stable_truth_digest
            || current.baseline_receipts_digest != marker.baseline_receipts_digest
            || (current.operation_receipts_digest != marker.operation_receipts_digest
                && !(current.phase == GcSweepPhase::Receipting
                    && marker.phase == GcSweepPhase::Sweeping
                    && current.operation_receipts_digest.is_none()
                    && marker.operation_receipts_digest.is_some()))
            || current.candidates != marker.candidates
            || current.trees != marker.trees
            || current.estimated_bytes != marker.estimated_bytes
            || current.index_initial != marker.index_initial
            || !valid_index_marker_delta(&current, marker)
        {
            return Err(corrupt("invalid GC marker transition"));
        }
        self.write_marker_expected(marker, &observed)
    }
    /// Bind every marker-owned receipt observation immediately before the
    /// caller advances Receipting -> Sweeping.
    pub fn bind_operation_receipts(
        &self,
        marker: &GcInProgressMarker,
    ) -> Result<GcInProgressMarker> {
        if marker.phase != GcSweepPhase::Receipting || marker.operation_receipts_digest.is_some() {
            return Err(corrupt(
                "operation receipt binding is only valid in receipting phase",
            ));
        }
        let mut bound = marker.clone();
        bound.operation_receipts_digest = Some(operation_receipt_observation_digest_bound(
            &self.kio, marker,
        )?);
        Ok(bound)
    }
    fn write_marker_expected(
        &self,
        marker: &GcInProgressMarker,
        expected: &FileObservation,
    ) -> Result<()> {
        let gc = ensure_child_dir(&self.kio, "gc")?;
        let internal = ensure_child_dir(&gc, "internal")?;
        let markers = ensure_child_dir(&internal, "markers")?;
        atomic_exchange_marker_expected(
            &gc,
            "in_progress",
            &markers,
            &marker.canonical_bytes()?,
            expected,
        )
    }
    pub fn create_receipt(
        &self,
        candidate: &GcCandidate,
        at: String,
    ) -> Result<GcReceiptPublication> {
        self.recheck_binding()?;
        self.ensure_no_pending_managed_restore_under_lock()?;
        if !is_hash(&candidate.commit_hash) || !is_hash(&candidate.tree_hash) {
            return Err(corrupt("invalid GC candidate"));
        }
        let marker = self
            .read_marker()?
            .ok_or_else(|| corrupt("GC marker is missing"))?;
        if !marker.is_frozen_pair(&candidate.commit_hash, &candidate.tree_hash) {
            return Err(corrupt("candidate is not in frozen GC marker"));
        }
        // Marker-owned receipts are fully deterministic; callers cannot select
        // an alternate timestamp and later claim it belongs to this operation.
        let receipt = ShallowReceipt::new(
            candidate.commit_hash.clone(),
            candidate.tree_hash.clone(),
            marker.started_at.clone(),
        )?;
        if at != marker.started_at {
            return Err(corrupt("GC receipt timestamp differs from marker start"));
        }
        let gc = ensure_child_dir(&self.kio, "gc")?;
        let shallowed = ensure_child_dir(&gc, "shallowed")?;
        let leaf = &candidate.commit_hash["sha256:".len()..];
        match read_regular_observed(&shallowed, leaf, MAX_METADATA) {
            Ok((existing, observed)) => {
                if ShallowReceipt::parse_canonical(&existing, leaf)? != receipt {
                    return Err(corrupt(
                        "existing shallow receipt differs from frozen candidate",
                    ));
                }
                // An exact pre-existing receipt is only an authorization once
                // its own retained handle and parent directory have been
                // forced durable.  Merely re-reading the pathname leaves a
                // crash window in which this invocation can start removing a
                // tree while the receipt has not reached stable storage.
                let durable = open_verified_file_handle(
                    &shallowed,
                    leaf,
                    &observed,
                    MAX_METADATA,
                    "existing GC shallow receipt",
                )?;
                durable.sync_all().map_err(|error| ioerr(error, leaf))?;
                sync_bound_directory(&shallowed, leaf)?;
                Ok(GcReceiptPublication::AlreadyPresent)
            }
            Err(error) if is_io_not_found(&error) => {
                let internal = ensure_child_dir(&gc, "internal")?;
                let receipts = ensure_child_dir(&internal, "receipts")?;
                let staged = unique_internal_name(&format!("{}-{}", marker.sweep_id, leaf));
                let bytes = receipt.canonical_bytes()?;
                // As with the operation marker, stage and fsync the complete
                // record before its final receipt name exists. A crash during
                // staging leaves no malformed authorization at `shallowed/`.
                create_new_bound(&receipts, &staged, &bytes, MAX_METADATA)?;
                self.inject_gc_tree_fault(GcTreeFaultPoint::ReceiptStageFsync)?;
                rename_noreplace_between(&receipts, &staged, &shallowed, leaf)?;
                sync_bound_directory(&shallowed, leaf)?;
                sync_bound_directory(&receipts, &staged)?;
                let (published, _) = read_regular_observed(&shallowed, leaf, MAX_METADATA)?;
                if published != bytes
                    || ShallowReceipt::parse_canonical(&published, leaf)? != receipt
                {
                    return Err(corrupt("GC receipt changed during atomic publication"));
                }
                Ok(GcReceiptPublication::NewlyPublished)
            }
            Err(error) => Err(error),
        }
    }
    /// Remove one tree after every candidate referencing it has a durable exact
    /// receipt. The leaf is read/hashed/schema-checked from its retained parent
    /// descriptor immediately before capability-relative unlink.
    pub fn remove_candidate_tree(
        &self,
        permit: &GcTreeRemovalPermit,
        marker: &GcInProgressMarker,
        tree_hash: &str,
    ) -> Result<bool> {
        self.recheck_binding()?;
        self.ensure_no_pending_managed_restore_under_lock()?;
        self.validate_tree_removal_permit(permit, marker)?;
        if marker.phase != GcSweepPhase::Sweeping
            || marker.index_pre_sweep.is_none()
            || marker.index_rotation.is_some()
        {
            return Err(corrupt(
                "GC tree removal requires durable pre-sweep index rotation",
            ));
        }
        self.require_live_pre_sweep_index_binding(marker)?;
        // Revalidate immutable truth and non-operation receipts for every
        // physical victim, not merely once at resume entry.
        self.validate_frozen_marker_current_truth(marker)?;
        let active = self
            .read_marker()?
            .ok_or_else(|| corrupt("GC marker is missing"))?;
        if active != *marker || !marker.trees.iter().any(|tree| tree == tree_hash) {
            return Err(corrupt("GC marker changed before tree removal"));
        }
        let receipts = read_shallow_receipts_bound(&self.kio)?;
        for candidate in marker
            .candidates
            .iter()
            .filter(|c| c.tree_hash == tree_hash)
        {
            let receipt = receipts
                .get(&candidate.commit_hash)
                .ok_or_else(|| corrupt("tree removal before every shared-tree receipt"))?;
            if receipt.tree_hash != tree_hash {
                return Err(corrupt("receipt tree does not match frozen candidate"));
            }
        }
        // Bind every frozen receipt used to authorize this victim twice through
        // retained descriptors. This rejects a replace/symlink/hardlink race
        // between recovery validation and the destructive move.
        self.recheck_tree_receipts(marker, tree_hash)?;
        let planner = self.bound_planner()?;
        let mut stats = GcPlanStats::default();
        let mut observations = BTreeMap::new();
        let refs = planner.read_refs(&mut stats, &mut observations)?;
        let commits = planner.inventory_commits(&mut stats, &mut observations)?;
        validate_commit_links(&commits)?;
        if refs.values().any(|tip| {
            commits
                .get(tip)
                .is_some_and(|commit| commit.tree == tree_hash)
        }) {
            return Err(corrupt(
                "current ref commit still references GC victim tree",
            ));
        }
        for (commit_hash, commit) in &commits {
            if commit.tree == tree_hash && !marker.is_frozen_pair(commit_hash, tree_hash) {
                return Err(corrupt("retained or new commit shares GC victim tree"));
            }
        }
        let raw = tree_hash
            .strip_prefix("sha256:")
            .filter(|x| x.len() == 64 && hex(x))
            .ok_or_else(|| corrupt("invalid tree hash"))?;
        let trees = open_path(&self.kio, "objects/trees")?;
        let a = open_required_dir(&trees, &raw[..2], "tree object fanout is missing")?;
        let d = open_required_dir(&a, &raw[2..4], "tree object fanout is missing")?;
        let gc = ensure_child_dir(&self.kio, "gc")?;
        let internal = ensure_child_dir(&gc, "internal")?;
        let archive_dir = ensure_child_dir(&internal, "trees")?;
        let quarantine = format!("{}-{}", marker.sweep_id, raw);
        let captured_name = tree_capture_name(marker, raw);

        // Either start from the canonical CAS leaf or resume the exact
        // operation-owned quarantine left by a crash after the durable rename.
        // Both paths bind a no-follow writable descriptor before the archive
        // name is removed; no ambient pathname is ever used for deletion.
        let canonical = read_regular_observed(&d, raw, MAX_TREE_OBJECT_BYTES);
        let (bytes, before, writable, already_captured) = match canonical {
            Ok((bytes, before)) => {
                self.validate_committed_tree_bytes(marker, tree_hash, &bytes)?;
                let writable = open_gc_tree_writable(&d, raw, &before)?;
                // A pre-existing operation archive together with the canonical
                // leaf is not a resumable state and must never be overwritten.
                if read_regular_observed(&archive_dir, &quarantine, MAX_TREE_OBJECT_BYTES).is_ok() {
                    return Err(corrupt("GC tree exists at canonical and archive paths"));
                }
                self.require_active_marker(marker)?;
                let (final_bytes, final_observation) =
                    read_regular_observed(&d, raw, MAX_TREE_OBJECT_BYTES)?;
                if final_observation != before || final_bytes != bytes {
                    return Err(corrupt("tree changed immediately before GC quarantine"));
                }
                self.wait_at_gc_pre_quarantine_barrier();
                // The index generation/identity is part of the irreversible
                // deletion authority, not merely an executor preflight.  Bind
                // it again at the final namespace transition after the longer
                // ref/receipt/tree verification above.
                self.validate_tree_removal_permit(permit, marker)?;
                self.require_live_pre_sweep_index_binding(marker)?;
                rename_noreplace_between(&d, raw, &archive_dir, &quarantine)?;
                // Persist disappearance from CAS and appearance in quarantine
                // before any crash seam or byte reclamation.
                sync_bound_directory(&archive_dir, &quarantine)?;
                sync_bound_directory(&d, raw)?;
                (bytes, before, writable, false)
            }
            Err(error) if is_io_not_found(&error) => {
                let (bytes, before, already_captured) =
                    match read_regular_observed(&archive_dir, &quarantine, MAX_TREE_OBJECT_BYTES) {
                        Ok((sentinel, _)) if sentinel == GC_RETIRE_SENTINEL => {
                            match read_regular_observed(
                                &archive_dir,
                                &captured_name,
                                MAX_TREE_OBJECT_BYTES,
                            ) {
                                Ok((captured, observed)) => (captured, observed, true),
                                Err(error) if is_io_not_found(&error) => {
                                    // The captured tree was already unlinked
                                    // before a crash. Its sentinel is a durable,
                                    // deterministic completion record; remove it
                                    // only after the fresh marker binding above.
                                    self.require_active_marker(marker)?;
                                    remove_sentinel_leaf(&archive_dir, &quarantine)?;
                                    sync_bound_directory(&archive_dir, &quarantine)?;
                                    return Ok(true);
                                }
                                Err(error) => return Err(error),
                            }
                        }
                        Ok(value) => (value.0, value.1, false),
                        Err(error) if is_io_not_found(&error) => return Ok(false),
                        Err(error) => return Err(error),
                    };
                self.validate_committed_tree_bytes(marker, tree_hash, &bytes)?;
                let writable = open_gc_tree_writable(
                    &archive_dir,
                    if already_captured {
                        &captured_name
                    } else {
                        &quarantine
                    },
                    &before,
                )?;
                (bytes, before, writable, already_captured)
            }
            Err(error) => return Err(error),
        };

        let quarantined = read_regular_observed(
            &archive_dir,
            if already_captured {
                &captured_name
            } else {
                &quarantine
            },
            MAX_TREE_OBJECT_BYTES,
        );
        // Rename legitimately updates ctime, so only identity + byte length and
        // hash are stable across the quarantine transition.
        let valid_quarantine = quarantined.as_ref().is_ok_and(|(bytes, observation)| {
            hash_bytes(bytes) == tree_hash
                && observation.identity == before.identity
                && observation.state.len == before.state.len
        });
        if !valid_quarantine {
            // Do not restore: a replacement raw leaf could have appeared after
            // the no-replace move. Leaving the quarantined object is safe and
            // forces explicit recovery rather than risking an overwrite.
            return Err(corrupt("tree changed during GC quarantine"));
        }
        let handle_meta =
            cap_fs::Metadata::from_file(&writable).map_err(|error| ioerr(error, raw))?;
        if id_meta(&handle_meta)? != before.identity || handle_meta.len() != before.state.len {
            return Err(corrupt("tree writable handle changed during quarantine"));
        }
        if hash_bytes(&bytes) != tree_hash {
            return Err(corrupt("tree bytes changed during GC quarantine"));
        }
        self.inject_gc_tree_fault(GcTreeFaultPoint::TreeQuarantine)?;
        self.wait_at_gc_tree_quarantine_barrier();
        // The test seam is deliberately before this final check. Rebind both
        // authorities immediately before capture: a marker replacement must
        // stop the erase, and a renamed archive victim is captured under a
        // fresh private name then compared before any unlink occurs.
        self.require_active_marker(marker)?;
        let erase_leaf = if already_captured {
            &captured_name
        } else {
            &quarantine
        };
        let (final_bytes, final_observation) =
            read_regular_observed(&archive_dir, erase_leaf, MAX_TREE_OBJECT_BYTES)?;
        if final_observation.identity != before.identity
            || final_observation.state.len != before.state.len
            || final_observation.digest != hash_bytes(&bytes)
            || final_bytes != bytes
        {
            return Err(corrupt("tree changed immediately before GC erase"));
        }
        let (captured, captured_observation) = if already_captured {
            (captured_name, final_observation)
        } else {
            exchange_capture_verified_named(
                &archive_dir,
                &quarantine,
                &captured_name,
                &final_observation,
                MAX_TREE_OBJECT_BYTES,
                "GC tree archive",
            )?
        };
        self.inject_gc_tree_fault(GcTreeFaultPoint::TreeRetirementCapture)?;
        // Unlink only the private captured name, then require the retained
        // object to have no remaining names before truncating it. A same-UID
        // hardlink inserted at any point leaves nlink > 0 and aborts without
        // modifying bytes owned by another pathname.
        remove_verified_leaf(
            &archive_dir,
            &captured,
            &captured_observation,
            MAX_TREE_OBJECT_BYTES,
            "GC captured tree archive",
        )?;
        remove_sentinel_leaf(&archive_dir, &quarantine)?;
        sync_bound_directory(&archive_dir, &quarantine)?;
        let unlinked = cap_fs::Metadata::from_file(&writable).map_err(|error| ioerr(error, raw))?;
        if id_meta(&unlinked)? != before.identity || link_count(&unlinked)? != 0 {
            return Err(corrupt("tree archive gained a hardlink before byte erase"));
        }
        writable.set_len(0).map_err(|error| ioerr(error, raw))?;
        writable.sync_all().map_err(|error| ioerr(error, raw))?;
        let erased = cap_fs::Metadata::from_file(&writable).map_err(|error| ioerr(error, raw))?;
        if id_meta(&erased)? != before.identity || erased.len() != 0 || link_count(&erased)? != 0 {
            return Err(corrupt(
                "tree archive truncate did not bind expected object",
            ));
        }
        sync_bound_directory(&d, raw)?;
        Ok(true)
    }
    pub fn remove_marker(&self, expected: &GcInProgressMarker) -> Result<()> {
        self.recheck_binding()?;
        self.ensure_no_pending_managed_restore_under_lock()?;
        let (current, observation) = self.read_marker_observed()?;
        if &current != expected {
            return Err(corrupt("GC marker changed before finalization"));
        }
        let gc = open_required_dir(&self.kio, "gc", "GC directory is missing")?;
        let internal = ensure_child_dir(&gc, "internal")?;
        let markers = ensure_child_dir(&internal, "markers")?;
        let archive = unique_internal_name("completed");
        rename_noreplace_between(&gc, "in_progress", &markers, &archive)?;
        sync_bound_directory(&markers, &archive)?;
        // Commit the source disappearance before retiring the destination,
        // otherwise a power loss could resurrect the public marker after the
        // internal archive has been unlinked.
        sync_bound_directory(&gc, "gc")?;
        let (moved_bytes, moved) = read_regular_observed(&markers, &archive, MAX_MARKER_BYTES)?;
        if moved.identity != observation.identity
            || moved.state.len != observation.state.len
            || moved.digest != observation.digest
            || GcInProgressMarker::parse_canonical(&moved_bytes).is_err()
        {
            // Never roll back after a failed identity check: a second move
            // could restore a hostile replacement into the public marker
            // name. Preserve the archived entry for fail-closed diagnosis.
            return Err(corrupt("GC marker changed during completion archive"));
        }
        let moved_handle = open_verified_file_handle(
            &markers,
            &archive,
            &moved,
            MAX_MARKER_BYTES,
            "GC completed marker",
        )?;
        // The archive name is only a capability-relative quarantine used to
        // prove that the exact active marker left the public name. Remove it
        // in the same completion step so successful sweeps do not retain an
        // unbounded marker history. If another name was added, the retained
        // handle exposes nlink != 0 and completion fails without changing the
        // bytes reachable through that other name.
        // Retire through an atomic exchange so a replacement at the archive
        // name is captured and identity-checked before any unlink. A foreign
        // marker is retained on mismatch rather than deleted.
        let (captured, captured_observation) = exchange_capture_verified(
            &markers,
            &archive,
            &moved,
            MAX_MARKER_BYTES,
            "GC completed marker",
        )?;
        remove_verified_leaf(
            &markers,
            &captured,
            &captured_observation,
            MAX_MARKER_BYTES,
            "GC captured completed marker",
        )?;
        remove_sentinel_leaf(&markers, &archive)?;
        sync_bound_directory(&markers, &archive)?;
        let unlinked =
            cap_fs::Metadata::from_file(&moved_handle).map_err(|error| ioerr(error, &archive))?;
        if id_meta(&unlinked)? != moved.identity || link_count(&unlinked)? != 0 {
            return Err(corrupt("GC completed marker gained another name"));
        }
        sync_bound_directory(&gc, "gc")
    }
    /// Check that the caller's public scope pathname still names exactly the
    /// retained capability handles. Invoke around any external index operation.
    pub fn assert_public_identity(&self) -> Result<()> {
        self.recheck_binding()
    }
    /// Retained `.kio` capability for the index-generation coordinator. The
    /// caller must not use it for object deletion; this accessor exists solely
    /// to avoid reopening the public scope pathname around SQLite rotation.
    pub fn retained_kio_handle(&self) -> Result<std::fs::File> {
        self.recheck_binding()?;
        self.kio.try_clone().map_err(|error| ioerr(error, "kio"))
    }
    fn bound_planner(&self) -> Result<GcPlanner> {
        Ok(GcPlanner {
            root: self.root.clone(),
            scope: self.scope.try_clone().map_err(|e| ioerr(e, "scope"))?,
            kio: self.kio.try_clone().map_err(|e| ioerr(e, "kio"))?,
            limits: GcPlanLimits::default(),
        })
    }
    fn recheck_binding(&self) -> Result<()> {
        if id_file(&self.scope)? != id_path(&self.root)?
            || id_file(&self.kio)? != id_child(&self.scope, ".kio")?
        {
            Err(corrupt("scope changed during GC sweep"))
        } else {
            Ok(())
        }
    }
    fn read_marker_observed(&self) -> Result<(GcInProgressMarker, FileObservation)> {
        let gc = open_required_dir(&self.kio, "gc", "GC directory is missing")?;
        let (bytes, observed) = read_regular_observed(&gc, "in_progress", MAX_MARKER_BYTES)?;
        Ok((GcInProgressMarker::parse_canonical(&bytes)?, observed))
    }
    fn require_active_marker(&self, expected: &GcInProgressMarker) -> Result<FileObservation> {
        let (actual, observed) = self.read_marker_observed()?;
        if &actual != expected {
            return Err(corrupt(
                "GC marker changed immediately before tree mutation",
            ));
        }
        Ok(observed)
    }
    fn marker_tree_presence(&self, marker: &GcInProgressMarker) -> Result<(bool, bool)> {
        let trees = open_path(&self.kio, "objects/trees")?;
        let mut any_missing = false;
        let mut all_missing = true;
        for tree_hash in &marker.trees {
            let raw = tree_hash
                .strip_prefix("sha256:")
                .filter(|value| value.len() == 64 && hex(value))
                .ok_or_else(|| corrupt("invalid marker tree hash"))?;
            let canonical = match open_optional_dir(&trees, &raw[..2])? {
                None => false,
                Some(a) => match open_optional_dir(&a, &raw[2..4])? {
                    None => false,
                    Some(dir) => match read_regular_observed(&dir, raw, MAX_TREE_OBJECT_BYTES) {
                        Ok((bytes, _)) => {
                            self.validate_committed_tree_bytes(marker, tree_hash, &bytes)?;
                            true
                        }
                        Err(error) if is_io_not_found(&error) => false,
                        Err(error) => return Err(error),
                    },
                },
            };
            let archive = format!("{}-{raw}", marker.sweep_id);
            let captured_name = tree_capture_name(marker, raw);
            // Keep each directory capability alive through the complete read.  In
            // Rust 2021, the nested `if let` scrutinee temporaries lived until the
            // end of this initializer; make that lifetime explicit for Rust 2024.
            let (archived, captured) = {
                let gc = open_optional_dir(&self.kio, "gc")?;
                let internal = match gc.as_ref() {
                    Some(gc) => open_optional_dir(gc, "internal")?,
                    None => None,
                };
                let archives = match internal.as_ref() {
                    Some(internal) => open_optional_dir(internal, "trees")?,
                    None => None,
                };
                match archives.as_ref() {
                    Some(archives) => {
                        match read_regular_observed(archives, &archive, MAX_TREE_OBJECT_BYTES) {
                            Ok((sentinel, _)) if sentinel == GC_RETIRE_SENTINEL => {
                                match read_regular_observed(
                                    archives,
                                    &captured_name,
                                    MAX_TREE_OBJECT_BYTES,
                                ) {
                                    Ok((bytes, _)) => {
                                        self.validate_committed_tree_bytes(
                                            marker, tree_hash, &bytes,
                                        )?;
                                        (false, true)
                                    }
                                    Err(error) if is_io_not_found(&error) => (false, false),
                                    Err(error) => return Err(error),
                                }
                            }
                            Ok((bytes, _)) => {
                                self.validate_committed_tree_bytes(marker, tree_hash, &bytes)?;
                                (true, false)
                            }
                            Err(error) if is_io_not_found(&error) => (false, false),
                            Err(error) => return Err(error),
                        }
                    }
                    None => (false, false),
                }
            };
            if canonical && (archived || captured) {
                return Err(corrupt("GC tree exists at canonical and archive paths"));
            }
            let present = canonical || archived || captured;
            any_missing |= !present;
            all_missing &= !present;
        }
        Ok((any_missing, all_missing))
    }
    fn validate_committed_tree_bytes(
        &self,
        marker: &GcInProgressMarker,
        tree_hash: &str,
        bytes: &[u8],
    ) -> Result<()> {
        let expected = marker
            .candidates
            .iter()
            .find(|candidate| candidate.tree_hash == tree_hash)
            .map(|candidate| candidate.size_bytes)
            .ok_or_else(|| corrupt("marker tree has no candidate size"))?;
        if bytes.len() as u64 != expected {
            return Err(corrupt("GC tree length differs from frozen marker"));
        }
        validate_gc_tree_bytes(bytes, tree_hash)
    }
    /// Prove, using only the retained `.kio` capability, that the public index
    /// leaf names the latest durable state recorded by the marker: the final
    /// rotation when present, otherwise the mandatory pre-sweep rotation.
    /// Core does not interpret SQLite pages (that remains `kio-index`'s job),
    /// but it must not trust an unverified marker field at a deletion boundary.
    fn require_live_pre_sweep_index_binding(&self, marker: &GcInProgressMarker) -> Result<()> {
        let expected = marker
            .index_final
            .as_ref()
            .or(marker.index_pre_sweep.as_ref())
            .ok_or_else(|| corrupt("GC pre-sweep index state is missing"))?;
        let index = match open_optional_dir(&self.kio, "index")? {
            Some(index) => index,
            None if matches!(expected, GcIndexState::Absent) => return Ok(()),
            None => {
                return Err(corrupt(
                    "GC source index changed: index directory is missing",
                ));
            }
        };
        let mut options = cap_fs::OpenOptions::new();
        options
            .read(true)
            ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
        let leaf = Path::new("sqlite.db");
        let before = match cap_fs::stat(&index, leaf, cap_fs::FollowSymlinks::No) {
            Ok(metadata) => metadata,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && matches!(expected, GcIndexState::Absent) =>
            {
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(corrupt("GC source index changed: sqlite.db is missing"));
            }
            Err(error) => return Err(ioerr(error, "index/sqlite.db")),
        };
        valid_file(&before, u64::MAX)?;
        let file = match cap_fs::open(&index, Path::new("sqlite.db"), &options) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(corrupt("GC source index changed while opening"));
            }
            Err(error) => return Err(ioerr(error, "index/sqlite.db")),
        };
        let metadata =
            cap_fs::Metadata::from_file(&file).map_err(|error| ioerr(error, "index/sqlite.db"))?;
        valid_file(&metadata, u64::MAX)?;
        if !same_file_state(&before, &metadata)? {
            return Err(corrupt("GC source index changed while opening"));
        }
        let after = cap_fs::stat(&index, leaf, cap_fs::FollowSymlinks::No)
            .map_err(|error| ioerr(error, "index/sqlite.db"))?;
        valid_file(&after, u64::MAX)?;
        if !same_file_state(&after, &metadata)? {
            return Err(corrupt("GC source index changed while binding"));
        }
        let live = canonical_gc_index_identity_from_metadata(&metadata)?;
        let active_final_rotation_target = marker.index_rotation.as_ref().and_then(|rotation| {
            (rotation.role == GcIndexRotationRole::Final).then_some(&rotation.target)
        });
        match expected {
            GcIndexState::Absent => Err(corrupt(
                "GC marker records an absent pre-sweep index but sqlite.db exists",
            )),
            GcIndexState::Present { identity, .. }
                if identity == &live
                    || matches!(
                        active_final_rotation_target,
                        Some(GcIndexState::Present { identity, .. }) if identity == &live
                    ) =>
            {
                Ok(())
            }
            GcIndexState::Present { .. } => Err(corrupt("GC source index changed identity")),
        }
    }
    fn validate_tree_removal_permit(
        &self,
        permit: &GcTreeRemovalPermit,
        marker: &GcInProgressMarker,
    ) -> Result<()> {
        if permit.kio_identity != id_file(&self.kio)?
            || permit.marker_digest != hash_bytes(&marker.canonical_bytes()?)
        {
            return Err(corrupt("GC tree-removal permit does not bind marker/store"));
        }
        match (
            marker.index_pre_sweep.as_ref(),
            permit.index_file.as_ref(),
            permit.index_state.as_ref(),
        ) {
            (Some(GcIndexState::Absent), None, None) => Ok(()),
            (Some(GcIndexState::Present { identity, .. }), Some(file), Some(expected_state)) => {
                let metadata = cap_fs::Metadata::from_file(file)
                    .map_err(|error| ioerr(error, "index/sqlite.db"))?;
                valid_file(&metadata, u64::MAX)?;
                if canonical_gc_index_identity_from_metadata(&metadata)? != *identity
                    || file_state(&metadata) != *expected_state
                {
                    return Err(corrupt("attested GC index changed before tree removal"));
                }
                Ok(())
            }
            _ => Err(corrupt("GC tree-removal permit index state is invalid")),
        }
    }
    fn recheck_tree_receipts(&self, marker: &GcInProgressMarker, tree_hash: &str) -> Result<()> {
        if let Some(expected) = &marker.operation_receipts_digest
            && &operation_receipt_observation_digest_bound(&self.kio, marker)? != expected
        {
            return Err(corrupt(
                "marker-owned receipt identity/content changed before tree removal",
            ));
        }
        let gc = open_required_dir(&self.kio, "gc", "GC directory is missing")?;
        let shallow = open_required_dir(&gc, "shallowed", "GC receipt directory is missing")?;
        for candidate in marker
            .candidates
            .iter()
            .filter(|candidate| candidate.tree_hash == tree_hash)
        {
            let leaf = &candidate.commit_hash["sha256:".len()..];
            let (first, observed) = read_regular_observed(&shallow, leaf, MAX_METADATA)?;
            let receipt = ShallowReceipt::parse_canonical(&first, leaf)?;
            let expected = ShallowReceipt::new(
                candidate.commit_hash.clone(),
                candidate.tree_hash.clone(),
                marker.started_at.clone(),
            )?;
            if receipt != expected {
                return Err(corrupt("frozen receipt differs from candidate"));
            }
            let (second, recheck) = read_regular_observed(&shallow, leaf, MAX_METADATA)?;
            if observed != recheck || first != second {
                return Err(corrupt("frozen receipt changed before tree removal"));
            }
        }
        Ok(())
    }
}

impl SnapshotAutoBinding {
    /// Re-read the complete effective ignore authority through the retained
    /// scope and `.kio` capabilities.  This is intentionally an exact
    /// observation comparison rather than a semantic comparison: replacements
    /// and same-byte edits are authorization changes for scheduled writers.
    pub(crate) fn recheck(&self, scope: &std::fs::File, kio: &std::fs::File) -> Result<()> {
        if id_file(scope)? != self.scope_identity
            || id_file(kio)? != self.kio_identity
            || id_child(scope, ".kio")? != self.kio_identity
        {
            return Err(snapshot_auto_authority_changed());
        }
        let (config_bytes, config_observation) =
            read_regular_observed(kio, "config.toml", MAX_METADATA)?;
        // Parsing is deliberately repeated, so an invalid replacement is also
        // rejected at the publication boundary rather than silently retaining
        // the previously parsed policy.
        if read_snapshot_auto_config_bytes(&config_bytes)? != self.config
            || config_observation != self.config_observation
        {
            return Err(snapshot_auto_authority_changed());
        }
        let root_ignore_observation = match read_regular_observed(scope, ".kioignore", MAX_METADATA)
        {
            Ok((_, observation)) => Some(observation),
            Err(error) if is_io_not_found(&error) => None,
            Err(error) => return Err(error),
        };
        if root_ignore_observation != self.root_ignore_observation {
            return Err(snapshot_auto_authority_changed());
        }
        Ok(())
    }
}

impl SnapshotAutoStateBinding {
    /// Confirm that the checkpoint just published still names the exact
    /// durable state leaf before a scheduled ref can advance.
    pub(crate) fn recheck(&self, kio: &std::fs::File) -> Result<()> {
        let actual = match read_regular_observed(kio, SNAPSHOT_AUTO_STATE_LEAF, MAX_METADATA) {
            Ok((bytes, observation)) => Some((parse_snapshot_auto_state(&bytes)?, observation)),
            Err(error) if is_io_not_found(&error) => None,
            Err(error) => return Err(error),
        };
        let expected = self.state.as_ref().cloned().zip(self.observation.clone());
        if actual != expected {
            return Err(snapshot_auto_state_changed());
        }
        Ok(())
    }
}

/// Index rotation state is itself recovery authority.  Permit only publication
/// of one fully described rotation, retention of that exact record, or its
/// completion into the matching recorded target.  The absent-index fast path
/// has no private file and is correspondingly limited to its one state write.
fn valid_index_marker_delta(current: &GcInProgressMarker, next: &GcInProgressMarker) -> bool {
    if current.index_pre_sweep == next.index_pre_sweep
        && current.index_final == next.index_final
        && current.index_rotation == next.index_rotation
    {
        return true;
    }
    match (&current.index_rotation, &next.index_rotation) {
        (None, Some(rotation)) => {
            let unchanged = current.index_pre_sweep == next.index_pre_sweep
                && current.index_final == next.index_final;
            let source = match rotation.role {
                GcIndexRotationRole::PreSweep => Some(
                    current
                        .index_pre_sweep
                        .as_ref()
                        .unwrap_or(&current.index_initial),
                ),
                GcIndexRotationRole::Final => current
                    .index_final
                    .as_ref()
                    .or(current.index_pre_sweep.as_ref()),
            };
            unchanged
                && source == Some(&rotation.source)
                && matches!(
                    (&rotation.role, &next.phase),
                    (GcIndexRotationRole::PreSweep, GcSweepPhase::Sweeping)
                        | (GcIndexRotationRole::Final, GcSweepPhase::Finalizing)
                )
        }
        (Some(rotation), None) => match rotation.role {
            GcIndexRotationRole::PreSweep => {
                next.index_pre_sweep.as_ref() == Some(&rotation.target)
                    && next.index_final == current.index_final
            }
            GcIndexRotationRole::Final => {
                next.index_final.as_ref() == Some(&rotation.target)
                    && next.index_pre_sweep == current.index_pre_sweep
            }
        },
        (None, None) => {
            (current.phase == GcSweepPhase::Sweeping
                && next.phase == GcSweepPhase::Sweeping
                && current.index_pre_sweep.is_none()
                && next.index_pre_sweep == Some(GcIndexState::Absent)
                && next.index_final == current.index_final)
                || (current.phase == GcSweepPhase::Finalizing
                    && next.phase == GcSweepPhase::Finalizing
                    && current.index_final.is_none()
                    && next.index_final == Some(GcIndexState::Absent)
                    && next.index_pre_sweep == current.index_pre_sweep)
        }
        (Some(_), Some(_)) => false,
    }
}

fn phase_rank(phase: &GcSweepPhase) -> u8 {
    match phase {
        GcSweepPhase::Prepared => 0,
        GcSweepPhase::Receipting => 1,
        GcSweepPhase::Sweeping => 2,
        GcSweepPhase::Finalizing => 3,
    }
}

fn validate_gc_tree_bytes(bytes: &[u8], expected_hash: &str) -> Result<()> {
    if bytes.is_empty() || hash_bytes(bytes) != expected_hash {
        return Err(corrupt("tree hash changed before removal"));
    }
    let tree: TreeObject =
        serde_json::from_slice(bytes).map_err(|_| corrupt("invalid tree object before removal"))?;
    tree.validate()
        .map_err(|_| corrupt("invalid tree object before removal"))
}

fn open_gc_tree_writable(
    directory: &std::fs::File,
    leaf: &str,
    expected: &FileObservation,
) -> Result<std::fs::File> {
    let mut options = cap_fs::OpenOptions::new();
    options.read(true).write(true);
    options._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let file =
        cap_fs::open(directory, Path::new(leaf), &options).map_err(|error| ioerr(error, leaf))?;
    let metadata = cap_fs::Metadata::from_file(&file).map_err(|error| ioerr(error, leaf))?;
    valid_file(&metadata, MAX_TREE_OBJECT_BYTES)?;
    if id_meta(&metadata)? != expected.identity || metadata.len() != expected.state.len {
        return Err(corrupt("tree changed before writable handle bind"));
    }
    Ok(file)
}

fn open_verified_file_handle(
    directory: &std::fs::File,
    leaf: &str,
    expected: &FileObservation,
    max: u64,
    label: &str,
) -> Result<std::fs::File> {
    let mut options = cap_fs::OpenOptions::new();
    options.read(true);
    options._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let file =
        cap_fs::open(directory, Path::new(leaf), &options).map_err(|error| ioerr(error, leaf))?;
    let metadata = cap_fs::Metadata::from_file(&file).map_err(|error| ioerr(error, leaf))?;
    valid_file(&metadata, max)?;
    if id_meta(&metadata)? != expected.identity || metadata.len() != expected.state.len {
        return Err(corrupt(&format!("{label} changed while binding handle")));
    }
    Ok(file)
}

#[derive(Clone, Copy)]
enum GcTreeFaultPoint {
    MarkerStageFsync,
    ReceiptStageFsync,
    TreeQuarantine,
    TreeRetirementCapture,
}

impl GcTreeFaultPoint {
    #[cfg(debug_assertions)]
    fn matches(self, fault: crate::test_control::GcFault) -> bool {
        matches!(
            (self, fault),
            (
                Self::MarkerStageFsync,
                crate::test_control::GcFault::AfterMarkerStageFsync
            ) | (
                Self::ReceiptStageFsync,
                crate::test_control::GcFault::AfterReceiptStageFsync
            ) | (
                Self::TreeQuarantine,
                crate::test_control::GcFault::AfterTreeQuarantine
            ) | (
                Self::TreeRetirementCapture,
                crate::test_control::GcFault::AfterTreeRetirementCapture
            )
        )
    }

    #[cfg(debug_assertions)]
    fn label(self) -> &'static str {
        match self {
            Self::MarkerStageFsync => "after_marker_stage_fsync",
            Self::ReceiptStageFsync => "after_receipt_stage_fsync",
            Self::TreeQuarantine => "after_tree_quarantine",
            Self::TreeRetirementCapture => "after_tree_retirement_capture",
        }
    }
}

#[cfg(debug_assertions)]
fn wait_at_gc_test_barrier(ready_path: Option<&Path>) {
    let Some(ready_path) = ready_path else {
        return;
    };
    if std::fs::write(ready_path, b"ready").is_err() {
        return;
    }
    let release_path = ready_path.with_extension("release");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !release_path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn ensure_child_dir(parent: &std::fs::File, name: &str) -> Result<std::fs::File> {
    match open_optional_dir(parent, name)? {
        Some(dir) => Ok(dir),
        None => {
            let options = cap_fs::DirOptions::new();
            cap_fs::create_dir(parent, Path::new(name), &options).map_err(|e| ioerr(e, name))?;
            let dir = open_dir(parent, name)?;
            sync_bound_directory(parent, name)?;
            Ok(dir)
        }
    }
}
fn create_new_bound(dir: &std::fs::File, leaf: &str, bytes: &[u8], max: u64) -> Result<()> {
    let mut options = cap_fs::OpenOptions::new();
    options.write(true).create_new(true);
    options._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut file = cap_fs::open(dir, Path::new(leaf), &options).map_err(|e| ioerr(e, leaf))?;
    file.write_all(bytes).map_err(|e| ioerr(e, leaf))?;
    file.sync_all().map_err(|e| ioerr(e, leaf))?;
    drop(file);
    let (_, observed) = read_regular_observed(dir, leaf, max)?;
    if observed.digest != hash_bytes(bytes) {
        return Err(corrupt("receipt changed after create"));
    }
    sync_bound_directory(dir, leaf)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn rename_snapshot_state_noreplace(dir: &std::fs::File, from: &str, to: &str) -> Result<()> {
    rename_noreplace_bound(dir, from, to)
}

// `std::fs::rename` is no-clobber on Windows. `cap_fs::rename` retains the
// already-open directory as the path-resolution authority and therefore gives
// first publication the same destination-must-be-absent contract.
#[cfg(windows)]
fn rename_snapshot_state_noreplace(dir: &std::fs::File, from: &str, to: &str) -> Result<()> {
    cap_fs::rename(dir, Path::new(from), dir, Path::new(to)).map_err(|error| ioerr(error, to))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn rename_snapshot_state_noreplace(_: &std::fs::File, _: &str, _: &str) -> Result<()> {
    Err(corrupt(
        "platform lacks a verified no-replace snapshot state primitive",
    ))
}

/// Replace an exact observed scheduler checkpoint without an absent public
/// state window. Darwin/Linux retain the predecessor through atomic exchange,
/// validate that it is the expected inode/bytes, and only then retire it.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn replace_snapshot_state_expected(
    dir: &std::fs::File,
    leaf: &str,
    temporary: &str,
    replacement: &[u8],
    expected: &FileObservation,
) -> Result<()> {
    exchange_bound(dir, leaf, temporary)?;
    sync_bound_directory(dir, leaf)?;
    let (old_bytes, old) = read_regular_observed(dir, temporary, MAX_METADATA)?;
    let old_ok = old.identity == expected.identity
        && old.state.len == expected.state.len
        && old.digest == expected.digest
        && parse_snapshot_auto_state(&old_bytes).is_ok();
    let public = read_regular_observed(dir, leaf, MAX_METADATA);
    let new_ok = public
        .as_ref()
        .is_ok_and(|(bytes, _)| bytes == replacement && parse_snapshot_auto_state(bytes).is_ok());
    if !old_ok || !new_ok {
        // The exchange captured a writer which won after our last observation.
        // Restore that captured checkpoint to the public name before failing.
        // A non-cooperating third writer can still alter either reserved state
        // name in this syscall-width window; the post-exchange observations
        // detect that case, and no pathname is unlinked in either outcome.
        let captured_before = old.clone();
        let replacement_before = public.map(|(_, observation)| observation)?;
        exchange_bound(dir, leaf, temporary)?;
        sync_bound_directory(dir, leaf)?;
        let restored = read_regular_observed(dir, leaf, MAX_METADATA);
        let retired_replacement = read_regular_observed(dir, temporary, MAX_METADATA);
        let restored_ok = restored.as_ref().is_ok_and(|(_, observation)| {
            observation.identity == captured_before.identity
                && observation.state.len == captured_before.state.len
                && observation.digest == captured_before.digest
        });
        let replacement_retired_ok =
            retired_replacement
                .as_ref()
                .is_ok_and(|(bytes, observation)| {
                    bytes == replacement
                        && observation.identity == replacement_before.identity
                        && observation.state.len == replacement_before.state.len
                        && observation.digest == replacement_before.digest
                });
        if !restored_ok || !replacement_retired_ok {
            return Err(corrupt(
                "snapshot auto state changed during failed replacement rollback",
            ));
        }
        retire_snapshot_state_temporary(dir, temporary, replacement, &replacement_before)?;
        return Err(snapshot_auto_state_changed());
    }
    let old_handle = open_verified_file_handle(
        dir,
        temporary,
        &old,
        MAX_METADATA,
        "snapshot auto predecessor state",
    )?;
    let (captured, captured_observation) = exchange_capture_verified(
        dir,
        temporary,
        &old,
        MAX_METADATA,
        "snapshot auto predecessor state",
    )?;
    remove_verified_leaf(
        dir,
        &captured,
        &captured_observation,
        MAX_METADATA,
        "snapshot auto captured predecessor state",
    )?;
    remove_sentinel_leaf(dir, temporary)?;
    sync_bound_directory(dir, temporary)?;
    let unlinked =
        cap_fs::Metadata::from_file(&old_handle).map_err(|error| ioerr(error, temporary))?;
    if id_meta(&unlinked)? != old.identity || link_count(&unlinked)? != 0 {
        return Err(corrupt(
            "snapshot auto predecessor state gained another name",
        ));
    }
    Ok(())
}

/// Retire only the scheduler temporary created by this invocation.  The
/// exchange first captures the current name and compares its identity/bytes;
/// a substituted entry is preserved and fails closed rather than being
/// removed as cleanup.  This keeps repeated CAS conflicts from accumulating
/// unbounded private state files.
fn retire_snapshot_state_temporary(
    dir: &std::fs::File,
    temporary: &str,
    expected_bytes: &[u8],
    expected: &FileObservation,
) -> Result<()> {
    let (bytes, current) = read_regular_observed(dir, temporary, MAX_METADATA)?;
    if bytes != expected_bytes
        || current.identity != expected.identity
        || current.state.len != expected.state.len
        || current.digest != expected.digest
    {
        return Err(corrupt("snapshot auto temporary changed before retirement"));
    }
    let (captured, captured_observation) = exchange_capture_verified(
        dir,
        temporary,
        &current,
        MAX_METADATA,
        "snapshot auto temporary",
    )?;
    remove_verified_leaf(
        dir,
        &captured,
        &captured_observation,
        MAX_METADATA,
        "snapshot auto captured temporary",
    )?;
    remove_sentinel_leaf(dir, temporary)?;
    sync_bound_directory(dir, temporary)
}

/// Retire only scheduler state temporaries which could have been produced by
/// [`unique_internal_name`].  This runs under the scheduler writer lock,
/// before allocating the next temporary.  Names outside this narrow private
/// grammar are deliberately ignored; a name in the namespace which does not
/// conform is evidence of interference and stops publication without
/// deleting anything.
fn cleanup_snapshot_auto_state_temporaries(dir: &std::fs::File) -> Result<()> {
    let mut temporaries = Vec::new();
    for entry in cap_fs::read_base_dir(dir).map_err(|error| ioerr(error, "directory"))? {
        let entry = entry.map_err(|error| ioerr(error, "directory"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            // A non-UTF-8 name cannot be generated by this ASCII-only
            // namespace. It is unrelated and must not be removed.
            continue;
        };
        if !name.starts_with(SNAPSHOT_AUTO_STATE_TEMP_PREFIX) {
            continue;
        }
        if !is_snapshot_auto_state_temporary_name(name) {
            return Err(corrupt("malformed snapshot auto temporary name"));
        }
        temporaries.push(name.to_owned());
        if temporaries.len() > MAX_SNAPSHOT_AUTO_STATE_TEMPORARIES {
            return Err(limit("snapshot auto temporary count"));
        }
    }
    temporaries.sort();

    for temporary in temporaries {
        let (bytes, observation) = read_regular_observed(dir, &temporary, MAX_METADATA)?;
        parse_snapshot_auto_state(&bytes)?;
        retire_snapshot_state_temporary(dir, &temporary, &bytes, &observation)?;
    }
    // The individual retirement operation syncs the directory, including the
    // no-op case this makes the cleanup completion boundary explicit.
    sync_bound_directory(dir, "snapshot auto temporary cleanup")
}

fn is_snapshot_auto_state_temporary_name(name: &str) -> bool {
    let Some(suffix) = name.strip_prefix(SNAPSHOT_AUTO_STATE_TEMP_PREFIX) else {
        return false;
    };
    let Some((pid, nanos)) = suffix.split_once('-') else {
        return false;
    };
    if nanos.contains('-') || !is_canonical_decimal(pid) || !is_canonical_decimal(nanos) {
        return false;
    }
    pid.parse::<u32>().is_ok_and(|value| value != 0) && nanos.parse::<u128>().is_ok()
}

fn is_canonical_decimal(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && (value == "0" || !value.starts_with('0'))
}

/// Windows has no rename-exchange primitive. The retained `.kio` directory
/// handle and the process-wide writer lock are the authority boundary; rename
/// the already-created temporary handle relative to that directory with
/// replace + write-through semantics, then the caller verifies the exact
/// public bytes and directory identity before reporting success.
#[cfg(windows)]
fn replace_snapshot_state_expected(
    dir: &std::fs::File,
    leaf: &str,
    temporary: &str,
    _: &[u8],
    expected: &FileObservation,
) -> Result<()> {
    use cap_fs::OpenOptionsExt;
    use std::mem::{offset_of, size_of};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FileRenameInfoEx, SYNCHRONIZE, SetFileInformationByHandle,
    };
    const FILE_RENAME_FLAG_REPLACE_IF_EXISTS: u32 = 1;
    const FILE_RENAME_FLAG_POSIX_SEMANTICS: u32 = 2;

    if !read_regular_observed(dir, leaf, MAX_METADATA)
        .is_ok_and(|(_, observed)| observed == *expected)
    {
        return Err(snapshot_auto_state_changed());
    }
    let mut options = cap_fs::OpenOptions::new();
    options.access_mode(DELETE | SYNCHRONIZE);
    options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    options._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let temporary_handle = cap_fs::open(dir, Path::new(temporary), &options)
        .map_err(|error| ioerr(error, temporary))?;
    let name = std::ffi::OsStr::new(leaf).encode_wide().collect::<Vec<_>>();
    let name_bytes = name
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| corrupt("snapshot auto state name is too long"))?;
    let header = offset_of!(FILE_RENAME_INFO, FileName);
    let total = header
        .checked_add(name_bytes as usize)
        .ok_or_else(|| corrupt("snapshot auto state rename buffer overflow"))?;
    let mut buffer = vec![0_u8; total];
    // SAFETY: `buffer` is large enough for FILE_RENAME_INFO through the exact
    // UTF-16 leaf payload, and every pointer remains live for the syscall.
    let result = unsafe {
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        (*info).Anonymous.Flags =
            FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS;
        (*info).RootDirectory = dir.as_raw_handle();
        (*info).FileNameLength = name_bytes;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            std::ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
            name.len(),
        );
        SetFileInformationByHandle(
            temporary_handle.as_raw_handle(),
            FileRenameInfoEx,
            buffer.as_ptr().cast(),
            u32::try_from(total).unwrap_or(u32::MAX),
        )
    };
    if result == 0 {
        Err(ioerr(
            std::io::Error::last_os_error(),
            SNAPSHOT_AUTO_STATE_LEAF,
        ))
    } else {
        Ok(())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn replace_snapshot_state_expected(
    _: &std::fs::File,
    _: &str,
    _: &str,
    _: &[u8],
    _: &FileObservation,
) -> Result<()> {
    Err(corrupt(
        "platform lacks a verified snapshot state replacement primitive",
    ))
}

/// Atomically exchange a public leaf with an operation-private sentinel, then
/// bind the captured entry's identity before a caller unlinks it. If a
/// replacement won the race, that replacement remains captured and is never
/// removed by this operation.
fn exchange_capture_verified(
    dir: &std::fs::File,
    leaf: &str,
    expected: &FileObservation,
    max: u64,
    label: &str,
) -> Result<(String, FileObservation)> {
    let captured = unique_internal_name(".gc-captured");
    exchange_capture_verified_named(dir, leaf, &captured, expected, max, label)
}

fn exchange_capture_verified_named(
    dir: &std::fs::File,
    leaf: &str,
    captured: &str,
    expected: &FileObservation,
    max: u64,
    label: &str,
) -> Result<(String, FileObservation)> {
    create_new_bound(dir, captured, GC_RETIRE_SENTINEL, MAX_METADATA)?;
    exchange_bound(dir, leaf, captured)?;
    // The deterministic captured state is a recovery state, so it must reach
    // stable storage before any caller can expose a crash seam after capture.
    sync_bound_directory(dir, leaf)?;
    let (_bytes, observed) = read_regular_observed(dir, captured, max)?;
    if observed.identity != expected.identity
        || observed.state.len != expected.state.len
        || observed.digest != expected.digest
    {
        return Err(corrupt(&format!(
            "{label} changed during retirement capture"
        )));
    }
    // Make both sides of the exchange durable before the caller may inject a
    // crash or remove the captured object.  Without this directory fsync, a
    // power loss could resurrect the pre-exchange archive name while recovery
    // observes marker progress that assumes the deterministic capture exists.
    sync_bound_directory(dir, captured)?;
    // The exchange changes ctime, but this exact observation is used by the
    // immediate descriptor-relative unlink below.
    Ok((captured.to_owned(), observed))
}

fn tree_capture_name(marker: &GcInProgressMarker, raw: &str) -> String {
    format!("{}-{raw}-captured", marker.sweep_id)
}

fn remove_verified_leaf(
    dir: &std::fs::File,
    leaf: &str,
    expected: &FileObservation,
    max: u64,
    label: &str,
) -> Result<()> {
    let (bytes, observed) = read_regular_observed(dir, leaf, max)?;
    if observed != *expected {
        return Err(corrupt(&format!("{label} changed before unlink")));
    }
    let handle = open_verified_file_handle(dir, leaf, &observed, max, label)?;
    let handle_observed =
        cap_fs::Metadata::from_file(&handle).map_err(|error| ioerr(error, leaf))?;
    if id_meta(&handle_observed)? != observed.identity
        || handle_observed.len() != bytes.len() as u64
    {
        return Err(corrupt(&format!(
            "{label} changed while binding unlink handle"
        )));
    }
    // This name lives only in the operation-reserved `.kio/gc/internal/`
    // namespace.  On Darwin, make the final descriptor-relative unlink also
    // reject a symlinked path, an escape, or a vnode which gained another
    // hardlink in the last syscall-width window.  POSIX still has no
    // unlink-if-inode-equals primitive; the remaining replacement window is
    // the explicitly documented reserved-namespace residual shared with the
    // restore quarantine protocol.
    remove_reserved_leaf(dir, leaf)
}

#[cfg(target_os = "macos")]
fn remove_reserved_leaf(dir: &std::fs::File, leaf: &str) -> Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;

    // Darwin private/full-level flags from <sys/fcntl.h>.  They are accepted
    // by the public unlinkat(2) entry point. AT_UNIQUE performs the link-count
    // check in the same kernel operation as removal.
    const AT_SYMLINK_NOFOLLOW_ANY: libc::c_int = 0x0800;
    const AT_RESOLVE_BENEATH: libc::c_int = 0x2000;
    const AT_UNIQUE: libc::c_int = 0x8000;

    let leaf = CString::new(leaf).map_err(|_| corrupt("invalid GC retirement leaf"))?;
    // SAFETY: `leaf` is NUL-terminated, `dir` remains open for the call, and
    // unlinkat does not retain either pointer or descriptor.
    let result = unsafe {
        libc::unlinkat(
            dir.as_raw_fd(),
            leaf.as_ptr(),
            AT_SYMLINK_NOFOLLOW_ANY | AT_RESOLVE_BENEATH | AT_UNIQUE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(ioerr(std::io::Error::last_os_error(), "GC retirement leaf"))
    }
}

#[cfg(not(target_os = "macos"))]
fn remove_reserved_leaf(dir: &std::fs::File, leaf: &str) -> Result<()> {
    cap_fs::remove_file(dir, Path::new(leaf)).map_err(|error| ioerr(error, leaf))
}

fn remove_sentinel_leaf(dir: &std::fs::File, leaf: &str) -> Result<()> {
    let (bytes, observed) = read_regular_observed(dir, leaf, MAX_METADATA)?;
    if bytes != GC_RETIRE_SENTINEL {
        return Err(corrupt(
            "GC retirement sentinel was replaced before cleanup",
        ));
    }
    remove_verified_leaf(dir, leaf, &observed, MAX_METADATA, "GC retirement sentinel")
}

/// Descriptor-relative atomic move which refuses to clobber the destination.
/// This is the critical quarantine primitive: an attacker cannot pre-create a
/// victim name and have it replaced by a tree object.
#[allow(dead_code)]
#[cfg(target_os = "macos")]
fn rename_noreplace_bound(dir: &std::fs::File, from: &str, to: &str) -> Result<()> {
    rename_noreplace_between(dir, from, dir, to)
}
#[cfg(target_os = "macos")]
fn rename_noreplace_between(
    from_dir: &std::fs::File,
    from: &str,
    to_dir: &std::fs::File,
    to: &str,
) -> Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    let from = CString::new(from).map_err(|_| corrupt("invalid quarantine name"))?;
    let to = CString::new(to).map_err(|_| corrupt("invalid quarantine name"))?;
    unsafe extern "C" {
        fn renameatx_np(
            fromfd: libc::c_int,
            from: *const libc::c_char,
            tofd: libc::c_int,
            to: *const libc::c_char,
            flags: libc::c_uint,
        ) -> libc::c_int;
    }
    // Darwin's RENAME_EXCL is documented as 0x00000004.
    let result = unsafe {
        renameatx_np(
            from_dir.as_raw_fd(),
            from.as_ptr(),
            to_dir.as_raw_fd(),
            to.as_ptr(),
            0x0000_0004,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(ioerr(
            std::io::Error::last_os_error(),
            from.to_string_lossy().as_ref(),
        ))
    }
}
#[allow(dead_code)]
#[cfg(target_os = "macos")]
fn exchange_bound(dir: &std::fs::File, left: &str, right: &str) -> Result<()> {
    exchange_between(dir, left, dir, right)
}
#[cfg(target_os = "macos")]
fn exchange_between(
    left_dir: &std::fs::File,
    left: &str,
    right_dir: &std::fs::File,
    right: &str,
) -> Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    let left = CString::new(left).map_err(|_| corrupt("invalid marker archive name"))?;
    let right = CString::new(right).map_err(|_| corrupt("invalid marker archive name"))?;
    unsafe extern "C" {
        fn renameatx_np(
            fromfd: libc::c_int,
            from: *const libc::c_char,
            tofd: libc::c_int,
            to: *const libc::c_char,
            flags: libc::c_uint,
        ) -> libc::c_int;
    }
    // Darwin RENAME_SWAP is 0x00000002.
    if unsafe {
        renameatx_np(
            left_dir.as_raw_fd(),
            left.as_ptr(),
            right_dir.as_raw_fd(),
            right.as_ptr(),
            0x0000_0002,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(ioerr(
            std::io::Error::last_os_error(),
            left.to_string_lossy().as_ref(),
        ))
    }
}
#[allow(dead_code)]
#[cfg(target_os = "linux")]
fn exchange_bound(dir: &std::fs::File, left: &str, right: &str) -> Result<()> {
    exchange_between(dir, left, dir, right)
}
#[cfg(target_os = "linux")]
fn exchange_between(
    left_dir: &std::fs::File,
    left: &str,
    right_dir: &std::fs::File,
    right: &str,
) -> Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    let left = CString::new(left).map_err(|_| corrupt("invalid marker archive name"))?;
    let right = CString::new(right).map_err(|_| corrupt("invalid marker archive name"))?;
    // Linux renameat2 RENAME_EXCHANGE is 0x2.
    if unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            left_dir.as_raw_fd(),
            left.as_ptr(),
            right_dir.as_raw_fd(),
            right.as_ptr(),
            2_u32,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(ioerr(
            std::io::Error::last_os_error(),
            left.to_string_lossy().as_ref(),
        ))
    }
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn exchange_bound(_dir: &std::fs::File, _left: &str, _right: &str) -> Result<()> {
    Err(corrupt(
        "platform lacks a verified atomic GC marker exchange primitive",
    ))
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn exchange_between(
    _left_dir: &std::fs::File,
    _left: &str,
    _right_dir: &std::fs::File,
    _right: &str,
) -> Result<()> {
    Err(corrupt(
        "platform lacks a verified atomic GC marker exchange primitive",
    ))
}
#[cfg(target_os = "linux")]
fn rename_noreplace_bound(dir: &std::fs::File, from: &str, to: &str) -> Result<()> {
    rename_noreplace_between(dir, from, dir, to)
}
#[cfg(target_os = "linux")]
fn rename_noreplace_between(
    from_dir: &std::fs::File,
    from: &str,
    to_dir: &std::fs::File,
    to: &str,
) -> Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    let from = CString::new(from).map_err(|_| corrupt("invalid quarantine name"))?;
    let to = CString::new(to).map_err(|_| corrupt("invalid quarantine name"))?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            from_dir.as_raw_fd(),
            from.as_ptr(),
            to_dir.as_raw_fd(),
            to.as_ptr(),
            1_u32,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(ioerr(
            std::io::Error::last_os_error(),
            from.to_string_lossy().as_ref(),
        ))
    }
}
#[allow(dead_code)]
#[cfg(windows)]
fn rename_noreplace_bound(_dir: &std::fs::File, _from: &str, _to: &str) -> Result<()> {
    Err(corrupt(
        "platform lacks a verified no-replace GC quarantine primitive",
    ))
}
#[cfg(windows)]
fn rename_noreplace_between(
    _from_dir: &std::fs::File,
    _from: &str,
    _to_dir: &std::fs::File,
    _to: &str,
) -> Result<()> {
    Err(corrupt(
        "platform lacks a verified no-replace GC quarantine primitive",
    ))
}
#[allow(dead_code)]
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn rename_noreplace_bound(_dir: &std::fs::File, _from: &str, _to: &str) -> Result<()> {
    Err(corrupt(
        "platform lacks a verified no-replace GC quarantine primitive",
    ))
}
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn rename_noreplace_between(
    _from_dir: &std::fs::File,
    _from: &str,
    _to_dir: &std::fs::File,
    _to: &str,
) -> Result<()> {
    Err(corrupt(
        "platform lacks a verified no-replace GC quarantine primitive",
    ))
}
fn atomic_exchange_marker_expected(
    dir: &std::fs::File,
    leaf: &str,
    archives: &std::fs::File,
    bytes: &[u8],
    expected: &FileObservation,
) -> Result<()> {
    let temporary = unique_internal_name(".gc-retired-marker");
    create_new_bound(archives, &temporary, bytes, MAX_MARKER_BYTES)?;
    // The expected marker is re-read through the same retained descriptor
    // immediately before exchange. The exchange preserves it at `temporary`;
    // no unverified leaf is ever overwritten or unlinked.
    let matches = read_regular_observed(dir, leaf, MAX_MARKER_BYTES)
        .is_ok_and(|(_, observed)| observed == *expected);
    if !matches {
        // The temporary was created by this call, but never remove its name
        // blindly after a failed public-marker comparison. A replacement is
        // retained for diagnosis instead of becoming an unlink victim.
        if let Ok((actual, observation)) =
            read_regular_observed(archives, &temporary, MAX_MARKER_BYTES)
            && actual == bytes
        {
            let _ = remove_verified_leaf(
                archives,
                &temporary,
                &observation,
                MAX_MARKER_BYTES,
                "GC unpublished replacement marker",
            );
        }
        return Err(corrupt("GC marker changed before replacement"));
    }
    exchange_between(dir, leaf, archives, &temporary)?;
    // Exchange legitimately updates ctime, so bind the archived marker by its
    // stable identity, length and digest rather than full metadata state.
    let archived = read_regular_observed(archives, &temporary, MAX_MARKER_BYTES);
    let old_ok = archived.as_ref().is_ok_and(|(actual, observed)| {
        observed.identity == expected.identity
            && observed.state.len == expected.state.len
            && observed.digest == expected.digest
            && GcInProgressMarker::parse_canonical(actual).is_ok()
    });
    let new_ok = read_regular_observed(dir, leaf, MAX_MARKER_BYTES).is_ok_and(|(actual, _)| {
        actual == bytes && GcInProgressMarker::parse_canonical(&actual).is_ok()
    });
    if !old_ok || !new_ok {
        // Do not attempt a rollback after validation fails. A second exchange
        // would itself need a fresh atomic identity precondition for both
        // names and could swap a hostile replacement into the public marker
        // path. Preserve both entries for fail-closed recovery/diagnosis.
        return Err(corrupt("GC marker exchange validation failed"));
    }
    sync_bound_directory(dir, leaf)?;
    sync_bound_directory(archives, &temporary)?;
    let (_, archived_observation) = archived?;
    let retired_handle = open_verified_file_handle(
        archives,
        &temporary,
        &archived_observation,
        MAX_MARKER_BYTES,
        "GC retired marker",
    )?;
    let (captured, captured_observation) = exchange_capture_verified(
        archives,
        &temporary,
        &archived_observation,
        MAX_MARKER_BYTES,
        "GC retired marker",
    )?;
    remove_verified_leaf(
        archives,
        &captured,
        &captured_observation,
        MAX_MARKER_BYTES,
        "GC captured retired marker",
    )?;
    remove_sentinel_leaf(archives, &temporary)?;
    sync_bound_directory(archives, &temporary)?;
    let unlinked =
        cap_fs::Metadata::from_file(&retired_handle).map_err(|error| ioerr(error, &temporary))?;
    if id_meta(&unlinked)? != archived_observation.identity || link_count(&unlinked)? != 0 {
        return Err(corrupt("GC retired marker gained another name"));
    }
    Ok(())
}

fn unique_internal_name(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |value| value.as_nanos())
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GcPlanLimits {
    pub max_commits: u64,
    pub max_tree_entries: u64,
    pub max_verified_bytes: u64,
    pub max_refs: u64,
    pub max_receipts: u64,
    pub max_dir_entries: u64,
    pub max_name_bytes: u64,
    pub max_depth: u64,
    pub max_graph_steps: u64,
}
impl Default for GcPlanLimits {
    fn default() -> Self {
        Self {
            max_commits: 100_000,
            max_tree_entries: 10_000_000,
            max_verified_bytes: 4 * 1024 * 1024 * 1024,
            max_refs: 10_000,
            max_receipts: 100_000,
            max_dir_entries: 200_000,
            max_name_bytes: 255,
            max_depth: 4,
            max_graph_steps: 10_000_000,
        }
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct GcPlanStats {
    pub commits: u64,
    pub trees_verified: u64,
    pub tree_entries: u64,
    pub verified_bytes: u64,
    pub refs: u64,
    pub receipts: u64,
    pub dir_entries: u64,
    pub graph_steps: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct FileObservation {
    identity: Identity,
    state: FileState,
    digest: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GcCandidate {
    pub commit_hash: String,
    pub tree_hash: String,
    pub commit_type: CommitType,
    pub created_at: String,
    pub policy: String,
    pub size_bytes: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GcExclusion {
    pub reason: String,
    pub count: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GcPolicy {
    pub keep_last_hours: u32,
    pub keep_hourly_days: u32,
    pub keep_daily_weeks: u32,
    pub keep_weekly_months: u32,
    pub keep_repaired: u32,
}
impl Default for GcPolicy {
    fn default() -> Self {
        Self {
            keep_last_hours: 24,
            keep_hourly_days: 7,
            keep_daily_weeks: 4,
            keep_weekly_months: 6,
            keep_repaired: 5,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GcPlan {
    pub status: String,
    pub as_of: String,
    pub scope_path: String,
    pub policy: GcPolicy,
    pub limits: GcPlanLimits,
    pub stats: GcPlanStats,
    /// A second, independently bounded full read of the planning truth. A
    /// mismatch aborts instead of returning a stale candidate list.
    pub stability_check_stats: GcPlanStats,
    pub candidate_count: u64,
    pub candidate_tree_count: u64,
    pub estimated_bytes: u64,
    pub candidates: Vec<GcCandidate>,
    pub exclusions: Vec<GcExclusion>,
    pub object_kinds_planned: Vec<String>,
    /// Digest of the bounded semantic store truth consumed to make this plan.
    /// It is not an authority for mutation: a locked executor must re-plan.
    pub truth_digest: String,
    pub stable_truth_digest: String,
    pub baseline_receipts_digest: String,
    /// Digest of the semantic mutation intent (truth, policy and candidates).
    pub plan_digest: String,
}

impl GcPlan {
    /// Compare exactly the parts an executor may rely on after it independently
    /// rebinds and replans under the store lock. Diagnostic counters are not
    /// mutation authority.
    #[must_use]
    pub fn mutation_equivalent(&self, other: &Self) -> bool {
        self.truth_digest == other.truth_digest
            && self.stable_truth_digest == other.stable_truth_digest
            && self.baseline_receipts_digest == other.baseline_receipts_digest
            && self.plan_digest == other.plan_digest
            && self.policy == other.policy
            && self.candidates == other.candidates
    }
}

#[derive(Debug)]
pub struct GcPlanner {
    root: PathBuf,
    scope: std::fs::File,
    kio: std::fs::File,
    limits: GcPlanLimits,
}

struct TruthSnapshot {
    scope_digest: String,
    config_digest: String,
    policy: GcPolicy,
    refs: BTreeMap<String, String>,
    receipts: HashMap<String, String>,
    commits: HashMap<String, CommitObject>,
    tree_sizes: HashMap<String, u64>,
    observations: BTreeMap<String, FileObservation>,
}
impl GcPlanner {
    pub fn bind_current() -> Result<Self> {
        Self::bind(std::env::current_dir().map_err(|e| ioerr(e, "."))?)
    }
    pub fn bind(root: impl Into<PathBuf>) -> Result<Self> {
        let requested = root.into();
        if !requested.is_absolute() {
            return Err(KioError::invalid_usage(
                "GC planner scope root must be absolute",
            ));
        }
        // Bind the caller's actual path component-by-component before asking
        // the OS for a canonical diagnostic name. This rejects a symlink or
        // reparse component instead of silently following it during bind.
        let scope = open_bound_absolute(&requested)?;
        let public = requested.canonicalize().map_err(|e| ioerr(e, "scope"))?;
        if id_file(&scope)? != id_path(&public)? {
            return Err(corrupt("scope root changed while binding"));
        }
        let kio = open_optional_dir(&scope, ".kio")?
            .ok_or_else(|| KioError::invalid_usage("current directory is not a Kio scope"))?;
        Ok(Self {
            root: public,
            scope,
            kio,
            limits: GcPlanLimits::default(),
        })
    }
    pub fn with_limits(mut self, limits: GcPlanLimits) -> Self {
        self.limits = limits;
        self
    }
    pub fn plan_at(&self, now: i64) -> Result<GcPlan> {
        self.plan_at_inner(now, || {})
    }

    fn plan_at_inner(&self, now: i64, before_stability_check: impl FnOnce()) -> Result<GcPlan> {
        let root_id = id_file(&self.scope)?;
        let kio_id = id_file(&self.kio)?;
        self.require_layout()?;
        let mut st = GcPlanStats::default();
        let mut observations = BTreeMap::new();
        let scope_bytes = read_accounted_observed(
            &self.kio,
            "scope.json",
            MAX_METADATA,
            &mut st,
            &self.limits,
            &mut observations,
            "scope.json",
        )?;
        let scope_digest = validate_scope_bytes(&scope_bytes)?;
        let config_bytes = read_accounted_observed(
            &self.kio,
            "config.toml",
            MAX_METADATA,
            &mut st,
            &self.limits,
            &mut observations,
            "config.toml",
        )?;
        let (policy, config_digest) = read_policy_bytes(&config_bytes)?;
        let refs = self.read_refs(&mut st, &mut observations)?;
        let receipts = self.read_receipts(&mut st, &mut observations)?;
        let all = self.inventory_commits(&mut st, &mut observations)?;
        validate_commit_links(&all)?;
        validate_receipt_links(&receipts, &all)?;
        self.validate_markerless_shallow_state(&refs, &receipts, &all)?;
        let mut sizes = HashMap::new();
        for (h, c) in &all {
            if !receipts.contains_key(h) && !sizes.contains_key(&c.tree) {
                sizes.insert(
                    c.tree.clone(),
                    self.verify_tree(&c.tree, &mut st, &mut observations)?,
                );
            }
        }
        let mut ex = BTreeMap::new();
        let pairs = retention_candidate_pairs(
            &policy,
            &refs,
            &all,
            &receipts,
            now,
            &mut st,
            &self.limits,
            &mut ex,
        )?;
        let mut candidates = Vec::with_capacity(pairs.len());
        for (h, tree_hash) in pairs {
            let c = &all[&h];
            let size_bytes = sizes
                .get(&tree_hash)
                .copied()
                .ok_or_else(|| corrupt("candidate tree was not verified"))?;
            candidates.push(GcCandidate {
                commit_hash: h,
                tree_hash,
                commit_type: c.commit_type,
                created_at: c.created_at.clone(),
                policy: if c.commit_type == CommitType::Auto {
                    "auto_retention".into()
                } else {
                    "derived_retention".into()
                },
                size_bytes,
            });
        }
        candidates.sort_by(|a, b| a.commit_hash.cmp(&b.commit_hash));
        let unique: BTreeSet<_> = candidates.iter().map(|c| c.tree_hash.clone()).collect();
        let estimated = unique.iter().try_fold(0u64, |n, t| {
            n.checked_add(sizes.get(t).copied().unwrap_or(0))
                .ok_or_else(|| limit("estimated bytes"))
        })?;
        before_stability_check();
        let stability_check_stats = self.require_stable_truth(&TruthSnapshot {
            scope_digest: scope_digest.clone(),
            config_digest: config_digest.clone(),
            policy: policy.clone(),
            refs: refs.clone(),
            receipts: receipts.clone(),
            commits: all.clone(),
            tree_sizes: sizes.clone(),
            observations: observations.clone(),
        })?;
        self.recheck(root_id, kio_id)?;
        let truth_digest = semantic_truth_digest(
            &scope_digest,
            &config_digest,
            &policy,
            &refs,
            &receipts,
            &all,
            &sizes,
            &observations,
        )?;
        let stable_truth_digest = semantic_stable_truth_digest(
            &scope_digest,
            &config_digest,
            &policy,
            &refs,
            &all,
            &observations,
        )?;
        let baseline_receipts_digest = receipt_observation_digest(&receipts, &observations)?;
        let plan_digest = semantic_plan_digest(&truth_digest, &policy, &candidates)?;
        Ok(GcPlan {
            status: "dry_run".into(),
            as_of: format_utc_seconds(now),
            scope_path: self.root.display().to_string(),
            policy,
            limits: self.limits.clone(),
            stats: st,
            stability_check_stats,
            candidate_count: candidates.len() as u64,
            candidate_tree_count: unique.len() as u64,
            estimated_bytes: estimated,
            candidates,
            exclusions: ex
                .into_iter()
                .map(|(reason, count)| GcExclusion { reason, count })
                .collect(),
            object_kinds_planned: vec!["tree".into()],
            truth_digest,
            stable_truth_digest,
            baseline_receipts_digest,
            plan_digest,
        })
    }
    fn recheck(&self, r: Identity, k: Identity) -> Result<()> {
        if id_file(&self.scope)? != r
            || id_file(&self.kio)? != k
            || id_path(&self.root)? != r
            || id_child(&self.scope, ".kio")? != k
        {
            Err(corrupt("scope root changed while planning"))
        } else {
            Ok(())
        }
    }
    fn require_layout(&self) -> Result<()> {
        for d in [
            "refs",
            "refs/tags-v1",
            "objects",
            "objects/commits",
            "objects/trees",
        ] {
            let _ = open_path(&self.kio, d)?;
        }
        Ok(())
    }
    fn read_refs(
        &self,
        s: &mut GcPlanStats,
        observations: &mut BTreeMap<String, FileObservation>,
    ) -> Result<BTreeMap<String, String>> {
        let mut o = BTreeMap::new();
        let h = String::from_utf8(read_accounted_observed(
            &self.kio,
            "HEAD",
            MAX_REF,
            s,
            &self.limits,
            observations,
            "HEAD",
        )?)
        .map_err(|_| corrupt("ref is not utf8"))?;
        if h == "unborn\n" {
            // The only valid empty-history HEAD encoding.
        } else if let Some(hash) = h.strip_suffix('\n') {
            add_ref(&mut o, "HEAD".into(), hash, s, &self.limits)?
        } else {
            return Err(corrupt("HEAD is not newline-terminated"));
        }
        let refs = open_path(&self.kio, "refs")?;
        for (dir, tag) in [("tags-v1", true)] {
            let d = open_dir(&refs, dir)?;
            for n in names(&d, s, &self.limits, 2)? {
                if tag && n == "names.jsonl" {
                    let _ = read_accounted_observed(
                        &d,
                        &n,
                        MAX_METADATA,
                        s,
                        &self.limits,
                        observations,
                        &format!("refs/{dir}/{n}"),
                    )?;
                    continue;
                }
                if tag && !(n.len() == 68 && n.starts_with("tag-") && hex(&n[4..])) {
                    return Err(corrupt("invalid tag ref leaf"));
                }
                let v = String::from_utf8(read_accounted_observed(
                    &d,
                    &n,
                    MAX_REF,
                    s,
                    &self.limits,
                    observations,
                    &format!("refs/{dir}/{n}"),
                )?)
                .map_err(|_| corrupt("ref is not utf8"))?;
                if v.trim().is_empty() {
                    return Err(corrupt("ref is empty"));
                } else {
                    add_ref(&mut o, format!("{dir}/{n}"), v.trim(), s, &self.limits)?;
                }
            }
        }
        Ok(o)
    }
    fn read_receipts(
        &self,
        s: &mut GcPlanStats,
        observations: &mut BTreeMap<String, FileObservation>,
    ) -> Result<HashMap<String, String>> {
        let gc = match open_optional_dir(&self.kio, "gc")? {
            Some(directory) => directory,
            None => return Ok(HashMap::new()),
        };
        let d = match open_optional_dir(&gc, "shallowed")? {
            Some(directory) => directory,
            None => return Ok(HashMap::new()),
        };
        let mut o = HashMap::new();
        for n in names(&d, s, &self.limits, 3)? {
            if n.len() != 64 || !hex(&n) {
                return Err(corrupt("invalid shallow receipt leaf"));
            }
            let bytes = read_accounted_observed(
                &d,
                &n,
                MAX_METADATA,
                s,
                &self.limits,
                observations,
                &format!("gc/shallowed/{n}"),
            )?;
            let r = ShallowReceipt::parse_canonical(&bytes, &n)?;
            if o.insert(r.commit_hash, r.tree_hash).is_some() {
                return Err(corrupt("invalid shallow receipt"));
            }
            s.receipts = checked(s.receipts, 1, "receipts")?;
            if s.receipts > self.limits.max_receipts {
                return Err(limit("receipts"));
            }
        }
        Ok(o)
    }
    fn inventory_commits(
        &self,
        s: &mut GcPlanStats,
        observations: &mut BTreeMap<String, FileObservation>,
    ) -> Result<HashMap<String, CommitObject>> {
        let b = open_path(&self.kio, "objects/commits")?;
        let mut o = HashMap::new();
        for a in names(&b, s, &self.limits, 2)? {
            if a.len() != 2 || !hex(&a) {
                return Err(corrupt("invalid commit fanout"));
            }
            let ad = open_dir(&b, &a)?;
            for bb in names(&ad, s, &self.limits, 3)? {
                if bb.len() != 2 || !hex(&bb) {
                    return Err(corrupt("invalid commit fanout"));
                }
                let bd = open_dir(&ad, &bb)?;
                for n in names(&bd, s, &self.limits, 4)? {
                    if n.len() != 64 || !hex(&n) || n[..2] != a || n[2..4] != bb {
                        return Err(corrupt("invalid commit leaf"));
                    }
                    let x = read_accounted_observed(
                        &bd,
                        &n,
                        MAX_COMMIT_OBJECT_BYTES,
                        s,
                        &self.limits,
                        observations,
                        &format!("commit/{a}/{bb}/{n}"),
                    )?;
                    let h = format!("sha256:{n}");
                    if hash_bytes(&x) != h {
                        return Err(corrupt("commit hash mismatch"));
                    }
                    let c: CommitObject =
                        serde_json::from_slice(&x).map_err(|_| corrupt("invalid commit object"))?;
                    c.validate().map_err(|_| corrupt("invalid commit object"))?;
                    if o.insert(h, c).is_some() {
                        return Err(corrupt("duplicate commit"));
                    }
                    s.commits = checked(s.commits, 1, "commits")?;
                    if s.commits > self.limits.max_commits {
                        return Err(limit("commits"));
                    }
                }
            }
        }
        Ok(o)
    }
    fn verify_tree(
        &self,
        h: &str,
        s: &mut GcPlanStats,
        observations: &mut BTreeMap<String, FileObservation>,
    ) -> Result<u64> {
        let raw = h
            .strip_prefix("sha256:")
            .ok_or_else(|| corrupt("invalid tree hash"))?;
        if raw.len() != 64 || !hex(raw) {
            return Err(corrupt("invalid tree hash"));
        }
        let b = open_path(&self.kio, "objects/trees")?;
        let a = open_required_dir(&b, &raw[..2], "tree object fanout is missing")?;
        let d = open_required_dir(&a, &raw[2..4], "tree object fanout is missing")?;
        let x = read_required_accounted_observed(
            &d,
            raw,
            MAX_TREE_OBJECT_BYTES,
            "tree object is missing",
            s,
            &self.limits,
            observations,
            &format!("tree/{}/{}/{}", &raw[..2], &raw[2..4], raw),
        )?;
        if hash_bytes(&x) != h {
            return Err(corrupt("tree hash mismatch"));
        }
        let t: TreeObject =
            serde_json::from_slice(&x).map_err(|_| corrupt("invalid tree object"))?;
        if t.entries.len() > MAX_TREE_ENTRIES {
            return Err(corrupt("tree entry limit exceeded"));
        }
        t.validate().map_err(|_| corrupt("invalid tree object"))?;
        s.trees_verified = checked(s.trees_verified, 1, "trees")?;
        s.tree_entries = checked(s.tree_entries, t.entries.len() as u64, "tree entries")?;
        if s.tree_entries > self.limits.max_tree_entries {
            return Err(limit("tree entries"));
        }
        Ok(x.len() as u64)
    }

    /// Outside an active, strictly validated operation marker, a receipt must
    /// explain an already-removed tree and may never name a current ref tip.
    /// This prevents a handcrafted receipt from suppressing a live tree.
    fn validate_markerless_shallow_state(
        &self,
        refs: &BTreeMap<String, String>,
        receipts: &HashMap<String, String>,
        commits: &HashMap<String, CommitObject>,
    ) -> Result<()> {
        if read_active_marker_bound(&self.kio)?.is_some() {
            return Ok(());
        }
        for (commit, tree) in receipts {
            if refs.values().any(|tip| tip == commit) {
                return Err(corrupt(
                    "shallow receipt names a current ref tip without active GC marker",
                ));
            }
            let raw = tree
                .strip_prefix("sha256:")
                .ok_or_else(|| corrupt("invalid shallow receipt tree"))?;
            let trees = open_path(&self.kio, "objects/trees")?;
            // Preserve the Rust 2021 lifetime of the directory capabilities used
            // for this object read instead of relying on tail-expression drops.
            let present = {
                let first = open_optional_dir(&trees, &raw[..2])?;
                let second = match first.as_ref() {
                    Some(first) => open_optional_dir(first, &raw[2..4])?,
                    None => None,
                };
                match second.as_ref() {
                    Some(dir) => match read_regular_observed(dir, raw, MAX_TREE_OBJECT_BYTES) {
                        Ok(_) => true,
                        Err(error) if is_io_not_found(&error) => false,
                        Err(error) => return Err(error),
                    },
                    None => false,
                }
            };
            if present {
                return Err(corrupt(
                    "markerless shallow receipt coexists with tree object",
                ));
            }
            let current = commits
                .get(commit)
                .ok_or_else(|| corrupt("shallow receipt commit is missing"))?;
            if &current.tree != tree {
                return Err(corrupt("shallow receipt tree differs from commit"));
            }
        }
        Ok(())
    }

    fn require_stable_truth(&self, expected: &TruthSnapshot) -> Result<GcPlanStats> {
        let mut stats = GcPlanStats::default();
        let mut observations = BTreeMap::new();
        let scope_bytes = read_accounted_observed(
            &self.kio,
            "scope.json",
            MAX_METADATA,
            &mut stats,
            &self.limits,
            &mut observations,
            "scope.json",
        )?;
        let scope_digest = validate_scope_bytes(&scope_bytes)?;
        let config_bytes = read_accounted_observed(
            &self.kio,
            "config.toml",
            MAX_METADATA,
            &mut stats,
            &self.limits,
            &mut observations,
            "config.toml",
        )?;
        let (policy, config_digest) = read_policy_bytes(&config_bytes)?;
        let refs = self.read_refs(&mut stats, &mut observations)?;
        let receipts = self.read_receipts(&mut stats, &mut observations)?;
        let commits = self.inventory_commits(&mut stats, &mut observations)?;
        validate_commit_links(&commits)?;
        validate_receipt_links(&receipts, &commits)?;
        self.validate_markerless_shallow_state(&refs, &receipts, &commits)?;

        let mut tree_sizes = HashMap::new();
        for (commit_hash, commit) in &commits {
            if !receipts.contains_key(commit_hash) && !tree_sizes.contains_key(&commit.tree) {
                tree_sizes.insert(
                    commit.tree.clone(),
                    self.verify_tree(&commit.tree, &mut stats, &mut observations)?,
                );
            }
        }
        if scope_digest != expected.scope_digest
            || config_digest != expected.config_digest
            || policy != expected.policy
            || refs != expected.refs
            || receipts != expected.receipts
            || commits != expected.commits
            || tree_sizes != expected.tree_sizes
            || observations != expected.observations
        {
            return Err(corrupt("store truth changed while planning GC"));
        }
        Ok(stats)
    }
}
/// The sole on-disk shallow receipt representation.  Its byte form is JCS plus
/// one LF, so a receipt cannot be silently accepted in a different encoding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShallowReceipt {
    pub commit_hash: String,
    pub tree_hash: String,
    pub gc_policy: String,
    pub shallowed_at: String,
}

impl ShallowReceipt {
    pub fn new(commit_hash: String, tree_hash: String, shallowed_at: String) -> Result<Self> {
        let receipt = Self {
            commit_hash,
            tree_hash,
            gc_policy: "shallow".into(),
            shallowed_at,
        };
        receipt.validate(None)?;
        Ok(receipt)
    }

    pub fn validate(&self, expected_leaf: Option<&str>) -> Result<()> {
        if !is_hash(&self.commit_hash)
            || !is_hash(&self.tree_hash)
            || self.gc_policy != "shallow"
            || !is_canonical_utc_timestamp(&self.shallowed_at)
        {
            return Err(corrupt("invalid shallow receipt"));
        }
        if let Some(leaf) = expected_leaf
            && (leaf.len() != 64 || !hex(leaf) || self.commit_hash != format!("sha256:{leaf}"))
        {
            return Err(corrupt(
                "shallow receipt filename does not match commit hash",
            ));
        }
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate(None)?;
        let mut bytes = canonical_json_bytes(
            &serde_json::to_value(self).map_err(|e| corrupt(&e.to_string()))?,
        )?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    pub fn parse_canonical(bytes: &[u8], expected_leaf: &str) -> Result<Self> {
        if !bytes.ends_with(b"\n") || bytes[..bytes.len().saturating_sub(1)].contains(&b'\n') {
            return Err(corrupt("shallow receipt is not canonical JCS+LF"));
        }
        let receipt: Self =
            serde_json::from_slice(bytes).map_err(|_| corrupt("malformed shallow receipt"))?;
        receipt.validate(Some(expected_leaf))?;
        if receipt.canonical_bytes()? != bytes {
            return Err(corrupt("shallow receipt is not canonical JCS+LF"));
        }
        Ok(receipt)
    }
}

#[allow(clippy::too_many_arguments)]
fn semantic_truth_digest(
    scope: &str,
    config: &str,
    policy: &GcPolicy,
    refs: &BTreeMap<String, String>,
    receipts: &HashMap<String, String>,
    commits: &HashMap<String, CommitObject>,
    trees: &HashMap<String, u64>,
    observations: &BTreeMap<String, FileObservation>,
) -> Result<String> {
    let mut receipt_values: BTreeMap<_, _> = BTreeMap::new();
    receipt_values.extend(receipts.iter());
    let mut commit_values: BTreeMap<_, _> = BTreeMap::new();
    commit_values.extend(commits.iter());
    let mut tree_values: BTreeMap<_, _> = BTreeMap::new();
    tree_values.extend(trees.iter());
    Ok(hash_bytes(&canonical_json_bytes(&json!({
        "scope": scope, "config": config, "policy": policy, "refs": refs,
        "receipts": receipt_values, "commits": commit_values, "trees": tree_values,
        "observations": observations,
    }))?))
}

fn semantic_plan_digest(
    truth: &str,
    policy: &GcPolicy,
    candidates: &[GcCandidate],
) -> Result<String> {
    Ok(hash_bytes(&canonical_json_bytes(&json!({
        "truth_digest": truth, "policy": policy, "candidates": candidates,
    }))?))
}

fn semantic_stable_truth_digest(
    scope: &str,
    config: &str,
    policy: &GcPolicy,
    refs: &BTreeMap<String, String>,
    commits: &HashMap<String, CommitObject>,
    observations: &BTreeMap<String, FileObservation>,
) -> Result<String> {
    let mut commit_values = BTreeMap::new();
    commit_values.extend(commits.iter());
    let stable_observations: BTreeMap<_, _> = observations
        .iter()
        .filter(|(path, _)| is_stable_truth_observation(path))
        .collect();
    Ok(hash_bytes(&canonical_json_bytes(&json!({
        "scope": scope, "config": config, "policy": policy,
        "refs": refs, "commits": commit_values, "observations": stable_observations,
    }))?))
}
fn is_stable_truth_observation(path: &str) -> bool {
    matches!(path, "scope.json" | "config.toml" | "HEAD")
        || path.starts_with("refs/")
        || path.starts_with("commit/")
}
fn receipt_observation_digest(
    receipts: &HashMap<String, String>,
    observations: &BTreeMap<String, FileObservation>,
) -> Result<String> {
    let mut records = BTreeMap::new();
    for (commit, tree) in receipts {
        let leaf = commit
            .strip_prefix("sha256:")
            .ok_or_else(|| corrupt("invalid receipt commit"))?;
        let observation = observations
            .get(&format!("gc/shallowed/{leaf}"))
            .ok_or_else(|| corrupt("receipt observation missing"))?;
        records.insert(commit, json!({"tree_hash":tree,"observation":observation}));
    }
    Ok(hash_bytes(&canonical_json_bytes(
        &serde_json::to_value(records).map_err(|e| corrupt(&e.to_string()))?,
    )?))
}

fn read_receipt_observation_digest_bound(
    kio: &std::fs::File,
    frozen: &BTreeMap<&String, &String>,
) -> Result<String> {
    let Some(gc) = open_optional_dir(kio, "gc")? else {
        return receipt_observation_digest(&HashMap::new(), &BTreeMap::new());
    };
    let Some(dir) = open_optional_dir(&gc, "shallowed")? else {
        return receipt_observation_digest(&HashMap::new(), &BTreeMap::new());
    };
    let mut receipts = HashMap::new();
    let mut observations = BTreeMap::new();
    let mut stats = GcPlanStats::default();
    let limits = GcPlanLimits::default();
    for leaf in names(&dir, &mut stats, &limits, 3)? {
        let (bytes, observation) = read_regular_observed(&dir, &leaf, MAX_METADATA)?;
        let receipt = ShallowReceipt::parse_canonical(&bytes, &leaf)?;
        if !frozen.contains_key(&receipt.commit_hash) {
            receipts.insert(receipt.commit_hash.clone(), receipt.tree_hash);
            observations.insert(format!("gc/shallowed/{leaf}"), observation);
        }
    }
    receipt_observation_digest(&receipts, &observations)
}
fn operation_receipt_observation_digest_bound(
    kio: &std::fs::File,
    marker: &GcInProgressMarker,
) -> Result<String> {
    let gc = open_required_dir(kio, "gc", "GC directory is missing")?;
    let dir = open_required_dir(&gc, "shallowed", "GC receipt directory is missing")?;
    let mut records = BTreeMap::new();
    for candidate in &marker.candidates {
        let leaf = &candidate.commit_hash["sha256:".len()..];
        let (bytes, observation) = read_regular_observed(&dir, leaf, MAX_METADATA)?;
        let receipt = ShallowReceipt::parse_canonical(&bytes, leaf)?;
        let expected = ShallowReceipt::new(
            candidate.commit_hash.clone(),
            candidate.tree_hash.clone(),
            marker.started_at.clone(),
        )?;
        if receipt != expected {
            return Err(corrupt("marker-owned receipt differs from frozen form"));
        }
        records.insert(
            candidate.commit_hash.clone(),
            json!({"receipt": receipt, "observation": observation}),
        );
    }
    Ok(hash_bytes(&canonical_json_bytes(
        &serde_json::to_value(records).map_err(|error| corrupt(&error.to_string()))?,
    )?))
}

fn validate_scope_bytes(bytes: &[u8]) -> Result<String> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|error| KioError::schema(error.to_string()))?;
    let version = match value.get("kio_format_version") {
        Some(serde_json::Value::String(version)) => version.as_str(),
        Some(_) => return Err(KioError::incompatible_format("<non-string>")),
        None => return Err(KioError::incompatible_format("<missing>")),
    };
    if version != KIO_FORMAT_VERSION {
        return Err(KioError::incompatible_format(version));
    }
    validate_json_schema(SchemaKind::Scope, &value)?;
    Ok(hash_bytes(bytes))
}

fn read_policy_bytes(bytes: &[u8]) -> Result<(GcPolicy, String)> {
    let value = parse_config_bytes(bytes)?;
    let mut policy = GcPolicy::default();
    if let Some(value) = value.as_ref() {
        if let Some(retention) = value.get("gc").and_then(|gc| gc.get("auto_retention")) {
            policy.keep_last_hours = num(retention, "keep_last_hours", policy.keep_last_hours)?;
            policy.keep_hourly_days = num(retention, "keep_hourly_days", policy.keep_hourly_days)?;
            policy.keep_daily_weeks = num(retention, "keep_daily_weeks", policy.keep_daily_weeks)?;
            policy.keep_weekly_months =
                num(retention, "keep_weekly_months", policy.keep_weekly_months)?;
        }
        if let Some(retention) = value.get("gc").and_then(|gc| gc.get("derived_retention")) {
            policy.keep_repaired = num(retention, "keep_repaired", policy.keep_repaired)?;
        }
    }
    let hourly_hours = policy
        .keep_hourly_days
        .checked_mul(24)
        .ok_or_else(|| limit("policy"))?;
    let daily_days = policy
        .keep_daily_weeks
        .checked_mul(7)
        .ok_or_else(|| limit("policy"))?;
    let weekly_days = policy
        .keep_weekly_months
        .checked_mul(30)
        .ok_or_else(|| limit("policy"))?;
    if policy.keep_last_hours > hourly_hours
        || policy.keep_hourly_days > daily_days
        || daily_days > weekly_days
    {
        return Err(KioError::schema("gc retention horizons must be monotonic"));
    }
    Ok((policy, hash_bytes(bytes)))
}

/// Parse the complete user configuration once, applying the same schema and
/// semantic checks used by planning and recovery. Empty configuration is the
/// documented default configuration rather than an unvalidated TOML special
/// case.
fn parse_config_bytes(bytes: &[u8]) -> Result<Option<toml::Value>> {
    if bytes.is_empty() {
        return Ok(None);
    }
    let value: toml::Value = toml::from_str(
        std::str::from_utf8(bytes).map_err(|_| KioError::schema("config not utf8"))?,
    )
    .map_err(|error| KioError::schema(error.to_string()))?;
    let json = serde_json::to_value(&value).map_err(|error| KioError::schema(error.to_string()))?;
    validate_json_schema(SchemaKind::Config, &json)?;
    enforce_scope_config_semantics(&json)?;
    Ok(Some(value))
}

fn read_automation_config_bytes(bytes: &[u8]) -> Result<GcAutomationConfig> {
    let Some(value) = parse_config_bytes(bytes)? else {
        return Ok(GcAutomationConfig::default());
    };
    let Some(gc) = value.get("gc") else {
        return Ok(GcAutomationConfig::default());
    };
    let mode = match gc.get("mode").and_then(toml::Value::as_str) {
        Some("manual_only") => GcAutomationMode::ManualOnly,
        Some("after_index") => GcAutomationMode::AfterIndex,
        Some("on_idle") => GcAutomationMode::OnIdle,
        None => return Err(KioError::schema("gc mode is required when [gc] is present")),
        // The full config schema above makes this unreachable for valid input;
        // retain this fail-closed arm if that schema changes independently.
        Some(_) => return Err(KioError::schema("invalid gc mode")),
    };
    let max_runtime_seconds = gc
        .get("max_runtime_seconds")
        .map(|value| {
            u64::try_from(
                value
                    .as_integer()
                    .ok_or_else(|| KioError::schema("gc max_runtime_seconds must be an integer"))?,
            )
            .map_err(|_| KioError::schema("gc max_runtime_seconds must be non-negative"))
        })
        .transpose()?;
    let idle_threshold_seconds =
        gc.get("idle_threshold_seconds")
            .map(|value| {
                u64::try_from(value.as_integer().ok_or_else(|| {
                    KioError::schema("gc idle_threshold_seconds must be an integer")
                })?)
                .map_err(|_| KioError::schema("gc idle_threshold_seconds must be non-negative"))
            })
            .transpose()?;
    Ok(GcAutomationConfig {
        mode,
        max_runtime_seconds,
        idle_threshold_seconds,
    })
}

fn read_snapshot_auto_config_bytes(bytes: &[u8]) -> Result<Option<SnapshotAutoConfig>> {
    let Some(value) = parse_config_bytes(bytes)? else {
        return Ok(None);
    };
    let Some(auto) = value
        .get("snapshot")
        .and_then(toml::Value::as_table)
        .and_then(|snapshot| snapshot.get("auto"))
        .and_then(toml::Value::as_table)
    else {
        return Ok(None);
    };
    let enabled = auto
        .get("enabled")
        .and_then(toml::Value::as_bool)
        .ok_or_else(|| KioError::schema("snapshot.auto enabled must be a boolean"))?;
    let interval_seconds = u64::try_from(
        auto.get("interval_seconds")
            .and_then(toml::Value::as_integer)
            .ok_or_else(|| KioError::schema("snapshot.auto interval_seconds must be an integer"))?,
    )
    .map_err(|_| KioError::schema("snapshot.auto interval_seconds is out of range"))?;
    let on_change_threshold = u64::try_from(
        auto.get("on_change_threshold")
            .and_then(toml::Value::as_integer)
            .ok_or_else(|| {
                KioError::schema("snapshot.auto on_change_threshold must be an integer")
            })?,
    )
    .map_err(|_| KioError::schema("snapshot.auto on_change_threshold is out of range"))?;
    // The schema is the canonical range authority. Keep a local assertion so
    // this parser remains fail-closed if that schema is edited independently.
    if !(1..=31_536_000).contains(&interval_seconds)
        || !(1..=1_000_000).contains(&on_change_threshold)
    {
        return Err(KioError::schema(
            "snapshot.auto value is outside its canonical range",
        ));
    }
    Ok(Some(SnapshotAutoConfig {
        enabled,
        interval_seconds,
        on_change_threshold,
    }))
}

fn canonical_snapshot_auto_state_bytes(state: &SnapshotAutoState) -> Result<Vec<u8>> {
    let mut bytes = canonical_json_bytes(
        &serde_json::to_value(state).map_err(|error| KioError::schema(error.to_string()))?,
    )?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_METADATA {
        return Err(limit("snapshot auto state bytes"));
    }
    Ok(bytes)
}

fn parse_snapshot_auto_state(bytes: &[u8]) -> Result<SnapshotAutoState> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| corrupt(&format!("invalid snapshot auto state: {error}")))?;
    if value.get("version").and_then(serde_json::Value::as_u64)
        != Some(u64::from(SNAPSHOT_AUTO_STATE_VERSION))
    {
        return Err(corrupt(
            "unsupported snapshot auto state version; remove it and allow clean recreation",
        ));
    }
    let state: SnapshotAutoState = serde_json::from_value(value)
        .map_err(|error| corrupt(&format!("invalid snapshot auto state: {error}")))?;
    let seconds = parse_utc_seconds(&state.last_successful_eligible_attempt_at)
        .filter(|seconds| format_utc_seconds(*seconds) == state.last_successful_eligible_attempt_at)
        .ok_or_else(|| corrupt("snapshot auto state time is not canonical UTC seconds"))?;
    let idle_seconds = parse_utc_seconds(&state.idle_observed_since)
        .filter(|seconds| format_utc_seconds(*seconds) == state.idle_observed_since)
        .ok_or_else(|| corrupt("snapshot auto state idle time is not canonical UTC seconds"))?;
    if !is_hash(&state.working_set_digest)
        || format_utc_seconds(seconds) != state.last_successful_eligible_attempt_at
        || format_utc_seconds(idle_seconds) != state.idle_observed_since
        || canonical_snapshot_auto_state_bytes(&state)? != bytes
    {
        return Err(corrupt("snapshot auto state is not canonical JCS+LF"));
    }
    Ok(state)
}

fn snapshot_auto_state_changed() -> KioError {
    KioError::new(
        "KIO-E-SNAPSHOT-STATE-CHANGED-001",
        "scheduled snapshot state changed during the locked eligibility handoff",
        json!({}),
        ExitCode::PartialFailure,
    )
}

fn snapshot_auto_authority_changed() -> KioError {
    KioError::new(
        "KIO-E-SNAPSHOT-AUTHORITY-CHANGED-001",
        "scheduled snapshot authority changed during publication",
        json!({}),
        ExitCode::PartialFailure,
    )
}

pub fn snapshot_auto_clock_error(message: &str) -> KioError {
    KioError::new(
        "KIO-E-SNAPSHOT-CLOCK-001",
        message,
        json!({}),
        ExitCode::PartialFailure,
    )
}

fn snapshot_auto_unsafe_entry(message: &str) -> KioError {
    KioError::new(
        "KIO-E-SNAPSHOT-UNSAFE-ENTRY-001",
        message,
        json!({}),
        ExitCode::PermanentFailure,
    )
}

/// Inventory one scheduler working-set root through its retained directory
/// capability. This is the single direct-entry contract used by both the CLI
/// observation and the repository's final publication checks: directories are
/// out of scope, while every other entry must be an unchanged, no-follow,
/// single-link regular file.
pub(crate) fn validated_snapshot_auto_direct_entries(
    scope: &std::fs::File,
) -> Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    for entry in cap_fs::read_base_dir(scope).map_err(|error| ioerr(error, "scope"))? {
        let entry = entry.map_err(|error| ioerr(error, "scope"))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| snapshot_auto_unsafe_entry("non-UTF-8 direct entry"))?;
        if name == ".kio" {
            continue;
        }
        let path = Path::new(&name);
        let before = cap_fs::stat(scope, path, cap_fs::FollowSymlinks::No)
            .map_err(|error| ioerr(error, &name))?;
        if before.is_dir() {
            #[cfg(windows)]
            {
                use cap_fs::_WindowsByHandle;
                use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
                if before.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    return Err(snapshot_auto_unsafe_entry(
                        "direct directory is a reparse point",
                    ));
                }
            }
            continue;
        }
        valid_file(&before, MAX_RAW_OBJECT_BYTES).map_err(|_| {
            snapshot_auto_unsafe_entry("direct entry is not a safe single-link regular file")
        })?;
        let mut options = cap_fs::OpenOptions::new();
        options.read(true);
        options._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
        let file = cap_fs::open(scope, path, &options).map_err(|error| ioerr(error, &name))?;
        let opened = cap_fs::Metadata::from_file(&file).map_err(|error| ioerr(error, &name))?;
        let after = cap_fs::stat(scope, path, cap_fs::FollowSymlinks::No)
            .map_err(|error| ioerr(error, &name))?;
        if valid_file(&opened, MAX_RAW_OBJECT_BYTES).is_err()
            || valid_file(&after, MAX_RAW_OBJECT_BYTES).is_err()
            || id_meta(&before)? != id_meta(&opened)?
            || id_meta(&before)? != id_meta(&after)?
        {
            return Err(snapshot_auto_unsafe_entry(
                "direct entry changed during scheduler validation",
            ));
        }
        names.insert(name);
    }
    Ok(names)
}

fn open_bound_absolute(path: &Path) -> Result<std::fs::File> {
    let mut filesystem_root = PathBuf::new();
    let mut descendants = Vec::new();
    let mut saw_root = false;
    for c in path.components() {
        match c {
            Component::Prefix(prefix) => filesystem_root.push(prefix.as_os_str()),
            Component::RootDir => {
                filesystem_root.push(c.as_os_str());
                saw_root = true;
            }
            Component::Normal(name) if saw_root => descendants.push(name.to_os_string()),
            Component::CurDir => {}
            _ => return Err(corrupt("invalid scope path")),
        }
    }
    if !saw_root {
        return Err(corrupt("scope path has no stable filesystem root"));
    }
    let mut directory = cap_fs::open_ambient_dir(&filesystem_root, ambient_authority())
        .map_err(|error| ioerr(error, &filesystem_root))?;
    validate_directory_handle(&directory, &filesystem_root)?;
    for name in descendants {
        directory = open_dir_os(&directory, &name)?;
    }
    Ok(directory)
}
fn open_path(base: &std::fs::File, path: &str) -> Result<std::fs::File> {
    let mut d = base.try_clone().map_err(|e| ioerr(e, path))?;
    for c in Path::new(path).components() {
        let Component::Normal(x) = c else {
            return Err(corrupt("invalid store layout path"));
        };
        d = open_dir_os(&d, x)?
    }
    Ok(d)
}
fn open_dir(base: &std::fs::File, n: &str) -> Result<std::fs::File> {
    open_dir_os(base, std::ffi::OsStr::new(n))
}

/// Open one optional direct-child directory without following its final
/// component. Only an exact `NotFound` is absence; every other failure remains
/// observable so an unsafe entry cannot masquerade as an empty namespace.
fn open_optional_dir(base: &std::fs::File, n: &str) -> Result<Option<std::fs::File>> {
    let path = Path::new(n);
    let directory = match cap_fs::open_dir_nofollow(base, path) {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ioerr(error, n)),
    };
    validate_directory_handle(&directory, n)?;
    Ok(Some(directory))
}

fn open_required_dir(
    base: &std::fs::File,
    n: &str,
    missing_message: &str,
) -> Result<std::fs::File> {
    open_optional_dir(base, n)?.ok_or_else(|| corrupt(missing_message))
}

fn open_dir_os(base: &std::fs::File, n: &std::ffi::OsStr) -> Result<std::fs::File> {
    let d = cap_fs::open_dir_nofollow(base, Path::new(n)).map_err(|e| ioerr(e, n))?;
    validate_directory_handle(&d, n)?;
    Ok(d)
}

fn validate_directory_handle(directory: &std::fs::File, display: impl AsRef<Path>) -> Result<()> {
    let metadata =
        cap_fs::Metadata::from_file(directory).map_err(|error| ioerr(error, display.as_ref()))?;
    if !metadata.is_dir() {
        return Err(corrupt("expected real directory"));
    }
    #[cfg(windows)]
    {
        use cap_fs::_WindowsByHandle;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(corrupt("directory is a Windows reparse point"));
        }
    }
    Ok(())
}

/// Capability directory handles are intentionally opened as `O_PATH` on
/// Linux, which is valid for descriptor-relative traversal but cannot be
/// passed to `fsync`. Reopen exactly `.` below the retained capability with
/// no-follow read access, verify that it is the same directory, then flush
/// that syncable descriptor. No ambient pathname is reconstructed.
#[cfg(unix)]
fn sync_bound_directory(directory: &std::fs::File, label: impl AsRef<Path>) -> Result<()> {
    let label = label.as_ref();
    let expected = cap_fs::Metadata::from_file(directory).map_err(|error| ioerr(error, label))?;
    validate_directory_handle(directory, label)?;
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let syncable =
        cap_fs::open(directory, Path::new("."), &options).map_err(|error| ioerr(error, label))?;
    let observed = cap_fs::Metadata::from_file(&syncable).map_err(|error| ioerr(error, label))?;
    if !observed.is_dir() || id_meta(&observed)? != id_meta(&expected)? {
        return Err(corrupt(
            "retained directory changed while reopening for fsync",
        ));
    }
    syncable.sync_all().map_err(|error| ioerr(error, label))
}

/// Windows cannot flush a directory handle (`FlushFileBuffers` rejects it),
/// so retain the fail-closed capability validation and rely on the preceding
/// file-level sync and NTFS metadata journaling. This deliberately does not
/// claim POSIX directory durability on Windows.
#[cfg(windows)]
fn sync_bound_directory(directory: &std::fs::File, label: impl AsRef<Path>) -> Result<()> {
    validate_directory_handle(directory, label.as_ref())
}

#[cfg(not(any(unix, windows)))]
fn sync_bound_directory(directory: &std::fs::File, label: impl AsRef<Path>) -> Result<()> {
    let _ = directory;
    Err(ioerr(
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "directory durability is unsupported on this platform",
        ),
        label.as_ref(),
    ))
}
fn names(
    d: &std::fs::File,
    s: &mut GcPlanStats,
    l: &GcPlanLimits,
    depth: u64,
) -> Result<Vec<String>> {
    if depth > l.max_depth {
        return Err(limit("directory depth"));
    }
    let mut v = Vec::new();
    for e in cap_fs::read_base_dir(d).map_err(|e| ioerr(e, "directory"))? {
        let e = e.map_err(|e| ioerr(e, "directory"))?;
        let n = e
            .file_name()
            .into_string()
            .map_err(|_| corrupt("non-utf8 store entry"))?;
        if n.len() as u64 > l.max_name_bytes {
            return Err(limit("name bytes"));
        }
        saturating_entry(s, l)?;
        v.push(n)
    }
    v.sort();
    Ok(v)
}
fn read_regular_observed(
    d: &std::fs::File,
    n: &str,
    max: u64,
) -> Result<(Vec<u8>, FileObservation)> {
    let p = Path::new(n);
    let before = cap_fs::stat(d, p, cap_fs::FollowSymlinks::No).map_err(|e| ioerr(e, n))?;
    valid_file(&before, max)?;
    let mut o = cap_fs::OpenOptions::new();
    o.read(true);
    o._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut f = cap_fs::open(d, p, &o).map_err(|e| ioerr(e, n))?;
    let opened = cap_fs::Metadata::from_file(&f).map_err(|e| ioerr(e, n))?;
    valid_file(&opened, max)?;
    if !same_file_state(&before, &opened)? {
        return Err(corrupt("store file changed while opening"));
    }
    let mut b = Vec::with_capacity(usize::try_from(opened.len()).map_err(|_| limit("file bytes"))?);
    (&mut f)
        .take(max.saturating_add(1))
        .read_to_end(&mut b)
        .map_err(|e| ioerr(e, n))?;
    if b.len() as u64 > max {
        return Err(limit("file bytes"));
    }
    let after = cap_fs::stat(d, p, cap_fs::FollowSymlinks::No).map_err(|e| ioerr(e, n))?;
    valid_file(&after, max)?;
    if b.len() as u64 != opened.len() || !same_file_state(&after, &opened)? {
        return Err(corrupt("store file changed while read"));
    }
    let observation = FileObservation {
        identity: id_meta(&opened)?,
        state: file_state(&opened),
        digest: hash_bytes(&b),
    };
    Ok((b, observation))
}

#[cfg(all(test, unix))]
fn read_regular(d: &std::fs::File, n: &str, max: u64) -> Result<Vec<u8>> {
    read_regular_observed(d, n, max).map(|(bytes, _)| bytes)
}

fn read_accounted_observed(
    directory: &std::fs::File,
    name: &str,
    max: u64,
    stats: &mut GcPlanStats,
    limits: &GcPlanLimits,
    observations: &mut BTreeMap<String, FileObservation>,
    observation_name: &str,
) -> Result<Vec<u8>> {
    let (bytes, observation) = read_regular_observed(directory, name, max)?;
    account(stats, bytes.len() as u64, limits)?;
    insert_observation(observations, observation_name, observation)?;
    Ok(bytes)
}

fn insert_observation(
    observations: &mut BTreeMap<String, FileObservation>,
    name: impl Into<String>,
    observation: FileObservation,
) -> Result<()> {
    if observations.insert(name.into(), observation).is_some() {
        Err(corrupt("duplicate observed store path"))
    } else {
        Ok(())
    }
}

fn observe_directory(
    directory: &std::fs::File,
    name: impl Into<String>,
    observations: &mut BTreeMap<String, FileObservation>,
) -> Result<()> {
    let metadata =
        cap_fs::Metadata::from_file(directory).map_err(|error| ioerr(error, "directory"))?;
    validate_directory_handle(directory, "directory")?;
    insert_observation(
        observations,
        name,
        FileObservation {
            identity: id_meta(&metadata)?,
            state: file_state(&metadata),
            digest: "directory".to_owned(),
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn read_required_accounted_observed(
    directory: &std::fs::File,
    name: &str,
    max: u64,
    missing_message: &str,
    stats: &mut GcPlanStats,
    limits: &GcPlanLimits,
    observations: &mut BTreeMap<String, FileObservation>,
    observation_name: &str,
) -> Result<Vec<u8>> {
    match read_accounted_observed(
        directory,
        name,
        max,
        stats,
        limits,
        observations,
        observation_name,
    ) {
        Err(error) if is_io_not_found(&error) => Err(corrupt(missing_message)),
        result => result,
    }
}
fn valid_file(m: &cap_fs::Metadata, max: u64) -> Result<()> {
    if !m.is_file() || m.len() > max {
        return Err(if m.len() > max {
            limit("file bytes")
        } else {
            corrupt("non-regular store entry")
        });
    }
    #[cfg(unix)]
    {
        use cap_primitives::fs::MetadataExt;
        if m.nlink() != 1 {
            return Err(corrupt("linked store entry"));
        }
    }
    #[cfg(windows)]
    {
        use cap_fs::_WindowsByHandle;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

        if m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || m.number_of_links() != Some(1)
        {
            return Err(corrupt("linked store entry"));
        }
    }
    Ok(())
}
fn link_count(metadata: &cap_fs::Metadata) -> Result<u64> {
    #[cfg(unix)]
    {
        use cap_primitives::fs::MetadataExt;
        Ok(metadata.nlink())
    }
    #[cfg(windows)]
    {
        use cap_fs::_WindowsByHandle;
        metadata
            .number_of_links()
            .map(u64::from)
            .ok_or_else(|| corrupt("store link count is unavailable"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        Err(corrupt("store link count is unavailable"))
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Identity(
    #[serde(serialize_with = "crate::identity_serde::u64_hex::serialize")] u64,
    #[serde(serialize_with = "crate::identity_serde::u64_hex::serialize")] u64,
);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct FileState {
    len: u64,
    modified_seconds: i64,
    modified_nanos: i64,
    changed_seconds: i64,
    changed_nanos: i64,
}
fn id_meta(m: &cap_fs::Metadata) -> Result<Identity> {
    #[cfg(unix)]
    {
        use cap_primitives::fs::MetadataExt;
        Ok(Identity(m.dev(), m.ino()))
    }
    #[cfg(windows)]
    {
        use cap_fs::_WindowsByHandle;
        let volume = m
            .volume_serial_number()
            .ok_or_else(|| corrupt("store identity is unavailable"))?;
        let index = m
            .file_index()
            .ok_or_else(|| corrupt("store identity is unavailable"))?;
        Ok(Identity(u64::from(volume), index))
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = m;
        Err(corrupt("store identity is unsupported on this platform"))
    }
}

#[cfg(unix)]
fn canonical_gc_index_identity_from_metadata(metadata: &cap_fs::Metadata) -> Result<String> {
    use cap_primitives::fs::MetadataExt;
    Ok(format!(
        "unix:{:016x}:{:016x}",
        metadata.dev(),
        metadata.ino()
    ))
}

#[cfg(windows)]
fn canonical_gc_index_identity_from_metadata(metadata: &cap_fs::Metadata) -> Result<String> {
    use cap_fs::_WindowsByHandle;
    let volume = metadata
        .volume_serial_number()
        .ok_or_else(|| corrupt("GC source index has no Windows volume identity"))?;
    let index = metadata
        .file_index()
        .ok_or_else(|| corrupt("GC source index has no Windows file identity"))?;
    Ok(format!("windows:{volume:08x}:{index:016x}"))
}

#[cfg(not(any(unix, windows)))]
fn canonical_gc_index_identity_from_metadata(metadata: &cap_fs::Metadata) -> Result<String> {
    let _ = metadata;
    Err(corrupt("GC source index identity is unsupported"))
}

#[cfg(unix)]
fn same_file_state(left: &cap_fs::Metadata, right: &cap_fs::Metadata) -> Result<bool> {
    Ok(id_meta(left)? == id_meta(right)? && file_state(left) == file_state(right))
}

#[cfg(not(unix))]
fn same_file_state(left: &cap_fs::Metadata, right: &cap_fs::Metadata) -> Result<bool> {
    Ok(id_meta(left)? == id_meta(right)?
        && left.len() == right.len()
        && left.modified().ok() == right.modified().ok())
}

#[cfg(unix)]
fn file_state(metadata: &cap_fs::Metadata) -> FileState {
    use cap_primitives::fs::MetadataExt;
    FileState {
        len: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanos: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanos: metadata.ctime_nsec(),
    }
}

#[cfg(not(unix))]
fn file_state(metadata: &cap_fs::Metadata) -> FileState {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.into_std().duration_since(std::time::UNIX_EPOCH).ok());
    FileState {
        len: metadata.len(),
        modified_seconds: modified
            .as_ref()
            .and_then(|duration| i64::try_from(duration.as_secs()).ok())
            .unwrap_or(-1),
        modified_nanos: modified
            .as_ref()
            .map_or(-1, |duration| i64::from(duration.subsec_nanos())),
        changed_seconds: -1,
        changed_nanos: -1,
    }
}
fn id_file(f: &std::fs::File) -> Result<Identity> {
    id_meta(&cap_fs::Metadata::from_file(f).map_err(|e| ioerr(e, "scope"))?)
}
fn id_path(p: &Path) -> Result<Identity> {
    let f = open_bound_absolute(p)?;
    id_file(&f)
}
fn id_child(d: &std::fs::File, n: &str) -> Result<Identity> {
    let x = open_dir(d, n)?;
    id_file(&x)
}
fn is_io_not_found(error: &KioError) -> bool {
    error.error_code() == "KIO-E-STORE-IO-001"
        && error
            .context()
            .get("io_error_kind")
            .and_then(serde_json::Value::as_str)
            == Some("not_found")
}
fn hex(x: &str) -> bool {
    !x.is_empty()
        && x.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn is_canonical_ulid(value: &str) -> bool {
    value.len() == 26 && value.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z'))
}
fn is_canonical_gc_index_identity(value: &str) -> bool {
    #[cfg(unix)]
    {
        let Some((platform, dev, ino)) = value.split_once(':').and_then(|(platform, rest)| {
            rest.split_once(':').map(|(dev, ino)| (platform, dev, ino))
        }) else {
            return false;
        };
        platform == "unix"
            && dev.len() == 16
            && ino.len() == 16
            && dev
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            && ino
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    }
    #[cfg(windows)]
    {
        let Some((platform, volume, index)) = value.split_once(':').and_then(|(platform, rest)| {
            rest.split_once(':')
                .map(|(volume, index)| (platform, volume, index))
        }) else {
            return false;
        };
        platform == "windows"
            && volume.len() == 8
            && index.len() == 16
            && volume
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            && index
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = value;
        false
    }
}
fn is_valid_gc_index_temp_leaf(value: &str) -> bool {
    value.starts_with(".gc-index-")
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}
fn checked(a: u64, b: u64, w: &str) -> Result<u64> {
    a.checked_add(b).ok_or_else(|| limit(w))
}
fn saturating_entry(s: &mut GcPlanStats, l: &GcPlanLimits) -> Result<()> {
    s.dir_entries = checked(s.dir_entries, 1, "directory entries")?;
    if s.dir_entries > l.max_dir_entries {
        Err(limit("directory entries"))
    } else {
        Ok(())
    }
}
fn account(s: &mut GcPlanStats, n: u64, l: &GcPlanLimits) -> Result<()> {
    s.verified_bytes = checked(s.verified_bytes, n, "verified bytes")?;
    if s.verified_bytes > l.max_verified_bytes {
        Err(limit("verified bytes"))
    } else {
        Ok(())
    }
}
fn add_ref(
    o: &mut BTreeMap<String, String>,
    n: String,
    v: &str,
    s: &mut GcPlanStats,
    l: &GcPlanLimits,
) -> Result<()> {
    if !is_hash(v) {
        return Err(corrupt("invalid ref hash"));
    }
    if o.insert(n, v.into()).is_some() {
        return Err(corrupt("duplicate ref"));
    }
    s.refs = checked(s.refs, 1, "refs")?;
    if s.refs > l.max_refs {
        Err(limit("refs"))
    } else {
        Ok(())
    }
}
fn num(t: &toml::Value, k: &str, d: u32) -> Result<u32> {
    match t.get(k).and_then(toml::Value::as_integer) {
        Some(n) => u32::try_from(n).map_err(|_| KioError::schema("gc retention must be uint32")),
        None => Ok(d),
    }
}
fn validate_commit_links(commits: &HashMap<String, CommitObject>) -> Result<()> {
    if commits.values().any(|commit| {
        commit
            .parent
            .as_ref()
            .is_some_and(|parent| !commits.contains_key(parent))
    }) {
        Err(corrupt("commit parent is missing"))
    } else {
        Ok(())
    }
}

fn validate_receipt_links(
    receipts: &HashMap<String, String>,
    commits: &HashMap<String, CommitObject>,
) -> Result<()> {
    for (commit_hash, tree_hash) in receipts {
        let commit = commits
            .get(commit_hash)
            .ok_or_else(|| corrupt("shallow receipt commit is missing"))?;
        if &commit.tree != tree_hash
            || !matches!(commit.commit_type, CommitType::Auto | CommitType::Repaired)
        {
            return Err(corrupt("invalid shallow receipt relation"));
        }
    }
    Ok(())
}

fn graph_step(stats: &mut GcPlanStats, limits: &GcPlanLimits) -> Result<()> {
    stats.graph_steps = checked(stats.graph_steps, 1, "graph steps")?;
    if stats.graph_steps > limits.max_graph_steps {
        Err(limit("graph steps"))
    } else {
        Ok(())
    }
}

fn closure(
    start: &str,
    all: &HashMap<String, CommitObject>,
    stats: &mut GcPlanStats,
    limits: &GcPlanLimits,
) -> Result<HashSet<String>> {
    let mut o = HashSet::new();
    let mut q: Vec<String> = vec![start.into()];
    while let Some(h) = q.pop() {
        graph_step(stats, limits)?;
        if o.insert(h.clone())
            && let Some(c) = all.get(&h)
        {
            q.extend(c.parent.iter().cloned())
        }
    }
    Ok(o)
}

fn fractional_digits(timestamp: &str) -> &str {
    timestamp
        .strip_suffix('Z')
        .and_then(|body| body.split_once('.').map(|(_, fraction)| fraction))
        .unwrap_or("")
}

fn is_canonical_utc_timestamp(timestamp: &str) -> bool {
    let Some(seconds) = parse_utc_seconds(timestamp) else {
        return false;
    };
    let seconds_shape = timestamp
        .strip_suffix('Z')
        .and_then(|body| body.split_once('.').map(|(whole, _)| format!("{whole}Z")))
        .unwrap_or_else(|| timestamp.to_owned());
    seconds_shape == format_utc_seconds(seconds)
}

fn compare_fractional(left: &str, right: &str) -> Ordering {
    let width = left.len().max(right.len());
    (0..width)
        .map(|index| {
            let left = left.as_bytes().get(index).copied().unwrap_or(b'0');
            let right = right.as_bytes().get(index).copied().unwrap_or(b'0');
            left.cmp(&right)
        })
        .find(|ordering| *ordering != Ordering::Equal)
        .unwrap_or(Ordering::Equal)
}

fn compare_timestamps(left: &str, right: &str) -> Ordering {
    let left_seconds = parse_utc_seconds(left).unwrap_or(i64::MIN);
    let right_seconds = parse_utc_seconds(right).unwrap_or(i64::MIN);
    left_seconds
        .cmp(&right_seconds)
        .then_with(|| compare_fractional(fractional_digits(left), fractional_digits(right)))
}

fn newer_first(left: (&CommitObject, &str), right: (&CommitObject, &str)) -> Ordering {
    compare_timestamps(&right.0.created_at, &left.0.created_at).then_with(|| left.1.cmp(right.1))
}

fn timestamp_is_after_seconds(timestamp: &str, seconds: i64) -> bool {
    let timestamp_seconds = parse_utc_seconds(timestamp).unwrap_or(i64::MAX);
    timestamp_seconds > seconds
        || timestamp_seconds == seconds
            && fractional_digits(timestamp)
                .bytes()
                .any(|digit| digit != b'0')
}

fn age_is_less_than(timestamp: &str, now: i64, horizon_seconds: i64) -> bool {
    let timestamp_seconds = parse_utc_seconds(timestamp).unwrap_or(i64::MIN);
    let age_seconds = now.saturating_sub(timestamp_seconds);
    age_seconds < horizon_seconds
        || age_seconds == horizon_seconds
            && fractional_digits(timestamp)
                .bytes()
                .any(|digit| digit != b'0')
}
fn auto_retained(
    c: &HashSet<String>,
    all: &HashMap<String, CommitObject>,
    now: i64,
    p: &GcPolicy,
    ex: &mut BTreeMap<String, u64>,
) -> HashSet<String> {
    let mut k = HashSet::new();
    let mut b = HashMap::new();
    for h in c {
        let x = &all[h];
        let Some(t) = parse_utc_seconds(&x.created_at) else {
            continue;
        };
        if timestamp_is_after_seconds(&x.created_at, now) {
            k.insert(h.clone());
            inc(ex, "future_timestamp");
            continue;
        }
        let (tier, key, why) =
            if age_is_less_than(&x.created_at, now, (p.keep_last_hours as i64) * 3600) {
                (0, 0, "retained_recent")
            } else if age_is_less_than(&x.created_at, now, (p.keep_hourly_days as i64) * 86400) {
                (1, t.div_euclid(3600), "retained_hourly")
            } else if age_is_less_than(&x.created_at, now, (p.keep_daily_weeks as i64) * 604800) {
                (2, t.div_euclid(86400), "retained_daily")
            } else if age_is_less_than(&x.created_at, now, (p.keep_weekly_months as i64) * 2592000)
            {
                (
                    3,
                    (t.div_euclid(86400) + 3).div_euclid(7),
                    "retained_weekly",
                )
            } else {
                continue;
            };
        if tier == 0 {
            k.insert(h.clone());
            inc(ex, why)
        } else if b
            .get(&(tier, key))
            .map(|old: &String| newer_first((x, h), (&all[old], old)) == Ordering::Less)
            .unwrap_or(true)
        {
            b.insert((tier, key), h.clone());
        }
    }
    for ((tier, _), h) in b {
        k.insert(h);
        inc(
            ex,
            match tier {
                1 => "retained_hourly",
                2 => "retained_daily",
                _ => "retained_weekly",
            },
        );
    }
    k
}

/// Compute the retention-authorized commit/tree pairs without reading tree
/// objects.  The regular planner verifies and sizes every selected tree after
/// this step; recovery deliberately cannot require those reads because its
/// own completed sweep may already have removed a selected tree.  Keeping the
/// policy selection here makes recovery prove the same eligibility rules as a
/// fresh plan instead of treating marker digests as authority.
// This deliberately mirrors the planner's independently supplied inputs:
// keeping policy/refs/commits/receipts/limits separate prevents a recovery
// caller from smuggling ambient plan state into authorization.
#[allow(clippy::too_many_arguments)]
fn retention_candidate_pairs(
    policy: &GcPolicy,
    refs: &BTreeMap<String, String>,
    all: &HashMap<String, CommitObject>,
    receipts: &HashMap<String, String>,
    now: i64,
    stats: &mut GcPlanStats,
    limits: &GcPlanLimits,
    exclusions: &mut BTreeMap<String, u64>,
) -> Result<Vec<(String, String)>> {
    let mut reachable = HashSet::new();
    let mut queue: VecDeque<_> = refs.values().cloned().collect();
    while let Some(hash) = queue.pop_front() {
        graph_step(stats, limits)?;
        if reachable.insert(hash.clone()) {
            let commit = all
                .get(&hash)
                .ok_or_else(|| corrupt("ref or parent commit is missing"))?;
            queue.extend(commit.parent.iter().cloned());
        }
    }
    for _ in all.keys().filter(|hash| !reachable.contains(*hash)) {
        inc(exclusions, "unreachable_commit");
    }
    let tips: HashSet<_> = refs.values().cloned().collect();
    let head_reachable = match refs.get("HEAD") {
        Some(head) => closure(head, all, stats, limits)?,
        None => HashSet::new(),
    };
    let keep = policy.keep_repaired as usize;
    let mut retained_repaired = HashSet::new();
    if keep > 0 {
        let mut repaired: Vec<_> = head_reachable
            .iter()
            .filter(|hash| all[*hash].commit_type == CommitType::Repaired)
            .cloned()
            .collect();
        if keep < repaired.len() {
            repaired.select_nth_unstable_by(keep, |left, right| {
                newer_first((&all[left], left), (&all[right], right))
            });
            repaired.truncate(keep);
        }
        retained_repaired.extend(repaired);
    }
    let mut possible = HashSet::new();
    for hash in &reachable {
        let commit = &all[hash];
        let exclusion = if receipts.contains_key(hash) {
            Some("already_shallow")
        } else if tips.contains(hash) {
            Some("ref_tip")
        } else if !matches!(commit.commit_type, CommitType::Auto | CommitType::Repaired) {
            Some("protected_commit_type")
        } else if commit.commit_type == CommitType::Repaired && retained_repaired.contains(hash) {
            Some("retained_repaired")
        } else if commit.commit_type == CommitType::Repaired && !head_reachable.contains(hash) {
            Some("repaired_outside_head")
        } else {
            None
        };
        if let Some(exclusion) = exclusion {
            inc(exclusions, exclusion);
        } else {
            possible.insert(hash.clone());
        }
    }
    let autos: HashSet<_> = possible
        .iter()
        .filter(|hash| all[*hash].commit_type == CommitType::Auto)
        .cloned()
        .collect();
    let retained_autos = auto_retained(&autos, all, now, policy, exclusions);
    possible.retain(|hash| !retained_autos.contains(hash));

    let mut by_tree: HashMap<String, Vec<String>> = HashMap::new();
    for (hash, commit) in all {
        by_tree
            .entry(commit.tree.clone())
            .or_default()
            .push(hash.clone());
    }
    let initial = possible.clone();
    possible.retain(|hash| {
        let safe = by_tree[&all[hash].tree]
            .iter()
            .all(|other| receipts.contains_key(other) || initial.contains(other));
        if !safe {
            inc(exclusions, "shared_tree_non_shallow");
        }
        safe
    });
    let mut pairs: Vec<_> = possible
        .into_iter()
        .map(|hash| {
            let tree = all[&hash].tree.clone();
            (hash, tree)
        })
        .collect();
    pairs.sort();
    Ok(pairs)
}

fn inc(x: &mut BTreeMap<String, u64>, n: &str) {
    *x.entry(n.into()).or_default() += 1
}
fn corrupt(m: &str) -> KioError {
    KioError::new(
        "KIO-E-STORE-CORRUPT-001",
        m,
        json!({}),
        ExitCode::PermanentFailure,
    )
}
fn limit(w: &str) -> KioError {
    KioError::new(
        "KIO-E-GC-PLAN-LIMIT-001",
        format!("GC planning limit exceeded: {w}"),
        json!({"limit":w}),
        ExitCode::PermanentFailure,
    )
}
fn ioerr(e: std::io::Error, p: impl AsRef<Path>) -> KioError {
    let kind = match e.kind() {
        std::io::ErrorKind::NotFound => "not_found",
        std::io::ErrorKind::PermissionDenied => "permission_denied",
        std::io::ErrorKind::AlreadyExists => "already_exists",
        std::io::ErrorKind::InvalidData => "invalid_data",
        _ => "other",
    };
    KioError::new(
        "KIO-E-STORE-IO-001",
        e.to_string(),
        json!({ "path": p.as_ref(), "io_error_kind": kind }),
        ExitCode::Failure,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::CommitStats;
    use crate::scope::Repository;

    #[test]
    fn gc_identity_jcs_preserves_all_bits() {
        for value in [(1_u64 << 53) + 1, 9_851_624_185_183_609, u64::MAX] {
            let identity = Identity(value, value);
            let bytes = serde_jcs::to_vec(&identity).unwrap();
            let components: Vec<String> = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(components, vec![format!("{value:016x}"); 2]);
            assert_eq!(
                canonical_json_bytes(&serde_json::to_value(&identity).unwrap()).unwrap(),
                bytes
            );
        }
    }

    #[test]
    fn gc_identity_truth_and_receipt_digests_distinguish_adjacent_large_ids() {
        fn digests(identity: Identity) -> [String; 3] {
            let commit_hash = hash_bytes(b"commit");
            let tree_hash = hash_bytes(b"tree");
            let observation = FileObservation {
                identity,
                state: FileState {
                    len: 1,
                    modified_seconds: 0,
                    modified_nanos: 0,
                    changed_seconds: 0,
                    changed_nanos: 0,
                },
                digest: hash_bytes(b"file"),
            };
            let observations = BTreeMap::from([
                ("HEAD".into(), observation.clone()),
                (format!("gc/shallowed/{}", &commit_hash[7..]), observation),
            ]);
            let receipts = HashMap::from([(commit_hash, tree_hash)]);
            let policy = GcPolicy::default();
            let refs = BTreeMap::new();
            let commits = HashMap::new();
            [
                semantic_truth_digest(
                    "scope",
                    "config",
                    &policy,
                    &refs,
                    &receipts,
                    &commits,
                    &HashMap::new(),
                    &observations,
                )
                .unwrap(),
                semantic_stable_truth_digest(
                    "scope",
                    "config",
                    &policy,
                    &refs,
                    &commits,
                    &observations,
                )
                .unwrap(),
                receipt_observation_digest(&receipts, &observations).unwrap(),
            ]
        }

        // Each pair collapses to the same IEEE-754 number if encoded numerically.
        // The middle value is an observed NTFS file index (0x002300000004bd79).
        for value in [(1_u64 << 53) + 1, 9_851_624_185_183_609, u64::MAX] {
            for (left, right) in [
                (Identity(value - 1, 42), Identity(value, 42)),
                (Identity(42, value - 1), Identity(42, value)),
            ] {
                assert_ne!(left, right);
                let before = digests(left);
                let after = digests(right);
                for (index, label) in ["truth", "stable truth", "receipt"].iter().enumerate() {
                    assert_ne!(
                        before[index], after[index],
                        "{label} digest lost identity bits"
                    );
                }
            }
        }
    }

    fn commit(created_at: &str) -> CommitObject {
        CommitObject::new(
            format!("sha256:{}", "a".repeat(64)),
            None,
            created_at.into(),
            "x".into(),
            format!("sha256:{}", "b".repeat(64)),
            CommitStats {
                files_added: 0,
                files_modified: 0,
                files_deleted: 0,
            },
            CommitType::Auto,
        )
        .unwrap()
    }

    #[test]
    fn shallow_receipt_requires_exact_jcs_lf_and_matching_leaf() {
        let commit = format!("sha256:{}", "a".repeat(64));
        let tree = format!("sha256:{}", "b".repeat(64));
        let receipt =
            ShallowReceipt::new(commit.clone(), tree, "2026-01-01T00:00:00Z".into()).unwrap();
        let bytes = receipt.canonical_bytes().unwrap();
        assert_eq!(
            ShallowReceipt::parse_canonical(&bytes, &commit[7..]).unwrap(),
            receipt
        );
        assert!(ShallowReceipt::parse_canonical(&bytes[..bytes.len() - 1], &commit[7..]).is_err());
        assert!(ShallowReceipt::parse_canonical(&bytes, &"c".repeat(64)).is_err());
    }

    #[test]
    fn snapshot_auto_config_has_no_implicit_fields_or_aliases() {
        assert_eq!(read_snapshot_auto_config_bytes(b"").unwrap(), None);
        assert_eq!(
            read_snapshot_auto_config_bytes(
                b"[snapshot.auto]\nenabled=true\ninterval_seconds=60\non_change_threshold=2\n",
            )
            .unwrap(),
            Some(SnapshotAutoConfig {
                enabled: true,
                interval_seconds: 60,
                on_change_threshold: 2,
            })
        );
        for invalid in [
            b"[snapshot.auto]\nenabled=true\ninterval_seconds=60\n".as_slice(),
            b"[snapshot.auto]\nenabled=true\ninterval_seconds=0\non_change_threshold=1\n",
            b"[snapshot.auto]\nenabled=true\ninterval_seconds=1\non_change_threshold=1\nlegacy=true\n",
        ] {
            assert!(read_snapshot_auto_config_bytes(invalid).is_err());
        }
    }

    #[test]
    fn gc_scope_config_rejects_device_local_ca_path() {
        let error = parse_config_bytes(
            b"[adapter.policy.offline_api]\nca_pem_path = \"/private/device-ca.pem\"\n",
        )
        .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-CONFIG-NOT-IMPLEMENTED-001");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn snapshot_auto_state_is_canonical_durable_and_monotonic() {
        let legacy = parse_snapshot_auto_state(
            b"{\"last_successful_eligible_attempt_at\":\"2026-08-14T00:00:00Z\",\"version\":1}\n",
        )
        .unwrap_err();
        assert_eq!(legacy.error_code(), "KIO-E-STORE-CORRUPT-001");
        assert!(legacy.message().contains("clean recreation"));

        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init(root.path()).unwrap();
        let session = GcSweepSession::bind(repo.canonical_root().to_path_buf()).unwrap();
        let absent = session.snapshot_auto_state().unwrap();
        assert!(absent.state.is_none());

        let first = session
            .publish_snapshot_auto_state(
                &absent,
                "2026-08-14T00:00:00Z",
                &format!("sha256:{}", "a".repeat(64)),
                true,
            )
            .unwrap();
        assert_eq!(
            first
                .state
                .as_ref()
                .unwrap()
                .last_successful_eligible_attempt_at,
            "2026-08-14T00:00:00Z"
        );
        assert_eq!(
            std::fs::read(repo.kio_dir().join(SNAPSHOT_AUTO_STATE_LEAF)).unwrap(),
            b"{\"idle_observed_since\":\"2026-08-14T00:00:00Z\",\"last_successful_eligible_attempt_at\":\"2026-08-14T00:00:00Z\",\"version\":2,\"working_set_digest\":\"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}\n"
        );
        let backwards = session
            .publish_snapshot_auto_state(
                &first,
                "2026-08-13T23:59:59Z",
                &format!("sha256:{}", "a".repeat(64)),
                true,
            )
            .unwrap_err();
        assert_eq!(backwards.error_code(), "KIO-E-SNAPSHOT-CLOCK-001");

        let second = session
            .publish_snapshot_auto_state(
                &first,
                "2026-08-14T00:01:00Z",
                &format!("sha256:{}", "a".repeat(64)),
                true,
            )
            .unwrap();
        assert_eq!(
            second
                .state
                .as_ref()
                .unwrap()
                .last_successful_eligible_attempt_at,
            "2026-08-14T00:01:00Z"
        );
        let unchanged = session
            .publish_snapshot_auto_state(
                &second,
                "2026-08-14T00:02:00Z",
                &format!("sha256:{}", "a".repeat(64)),
                false,
            )
            .unwrap();
        assert_eq!(
            unchanged
                .state
                .as_ref()
                .unwrap()
                .last_successful_eligible_attempt_at,
            "2026-08-14T00:01:00Z"
        );
        assert_eq!(
            unchanged.state.as_ref().unwrap().idle_observed_since,
            "2026-08-14T00:00:00Z"
        );
        let changed = session
            .publish_snapshot_auto_state(
                &unchanged,
                "2026-08-14T00:03:00Z",
                &format!("sha256:{}", "b".repeat(64)),
                false,
            )
            .unwrap();
        assert_eq!(
            changed
                .state
                .as_ref()
                .unwrap()
                .last_successful_eligible_attempt_at,
            "2026-08-14T00:01:00Z"
        );
        assert_eq!(
            changed.state.as_ref().unwrap().idle_observed_since,
            "2026-08-14T00:03:00Z"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn snapshot_auto_state_retires_only_verified_crash_temporaries() {
        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init(root.path()).unwrap();
        let temporary = repo.kio_dir().join(".snapshot-auto-state-123-456");
        std::fs::write(
            &temporary,
            b"{\"idle_observed_since\":\"2026-08-14T00:00:00Z\",\"last_successful_eligible_attempt_at\":\"2026-08-14T00:00:00Z\",\"version\":2,\"working_set_digest\":\"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}\n",
        )
        .unwrap();

        let session = GcSweepSession::bind(repo.canonical_root().to_path_buf()).unwrap();
        let absent = session.snapshot_auto_state().unwrap();
        session
            .publish_snapshot_auto_state(
                &absent,
                "2026-08-14T00:01:00Z",
                &format!("sha256:{}", "a".repeat(64)),
                true,
            )
            .unwrap();

        assert!(!temporary.exists());
        assert!(std::fs::read_dir(repo.kio_dir()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(SNAPSHOT_AUTO_STATE_TEMP_PREFIX)
        }));
    }

    #[test]
    fn snapshot_auto_state_rejects_malformed_crash_temporary_name() {
        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init(root.path()).unwrap();
        let malformed = repo.kio_dir().join(".snapshot-auto-state-not-a-pid-456");
        std::fs::write(
            &malformed,
            b"{\"idle_observed_since\":\"2026-08-14T00:00:00Z\",\"last_successful_eligible_attempt_at\":\"2026-08-14T00:00:00Z\",\"version\":2,\"working_set_digest\":\"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}\n",
        )
        .unwrap();

        let session = GcSweepSession::bind(repo.canonical_root().to_path_buf()).unwrap();
        let absent = session.snapshot_auto_state().unwrap();
        let error = session
            .publish_snapshot_auto_state(
                &absent,
                "2026-08-14T00:01:00Z",
                &format!("sha256:{}", "a".repeat(64)),
                true,
            )
            .unwrap_err();

        assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
        assert!(malformed.exists());
        assert!(!repo.kio_dir().join(SNAPSHOT_AUTO_STATE_LEAF).exists());
    }

    #[test]
    fn snapshot_auto_state_rejects_noncanonical_and_future_records() {
        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init(root.path()).unwrap();
        let session = GcSweepSession::bind(repo.canonical_root().to_path_buf()).unwrap();
        for invalid in [
            b"{\"last_successful_eligible_attempt_at\":\"2026-08-14T00:00:00Z\",\"version\":2}\n"
                .as_slice(),
            b"{\"version\":1,\"last_successful_eligible_attempt_at\":\"2026-08-14T00:00:00Z\"}\n",
            b"{\"last_successful_eligible_attempt_at\":\"2026-08-14T00:00:00Z\",\"version\":1}",
        ] {
            std::fs::write(repo.kio_dir().join(SNAPSHOT_AUTO_STATE_LEAF), invalid).unwrap();
            assert!(session.snapshot_auto_state().is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_auto_writer_lock_rejects_replaced_public_scope_before_mutation() {
        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init(root.path()).unwrap();
        let session = GcSweepSession::bind(repo.canonical_root().to_path_buf()).unwrap();

        let replacement_root = tempfile::tempdir().unwrap();
        let replacement = Repository::init(replacement_root.path()).unwrap();
        let retained = root.path().join(".kio-retained");
        std::fs::rename(repo.kio_dir(), &retained).unwrap();
        std::fs::rename(replacement.kio_dir(), root.path().join(".kio")).unwrap();

        let error = match session.acquire_snapshot_auto_store_lock() {
            Ok(_) => panic!("replaced public scope acquired a scheduler writer lock"),
            Err(error) => error,
        };
        assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
        assert!(!root.path().join(".kio/.lock").exists());
        assert!(!retained.join(".lock").exists());
    }

    #[test]
    fn marker_rejects_a_serialized_body_larger_than_the_read_bound() {
        let candidates: Vec<_> = (0_u64..40_000)
            .map(|index| GcMarkerCandidate {
                commit_hash: format!("sha256:{index:064x}"),
                tree_hash: format!("sha256:{:064x}", index + 40_000),
                size_bytes: 0,
            })
            .collect();
        let marker = GcInProgressMarker {
            version: 1,
            sweep_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            started_at: "2026-01-01T00:00:00Z".into(),
            phase: GcSweepPhase::Prepared,
            plan_digest: format!("sha256:{}", "1".repeat(64)),
            truth_digest: format!("sha256:{}", "2".repeat(64)),
            stable_truth_digest: format!("sha256:{}", "3".repeat(64)),
            baseline_receipts_digest: format!("sha256:{}", "4".repeat(64)),
            operation_receipts_digest: None,
            trees: candidates
                .iter()
                .map(|candidate| candidate.tree_hash.clone())
                .collect(),
            candidates,
            estimated_bytes: 0,
            index_initial: GcIndexState::Absent,
            index_pre_sweep: None,
            index_final: None,
            index_rotation: None,
        };
        assert!(marker.validate().is_err());
        assert!(marker.canonical_bytes().is_err());
    }

    #[test]
    fn marker_publication_uses_the_marker_bound_not_the_receipt_bound() {
        let shared_tree = format!("sha256:{}", "f".repeat(64));
        let candidates: Vec<_> = (0_u64..8_000)
            .map(|index| GcMarkerCandidate {
                commit_hash: format!("sha256:{index:064x}"),
                tree_hash: shared_tree.clone(),
                size_bytes: 0,
            })
            .collect();
        let marker = GcInProgressMarker {
            version: 1,
            sweep_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            started_at: "2026-01-01T00:00:00Z".into(),
            phase: GcSweepPhase::Prepared,
            plan_digest: format!("sha256:{}", "1".repeat(64)),
            truth_digest: format!("sha256:{}", "2".repeat(64)),
            stable_truth_digest: format!("sha256:{}", "3".repeat(64)),
            baseline_receipts_digest: format!("sha256:{}", "4".repeat(64)),
            operation_receipts_digest: None,
            trees: vec![shared_tree],
            candidates,
            estimated_bytes: 0,
            index_initial: GcIndexState::Absent,
            index_pre_sweep: None,
            index_final: None,
            index_rotation: None,
        };
        let bytes = marker.canonical_bytes().unwrap();
        assert!(bytes.len() as u64 > MAX_METADATA);
        assert!(bytes.len() as u64 <= MAX_MARKER_BYTES);
        let temp = tempfile::tempdir().unwrap();
        let directory = cap_fs::open_ambient_dir(temp.path(), ambient_authority()).unwrap();
        create_new_bound(&directory, "marker", &bytes, MAX_MARKER_BYTES).unwrap();
        assert_eq!(std::fs::read(temp.path().join("marker")).unwrap(), bytes);
    }

    #[test]
    fn marker_rejects_an_estimated_byte_count_above_the_planner_bound() {
        let marker = GcInProgressMarker {
            version: 1,
            sweep_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            started_at: "2026-01-01T00:00:00Z".into(),
            phase: GcSweepPhase::Prepared,
            plan_digest: format!("sha256:{}", "1".repeat(64)),
            truth_digest: format!("sha256:{}", "2".repeat(64)),
            stable_truth_digest: format!("sha256:{}", "3".repeat(64)),
            baseline_receipts_digest: format!("sha256:{}", "4".repeat(64)),
            operation_receipts_digest: None,
            candidates: Vec::new(),
            trees: Vec::new(),
            estimated_bytes: MAX_SWEEP_ESTIMATED_BYTES + 1,
            index_initial: GcIndexState::Absent,
            index_pre_sweep: None,
            index_final: None,
            index_rotation: None,
        };
        assert!(marker.validate().is_err());
        assert!(marker.canonical_bytes().is_err());
    }

    #[test]
    fn retention_is_deterministic_at_bucket_boundary() {
        let now = parse_utc_seconds("2026-01-02T00:00:00Z").unwrap();
        let a = format!("sha256:{}", "a".repeat(64));
        let b = format!("sha256:{}", "b".repeat(64));
        let mut all = HashMap::new();
        all.insert(a.clone(), commit("2026-01-01T00:00:00Z"));
        all.insert(b.clone(), commit("2026-01-01T00:00:00Z"));
        let mut ex = BTreeMap::new();
        assert_eq!(
            auto_retained(
                &HashSet::from([a.clone(), b]),
                &all,
                now,
                &GcPolicy::default(),
                &mut ex
            ),
            HashSet::from([a])
        );
    }

    #[cfg(unix)]
    #[test]
    fn capability_reader_rejects_symlink_and_hardlink() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let dir = cap_fs::open_ambient_dir(tmp.path(), ambient_authority()).unwrap();
        std::fs::write(tmp.path().join("file"), b"ok").unwrap();
        symlink("file", tmp.path().join("link")).unwrap();
        assert!(read_regular(&dir, "link", 16).is_err());
        std::fs::hard_link(tmp.path().join("file"), tmp.path().join("other")).unwrap();
        assert!(read_regular(&dir, "file", 16).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn stability_check_rejects_same_content_identity_replacement() {
        let tmp = tempfile::tempdir().unwrap();
        Repository::init(tmp.path()).unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let head = root.join(".kio/HEAD");
        let replacement = root.join(".kio/HEAD.replacement");
        let bytes = std::fs::read(&head).unwrap();
        let planner = GcPlanner::bind(&root).unwrap();

        let error = planner
            .plan_at_inner(0, || {
                std::fs::write(&replacement, bytes).unwrap();
                std::fs::rename(&replacement, &head).unwrap();
            })
            .unwrap_err();

        assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
    }
}
