//! Descriptor-bound, read-only inventory of physical CAS objects.
//!
//! This module intentionally has no deletion plan or executor. Its output is
//! diagnostic evidence only and is never mutation authority.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use cap_primitives::fs as cap_fs;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::*;
use crate::cas::{
    ChunkObject, EmbeddingObject, MAX_CHUNK_OBJECT_BYTES, MAX_EMBEDDING_OBJECT_BYTES,
    MAX_MANIFEST_OBJECT_BYTES, MAX_NORMALIZED_UNIT_OBJECT_BYTES, canonical_json_bytes, lower_hex,
};
use crate::purge::{
    CanonicalFinalEvent, EraseReceipt, EventKind, TombstoneRecord, canonical_final_event,
    parse_erase_receipt_bytes, parse_tombstone_bytes, timestamp_is_after, verify_marker_binding,
};
use crate::scope::{acquire_bound_store_read_guard, canonical_tool_lock_value, now_utc_seconds};

const CAS_KINDS: [(&str, &str, u64, bool); 10] = [
    ("commit", "commits", MAX_COMMIT_OBJECT_BYTES, true),
    ("tree", "trees", MAX_TREE_OBJECT_BYTES, true),
    ("raw", "raw", MAX_RAW_OBJECT_BYTES, false),
    ("chunk", "chunks", MAX_CHUNK_OBJECT_BYTES, true),
    ("manifest", "manifests", MAX_MANIFEST_OBJECT_BYTES, true),
    (
        "normalized_unit",
        "normalized_unit_objects",
        MAX_NORMALIZED_UNIT_OBJECT_BYTES,
        true,
    ),
    ("embedding", "embeddings", MAX_EMBEDDING_OBJECT_BYTES, true),
    ("toollock", "toollocks", MAX_METADATA, true),
    ("prepared", "prepared", MAX_RAW_OBJECT_BYTES, false),
    ("image", "image", MAX_RAW_OBJECT_BYTES, false),
];

