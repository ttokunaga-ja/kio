//! Read-only, capability-bound garbage collection planning.
//!
//! Public paths are used only to bind and diagnose a scope.  Every store read
//! below is relative to a retained directory descriptor.

use crate::platform::{
    gc_canonical_index_identity as canonical_gc_index_identity_from_metadata,
    gc_file_state as file_state, gc_publish_staged as publish_staged,
    gc_receipt_handle as open_verified_receipt_durable_handle,
    gc_remove_completed_marker as remove_completed_marker,
    gc_rename_snapshot_state_noreplace as rename_snapshot_state_noreplace,
    gc_retire_snapshot_state_temporary as retire_snapshot_state_temporary,
    gc_same_file_state as same_file_state, gc_sync_bound_directory as sync_bound_directory,
};

use crate::platform::gc_verify_retained_receipt_observation as verify_retained_receipt_observation;

mod construction;
mod marker_impl;
mod planner_impl;
mod session_impl;
pub use construction::{ensure_no_active_sweep_with_control, read_active_marker_with_control};
mod control;
pub use control::{
    ensure_no_active_sweep, read_active_marker, validate_final_shallow_tree,
    validated_final_shallow_receipts,
};
pub mod exchange;
mod observed_read;
use observed_read::read_user_regular_observed;
use observed_read::{
    read_accounted_observed, read_cas_accounted_observed, read_required_accounted_observed,
};
pub(crate) use observed_read::{read_cas_regular_observed, read_regular_observed};
mod unreachable_inventory;
#[cfg(all(test, windows))]
#[path = "gc/windows_snapshot_state_tests.rs"]
mod windows_snapshot_state;

pub use unreachable_inventory::{
    InventoryObject, InventoryStats, ShallowBoundary, UnreachableInventoryLimits,
    UnreachableInventoryPassStats, UnreachableInventoryReport, UnreachableInventorySummary,
    UnreachableObjectInventory,
};

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use cap_primitives::{ambient_authority, fs as cap_fs};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::ExitCode;
use crate::cas::{
    CasReadAccess, MAX_COMMIT_OBJECT_BYTES, MAX_RAW_OBJECT_BYTES, MAX_TREE_OBJECT_BYTES,
    canonical_json_bytes, hash_bytes, is_hash,
};
use crate::dag::{CommitObject, CommitType, MAX_TREE_ENTRIES, TreeObject};
use crate::error::{KioError, Result};
use crate::schema::{SchemaKind, validate_json_schema};
use crate::scope::{
    BoundReentrantStoreLock, BoundStoreLock, KIO_FORMAT_VERSION, Repository,
    enforce_scope_config_semantics, format_utc_seconds, parse_utc_seconds,
    reject_pending_managed_restore,
};
use crate::store_dir::StoreDirectory;

pub(crate) const MAX_METADATA: u64 = 1024 * 1024;
const MAX_REF: u64 = 4096;
pub(crate) const MAX_MARKER_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SWEEP_CANDIDATES: usize = 100_000;
const MAX_SWEEP_ESTIMATED_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub(crate) const SNAPSHOT_AUTO_STATE_LEAF: &str = "snapshot-auto.json";
const SNAPSHOT_AUTO_STATE_VERSION: u32 = 2;
const SNAPSHOT_AUTO_STATE_TEMP_PREFIX: &str = ".snapshot-auto-state-";
/// Scheduler state publishing may leave a private create-new file behind if
/// the process dies after fsync and before the final rename/exchange.  Keep
/// recovery bounded even when a scheduler repeatedly dies at that seam.
pub(crate) const MAX_SNAPSHOT_AUTO_STATE_TEMPORARIES: usize = 64;

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

fn read_active_marker_bound(
    kio: &std::fs::File,
    control: &crate::control::ControlHandle,
) -> Result<Option<GcInProgressMarker>> {
    reject_pending_snapshot_state_exchange(kio, control)?;
    if has_pending_marker_exchange(kio, control)? {
        return Err(corrupt(
            "pending GC marker exchange requires locked recovery",
        ));
    }
    read_marker_without_pending_check(kio)
}

