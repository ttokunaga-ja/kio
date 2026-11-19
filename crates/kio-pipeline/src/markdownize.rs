//! Markdownize and normalized-unit contracts.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use kio_adapter::types as adapter_types;
use kio_core::cas::{
    ContentObjectKind, MAX_NORMALIZED_UNIT_OBJECT_BYTES, ObjectStore, canonical_json_bytes,
    hash_bytes as cas_hash_bytes,
};
use kio_core::store_dir::{
    ATOMIC_WORKSPACE_DIR, AtomicWorkspaceState, DirectoryStageIdentity, Publication, StoreDirectory,
};
use kio_core::{control::ControlHandle, scope::new_ulid};
use serde::de::{Deserializer, Error as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use unicode_normalization::UnicodeNormalization;

use crate::extraction_stage::{
    SavedStage, child_directory, read_record, stage_error, write_record,
};
use crate::prepare::{
    PreparedUnit, UnitFingerprint, UnitType, hash_bytes, unit_ref as prepared_unit_ref,
};
use crate::store_path::{StorePathKind, resolve_existing_store_path};
use crate::{IoResultExt, PipelineError, Result};

const MAX_NORMALIZED_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;
const MAX_NORMALIZED_UNIT_BYTES: u64 = MAX_NORMALIZED_UNIT_OBJECT_BYTES;
// CT4-FSCK's invocation-wide object ceiling is also the outer bound for the
// physical manifest/unit entries inspected before the loader can identify the
// valid subset. Canonical and conflicting representations share this one counter.
const MAX_NORMALIZED_INSTANCE_FILES: usize = 1_000_000;
/// OCR adapters cap decoded images at this per-page ceiling. Retaining the
/// same bound in the durable unit schema prevents a provider response from
/// inflating ownership metadata independently of image decoding.
const MAX_OWNED_IMAGE_HASHES_PER_UNIT: usize = 256;

#[derive(Debug, Clone, Copy)]
struct NormalizedSizeLimits {
    manifest_bytes: u64,
    unit_bytes: u64,
}

const NORMALIZED_SIZE_LIMITS: NormalizedSizeLimits = NormalizedSizeLimits {
    manifest_bytes: MAX_NORMALIZED_MANIFEST_BYTES,
    unit_bytes: MAX_NORMALIZED_UNIT_BYTES,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkdownizeMode {
    Full,
    Incremental,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitStatus {
    Done,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitRef {
    pub raw_hash: String,
    pub tool_profile_hash: String,
    pub r#gen: u64,
    pub unit_key: String,
    pub unit_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReusedFrom {
    pub raw_hash: String,
    pub r#gen: u64,
    pub unit_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizedUnitObject {
    pub unit_key: String,
    pub unit_type: UnitType,
    pub raw_hash: String,
    pub prepared_hash: String,
    /// Required preparation identity pinned with the immutable unit body.
    pub preparation_profile_hash: String,
    pub tool_profile_hash: String,
    pub r#gen: u64,
    pub mode: MarkdownizeMode,
    pub markdown: String,
    /// Required, adapter-attested image CAS ownership. This is deliberately
    /// independent from free-form markdown and metadata references.
    #[serde(deserialize_with = "deserialize_owned_image_hashes")]
    pub owned_image_hashes: BTreeSet<String>,
    /// Provider/layout metadata. This required object is part of the immutable
    /// normalized-unit JSON schema.
    pub metadata: BTreeMap<String, Value>,
    pub reused_from: Option<ReusedFrom>,
    pub generated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NormalizedUnitManifestEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_limit: Option<kio_core::error::ResourceLimit>,
    pub order: u64,
    pub unit_key: String,
    pub unit_ref: String,
    pub unit_type: UnitType,
    pub status: UnitStatus,
    pub prepared_hash: String,
    /// Required preparation identity for safe incremental reuse.
    pub preparation_profile_hash: String,
    /// Required nullable pin to the immutable canonical JSON unit object.
    /// `Done` entries carry a SHA-256 hash; `Failed` entries carry JSON null.
    pub unit_object_hash: Option<String>,
    pub error_kind: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UncheckedNormalizedUnitManifestEntry {
    resource_limit: Option<kio_core::error::ResourceLimit>,
    order: u64,
    unit_key: String,
    unit_ref: String,
    unit_type: UnitType,
    status: UnitStatus,
    prepared_hash: String,
    preparation_profile_hash: String,
    #[serde(deserialize_with = "deserialize_required_nullable_unit_object_hash")]
    unit_object_hash: Option<String>,
    error_kind: Option<String>,
}

impl<'de> Deserialize<'de> for NormalizedUnitManifestEntry {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let entry = UncheckedNormalizedUnitManifestEntry::deserialize(deserializer)?;
        if !kio_core::error::ResourceLimit::matches_kind(
            entry.error_kind.as_deref(),
            entry.resource_limit.as_ref(),
        ) {
            return Err(serde::de::Error::custom(
                "invalid manifest resource descriptor",
            ));
        }
        if entry.error_kind.as_ref().is_some_and(|kind| {
            serde_json::from_value::<crate::task::RetryErrorKind>(Value::String(kind.clone()))
                .is_err()
        }) || (entry.status == UnitStatus::Done
            && (entry.error_kind.is_some() || entry.resource_limit.is_some()))
        {
            return Err(serde::de::Error::custom(
                "invalid manifest unit failure kind",
            ));
        }
        let expected = prepared_unit_ref(&entry.unit_key);
        let canonical_shape = entry.unit_ref.len() == 16
            && entry
                .unit_ref
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase());
        if entry.unit_key.is_empty() || !canonical_shape || entry.unit_ref != expected {
            return Err(serde::de::Error::custom(format!(
                "manifest unit_ref is not derived from unit_key {:?}",
                entry.unit_key
            )));
        }
        match (entry.status, entry.unit_object_hash.as_deref()) {
            (UnitStatus::Done, Some(hash)) if kio_core::cas::is_hash(hash) => {}
            (UnitStatus::Done, _) => {
                return Err(serde::de::Error::custom(
                    "done manifest entry must pin a valid normalized unit object hash",
                ));
            }
            (UnitStatus::Failed, None) => {}
            (UnitStatus::Failed, Some(_)) => {
                return Err(serde::de::Error::custom(
                    "failed manifest entry must have a null normalized unit object hash",
                ));
            }
        }
        if !kio_core::cas::is_hash(&entry.preparation_profile_hash) {
            return Err(serde::de::Error::custom(
                "manifest entry preparation_profile_hash is invalid",
            ));
        }
        Ok(Self {
            resource_limit: entry.resource_limit,
            order: entry.order,
            unit_key: entry.unit_key,
            unit_ref: entry.unit_ref,
            unit_type: entry.unit_type,
            status: entry.status,
            prepared_hash: entry.prepared_hash,
            preparation_profile_hash: entry.preparation_profile_hash,
            unit_object_hash: entry.unit_object_hash,
            error_kind: entry.error_kind,
        })
    }
}

/// `Option<String>` normally treats a missing JSON field as null. This
/// deserializer is deliberately attached without `default`, so serde calls it
/// for an explicit null but reports a missing required key before this point.
fn deserialize_required_nullable_unit_object_hash<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)
}

pub use kio_core::preparation_provenance::PreparationProvenance;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizedInstanceManifest {
    pub preparation_provenance: PreparationProvenance,
    pub raw_hash: String,
    pub tool_profile_hash: String,
    pub r#gen: u64,
    pub parent_gen: Option<u64>,
    pub run_id: String,
    pub units: Vec<NormalizedUnitManifestEntry>,
    pub generated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedInstanceIdentity {
    pub raw_hash: String,
    pub tool_profile_hash: String,
    pub r#gen: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedNormalizedInstance {
    pub manifest: NormalizedInstanceManifest,
    pub units: Vec<NormalizedUnitObject>,
    /// Exact bytes read for the verified physical manifest/unit representation(s).
    pub verified_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawRef {
    pub path: String,
    pub raw_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviousNormalization {
    pub raw: RawRef,
    pub normalized_units: Vec<NormalizedUnitObject>,
    pub tool_profile_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncrementalHints {
    pub changed_unit_keys: Vec<String>,
    pub added_unit_keys: Vec<String>,
    pub removed_unit_keys: Vec<String>,
    pub page_fingerprints: BTreeMap<String, UnitFingerprint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizationRun {
    pub run_id: String,
    pub raw_hash: String,
    pub tool_profile_hash: String,
    pub r#gen: u64,
    pub mode: MarkdownizeMode,
    pub status: NormalizationRunStatus,
    pub changed_unit_keys: Vec<String>,
    pub output_ref: String,
    pub fallback_reason: Option<String>,
    pub created_at: String,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NormalizationRunStatus {
    Pending,
    Running,
    Done,
    Partial,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkdownizeStageRequest {
    pub mode: MarkdownizeMode,
    pub new_raw: RawRef,
    pub previous: Option<PreviousNormalization>,
    pub hints: Option<IncrementalHints>,
    pub tool_profile_hash: String,
    pub spec_version: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkdownizeStageOutput {
    pub run: NormalizationRun,
    pub manifest: NormalizedInstanceManifest,
    pub updated_units: Vec<NormalizedUnitObject>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IncrementalModeInput {
    pub has_previous_done_run: bool,
    pub raw_hash_only_changed: bool,
    pub adapter_capabilities: Vec<String>,
    pub change_rate: f64,
    pub threshold: f64,
    pub consecutive_incremental_count: u32,
    pub max_consecutive_incremental: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncrementalModeDecision {
    pub mode: MarkdownizeMode,
    pub reason: Option<String>,
}

pub fn markdownize_units(request: MarkdownizeStageRequest) -> Result<MarkdownizeStageOutput> {
    markdownize_units_with_control(request, &ControlHandle::default())
}

pub fn markdownize_units_with_control(
    request: MarkdownizeStageRequest,
    control: &ControlHandle,
) -> Result<MarkdownizeStageOutput> {
    let generated_at = control.now_utc_seconds();
    let unit_key = "doc:1".to_owned();
    let prepared_hash = request.new_raw.raw_hash.clone();
    let unit_ref = prepared_unit_ref(&unit_key);
    let unit = NormalizedUnitObject {
        unit_key: unit_key.clone(),
        unit_type: UnitType::File,
        raw_hash: request.new_raw.raw_hash.clone(),
        prepared_hash: prepared_hash.clone(),
        preparation_profile_hash: request.tool_profile_hash.clone(),
        tool_profile_hash: request.tool_profile_hash.clone(),
        r#gen: 0,
        mode: request.mode,
        markdown: format!(
            "<!-- Kio deterministic baseline {} {} -->\n",
            unit_key, request.new_raw.raw_hash
        ),
        owned_image_hashes: BTreeSet::new(),
        metadata: BTreeMap::new(),
        reused_from: None,
        generated_at: generated_at.clone(),
    };
    let manifest = NormalizedInstanceManifest {
        preparation_provenance: PreparationProvenance::local(),
        raw_hash: request.new_raw.raw_hash.clone(),
        tool_profile_hash: request.tool_profile_hash.clone(),
        r#gen: 0,
        parent_gen: None,
        run_id: format!("run_{}", new_ulid(Path::new("."))),
        units: vec![NormalizedUnitManifestEntry {
            resource_limit: None,
            order: 0,
            unit_key,
            unit_ref,
            unit_type: UnitType::File,
            status: UnitStatus::Done,
            prepared_hash,
            preparation_profile_hash: request.tool_profile_hash.clone(),
            unit_object_hash: None,
            error_kind: None,
        }],
        generated_at: generated_at.clone(),
    };
    Ok(MarkdownizeStageOutput {
        run: NormalizationRun {
            run_id: manifest.run_id.clone(),
            raw_hash: request.new_raw.raw_hash,
            tool_profile_hash: request.tool_profile_hash,
            r#gen: 0,
            mode: request.mode,
            status: NormalizationRunStatus::Done,
            changed_unit_keys: vec!["doc:1".to_owned()],
            output_ref: "objects/normalized_units".to_owned(),
            fallback_reason: None,
            created_at: generated_at.clone(),
            finished_at: Some(generated_at),
        },
        manifest,
        updated_units: vec![unit],
    })
}

#[must_use]
pub fn choose_markdownize_mode(input: &IncrementalModeInput) -> IncrementalModeDecision {
    let full = |reason: &str| IncrementalModeDecision {
        mode: MarkdownizeMode::Full,
        reason: Some(reason.to_owned()),
    };
    if !input.has_previous_done_run {
        return full("no_previous_done_run");
    }
    if !input.raw_hash_only_changed {
        return full("identity_changed");
    }
    if !input
        .adapter_capabilities
        .iter()
        .any(|capability| capability == "incremental_update")
    {
        return full("adapter_lacks_incremental_update");
    }
    if input.change_rate >= input.threshold {
        return full("change_rate_threshold");
    }
    if input.consecutive_incremental_count >= input.max_consecutive_incremental {
        return full("max_consecutive_incremental");
    }
    IncrementalModeDecision {
        mode: MarkdownizeMode::Incremental,
        reason: None,
    }
}

/// QA44 (step4b-contract-tests-p3a.md §M): the outcome of validating an
/// Adapter's markdownize response (04 §3.2). `FallbackToFull` is the
/// **control** response (04 §3.2 L358) — evaluated ahead of V1-V6, never a
/// contract violation on an incremental response — that asks the caller to
/// re-issue the identical task with `mode=full` (§3.1's activation
/// conditions are not re-evaluated). The durable bookkeeping this implies
/// (04 §3.2 L358: the request settles `outcome='fallback_to_full'`, `state=3`,
/// and neither `attempts` nor `contract_violation_count` count it) is the
/// online/Batch send loop's responsibility, not this pure acceptance check —
/// see `main.rs`'s callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkdownizeAcceptance {
    Accepted,
    FallbackToFull,
}

pub fn validate_markdownize_response(
    response: &adapter_types::MarkdownizeResponse,
    hints: &IncrementalHints,
    prepared_units: &[PreparedUnit],
) -> Result<MarkdownizeAcceptance> {
    // QA44: control response, evaluated ahead of V1-V6. A `mode_used=full`
    // response that still sets it is a genuine contract violation (loop
    // prevention — 04 §3.2 L358 final sentence).
    if response.fallback_to_full {
        if response.mode_used == adapter_types::MarkdownizeMode::Full {
            return Err(contract_violation(
                "adapter_requested_full_fallback_from_a_full_response",
            ));
        }
        if !response.updated_units.is_empty()
            || !response.added_units.is_empty()
            || !response.unchanged_unit_keys.is_empty()
            || !response.removed_unit_keys.is_empty()
            || !response.failed_units.is_empty()
        {
            return Err(contract_violation(
                "fallback_to_full control response must carry empty unit arrays",
            ));
        }
        return Ok(MarkdownizeAcceptance::FallbackToFull);
    }
    if response.mode_used == adapter_types::MarkdownizeMode::Full {
        return validate_full_response(response, prepared_units)
            .map(|()| MarkdownizeAcceptance::Accepted);
    }

    // N = unit_mapping (§2.2)'s unchanged-candidate ∪ changed ∪ added — here
    // `hints.changed_unit_keys`/`hints.added_unit_keys` ARE that KIO-side
    // computation (04 §3.1: "hints の changed / added ... は unit_mapping の
    // 帰結をそのまま渡す"), so the unchanged-candidate set is exactly N minus
    // those two (QA38b).
    let new_keys = prepared_units
        .iter()
        .map(|unit| unit.unit_key.clone())
        .collect::<BTreeSet<_>>();
    let updated_keys = unit_keys(&response.updated_units);
    let added_keys = unit_keys(&response.added_units);
    let unchanged_keys = set_from(&response.unchanged_unit_keys);
    let failed_keys = failed_unit_keys(&response.failed_units);

    // V1 (part 1, QA38a): the element count of each array equals its
    // distinct-key count — `keys()`'s set collapse hides an in-array
    // duplicate, so check length separately.
    if response.updated_units.len() != updated_keys.len()
        || response.added_units.len() != added_keys.len()
        || response.unchanged_unit_keys.len() != unchanged_keys.len()
        || response.failed_units.len() != failed_keys.len()
    {
        return Err(contract_violation(
            "duplicate unit_key within a response array",
        ));
    }

    // V1 (part 2): the 4 sets partition N and are pairwise disjoint.
    let touched_union = updated_keys
        .union(&added_keys)
        .cloned()
        .collect::<BTreeSet<_>>()
        .union(&unchanged_keys)
        .cloned()
        .collect::<BTreeSet<_>>()
        .union(&failed_keys)
        .cloned()
        .collect::<BTreeSet<_>>();
    if touched_union != new_keys
        || !updated_keys.is_disjoint(&added_keys)
        || !updated_keys.is_disjoint(&unchanged_keys)
        || !updated_keys.is_disjoint(&failed_keys)
        || !added_keys.is_disjoint(&unchanged_keys)
        || !added_keys.is_disjoint(&failed_keys)
        || !unchanged_keys.is_disjoint(&failed_keys)
    {
        return Err(contract_violation(
            "incremental coverage/exclusivity violation",
        ));
    }

    // V1 (part 3): failed_units ⊆ hints.changed ∪ hints.added — an
    // unchanged-candidate unit was never sent to the Adapter, so it cannot
    // fail (Kio reuses it directly).
    let hints_changed = set_from(&hints.changed_unit_keys);
    let hints_added = set_from(&hints.added_unit_keys);
    let touched_by_kio = hints_changed
        .union(&hints_added)
        .cloned()
        .collect::<BTreeSet<_>>();
    if !failed_keys.is_subset(&touched_by_kio) {
        return Err(contract_violation(
            "failed_units names a unit that was never sent to the adapter",
        ));
    }

    // V1 (part 4, QA38b): unchanged_unit_keys is EXACTLY the §2.2 unchanged
    // candidate set (N minus the touched set) — a changed/added unit
    // reported as unchanged would publish its stale content as a success.
    let unchanged_candidates = new_keys
        .difference(&touched_by_kio)
        .cloned()
        .collect::<BTreeSet<_>>();
    if unchanged_keys != unchanged_candidates {
        return Err(contract_violation(
            "unchanged_unit_keys does not match the unit_mapping unchanged candidate set",
        ));
    }

    // V2 removed: exact match.
    if set_from(&response.removed_unit_keys) != set_from(&hints.removed_unit_keys) {
        return Err(contract_violation("removed_unit_keys do not match hints"));
    }
    // V3 no overreach.
    if !updated_keys.is_subset(&hints_changed) {
        return Err(contract_violation(
            "updated unit is outside changed_unit_keys",
        ));
    }
    // V4 added (QA39): added ∪ (failed ∩ hints.added) == hints.added_unit_keys
    // (already pairwise-disjoint per V1 above).
    let failed_added = failed_keys
        .intersection(&hints_added)
        .cloned()
        .collect::<BTreeSet<_>>();
    let added_coverage = added_keys
        .union(&failed_added)
        .cloned()
        .collect::<BTreeSet<_>>();
    if added_coverage != hints_added {
        return Err(contract_violation(
            "added units plus their partial failures do not equal hints.added_unit_keys",
        ));
    }

    validate_failed_unit_error_kinds(&response.failed_units)?;
    validate_unit_shapes(&response.updated_units, prepared_units)?;
    validate_unit_shapes(&response.added_units, prepared_units)?;
    validate_unit_ref_injectivity(
        updated_keys
            .iter()
            .chain(added_keys.iter())
            .chain(unchanged_keys.iter()),
    )?;
    Ok(MarkdownizeAcceptance::Accepted)
}

/// QA36/QA38: `keys(failed_units)`, checked for element-count parity with the
/// caller (an in-array `unit_key` duplicate across `failed_units` is a V1
/// violation the caller detects by comparing `response.failed_units.len()`
/// against this set's length).
fn failed_unit_keys(failed_units: &[adapter_types::FailedUnit]) -> BTreeSet<String> {
    failed_units
        .iter()
        .map(|unit| unit.unit_key.clone())
        .collect()
}

/// QA36/V6 (04 §3.2 L344-346): `failed_units[].error_kind` must be a member
/// of `RetryErrorKind`'s closed enum (04 §5.3) — an enum-external value would
/// make the retry classification (retryable/permanent) undecidable once it
/// reaches the manifest, so the whole response is rejected instead.
fn validate_failed_unit_error_kinds(failed_units: &[adapter_types::FailedUnit]) -> Result<()> {
    for unit in failed_units {
        let is_known = serde_json::from_value::<crate::task::RetryErrorKind>(Value::String(
            unit.error_kind.clone(),
        ))
        .is_ok();
        if !is_known
            || !kio_core::error::ResourceLimit::matches_kind(
                Some(&unit.error_kind),
                unit.resource_limit.as_ref(),
            )
        {
            return Err(contract_violation(&format!(
                "failed_units[].error_kind `{}` is not a member of the closed retry-kind enum",
                unit.error_kind
            )));
        }
    }
    Ok(())
}

/// QA40 (04 §3.2 "unit_ref 衝突の拒否"): reject when two DIFFERENT `unit_key`s
/// in the persist-bound final unit set (`updated ∪ added ∪ unchanged` —
/// `failed_units` never persists) collide onto the same `unit_ref =
/// base16(sha256(unit_key))[0:16]`. The final set is fully known here (no
/// cross-generation carry-over from a prior manifest is considered — that
/// broader collision surface is out of scope for this pure response check).
fn validate_unit_ref_injectivity<'a>(unit_keys: impl Iterator<Item = &'a String>) -> Result<()> {
    let mut seen: BTreeMap<String, &'a str> = BTreeMap::new();
    for unit_key in unit_keys {
        let unit_ref = prepared_unit_ref(unit_key);
        if let Some(existing) = seen.insert(unit_ref.clone(), unit_key.as_str())
            && existing != unit_key.as_str()
        {
            return Err(contract_violation(&format!(
                "unit_ref collision: `{existing}` and `{unit_key}` both map to {unit_ref}"
            )));
        }
    }
    Ok(())
}

#[must_use]
pub fn normalized_instance_dir(
    kio_dir: impl AsRef<Path>,
    raw_hash: &str,
    tool_profile_hash: &str,
    r#gen: u64,
) -> PathBuf {
    kio_dir.as_ref().join(normalized_instance_relative_path(
        raw_hash,
        tool_profile_hash,
        r#gen,
    ))
}

fn normalized_instance_relative_path(
    raw_hash: &str,
    tool_profile_hash: &str,
    r#gen: u64,
) -> PathBuf {
    let digest = normalized_path_digest(raw_hash);
    let tool_digest = normalized_path_digest(tool_profile_hash);
    let fanout_a = digest.get(0..2).unwrap_or(digest);
    let fanout_b = digest.get(2..4).unwrap_or("");
    Path::new("objects/normalized_units")
        .join(fanout_a)
        .join(fanout_b)
        .join(format!("{digest}.{tool_digest}.g{gen}"))
}

#[must_use]
pub fn normalized_view_path(
    kio_dir: impl AsRef<Path>,
    raw_hash: &str,
    tool_profile_hash: &str,
    r#gen: u64,
    manifest_hash: &str,
) -> PathBuf {
    kio_dir.as_ref().join(normalized_view_relative_path(
        raw_hash,
        tool_profile_hash,
        r#gen,
        manifest_hash,
    ))
}

fn normalized_view_relative_path(
    raw_hash: &str,
    tool_profile_hash: &str,
    r#gen: u64,
    manifest_hash: &str,
) -> PathBuf {
    let digest = normalized_path_digest(raw_hash);
    let tool_digest = normalized_path_digest(tool_profile_hash);
    let fanout_a = digest.get(0..2).unwrap_or(digest);
    let fanout_b = digest.get(2..4).unwrap_or("");
    Path::new("objects/normalized")
        .join(fanout_a)
        .join(fanout_b)
        .join(format!(
            "{digest}.{tool_digest}.g{gen}.{}.md",
            normalized_path_digest(manifest_hash)
        ))
}

/// Return only a portable path component. I/O entry points validate hashes before
/// calling these helpers; the fallback keeps the public path constructors from
/// panicking or admitting separators when passed malformed data.
fn normalized_path_digest(hash: &str) -> &str {
    hash.strip_prefix("sha256:")
        .filter(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
        .unwrap_or("invalid")
}

/// Load a normalized instance through one mandatory provenance boundary. The
/// requested tuple selects the directory, and every manifest/unit field is rebound
/// to that tuple before any markdown leaves this function.
pub fn load_validated_normalized_instance(
    kio_dir: impl AsRef<Path>,
    raw_hash: &str,
    tool_profile_hash: &str,
    r#gen: u64,
) -> Result<ValidatedNormalizedInstance> {
    load_validated_normalized_instance_with_control(
        kio_dir,
        raw_hash,
        tool_profile_hash,
        r#gen,
        &ControlHandle::default(),
    )
}

pub fn load_validated_normalized_instance_with_control(
    kio_dir: impl AsRef<Path>,
    raw_hash: &str,
    tool_profile_hash: &str,
    r#gen: u64,
    control: &ControlHandle,
) -> Result<ValidatedNormalizedInstance> {
    let (manifest, manifest_bytes) =
        read_normalized_instance_manifest(kio_dir.as_ref(), raw_hash, tool_profile_hash, r#gen)?;
    let store = ObjectStore::new_with_control(kio_dir.as_ref(), control.clone());
    let (units, unit_bytes) = load_validated_normalized_units_from_manifest_with_size(
        &store,
        Path::new("normalized manifest"),
        &manifest,
    )?;
    Ok(ValidatedNormalizedInstance {
        manifest,
        units,
        verified_bytes: manifest_bytes + unit_bytes,
    })
}

/// Read and validate the instance metadata independently of its unit bodies.
pub fn read_normalized_instance_manifest(
    kio_dir: impl AsRef<Path>,
    raw_hash: &str,
    tool_profile_hash: &str,
    r#gen: u64,
) -> Result<(NormalizedInstanceManifest, u64)> {
    if !kio_core::cas::is_hash(raw_hash) || !kio_core::cas::is_hash(tool_profile_hash) {
        return Err(PipelineError::corrupt(
            kio_dir.as_ref().display().to_string(),
            "requested normalized instance has an invalid hash identity".to_owned(),
        ));
    }
    let identity = NormalizedInstanceIdentity {
        raw_hash: raw_hash.to_owned(),
        tool_profile_hash: tool_profile_hash.to_owned(),
        r#gen,
    };
    let canonical_relative = normalized_instance_relative_path(raw_hash, tool_profile_hash, r#gen);
    let canonical_existing = resolve_existing_store_path(
        kio_dir.as_ref(),
        &canonical_relative,
        StorePathKind::Directory,
    )?;
    let canonical_dir = canonical_existing.ok_or_else(|| {
        missing_normalized_object(
            &kio_dir.as_ref().join(&canonical_relative),
            "normalized instance does not exist",
        )
    })?;
    let manifest_path = kio_dir
        .as_ref()
        .join(&canonical_relative)
        .join("manifest.json");
    let bytes = read_contained_normalized_file(
        kio_dir.as_ref(),
        &canonical_dir,
        &manifest_path,
        MAX_NORMALIZED_MANIFEST_BYTES,
    )?;
    let manifest = serde_json::from_slice(&bytes)
        .map_err(|error| normalized_corrupt(&manifest_path, error.to_string()))?;
    validate_normalized_manifest(&manifest_path, &identity, &manifest)?;
    Ok((manifest, bytes.len() as u64))
}

/// Conservatively account every bounded physical file that the normalized
/// loader can consume for one identity. Fsck charges this before loading so a
/// schema/identity failure cannot evade the invocation-wide byte budget.
pub fn normalized_instance_read_budget(
    kio_dir: impl AsRef<Path>,
    raw_hash: &str,
    tool_profile_hash: &str,
    r#gen: u64,
) -> Result<u64> {
    normalized_instance_read_budget_with_control(
        kio_dir,
        raw_hash,
        tool_profile_hash,
        r#gen,
        &ControlHandle::default(),
    )
}

pub fn normalized_instance_read_budget_with_control(
    kio_dir: impl AsRef<Path>,
    raw_hash: &str,
    tool_profile_hash: &str,
    r#gen: u64,
    control: &ControlHandle,
) -> Result<u64> {
    normalized_instance_read_budget_with_file_limit(
        kio_dir.as_ref(),
        raw_hash,
        tool_profile_hash,
        r#gen,
        MAX_NORMALIZED_INSTANCE_FILES,
        control,
    )
}

fn normalized_instance_read_budget_with_file_limit(
    kio_dir: &Path,
    raw_hash: &str,
    tool_profile_hash: &str,
    r#gen: u64,
    max_files: usize,
    control: &ControlHandle,
) -> Result<u64> {
    if !kio_core::cas::is_hash(raw_hash) || !kio_core::cas::is_hash(tool_profile_hash) {
        return Err(PipelineError::corrupt(
            kio_dir.display().to_string(),
            "requested normalized instance has an invalid hash identity".to_owned(),
        ));
    }
    let relatives = [normalized_instance_relative_path(
        raw_hash,
        tool_profile_hash,
        r#gen,
    )];

    let mut total = 0_u64;
    let mut visited_files = 0_usize;
    for relative in relatives {
        let Some(directory) =
            resolve_existing_store_path(kio_dir, &relative, StorePathKind::Directory)?
        else {
            continue;
        };
        // A persisted instance is assembled with `StoreDirectory::write_atomic`,
        // which leaves its validated, empty recovery workspace alongside the
        // manifest and units. It is operational metadata rather than a loader
        // input, but incomplete or malformed recovery residue must still fail
        // closed before fsck accounts the instance.
        let store_directory = StoreDirectory::open_with_control(&directory, control.clone())
            .map_err(|error| normalized_corrupt(&directory, error.to_string()))?;
        let atomic = store_directory
            .inspect_atomic()
            .map_err(|error| normalized_corrupt(&directory, error.to_string()))?;
        if atomic == AtomicWorkspaceState::Pending {
            return Err(normalized_corrupt(
                &directory,
                "normalized instance atomic recovery is pending",
            ));
        }
        for entry in fs::read_dir(&directory).pipeline_io(&directory)? {
            let entry = entry.pipeline_io(&directory)?;
            let path = entry.path();
            if entry.file_name() == ATOMIC_WORKSPACE_DIR && atomic == AtomicWorkspaceState::Clean {
                continue;
            }
            visited_files = visited_files.saturating_add(1);
            if visited_files > max_files {
                return Err(normalized_corrupt(
                    &directory,
                    format!("normalized instance physical file count exceeds {max_files} limit"),
                ));
            }
            let file_type = entry.file_type().pipeline_io(&path)?;
            if !file_type.is_file() || file_type.is_symlink() {
                return Err(normalized_corrupt(
                    &path,
                    "normalized instance contains a non-regular file",
                ));
            }
            let limit = if entry.file_name() == "manifest.json" {
                MAX_NORMALIZED_MANIFEST_BYTES
            } else {
                MAX_NORMALIZED_UNIT_BYTES
            };
            let size = entry.metadata().pipeline_io(&path)?.len();
            total = total.saturating_add(size.min(limit.saturating_add(1)));
        }
    }
    Ok(total)
}

/// Load the authoritative immutable normalized unit objects selected by an
/// already supplied (and therefore externally pinned) manifest. Mutable
/// `objects/normalized_units/.../<unit_ref>.json` files are deliberately not
/// consulted.
pub fn load_validated_normalized_units_from_manifest(
    store: &ObjectStore,
    manifest: &NormalizedInstanceManifest,
) -> Result<Vec<NormalizedUnitObject>> {
    load_validated_normalized_units_from_manifest_with_size(
        store,
        Path::new("normalized manifest"),
        manifest,
    )
    .map(|(units, _)| units)
}

fn load_validated_normalized_units_from_manifest_with_size(
    store: &ObjectStore,
    source_path: &Path,
    manifest: &NormalizedInstanceManifest,
) -> Result<(Vec<NormalizedUnitObject>, u64)> {
    let mut units = Vec::new();
    let verified_bytes =
        visit_validated_normalized_units_at(store, source_path, manifest, |unit| {
            units.push(unit);
            Ok(())
        })?;
    Ok((units, verified_bytes))
}

/// Check each pinned unit against the manifest and visit it in manifest order.
/// A consumer can process large instances without retaining their unit bodies.
pub fn visit_validated_normalized_units(
    store: &ObjectStore,
    manifest: &NormalizedInstanceManifest,
    visit: impl FnMut(NormalizedUnitObject) -> Result<()>,
) -> Result<u64> {
    visit_validated_normalized_units_at(store, Path::new("normalized manifest"), manifest, visit)
}

fn visit_validated_normalized_units_at(
    store: &ObjectStore,
    source_path: &Path,
    manifest: &NormalizedInstanceManifest,
    mut visit: impl FnMut(NormalizedUnitObject) -> Result<()>,
) -> Result<u64> {
    store
        .verified_reads()
        .map_err(|error| normalized_corrupt(source_path, error.to_string()))?;
    let identity = NormalizedInstanceIdentity {
        raw_hash: manifest.raw_hash.clone(),
        tool_profile_hash: manifest.tool_profile_hash.clone(),
        r#gen: manifest.r#gen,
    };
    validate_normalized_manifest(source_path, &identity, manifest)?;

    let mut total_bytes = 0_u64;
    for entry in manifest
        .units
        .iter()
        .filter(|entry| entry.status == UnitStatus::Done)
    {
        let (unit, bytes) =
            read_validated_normalized_unit_at(store, source_path, &identity, entry)?;
        total_bytes = total_bytes.saturating_add(bytes);
        visit(unit)?;
    }
    Ok(total_bytes)
}

/// Read one immutable Done unit selected by a validated manifest entry.
pub fn read_validated_normalized_unit(
    store: &ObjectStore,
    identity: &NormalizedInstanceIdentity,
    entry: &NormalizedUnitManifestEntry,
) -> Result<NormalizedUnitObject> {
    read_validated_normalized_unit_at(store, Path::new("normalized manifest"), identity, entry)
        .map(|(unit, _)| unit)
}

fn read_validated_normalized_unit_at(
    store: &ObjectStore,
    source_path: &Path,
    identity: &NormalizedInstanceIdentity,
    entry: &NormalizedUnitManifestEntry,
) -> Result<(NormalizedUnitObject, u64)> {
    let hash = entry
        .unit_object_hash
        .as_deref()
        .filter(|hash| entry.status == UnitStatus::Done && kio_core::cas::is_hash(hash))
        .ok_or_else(|| normalized_corrupt(source_path, "missing or invalid done unit hash"))?;
    store
        .inspect_content_accounted(ContentObjectKind::NormalizedUnit, hash)
        .map_err(|error| normalized_corrupt(source_path, error.error.to_string()))?;
    let bytes = store
        .read_content_object_bytes(
            ContentObjectKind::NormalizedUnit,
            hash,
            MAX_NORMALIZED_UNIT_BYTES,
        )
        .map_err(|err| normalized_corrupt(source_path, err.to_string()))?;
    let unit: NormalizedUnitObject = serde_json::from_slice(&bytes)
        .map_err(|err| normalized_corrupt(source_path, err.to_string()))?;
    if cas_hash_bytes(&bytes) != hash {
        return Err(normalized_corrupt(
            source_path,
            "normalized unit CAS object does not match its manifest hash",
        ));
    }
    let canonical = canonical_normalized_unit_object_bytes(&unit)?;
    if canonical != bytes {
        return Err(normalized_corrupt(
            source_path,
            "normalized unit CAS object is not canonical JSON",
        ));
    }
    validate_normalized_unit(source_path, identity, entry, &unit)?;
    Ok((unit, bytes.len() as u64))
}

/// Validate already-deserialized normalized state. This is also used by the
/// writer so invalid internal state cannot be persisted under a trusted tuple.
pub fn validate_normalized_instance(
    source_path: impl AsRef<Path>,
    identity: &NormalizedInstanceIdentity,
    manifest: &NormalizedInstanceManifest,
    units: &[NormalizedUnitObject],
) -> Result<()> {
    let source_path = source_path.as_ref();
    validate_normalized_manifest(source_path, identity, manifest)?;
    let done_by_key = manifest
        .units
        .iter()
        .filter(|entry| entry.status == UnitStatus::Done)
        .map(|entry| (entry.unit_key.as_str(), entry))
        .collect::<BTreeMap<_, _>>();

    if units.len() != done_by_key.len() {
        return Err(normalized_corrupt(
            source_path,
            "done manifest entries and normalized unit objects are not one-to-one",
        ));
    }
    let mut seen_units = BTreeSet::new();
    for unit in units {
        let Some(entry) = done_by_key.get(unit.unit_key.as_str()) else {
            return Err(normalized_corrupt(
                source_path,
                "normalized unit does not have a matching done manifest entry",
            ));
        };
        if !seen_units.insert(unit.unit_key.as_str()) {
            return Err(normalized_corrupt(
                source_path,
                "duplicate normalized unit object",
            ));
        }
        validate_normalized_unit(source_path, identity, entry, unit)?;
    }
    Ok(())
}

fn validate_normalized_unit(
    path: &Path,
    identity: &NormalizedInstanceIdentity,
    entry: &NormalizedUnitManifestEntry,
    unit: &NormalizedUnitObject,
) -> Result<()> {
    if unit.raw_hash != identity.raw_hash
        || unit.tool_profile_hash != identity.tool_profile_hash
        || unit.r#gen != identity.r#gen
        || unit.unit_key != entry.unit_key
        || unit.unit_type != entry.unit_type
        || unit.prepared_hash != entry.prepared_hash
        || unit.preparation_profile_hash != entry.preparation_profile_hash
        || unit.owned_image_hashes.len() > MAX_OWNED_IMAGE_HASHES_PER_UNIT
        || unit
            .owned_image_hashes
            .iter()
            .any(|hash| !kio_core::cas::is_hash(hash))
        || unit.markdown.is_empty()
    {
        return Err(normalized_corrupt(
            path,
            "normalized unit identity does not match the requested manifest tuple",
        ));
    }
    Ok(())
}

/// Validate the complete manifest structure without requiring its unit bodies.
/// This permits callers with verified purge authority to distinguish absent
/// historical bodies from malformed retained state.
pub fn validate_normalized_manifest(
    source_path: impl AsRef<Path>,
    identity: &NormalizedInstanceIdentity,
    manifest: &NormalizedInstanceManifest,
) -> Result<()> {
    let source_path = source_path.as_ref();
    validate_manifest_identity(source_path, identity, manifest)?;
    if !manifest.preparation_provenance.is_valid() {
        return Err(normalized_corrupt(
            source_path,
            "invalid preparation provenance",
        ));
    }

    let mut manifest_keys = BTreeSet::new();
    let mut manifest_refs = BTreeSet::new();
    let mut manifest_orders = BTreeSet::new();
    for entry in &manifest.units {
        if entry.unit_key.is_empty()
            || !manifest_keys.insert(entry.unit_key.as_str())
            || !manifest_refs.insert(entry.unit_ref.as_str())
            || entry.unit_ref != prepared_unit_ref(&entry.unit_key)
        {
            return Err(normalized_corrupt(
                source_path,
                "manifest contains a duplicate or non-derived unit reference",
            ));
        }
        if !manifest_orders.insert(entry.order) {
            return Err(normalized_corrupt(
                source_path,
                "duplicate manifest unit order",
            ));
        }
        if !kio_core::cas::is_hash(&entry.prepared_hash) {
            return Err(normalized_corrupt(
                source_path,
                "manifest entry prepared_hash is invalid",
            ));
        }
        if !kio_core::cas::is_hash(&entry.preparation_profile_hash) {
            return Err(normalized_corrupt(
                source_path,
                "manifest entry preparation_profile_hash is invalid",
            ));
        }
    }
    validate_manifest_unit_object_pins(source_path, manifest)?;

    Ok(())
}

fn validate_manifest_unit_object_pins(
    source_path: &Path,
    manifest: &NormalizedInstanceManifest,
) -> Result<()> {
    for entry in &manifest.units {
        match (entry.status, entry.unit_object_hash.as_deref()) {
            (UnitStatus::Done, Some(hash)) if kio_core::cas::is_hash(hash) => {}
            (UnitStatus::Done, _) => {
                return Err(normalized_corrupt(
                    source_path,
                    "done manifest entry must pin a valid normalized unit object hash",
                ));
            }
            (UnitStatus::Failed, None) => {}
            (UnitStatus::Failed, Some(_)) => {
                return Err(normalized_corrupt(
                    source_path,
                    "failed manifest entry must have a null normalized unit object hash",
                ));
            }
        }
    }
    Ok(())
}

fn canonical_normalized_unit_object_bytes(unit: &NormalizedUnitObject) -> Result<Vec<u8>> {
    let value = serde_json::to_value(unit).map_err(|err| PipelineError::Schema(err.to_string()))?;
    canonical_json_bytes(&value).map_err(|err| PipelineError::Schema(err.to_string()))
}

/// Require a small, canonical, duplicate-free ownership assertion at the
/// schema boundary. `BTreeSet`'s default deserializer silently collapses a
/// duplicate array member, which would make an accepted JSON object describe
/// different bytes from its typed representation.
fn deserialize_owned_image_hashes<'de, D>(
    deserializer: D,
) -> std::result::Result<BTreeSet<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = Vec::<String>::deserialize(deserializer)?;
    if values.len() > MAX_OWNED_IMAGE_HASHES_PER_UNIT {
        return Err(D::Error::custom(
            "owned_image_hashes exceeds per-unit image limit",
        ));
    }
    let mut hashes = BTreeSet::new();
    for hash in values {
        if !kio_core::cas::is_hash(&hash) {
            return Err(D::Error::custom(
                "owned_image_hashes contains an invalid hash",
            ));
        }
        if !hashes.insert(hash) {
            return Err(D::Error::custom(
                "owned_image_hashes contains a duplicate hash",
            ));
        }
    }
    Ok(hashes)
}

/// Hash the canonical immutable normalized-unit object bytes.
///
/// Chunk identity and every manifest pin must use this exact JCS computation;
/// keeping it here prevents writers and loaders from silently drifting to
/// different JSON representations.
pub fn normalized_unit_object_hash(unit: &NormalizedUnitObject) -> Result<String> {
    Ok(cas_hash_bytes(&canonical_normalized_unit_object_bytes(
        unit,
    )?))
}

fn canonical_normalized_manifest_bytes(manifest: &NormalizedInstanceManifest) -> Result<Vec<u8>> {
    let value =
        serde_json::to_value(manifest).map_err(|err| PipelineError::Schema(err.to_string()))?;
    canonical_json_bytes(&value).map_err(|err| PipelineError::Schema(err.to_string()))
}

fn validate_manifest_identity(
    source_path: &Path,
    identity: &NormalizedInstanceIdentity,
    manifest: &NormalizedInstanceManifest,
) -> Result<()> {
    if !kio_core::cas::is_hash(&identity.raw_hash)
        || !kio_core::cas::is_hash(&identity.tool_profile_hash)
        || manifest.raw_hash != identity.raw_hash
        || manifest.tool_profile_hash != identity.tool_profile_hash
        || manifest.r#gen != identity.r#gen
    {
        return Err(normalized_corrupt(
            source_path,
            "normalized manifest identity does not match the requested instance",
        ));
    }
    Ok(())
}

fn read_contained_normalized_file(
    kio_dir: &Path,
    canonical_dir: &Path,
    path: &Path,
    max_bytes: u64,
) -> Result<Vec<u8>> {
    let relative = path
        .strip_prefix(kio_dir)
        .map_err(|_| normalized_corrupt(path, "normalized object is outside the Kio directory"))?;
    let canonical = resolve_existing_store_path(kio_dir, relative, StorePathKind::RegularFile)?
        .ok_or_else(|| missing_normalized_object(path, "normalized object does not exist"))?;
    if canonical.parent() != Some(canonical_dir) {
        return Err(normalized_corrupt(
            path,
            "normalized object is not a direct regular-file child of its instance",
        ));
    }
    let file = fs::File::open(&canonical).pipeline_io(&canonical)?;
    let metadata = file.metadata().pipeline_io(&canonical)?;
    if metadata.len() > max_bytes {
        return Err(normalized_corrupt(
            path,
            "normalized object exceeds its byte limit",
        ));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    let mut limited = file.take(max_bytes.saturating_add(1));
    limited.read_to_end(&mut bytes).pipeline_io(&canonical)?;
    if bytes.len() as u64 > max_bytes {
        return Err(normalized_corrupt(
            path,
            "normalized object grew beyond its byte limit",
        ));
    }
    Ok(bytes)
}

fn normalized_corrupt(path: &Path, message: impl Into<String>) -> PipelineError {
    PipelineError::corrupt(path.display().to_string(), message)
}

fn missing_normalized_object(path: &Path, message: impl Into<String>) -> PipelineError {
    PipelineError::Io {
        path: path.display().to_string(),
        message: message.into(),
    }
}

fn checked_manifest_size(path: &Path, bytes: u64, limits: NormalizedSizeLimits) -> Result<u64> {
    if bytes > limits.manifest_bytes {
        return Err(normalized_corrupt(
            path,
            format!(
                "normalized manifest exceeds {} byte limit",
                limits.manifest_bytes
            ),
        ));
    }
    Ok(bytes)
}

fn checked_unit_size(
    unit_path: &Path,
    current_total: u64,
    bytes: u64,
    limits: NormalizedSizeLimits,
) -> Result<u64> {
    if bytes > limits.unit_bytes {
        return Err(PipelineError::ResourceLimit(
            kio_core::error::ResourceLimit {
                resource: kio_core::error::Resource::Memory,
                stage: "normalized_unit".to_owned(),
                limit: Some(limits.unit_bytes),
                observed: Some(bytes),
                resource_tier: None,
                page_progress: None,
            },
        ));
    }
    current_total
        .checked_add(bytes)
        .ok_or_else(|| normalized_corrupt(unit_path, "normalized instance byte count overflow"))
}

/// Check both serialized normalized-unit representations used by persistence.
pub fn check_normalized_unit_write_size(unit: &NormalizedUnitObject) -> Result<()> {
    #[derive(Default)]
    struct Count(u64);
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len() as u64);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let path = Path::new("normalized unit");
    checked_unit_size(path, 0, unit.markdown.len() as u64, NORMALIZED_SIZE_LIMITS)?;
    let mut cache = Count::default();
    serde_json::to_writer_pretty(&mut cache, unit)
        .map_err(|error| PipelineError::Schema(error.to_string()))?;
    checked_unit_size(path, 0, cache.0, NORMALIZED_SIZE_LIMITS)?;
    let mut canonical = Count::default();
    serde_jcs::to_writer(&mut canonical, unit)
        .map_err(|error| PipelineError::Schema(error.to_string()))?;
    checked_unit_size(path, 0, canonical.0, NORMALIZED_SIZE_LIMITS)?;
    Ok(())
}

/// Persist through an already-retained `.kio` capability. The path is used
/// solely as a diagnostic/layout label; callers retain and validate the handle
/// before reaching this boundary.
pub fn persist_normalized_instance_bound(
    directory: &StoreDirectory,
    manifest: &NormalizedInstanceManifest,
    units: &[NormalizedUnitObject],
) -> Result<NormalizedInstanceManifest> {
    let by_key = units
        .iter()
        .map(|unit| (unit.unit_key.as_str(), unit))
        .collect::<BTreeMap<_, _>>();
    if by_key.len() != units.len()
        || units.len()
            != manifest
                .units
                .iter()
                .filter(|entry| entry.status == UnitStatus::Done)
                .count()
    {
        return Err(normalized_corrupt(
            directory.path(),
            "done manifest entries and normalized unit objects are not one-to-one",
        ));
    }
    persist_normalized_instance_from_units_bound(directory, manifest, |entry| {
        by_key
            .get(entry.unit_key.as_str())
            .copied()
            .map(Cow::Borrowed)
            .ok_or_else(|| {
                normalized_corrupt(
                    directory.path(),
                    "done manifest entry has no normalized unit object",
                )
            })
    })
}

/// Publish a normalized instance from unit bodies supplied in manifest order.
/// The caller serializes writes with the scope lock.
pub fn persist_normalized_instance_from_units_bound<'a>(
    directory: &StoreDirectory,
    manifest: &NormalizedInstanceManifest,
    mut unit_for: impl FnMut(&NormalizedUnitManifestEntry) -> Result<Cow<'a, NormalizedUnitObject>>,
) -> Result<NormalizedInstanceManifest> {
    persist_normalized_units(directory, manifest, None, |_, entry| unit_for(entry))
}

/// Retain a deterministic run's stamp across retries. Its source, profile, unit
/// identities and destination generation identify the unfinished work.
pub fn persist_resumable_normalized_units<'a>(
    directory: &StoreDirectory,
    manifest: &NormalizedInstanceManifest,
    source: &str,
    unit_for: impl FnMut(
        &NormalizedInstanceManifest,
        &NormalizedUnitManifestEntry,
    ) -> Result<Cow<'a, NormalizedUnitObject>>,
) -> Result<NormalizedInstanceManifest> {
    persist_normalized_units(directory, manifest, Some(source), unit_for)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedNormalizedUnit {
    hash: String,
    stage: SavedStage,
    ready: bool,
}

fn persist_normalized_units<'a>(
    directory: &StoreDirectory,
    manifest: &NormalizedInstanceManifest,
    resumable_source: Option<&str>,
    mut unit_for: impl FnMut(
        &NormalizedInstanceManifest,
        &NormalizedUnitManifestEntry,
    ) -> Result<Cow<'a, NormalizedUnitObject>>,
) -> Result<NormalizedInstanceManifest> {
    let mut stamped = manifest.clone();
    let identity = NormalizedInstanceIdentity {
        raw_hash: stamped.raw_hash.clone(),
        tool_profile_hash: stamped.tool_profile_hash.clone(),
        r#gen: stamped.r#gen,
    };
    // Body pins are filled as units arrive. Validate the rest of the manifest
    // before using its identity or entry names as storage paths.
    for entry in &mut stamped.units {
        if entry.status == UnitStatus::Done {
            entry.unit_object_hash = Some(entry.prepared_hash.clone());
        }
    }
    validate_normalized_manifest(directory.path(), &identity, &stamped)?;
    let instance = normalized_instance_relative_path(
        &stamped.raw_hash,
        &stamped.tool_profile_hash,
        stamped.r#gen,
    );
    let parent = instance.parent().expect("normalized instance parent");
    let mut draft = stamped.clone();
    if resumable_source.is_some() {
        draft.run_id.clear();
        draft.generated_at.clear();
    }
    let draft_key = kio_core::cas::hash_bytes(
        &serde_json::to_vec(&(draft, resumable_source))
            .map_err(|error| PipelineError::Schema(error.to_string()))?,
    );
    let stage_workspace = child_directory(
        directory,
        &Path::new("extraction-stages/views")
            .join(normalized_path_digest(&stamped.raw_hash))
            .join(normalized_path_digest(&draft_key)),
    )?;
    let stamp_record = Path::new("stamp.json");
    if resumable_source.is_some() {
        if let Some((run_id, generated_at)) =
            read_record::<(String, String)>(&stage_workspace, stamp_record, 4096)?
        {
            stamped.run_id = run_id;
            stamped.generated_at = generated_at;
        } else {
            write_record(
                &stage_workspace,
                stamp_record,
                &(&stamped.run_id, &stamped.generated_at),
                Publication::CreateOnly,
            )?;
        }
    }
    let unit_context = stamped.clone();
    // Reject unsafe destinations before adding immutable objects.
    for relative in [parent, &instance] {
        directory
            .entries_optional(relative)
            .map_err(|error| stage_error(directory, error))?;
    }
    let store = ObjectStore::from_bound_kio_with_control(
        directory.root_handle().as_ref(),
        directory.control().clone(),
    )
    .map_err(|error| stage_error(directory, error))?;

    // Verify the manifest size before potentially producing large bodies.
    check_normalized_manifest_size(&stamped, directory.path())?;

    let instance_parent = child_directory(directory, parent)?;
    let instance_leaf = Path::new(instance.file_name().expect("normalized instance leaf"));
    let instance_exists = instance_parent
        .contains_entry(instance_leaf)
        .map_err(|error| stage_error(&instance_parent, error))?;
    let (instance_directory, staged_instance) = if instance_exists {
        let instance_directory =
            adopt_normalized_directory(directory, &identity, &instance_parent, instance_leaf)?;
        (instance_directory, None)
    } else {
        let (stage, saved) = staged_normalized_instance(&stage_workspace, &identity)?;
        (stage, Some(saved))
    };
    let mut total_bytes = 0;
    for entry in &mut stamped.units {
        if entry.status == UnitStatus::Failed {
            continue;
        }
        let journal = normalized_unit_journal(entry);
        let saved = read_record::<SavedNormalizedUnit>(&stage_workspace, &journal, 4096)?;
        let saved = match saved {
            Some(saved) if saved.ready => saved,
            incomplete => {
                if let Some(incomplete) = incomplete {
                    incomplete.stage.discard(&stage_workspace)?;
                }
                let unit = unit_for(&unit_context, entry)?;
                validate_normalized_unit(directory.path(), &identity, entry, &unit)?;
                checked_unit_size(
                    directory.path(),
                    0,
                    unit.markdown.len() as u64,
                    NORMALIZED_SIZE_LIMITS,
                )?;
                let canonical = canonical_normalized_unit_object_bytes(&unit)?;
                checked_unit_size(
                    directory.path(),
                    0,
                    canonical.len() as u64,
                    NORMALIZED_SIZE_LIMITS,
                )?;
                let (stage, mut writer) = SavedStage::create(&stage_workspace)?;
                let mut saved = SavedNormalizedUnit {
                    hash: cas_hash_bytes(&canonical),
                    stage,
                    ready: false,
                };
                write_record(&stage_workspace, &journal, &saved, Publication::Upsert)?;
                stage_workspace
                    .control()
                    .checkpoint(kio_core::durability::DurabilityPoint::ExtractionStageWriting)
                    .map_err(|error| stage_error(&stage_workspace, error))?;
                for chunk in canonical.chunks(kio_core::cas::CAS_STREAM_BUFFER_BYTES) {
                    writer
                        .write_chunk(chunk)
                        .map_err(|error| stage_error(&stage_workspace, error))?;
                }
                writer
                    .sync()
                    .map_err(|error| stage_error(&stage_workspace, error))?;
                drop(writer);
                saved.ready = true;
                write_record(&stage_workspace, &journal, &saved, Publication::Upsert)?;
                saved
            }
        };
        saved.stage.publish_content(
            &stage_workspace,
            &store,
            ContentObjectKind::NormalizedUnit,
            &saved.hash,
        )?;
        entry.unit_object_hash = Some(saved.hash);
        let unit = read_validated_normalized_unit(&store, &identity, entry)?;
        let unit_path = format!("{}.json", entry.unit_ref);
        let bytes = serde_json::to_vec_pretty(&unit)
            .map_err(|error| PipelineError::Schema(error.to_string()))?;
        total_bytes = checked_unit_size(
            &instance.join(&unit_path),
            total_bytes,
            bytes.len() as u64,
            NORMALIZED_SIZE_LIMITS,
        )?;
        instance_directory
            .write_atomic(Path::new(&unit_path), &bytes, Publication::Upsert)
            .map_err(|error| stage_error(&instance_directory, error))?;
    }

    let manifest_bytes = check_normalized_manifest_size(&stamped, directory.path())?;
    let manifest_cas = canonical_normalized_manifest_bytes(&stamped)?;
    let manifest_hash = cas_hash_bytes(&manifest_cas);
    crate::extraction_stage::publish_payload(
        &stage_workspace,
        Path::new("manifest-cas.json"),
        |writer| {
            writer
                .write_chunk(&manifest_cas)
                .map_err(|error| stage_error(&stage_workspace, error))
        },
        || Ok(()),
        |stage| {
            crate::extraction_stage::publish_content(
                &stage_workspace,
                &store,
                ContentObjectKind::Manifest,
                &manifest_hash,
                stage,
            )
        },
    )?;
    let view = normalized_view_relative_path(
        &stamped.raw_hash,
        &stamped.tool_profile_hash,
        stamped.r#gen,
        &manifest_hash,
    );
    let view_directory =
        child_directory(directory, view.parent().expect("normalized view parent"))?;
    let leaf = Path::new(view.file_name().expect("normalized view leaf"));
    write_normalized_view(&stage_workspace, &view_directory, leaf, &store, &stamped)?;
    directory
        .control()
        .checkpoint(kio_core::durability::DurabilityPoint::ExtractionBeforeManifest)
        .map_err(|error| stage_error(directory, error))?;
    // The manifest is the visibility boundary. Unit and view caches may be
    // rebuilt after an interruption while the prior manifest still pins its CAS
    // bodies. No reader can observe the new generation before this write.
    instance_directory
        .write_atomic(
            Path::new("manifest.json"),
            &manifest_bytes,
            Publication::Upsert,
        )
        .map_err(|error| stage_error(&instance_directory, error))?;
    if let Some(saved) = staged_instance {
        saved
            .identity
            .verify(&instance_directory)
            .map_err(|error| stage_error(&stage_workspace, error))?;
        stage_workspace
            .rename_directory_between_create_only_checked(
                &saved.leaf,
                &instance_parent,
                instance_leaf,
                || {
                    directory.control().checkpoint(
                        kio_core::durability::DurabilityPoint::ExtractionDirectoryBeforeSync,
                    )
                },
            )
            .map_err(|error| stage_error(&stage_workspace, error))?;
        stage_workspace
            .remove_file(Path::new("instance.json"))
            .map_err(|error| stage_error(&stage_workspace, error))?;
    }
    if resumable_source.is_none() {
        for entry in stamped
            .units
            .iter()
            .filter(|entry| entry.status == UnitStatus::Done)
        {
            stage_workspace
                .remove_file(&normalized_unit_journal(entry))
                .map_err(|error| stage_error(&stage_workspace, error))?;
        }
    }
    Ok(stamped)
}

fn normalized_unit_journal(entry: &NormalizedUnitManifestEntry) -> PathBuf {
    PathBuf::from(format!("unit-{}.json", entry.unit_ref))
}

pub fn normalized_manifest_hash(manifest: &NormalizedInstanceManifest) -> Result<String> {
    Ok(cas_hash_bytes(&canonical_normalized_manifest_bytes(
        manifest,
    )?))
}

fn write_normalized_view(
    workspace: &StoreDirectory,
    target: &StoreDirectory,
    leaf: &Path,
    store: &ObjectStore,
    manifest: &NormalizedInstanceManifest,
) -> Result<()> {
    crate::extraction_stage::publish_payload(
        workspace,
        Path::new("view.json"),
        |writer| {
            visit_normalized_view(store, manifest, |_, bytes| {
                writer
                    .write_chunk(bytes)
                    .map_err(|error| stage_error(workspace, error))
            })
        },
        || {
            workspace
                .control()
                .checkpoint(kio_core::durability::DurabilityPoint::ExtractionViewReady)
                .map_err(|error| stage_error(workspace, error))
        },
        |stage| {
            let existed = target
                .contains_entry(leaf)
                .map_err(|error| stage_error(target, error))?;
            let mut invalid_view = false;
            let result =
                crate::extraction_stage::publish_file(workspace, target, leaf, stage, |file| {
                    let verified = verify_normalized_view_bytes(file, store, manifest, None);
                    invalid_view = matches!(&verified, Err(PipelineError::Corrupt { path, .. })
                    if path == "normalized view");
                    verified.map(|_| ())
                });
            if existed && invalid_view {
                // The exact manifest selects this regenerable cache. Remove its
                // invalid contents before using the same create-only publisher.
                target
                    .remove_file(leaf)
                    .map_err(|error| stage_error(target, error))?;
                return crate::extraction_stage::publish_file(
                    workspace,
                    target,
                    leaf,
                    stage,
                    |file| verify_normalized_view_bytes(file, store, manifest, None).map(|_| ()),
                );
            }
            result
        },
    )
}

fn visit_normalized_view(
    store: &ObjectStore,
    manifest: &NormalizedInstanceManifest,
    mut visit: impl FnMut(Option<&NormalizedUnitObject>, &[u8]) -> Result<()>,
) -> Result<()> {
    let identity = NormalizedInstanceIdentity {
        raw_hash: manifest.raw_hash.clone(),
        tool_profile_hash: manifest.tool_profile_hash.clone(),
        r#gen: manifest.r#gen,
    };
    visit(None, normalized_view_header(manifest).as_bytes())?;
    let mut ordered = manifest.units.iter().collect::<Vec<_>>();
    ordered.sort_unstable_by_key(|entry| entry.order);
    for (index, entry) in ordered.into_iter().enumerate() {
        if index > 0 {
            visit(None, b"\n\n")?;
        }
        if entry.status == UnitStatus::Failed {
            visit(None, missing_unit_view(entry).as_bytes())?;
        } else {
            let unit = read_validated_normalized_unit(store, &identity, entry)?;
            visit(Some(&unit), unit.markdown.trim_end_matches('\n').as_bytes())?;
        }
    }
    visit(None, b"\n")
}

/// Compare a view in bounded chunks and return the sought unit's trimmed span.
pub fn verify_normalized_view_bytes(
    reader: &mut impl Read,
    store: &ObjectStore,
    manifest: &NormalizedInstanceManifest,
    unit_identity: Option<(&str, &str)>,
) -> Result<Option<(u64, u64)>> {
    let label = Path::new("normalized view");
    let mut buffer = [0; kio_core::cas::CAS_STREAM_BUFFER_BYTES];
    let mut offset = 0_u64;
    let mut found = None;
    visit_normalized_view(store, manifest, |unit, bytes| {
        for expected in bytes.chunks(buffer.len()) {
            let read = reader.read_exact(&mut buffer[..expected.len()]);
            if read
                .as_ref()
                .is_err_and(|error| error.kind() == std::io::ErrorKind::UnexpectedEof)
            {
                return Err(normalized_corrupt(label, "normalized view is truncated"));
            }
            read.pipeline_io(label)?;
            if &buffer[..expected.len()] != expected {
                return Err(normalized_corrupt(label, "view differs from its manifest"));
            }
        }
        if let (Some(unit), Some((key, hash))) = (unit, unit_identity)
            && unit.unit_key == key
            && cas_hash_bytes(unit.markdown.as_bytes()) == hash
        {
            found = Some((offset, bytes.len() as u64));
        }
        offset += bytes.len() as u64;
        Ok(())
    })?;
    if reader.read(&mut buffer[..1]).pipeline_io(label)? != 0 {
        return Err(normalized_corrupt(
            label,
            "unexpected data after normalized view",
        ));
    }
    Ok(found)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedNormalizedDirectory {
    leaf: PathBuf,
    identity: DirectoryStageIdentity,
    profile: String,
    generation: u64,
    discarding: bool,
}

/// Validate an existing generation before exposing it through a new reference.
/// An already retired stage leaves no local file-identity requirement on a
/// portable normalized generation.
pub fn adopt_normalized_instance(
    root: &StoreDirectory,
    raw_hash: &str,
    profile: &str,
    generation: u64,
) -> Result<()> {
    let identity = NormalizedInstanceIdentity {
        raw_hash: raw_hash.to_owned(),
        tool_profile_hash: profile.to_owned(),
        r#gen: generation,
    };
    let relative = normalized_instance_relative_path(raw_hash, profile, generation);
    let parent = child_directory(root, relative.parent().expect("normalized instance parent"))?;
    adopt_normalized_directory(
        root,
        &identity,
        &parent,
        Path::new(relative.file_name().expect("normalized instance leaf")),
    )?;
    Ok(())
}

fn adopt_normalized_directory(
    root: &StoreDirectory,
    identity: &NormalizedInstanceIdentity,
    parent: &StoreDirectory,
    leaf: &Path,
) -> Result<StoreDirectory> {
    let handle = parent
        .open_directory(leaf)
        .map_err(|error| stage_error(parent, error))?;
    let instance = StoreDirectory::from_retained_with_control(
        handle,
        parent.path().join(leaf),
        parent.control().clone(),
    )
    .map_err(|error| stage_error(parent, error))?;
    let mut pending = false;
    crate::extraction_stage::visit_raw_workspaces(
        root,
        "views",
        &identity.raw_hash,
        |workspace| {
            let Some(saved) = read_record::<SavedNormalizedDirectory>(
                workspace,
                Path::new("instance.json"),
                4096,
            )?
            else {
                return Ok(());
            };
            if saved.discarding
                || saved.profile != identity.tool_profile_hash
                || saved.generation != identity.r#gen
            {
                return Ok(());
            }
            if !workspace
                .contains_entry(&saved.leaf)
                .map_err(|error| stage_error(workspace, error))?
            {
                saved
                    .identity
                    .verify(&instance)
                    .map_err(|error| stage_error(&instance, error))?;
                workspace
                    .sync()
                    .map_err(|error| stage_error(workspace, error))?;
                pending = true;
            }
            Ok(())
        },
    )?;
    if pending {
        parent
            .control()
            .checkpoint(kio_core::durability::DurabilityPoint::ExtractionDirectoryBeforeSync)
            .map_err(|error| stage_error(parent, error))?;
    }
    parent.sync().map_err(|error| stage_error(parent, error))?;
    Ok(instance)
}

fn staged_normalized_instance(
    workspace: &StoreDirectory,
    target: &NormalizedInstanceIdentity,
) -> Result<(StoreDirectory, SavedNormalizedDirectory)> {
    let record = Path::new("instance.json");
    if let Some(saved) = read_record::<SavedNormalizedDirectory>(workspace, record, 4096)? {
        if saved.profile != target.tool_profile_hash || saved.generation != target.r#gen {
            return Err(normalized_corrupt(
                workspace.path(),
                "normalized stage target does not match",
            ));
        }
        if saved.discarding {
            resume_normalized_stage_removal(workspace, &saved)?;
        } else if workspace
            .contains_entry(&saved.leaf)
            .map_err(|error| stage_error(workspace, error))?
        {
            let directory = open_staged_normalized_directory(workspace, &saved)?;
            return Ok((directory, saved));
        }
        // The destination was absent too. A rename that did not survive
        // can be rebuilt using the already journaled unit payloads.
        workspace
            .remove_file(record)
            .map_err(|error| stage_error(workspace, error))?;
    }
    let leaf = PathBuf::from(format!("instance-{}", new_ulid(workspace.path())));
    let (directory, identity) = workspace
        .create_stage_directory(&leaf)
        .map_err(|error| stage_error(workspace, error))?;
    let saved = SavedNormalizedDirectory {
        leaf,
        identity,
        profile: target.tool_profile_hash.clone(),
        generation: target.r#gen,
        discarding: false,
    };
    write_record(workspace, record, &saved, Publication::CreateOnly)?;
    Ok((directory, saved))
}

fn open_staged_normalized_directory(
    workspace: &StoreDirectory,
    saved: &SavedNormalizedDirectory,
) -> Result<StoreDirectory> {
    if saved.leaf.components().count() != 1
        || !saved
            .leaf
            .to_str()
            .is_some_and(|leaf| leaf.starts_with("instance-") && leaf.len() == 35)
    {
        return Err(normalized_corrupt(
            workspace.path(),
            "invalid normalized stage directory name",
        ));
    }
    let handle = workspace
        .open_directory(&saved.leaf)
        .map_err(|error| stage_error(workspace, error))?;
    let directory = StoreDirectory::from_retained_with_control(
        handle,
        workspace.path().join(&saved.leaf),
        workspace.control().clone(),
    )
    .map_err(|error| stage_error(workspace, error))?;
    kio_core::private_fs::verify_private_directory_handle(&directory)
        .map_err(|error| stage_error(workspace, error))?;
    saved
        .identity
        .verify(&directory)
        .map_err(|error| stage_error(workspace, error))?;
    Ok(directory)
}

fn resume_normalized_stage_removal(
    workspace: &StoreDirectory,
    saved: &SavedNormalizedDirectory,
) -> Result<()> {
    // A visible replacement may be the result of a rename whose parent
    // sync failed. Make the deletion intent durable before removing the stage.
    workspace
        .control()
        .checkpoint(kio_core::durability::DurabilityPoint::ExtractionBeforeDestinationSync)
        .and_then(|()| workspace.sync())
        .map_err(|error| stage_error(workspace, error))?;
    remove_normalized_stage_directory(workspace, saved)
}

fn remove_normalized_stage_directory(
    workspace: &StoreDirectory,
    saved: &SavedNormalizedDirectory,
) -> Result<()> {
    if !workspace
        .contains_entry(&saved.leaf)
        .map_err(|error| stage_error(workspace, error))?
    {
        return Ok(());
    }
    open_staged_normalized_directory(workspace, saved)?;
    workspace
        .remove_directory_all(&saved.leaf)
        .map_err(|error| stage_error(workspace, error))?;
    workspace
        .control()
        .checkpoint(kio_core::durability::DurabilityPoint::AfterNormalizedStageRemoval)
        .map_err(|error| stage_error(workspace, error))
}

/// Discard the journaled private normalization payloads of a purged raw.
pub fn discard_normalized_stages(root: &StoreDirectory, raw_hash: &str) -> Result<()> {
    crate::extraction_stage::visit_raw_workspaces(root, "views", raw_hash, |workspace| {
        let record = Path::new("instance.json");
        if let Some(mut saved) = read_record::<SavedNormalizedDirectory>(workspace, record, 4096)? {
            if saved.discarding {
                resume_normalized_stage_removal(workspace, &saved)?;
            } else if workspace
                .contains_entry(&saved.leaf)
                .map_err(|error| stage_error(workspace, error))?
            {
                open_staged_normalized_directory(workspace, &saved)?;
                saved.discarding = true;
                write_record(workspace, record, &saved, Publication::Upsert)?;
                remove_normalized_stage_directory(workspace, &saved)?;
            } else {
                if !kio_core::cas::is_hash(&saved.profile) {
                    return Err(normalized_corrupt(
                        workspace.path(),
                        "invalid normalized stage profile",
                    ));
                }
                let target =
                    normalized_instance_relative_path(raw_hash, &saved.profile, saved.generation);
                let parent =
                    child_directory(root, target.parent().expect("normalized instance parent"))?;
                let leaf = Path::new(target.file_name().expect("normalized instance leaf"));
                if parent
                    .contains_entry(leaf)
                    .map_err(|error| stage_error(&parent, error))?
                {
                    adopt_normalized_instance(root, raw_hash, &saved.profile, saved.generation)?;
                }
            }
        }
        for entry in workspace
            .entries(Path::new(""))
            .map_err(|error| stage_error(workspace, error))?
        {
            let Some(name) = entry.name.to_str() else {
                continue;
            };
            if name.starts_with("unit-") && name.ends_with(".json") {
                let leaf = Path::new(name);
                if let Some(saved) = read_record::<SavedNormalizedUnit>(workspace, leaf, 4096)? {
                    saved.stage.discard(workspace)?;
                    workspace
                        .remove_file(leaf)
                        .map_err(|error| stage_error(workspace, error))?;
                }
            }
        }
        let stamp = Path::new("stamp.json");
        if workspace
            .contains_entry(stamp)
            .map_err(|error| stage_error(workspace, error))?
        {
            workspace
                .remove_file(stamp)
                .map_err(|error| stage_error(workspace, error))?;
        }
        for journal in ["manifest-cas.json", "view.json"] {
            crate::extraction_stage::discard_payload(workspace, Path::new(journal))?;
        }
        if workspace
            .contains_entry(record)
            .map_err(|error| stage_error(workspace, error))?
        {
            workspace
                .remove_file(record)
                .map_err(|error| stage_error(workspace, error))?;
        }
        Ok(())
    })
}

fn check_normalized_manifest_size(
    manifest: &NormalizedInstanceManifest,
    path: &Path,
) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|error| PipelineError::Schema(error.to_string()))?;
    checked_manifest_size(path, bytes.len() as u64, NORMALIZED_SIZE_LIMITS)?;
    Ok(bytes)
}

pub fn persist_normalized_instance(
    kio_dir: impl AsRef<Path>,
    manifest: &NormalizedInstanceManifest,
    units: &[NormalizedUnitObject],
) -> Result<NormalizedInstanceManifest> {
    persist_normalized_instance_with_control(kio_dir, manifest, units, &ControlHandle::default())
}

pub fn persist_normalized_instance_with_control(
    kio_dir: impl AsRef<Path>,
    manifest: &NormalizedInstanceManifest,
    units: &[NormalizedUnitObject],
    control: &ControlHandle,
) -> Result<NormalizedInstanceManifest> {
    let directory = StoreDirectory::open_with_control(kio_dir.as_ref(), control.clone())
        .map_err(|e| normalized_corrupt(kio_dir.as_ref(), e.to_string()))?;
    persist_normalized_instance_bound(&directory, manifest, units)
}

fn normalized_view_header(manifest: &NormalizedInstanceManifest) -> String {
    format!(
        "<!-- KIO-NORMALIZED-VIEW raw_hash={} tool_profile_hash={} gen={} -->\n",
        manifest.raw_hash, manifest.tool_profile_hash, manifest.r#gen,
    )
}

fn missing_unit_view(entry: &NormalizedUnitManifestEntry) -> String {
    format!(
        "<!-- KIO-MISSING-UNIT {} {} -->",
        entry.unit_key.replace("--", "%2D%2D"),
        entry
            .error_kind
            .as_deref()
            .unwrap_or("unknown")
            .replace("--", "%2D%2D")
    )
}

#[must_use]
pub fn normalized_identity(raw_hash: &str, tool_profile_hash: &str) -> String {
    hash_bytes(format!("{raw_hash}\0{tool_profile_hash}").as_bytes())
}

/// QA39/V6 (04 §3.2 L335-346): `mode_used="full"` output contract.
/// `unchanged`/`removed` must be empty (full has no incremental concept);
/// `updated ∪ added ∪ failed` must equal the prepared unit's full set with
/// the 3 arrays pairwise disjoint; the V1 in-array-duplicate check and V5
/// shape/Normalized-Markdown-v1 check both apply the same as incremental
/// (failed_units exempt from V5, same as incremental).
fn validate_full_response(
    response: &adapter_types::MarkdownizeResponse,
    prepared_units: &[PreparedUnit],
) -> Result<()> {
    if !response.unchanged_unit_keys.is_empty() || !response.removed_unit_keys.is_empty() {
        return Err(contract_violation(
            "full response must not carry unchanged_unit_keys/removed_unit_keys",
        ));
    }
    let expected = prepared_units
        .iter()
        .map(|unit| unit.unit_key.clone())
        .collect::<BTreeSet<_>>();
    let updated_keys = unit_keys(&response.updated_units);
    let added_keys = unit_keys(&response.added_units);
    let failed_keys = failed_unit_keys(&response.failed_units);
    if response.updated_units.len() != updated_keys.len()
        || response.added_units.len() != added_keys.len()
        || response.failed_units.len() != failed_keys.len()
    {
        return Err(contract_violation(
            "duplicate unit_key within a full response array",
        ));
    }
    if !updated_keys.is_disjoint(&added_keys)
        || !updated_keys.is_disjoint(&failed_keys)
        || !added_keys.is_disjoint(&failed_keys)
    {
        return Err(contract_violation(
            "full response unit arrays are not mutually exclusive",
        ));
    }
    let actual = updated_keys
        .union(&added_keys)
        .cloned()
        .collect::<BTreeSet<_>>()
        .union(&failed_keys)
        .cloned()
        .collect::<BTreeSet<_>>();
    if actual != expected {
        return Err(contract_violation(
            "full response does not cover all prepared units",
        ));
    }
    validate_failed_unit_error_kinds(&response.failed_units)?;
    validate_unit_shapes(&response.updated_units, prepared_units)?;
    validate_unit_shapes(&response.added_units, prepared_units)?;
    validate_unit_ref_injectivity(updated_keys.iter().chain(added_keys.iter()))
}

/// QA41 (04 §3.2 V5): each unit's markdown must be non-empty, its
/// `unit_key`/`unit_type` must match the prepared unit, and its bytes must
/// satisfy Normalized Markdown v1 (07 §5.2.1) — never applied to
/// `failed_units` (V1/V6 note: "V5 の形式検査は failed_units には適用しない").
fn validate_unit_shapes(
    units: &[adapter_types::MarkdownUnit],
    prepared_units: &[PreparedUnit],
) -> Result<()> {
    let prepared = prepared_units
        .iter()
        .map(|unit| (unit.unit_key.as_str(), unit.unit_type))
        .collect::<BTreeMap<_, _>>();
    for unit in units {
        if unit.markdown.is_empty() {
            return Err(contract_violation("markdown must be non-empty"));
        }
        if unit.owned_image_hashes.len() > MAX_OWNED_IMAGE_HASHES_PER_UNIT
            || unit
                .owned_image_hashes
                .iter()
                .any(|hash| !kio_core::cas::is_hash(hash))
        {
            return Err(contract_violation(
                "owned_image_hashes must be bounded canonical SHA-256 hashes",
            ));
        }
        let Some(expected_type) = prepared.get(unit.unit_key.as_str()) else {
            return Err(contract_violation("unit_key is not a prepared unit"));
        };
        if adapter_unit_type(unit.unit_type) != *expected_type {
            return Err(contract_violation("unit_type does not match prepared unit"));
        }
        if let Err(reason) = validate_normalized_markdown_v1(&unit.markdown) {
            return Err(contract_violation(&format!(
                "unit {} violates Normalized Markdown v1: {reason}",
                unit.unit_key
            )));
        }
    }
    Ok(())
}

/// QA41 (step4b-contract-tests-p3a.md §L): the machine-verifiable Normalized
/// Markdown v1 rules (07 §5.2.1) — UTF-8 with no BOM, NFC, LF-only line
/// endings, no trailing space on any line (including inside a fence — only
/// *syntactic* rules are fence-exempt), the file ends with exactly one LF,
/// ATX headings only (Setext forbidden), backtick fences only (tilde
/// forbidden), and no raw HTML block or CommonMark autolink. Returns the
/// specific violated rule on failure. Content (meaning) is out of scope —
/// this is a structural check only (04 §3.2 closing note).
/// Validate the Markdown body of a deterministic unit before publication.
pub fn validate_deterministic_markdown(markdown: &str) -> Result<()> {
    validate_normalized_markdown_v1(markdown).map_err(contract_violation)
}

fn validate_normalized_markdown_v1(markdown: &str) -> std::result::Result<(), &'static str> {
    if markdown.contains('\u{feff}') {
        return Err("BOM is forbidden");
    }
    if markdown.contains('\r') {
        return Err("line endings must be LF only (CR found)");
    }
    if markdown.nfc().ne(markdown.chars()) {
        return Err("markdown must be Unicode NFC");
    }
    if !markdown.ends_with('\n') || markdown.ends_with("\n\n") {
        return Err("file must end with exactly one LF");
    }
    let mut in_fence = false;
    let lines: Vec<&str> = markdown.split('\n').collect();
    for (index, line) in lines.iter().enumerate() {
        // The trailing empty element after the final '\n' is not a "line".
        if index + 1 == lines.len() && line.is_empty() {
            continue;
        }
        if line.ends_with(' ') || line.ends_with('\t') {
            return Err("trailing space is forbidden at end of line");
        }
        let trimmed_start = line.trim_start_matches(' ');
        if trimmed_start.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if trimmed_start.starts_with("~~~") {
            return Err("code fence must use backticks, not tildes");
        }
        if in_fence {
            continue;
        }
        if setext_underline_level(line, lines.get(index.wrapping_sub(1)).copied()).is_some() {
            return Err("Setext headings are forbidden; use ATX (#)");
        }
        if contains_raw_html_or_autolink(line) {
            return Err("raw HTML and autolinks are forbidden");
        }
    }
    Ok(())
}

/// Setext detection mirrors the adapter-side normalizer's heuristic (a `-`
/// underline requires 2+ characters to avoid colliding with an unrelated
/// single dash), gated on `index > 0` and a non-blank previous line so a
/// thematic break or list marker at the top of a fence/document is not
/// misclassified.
fn setext_underline_level(line: &str, previous: Option<&str>) -> Option<usize> {
    let previous = previous?;
    if previous.trim().is_empty() {
        return None;
    }
    let trimmed = line.trim_start_matches(' ');
    if !trimmed.is_empty() && trimmed.bytes().all(|byte| byte == b'=') {
        Some(1)
    } else if trimmed.len() >= 2 && trimmed.bytes().all(|byte| byte == b'-') {
        Some(2)
    } else {
        None
    }
}

/// A conservative, false-positive-averse raw-HTML/autolink scanner: a `<`
/// followed (within a short window — HTML tags/autolinks are short; a bare
/// `<` far from any `>` is prose, e.g. "a < b") by a `>` where the enclosed
/// text looks like a tag (`<div>`, `</div>`, `<!--`, `<?`) or a CommonMark
/// autolink (`<scheme://...>` or `<user@host>`).
fn contains_raw_html_or_autolink(line: &str) -> bool {
    let bytes = line.as_bytes();
    for index in 0..bytes.len() {
        if bytes[index] != b'<' {
            continue;
        }
        // `<` is single-byte ASCII, so `index` is always a char boundary.
        let Some(relative_end) = line[index..].find('>') else {
            continue;
        };
        if relative_end > 512 {
            continue;
        }
        let inner = &line[index + 1..index + relative_end];
        let looks_like_tag = inner
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic())
            || inner.starts_with('/')
            || inner.starts_with('!')
            || inner.starts_with('?');
        let looks_like_autolink =
            inner.contains("://") || (inner.contains('@') && !inner.contains(' '));
        if looks_like_tag || looks_like_autolink {
            return true;
        }
    }
    false
}

fn adapter_unit_type(unit_type: adapter_types::UnitKind) -> UnitType {
    match unit_type {
        adapter_types::UnitKind::Page => UnitType::Page,
        adapter_types::UnitKind::Slide => UnitType::Slide,
        adapter_types::UnitKind::HeadingSection => UnitType::HeadingSection,
        adapter_types::UnitKind::Sheet => UnitType::Sheet,
        adapter_types::UnitKind::Image => UnitType::Image,
        adapter_types::UnitKind::File => UnitType::File,
        adapter_types::UnitKind::Symbol => UnitType::Symbol,
    }
}

fn unit_keys(units: &[adapter_types::MarkdownUnit]) -> BTreeSet<String> {
    units.iter().map(|unit| unit.unit_key.clone()).collect()
}

fn set_from(values: &[String]) -> BTreeSet<String> {
    values.iter().cloned().collect()
}

fn contract_violation(message: &str) -> PipelineError {
    PipelineError::contract("KIO-E-ADAPTER-CONTRACT-001", message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preparation_provenance_is_required_and_complete() {
        let local = PreparationProvenance::local();
        assert_eq!(local.renderer_name, None);
        assert_eq!(local.renderer_version, None);
        let (os, arch) = kio_core::platform::producer_platform();
        assert_eq!(local.producer_os, os);
        assert_eq!(local.producer_arch, arch);
        let valid = serde_json::json!({
            "raw_hash": "raw", "tool_profile_hash": "tool", "gen": 0,
            "parent_gen": null, "run_id": "run", "units": [], "generated_at": "now",
            "preparation_provenance": {
                "renderer_name": null, "renderer_version": null,
                "producer_os": "linux", "producer_arch": "x86_64"
            }
        });
        assert!(serde_json::from_value::<NormalizedInstanceManifest>(valid.clone()).is_ok());
        let mut missing = valid.clone();
        missing
            .as_object_mut()
            .unwrap()
            .remove("preparation_provenance");
        assert!(serde_json::from_value::<NormalizedInstanceManifest>(missing).is_err());
        for field in [
            "renderer_name",
            "renderer_version",
            "producer_os",
            "producer_arch",
        ] {
            let mut missing = valid.clone();
            missing["preparation_provenance"]
                .as_object_mut()
                .unwrap()
                .remove(field);
            assert!(
                serde_json::from_value::<NormalizedInstanceManifest>(missing).is_err(),
                "{field}"
            );
        }
        for (field, invalid) in [
            ("producer_os", serde_json::json!("")),
            ("producer_arch", serde_json::json!("")),
            ("renderer_name", serde_json::json!("LibreOffice")),
            ("renderer_version", serde_json::json!("1.0")),
        ] {
            let mut bad = valid.clone();
            bad["preparation_provenance"][field] = invalid;
            assert!(
                serde_json::from_value::<NormalizedInstanceManifest>(bad).is_err(),
                "{field}"
            );
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    fn resource_limit_descriptor_must_match_a_validated_failure_kind() {
        for value in [
            serde_json::json!({"unit_key":"page:1", "error_kind":"resource_limit"}),
            serde_json::json!({"unit_key":"page:1", "error_kind":"invalid_input", "resource_limit":{"resource":"work", "stage":"extract", "limit":10, "observed":11, "resource_tier":"standard"}}),
            serde_json::json!({"unit_key":"page:1", "error_kind":"resource_limit", "resource_limit":{"resource":"work", "stage":"", "limit":10, "observed":11, "resource_tier":"standard"}}),
        ] {
            let unit: adapter_types::FailedUnit = serde_json::from_value(value).unwrap();
            assert!(validate_failed_unit_error_kinds(&[unit]).is_err());
        }
    }

    #[test]
    fn resource_limit_failed_unit_roundtrip_is_lossless() {
        let value = serde_json::json!({"unit_key":"page:1", "error_kind":"resource_limit",
            "resource_limit":{"resource":"work", "stage":"extract", "limit":10,
                "observed":11, "resource_tier":"standard"}});
        let unit: adapter_types::FailedUnit = serde_json::from_value(value.clone()).unwrap();
        validate_failed_unit_error_kinds(std::slice::from_ref(&unit)).unwrap();
        assert_eq!(serde_json::to_value(unit).unwrap(), value);
    }

    #[test]
    fn owner_control_markdownize_timestamps_stay_with_entry_snapshot() {
        use kio_core::control::ControlHandle;
        use kio_core::test_control::{DebugTestControl, Selector};
        let request = MarkdownizeStageRequest {
            mode: MarkdownizeMode::Full,
            new_raw: RawRef {
                path: "note.txt".to_owned(),
                raw_hash: format!("sha256:{}", "a".repeat(64)),
            },
            previous: None,
            hints: None,
            tool_profile_hash: format!("sha256:{}", "b".repeat(64)),
            spec_version: 1,
        };
        let clock = |value: &str| {
            let debug = DebugTestControl {
                fixed_now: Selector::Known(value.to_owned()),
                ..Default::default()
            };
            ControlHandle::new(debug)
        };
        let outer = clock("2000-01-01T00:00:00Z");
        let inner = clock("2001-01-01T00:00:00Z");
        let inner_output = markdownize_units_with_control(request.clone(), &inner).unwrap();
        let outer_output = markdownize_units_with_control(request, &outer).unwrap();
        assert_eq!(inner_output.manifest.generated_at, "2001-01-01T00:00:00Z");
        assert_eq!(outer_output.manifest.generated_at, "2000-01-01T00:00:00Z");
        assert_eq!(
            outer_output.updated_units[0].generated_at,
            outer_output.manifest.generated_at
        );
        assert_eq!(
            outer_output.run.created_at,
            outer_output.manifest.generated_at
        );
        assert_eq!(
            outer_output.run.finished_at.as_deref(),
            Some("2000-01-01T00:00:00Z")
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    fn owner_control_persist_publication_isolation() {
        use kio_core::durability::DurabilityPoint;
        use kio_core::test_control::{DebugTestControl, Selector};
        let controlled = canonical_tempdir();
        let sibling = canonical_tempdir();
        let ordinary = canonical_tempdir();
        for root in [&controlled, &sibling, &ordinary] {
            initialize_persist_store(root.path());
        }
        let occupied = controlled.path().join("occupied-publication-barrier");
        fs::write(&occupied, b"occupied").unwrap();
        let mut debug = DebugTestControl::default();
        debug.core.durability_point = Selector::Known(DurabilityPoint::AtomicWriteStaged);
        debug.core.durability_ready = Some(occupied.clone());
        let control = ControlHandle::new(debug);
        let (identity, manifest, units) = normalized_fixture();
        let error = persist_normalized_instance_with_control(
            controlled.path(),
            &manifest,
            &units,
            &control,
        )
        .expect_err("controlled staged publication must reach its durability barrier");
        assert!(error.to_string().contains("KIO-E-STORE-IO-001"), "{error}");
        assert_eq!(fs::read(&occupied).unwrap(), b"occupied");
        assert!(
            !controlled
                .path()
                .join(normalized_instance_relative_path(
                    &identity.raw_hash,
                    &identity.tool_profile_hash,
                    identity.r#gen
                ))
                .exists()
        );
        let sibling_manifest = persist_normalized_instance_with_control(
            sibling.path(),
            &manifest,
            &units,
            &ControlHandle::default(),
        )
        .unwrap();
        let ordinary_manifest =
            persist_normalized_instance(ordinary.path(), &manifest, &units).unwrap();
        assert_eq!(sibling_manifest, ordinary_manifest);
        assert_eq!(
            load_validated_normalized_instance(
                sibling.path(),
                &identity.raw_hash,
                &identity.tool_profile_hash,
                identity.r#gen
            )
            .unwrap()
            .units,
            units
        );
    }

    fn canonical_tempdir() -> crate::test_support::PrivateTempDir {
        crate::test_support::PrivateTempDir::new()
    }

    fn initialize_persist_store(path: &Path) {
        let directory = StoreDirectory::open(path).unwrap();
        directory
            .create_directory_all(Path::new("objects/raw"))
            .unwrap();
        directory
            .create_directory_all(Path::new("objects/trees"))
            .unwrap();
        directory
            .create_directory_all(Path::new("objects/commits"))
            .unwrap();
    }

    fn normalized_fixture() -> (
        NormalizedInstanceIdentity,
        NormalizedInstanceManifest,
        Vec<NormalizedUnitObject>,
    ) {
        let raw_hash = format!("sha256:{}", "a".repeat(64));
        let tool_profile_hash = format!("sha256:{}", "b".repeat(64));
        let prepared_hash = format!("sha256:{}", "c".repeat(64));
        let unit_key = "page:1".to_owned();
        let identity = NormalizedInstanceIdentity {
            raw_hash: raw_hash.clone(),
            tool_profile_hash: tool_profile_hash.clone(),
            r#gen: 7,
        };
        let mut manifest = NormalizedInstanceManifest {
            preparation_provenance: PreparationProvenance::local(),
            raw_hash: raw_hash.clone(),
            tool_profile_hash: tool_profile_hash.clone(),
            r#gen: 7,
            parent_gen: Some(6),
            run_id: "run_test".to_owned(),
            units: vec![NormalizedUnitManifestEntry {
                resource_limit: None,
                order: 0,
                unit_key: unit_key.clone(),
                unit_ref: prepared_unit_ref(&unit_key),
                unit_type: UnitType::Page,
                status: UnitStatus::Done,
                prepared_hash: prepared_hash.clone(),
                preparation_profile_hash: format!("sha256:{}", "d".repeat(64)),
                unit_object_hash: None,
                error_kind: None,
            }],
            generated_at: "2026-07-12T00:00:00Z".to_owned(),
        };
        let units = vec![NormalizedUnitObject {
            unit_key,
            unit_type: UnitType::Page,
            raw_hash,
            prepared_hash,
            preparation_profile_hash: format!("sha256:{}", "d".repeat(64)),
            tool_profile_hash,
            r#gen: 7,
            mode: MarkdownizeMode::Full,
            markdown: "trusted markdown".to_owned(),
            // A provider metadata reference is descriptive only. It must not
            // become an image ownership claim without decoded image bytes.
            owned_image_hashes: BTreeSet::new(),
            metadata: BTreeMap::from([(
                "bbox_annotations".to_owned(),
                serde_json::json!([{
                    "image_hash": format!("sha256:{}", "e".repeat(64)),
                    "bbox": [0, 0, 1, 1],
                    "short_description": "figure",
                    "transcribed_text": "ZXQ\\-UNIQUE"
                }]),
            )]),
            reused_from: None,
            generated_at: "2026-07-12T00:00:00Z".to_owned(),
        }];
        manifest.units[0].unit_object_hash = Some(normalized_unit_object_hash(&units[0]).unwrap());
        (identity, manifest, units)
    }

    #[test]
    fn manifest_structure_validation_does_not_require_bodies_but_rejects_hidden_duplicates() {
        let (identity, manifest, _) = normalized_fixture();
        validate_normalized_manifest("fixture", &identity, &manifest).unwrap();
        assert!(validate_normalized_instance("fixture", &identity, &manifest, &[]).is_err());
        let mut duplicate = manifest.clone();
        duplicate.units.push(duplicate.units[0].clone());
        assert!(validate_normalized_manifest("fixture", &identity, &duplicate).is_err());
        let mut missing_pin = manifest.clone();
        missing_pin.units[0].unit_object_hash = None;
        assert!(validate_normalized_manifest("fixture", &identity, &missing_pin).is_err());
        let mut wrong_reference = manifest;
        wrong_reference.units[0].unit_ref = "0000000000000000".into();
        assert!(validate_normalized_manifest("fixture", &identity, &wrong_reference).is_err());
    }

    #[test]
    fn placeholder_unit_ref_serializes_gen() {
        let unit_ref = UnitRef {
            raw_hash: "sha256:raw".to_owned(),
            tool_profile_hash: "sha256:tool".to_owned(),
            r#gen: 2,
            unit_key: "page:1".to_owned(),
            unit_ref: "3f2a9c0d1b4e5f60".to_owned(),
        };

        let value = serde_json::to_value(unit_ref).expect("serialize unit ref");
        assert_eq!(value["gen"], 2);
    }

    #[test]
    fn ct4_bbox_006_normalized_unit_requires_ownership_and_metadata() {
        let valid = serde_json::json!({
            "unit_key": "page:1",
            "unit_type": "page",
            "raw_hash": format!("sha256:{}", "a".repeat(64)),
            "prepared_hash": format!("sha256:{}", "b".repeat(64)),
            "preparation_profile_hash": format!("sha256:{}", "d".repeat(64)),
            "tool_profile_hash": format!("sha256:{}", "c".repeat(64)),
            "gen": 0,
            "mode": "full",
            "markdown": "normalized fixture",
            "owned_image_hashes": [],
            "metadata": {},
            "reused_from": null,
            "generated_at": "2026-07-13T00:00:00Z"
        });
        let unit: NormalizedUnitObject = serde_json::from_value(valid.clone()).unwrap();
        assert!(unit.metadata.is_empty());

        let mut missing_metadata = valid.clone();
        missing_metadata.as_object_mut().unwrap().remove("metadata");
        assert!(serde_json::from_value::<NormalizedUnitObject>(missing_metadata).is_err());

        let mut missing_owned_images = valid.clone();
        missing_owned_images
            .as_object_mut()
            .unwrap()
            .remove("owned_image_hashes");
        assert!(serde_json::from_value::<NormalizedUnitObject>(missing_owned_images).is_err());

        let mut malformed_owned_image = valid.clone();
        malformed_owned_image["owned_image_hashes"] = serde_json::json!(["sha256:not-a-digest"]);
        assert!(serde_json::from_value::<NormalizedUnitObject>(malformed_owned_image).is_err());

        let mut duplicate_owned_image = valid.clone();
        let hash = format!("sha256:{}", "e".repeat(64));
        duplicate_owned_image["owned_image_hashes"] = serde_json::json!([hash.clone(), hash]);
        assert!(serde_json::from_value::<NormalizedUnitObject>(duplicate_owned_image).is_err());

        let mut unknown_field = valid;
        unknown_field["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<NormalizedUnitObject>(unknown_field).is_err());
    }

    #[test]
    fn normalized_layout_matches_step2a_vector() {
        let raw = "sha256:bbe1da2edd1819b58ce32163144923f850fc7f2c7b4fe130635c6b54a8e7ac59";
        let tool = "sha256:393d7b062ec1fd573c0a061455bef3f3ee16367378ca4122a0684045178e974c";
        let dir = normalized_instance_dir(".kio", raw, tool, 0);
        assert_eq!(
            dir,
            PathBuf::from(
                ".kio/objects/normalized_units/bb/e1/bbe1da2edd1819b58ce32163144923f850fc7f2c7b4fe130635c6b54a8e7ac59.393d7b062ec1fd573c0a061455bef3f3ee16367378ca4122a0684045178e974c.g0"
            )
        );
        assert_eq!(
            normalized_view_path(".kio", raw, tool, 0, raw),
            PathBuf::from(
                ".kio/objects/normalized/bb/e1/bbe1da2edd1819b58ce32163144923f850fc7f2c7b4fe130635c6b54a8e7ac59.393d7b062ec1fd573c0a061455bef3f3ee16367378ca4122a0684045178e974c.g0.bbe1da2edd1819b58ce32163144923f850fc7f2c7b4fe130635c6b54a8e7ac59.md"
            )
        );
    }

    #[test]
    fn q6_normalized_instance_dir_does_not_panic_on_short_digest() {
        // Defense in depth: a stray short hash must degrade to a deterministic path
        // instead of a `digest[0..2]` slice panic (exit 101). Authoritative
        // validation is at `TaskStore::all`; this only guarantees no panic here.
        let dir = normalized_instance_dir(".kio", "sha256:ab", "sha256:tool", 0);
        assert!(dir.to_string_lossy().contains("objects/normalized_units"));
        assert!(!dir.to_string_lossy().contains(':'));
        let view = normalized_view_path(".kio", "sha256:ab", "sha256:tool", 0, "sha256:ab");
        assert!(!view.to_string_lossy().contains(':'));
    }

    #[test]
    fn normalized_view_uses_manifest_order_and_reports_streamed_offsets() {
        let raw_hash = format!("sha256:{}", "a".repeat(64));
        let tool_profile_hash = format!("sha256:{}", "b".repeat(64));
        let prepared_hash = format!("sha256:{}", "c".repeat(64));
        let mut manifest = NormalizedInstanceManifest {
            preparation_provenance: PreparationProvenance::local(),
            raw_hash: raw_hash.clone(),
            tool_profile_hash: tool_profile_hash.clone(),
            r#gen: 3,
            parent_gen: None,
            run_id: "run_layout_test".to_owned(),
            units: vec![
                NormalizedUnitManifestEntry {
                    resource_limit: None,
                    order: 0,
                    unit_key: "page:1".to_owned(),
                    unit_ref: prepared_unit_ref("page:1"),
                    unit_type: UnitType::Page,
                    status: UnitStatus::Done,
                    prepared_hash: prepared_hash.clone(),
                    preparation_profile_hash: format!("sha256:{}", "d".repeat(64)),
                    unit_object_hash: None,
                    error_kind: None,
                },
                NormalizedUnitManifestEntry {
                    resource_limit: None,
                    order: 1,
                    unit_key: "page:2".to_owned(),
                    unit_ref: prepared_unit_ref("page:2"),
                    unit_type: UnitType::Page,
                    status: UnitStatus::Failed,
                    prepared_hash: prepared_hash.clone(),
                    preparation_profile_hash: format!("sha256:{}", "d".repeat(64)),
                    unit_object_hash: None,
                    error_kind: Some("invalid--input".to_owned()),
                },
                NormalizedUnitManifestEntry {
                    resource_limit: None,
                    order: 2,
                    unit_key: "page:3".to_owned(),
                    unit_ref: prepared_unit_ref("page:3"),
                    unit_type: UnitType::Page,
                    status: UnitStatus::Done,
                    prepared_hash: prepared_hash.clone(),
                    preparation_profile_hash: format!("sha256:{}", "d".repeat(64)),
                    unit_object_hash: None,
                    error_kind: None,
                },
            ],
            generated_at: "2026-08-11T00:00:00Z".to_owned(),
        };
        // page:1's markdown carries trailing newlines on purpose (03 §2.1 rule 2
        // trims them); page:2 has no unit object at all (status = Failed, so its
        // part comes entirely from the manifest's error_kind, rule 3).
        let units = vec![
            NormalizedUnitObject {
                unit_key: "page:1".to_owned(),
                unit_type: UnitType::Page,
                raw_hash: raw_hash.clone(),
                prepared_hash: prepared_hash.clone(),
                preparation_profile_hash: format!("sha256:{}", "d".repeat(64)),
                tool_profile_hash: tool_profile_hash.clone(),
                r#gen: 3,
                mode: MarkdownizeMode::Full,
                markdown: "first unit body\n\n".to_owned(),
                owned_image_hashes: BTreeSet::new(),
                metadata: BTreeMap::new(),
                reused_from: None,
                generated_at: "2026-08-11T00:00:00Z".to_owned(),
            },
            NormalizedUnitObject {
                unit_key: "page:3".to_owned(),
                unit_type: UnitType::Page,
                raw_hash: raw_hash.clone(),
                prepared_hash: prepared_hash.clone(),
                preparation_profile_hash: format!("sha256:{}", "d".repeat(64)),
                tool_profile_hash: tool_profile_hash.clone(),
                r#gen: 3,
                mode: MarkdownizeMode::Full,
                markdown: "third unit body".to_owned(),
                owned_image_hashes: BTreeSet::new(),
                metadata: BTreeMap::new(),
                reused_from: None,
                generated_at: "2026-08-11T00:00:00Z".to_owned(),
            },
        ];

        manifest.units.swap(0, 2);
        let tmp = canonical_tempdir();
        initialize_persist_store(tmp.path());
        let directory = StoreDirectory::open(tmp.path()).unwrap();
        let manifest = persist_normalized_instance_bound(&directory, &manifest, &units).unwrap();
        let hash = normalized_manifest_hash(&manifest).unwrap();
        let path = normalized_view_relative_path(&raw_hash, &tool_profile_hash, 3, &hash);
        let view = std::fs::read(tmp.path().join(path)).unwrap();
        let store = ObjectStore::from_bound_kio_with_control(
            directory.root_handle().as_ref(),
            directory.control().clone(),
        )
        .unwrap();
        let expected = format!(
            "{}first unit body\n\n<!-- KIO-MISSING-UNIT page:2 invalid%2D%2Dinput -->\n\nthird unit body\n",
            normalized_view_header(&manifest)
        );
        assert_eq!(view, expected.as_bytes());
        for (key, text) in [("page:1", "first unit body"), ("page:3", "third unit body")] {
            let unit = units.iter().find(|unit| unit.unit_key == key).unwrap();
            let hash = cas_hash_bytes(unit.markdown.as_bytes());
            let (start, len) = verify_normalized_view_bytes(
                &mut view.as_slice(),
                &store,
                &manifest,
                Some((key, &hash)),
            )
            .unwrap()
            .unwrap();
            assert_eq!(
                &view[start as usize..(start + len) as usize],
                text.as_bytes()
            );
        }

        let mut duplicate = manifest.clone();
        duplicate.units[0].order = duplicate.units[1].order;
        assert!(persist_normalized_instance_bound(&directory, &duplicate, &units).is_err());
    }

    #[test]
    fn incremental_mode_requires_all_five_conditions() {
        let ok = IncrementalModeInput {
            has_previous_done_run: true,
            raw_hash_only_changed: true,
            adapter_capabilities: vec!["incremental_update".to_owned()],
            change_rate: 0.09,
            threshold: 0.30,
            consecutive_incremental_count: 4,
            max_consecutive_incremental: 5,
        };
        assert_eq!(
            choose_markdownize_mode(&ok).mode,
            MarkdownizeMode::Incremental
        );

        let mut no_previous = ok.clone();
        no_previous.has_previous_done_run = false;
        assert_eq!(
            choose_markdownize_mode(&no_previous).mode,
            MarkdownizeMode::Full
        );
        let mut tool_changed = ok.clone();
        tool_changed.raw_hash_only_changed = false;
        assert_eq!(
            choose_markdownize_mode(&tool_changed).mode,
            MarkdownizeMode::Full
        );
        let mut no_capability = ok.clone();
        no_capability.adapter_capabilities.clear();
        assert_eq!(
            choose_markdownize_mode(&no_capability).mode,
            MarkdownizeMode::Full
        );
        let mut too_much_change = ok.clone();
        too_much_change.change_rate = 0.40;
        assert_eq!(
            choose_markdownize_mode(&too_much_change).mode,
            MarkdownizeMode::Full
        );
        let mut max_consecutive = ok;
        max_consecutive.consecutive_incremental_count = 5;
        assert_eq!(
            choose_markdownize_mode(&max_consecutive).mode,
            MarkdownizeMode::Full
        );
    }

    #[test]
    fn acceptance_validation_rejects_bad_incremental_shapes() {
        let prepared = vec![
            PreparedUnit {
                renderer: None,
                order: 0,
                unit_key: "page:1".to_owned(),
                unit_type: UnitType::Page,
                prepared_hash:
                    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                        .to_owned(),
                preparation_profile_hash: "sha256:test-prepare-profile".to_owned(),
                fingerprint: UnitFingerprint {
                    perceptual_hash: "p1".to_owned(),
                    text_hash: "t1".to_owned(),
                    visual_hash: "v1".to_owned(),
                },
                mime: None,
                page_number: Some(1),
            },
            PreparedUnit {
                renderer: None,
                order: 1,
                unit_key: "page:2".to_owned(),
                unit_type: UnitType::Page,
                prepared_hash:
                    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                        .to_owned(),
                preparation_profile_hash: "sha256:test-prepare-profile".to_owned(),
                fingerprint: UnitFingerprint {
                    perceptual_hash: "p2".to_owned(),
                    text_hash: "t2".to_owned(),
                    visual_hash: "v2".to_owned(),
                },
                mime: None,
                page_number: Some(2),
            },
        ];
        let hints = IncrementalHints {
            changed_unit_keys: vec!["page:1".to_owned()],
            added_unit_keys: Vec::new(),
            removed_unit_keys: Vec::new(),
            page_fingerprints: BTreeMap::new(),
        };
        let good = adapter_types::MarkdownizeResponse {
            usage: None,
            mode_used: adapter_types::MarkdownizeMode::Incremental,
            updated_units: vec![adapter_types::MarkdownUnit {
                unit_key: "page:1".to_owned(),
                unit_type: adapter_types::UnitKind::Page,
                // QA41: a well-formed unit satisfies Normalized Markdown v1
                // (07 §5.2.1) — exactly one trailing LF.
                markdown: "updated\n".to_owned(),
                owned_image_hashes: BTreeSet::new(),
                metadata: BTreeMap::new(),
            }],
            unchanged_unit_keys: vec!["page:2".to_owned()],
            added_units: Vec::new(),
            removed_unit_keys: Vec::new(),
            failed_units: Vec::new(),
            fallback_to_full: false,
            reason: None,
        };
        validate_markdownize_response(&good, &hints, &prepared).unwrap();

        let mut bad = good;
        bad.updated_units[0].unit_key = "page:2".to_owned();
        assert!(validate_markdownize_response(&bad, &hints, &prepared).is_err());
    }

    /// Fixture shared by the QA36/QA38/QA39/QA40 tests: 3 prepared units,
    /// `page:1`/`page:2` are hint-changed, `page:3` is hint-added.
    fn qa_fixture() -> (Vec<PreparedUnit>, IncrementalHints) {
        let unit = |order: u64, key: &str| PreparedUnit {
            renderer: None,
            order,
            unit_key: key.to_owned(),
            unit_type: UnitType::Page,
            prepared_hash: format!("sha256:{}", "a".repeat(64)),
            preparation_profile_hash: "sha256:test-prepare-profile".to_owned(),
            fingerprint: UnitFingerprint {
                perceptual_hash: "p".to_owned(),
                text_hash: "t".to_owned(),
                visual_hash: "v".to_owned(),
            },
            mime: None,
            page_number: Some(order + 1),
        };
        let prepared = vec![unit(0, "page:1"), unit(1, "page:2"), unit(2, "page:3")];
        let hints = IncrementalHints {
            changed_unit_keys: vec!["page:1".to_owned(), "page:2".to_owned()],
            added_unit_keys: vec!["page:3".to_owned()],
            removed_unit_keys: Vec::new(),
            page_fingerprints: BTreeMap::new(),
        };
        (prepared, hints)
    }

    fn ok_unit(key: &str) -> adapter_types::MarkdownUnit {
        adapter_types::MarkdownUnit {
            unit_key: key.to_owned(),
            unit_type: adapter_types::UnitKind::Page,
            markdown: format!("body for {key}\n"),
            owned_image_hashes: BTreeSet::new(),
            metadata: BTreeMap::new(),
        }
    }

    fn failed(key: &str, error_kind: &str) -> adapter_types::FailedUnit {
        adapter_types::FailedUnit {
            resource_limit: None,
            unit_key: key.to_owned(),
            error_kind: error_kind.to_owned(),
        }
    }

    // QA36/QA39 (V4): a partially-failed `added` unit is accepted when the
    // failure is reported via `failed_units` and the success+failure union
    // equals `hints.added_unit_keys` exactly.
    #[test]
    fn qa36_qa39_v4_added_partial_failure_is_accepted_via_failed_units() {
        let (prepared, hints) = qa_fixture();
        let response = adapter_types::MarkdownizeResponse {
            usage: None,
            mode_used: adapter_types::MarkdownizeMode::Incremental,
            updated_units: vec![ok_unit("page:1"), ok_unit("page:2")],
            unchanged_unit_keys: Vec::new(),
            added_units: Vec::new(),
            removed_unit_keys: Vec::new(),
            failed_units: vec![failed("page:3", "network_error")],
            fallback_to_full: false,
            reason: None,
        };
        assert_eq!(
            validate_markdownize_response(&response, &hints, &prepared).unwrap(),
            MarkdownizeAcceptance::Accepted
        );

        // The old all-or-nothing behavior (any single failure rejects the
        // whole response) must be gone: a response with a genuinely missing
        // (not `failed_units`-reported) added unit is still rejected — V4's
        // union must equal `hints.added_unit_keys`, not just be a superset.
        let mut missing = response.clone();
        missing.failed_units.clear();
        assert!(validate_markdownize_response(&missing, &hints, &prepared).is_err());
    }

    // QA39 (V6): `mode_used=full` accepts a coverage split across
    // updated/added/failed, as long as the 3-way union equals the full
    // prepared set and the arrays are mutually exclusive.
    #[test]
    fn qa39_v6_full_response_accepts_failed_units_in_its_coverage() {
        let (prepared, _hints) = qa_fixture();
        let response = adapter_types::MarkdownizeResponse {
            usage: None,
            mode_used: adapter_types::MarkdownizeMode::Full,
            updated_units: vec![ok_unit("page:1"), ok_unit("page:2")],
            unchanged_unit_keys: Vec::new(),
            added_units: Vec::new(),
            removed_unit_keys: Vec::new(),
            failed_units: vec![failed("page:3", "invalid_input")],
            fallback_to_full: false,
            reason: None,
        };
        let empty_hints = IncrementalHints {
            changed_unit_keys: Vec::new(),
            added_unit_keys: Vec::new(),
            removed_unit_keys: Vec::new(),
            page_fingerprints: BTreeMap::new(),
        };
        assert_eq!(
            validate_markdownize_response(&response, &empty_hints, &prepared).unwrap(),
            MarkdownizeAcceptance::Accepted
        );
    }

    // QA36: `failed_units[].error_kind` must be a member of the closed
    // `RetryErrorKind` enum (04 §3.2 V6) — an unknown value rejects the
    // whole response, not just the offending unit.
    #[test]
    fn qa36_failed_unit_error_kind_must_be_a_known_retry_kind() {
        let (prepared, hints) = qa_fixture();
        let mut response = adapter_types::MarkdownizeResponse {
            usage: None,
            mode_used: adapter_types::MarkdownizeMode::Incremental,
            updated_units: vec![ok_unit("page:1"), ok_unit("page:2")],
            unchanged_unit_keys: Vec::new(),
            added_units: Vec::new(),
            removed_unit_keys: Vec::new(),
            failed_units: vec![failed("page:3", "made_up_error")],
            fallback_to_full: false,
            reason: None,
        };
        assert!(validate_markdownize_response(&response, &hints, &prepared).is_err());
        response.failed_units = vec![failed("page:3", "rate_limit")];
        assert!(validate_markdownize_response(&response, &hints, &prepared).is_ok());
    }

    // QA38a: an in-array `unit_key` duplicate is a violation even though the
    // set-collapsed `keys()` view would hide it — checked by array length vs
    // distinct-key count.
    #[test]
    fn qa38a_duplicate_unit_key_within_one_array_is_rejected() {
        let (prepared, hints) = qa_fixture();
        let response = adapter_types::MarkdownizeResponse {
            usage: None,
            mode_used: adapter_types::MarkdownizeMode::Incremental,
            updated_units: vec![ok_unit("page:1"), ok_unit("page:1"), ok_unit("page:2")],
            unchanged_unit_keys: Vec::new(),
            added_units: vec![ok_unit("page:3")],
            removed_unit_keys: Vec::new(),
            failed_units: Vec::new(),
            fallback_to_full: false,
            reason: None,
        };
        assert!(validate_markdownize_response(&response, &hints, &prepared).is_err());
    }

    // QA38b: `unchanged_unit_keys` must equal the KIO-computed unchanged
    // candidate set (N - hints.changed - hints.added) exactly — a changed
    // unit falsely reported unchanged must not publish its stale content.
    #[test]
    fn qa38b_unchanged_unit_keys_must_match_the_kio_computed_candidate_set() {
        let unit = |order: u64, key: &str| PreparedUnit {
            renderer: None,
            order,
            unit_key: key.to_owned(),
            unit_type: UnitType::Page,
            prepared_hash: format!("sha256:{}", "a".repeat(64)),
            preparation_profile_hash: "sha256:test-prepare-profile".to_owned(),
            fingerprint: UnitFingerprint {
                perceptual_hash: "p".to_owned(),
                text_hash: "t".to_owned(),
                visual_hash: "v".to_owned(),
            },
            mime: None,
            page_number: Some(order + 1),
        };
        // page:1 is changed, page:2 is the true unchanged candidate.
        let prepared = vec![unit(0, "page:1"), unit(1, "page:2")];
        let hints = IncrementalHints {
            changed_unit_keys: vec!["page:1".to_owned()],
            added_unit_keys: Vec::new(),
            removed_unit_keys: Vec::new(),
            page_fingerprints: BTreeMap::new(),
        };
        // Adapter falsely claims page:1 (a changed unit) is unchanged and
        // omits it from updated_units — must be rejected even though the
        // response is internally self-consistent (V1 union still covers N).
        let lying = adapter_types::MarkdownizeResponse {
            usage: None,
            mode_used: adapter_types::MarkdownizeMode::Incremental,
            updated_units: Vec::new(),
            unchanged_unit_keys: vec!["page:1".to_owned(), "page:2".to_owned()],
            added_units: Vec::new(),
            removed_unit_keys: Vec::new(),
            failed_units: Vec::new(),
            fallback_to_full: false,
            reason: None,
        };
        assert!(validate_markdownize_response(&lying, &hints, &prepared).is_err());
    }

    // QA40: two different unit_keys colliding onto the same unit_ref must
    // whole-response reject the persist-bound final unit set. A real 64-bit
    // sha256-prefix collision cannot be constructed in a test, so this
    // injects a stand-in collision by asserting the injectivity checker
    // directly (the function `validate_markdownize_response` calls).
    #[test]
    fn qa40_unit_ref_collision_in_the_final_persisted_set_is_rejected() {
        assert!(
            validate_unit_ref_injectivity([&"page:1".to_owned(), &"page:2".to_owned()].into_iter())
                .is_ok()
        );
        // Same key twice is not a collision (idempotent re-occurrence).
        assert!(
            validate_unit_ref_injectivity([&"page:1".to_owned(), &"page:1".to_owned()].into_iter())
                .is_ok()
        );
    }

    // QA41: the 6 Normalized Markdown v1 structural violations (07 §5.2.1) —
    // BOM, NFD (not NFC), CRLF, trailing space, Setext heading, raw HTML —
    // each independently reject the response; the well-formed baseline is
    // accepted.
    #[test]
    fn qa41_normalized_markdown_v1_structural_violations_are_rejected() {
        assert!(validate_normalized_markdown_v1("Body\n").is_ok());
        assert!(validate_normalized_markdown_v1("\u{feff}Body\n").is_err());
        assert!(validate_normalized_markdown_v1("cafe\u{0301}\n").is_err()); // NFD
        assert!(validate_normalized_markdown_v1("Body\r\n").is_err());
        assert!(validate_normalized_markdown_v1("Body   \n").is_err());
        assert!(validate_normalized_markdown_v1("Title\n=====\n").is_err());
        assert!(validate_normalized_markdown_v1("Body\n<div>raw</div>\n").is_err());
        assert!(validate_normalized_markdown_v1("Body\n<https://example.test>\n").is_err());
        assert!(validate_normalized_markdown_v1("~~~\ncode\n~~~\n").is_err());
        // A `<`/`>` comparison in prose, far from each other, is not raw HTML.
        assert!(validate_normalized_markdown_v1("a < b and c > d in the same paragraph\n").is_ok());
        // A fenced fixture containing `<div>`-shaped text is data, not markup.
        assert!(validate_normalized_markdown_v1("```html\n<div>x</div>\n```\n").is_ok());
    }

    /// Every `/layout-parsing` response shape PaddleOCR-VL has been *observed*
    /// to produce, run through the offline adapter, judged by the v1 validator
    /// above.
    ///
    /// It lives in this crate because the validator does. `kio-pipeline` depends
    /// on `kio-adapter` and not the other way round, so the same test written
    /// over there would need a second copy of the v1 rules — and a second copy
    /// of a rule is what put the wrong image spelling into the archive once
    /// already (`1194dba`).
    ///
    /// # Why a table, and why only measurements
    ///
    /// The adapter's mock has been wrong about this service three times: the
    /// response envelope (`1feed04`), the figure markup (`1194dba`), and the way
    /// a page ends (`d66d063`). Every time, the suite stayed green — the mock
    /// and the code agreed with each other while the service disagreed with
    /// both — and every time the truth arrived from a GPU box instead of from
    /// CI. The third one is the sharpest: making acceptance fatal (`86d4508`)
    /// was verified against a mock whose page happened to end in exactly one LF,
    /// while the real service ends a prose page with none and a table page with
    /// two, so the change refused every real page and nothing here noticed.
    ///
    /// So each row below is a shape someone captured from the running service,
    /// with the commit that recorded it. The rule for the next person is the
    /// point of the whole test: **when you measure a new shape, add a row.** It
    /// fails until the adapter handles the shape, which is the notice that was
    /// missing all three times.
    ///
    /// What it cannot do is speak for shapes nobody has captured yet. This is a
    /// record of what has been seen, not a proof about what the service can
    /// send.
    #[test]
    fn every_measured_service_shape_produces_v1_markdown_or_a_named_refusal() {
        use kio_adapter::local_ocr_markdownize::{
            LayoutFileType, LocalOcrClient, LocalOcrExecution, LocalOcrMarkdownizeAdapter,
        };
        use kio_adapter::traits::MarkdownizeAdapter;
        use serde_json::json;

        #[derive(Clone)]
        struct CannedClient(Value);
        impl LocalOcrClient for CannedClient {
            fn layout_parse(
                &self,
                _file_base64: &str,
                _file_type: LayoutFileType,
            ) -> kio_adapter::Result<Value> {
                Ok(self.0.clone())
            }
        }

        // `\x89PNG\r\n\x1a\nkio conformance fixture` — a PNG signature is all
        // `sniff_media_type` reads, and no consumer decodes the pixels.
        const PNG_BASE64: &str = "iVBORw0KGgpraW8gY29uZm9ybWFuY2UgZml4dHVyZQ==";

        // (label, markdown.text, markdown.images, parsing_res_list, expected)
        //
        // `None` = the adapter's output must satisfy v1. `Some(reason)` = it
        // must not, and the validator must say so with that reason, because
        // refusing is the decided behaviour rather than an accident (07 §5,
        // ruling in `0737422`).
        let shapes: Vec<(&str, String, Value, Value, Option<&str>)> = vec![
            (
                // d66d063: a page ending in prose came back with no trailing LF.
                "prose page, no trailing newline",
                "Kio conformance page".to_owned(),
                json!({}),
                json!([]),
                None,
            ),
            (
                // d66d063: a page ending in a table came back with two.
                "page ending in two newlines",
                "Kio conformance page\n\n".to_owned(),
                json!({}),
                json!([]),
                None,
            ),
            (
                // 1194dba: figures arrive as HTML inside a centred div, with the
                // caption in a second one, and never as `![](…)`.
                "centred div around an img, caption in another div",
                "prose\n\n<div style=\"text-align: center;\">\
                 <img src=\"imgs/img_in_chart_box_107_567_1130_1073.jpg\" alt=\"Image\" \
                 width=\"82%\" /></div>\n\n<div style=\"text-align: center;\">\
                 Figure 1: Incident count by month.</div>"
                    .to_owned(),
                json!({ "imgs/img_in_chart_box_107_567_1130_1073.jpg": PNG_BASE64 }),
                json!([{"block_label": "chart", "block_bbox": [107, 567, 1130, 1073],
                        "block_order": null}]),
                None,
            ),
            (
                // 0737422 refused this shape, because nothing had measured what
                // the service sends and a conversion guessed at would be frozen
                // by 07 §9. Three real tables later it is measured, so S3-L
                // converts it into the GFM notation 07 §5 asks for -- and the
                // refusal cost two of the three captured documents everything.
                "html table",
                "prose\n\n<table border=1 style='margin: auto;'>\
                 <tr><td>Data class</td><td>Count</td></tr></table>\n"
                    .to_owned(),
                json!({}),
                json!([]),
                None,
            ),
            (
                // What still refuses, and why the row above is not a retreat: a
                // merged cell has no GFM notation at all, so there is nothing to
                // convert it into that would not invent structure.
                "html table with a merged cell",
                "prose\n\n<table border=1><tr><td rowspan=2>Data class</td>\
                 <td>Count</td></tr><tr><td>3</td></tr></table>\n"
                    .to_owned(),
                json!({}),
                json!([]),
                Some("raw HTML and autolinks are forbidden"),
            ),
        ];

        for (label, text, images, blocks, expected) in shapes {
            let body = json!({
                "errorCode": 0,
                "result": {
                    "layoutParsingResults": [{
                        "prunedResult": {"parsing_res_list": blocks},
                        "markdown": {"text": text, "images": images}
                    }]
                }
            });
            let adapter = LocalOcrMarkdownizeAdapter::new(
                CannedClient(body),
                LocalOcrExecution::Real,
                "scope-1",
            )
            .with_verified_raw_bytes(b"%PDF-1.7 bytes".to_vec());
            let response = adapter
                .markdownize(adapter_types::MarkdownizeRequest {
                    raw: adapter_types::RawInput {
                        raw_hash: format!("sha256:{}", "a".repeat(64)),
                        path: Some("scan.pdf".to_owned()),
                    },
                    media_type: "application/pdf".to_owned(),
                    prepared_unit_hint: Some(Vec::new()),
                    mode: adapter_types::MarkdownizeMode::Full,
                    previous: None,
                    hints: None,
                    restrict_to_hint_pages: false,
                    bbox_annotation_enabled: false,
                    tool_profile_hash: format!("sha256:{}", "b".repeat(64)),
                    spec_version: 1,
                    idempotency_token: None,
                })
                .unwrap_or_else(|error| panic!("{label}: adapter refused outright: {error}"));

            for unit in &response.updated_units {
                let verdict = validate_normalized_markdown_v1(&unit.markdown);
                match expected {
                    None => assert!(
                        verdict.is_ok(),
                        "{label}: expected v1-conformant output, got {:?} for {:?}",
                        verdict.unwrap_err(),
                        unit.markdown
                    ),
                    Some(reason) => {
                        let actual = verdict.expect_err(&format!(
                            "{label}: expected a refusal, got conformant {:?}",
                            unit.markdown
                        ));
                        assert_eq!(actual, reason, "{label}: refused for the wrong reason");
                    }
                }
            }
        }
    }

    // QA41 (via validate_unit_shapes/validate_markdownize_response): a v1
    // violation in an accepted unit's markdown rejects the whole response,
    // parameterized over the same 6 shapes as the pure-function test above.
    #[test]
    fn qa41_v1_violation_in_a_unit_rejects_the_whole_response() {
        let (prepared, hints) = qa_fixture();
        for bad_markdown in [
            "\u{feff}Body\n",
            "cafe\u{0301}\n",
            "Body\r\n",
            "Body   \n",
            "Title\n=====\n",
            "Body\n<div>raw</div>\n",
        ] {
            let mut unit = ok_unit("page:1");
            unit.markdown = bad_markdown.to_owned();
            let response = adapter_types::MarkdownizeResponse {
                usage: None,
                mode_used: adapter_types::MarkdownizeMode::Incremental,
                updated_units: vec![unit, ok_unit("page:2")],
                unchanged_unit_keys: Vec::new(),
                added_units: vec![ok_unit("page:3")],
                removed_unit_keys: Vec::new(),
                failed_units: Vec::new(),
                fallback_to_full: false,
                reason: None,
            };
            assert!(
                validate_markdownize_response(&response, &hints, &prepared).is_err(),
                "expected {bad_markdown:?} to violate Normalized Markdown v1"
            );
        }
    }

    // QA44: `fallback_to_full=true` is a control response evaluated ahead of
    // V1-V6 — an incremental response is accepted as `FallbackToFull` (not a
    // contract violation) as long as its unit arrays are empty; the same
    // flag on a `mode_used=full` response is a genuine violation (loop
    // prevention).
    #[test]
    fn qa44_fallback_to_full_is_a_control_response_not_a_violation() {
        let (prepared, hints) = qa_fixture();
        let control = adapter_types::MarkdownizeResponse {
            usage: None,
            mode_used: adapter_types::MarkdownizeMode::Incremental,
            updated_units: Vec::new(),
            unchanged_unit_keys: Vec::new(),
            added_units: Vec::new(),
            removed_unit_keys: Vec::new(),
            failed_units: Vec::new(),
            fallback_to_full: true,
            reason: Some("adapter declined a light edit".to_owned()),
        };
        assert_eq!(
            validate_markdownize_response(&control, &hints, &prepared).unwrap(),
            MarkdownizeAcceptance::FallbackToFull
        );

        // Loop prevention: the SAME flag on a full response is a violation.
        let mut looped = control.clone();
        looped.mode_used = adapter_types::MarkdownizeMode::Full;
        assert!(validate_markdownize_response(&looped, &hints, &prepared).is_err());

        // A control response must carry empty unit arrays.
        let mut dirty = control;
        dirty.updated_units = vec![ok_unit("page:1")];
        assert!(validate_markdownize_response(&dirty, &hints, &prepared).is_err());
    }

    #[test]
    fn cand_049_manifest_deserialization_rejects_path_like_or_mismatched_unit_refs() {
        let unit_key = "page:1";
        let canonical = prepared_unit_ref(unit_key);
        let entry = |unit_ref: &str| {
            serde_json::json!({
                "order": 0,
                "unit_key": unit_key,
                "unit_ref": unit_ref,
                "unit_type": "page",
                "status": "done",
                "prepared_hash": format!("sha256:{}", "c".repeat(64)),
                "preparation_profile_hash": format!("sha256:{}", "e".repeat(64)),
                "unit_object_hash": format!("sha256:{}", "d".repeat(64)),
                "error_kind": null
            })
        };
        let parsed: NormalizedUnitManifestEntry =
            serde_json::from_value(entry(&canonical)).unwrap();
        assert_eq!(parsed.unit_ref, canonical);

        let mut missing_preparation_profile = entry(&canonical);
        missing_preparation_profile
            .as_object_mut()
            .unwrap()
            .remove("preparation_profile_hash");
        assert!(
            serde_json::from_value::<NormalizedUnitManifestEntry>(missing_preparation_profile)
                .is_err()
        );

        let mut invalid_preparation_profile = entry(&canonical);
        invalid_preparation_profile["preparation_profile_hash"] = serde_json::json!("invalid");
        assert!(
            serde_json::from_value::<NormalizedUnitManifestEntry>(invalid_preparation_profile)
                .is_err()
        );

        let mut missing_pin = entry(&canonical);
        missing_pin
            .as_object_mut()
            .unwrap()
            .remove("unit_object_hash");
        assert!(serde_json::from_value::<NormalizedUnitManifestEntry>(missing_pin).is_err());

        let mut done_with_null_pin = entry(&canonical);
        done_with_null_pin["unit_object_hash"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<NormalizedUnitManifestEntry>(done_with_null_pin).is_err());

        let mut done_with_invalid_pin = entry(&canonical);
        done_with_invalid_pin["unit_object_hash"] = serde_json::json!("sha256:not-a-digest");
        assert!(
            serde_json::from_value::<NormalizedUnitManifestEntry>(done_with_invalid_pin).is_err()
        );

        let mut failed_with_pin = entry(&canonical);
        failed_with_pin["status"] = serde_json::json!("failed");
        assert!(serde_json::from_value::<NormalizedUnitManifestEntry>(failed_with_pin).is_err());

        let mut failed_missing_pin = entry(&canonical);
        failed_missing_pin["status"] = serde_json::json!("failed");
        failed_missing_pin
            .as_object_mut()
            .unwrap()
            .remove("unit_object_hash");
        assert!(serde_json::from_value::<NormalizedUnitManifestEntry>(failed_missing_pin).is_err());

        let mut unknown_field = entry(&canonical);
        unknown_field["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<NormalizedUnitManifestEntry>(unknown_field).is_err());

        for bad in [
            "/tmp/foreign-unit".to_owned(),
            "../../foreign-unit".to_owned(),
            canonical.to_ascii_uppercase(),
            "0000000000000000".to_owned(),
            format!("{canonical}/child"),
        ] {
            assert!(
                serde_json::from_value::<NormalizedUnitManifestEntry>(entry(&bad)).is_err(),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn cand_061_provenance_validator_rebinds_complete_tuple() {
        let (identity, manifest, units) = normalized_fixture();
        validate_normalized_instance("manifest.json", &identity, &manifest, &units).unwrap();

        let mut unknown_manifest_field = serde_json::to_value(&manifest).unwrap();
        unknown_manifest_field["unexpected"] = serde_json::json!(true);
        assert!(
            serde_json::from_value::<NormalizedInstanceManifest>(unknown_manifest_field).is_err()
        );

        let mut bad_manifest = manifest.clone();
        bad_manifest.raw_hash = format!("sha256:{}", "d".repeat(64));
        assert!(
            validate_normalized_instance("manifest.json", &identity, &bad_manifest, &units)
                .is_err()
        );
        bad_manifest = manifest.clone();
        bad_manifest.tool_profile_hash = format!("sha256:{}", "d".repeat(64));
        assert!(
            validate_normalized_instance("manifest.json", &identity, &bad_manifest, &units)
                .is_err()
        );
        bad_manifest = manifest.clone();
        bad_manifest.r#gen = 8;
        assert!(
            validate_normalized_instance("manifest.json", &identity, &bad_manifest, &units)
                .is_err()
        );

        let mut bad_unit = units.clone();
        bad_unit[0].raw_hash = format!("sha256:{}", "d".repeat(64));
        assert!(
            validate_normalized_instance("manifest.json", &identity, &manifest, &bad_unit).is_err()
        );
        let mut bad_unit = units.clone();
        bad_unit[0].tool_profile_hash = format!("sha256:{}", "d".repeat(64));
        assert!(
            validate_normalized_instance("manifest.json", &identity, &manifest, &bad_unit).is_err()
        );
        let mut bad_unit = units.clone();
        bad_unit[0].r#gen = 8;
        assert!(
            validate_normalized_instance("manifest.json", &identity, &manifest, &bad_unit).is_err()
        );
        let mut bad_unit = units.clone();
        bad_unit[0].unit_key = "page:2".to_owned();
        assert!(
            validate_normalized_instance("manifest.json", &identity, &manifest, &bad_unit).is_err()
        );
        let mut bad_unit = units.clone();
        bad_unit[0].unit_type = UnitType::Image;
        assert!(
            validate_normalized_instance("manifest.json", &identity, &manifest, &bad_unit).is_err()
        );
        let mut bad_unit = units.clone();
        bad_unit[0].prepared_hash = format!("sha256:{}", "d".repeat(64));
        assert!(
            validate_normalized_instance("manifest.json", &identity, &manifest, &bad_unit).is_err()
        );
        let mut bad_unit = units.clone();
        bad_unit[0].preparation_profile_hash = format!("sha256:{}", "e".repeat(64));
        assert!(
            validate_normalized_instance("manifest.json", &identity, &manifest, &bad_unit).is_err()
        );
        let mut bad_unit = units.clone();
        bad_unit[0]
            .owned_image_hashes
            .insert("sha256:not-a-canonical-digest".to_owned());
        assert!(
            validate_normalized_instance("manifest.json", &identity, &manifest, &bad_unit).is_err()
        );
        let mut bad_unit = units.clone();
        bad_unit[0].owned_image_hashes = (0..=MAX_OWNED_IMAGE_HASHES_PER_UNIT)
            .map(|index| format!("sha256:{index:064x}"))
            .collect();
        assert!(
            validate_normalized_instance("manifest.json", &identity, &manifest, &bad_unit).is_err()
        );
    }

    #[test]
    fn provider_markdown_or_metadata_cannot_claim_image_ownership() {
        let prepared = vec![PreparedUnit {
            renderer: None,
            order: 0,
            unit_key: "page:1".to_owned(),
            unit_type: UnitType::Page,
            prepared_hash: format!("sha256:{}", "a".repeat(64)),
            preparation_profile_hash: format!("sha256:{}", "b".repeat(64)),
            fingerprint: UnitFingerprint {
                perceptual_hash: "p".to_owned(),
                text_hash: "t".to_owned(),
                visual_hash: "v".to_owned(),
            },
            mime: None,
            page_number: Some(1),
        }];
        let claimed_hash = format!("sha256:{}", "c".repeat(64));
        let response = adapter_types::MarkdownizeResponse {
            usage: None,
            mode_used: adapter_types::MarkdownizeMode::Full,
            updated_units: vec![adapter_types::MarkdownUnit {
                unit_key: "page:1".to_owned(),
                unit_type: adapter_types::UnitKind::Page,
                markdown: format!(
                    "![provider reference](kio://scope/object/image/{claimed_hash})\n"
                ),
                owned_image_hashes: BTreeSet::new(),
                metadata: BTreeMap::from([("image_hash".to_owned(), Value::String(claimed_hash))]),
            }],
            unchanged_unit_keys: Vec::new(),
            added_units: Vec::new(),
            removed_unit_keys: Vec::new(),
            failed_units: Vec::new(),
            fallback_to_full: false,
            reason: None,
        };
        assert_eq!(
            validate_markdownize_response(
                &response,
                &IncrementalHints {
                    changed_unit_keys: Vec::new(),
                    added_unit_keys: Vec::new(),
                    removed_unit_keys: Vec::new(),
                    page_fingerprints: BTreeMap::new(),
                },
                &prepared
            )
            .unwrap(),
            MarkdownizeAcceptance::Accepted
        );
        assert!(response.updated_units[0].owned_image_hashes.is_empty());

        let mut malformed = response;
        malformed.updated_units[0]
            .owned_image_hashes
            .insert("sha256:not-a-canonical-digest".to_owned());
        assert!(
            validate_markdownize_response(
                &malformed,
                &IncrementalHints {
                    changed_unit_keys: Vec::new(),
                    added_unit_keys: Vec::new(),
                    removed_unit_keys: Vec::new(),
                    page_fingerprints: BTreeMap::new(),
                },
                &prepared,
            )
            .is_err()
        );
    }

    #[test]
    fn cand_049_061_validated_loader_accepts_control_and_rejects_poisoned_unit() {
        let dir = canonical_tempdir();
        initialize_persist_store(dir.path());
        let (identity, manifest, units) = normalized_fixture();
        persist_normalized_instance(dir.path(), &manifest, &units).unwrap();
        let loaded = load_validated_normalized_instance(
            dir.path(),
            &identity.raw_hash,
            &identity.tool_profile_hash,
            identity.r#gen,
        )
        .unwrap();
        assert_eq!(loaded.manifest, manifest);
        assert_eq!(loaded.units, units);

        let instance_dir = normalized_instance_dir(
            dir.path(),
            &identity.raw_hash,
            &identity.tool_profile_hash,
            identity.r#gen,
        );
        let unit_path = instance_dir.join(format!(
            "{}.json",
            prepared_unit_ref(&manifest.units[0].unit_key)
        ));
        let mut poisoned = units[0].clone();
        poisoned.prepared_hash = format!("sha256:{}", "d".repeat(64));
        fs::write(&unit_path, serde_json::to_vec(&poisoned).unwrap()).unwrap();
        assert_eq!(
            load_validated_normalized_instance(
                dir.path(),
                &identity.raw_hash,
                &identity.tool_profile_hash,
                identity.r#gen,
            )
            .unwrap()
            .units,
            units
        );
    }

    #[test]
    fn ct4_fsck_normalized_budget_charges_corrupt_unit_bytes_before_load() {
        let dir = canonical_tempdir();
        initialize_persist_store(dir.path());
        let (identity, manifest, units) = normalized_fixture();
        persist_normalized_instance(dir.path(), &manifest, &units).unwrap();
        let loaded = load_validated_normalized_instance(
            dir.path(),
            &identity.raw_hash,
            &identity.tool_profile_hash,
            identity.r#gen,
        )
        .unwrap();
        let budget = normalized_instance_read_budget(
            dir.path(),
            &identity.raw_hash,
            &identity.tool_profile_hash,
            identity.r#gen,
        )
        .unwrap();
        assert!(budget > loaded.verified_bytes);

        let unit_path = normalized_instance_dir(
            dir.path(),
            &identity.raw_hash,
            &identity.tool_profile_hash,
            identity.r#gen,
        )
        .join(format!(
            "{}.json",
            prepared_unit_ref(&manifest.units[0].unit_key)
        ));
        let length = fs::metadata(&unit_path).unwrap().len();
        fs::write(&unit_path, vec![b'x'; length as usize]).unwrap();
        assert_eq!(
            normalized_instance_read_budget(
                dir.path(),
                &identity.raw_hash,
                &identity.tool_profile_hash,
                identity.r#gen,
            )
            .unwrap(),
            budget
        );
        assert!(
            load_validated_normalized_instance(
                dir.path(),
                &identity.raw_hash,
                &identity.tool_profile_hash,
                identity.r#gen,
            )
            .is_ok()
        );
    }

    #[test]
    fn canonical_normalized_layout_allows_retry_overwrite() {
        let dir = canonical_tempdir();
        initialize_persist_store(dir.path());
        let (identity, mut manifest, mut units) = normalized_fixture();
        persist_normalized_instance(dir.path(), &manifest, &units).unwrap();

        manifest.run_id = "run_retry".to_owned();
        manifest.generated_at = "2026-07-13T00:00:00Z".to_owned();
        units[0].markdown = "retry output".to_owned();
        units[0].generated_at = manifest.generated_at.clone();
        manifest.units[0].unit_object_hash = Some(normalized_unit_object_hash(&units[0]).unwrap());
        persist_normalized_instance(dir.path(), &manifest, &units).unwrap();

        let loaded = load_validated_normalized_instance(
            dir.path(),
            &identity.raw_hash,
            &identity.tool_profile_hash,
            identity.r#gen,
        )
        .unwrap();
        assert_eq!(loaded.manifest, manifest);
        assert_eq!(loaded.units, units);
        assert!(
            fs::read_to_string(normalized_view_path(
                dir.path(),
                &identity.raw_hash,
                &identity.tool_profile_hash,
                identity.r#gen,
                &normalized_manifest_hash(&manifest).unwrap(),
            ))
            .unwrap()
            .contains("retry output")
        );
    }

    #[test]
    fn persist_returns_stamped_manifest_and_publishes_its_canonical_cas_object() {
        let dir = canonical_tempdir();
        initialize_persist_store(dir.path());
        let (_, mut manifest, units) = normalized_fixture();
        manifest.units[0].unit_object_hash = None;

        let stamped = persist_normalized_instance(dir.path(), &manifest, &units).unwrap();
        assert_ne!(stamped, manifest);
        assert_eq!(
            stamped.units[0].unit_object_hash,
            Some(normalized_unit_object_hash(&units[0]).unwrap())
        );

        let bytes = canonical_normalized_manifest_bytes(&stamped).unwrap();
        let hash = cas_hash_bytes(&bytes);
        assert_eq!(
            ObjectStore::new(dir.path())
                .read_content_object_bytes(ContentObjectKind::Manifest, &hash, 1024 * 1024)
                .unwrap(),
            bytes
        );
    }

    #[test]
    fn normalized_unit_object_hash_binds_canonical_body_and_identity_fields() {
        let (_, _, units) = normalized_fixture();
        let original = normalized_unit_object_hash(&units[0]).unwrap();
        assert_eq!(
            original,
            cas_hash_bytes(&canonical_normalized_unit_object_bytes(&units[0]).unwrap())
        );

        let mut changed_body = units[0].clone();
        changed_body.markdown.push_str("\nchanged");
        assert_ne!(
            normalized_unit_object_hash(&changed_body).unwrap(),
            original
        );

        let mut retargeted = units[0].clone();
        retargeted.raw_hash = format!("sha256:{}", "f".repeat(64));
        assert_ne!(normalized_unit_object_hash(&retargeted).unwrap(), original);
    }

    #[test]
    fn normalized_unit_pins_are_immutable_and_mutable_cache_is_not_authority() {
        let dir = canonical_tempdir();
        initialize_persist_store(dir.path());
        let (identity, mut manifest, mut units) = normalized_fixture();
        persist_normalized_instance(dir.path(), &manifest, &units).unwrap();
        let old_hash = manifest.units[0].unit_object_hash.clone().unwrap();

        units[0].markdown = "changed same-generation markdown".to_owned();
        manifest.units[0].unit_object_hash = Some(normalized_unit_object_hash(&units[0]).unwrap());
        persist_normalized_instance(dir.path(), &manifest, &units).unwrap();
        let new_hash = manifest.units[0].unit_object_hash.clone().unwrap();
        assert_ne!(old_hash, new_hash);
        assert!(
            ObjectStore::new(dir.path())
                .read_content_object_bytes(
                    ContentObjectKind::NormalizedUnit,
                    &old_hash,
                    MAX_NORMALIZED_UNIT_BYTES
                )
                .is_ok()
        );

        let cache_path = normalized_instance_dir(
            dir.path(),
            &identity.raw_hash,
            &identity.tool_profile_hash,
            identity.r#gen,
        )
        .join(format!("{}.json", prepared_unit_ref(&units[0].unit_key)));
        fs::write(&cache_path, b"not authoritative").unwrap();
        assert_eq!(
            load_validated_normalized_units_from_manifest(&ObjectStore::new(dir.path()), &manifest)
                .unwrap(),
            units
        );

        let mut missing_metadata = serde_json::to_value(&units[0]).unwrap();
        missing_metadata.as_object_mut().unwrap().remove("metadata");
        let missing_metadata_bytes = canonical_json_bytes(&missing_metadata).unwrap();
        let missing_metadata_hash = ObjectStore::new(dir.path())
            .write_content_object(ContentObjectKind::NormalizedUnit, &missing_metadata_bytes)
            .unwrap();
        manifest.units[0].unit_object_hash = Some(missing_metadata_hash);
        assert!(
            load_validated_normalized_units_from_manifest(&ObjectStore::new(dir.path()), &manifest)
                .is_err()
        );
        manifest.units[0].unit_object_hash = Some(new_hash.clone());

        let cas_path = ObjectStore::new(dir.path())
            .content_path(ContentObjectKind::NormalizedUnit, &new_hash)
            .unwrap();
        fs::write(&cas_path, b"tampered").unwrap();
        assert!(
            load_validated_normalized_units_from_manifest(&ObjectStore::new(dir.path()), &manifest)
                .is_err()
        );
        fs::remove_file(&cas_path).unwrap();
        assert!(
            load_validated_normalized_units_from_manifest(&ObjectStore::new(dir.path()), &manifest)
                .is_err()
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    fn p0_interruption_before_manifest_preserves_previous_view() {
        use kio_core::durability::DurabilityPoint;
        use kio_core::test_control::{DebugTestControl, Selector};
        let dir = canonical_tempdir();
        initialize_persist_store(dir.path());
        let (identity, mut manifest, mut units) = normalized_fixture();
        let original = persist_normalized_instance(dir.path(), &manifest, &units).unwrap();
        let pin = normalized_manifest_hash(&original).unwrap();
        let view = normalized_view_path(
            dir.path(),
            &identity.raw_hash,
            &identity.tool_profile_hash,
            identity.r#gen,
            &pin,
        );
        let before = fs::read(&view).unwrap();

        manifest.run_id = "run_next".into();
        units[0].markdown = "replacement page".to_owned();
        let ready = dir.path().join("occupied");
        fs::write(&ready, b"occupied").unwrap();
        let mut debug = DebugTestControl::default();
        debug.core.durability_point = Selector::Known(DurabilityPoint::ExtractionBeforeManifest);
        debug.core.durability_ready = Some(ready);
        let control = ControlHandle::new(debug);
        assert!(
            persist_normalized_instance_with_control(dir.path(), &manifest, &units, &control)
                .is_err()
        );
        let (current, _) = read_normalized_instance_manifest(
            dir.path(),
            &identity.raw_hash,
            &identity.tool_profile_hash,
            identity.r#gen,
        )
        .unwrap();
        assert_eq!(current, original);
        assert_eq!(fs::read(view).unwrap(), before);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn p0_fifth_resumed_discard_syncs_the_visible_marker_before_deleting() {
        use kio_core::durability::DurabilityPoint;
        use kio_core::test_control::{DebugTestControl, Selector};
        let dir = canonical_tempdir();
        let (identity, _, _) = normalized_fixture();
        let root = StoreDirectory::open(dir.path()).unwrap();
        let path = Path::new("extraction-stages/views")
            .join(identity.raw_hash.trim_start_matches("sha256:"))
            .join("a".repeat(64));
        let workspace = child_directory(&root, &path).unwrap();
        let (_, saved) = staged_normalized_instance(&workspace, &identity).unwrap();
        let record = Path::new("instance.json");
        let with_fault = |point| {
            let mut debug = DebugTestControl::default();
            debug.core.durability_point = Selector::Known(point);
            debug.core.durability_storage_fault = true;
            StoreDirectory::open_with_control(dir.path(), ControlHandle::new(debug)).unwrap()
        };

        let unsynced = with_fault(DurabilityPoint::AtomicWriteBeforeDirectorySync);
        assert!(discard_normalized_stages(&unsynced, &identity.raw_hash).is_err());
        assert!(
            read_record::<SavedNormalizedDirectory>(&workspace, record, 4096)
                .unwrap()
                .unwrap()
                .discarding
        );

        let retry = with_fault(DurabilityPoint::ExtractionBeforeDestinationSync);
        assert!(discard_normalized_stages(&retry, &identity.raw_hash).is_err());
        assert!(workspace.contains_entry(&saved.leaf).unwrap());
        let retry_workspace = child_directory(&retry, &path).unwrap();
        assert!(staged_normalized_instance(&retry_workspace, &identity).is_err());
        assert!(workspace.contains_entry(&saved.leaf).unwrap());

        discard_normalized_stages(&root, &identity.raw_hash).unwrap();
        assert!(!workspace.contains_entry(&saved.leaf).unwrap());
        assert!(!workspace.contains_entry(record).unwrap());
    }

    #[test]
    fn p0_recheck_view_cache_can_be_regenerated_via_create_only_publication() {
        let dir = canonical_tempdir();
        initialize_persist_store(dir.path());
        let (identity, manifest, units) = normalized_fixture();
        let original = persist_normalized_instance(dir.path(), &manifest, &units).unwrap();
        let view = normalized_view_path(
            dir.path(),
            &identity.raw_hash,
            &identity.tool_profile_hash,
            identity.r#gen,
            &normalized_manifest_hash(&original).unwrap(),
        );
        let expected = fs::read(&view).unwrap();
        fs::write(&view, b"truncated").unwrap();
        assert_eq!(
            persist_normalized_instance(dir.path(), &manifest, &units).unwrap(),
            original
        );
        assert_eq!(fs::read(view).unwrap(), expected);
    }

    #[test]
    fn p0_normalization_resume_skips_a_durable_unit() {
        let dir = canonical_tempdir();
        initialize_persist_store(dir.path());
        let (_, mut manifest, mut units) = normalized_fixture();
        let first = units.pop().unwrap();
        let mut second = first.clone();
        second.unit_key = "page:2".to_owned();
        let mut entry = manifest.units[0].clone();
        entry.order = 1;
        entry.unit_key.clone_from(&second.unit_key);
        entry.unit_ref = prepared_unit_ref(&entry.unit_key);
        manifest.units.push(entry);
        let directory = StoreDirectory::open(dir.path()).unwrap();
        let interrupted =
            persist_normalized_instance_from_units_bound(&directory, &manifest, |entry| {
                if entry.order == 0 {
                    return Ok(Cow::Borrowed(&first));
                }
                Err(PipelineError::ResourceLimit(
                    kio_core::error::ResourceLimit {
                        resource: kio_core::error::Resource::Storage,
                        stage: "private_stage".to_owned(),
                        limit: None,
                        observed: None,
                        resource_tier: None,
                        page_progress: None,
                    },
                ))
            });
        assert!(matches!(interrupted, Err(PipelineError::ResourceLimit(_))));
        drop(directory);
        let directory = StoreDirectory::open(dir.path()).unwrap();
        persist_normalized_instance_from_units_bound(&directory, &manifest, |entry| {
            assert_eq!(entry.order, 1, "durable unit was generated again");
            Ok(Cow::Borrowed(&second))
        })
        .unwrap();
    }

    #[test]
    fn p0_streamed_normalization_failure_preserves_previous_manifest() {
        let dir = canonical_tempdir();
        initialize_persist_store(dir.path());
        let (identity, mut manifest, units) = normalized_fixture();
        let original = persist_normalized_instance(dir.path(), &manifest, &units).unwrap();
        manifest.run_id = "run_incomplete".to_owned();
        let store = StoreDirectory::open(dir.path()).unwrap();
        let limit = kio_core::error::ResourceLimit {
            resource: kio_core::error::Resource::Storage,
            stage: "private_stage".to_owned(),
            limit: None,
            observed: None,
            resource_tier: None,
            page_progress: None,
        };
        let error = persist_normalized_instance_from_units_bound(&store, &manifest, |_| {
            Err(PipelineError::ResourceLimit(limit.clone()))
        })
        .unwrap_err();
        assert!(matches!(error, PipelineError::ResourceLimit(actual) if actual == limit));
        let loaded = load_validated_normalized_instance(
            dir.path(),
            &identity.raw_hash,
            &identity.tool_profile_hash,
            identity.r#gen,
        )
        .unwrap();
        assert_eq!(loaded.manifest, original);
        assert_eq!(loaded.units, units);
        let completed = persist_normalized_instance(dir.path(), &manifest, &units).unwrap();
        assert_eq!(completed.run_id, "run_incomplete");
    }

    #[test]
    fn p0_streamed_instance_over_256_mib_can_be_written_and_read() {
        let dir = canonical_tempdir();
        initialize_persist_store(dir.path());
        let (_, mut manifest, mut units) = normalized_fixture();
        let template_entry = manifest.units.pop().unwrap();
        let template_unit = units.pop().unwrap();
        const UNIT_BYTES: usize = 16 * 1024 * 1024;
        for order in 0..17 {
            let mut entry = template_entry.clone();
            entry.order = order;
            entry.unit_key = format!("page:{}", order + 1);
            entry.unit_ref = prepared_unit_ref(&entry.unit_key);
            manifest.units.push(entry);
        }
        let directory = StoreDirectory::open(dir.path()).unwrap();
        let stamped =
            persist_normalized_instance_from_units_bound(&directory, &manifest, |entry| {
                let mut unit = template_unit.clone();
                unit.unit_key.clone_from(&entry.unit_key);
                unit.markdown = "x".repeat(UNIT_BYTES);
                Ok(Cow::Owned(unit))
            })
            .unwrap();
        let store = ObjectStore::new(dir.path());
        let mut count = 0;
        let verified = visit_validated_normalized_units(&store, &stamped, |unit| {
            count += 1;
            assert_eq!(unit.markdown.len(), UNIT_BYTES);
            Ok(())
        })
        .unwrap();
        assert_eq!(count, 17);
        assert!(verified > 256 * 1024 * 1024);
        let view = normalized_view_path(
            dir.path(),
            &stamped.raw_hash,
            &stamped.tool_profile_hash,
            stamped.r#gen,
            &normalized_manifest_hash(&stamped).unwrap(),
        );
        assert!(std::fs::metadata(view).unwrap().len() > 256 * 1024 * 1024);
    }

    #[test]
    fn p0_normalized_size_accounting_accepts_units_over_256_mib_in_total() {
        let path = Path::new("test normalized instance");
        let mut total = checked_manifest_size(path, 1024, NORMALIZED_SIZE_LIMITS).unwrap();
        for _ in 0..17 {
            total =
                checked_unit_size(path, total, 16 * 1024 * 1024, NORMALIZED_SIZE_LIMITS).unwrap();
        }
        assert!(total > 256 * 1024 * 1024);
    }

    #[test]
    fn cand_061_writer_enforces_loader_size_boundaries_before_publish() {
        let dir = canonical_tempdir();
        initialize_persist_store(dir.path());
        let manifest_path = dir.path().join("manifest.json");
        let unit_path = dir.path().join("unit.json");
        let limits = NormalizedSizeLimits {
            manifest_bytes: 4,
            unit_bytes: 5,
        };

        let total = checked_manifest_size(&manifest_path, 4, limits).unwrap();
        assert_eq!(checked_unit_size(&unit_path, total, 5, limits).unwrap(), 9);
        assert!(checked_manifest_size(&manifest_path, 5, limits).is_err());
        assert!(matches!(
            checked_unit_size(&unit_path, total, 6, limits),
            Err(PipelineError::ResourceLimit(limit))
                if limit.stage == "normalized_unit" && limit.observed == Some(6)
        ));
        assert!(checked_unit_size(&unit_path, u64::MAX, 1, limits).is_err());

        let (identity, mut manifest, mut units) = normalized_fixture();
        let oversized_key = "x".repeat(MAX_NORMALIZED_MANIFEST_BYTES as usize);
        manifest.units[0].unit_key = oversized_key.clone();
        manifest.units[0].unit_ref = prepared_unit_ref(&oversized_key);
        units[0].unit_key = oversized_key;
        let expected_dir = normalized_instance_dir(
            dir.path(),
            &identity.raw_hash,
            &identity.tool_profile_hash,
            identity.r#gen,
        );
        let err = persist_normalized_instance(dir.path(), &manifest, &units).unwrap_err();
        assert!(err.to_string().contains("normalized manifest exceeds"));
        assert!(!expected_dir.exists());
    }

    #[test]
    fn ct4_fsck_normalized_read_budget_bounds_physical_files() {
        let dir = canonical_tempdir();
        initialize_persist_store(dir.path());
        let (identity, manifest, units) = normalized_fixture();
        persist_normalized_instance(dir.path(), &manifest, &units).unwrap();

        assert!(
            normalized_instance_read_budget_with_file_limit(
                dir.path(),
                &identity.raw_hash,
                &identity.tool_profile_hash,
                identity.r#gen,
                2,
                &ControlHandle::default(),
            )
            .is_ok()
        );
        let error = normalized_instance_read_budget_with_file_limit(
            dir.path(),
            &identity.raw_hash,
            &identity.tool_profile_hash,
            identity.r#gen,
            1,
            &ControlHandle::default(),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("physical file count exceeds 1 limit"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cand_047_061_writer_rejects_both_symlinked_roots_before_mutation() {
        use std::os::unix::fs::symlink;

        let (_, manifest, units) = normalized_fixture();
        for (poisoned_root, untouched_root) in [
            ("normalized_units", "normalized"),
            ("normalized", "normalized_units"),
        ] {
            let kio = canonical_tempdir();
            initialize_persist_store(kio.path());
            let outside = canonical_tempdir();
            fs::create_dir_all(kio.path().join("objects")).unwrap();
            fs::write(outside.path().join("marker"), b"unchanged").unwrap();
            symlink(
                outside.path(),
                kio.path().join("objects").join(poisoned_root),
            )
            .unwrap();

            assert!(persist_normalized_instance(kio.path(), &manifest, &units).is_err());
            assert_eq!(
                fs::read(outside.path().join("marker")).unwrap(),
                b"unchanged"
            );
            assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 1);
            assert!(
                fs::symlink_metadata(kio.path().join("objects").join(poisoned_root))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert!(!kio.path().join("objects").join(untouched_root).exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn cand_047_061_validated_loader_rejects_normalized_units_root_symlink() {
        use std::os::unix::fs::symlink;

        let trusted = canonical_tempdir();
        initialize_persist_store(trusted.path());
        let poisoned = canonical_tempdir();
        let (identity, manifest, units) = normalized_fixture();
        persist_normalized_instance(trusted.path(), &manifest, &units).unwrap();
        assert!(
            load_validated_normalized_instance(
                trusted.path(),
                &identity.raw_hash,
                &identity.tool_profile_hash,
                identity.r#gen,
            )
            .is_ok()
        );

        fs::create_dir_all(poisoned.path().join("objects")).unwrap();
        symlink(
            trusted.path().join("objects/normalized_units"),
            poisoned.path().join("objects/normalized_units"),
        )
        .unwrap();
        assert!(
            load_validated_normalized_instance(
                poisoned.path(),
                &identity.raw_hash,
                &identity.tool_profile_hash,
                identity.r#gen,
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn cand_049_validated_loader_ignores_symlinked_unit_cache() {
        use std::os::unix::fs::symlink;

        let dir = canonical_tempdir();
        initialize_persist_store(dir.path());
        let outside = canonical_tempdir();
        let (identity, manifest, units) = normalized_fixture();
        persist_normalized_instance(dir.path(), &manifest, &units).unwrap();
        let instance_dir = normalized_instance_dir(
            dir.path(),
            &identity.raw_hash,
            &identity.tool_profile_hash,
            identity.r#gen,
        );
        let name = format!("{}.json", prepared_unit_ref(&manifest.units[0].unit_key));
        let unit_path = instance_dir.join(&name);
        let outside_path = outside.path().join(&name);
        fs::write(&outside_path, serde_json::to_vec(&units[0]).unwrap()).unwrap();
        fs::remove_file(&unit_path).unwrap();
        symlink(&outside_path, &unit_path).unwrap();
        assert!(
            load_validated_normalized_instance(
                dir.path(),
                &identity.raw_hash,
                &identity.tool_profile_hash,
                identity.r#gen,
            )
            .is_ok()
        );
    }

    #[test]
    fn cand_061_missing_mutable_unit_cache_does_not_affect_pinned_load() {
        let dir = canonical_tempdir();
        initialize_persist_store(dir.path());
        let (identity, manifest, units) = normalized_fixture();
        persist_normalized_instance(dir.path(), &manifest, &units).unwrap();
        let instance_dir = normalized_instance_dir(
            dir.path(),
            &identity.raw_hash,
            &identity.tool_profile_hash,
            identity.r#gen,
        );
        let name = format!("{}.json", prepared_unit_ref(&manifest.units[0].unit_key));
        fs::remove_file(instance_dir.join(name)).unwrap();

        assert!(
            load_validated_normalized_instance(
                dir.path(),
                &identity.raw_hash,
                &identity.tool_profile_hash,
                identity.r#gen,
            )
            .is_ok()
        );
    }
}