const NON_CAS_OBJECT_DIRECTORIES: [&str; 2] = ["normalized", "normalized_units"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnreachableInventoryLimits {
    pub max_objects: u64,
    pub max_physical_bytes: u64,
    pub max_verified_bytes: u64,
    pub max_manifest_units: u64,
    pub max_history_steps: u64,
    pub max_refs: u64,
    pub max_receipts: u64,
    pub max_directory_entries: u64,
    pub max_name_bytes: u64,
    pub max_depth: u64,
}

impl Default for UnreachableInventoryLimits {
    fn default() -> Self {
        Self {
            max_objects: 100_000,
            max_physical_bytes: 4 * 1024 * 1024 * 1024,
            max_verified_bytes: 4 * 1024 * 1024 * 1024,
            max_manifest_units: 10_000_000,
            max_history_steps: 10_000_000,
            max_refs: 10_000,
            max_receipts: 100_000,
            max_directory_entries: 200_000,
            max_name_bytes: 255,
            max_depth: 6,
        }
    }
}

#[derive(Debug)]
pub struct UnreachableObjectInventory {
    root: PathBuf,
    scope: std::fs::File,
    kio: std::fs::File,
    limits: UnreachableInventoryLimits,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct InventoryObject {
    kind: String,
    hash: String,
    physical_bytes: u64,
    classification: String,
    reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ShallowBoundary {
    commit_hash: String,
    tree_hash: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
struct InventoryStats {
    objects: u64,
    physical_bytes: u64,
    verified_bytes: u64,
    refs: u64,
    receipts: u64,
    manifest_units: u64,
    history_steps: u64,
    directory_entries: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pass {
    objects: Vec<InventoryObject>,
    shallow_boundaries: Vec<ShallowBoundary>,
    stats: InventoryStats,
    observations: BTreeMap<String, FileObservation>,
}

#[derive(Debug, Clone)]
struct NormalizedUnitIdentity {
    unit_key: String,
    unit_type: String,
    raw_hash: String,
    prepared_hash: String,
    preparation_profile_hash: String,
    tool_profile_hash: String,
    generation: u64,
}

#[derive(Debug, Clone)]
struct ManifestPin {
    unit_key: String,
    unit_type: String,
    raw_hash: String,
    prepared_hash: String,
    preparation_profile_hash: String,
    tool_profile_hash: String,
    generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct TreeManifestEdge {
    raw_hash: String,
    tree_hash: String,
    tool_profile_hash: String,
    generation: u64,
}

#[derive(Debug, Clone)]
struct ManifestIdentity {
    raw_hash: String,
    tool_profile_hash: String,
    generation: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InventoryManifest {
    raw_hash: String,
    tool_profile_hash: String,
    #[serde(rename = "gen")]
    generation: u64,
    #[serde(rename = "parent_gen")]
    _parent_generation: Option<u64>,
    run_id: String,
    units: Vec<InventoryManifestUnit>,
    generated_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InventoryManifestUnit {
    order: u64,
    unit_key: String,
    unit_ref: String,
    unit_type: String,
    status: String,
    prepared_hash: String,
    preparation_profile_hash: String,
    #[serde(deserialize_with = "deserialize_required_nullable_hash")]
    unit_object_hash: Option<String>,
    error_kind: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InventoryNormalizedUnit {
    unit_key: String,
    unit_type: String,
    raw_hash: String,
    prepared_hash: String,
    preparation_profile_hash: String,
    tool_profile_hash: String,
    #[serde(rename = "gen")]
    generation: u64,
    mode: String,
    markdown: String,
    #[serde(deserialize_with = "deserialize_owned_image_hashes")]
    owned_image_hashes: BTreeSet<String>,
    metadata: BTreeMap<String, Value>,
    #[serde(deserialize_with = "deserialize_required_nullable_reused_from")]
    reused_from: Option<InventoryReusedFrom>,
    generated_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InventoryReusedFrom {
    raw_hash: String,
    #[serde(rename = "gen")]
    generation: u64,
    unit_key: String,
}

fn deserialize_required_nullable_hash<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)
}

fn deserialize_owned_image_hashes<'de, D>(
    deserializer: D,
) -> std::result::Result<BTreeSet<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = Vec::<String>::deserialize(deserializer)?;
    if values.len() > 256 {
        return Err(serde::de::Error::custom(
            "owned_image_hashes exceeds per-unit image limit",
        ));
    }
    let mut hashes = BTreeSet::new();
    for hash in values {
        if !crate::cas::is_hash(&hash) {
            return Err(serde::de::Error::custom(
                "owned_image_hashes contains an invalid hash",
            ));
        }
        if !hashes.insert(hash) {
            return Err(serde::de::Error::custom(
                "owned_image_hashes contains a duplicate hash",
            ));
        }
    }
    Ok(hashes)
}

fn deserialize_required_nullable_reused_from<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<InventoryReusedFrom>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<InventoryReusedFrom>::deserialize(deserializer)
}

struct ScanState<'a> {
    limits: &'a UnreachableInventoryLimits,
    walker_limits: GcPlanLimits,
    walker_stats: GcPlanStats,
    stats: InventoryStats,
    observations: BTreeMap<String, FileObservation>,
    physical: BTreeMap<(String, String), u64>,
    commits: BTreeMap<String, CommitObject>,
    trees: BTreeMap<String, TreeObject>,
    tree_manifests: BTreeMap<String, BTreeSet<TreeManifestEdge>>,
    commit_toollocks: BTreeSet<String>,
    manifests: BTreeMap<String, ManifestIdentity>,
    manifest_pins: BTreeMap<String, Vec<ManifestPin>>,
    normalized_units: BTreeMap<String, NormalizedUnitIdentity>,
    historical_unit_uncertainty: BTreeSet<String>,
    // A valid shallow receipt proves that an old tree was intentionally
    // discarded, but not the contents of that tree.  That missing closure can
    // have referenced any otherwise-orphaned manifest or normalized unit.
    shallow_closure_uncertainty: bool,
    chunk_text_hashes: BTreeSet<String>,
    embedding_targets: BTreeMap<String, (String, String)>,
    images: BTreeSet<String>,
    refs: BTreeMap<String, String>,
    receipts: BTreeMap<String, ShallowReceipt>,
    reachable_commits: BTreeSet<String>,
    tombstones: BTreeMap<String, TombstoneRecord>,
    erase_receipts: BTreeMap<String, EraseReceipt>,
    final_events: BTreeMap<String, CanonicalFinalEvent>,
    purge_epoch: Option<u64>,
    lifecycle_epoch: u64,
    gc_barrier: Option<Option<FileObservation>>,
    purge_barrier: Option<Option<(FileObservation, u64)>>,
    lifecycle_barrier: Option<Option<(FileObservation, Option<u64>)>>,
}

impl<'a> ScanState<'a> {
    fn new(limits: &'a UnreachableInventoryLimits) -> Self {
        let walker_limits = GcPlanLimits {
            max_refs: limits.max_refs,
            max_receipts: limits.max_receipts,
            max_dir_entries: limits.max_directory_entries,
            max_name_bytes: limits.max_name_bytes,
            max_depth: limits.max_depth,
            max_verified_bytes: limits.max_verified_bytes,
            ..GcPlanLimits::default()
        };
        Self {
            limits,
            walker_limits,
            walker_stats: GcPlanStats::default(),
            stats: InventoryStats::default(),
            observations: BTreeMap::new(),
            physical: BTreeMap::new(),
            commits: BTreeMap::new(),
            trees: BTreeMap::new(),
            tree_manifests: BTreeMap::new(),
            commit_toollocks: BTreeSet::new(),
            manifests: BTreeMap::new(),
            manifest_pins: BTreeMap::new(),
            normalized_units: BTreeMap::new(),
            historical_unit_uncertainty: BTreeSet::new(),
            shallow_closure_uncertainty: false,
            chunk_text_hashes: BTreeSet::new(),
            embedding_targets: BTreeMap::new(),
            images: BTreeSet::new(),
            refs: BTreeMap::new(),
            receipts: BTreeMap::new(),
            reachable_commits: BTreeSet::new(),
            tombstones: BTreeMap::new(),
            erase_receipts: BTreeMap::new(),
            final_events: BTreeMap::new(),
            purge_epoch: None,
            lifecycle_epoch: 0,
            gc_barrier: None,
            purge_barrier: None,
            lifecycle_barrier: None,
        }
    }

    fn account_verified(&mut self, bytes: u64) -> Result<()> {
        self.stats.verified_bytes = self
            .stats
            .verified_bytes
            .checked_add(bytes)
            .ok_or_else(|| limit("verified bytes"))?;
        if self.stats.verified_bytes > self.limits.max_verified_bytes {
            return Err(limit("verified bytes"));
        }
        Ok(())
    }

    fn add_object(&mut self, kind: &str, hash: String, bytes: u64) -> Result<()> {
        self.stats.objects = self
            .stats
            .objects
            .checked_add(1)
            .ok_or_else(|| limit("object count"))?;
        if self.stats.objects > self.limits.max_objects {
            return Err(limit("object count"));
        }
        self.stats.physical_bytes = self
            .stats
            .physical_bytes
            .checked_add(bytes)
            .ok_or_else(|| limit("physical bytes"))?;
        if self.stats.physical_bytes > self.limits.max_physical_bytes {
            return Err(limit("physical bytes"));
        }
        if self
            .physical
            .insert((kind.to_owned(), hash), bytes)
            .is_some()
        {
            return Err(corrupt("duplicate physical CAS object"));
        }
        Ok(())
    }

    fn observe_directory(&mut self, directory: &std::fs::File, name: &str) -> Result<()> {
        super::observe_directory(directory, name, &mut self.observations)
    }

    fn observe_file(&mut self, name: &str, observation: FileObservation, bytes: u64) -> Result<()> {
        self.account_verified(bytes)?;
        insert_observation(&mut self.observations, name, observation)
    }

    fn finish_stats(&mut self) {
        self.stats.directory_entries = self.walker_stats.dir_entries;
    }
}

impl UnreachableObjectInventory {
    pub fn bind_current() -> Result<Self> {
        Self::bind(std::env::current_dir().map_err(|error| ioerr(error, "."))?)
    }

    pub fn bind(root: impl Into<PathBuf>) -> Result<Self> {
        let requested = root.into();
        if !requested.is_absolute() {
            return Err(KioError::invalid_usage(
                "GC inventory scope root must be absolute",
            ));
        }
        let scope = open_bound_absolute(&requested)?;
        let root = requested
            .canonicalize()
            .map_err(|error| ioerr(error, "scope"))?;
        if id_file(&scope)? != id_path(&root)? {
            return Err(corrupt("scope root changed while binding inventory"));
        }
        let kio = open_optional_dir(&scope, ".kio")?
            .ok_or_else(|| KioError::invalid_usage("current directory is not a Kio scope"))?;
        if id_file(&kio)? != id_child(&scope, ".kio")? {
            return Err(corrupt(".kio changed while binding inventory"));
        }
        Ok(Self {
            root,
            scope,
            kio,
            limits: UnreachableInventoryLimits::default(),
        })
    }

    #[must_use]
    pub fn with_limits(mut self, limits: UnreachableInventoryLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn inventory(&self) -> Result<Value> {
        self.assert_stable()?;
        let guard = acquire_bound_store_read_guard(&self.kio)?;
        guard.recheck_idle()?;
        let invocation_time = now_utc_seconds();
        let first = self.pass(&invocation_time)?;
        guard.recheck_idle()?;
        wait_at_first_pass_test_barrier();
        let second = self.pass(&invocation_time)?;
        guard.recheck_idle()?;
        self.assert_stable()?;
        if first != second {
            return Err(corrupt(
                "scope truth or filesystem identity changed between inventory passes",
            ));
        }
        let stability_stats = second.stats;
        Ok(self.report(first, stability_stats))
    }

    fn assert_stable(&self) -> Result<()> {
        if id_file(&self.scope)? != id_path(&self.root)?
            || id_file(&self.kio)? != id_child(&self.scope, ".kio")?
        {
            return Err(corrupt("public scope changed during inventory"));
        }
        Ok(())
    }

    fn report(&self, pass: Pass, stability_stats: InventoryStats) -> Value {
        let mut candidate_count = 0_u64;
        let mut candidate_bytes = 0_u64;
        let mut protected_count = 0_u64;
        let mut protected_bytes = 0_u64;
        let mut inventory_only_count = 0_u64;
        let mut inventory_only_bytes = 0_u64;
        for object in &pass.objects {
            match object.classification.as_str() {
                "candidate" => {
                    candidate_count += 1;
                    candidate_bytes += object.physical_bytes;
                }
                "protected" => {
                    protected_count += 1;
                    protected_bytes += object.physical_bytes;
                }
                "inventory_only" => {
                    inventory_only_count += 1;
                    inventory_only_bytes += object.physical_bytes;
                }
                _ => unreachable!("inventory classification is closed"),
            }
        }
        json!({
            "schema_version": 1,
            "operation": "unreachable_object_inventory",
            "status": "dry_run",
            "read_only": true,
            "diagnostic_only": true,
            "mutation_authority": false,
            "objects": pass.objects,
            "summary": {
                "object_count": pass.stats.objects,
                "physical_bytes": pass.stats.physical_bytes,
                "candidate_count": candidate_count,
                "candidate_bytes": candidate_bytes,
                "protected_count": protected_count,
                "protected_bytes": protected_bytes,
                "inventory_only_count": inventory_only_count,
                "inventory_only_bytes": inventory_only_bytes,
            },
            "shallow_boundaries": pass.shallow_boundaries,
            "limits": self.limits,
            "stats": {
                "inventory_pass": pass.stats,
                "stability_pass": stability_stats,
            },
        })
    }

    fn pass(&self, invocation_time: &str) -> Result<Pass> {
        let mut state = ScanState::new(&self.limits);
        state.observe_directory(&self.scope, "scope")?;
        state.observe_directory(&self.kio, ".kio")?;
        self.observe_scope_identity(&mut state)?;
        self.observe_gc_and_purge_barriers(&mut state, "start")?;
        self.read_refs(&mut state)?;
        self.read_receipts(&mut state)?;
        self.scan_objects(&mut state)?;
        self.scan_purge_markers(&mut state)?;
        let shallow_boundaries = self.validate_graph(&mut state, invocation_time)?;
        self.observe_gc_and_purge_barriers(&mut state, "end")?;
        state.finish_stats();
        let objects = classify_objects(&state)?;
        Ok(Pass {
            objects,
            shallow_boundaries,
            stats: state.stats,
            observations: state.observations,
        })
    }

    fn observe_scope_identity(&self, state: &mut ScanState<'_>) -> Result<()> {
        let (bytes, observation) = read_regular_observed(&self.kio, "scope.json", MAX_METADATA)?;
        validate_scope_bytes(&bytes)?;
        state.observe_file("scope.json", observation, bytes.len() as u64)
    }

    fn observe_gc_and_purge_barriers(&self, state: &mut ScanState<'_>, phase: &str) -> Result<()> {
        if let Some(marker) = read_active_marker_bound(&self.kio)? {
            return Err(active_sweep_error(&marker));
        }
        let current_gc_barrier = if let Some(gc) = open_optional_dir(&self.kio, "gc")? {
            let label = format!("barrier/{phase}/gc");
            state.observe_directory(&gc, &label)?;
            Some(
                state
                    .observations
                    .get(&label)
                    .cloned()
                    .expect("observed GC directory is recorded"),
            )
        } else {
            None
        };
        match phase {
            "start" => {
                if state.gc_barrier.replace(current_gc_barrier).is_some() {
                    return Err(corrupt("GC barrier was observed more than once"));
                }
            }
            "end" if state.gc_barrier.as_ref() != Some(&current_gc_barrier) => {
                return Err(corrupt("GC barrier changed within inventory pass"));
            }
            "end" => {}
            _ => unreachable!("inventory barrier phase is closed"),
        }

        let current_purge_barrier = if let Some(purge) = open_optional_dir(&self.kio, "purge")? {
            let label = format!("barrier/{phase}/purge");
            state.observe_directory(&purge, &label)?;
            let directory_observation = state
                .observations
                .get(&label)
                .cloned()
                .expect("observed purge directory is recorded");
            match read_regular_observed(&purge, "in-progress.json", MAX_MARKER_BYTES) {
                Ok(_) => return Err(active_purge_error()),
                Err(error) if is_io_not_found(&error) => {}
                Err(error) => return Err(error),
            }
            let (epoch, observation) = match read_regular_observed(&purge, "epoch", MAX_REF) {
                Ok(value) => value,
                Err(error) if is_io_not_found(&error) => return Err(active_purge_error()),
                Err(error) => return Err(error),
            };
            let parsed_epoch = parse_counter(&epoch).ok_or_else(active_purge_error)?;
            state.observe_file(
                &format!("barrier/{phase}/purge/epoch"),
                observation,
                epoch.len() as u64,
            )?;
            Some((directory_observation, parsed_epoch))
        } else {
            None
        };
        match phase {
            "start" => {
                if state
                    .purge_barrier
                    .replace(current_purge_barrier.clone())
                    .is_some()
                {
                    return Err(corrupt("purge barrier was observed more than once"));
                }
            }
            "end" if state.purge_barrier.as_ref() != Some(&current_purge_barrier) => {
                return Err(corrupt("purge epoch changed within inventory pass"));
            }
            "end" => {}
            _ => unreachable!("inventory barrier phase is closed"),
        }
        state.purge_epoch = current_purge_barrier.map(|(_, epoch)| epoch);

        let mut current_lifecycle_epoch = None;
        let current_lifecycle_barrier =
            if let Some(tombstones) = open_optional_dir(&self.kio, "tombstones")? {
                let label = format!("barrier/{phase}/tombstones");
                state.observe_directory(&tombstones, &label)?;
                let directory_observation = state
                    .observations
                    .get(&label)
                    .cloned()
                    .expect("observed tombstone directory is recorded");
                match read_regular_observed(&tombstones, "lifecycle-epoch", MAX_REF) {
                    Ok((bytes, observation)) => {
                        let lifecycle_epoch = parse_counter(&bytes).ok_or_else(|| {
                            corrupt("tombstone lifecycle epoch is malformed during inventory")
                        })?;
                        current_lifecycle_epoch = Some(lifecycle_epoch);
                        state.observe_file(
                            &format!("barrier/{phase}/tombstones/lifecycle-epoch"),
                            observation,
                            bytes.len() as u64,
                        )?;
                    }
                    Err(error) if is_io_not_found(&error) => {}
                    Err(error) => return Err(error),
                }
                Some((directory_observation, current_lifecycle_epoch))
            } else {
                None
            };
        match phase {
            "start" => {
                if state
                    .lifecycle_barrier
                    .replace(current_lifecycle_barrier.clone())
                    .is_some()
                {
                    return Err(corrupt("lifecycle barrier was observed more than once"));
                }
            }
            "end" if state.lifecycle_barrier.as_ref() != Some(&current_lifecycle_barrier) => {
                return Err(corrupt(
                    "tombstone lifecycle barrier changed within inventory pass",
                ));
            }
            "end" => {}
            _ => unreachable!("inventory barrier phase is closed"),
        }
        state.lifecycle_epoch = current_lifecycle_barrier
            .and_then(|(_, epoch)| epoch)
            .unwrap_or(0);
        Ok(())
    }

    fn read_refs(&self, state: &mut ScanState<'_>) -> Result<()> {
        let (head_bytes, head_observation) = read_regular_observed(&self.kio, "HEAD", MAX_REF)?;
        state.observe_file("refs/HEAD", head_observation, head_bytes.len() as u64)?;
        let head = parse_ref(&head_bytes, true)?;
        if let Some(value) = head.as_ref() {
            add_inventory_ref(state, "HEAD", value)?;
        }

        let refs = open_required_dir(&self.kio, "refs", "refs directory is missing")?;
        state.observe_directory(&refs, "refs")?;

        let tags = open_required_dir(&refs, "tags-v1", "tag refs directory is missing")?;
        state.observe_directory(&tags, "refs/tags-v1")?;
        for name in names(&tags, &mut state.walker_stats, &state.walker_limits, 3)? {
            let (bytes, observation) = read_regular_observed(&tags, &name, MAX_METADATA)?;
            state.observe_file(
                &format!("refs/tags-v1/{name}"),
                observation,
                bytes.len() as u64,
            )?;
            if name == "names.jsonl" {
                continue;
            }
            if name.len() != 68 || !name.starts_with("tag-") || !hex(&name[4..]) {
                return Err(corrupt("invalid tag ref leaf"));
            }
            let value =
                parse_ref(&bytes, false)?.ok_or_else(|| corrupt("tag ref must not be empty"))?;
            add_inventory_ref(state, &format!("tags-v1/{name}"), &value)?;
        }
        Ok(())
    }

    fn read_receipts(&self, state: &mut ScanState<'_>) -> Result<()> {
        let Some(gc) = open_optional_dir(&self.kio, "gc")? else {
            return Ok(());
        };
        let Some(shallowed) = open_optional_dir(&gc, "shallowed")? else {
            return Ok(());
        };
        state.observe_directory(&shallowed, "gc/shallowed")?;
        for leaf in names(&shallowed, &mut state.walker_stats, &state.walker_limits, 3)? {
            if leaf.len() != 64 || !hex(&leaf) {
                return Err(corrupt("invalid shallow receipt leaf"));
            }
            let (bytes, observation) = read_regular_observed(&shallowed, &leaf, MAX_METADATA)?;
            state.observe_file(
                &format!("gc/shallowed/{leaf}"),
                observation,
                bytes.len() as u64,
            )?;
            let receipt = ShallowReceipt::parse_canonical(&bytes, &leaf)?;
            if state
                .receipts
                .insert(receipt.commit_hash.clone(), receipt)
                .is_some()
            {
                return Err(corrupt("duplicate shallow receipt"));
            }
            state.stats.receipts = state
                .stats
                .receipts
                .checked_add(1)
                .ok_or_else(|| limit("receipts"))?;
            if state.stats.receipts > state.limits.max_receipts {
                return Err(limit("receipts"));
            }
        }
        Ok(())
    }

    /// Marker records are durable truth, but unlike [`PurgeState`] this reader
    /// never opens paths by ambient pathname or repairs a counter.  Every leaf
    /// participates in the two-pass identity comparison just like CAS leaves.
    fn scan_purge_markers(&self, state: &mut ScanState<'_>) -> Result<()> {
        if let Some(tombstones) = open_optional_dir(&self.kio, "tombstones")? {
            state.observe_directory(&tombstones, "tombstones")?;
            self.scan_marker_fanout(state, &tombstones, "tombstones", true)?;
        }
        let Some(purge) = open_optional_dir(&self.kio, "purge")? else {
            return Ok(());
        };
        let Some(receipts) = open_optional_dir(&purge, "erase-receipts")? else {
            return Ok(());
        };
        state.observe_directory(&receipts, "purge/erase-receipts")?;
        self.scan_marker_fanout(state, &receipts, "purge/erase-receipts", false)
    }

    fn scan_marker_fanout(
        &self,
        state: &mut ScanState<'_>,
        base: &std::fs::File,
        label: &str,
        is_tombstone: bool,
    ) -> Result<()> {
        for first in names(base, &mut state.walker_stats, &state.walker_limits, 3)? {
            // The lifecycle counter is the sole non-fanout tombstone sibling.
            if is_tombstone && first == "lifecycle-epoch" {
                continue;
            }
            if first.len() != 2 || !hex(&first) {
                return Err(corrupt("malformed purge marker fanout"));
            }
            let first_dir = open_required_dir(base, &first, "purge marker fanout is missing")?;
            state.observe_directory(&first_dir, &format!("{label}/{first}"))?;
            for second in names(&first_dir, &mut state.walker_stats, &state.walker_limits, 4)? {
                if second.len() != 2 || !hex(&second) {
                    return Err(corrupt("malformed purge marker fanout"));
                }
                let second_dir =
                    open_required_dir(&first_dir, &second, "purge marker fanout is missing")?;
                state.observe_directory(&second_dir, &format!("{label}/{first}/{second}"))?;
                for leaf in names(
                    &second_dir,
                    &mut state.walker_stats,
                    &state.walker_limits,
                    5,
                )? {
                    if leaf.len() != 64 || !hex(&leaf) || leaf[..2] != first || leaf[2..4] != second
                    {
                        return Err(corrupt("malformed purge marker leaf"));
                    }
                    let (bytes, observation) = read_regular_observed(
                        &second_dir,
                        &leaf,
                        crate::purge::MAX_PURGE_RECORD_BYTES,
                    )?;
                    state.observe_file(
                        &format!("{label}/{first}/{second}/{leaf}"),
                        observation,
                        bytes.len() as u64,
                    )?;
                    let raw_hash = format!("sha256:{leaf}");
                    if is_tombstone {
                        let record = parse_tombstone_bytes(&bytes, &raw_hash)?;
                        if state.tombstones.insert(raw_hash, record).is_some() {
                            return Err(corrupt("duplicate tombstone marker"));
                        }
                    } else {
                        let receipt = parse_erase_receipt_bytes(&bytes, &raw_hash)?;
                        if state.erase_receipts.insert(raw_hash, receipt).is_some() {
                            return Err(corrupt("duplicate erase receipt marker"));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn scan_objects(&self, state: &mut ScanState<'_>) -> Result<()> {
        let objects = open_required_dir(&self.kio, "objects", "objects directory is missing")?;
        state.observe_directory(&objects, "objects")?;
        let present = names(&objects, &mut state.walker_stats, &state.walker_limits, 2)?;
        let known = CAS_KINDS
            .iter()
            .map(|(_, directory, _, _)| *directory)
            .chain(NON_CAS_OBJECT_DIRECTORIES)
            .collect::<BTreeSet<_>>();
        for name in &present {
            if !known.contains(name.as_str()) {
                return Err(corrupt("unknown entry in objects directory"));
            }
        }
        for name in NON_CAS_OBJECT_DIRECTORIES {
            if let Some(directory) = open_optional_dir(&objects, name)? {
                state.observe_directory(&directory, &format!("objects/{name}"))?;
            }
        }

        for (kind, directory, max_bytes, materialize) in CAS_KINDS {
            self.scan_kind(&objects, state, kind, directory, max_bytes, materialize)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn scan_kind(
        &self,
        objects: &std::fs::File,
        state: &mut ScanState<'_>,
        kind: &str,
        directory_name: &str,
        max_bytes: u64,
        materialize: bool,
    ) -> Result<()> {
        let Some(base) = open_optional_dir(objects, directory_name)? else {
            return Ok(());
        };
        state.observe_directory(&base, &format!("objects/{directory_name}"))?;
        for first in names(&base, &mut state.walker_stats, &state.walker_limits, 3)? {
            if first.len() != 2 || !hex(&first) {
                return Err(corrupt("malformed CAS fanout"));
            }
            let first_dir = open_required_dir(&base, &first, "CAS fanout is missing")?;
            state.observe_directory(&first_dir, &format!("objects/{directory_name}/{first}"))?;
            for second in names(&first_dir, &mut state.walker_stats, &state.walker_limits, 4)? {
                if second.len() != 2 || !hex(&second) {
                    return Err(corrupt("malformed CAS fanout"));
                }
                let second_dir = open_required_dir(&first_dir, &second, "CAS fanout is missing")?;
                state.observe_directory(
                    &second_dir,
                    &format!("objects/{directory_name}/{first}/{second}"),
                )?;
                for leaf in names(
                    &second_dir,
                    &mut state.walker_stats,
                    &state.walker_limits,
                    5,
                )? {
                    if leaf.len() != 64 || !hex(&leaf) || leaf[..2] != first || leaf[2..4] != second
                    {
                        return Err(corrupt("malformed CAS object leaf"));
                    }
                    let observation_name =
                        format!("objects/{directory_name}/{first}/{second}/{leaf}");
                    let canonical_hash = format!("sha256:{leaf}");
                    if materialize {
                        let (bytes, observation) =
                            read_regular_observed(&second_dir, &leaf, max_bytes)?;
                        let size = bytes.len() as u64;
                        state.observe_file(&observation_name, observation, size)?;
                        state.add_object(kind, canonical_hash.clone(), size)?;
                        self.validate_semantic_object(state, kind, &canonical_hash, &bytes)?;
                    } else {
                        let (size, digest, observation) =
                            inspect_regular_observed(&second_dir, &leaf, max_bytes)?;
                        state.observe_file(&observation_name, observation, size)?;
                        state.add_object(kind, canonical_hash.clone(), size)?;
                        if digest != canonical_hash {
                            return Err(corrupt("content-addressed object hash mismatch"));
                        }
                        if kind == "image" {
                            state.images.insert(canonical_hash);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_semantic_object(
        &self,
        state: &mut ScanState<'_>,
        kind: &str,
        hash: &str,
        bytes: &[u8],
    ) -> Result<()> {
        match kind {
            "commit" => {
                require_content_hash(bytes, hash, "commit")?;
                let commit: CommitObject = strict_canonical_json(bytes, "commit")?;
                commit
                    .validate()
                    .map_err(|_| corrupt("invalid commit object"))?;
                state.commit_toollocks.insert(commit.tool_lock_hash.clone());
                if state.commits.insert(hash.to_owned(), commit).is_some() {
                    return Err(corrupt("duplicate commit object"));
                }
            }
            "tree" => {
                require_content_hash(bytes, hash, "tree")?;
                let tree: TreeObject = strict_canonical_json(bytes, "tree")?;
                if tree.entries.len() > MAX_TREE_ENTRIES {
                    return Err(limit("tree entries"));
                }
                tree.validate()
                    .map_err(|_| corrupt("invalid tree object"))?;
                for entry in &tree.entries {
                    if let Some(normalize) = &entry.normalize {
                        state
                            .tree_manifests
                            .entry(normalize.manifest_hash.clone())
                            .or_default()
                            .insert(TreeManifestEdge {
                                raw_hash: entry.raw_hash.clone(),
                                tree_hash: hash.to_owned(),
                                tool_profile_hash: normalize.tool_profile_hash.clone(),
                                generation: normalize.r#gen,
                            });
                    }
                }
                if state.trees.insert(hash.to_owned(), tree).is_some() {
                    return Err(corrupt("duplicate tree object"));
                }
            }
            "chunk" => {
                let chunk: ChunkObject = strict_canonical_json(bytes, "chunk")?;
                if chunk.identity_hash()? != hash {
                    return Err(corrupt("chunk identity hash mismatch"));
                }
                state.chunk_text_hashes.insert(chunk.text_hash);
            }
            "manifest" => {
                require_content_hash(bytes, hash, "manifest")?;
                let manifest: InventoryManifest = strict_canonical_json(bytes, "manifest")?;
                let identity = validate_manifest(state, &manifest)?;
                if state.manifests.insert(hash.to_owned(), identity).is_some() {
                    return Err(corrupt("duplicate normalized manifest object"));
                }
            }
            "normalized_unit" => {
                require_content_hash(bytes, hash, "normalized unit")?;
                let unit: InventoryNormalizedUnit =
                    strict_canonical_json(bytes, "normalized unit")?;
                let identity = validate_normalized_unit(&unit)?;
                if state
                    .normalized_units
                    .insert(hash.to_owned(), identity)
                    .is_some()
                {
                    return Err(corrupt("duplicate normalized unit object"));
                }
            }
            "embedding" => {
                let embedding = EmbeddingObject::from_bytes(bytes)
                    .map_err(|_| corrupt("invalid embedding object"))?;
                if embedding.identity_hash()? != hash || embedding.to_bytes()? != bytes {
                    return Err(corrupt("embedding identity or canonical bytes mismatch"));
                }
                state.embedding_targets.insert(
                    hash.to_owned(),
                    (embedding.target_type, embedding.target_hash),
                );
            }
            "toollock" => {
                require_content_hash(bytes, hash, "tool-lock")?;
                let value: Value = strict_canonical_json(bytes, "tool-lock")?;
                let canonical = canonical_tool_lock_value(&value)
                    .map_err(|_| corrupt("invalid tool-lock object"))?;
                if canonical_json_bytes(&canonical)? != bytes {
                    return Err(corrupt("tool-lock object is not canonical"));
                }
            }
            _ => unreachable!("materialized CAS kind is closed"),
        }
        Ok(())
    }

    fn validate_graph(
        &self,
        state: &mut ScanState<'_>,
        invocation_time: &str,
    ) -> Result<Vec<ShallowBoundary>> {
        for commit in state.commits.values() {
            if let Some(parent) = &commit.parent
                && !state.commits.contains_key(parent)
            {
                return Err(corrupt("commit references a missing parent"));
            }
        }
        for tip in state.refs.values() {
            if !state.commits.contains_key(tip) {
                return Err(corrupt("ref points to a missing commit"));
            }
        }

        let mut pending = state.refs.values().cloned().collect::<Vec<_>>();
        while let Some(hash) = pending.pop() {
            state.stats.history_steps = state
                .stats
                .history_steps
                .checked_add(1)
                .ok_or_else(|| limit("history traversal"))?;
            if state.stats.history_steps > state.limits.max_history_steps {
                return Err(limit("history traversal"));
            }
            if !state.reachable_commits.insert(hash.clone()) {
                continue;
            }
            let commit = state
                .commits
                .get(&hash)
                .ok_or_else(|| corrupt("history traversal reached a missing commit"))?;
            pending.extend(commit.parent.iter().cloned());
        }

        let mut boundaries = Vec::new();
        for (commit_hash, commit) in &state.commits {
            match state.receipts.get(commit_hash) {
                Some(receipt) => {
                    if receipt.tree_hash != commit.tree {
                        return Err(corrupt("shallow receipt tree differs from commit"));
                    }
                    if state.refs.values().any(|tip| tip == commit_hash) {
                        return Err(corrupt("shallow receipt names a current ref tip"));
                    }
                    if state.trees.contains_key(&commit.tree) {
                        return Err(corrupt("shallow receipt coexists with its tree"));
                    }
                    boundaries.push(ShallowBoundary {
                        commit_hash: commit_hash.clone(),
                        tree_hash: commit.tree.clone(),
                    });
                }
                None if !state.trees.contains_key(&commit.tree) => {
                    return Err(corrupt("commit references a missing tree"));
                }
                None => {}
            }
        }
        for receipt_hash in state.receipts.keys() {
            if !state.commits.contains_key(receipt_hash) {
                return Err(corrupt("shallow receipt references a missing commit"));
            }
        }
        // Do not infer that an object was never referenced merely because no
        // currently-readable tree points to it: every receipt hides a whole
        // historical tree closure.  This is deliberately scope-wide rather
        // than attempting to guess which semantic objects that closure held.
        state.shallow_closure_uncertainty = !state.receipts.is_empty();
        for hash in &state.commit_toollocks {
            if !state
                .physical
                .contains_key(&("toollock".to_owned(), hash.clone()))
            {
                return Err(corrupt("commit references a missing tool-lock"));
            }
        }
        self.validate_purge_markers(state, invocation_time)?;
        self.validate_tree_closure(state)?;
        for (unit_hash, pins) in &state.manifest_pins {
            let unit = state
                .normalized_units
                .get(unit_hash)
                .ok_or_else(|| corrupt("manifest references a missing normalized unit"))?;
            for pin in pins {
                if unit.unit_key != pin.unit_key
                    || unit.unit_type != pin.unit_type
                    || unit.raw_hash != pin.raw_hash
                    || unit.prepared_hash != pin.prepared_hash
                    || unit.preparation_profile_hash != pin.preparation_profile_hash
                    || unit.tool_profile_hash != pin.tool_profile_hash
                    || unit.generation != pin.generation
                {
                    return Err(corrupt(
                        "normalized unit identity differs from its manifest pin",
                    ));
                }
            }
        }
        boundaries.sort_by(|left, right| left.commit_hash.cmp(&right.commit_hash));
        Ok(boundaries)
    }

    fn validate_purge_markers(
        &self,
        state: &mut ScanState<'_>,
        invocation_time: &str,
    ) -> Result<()> {
        let raws = state
            .tombstones
            .keys()
            .chain(state.erase_receipts.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut max_lifecycle_epoch = 0_u64;
        let mut max_purge_epoch = 0_u64;
        for raw_hash in raws {
            let tombstone = state.tombstones.get(&raw_hash);
            let receipt = state.erase_receipts.get(&raw_hash);
            for events in [
                tombstone.map(|record| record.events.as_slice()),
                receipt.map(|record| record.events.as_slice()),
            ] {
                let Some(events) = events else { continue };
                for (index, event) in events.iter().enumerate() {
                    max_lifecycle_epoch =
                        max_lifecycle_epoch.max(event.lifecycle_epoch.unwrap_or(0));
                    max_purge_epoch = max_purge_epoch.max(event.epoch.unwrap_or(0));
                    match event.kind {
                        EventKind::Purged | EventKind::Erased => {
                            if !state.reachable_commits.contains(&event.in_commit) {
                                return Err(corrupt(
                                    "lifecycle event in_commit is not ref-reachable",
                                ));
                            }
                            let commit = state.commits.get(&event.in_commit).ok_or_else(|| {
                                corrupt("lifecycle event in_commit is missing from inventory")
                            })?;
                            verify_marker_binding(&raw_hash, event, commit, invocation_time)?;
                        }
                        EventKind::Retired => {
                            if !state.reachable_commits.contains(&event.in_commit) {
                                return Err(corrupt(
                                    "retired lifecycle event is not ref-reachable",
                                ));
                            }
                            if timestamp_is_after(&event.at, invocation_time)? {
                                return Err(corrupt("retired lifecycle event is in the future"));
                            }
                            let previous = events
                                .get(index.checked_sub(1).ok_or_else(|| {
                                    corrupt("retired lifecycle event has no opening event")
                                })?)
                                .ok_or_else(|| corrupt("retired lifecycle event is malformed"))?;
                            if !strict_descendant(
                                &state.commits,
                                &event.in_commit,
                                &previous.in_commit,
                                &mut state.stats.history_steps,
                                state.limits.max_history_steps,
                            )? {
                                return Err(corrupt(
                                    "retired lifecycle event is not a strict descendant",
                                ));
                            }
                            let resurrection =
                                state.commits.get(&event.in_commit).ok_or_else(|| {
                                    corrupt(
                                        "retired lifecycle event commit is missing from inventory",
                                    )
                                })?;
                            if let Some(tree) = state.trees.get(&resurrection.tree)
                                && !tree.entries.iter().any(|entry| entry.raw_hash == raw_hash)
                            {
                                return Err(corrupt(
                                    "resurrection tree does not contain retired raw",
                                ));
                            }
                        }
                    }
                }
            }
            if let Some(final_event) = canonical_final_event(
                tombstone.map(TombstoneRecord::tail),
                receipt.map(EraseReceipt::tail),
            )? {
                state.final_events.insert(raw_hash, final_event);
            }
        }
        if max_lifecycle_epoch > state.lifecycle_epoch {
            return Err(corrupt("lifecycle epoch counter is behind marker events"));
        }
        if max_purge_epoch > 0 {
            let purge_epoch = state.purge_epoch.ok_or_else(active_purge_error)?;
            if max_purge_epoch > purge_epoch {
                return Err(corrupt("purge epoch counter is behind marker events"));
            }
        }
        Ok(())
    }

    fn validate_tree_closure(&self, state: &mut ScanState<'_>) -> Result<()> {
        let mut history_steps = state.stats.history_steps;
        for (tree_hash, tree) in &state.trees {
            for entry in &tree.entries {
                let raw_present = state
                    .physical
                    .contains_key(&("raw".to_owned(), entry.raw_hash.clone()));
                if !raw_present
                    && !historical_tree_gap_is_explained(
                        &state.final_events,
                        &state.commits,
                        &entry.raw_hash,
                        tree_hash,
                        false,
                        &mut history_steps,
                        state.limits.max_history_steps,
                    )?
                {
                    return Err(corrupt(
                        "tree references a missing raw outside verified pre-purge history",
                    ));
                }
            }
        }
        let mut uncertain_manifest_identities = BTreeSet::new();
        for (manifest_hash, edges) in &state.tree_manifests {
            if let Some(manifest) = state.manifests.get(manifest_hash) {
                for edge in edges {
                    if manifest.raw_hash != edge.raw_hash
                        || manifest.tool_profile_hash != edge.tool_profile_hash
                        || manifest.generation != edge.generation
                    {
                        return Err(corrupt("tree normalize identity differs from its manifest"));
                    }
                }
                continue;
            }
            for edge in edges {
                if !historical_tree_gap_is_explained(
                    &state.final_events,
                    &state.commits,
                    &edge.raw_hash,
                    &edge.tree_hash,
                    true,
                    &mut history_steps,
                    state.limits.max_history_steps,
                )? {
                    return Err(corrupt(
                        "tree references a missing manifest without a valid lifecycle explanation",
                    ));
                }
                uncertain_manifest_identities.insert((
                    edge.raw_hash.clone(),
                    edge.tool_profile_hash.clone(),
                    edge.generation,
                ));
            }
        }
        for (hash, unit) in &state.normalized_units {
            if uncertain_manifest_identities.contains(&(
                unit.raw_hash.clone(),
                unit.tool_profile_hash.clone(),
                unit.generation,
            )) {
                state.historical_unit_uncertainty.insert(hash.clone());
            }
        }
        state.stats.history_steps = history_steps;
        Ok(())
    }
}

fn classify_objects(state: &ScanState<'_>) -> Result<Vec<InventoryObject>> {
    let mut output = Vec::with_capacity(state.physical.len());
    for ((kind, hash), bytes) in &state.physical {
        let (classification, reason) = match kind.as_str() {
            "commit" if state.receipts.contains_key(hash) => {
                ("protected", "append_only_shallow_history")
            }
            "commit" if state.reachable_commits.contains(hash) => {
                ("protected", "append_only_history")
            }
            "commit" => ("protected", "append_only_unreachable_history"),
            "tree" => ("protected", "retention_gc_owned"),
            "raw" | "chunk" => ("protected", "evidence_pointer_permanence"),
            "prepared" | "image" => ("inventory_only", "verify_objects_orphan_lifecycle"),
            "manifest" if state.tree_manifests.contains_key(hash) => {
                ("protected", "tree_referenced")
            }
            "manifest" if state.shallow_closure_uncertainty => {
                ("inventory_only", "shallow_history_unavailable")
            }
            "manifest" => ("candidate", "zero_tree_references"),
            "normalized_unit" if state.manifest_pins.contains_key(hash) => {
                ("protected", "manifest_referenced")
            }
            "normalized_unit" if state.historical_unit_uncertainty.contains(hash) => {
                ("inventory_only", "historical_manifest_unavailable")
            }
            "normalized_unit" if state.shallow_closure_uncertainty => {
                ("inventory_only", "shallow_history_unavailable")
            }
            "normalized_unit" => ("candidate", "zero_manifest_references"),
            "toollock" if state.commit_toollocks.contains(hash) => {
                ("protected", "commit_referenced")
            }
            "toollock" => ("candidate", "zero_commit_references"),
            "embedding" => {
                let (target_type, target_hash) = state
                    .embedding_targets
                    .get(hash)
                    .ok_or_else(|| corrupt("embedding target classification is missing"))?;
                match target_type.as_str() {
                    "chunk" if state.chunk_text_hashes.contains(target_hash) => {
                        ("protected", "target_referenced")
                    }
                    "image" if state.images.contains(target_hash) => {
                        ("protected", "target_referenced")
                    }
                    "chunk" | "image" if crate::cas::is_hash(target_hash) => {
                        ("candidate", "zero_target_references")
                    }
                    _ => ("inventory_only", "unprovable_target"),
                }
            }
            _ => return Err(corrupt("unknown object kind during classification")),
        };
        output.push(InventoryObject {
            kind: kind.clone(),
            hash: hash.clone(),
            physical_bytes: *bytes,
            classification: classification.to_owned(),
            reason: reason.to_owned(),
        });
    }
    Ok(output)
}

fn historical_tree_gap_is_explained(
    final_events: &BTreeMap<String, CanonicalFinalEvent>,
    commits: &BTreeMap<String, CommitObject>,
    raw_hash: &str,
    tree_hash: &str,
    allow_retired: bool,
    history_steps: &mut u64,
    max_history_steps: u64,
) -> Result<bool> {
    let Some(final_event) = final_events.get(raw_hash) else {
        return Ok(false);
    };
    let anchor = match final_event.event.kind {
        EventKind::Purged | EventKind::Erased => final_event.event.in_commit.as_str(),
        EventKind::Retired if allow_retired => final_event
            .event
            .resurrection_commit
            .as_deref()
            .ok_or_else(|| corrupt("retired event is missing its resurrection commit"))?,
        EventKind::Retired => return Ok(false),
    };
    let mut referenced = false;
    for (commit_hash, commit) in commits {
        if commit.tree != tree_hash {
            continue;
        }
        referenced = true;
        if commit_hash == anchor
            || !strict_descendant(
                commits,
                anchor,
                commit_hash,
                history_steps,
                max_history_steps,
            )?
        {
            return Ok(false);
        }
    }
    Ok(referenced)
}

fn strict_descendant(
    commits: &BTreeMap<String, CommitObject>,
    descendant: &str,
    ancestor: &str,
    steps: &mut u64,
    max_steps: u64,
) -> Result<bool> {
    let Some(start) = commits.get(descendant) else {
        return Err(corrupt("descendant commit is missing"));
    };
    let mut pending: Vec<_> = start.parent.iter().cloned().collect();
    let mut seen = BTreeSet::new();
    while let Some(hash) = pending.pop() {
        *steps = steps
            .checked_add(1)
            .ok_or_else(|| limit("retired ancestry traversal"))?;
        if *steps > max_steps {
            return Err(limit("retired ancestry traversal"));
        }
        if hash == ancestor {
            return Ok(true);
        }
        if !seen.insert(hash.clone()) {
            continue;
        }
        let commit = commits
            .get(&hash)
            .ok_or_else(|| corrupt("retired ancestry references a missing commit"))?;
        pending.extend(commit.parent.iter().cloned());
    }
    Ok(false)
}

fn validate_manifest(
    state: &mut ScanState<'_>,
    manifest: &InventoryManifest,
) -> Result<ManifestIdentity> {
    if !crate::cas::is_hash(&manifest.raw_hash)
        || !crate::cas::is_hash(&manifest.tool_profile_hash)
        || manifest.run_id.is_empty()
        || !is_canonical_utc_timestamp(&manifest.generated_at)
    {
        return Err(corrupt("invalid normalized manifest identity"));
    }
    let mut orders = BTreeSet::new();
    let mut keys = BTreeSet::new();
    let mut refs = BTreeSet::new();
    for entry in &manifest.units {
        state.stats.manifest_units = state
            .stats
            .manifest_units
            .checked_add(1)
            .ok_or_else(|| limit("manifest units"))?;
        if state.stats.manifest_units > state.limits.max_manifest_units {
            return Err(limit("manifest units"));
        }
        if !orders.insert(entry.order)
            || !keys.insert(entry.unit_key.as_str())
            || !refs.insert(entry.unit_ref.as_str())
            || entry.unit_key.is_empty()
            || entry.unit_ref != unit_ref(&entry.unit_key)
            || !valid_unit_type(&entry.unit_type)
            || !crate::cas::is_hash(&entry.prepared_hash)
            || !crate::cas::is_hash(&entry.preparation_profile_hash)
        {
            return Err(corrupt("invalid normalized manifest unit entry"));
        }
        match (entry.status.as_str(), entry.unit_object_hash.as_deref()) {
            ("done", Some(hash)) if crate::cas::is_hash(hash) && entry.error_kind.is_none() => {
                state
                    .manifest_pins
                    .entry(hash.to_owned())
                    .or_default()
                    .push(ManifestPin {
                        unit_key: entry.unit_key.clone(),
                        unit_type: entry.unit_type.clone(),
                        raw_hash: manifest.raw_hash.clone(),
                        prepared_hash: entry.prepared_hash.clone(),
                        preparation_profile_hash: entry.preparation_profile_hash.clone(),
                        tool_profile_hash: manifest.tool_profile_hash.clone(),
                        generation: manifest.generation,
                    });
            }
            ("failed", None)
                if entry
                    .error_kind
                    .as_deref()
                    .is_some_and(|kind| !kind.is_empty()) => {}
            _ => return Err(corrupt("invalid normalized manifest unit status")),
        }
    }
    Ok(ManifestIdentity {
        raw_hash: manifest.raw_hash.clone(),
        tool_profile_hash: manifest.tool_profile_hash.clone(),
        generation: manifest.generation,
    })
}

fn validate_normalized_unit(unit: &InventoryNormalizedUnit) -> Result<NormalizedUnitIdentity> {
    if unit.unit_key.is_empty()
        || !valid_unit_type(&unit.unit_type)
        || !crate::cas::is_hash(&unit.raw_hash)
        || !crate::cas::is_hash(&unit.prepared_hash)
        || !crate::cas::is_hash(&unit.preparation_profile_hash)
        || !crate::cas::is_hash(&unit.tool_profile_hash)
        || unit.owned_image_hashes.len() > 256
        || unit
            .owned_image_hashes
            .iter()
            .any(|hash| !crate::cas::is_hash(hash))
        || !matches!(unit.mode.as_str(), "full" | "incremental")
        || unit.markdown.is_empty()
        || !is_canonical_utc_timestamp(&unit.generated_at)
    {
        return Err(corrupt("invalid normalized unit object"));
    }
    let _ = &unit.metadata;
    if let Some(reused) = &unit.reused_from
        && (!crate::cas::is_hash(&reused.raw_hash) || reused.unit_key.is_empty())
    {
        return Err(corrupt("invalid normalized unit reuse identity"));
    }
    if let Some(reused) = &unit.reused_from {
        let _ = reused.generation;
    }
    Ok(NormalizedUnitIdentity {
        unit_key: unit.unit_key.clone(),
        unit_type: unit.unit_type.clone(),
        raw_hash: unit.raw_hash.clone(),
        prepared_hash: unit.prepared_hash.clone(),
        preparation_profile_hash: unit.preparation_profile_hash.clone(),
        tool_profile_hash: unit.tool_profile_hash.clone(),
        generation: unit.generation,
    })
}

fn strict_canonical_json<T>(bytes: &[u8], label: &str) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| corrupt(&format!("malformed {label} JSON")))?;
    if canonical_json_bytes(&value)? != bytes {
        return Err(corrupt(&format!("{label} object is not canonical JSON")));
    }
    serde_json::from_value(value).map_err(|_| corrupt(&format!("invalid {label} object schema")))
}

fn require_content_hash(bytes: &[u8], expected: &str, label: &str) -> Result<()> {
    if hash_bytes(bytes) != expected {
        Err(corrupt(&format!("{label} object hash mismatch")))
    } else {
        Ok(())
    }
}

fn parse_ref(bytes: &[u8], empty_allowed: bool) -> Result<Option<String>> {
    let value = std::str::from_utf8(bytes).map_err(|_| corrupt("ref is not UTF-8"))?;
    let value = value.trim();
    if value == "unborn" && empty_allowed {
        return Ok(None);
    }
    if value.is_empty() {
        return Err(corrupt("ref must not be empty"));
    }
    if !crate::cas::is_hash(value) {
        return Err(corrupt("ref is not a canonical commit hash"));
    }
    Ok(Some(value.to_owned()))
}

fn add_inventory_ref(state: &mut ScanState<'_>, name: &str, value: &str) -> Result<()> {
    state.stats.refs = state
        .stats
        .refs
        .checked_add(1)
        .ok_or_else(|| limit("refs"))?;
    if state.stats.refs > state.limits.max_refs {
        return Err(limit("refs"));
    }
    if state
        .refs
        .insert(name.to_owned(), value.to_owned())
        .is_some()
    {
        return Err(corrupt("duplicate ref name"));
    }
    Ok(())
}

fn inspect_regular_observed(
    directory: &std::fs::File,
    name: &str,
    max_bytes: u64,
) -> Result<(u64, String, FileObservation)> {
    let path = Path::new(name);
    let before = cap_fs::stat(directory, path, cap_fs::FollowSymlinks::No)
        .map_err(|error| ioerr(error, name))?;
    valid_file(&before, max_bytes)?;
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut file = cap_fs::open(directory, path, &options).map_err(|error| ioerr(error, name))?;
    let opened = cap_fs::Metadata::from_file(&file).map_err(|error| ioerr(error, name))?;
    valid_file(&opened, max_bytes)?;
    if !same_file_state(&before, &opened)? {
        return Err(corrupt("store file changed while opening"));
    }
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read_cap = max_bytes
            .saturating_sub(total)
            .saturating_add(1)
            .min(buffer.len() as u64) as usize;
        let count = file
            .read(&mut buffer[..read_cap])
            .map_err(|error| ioerr(error, name))?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .ok_or_else(|| limit("file bytes"))?;
        if total > max_bytes {
            return Err(limit("file bytes"));
        }
        hasher.update(&buffer[..count]);
    }
    let after = cap_fs::stat(directory, path, cap_fs::FollowSymlinks::No)
        .map_err(|error| ioerr(error, name))?;
    valid_file(&after, max_bytes)?;
    if total != opened.len() || !same_file_state(&after, &opened)? {
        return Err(corrupt("store file changed while read"));
    }
    let digest = format!("sha256:{}", lower_hex(&hasher.finalize()));
    Ok((
        total,
        digest.clone(),
        FileObservation {
            identity: id_meta(&opened)?,
            state: file_state(&opened),
            digest,
        },
    ))
}

fn valid_unit_type(value: &str) -> bool {
    matches!(
        value,
        "page" | "slide" | "heading_section" | "sheet" | "image" | "file" | "symbol"
    )
}

fn unit_ref(unit_key: &str) -> String {
    lower_hex(&Sha256::digest(unit_key.as_bytes()))[..16].to_owned()
}

fn parse_counter(bytes: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(bytes).ok()?;
    if text.is_empty() || text.trim() != text || (text.len() > 1 && text.starts_with('0')) {
        return None;
    }
    text.parse().ok()
}

fn active_purge_error() -> KioError {
    KioError::new(
        "KIO-E-PURGE-JOURNAL-ACTIVE-001",
        "active or uncertain purge state blocks unreachable-object inventory",
        json!({"component": "purge_state"}),
        ExitCode::PartialFailure,
    )
}

#[cfg(test)]
thread_local! {
    static TEST_FIRST_PASS_BARRIER: std::cell::RefCell<Option<(
        std::sync::mpsc::SyncSender<()>,
        std::sync::mpsc::Receiver<()>,
    )>> = const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, unix))]
fn set_first_pass_test_barrier(
    barrier: Option<(
        std::sync::mpsc::SyncSender<()>,
        std::sync::mpsc::Receiver<()>,
    )>,
) {
    TEST_FIRST_PASS_BARRIER.with(|slot| *slot.borrow_mut() = barrier);
}

#[cfg(test)]
fn wait_at_first_pass_test_barrier() {
    TEST_FIRST_PASS_BARRIER.with(|slot| {
        if let Some((ready, release)) = slot.borrow().as_ref() {
            ready.send(()).expect("inventory race test receiver exists");
            release
                .recv()
                .expect("inventory race test release sender exists");
        }
    });
}

#[cfg(not(test))]
fn wait_at_first_pass_test_barrier() {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::scope::Repository;
    use std::fs;
    use std::sync::mpsc;
    use std::time::Duration;

    fn inventory_race(root: &Path, mutate: impl FnOnce() + Send + 'static) -> crate::KioError {
        let inventory = UnreachableObjectInventory::bind(root.canonicalize().unwrap()).unwrap();
        let (ready_tx, ready_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let child = std::thread::spawn(move || {
            set_first_pass_test_barrier(Some((ready_tx, release_rx)));
            let result = inventory.inventory();
            set_first_pass_test_barrier(None);
            result
        });
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("inventory reached its first-pass test barrier");
        mutate();
        release_tx.send(()).unwrap();
        child.join().unwrap().unwrap_err()
    }

    #[test]
    fn identical_scope_file_inode_replacement_between_passes_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        Repository::init(root.path()).unwrap();
        let scope_json = root.path().join(".kio/scope.json");
        let replacement = root.path().join(".kio/scope.json.replacement");
        let error = inventory_race(root.path(), move || {
            fs::write(&replacement, fs::read(&scope_json).unwrap()).unwrap();
            fs::rename(&replacement, &scope_json).unwrap();
        });
        assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
    }

    #[test]
    fn objects_directory_swap_between_passes_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        Repository::init(root.path()).unwrap();
        let objects = root.path().join(".kio/objects");
        let displaced = root.path().join(".kio/objects.displaced");
        let error = inventory_race(root.path(), move || {
            fs::rename(&objects, &displaced).unwrap();
            fs::create_dir(&objects).unwrap();
        });
        assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
    }

    #[test]
    fn normalized_unit_inventory_schema_requires_canonical_owned_hashes() {
        let valid = serde_json::json!({
            "unit_key": "page:1",
            "unit_type": "page",
            "raw_hash": format!("sha256:{}", "a".repeat(64)),
            "prepared_hash": format!("sha256:{}", "b".repeat(64)),
            "preparation_profile_hash": format!("sha256:{}", "c".repeat(64)),
            "tool_profile_hash": format!("sha256:{}", "d".repeat(64)),
            "gen": 0,
            "mode": "full",
            "markdown": "body",
            "owned_image_hashes": [],
            "metadata": {},
            "reused_from": null,
            "generated_at": "2026-09-08T00:00:00Z"
        });
        let parsed: InventoryNormalizedUnit = serde_json::from_value(valid.clone()).unwrap();
        validate_normalized_unit(&parsed).unwrap();

        let mut missing = valid.clone();
        missing
            .as_object_mut()
            .unwrap()
            .remove("owned_image_hashes");
        assert!(serde_json::from_value::<InventoryNormalizedUnit>(missing).is_err());

        let mut malformed = valid.clone();
        malformed["owned_image_hashes"] = serde_json::json!(["sha256:not-a-digest"]);
        assert!(serde_json::from_value::<InventoryNormalizedUnit>(malformed).is_err());

        let hash = format!("sha256:{}", "e".repeat(64));
        let mut duplicate = valid;
        duplicate["owned_image_hashes"] = serde_json::json!([hash.clone(), hash]);
        assert!(serde_json::from_value::<InventoryNormalizedUnit>(duplicate).is_err());
    }
}