pub(crate) fn has_pending_snapshot_state_exchange(
    kio: &std::fs::File,
    control: &crate::control::ControlHandle,
) -> Result<bool> {
    let mut parent = kio
        .try_clone()
        .map_err(|e| ioerr(e, "snapshot state exchange"))?;
    for leaf in ["gc", "internal", "snapshot-state-exchange"] {
        let Some(child) = open_optional_dir(&parent, leaf)? else {
            return Ok(false);
        };
        parent = child;
    }
    let owner = StoreDirectory::from_retained_with_control(
        parent,
        PathBuf::from("<snapshot state exchange>"),
        control.clone(),
    )?;
    exchange::inspect_pending(&owner)
}

pub(crate) fn reject_pending_snapshot_state_exchange(
    kio: &std::fs::File,
    control: &crate::control::ControlHandle,
) -> Result<()> {
    if has_pending_snapshot_state_exchange(kio, control)? {
        return Err(corrupt(
            "pending snapshot auto state exchange requires locked scheduler recovery",
        ));
    }
    Ok(())
}

pub(crate) fn has_pending_marker_exchange(
    kio: &std::fs::File,
    control: &crate::control::ControlHandle,
) -> Result<bool> {
    let mut parent = kio
        .try_clone()
        .map_err(|e| ioerr(e, "GC marker exchange"))?;
    for leaf in ["gc", "internal", "marker-exchange"] {
        let Some(child) = open_optional_dir(&parent, leaf)? else {
            return Ok(false);
        };
        parent = child;
    }
    let owner = StoreDirectory::from_retained_with_control(
        parent,
        PathBuf::from("<GC marker exchange>"),
        control.clone(),
    )?;
    exchange::inspect_pending(&owner)
}

fn read_marker_without_pending_check(kio: &std::fs::File) -> Result<Option<GcInProgressMarker>> {
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

/// Retained-authority counterpart for repository and scheduler writers which
/// already own the exact `.kio` directory and must not reopen its pathname.
pub(crate) fn ensure_no_active_sweep_bound(
    kio: &std::fs::File,
    control: &crate::control::ControlHandle,
) -> Result<()> {
    if let Some(marker) = read_active_marker_bound(kio, control)? {
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
pub fn validated_final_shallow_receipts_with_control(
    kio_dir: &Path,
    control: &crate::control::ControlHandle,
) -> Result<BTreeMap<String, String>> {
    // Bind the supplied `.kio` spelling before any canonical diagnostic
    // lookup.  This rejects a symlink/reparse replacement at the public
    // boundary; `GcPlanner::bind` may subsequently canonicalize the already
    // verified scope only to normalize macOS's `/var` spelling.
    let bound_kio_path = absolute_lexical_path(kio_dir)?;
    let root = bound_kio_path
        .parent()
        .ok_or_else(|| corrupt("Kio directory has no scope root"))?;
    let planner = GcPlanner::bind_with_control(root.to_path_buf(), control.clone())?;
    let supplied_kio = open_bound_absolute(&bound_kio_path)?;
    if id_file(&supplied_kio)? != id_file(&planner.kio)? {
        return Err(corrupt(
            "Kio directory changed while binding shallow inventory",
        ));
    }
    validated_final_shallow_receipts_bound(&planner)
}

fn validated_final_shallow_receipts_bound(planner: &GcPlanner) -> Result<BTreeMap<String, String>> {
    planner.reads.charge(0)?;
    let root_id = id_file(&planner.scope)?;
    let kio_id = id_file(&planner.kio)?;
    planner.require_layout()?;
    let first = inventory_final_shallow_receipts(planner)?;
    let second = inventory_final_shallow_receipts(planner)?;
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
    Ok(crate::platform::normalize_gc_scope_path(&absolute))
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
    if let Some(marker) = read_active_marker_bound(&planner.kio, &planner.control)? {
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
                        match read_cas_regular_observed(
                            &planner.reads,
                            &second,
                            raw,
                            MAX_TREE_OBJECT_BYTES,
                        ) {
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
pub fn validate_final_shallow_tree_with_control(
    kio_dir: &Path,
    commit_hash: &str,
    tree_hash: &str,
    control: &crate::control::ControlHandle,
) -> Result<()> {
    validate_final_shallow_receipt(
        &validated_final_shallow_receipts_with_control(kio_dir, control)?,
        commit_hash,
        tree_hash,
    )
}

fn validate_final_shallow_receipt(
    receipts: &BTreeMap<String, String>,
    commit_hash: &str,
    tree_hash: &str,
) -> Result<()> {
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
    reads: CasReadAccess,
    lock_session: crate::scope::LockSession,
    root: PathBuf,
    scope: std::fs::File,
    kio: std::fs::File,
    control: crate::control::ControlHandle,
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

/// Immutable common-domain proof; native exchange only receives its binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SnapshotExchangeAuthority {
    pub(crate) binding: String,
    pub(crate) head_observation: FileObservation,
    pub(crate) tool_observation: FileObservation,
}

impl SnapshotAutoBinding {
    /// Re-read the complete effective ignore authority through the retained
    /// scope and `.kio` capabilities.  This is intentionally an exact
    /// observation comparison rather than a semantic comparison: replacements
    /// and same-byte edits are authorization changes for scheduled writers.
    pub(crate) fn recheck(&self, scope: &std::fs::File, kio: &std::fs::File) -> Result<()> {
        if id_file(scope)? != self.scope_identity
            || id_file(kio)? != self.kio_identity
            || id_child(scope, ".kio", &self.kio_identity)? != self.kio_identity
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
        let root_ignore_observation =
            match read_user_regular_observed(scope, ".kioignore", MAX_METADATA) {
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
    pub(crate) fn recheck(
        &self,
        kio: &std::fs::File,
        control: &crate::control::ControlHandle,
    ) -> Result<()> {
        reject_pending_snapshot_state_exchange(kio, control)?;
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

pub(crate) fn ensure_child_dir(parent: &std::fs::File, name: &str) -> Result<std::fs::File> {
    let retained = crate::store_dir::StoreDirectory::from_retained(
        parent.try_clone().map_err(|error| ioerr(error, name))?,
        PathBuf::from(name),
    )?;
    crate::private_fs::verify_private_directory_handle(&retained)?;
    let dir = match open_optional_dir(parent, name)? {
        Some(dir) => dir,
        None => {
            let dir = retained.create_directory(Path::new(name))?;
            sync_bound_directory(parent, name)?;
            dir
        }
    };
    let child = crate::store_dir::StoreDirectory::from_retained(
        dir.try_clone().map_err(|error| ioerr(error, name))?,
        PathBuf::from(name),
    )?;
    crate::private_fs::verify_private_directory_handle(&child)?;
    Ok(dir)
}
pub(crate) fn create_new_bound(
    dir: &std::fs::File,
    leaf: &str,
    bytes: &[u8],
    max: u64,
) -> Result<()> {
    let retained = crate::store_dir::StoreDirectory::from_retained(
        dir.try_clone().map_err(|error| ioerr(error, leaf))?,
        PathBuf::from(leaf),
    )?;
    crate::private_fs::verify_private_directory_handle(&retained)?;
    let mut file = retained.create_private_file(Path::new(leaf))?;
    file.write_all(bytes).map_err(|e| ioerr(e, leaf))?;
    file.sync_all().map_err(|e| ioerr(e, leaf))?;
    drop(file);
    let (_, observed) = read_regular_observed(dir, leaf, max)?;
    if observed.digest != hash_bytes(bytes) {
        return Err(corrupt("receipt changed after create"));
    }
    sync_bound_directory(dir, leaf)
}

/// Retire only scheduler state temporaries which could have been produced by
/// [`unique_internal_name`].  This runs under the scheduler writer lock,
/// before allocating the next temporary.  Names outside this narrow private
/// grammar are deliberately ignored; a name in the namespace which does not
/// conform is evidence of interference and stops publication without
/// deleting anything.
fn cleanup_snapshot_auto_state_temporaries(
    dir: &std::fs::File,
    control: &crate::control::ControlHandle,
) -> Result<()> {
    reject_pending_snapshot_state_exchange(dir, control)?;
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
        retire_snapshot_state_temporary(dir, &temporary, &bytes, &observation, control)?;
    }
    // The individual retirement operation syncs the directory, including the
    // no-op case this makes the cleanup completion boundary explicit.
    sync_bound_directory(dir, "snapshot auto temporary cleanup")
}

pub(crate) fn is_snapshot_auto_state_temporary_name(name: &str) -> bool {
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

fn validate_marker_transition(
    current: &GcInProgressMarker,
    marker: &GcInProgressMarker,
) -> Result<()> {
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
        || !valid_index_marker_delta(current, marker)
    {
        return Err(corrupt("invalid GC marker transition"));
    }
    Ok(())
}

pub(crate) fn unique_internal_name(prefix: &str) -> String {
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
pub(crate) struct FileObservation {
    pub(crate) identity: Identity,
    pub(crate) state: FileState,
    pub(crate) digest: String,
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
    reads: CasReadAccess,
    control: crate::control::ControlHandle,
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

pub(crate) fn parse_snapshot_auto_state(bytes: &[u8]) -> Result<SnapshotAutoState> {
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

pub(crate) fn snapshot_auto_state_changed() -> KioError {
    KioError::new(
        "KIO-E-SNAPSHOT-STATE-CHANGED-001",
        "scheduled snapshot state changed during the locked eligibility handoff",
        json!({}),
        ExitCode::PartialFailure,
    )
}

pub(crate) fn snapshot_auto_authority_changed() -> KioError {
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

pub(crate) fn snapshot_auto_unsafe_entry(message: &str) -> KioError {
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
            crate::platform::validate_gc_direct_directory_metadata(&before)?;
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
        d = open_dir_os(&d, x)?;
        let retained = crate::store_dir::StoreDirectory::from_retained(
            d.try_clone().map_err(|error| ioerr(error, path))?,
            PathBuf::from(path),
        )?;
        crate::private_fs::verify_private_directory_handle(&retained)?;
    }
    Ok(d)
}
fn open_dir(base: &std::fs::File, n: &str) -> Result<std::fs::File> {
    let directory = open_dir_os(base, std::ffi::OsStr::new(n))?;
    let retained = crate::store_dir::StoreDirectory::from_retained(
        directory.try_clone().map_err(|error| ioerr(error, n))?,
        PathBuf::from(n),
    )?;
    crate::private_fs::verify_private_directory_handle(&retained)?;
    Ok(directory)
}

/// Open one optional direct-child directory without following its final
/// component. Only an exact `NotFound` is absence; every other failure remains
/// observable so an unsafe entry cannot masquerade as an empty namespace.
pub(crate) fn open_optional_dir(base: &std::fs::File, n: &str) -> Result<Option<std::fs::File>> {
    let path = Path::new(n);
    let directory = match cap_fs::open_dir_nofollow(base, path) {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ioerr(error, n)),
    };
    validate_directory_handle(&directory, n)?;
    let retained = crate::store_dir::StoreDirectory::from_retained(
        directory.try_clone().map_err(|error| ioerr(error, n))?,
        PathBuf::from(n),
    )?;
    crate::private_fs::verify_private_directory_handle(&retained)?;
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

pub(crate) fn validate_directory_handle(
    directory: &std::fs::File,
    display: impl AsRef<Path>,
) -> Result<()> {
    let metadata =
        cap_fs::Metadata::from_file(directory).map_err(|error| ioerr(error, display.as_ref()))?;
    if !metadata.is_dir() {
        return Err(corrupt("expected real directory"));
    }
    crate::platform::validate_gc_directory_metadata(&metadata)
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
#[cfg(all(test, unix))]
fn read_regular(d: &std::fs::File, n: &str, max: u64) -> Result<Vec<u8>> {
    read_regular_observed(d, n, max).map(|(bytes, _)| bytes)
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

pub(crate) fn valid_file(m: &cap_fs::Metadata, max: u64) -> Result<()> {
    if !m.is_file() || m.len() > max {
        return Err(if m.len() > max {
            limit("file bytes")
        } else {
            corrupt("non-regular store entry")
        });
    }
    crate::platform::validate_gc_file_links(m)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Identity(
    #[serde(serialize_with = "crate::identity_serde::u64_hex::serialize")] pub(crate) u64,
    #[serde(serialize_with = "crate::identity_serde::u64_hex::serialize")] pub(crate) u64,
);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct FileState {
    pub(crate) len: u64,
    pub(crate) modified_seconds: i64,
    pub(crate) modified_nanos: i64,
    pub(crate) changed_seconds: i64,
    pub(crate) changed_nanos: i64,
}

pub(crate) fn id_meta(m: &cap_fs::Metadata) -> Result<Identity> {
    crate::platform::gc_metadata_identity(m)
}

fn id_file(f: &std::fs::File) -> Result<Identity> {
    id_meta(&cap_fs::Metadata::from_file(f).map_err(|e| ioerr(e, "scope"))?)
}
fn id_path(p: &Path) -> Result<Identity> {
    let f = open_bound_absolute(p)?;
    id_file(&f)
}
fn id_child(d: &std::fs::File, n: &str, expected: &Identity) -> Result<Identity> {
    let child = open_dir_os(d, std::ffi::OsStr::new(n))?;
    let observed = id_file(&child)?;
    // A replacement reaches the caller's identity mismatch guard. The same
    // admitted object must still pass privacy checks before any owned read.
    if &observed == expected {
        let directory = crate::store_dir::StoreDirectory::from_retained(child, PathBuf::from(n))?;
        crate::private_fs::verify_private_directory_handle(&directory)?;
    }
    Ok(observed)
}
pub(crate) fn is_io_not_found(error: &KioError) -> bool {
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
    crate::platform::is_canonical_gc_index_identity(value)
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
pub(crate) fn corrupt(m: &str) -> KioError {
    KioError::new(
        "KIO-E-STORE-CORRUPT-001",
        m,
        json!({}),
        ExitCode::PermanentFailure,
    )
}
pub(crate) fn limit(w: &str) -> KioError {
    KioError::new(
        "KIO-E-GC-PLAN-LIMIT-001",
        format!("GC planning limit exceeded: {w}"),
        json!({"limit":w}),
        ExitCode::PermanentFailure,
    )
}
pub(crate) fn ioerr(e: std::io::Error, p: impl AsRef<Path>) -> KioError {
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
    fn private_fixture_directory(
        root: &std::path::Path,
        relative: &std::path::Path,
    ) -> crate::store_dir::StoreDirectory {
        let root = root.canonicalize().unwrap();
        let external = crate::store_dir::StoreDirectory::open(&root).unwrap();
        let handle = external.create_directory_all(relative).unwrap();
        let directory =
            crate::store_dir::StoreDirectory::from_retained(handle, root.join(relative)).unwrap();
        crate::private_fs::verify_private_directory_handle(&directory).unwrap();
        directory
    }
    use super::*;
    use crate::dag::CommitStats;
    use crate::platform::gc_snapshot_logic::validate_snapshot_state_transition;
    use crate::scope::Repository;

    #[test]
    fn shallow_receipt_verification_charges_both_physical_commit_inventory_passes() {
        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init(root.path()).unwrap();
        std::fs::write(root.path().join("file.txt"), b"gc budget fixture").unwrap();
        repo.snapshot(Some("gc budget fixture"), Some("2026-08-14T00:00:00Z"))
            .unwrap();
        let head = repo.head_commit_hash().unwrap().unwrap();
        let commit_bytes = repo
            .object_store()
            .read_object(crate::cas::ObjectKind::Commit, &head)
            .unwrap()
            .bytes
            .len() as u64;
        let expected = 2 * commit_bytes;
        let owner = crate::cas::VerifiedCasReadBudget::new();
        let (receipts, consumed) = owner
            .run(expected, |reads| {
                let metered = repo.with_verified_reads(reads)?;
                GcSweepSession::bind_repository(&metered)?.validated_final_shallow_receipts()
            })
            .unwrap();
        assert!(receipts.is_empty());
        assert_eq!(consumed, expected);
        let error = owner
            .run(expected - 1, |reads| {
                let metered = repo.with_verified_reads(reads)?;
                GcSweepSession::bind_repository(&metered)?.validated_final_shallow_receipts()
            })
            .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-VERIFIED-BYTES-LIMIT-001");
    }

    #[test]
    fn pending_marker_exchange_blocks_readers_when_public_marker_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        let owner = temp.path().join("gc/internal/marker-exchange");
        let owned =
            private_fixture_directory(temp.path(), Path::new("gc/internal/marker-exchange"));
        owned
            .append_private(Path::new("intent.json"), b"incomplete intent")
            .unwrap();
        let bound = open_bound_absolute(&std::fs::canonicalize(temp.path()).unwrap()).unwrap();
        assert!(
            has_pending_marker_exchange(&bound, &crate::control::ControlHandle::default()).unwrap()
        );
        assert!(
            read_active_marker_bound(&bound, &crate::control::ControlHandle::default()).is_err()
        );
        assert!(ensure_no_active_sweep(&std::fs::canonicalize(temp.path()).unwrap()).is_err());
        assert_eq!(
            std::fs::read(owner.join("intent.json")).unwrap(),
            b"incomplete intent"
        );
        assert!(!temp.path().join("gc/in_progress").exists());
    }

    #[test]
    fn unexpected_marker_exchange_residue_fails_closed_without_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        let owner = temp.path().join("gc/internal/marker-exchange");
        let _owned =
            private_fixture_directory(temp.path(), Path::new("gc/internal/marker-exchange"));
        std::fs::write(owner.join("foreign"), b"preserve").unwrap();
        let bound = open_bound_absolute(&std::fs::canonicalize(temp.path()).unwrap()).unwrap();
        assert!(
            has_pending_marker_exchange(&bound, &crate::control::ControlHandle::default()).is_err()
        );
        assert!(
            read_active_marker_bound(&bound, &crate::control::ControlHandle::default()).is_err()
        );
        assert_eq!(std::fs::read(owner.join("foreign")).unwrap(), b"preserve");
    }

    #[test]
    fn empty_marker_exchange_owner_is_read_only_and_inactive() {
        let temp = tempfile::tempdir().unwrap();
        let owner = temp.path().join("gc/internal/marker-exchange");
        let _owned =
            private_fixture_directory(temp.path(), Path::new("gc/internal/marker-exchange"));
        let bound = open_bound_absolute(&std::fs::canonicalize(temp.path()).unwrap()).unwrap();
        assert!(
            !has_pending_marker_exchange(&bound, &crate::control::ControlHandle::default())
                .unwrap()
        );
        assert_eq!(
            read_active_marker_bound(&bound, &crate::control::ControlHandle::default()).unwrap(),
            None
        );
        assert_eq!(std::fs::read_dir(owner).unwrap().count(), 0);
    }

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

    #[test]
    fn snapshot_state_pending_journal_is_not_a_fresh_missing_checkpoint() {
        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init(root.path()).unwrap();
        let session = GcSweepSession::bind(repo.canonical_root().to_path_buf()).unwrap();
        assert!(session.snapshot_auto_state().unwrap().state.is_none());
        let owner = repo
            .canonical_root()
            .join(".kio/gc/internal/snapshot-state-exchange");
        let owned = private_fixture_directory(
            repo.kio_dir(),
            Path::new("gc/internal/snapshot-state-exchange"),
        );
        owned
            .append_private(
                Path::new("intent.json"),
                b"foreign pending Windows snapshot state",
            )
            .unwrap();
        assert!(session.has_pending_snapshot_auto_state_exchange().unwrap());
        assert!(session.snapshot_auto_state().is_err());
        assert!(ensure_no_active_sweep_bound(&session.kio, &session.control).is_err());
        assert_eq!(
            std::fs::read(owner.join("intent.json")).unwrap(),
            b"foreign pending Windows snapshot state"
        );
    }

    #[test]
    fn snapshot_state_transition_matches_scheduled_publication_rules() {
        let old = SnapshotAutoState {
            version: 2,
            last_successful_eligible_attempt_at: "2026-09-27T00:00:00Z".into(),
            idle_observed_since: "2026-09-27T00:00:01Z".into(),
            working_set_digest: format!("sha256:{}", "a".repeat(64)),
        };
        let mut new = old.clone();
        assert!(validate_snapshot_state_transition(&old, &new).is_ok());
        new.last_successful_eligible_attempt_at = "2026-09-27T00:00:02Z".into();
        assert!(validate_snapshot_state_transition(&old, &new).is_ok());
        new.idle_observed_since = "2026-09-27T00:00:02Z".into();
        assert!(validate_snapshot_state_transition(&old, &new).is_err());
        new.working_set_digest = format!("sha256:{}", "b".repeat(64));
        assert!(validate_snapshot_state_transition(&old, &new).is_ok());
        new.last_successful_eligible_attempt_at = old.last_successful_eligible_attempt_at.clone();
        assert!(validate_snapshot_state_transition(&old, &new).is_ok());
        new.last_successful_eligible_attempt_at = "2026-09-27T00:00:01Z".into();
        assert!(validate_snapshot_state_transition(&old, &new).is_err());
        new.idle_observed_since = "2026-09-26T23:59:59Z".into();
        assert!(validate_snapshot_state_transition(&old, &new).is_err());
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
        crate::store_dir::StoreDirectory::open(repo.kio_dir()).unwrap().append_private(
            Path::new(".snapshot-auto-state-123-456"),
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

    #[test]
    fn bound_repository_session_rejects_replaced_public_kio() {
        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init(root.path()).unwrap();
        assert!(repo.bound_kio_handle().is_some());
        GcSweepSession::bind_repository(&repo).unwrap();
        let replacement_root = tempfile::tempdir().unwrap();
        let replacement = Repository::init(replacement_root.path()).unwrap();
        std::fs::rename(repo.kio_dir(), root.path().join(".kio-retained")).unwrap();
        std::fs::rename(replacement.kio_dir(), root.path().join(".kio")).unwrap();
        let error = match GcSweepSession::bind_repository(&repo) {
            Ok(_) => panic!("mixed retained repository and replacement public store"),
            Err(error) => error,
        };
        assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
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
        let owned = private_fixture_directory(temp.path(), Path::new("owned"));
        create_new_bound(&owned.root_handle(), "marker", &bytes, MAX_MARKER_BYTES).unwrap();
        assert_eq!(std::fs::read(owned.path().join("marker")).unwrap(), bytes);
    }

    #[test]
    fn owned_gc_reads_reject_nonprivate_marker_receipt_and_state_without_mutation() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        let external = crate::store_dir::StoreDirectory::open(&root).unwrap();
        let owned = crate::store_dir::StoreDirectory::from_retained(
            external.create_directory(Path::new("owned")).unwrap(),
            root.join("owned"),
        )
        .unwrap();
        let bytes = b"unchanged private GC record";
        for leaf in ["in_progress", "receipt", "snapshot-auto.json"] {
            owned.append_private(Path::new(leaf), bytes).unwrap();
            let path = owned.path().join(leaf);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            }
            #[cfg(windows)]
            {
                let status = std::process::Command::new("icacls.exe")
                    .arg(&path)
                    .args(["/grant", "*S-1-1-0:R"])
                    .status()
                    .unwrap();
                assert!(status.success());
            }
            let before = owned.entries(Path::new("")).unwrap();
            let error = read_regular_observed(&owned.root_handle(), leaf, MAX_METADATA)
                .expect_err("GC must reject unsafe owned bytes before hashing or trusting them");
            assert_eq!(error.error_code(), "KIO-E-PRIVATE-FILE-UNSAFE-001");
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert_eq!(owned.entries(Path::new("")).unwrap(), before);
        }
    }

    #[test]
    fn snapshot_authority_accepts_user_ignore_permissions_without_repair() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        let repo = Repository::init(&root).unwrap();
        let ignore = root.join(".kioignore");
        std::fs::write(&ignore, b"ignored.md\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&ignore, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        fn permissions(path: &Path) -> Vec<u8> {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::metadata(path)
                    .unwrap()
                    .permissions()
                    .mode()
                    .to_le_bytes()
                    .to_vec()
            }
            #[cfg(windows)]
            {
                let output = std::process::Command::new("icacls.exe")
                    .arg(path)
                    .output()
                    .unwrap();
                assert!(output.status.success());
                output.stdout
            }
        }
        let before = permissions(&ignore);
        let session = GcSweepSession::bind_repository(&repo).unwrap();
        let authority = session.snapshot_auto_binding().unwrap();
        authority.recheck(&session.scope, &session.kio).unwrap();
        assert_eq!(permissions(&ignore), before);
        assert_eq!(std::fs::read(&ignore).unwrap(), b"ignored.md\n");
        // User-file permission admission must not relax its exact authority
        // observation: a content change still invalidates the capture.
        std::fs::write(&ignore, b"other.md\n").unwrap();
        assert!(authority.recheck(&session.scope, &session.kio).is_err());
        assert_eq!(permissions(&ignore), before);
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
    fn same_identity_kio_permission_drift_is_refused_without_repair() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = tempfile::tempdir().unwrap();
        let repo = Repository::init(fixture.path()).unwrap();
        let planner = GcPlanner::bind(repo.canonical_root().to_path_buf()).unwrap();
        let session = GcSweepSession::bind_repository(&repo).unwrap();
        let authority = session.snapshot_auto_binding().unwrap();
        let inventory =
            UnreachableObjectInventory::bind(repo.canonical_root().to_path_buf()).unwrap();
        let original_identity = id_file(&session.kio).unwrap();
        let head_before = std::fs::read(repo.kio_dir().join("HEAD")).unwrap();
        std::fs::set_permissions(repo.kio_dir(), std::fs::Permissions::from_mode(0o775)).unwrap();
        assert_eq!(id_file(&session.kio).unwrap(), original_identity);
        for result in [
            planner.plan_at(0).map(|_| ()),
            session.snapshot_auto_binding().map(|_| ()),
            authority.recheck(&session.scope, &session.kio),
            inventory.inventory().map(|_| ()),
        ] {
            assert_eq!(
                result.unwrap_err().error_code(),
                "KIO-E-PRIVATE-FILE-UNSAFE-001"
            );
        }
        assert_eq!(
            std::fs::metadata(repo.kio_dir())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o775
        );
        assert_eq!(
            std::fs::read(repo.kio_dir().join("HEAD")).unwrap(),
            head_before
        );
        assert!(!repo.kio_dir().join("gc").exists());
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
                let directory =
                    crate::store_dir::StoreDirectory::open(head.parent().unwrap()).unwrap();
                directory
                    .append_private(Path::new("HEAD.replacement"), &bytes)
                    .unwrap();
                std::fs::rename(&replacement, &head).unwrap();
            })
            .unwrap_err();

        assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
    }
}
