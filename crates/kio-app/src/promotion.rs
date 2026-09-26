//! Validation and deterministic planning for online Markdownize promotion.
//!
//! The online executor owns adapter calls and task lifecycle.  This module starts
//! strictly after a task reached `Done`: it re-opens the typed normalized-instance
//! reference and verifies that the manifest and every unit form one complete,
//! immutable identity before the CLI is allowed to bind that identity into HEAD.

use std::fs;
use std::path::{Path, PathBuf};

use kio_adapter::tool_lock::{canonical_tool_lock_value, load_tool_lock, tool_lock_hash};
use kio_core::cas::{ContentObjectKind, canonical_json_bytes, is_hash, read_bounded_regular_file};
use kio_core::scope::Repository;
#[cfg(debug_assertions)]
use kio_core::test_control::PromotionFault;
use kio_core::{ExitCode, KioError};
use kio_pipeline::markdownize::{UnitStatus, load_validated_normalized_instance};
use kio_pipeline::task::{TaskDescriptor, TaskOutputRef, TaskStatus, validate_task_output_ref};
use kio_pipeline::{PipelineError, Result as PipelineResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::atomic_overwrite_file;
use crate::*;

const PROMOTION_STATE_FILE: &str = "promotion-state.json";
const PROMOTION_STATE_MAX_BYTES: u64 = 1024 * 1024;
const PROMOTION_STATE_SPEC_VERSION: u32 = 1;

/// Durable boundary between immutable HEAD publication and the derived SQLite
/// swap. The staged tool-lock contains no execution/auth authority and is kept in
/// the journal so a crash immediately after HEAD can restore the exact live
/// projection before rebuilding the index.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PromotionState {
    pub spec_version: u32,
    pub previous_head: String,
    pub staged_tool_lock: Value,
    pub staged_tool_lock_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_head: Option<String>,
}

pub(crate) fn promotion_state_path(kio_dir: &Path) -> PathBuf {
    kio_dir.join(PROMOTION_STATE_FILE)
}

pub(crate) fn stage_promotion(
    repo: &Repository,
    previous_head: &str,
    staged_tool_lock: Value,
) -> kio_core::Result<PromotionState> {
    if !is_hash(previous_head) {
        return Err(KioError::schema(
            "promotion previous_head must be sha256 lowercase hex",
        ));
    }
    let tool_lock_bytes = serde_json::to_vec_pretty(&staged_tool_lock)
        .map_err(|error| KioError::schema(error.to_string()))?;
    load_tool_lock(&tool_lock_bytes).map_err(|error| KioError::schema(error.to_string()))?;
    let staged_tool_lock_hash =
        tool_lock_hash(&staged_tool_lock).map_err(|error| KioError::schema(error.to_string()))?;
    let canonical_tool_lock = canonical_tool_lock_value(&staged_tool_lock)
        .map_err(|error| KioError::schema(error.to_string()))?;
    let canonical_bytes = canonical_json_bytes(&canonical_tool_lock)?;
    let published_hash = repo
        .object_store()
        .write_content_object(ContentObjectKind::Toollock, &canonical_bytes)?;
    if published_hash != staged_tool_lock_hash {
        return Err(KioError::schema(
            "published immutable tool-lock hash differs from staged promotion authority",
        ));
    }
    let state = PromotionState {
        spec_version: PROMOTION_STATE_SPEC_VERSION,
        previous_head: previous_head.to_owned(),
        staged_tool_lock,
        staged_tool_lock_hash,
        target_head: None,
    };
    persist_promotion_state(repo.kio_dir(), &state)?;
    Ok(state)
}

pub(crate) fn load_promotion_state(kio_dir: &Path) -> kio_core::Result<Option<PromotionState>> {
    let path = promotion_state_path(kio_dir);
    match fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(KioError::io(error.to_string(), path.display().to_string())),
    }
    let bytes = read_bounded_regular_file(&path, PROMOTION_STATE_MAX_BYTES)?;
    let state: PromotionState = serde_json::from_slice(&bytes)
        .map_err(|error| promotion_recovery_error("invalid", error.to_string()))?;
    validate_promotion_state(&state)?;
    Ok(Some(state))
}

pub(crate) fn persist_promotion_state(
    kio_dir: &Path,
    state: &PromotionState,
) -> kio_core::Result<()> {
    validate_promotion_state(state)?;
    let bytes = serde_json::to_vec_pretty(state)
        .map_err(|error| promotion_recovery_error("invalid", error.to_string()))?;
    if bytes.len() as u64 > PROMOTION_STATE_MAX_BYTES {
        return Err(promotion_recovery_error(
            "invalid",
            "promotion state exceeds its byte limit",
        ));
    }
    atomic_overwrite_file(&promotion_state_path(kio_dir), &bytes)
}

pub(crate) fn publish_staged_tool_lock(
    kio_dir: &Path,
    state: &PromotionState,
) -> kio_core::Result<()> {
    validate_promotion_state(state)?;
    let bytes = serde_json::to_vec_pretty(&state.staged_tool_lock)
        .map_err(|error| promotion_recovery_error("invalid", error.to_string()))?;
    atomic_overwrite_file(&kio_dir.join("tool-lock.json"), &bytes)
}

pub(crate) fn clear_promotion_state(kio_dir: &Path) -> kio_core::Result<()> {
    let path = promotion_state_path(kio_dir);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(KioError::io(error.to_string(), path.display().to_string())),
    }
}

pub(crate) fn maybe_inject_promotion_fault(phase: &str) -> kio_core::Result<()> {
    if promotion_fault_enabled(phase) {
        return Err(KioError::new(
            "KIO-E-PROMOTION-FAULT-001",
            "injected promotion publication fault",
            json!({ "phase": phase }),
            ExitCode::Failure,
        ));
    }
    Ok(())
}

#[cfg(debug_assertions)]
fn promotion_fault_enabled(phase: &str) -> bool {
    let expected = match phase {
        "before_head" => PromotionFault::BeforeHead,
        "after_head" => PromotionFault::AfterHead,
        "after_index_swap" => PromotionFault::AfterIndexSwap,
        _ => return false,
    };
    crate::debug_test_control()
        .expect("command dispatch installs debug test control")
        .cli
        .promotion_fault
        .known()
        == Some(&expected)
}

#[cfg(not(debug_assertions))]
fn promotion_fault_enabled(_: &str) -> bool {
    false
}

fn validate_promotion_state(state: &PromotionState) -> kio_core::Result<()> {
    if state.spec_version != PROMOTION_STATE_SPEC_VERSION
        || !is_hash(&state.previous_head)
        || !is_hash(&state.staged_tool_lock_hash)
        || state
            .target_head
            .as_deref()
            .is_some_and(|hash| !is_hash(hash))
    {
        return Err(promotion_recovery_error(
            "invalid",
            "promotion state identity is invalid",
        ));
    }
    let bytes = serde_json::to_vec_pretty(&state.staged_tool_lock)
        .map_err(|error| promotion_recovery_error("invalid", error.to_string()))?;
    load_tool_lock(&bytes)
        .map_err(|error| promotion_recovery_error("invalid", error.to_string()))?;
    let actual = tool_lock_hash(&state.staged_tool_lock)
        .map_err(|error| promotion_recovery_error("invalid", error.to_string()))?;
    if actual != state.staged_tool_lock_hash {
        return Err(promotion_recovery_error(
            "invalid",
            "staged tool-lock hash does not match its promotion state",
        ));
    }
    Ok(())
}

pub(crate) fn promotion_recovery_error(phase: &str, message: impl Into<String>) -> KioError {
    KioError::new(
        "KIO-E-PROMOTION-RECOVERY-001",
        message,
        json!({ "phase": phase }),
        ExitCode::Failure,
    )
}

/// One complete online result that is safe to consider for promotion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidatedOnlinePromotion {
    pub task_id: String,
    pub input_path: String,
    pub raw_hash: String,
    pub tool_profile_hash: String,
    pub r#gen: u64,
    pub created_at: String,
    pub bbox_annotation_enabled: bool,
}

/// Validate every completed online Markdownize task and return a deterministic
/// path/time/task ordered promotion pool.
///
/// Partial, failed, pending, baseline, and superseded tasks are intentionally not
/// candidates.  A task that claims `online_adapter_done` but points at malformed or
/// incomplete persisted output is store corruption, not something to silently bind
/// into a commit.
pub(crate) fn validated_online_promotions(
    kio_dir: &Path,
    tasks: &[TaskDescriptor],
) -> PipelineResult<Vec<ValidatedOnlinePromotion>> {
    let mut promotions = Vec::new();
    for task in tasks {
        if task.task_type != kio_pipeline::task::TaskType::Markdownize
            || task.status != TaskStatus::Done
            || task.fallback_reason.as_deref() != Some("online_adapter_done")
        {
            continue;
        }

        let output = validate_task_output_ref(kio_dir, task)?;
        let TaskOutputRef::NormalizedInstance {
            raw_hash,
            tool_profile_hash,
            r#gen,
            ..
        } = output
        else {
            return Err(PipelineError::corrupt(
                kio_dir.join("tasks.jsonl").display().to_string(),
                "completed online Markdownize task has no normalized-instance output",
            ));
        };
        let instance =
            load_validated_normalized_instance(kio_dir, &raw_hash, &tool_profile_hash, r#gen)?;
        let complete = !instance.manifest.units.is_empty()
            && instance
                .manifest
                .units
                .iter()
                .all(|unit| unit.status == UnitStatus::Done)
            && instance.units.len() == instance.manifest.units.len();
        if !complete {
            return Err(PipelineError::corrupt(
                task.output_ref.clone(),
                "Done online Markdownize task points at an incomplete normalized instance",
            ));
        }
        let bbox_annotation_enabled = task.bbox_annotation_enabled.ok_or_else(|| {
            PipelineError::corrupt(
                kio_dir.join("tasks.jsonl").display().to_string(),
                "completed online Markdownize task is missing bbox_annotation_enabled policy stamp",
            )
        })?;
        promotions.push(ValidatedOnlinePromotion {
            task_id: task.task_id.clone(),
            input_path: task.input_path.clone(),
            raw_hash,
            tool_profile_hash,
            r#gen,
            created_at: task.created_at.clone(),
            bbox_annotation_enabled,
        });
    }
    promotions.sort_by(|left, right| {
        left.input_path
            .as_bytes()
            .cmp(right.input_path.as_bytes())
            .then_with(|| left.created_at.cmp(&right.created_at))
            .then_with(|| left.task_id.cmp(&right.task_id))
    });
    Ok(promotions)
}

#[derive(Debug)]
struct OnlinePromotionPlan {
    normalize_by_path: BTreeMap<String, PendingNormalizeRef>,
    active_profile_hash: String,
}

/// Re-derive the promotable online result set from persisted task/instance truth.
/// Mutable task status alone is never enough: `validated_online_promotions`
/// verifies the typed output and complete manifest/unit set, while this boundary
/// rechecks current scope policy, working bytes, HEAD membership, and the adapter's
/// currently resolved immutable profile.
fn plan_online_markdownize_promotion(
    repo: &Repository,
) -> kio_core::Result<Option<OnlinePromotionPlan>> {
    let task_store = TaskStore::new(repo.kio_dir());
    let tasks = task_store.all().map_err(pipeline_to_kio)?;
    let Some(head_hash) = repo.head_commit_hash()? else {
        return Ok(None);
    };
    let head = match repo.read_commit(&head_hash) {
        Ok(commit) => commit,
        Err(error) if is_store_not_found(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let head_tree = match repo.read_tree(&head.tree) {
        Ok(tree) => tree,
        Err(error) if is_store_not_found(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let head_entries = head_tree
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect::<BTreeMap<_, _>>();
    let max_input_bytes = effective_max_input_bytes(repo);
    // Establish that the task still describes the live HEAD/working identity
    // before opening its normalized output. A corrupt output for the current
    // identity is fatal, but a stale previous generation must not break a later
    // full re-index after the file changed.
    let mut current_tasks = Vec::new();
    for task in tasks {
        if task.task_type != TaskType::Markdownize
            || task.status != TaskStatus::Done
            || task.fallback_reason.as_deref() != Some("online_adapter_done")
        {
            continue;
        }
        let Some(head_entry) = head_entries.get(task.input_path.as_str()) else {
            continue;
        };
        if head_entry.raw_hash != task.input_hash
            || !repository_scan_policy_allows_file(repo, &task.input_path)
                .map_err(pipeline_to_kio)?
        {
            continue;
        }
        let Ok(verified) = read_repository_scan_input(repo, &task.input_path, max_input_bytes)
        else {
            continue;
        };
        if verified.raw_hash != task.input_hash {
            continue;
        }
        ensure_raw_ingest_allowed(repo, &task.input_hash)?;
        current_tasks.push(task);
    }
    let current =
        validated_online_promotions(repo.kio_dir(), &current_tasks).map_err(pipeline_to_kio)?;
    if current.is_empty() {
        return Ok(None);
    }

    // A result already bound by HEAD stays authoritative without another model
    // resolution. A not-yet-bound result is promotable only if its immutable hash
    // matches the profile resolved now; this prevents a Done result from an older
    // mutable model alias being promoted after the alias advances.
    let policies_to_resolve = current
        .iter()
        .filter(|candidate| {
            head_entries
                .get(candidate.input_path.as_str())
                .and_then(|entry| entry.normalize.as_ref())
                .is_none_or(|normalize| {
                    normalize.tool_profile_hash != candidate.tool_profile_hash
                        || normalize.r#gen != candidate.r#gen
                })
        })
        .map(|candidate| candidate.bbox_annotation_enabled)
        .collect::<BTreeSet<_>>();
    let adapter_scope_id = repo.scope_identity()?.scope_id;
    let resolved_profiles = policies_to_resolve
        .into_iter()
        .filter_map(|bbox_enabled| {
            resolve_standard_online_markdownize_profile_with_bbox(&adapter_scope_id, bbox_enabled)
                .ok()
                .map(|profile| (bbox_enabled, profile))
        })
        .collect::<BTreeMap<_, _>>();

    let mut by_path = BTreeMap::<String, Vec<_>>::new();
    for candidate in current {
        by_path
            .entry(candidate.input_path.clone())
            .or_default()
            .push(candidate);
    }
    let mut selected = Vec::new();
    for (path, candidates) in by_path {
        let resolved = candidates.iter().rev().find(|candidate| {
            resolved_profiles
                .get(&candidate.bbox_annotation_enabled)
                .is_some_and(|profile| candidate.tool_profile_hash == profile.tool_profile_hash)
        });
        let already_bound = candidates.iter().rev().find(|candidate| {
            head_entries
                .get(path.as_str())
                .and_then(|entry| entry.normalize.as_ref())
                .is_some_and(|normalize| {
                    normalize.tool_profile_hash == candidate.tool_profile_hash
                        && normalize.r#gen == candidate.r#gen
                })
        });
        if let Some(candidate) = resolved.or(already_bound) {
            selected.push(candidate.clone());
        }
    }
    if selected.is_empty() {
        return Ok(None);
    }
    selected.sort_by(|left, right| left.input_path.as_bytes().cmp(right.input_path.as_bytes()));
    let effective_bbox_policy = bbox_annotation_enabled(repo)?;
    let active_profile_hash = selected
        .iter()
        .rev()
        .filter(|candidate| candidate.bbox_annotation_enabled == effective_bbox_policy)
        .find_map(|candidate| {
            resolved_profiles
                .get(&candidate.bbox_annotation_enabled)
                .filter(|profile| profile.tool_profile_hash == candidate.tool_profile_hash)
                .map(|profile| profile.tool_profile_hash.clone())
        })
        .unwrap_or_else(|| {
            selected
                .iter()
                .rev()
                .find(|candidate| candidate.bbox_annotation_enabled == effective_bbox_policy)
                .or_else(|| selected.last())
                .expect("selected promotion is non-empty")
                .tool_profile_hash
                .clone()
        });
    let normalize_by_path = selected
        .into_iter()
        .map(|candidate| {
            // PB04: this online candidate's normalized instance was just
            // materialized (`validated_online_promotions` above already
            // opened and validated its manifest/units), so its manifest.json
            // is hashable here.
            let manifest_hash = crate::compute_manifest_hash(
                repo,
                &candidate.raw_hash,
                &candidate.tool_profile_hash,
                candidate.r#gen,
            )?;
            Ok((
                candidate.input_path,
                PendingNormalizeRef {
                    expected_raw_hash: candidate.raw_hash,
                    normalize: NormalizeRef {
                        tool_profile_hash: candidate.tool_profile_hash,
                        r#gen: candidate.r#gen,
                        manifest_hash,
                    },
                },
            ))
        })
        .collect::<Result<_>>()?;
    Ok(Some(OnlinePromotionPlan {
        normalize_by_path,
        active_profile_hash,
    }))
}

/// Replace only the Markdownize identity in tool-lock. The entry deliberately
/// carries no URL, command, arguments, credentials, or other execution authority.
fn promoted_markdown_tool_lock(repo: &Repository, profile_hash: &str) -> kio_core::Result<Value> {
    const TOOL_LOCK_MAX_BYTES: u64 = 1024 * 1024;
    let path = repo.kio_dir().join("tool-lock.json");
    let bytes = read_bounded_regular_file(&path, TOOL_LOCK_MAX_BYTES)?;
    let mut value: Value =
        serde_json::from_slice(&bytes).map_err(|error| KioError::schema(error.to_string()))?;
    let online = standard_online_markdownize_profile();
    value["markdown"] = json!({
        "tool_id": online.adapter_id,
        "profile_hash": profile_hash,
        "kind": "online_api",
        "capabilities": online.capability_flags,
    });
    let bytes =
        serde_json::to_vec_pretty(&value).map_err(|error| KioError::schema(error.to_string()))?;
    load_tool_lock(&bytes).map_err(adapter_to_kio)?;
    Ok(value)
}

fn materialize_promoted_markdown_tool_lock(
    repo: &Repository,
    profile_hash: &str,
) -> kio_core::Result<()> {
    let path = repo.kio_dir().join("tool-lock.json");
    let value = promoted_markdown_tool_lock(repo, profile_hash)?;
    let bytes =
        serde_json::to_vec_pretty(&value).map_err(|error| KioError::schema(error.to_string()))?;
    atomic_overwrite_file(&path, &bytes)
}

/// Reconcile already-Done online results into an ordinary index's single pending
/// snapshot. This prevents the deterministic baseline pass from demoting a
/// previously promoted file and avoids an extra promotion commit.
pub(super) fn apply_online_promotion_to_index(
    repo: &Repository,
    result: &mut IndexPipelineResult,
) -> kio_core::Result<()> {
    let Some(plan) = plan_online_markdownize_promotion(repo)? else {
        return Ok(());
    };
    materialize_promoted_markdown_tool_lock(repo, &plan.active_profile_hash)?;
    result.normalize_by_path.extend(plan.normalize_by_path);
    Ok(())
}

/// Publish one deterministic promotion commit for all accepted outputs completed
/// by a batch pass. The repository primitive changes only matching HEAD bindings;
/// unrelated deferred working-tree edits are never captured.
pub(super) fn promote_completed_online_markdownize(repo: &Repository) -> kio_core::Result<bool> {
    let Some(plan) = plan_online_markdownize_promotion(repo)? else {
        return Ok(false);
    };
    let previous_head = repo
        .head_commit_hash()?
        .ok_or_else(|| KioError::invalid_usage("cannot promote in an unborn scope"))?;
    let staged_tool_lock = promoted_markdown_tool_lock(repo, &plan.active_profile_hash)?;
    let mut state = stage_promotion(repo, &previous_head, staged_tool_lock)?;
    maybe_inject_promotion_fault("before_head")?;
    let outcome = repo.promote_normalize_refs_with_staged_tool_lock(
        Some("online Markdownize promotion"),
        &plan.normalize_by_path,
        &state.staged_tool_lock_hash,
    )?;
    if outcome.noop {
        publish_staged_tool_lock(repo.kio_dir(), &state)?;
        clear_promotion_state(repo.kio_dir())?;
        return Ok(false);
    }
    let commit_hash = outcome
        .commit_hash
        .as_ref()
        .ok_or_else(|| promotion_recovery_error("after_head", "promotion commit is missing"))?;
    // HEAD now names normalized content that the existing source index and
    // replica projection do not describe.  Take the replica out of service
    // before any later durable-publication step (including the fault seam)
    // can return: direct search is replica-only and must not quietly serve the
    // previous HEAD as if it were current.  `finish_pending_online_promotion`
    // publishes Ready again only after the source-index rebuild and complete
    // writer projection succeed.
    mark_replica_rebuilding_or_log(repo.kio_dir(), commit_hash);
    state.target_head = Some(commit_hash.clone());
    persist_promotion_state(repo.kio_dir(), &state)?;
    publish_staged_tool_lock(repo.kio_dir(), &state)?;
    maybe_inject_promotion_fault("after_head")?;
    if let Some(commit_hash) = &outcome.commit_hash {
        append_event_log(
            "KIO-I-COMMIT-CREATED-001",
            "online Markdownize promotion commit created",
            json!({
                "commit_hash": commit_hash,
                "tree_hash": outcome.tree_hash,
                "commit_type": "auto",
            }),
        )?;
    }
    Ok(true)
}

/// Recover the durable half of a promotion publication. A prepared transaction
/// whose HEAD never moved is discarded so the task can be planned again. Once
/// HEAD moved, the exact staged tool-lock is restored and the retained journal
/// makes the required SQLite rebuild explicit to the caller.
pub(super) fn recover_pending_online_promotion(repo: &Repository) -> kio_core::Result<bool> {
    let Some(mut state) = load_promotion_state(repo.kio_dir())? else {
        return Ok(false);
    };
    let current_head = repo
        .head_commit_hash()?
        .ok_or_else(|| promotion_recovery_error("invalid", "promotion HEAD is missing"))?;
    if current_head == state.previous_head {
        if state.target_head.is_some() {
            return Err(promotion_recovery_error(
                "invalid",
                "promotion state names a published HEAD but repository HEAD is still its parent",
            ));
        }
        clear_promotion_state(repo.kio_dir())?;
        return Ok(false);
    }

    if state
        .target_head
        .as_deref()
        .is_some_and(|target| target != current_head)
    {
        return Err(promotion_recovery_error(
            "invalid",
            "repository HEAD diverged from the pending promotion",
        ));
    }
    let commit = repo.read_commit(&current_head)?;
    if commit.parent.as_deref() != Some(state.previous_head.as_str())
        || commit.commit_type != CommitType::Auto
        || commit.message != "online Markdownize promotion"
        || commit.tool_lock_hash != state.staged_tool_lock_hash
    {
        return Err(promotion_recovery_error(
            "invalid",
            "repository HEAD does not match the pending promotion identity",
        ));
    }
    if state.target_head.is_none() {
        state.target_head = Some(current_head);
        persist_promotion_state(repo.kio_dir(), &state)?;
    }
    publish_staged_tool_lock(repo.kio_dir(), &state)?;
    Ok(true)
}

pub(super) fn finish_pending_online_promotion(repo: &Repository) -> kio_core::Result<()> {
    rebuild_step3_index(repo)?;
    maybe_inject_promotion_fault("after_index_swap")?;
    clear_promotion_state(repo.kio_dir())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use kio_core::scope::Repository;
    use kio_pipeline::markdownize::{
        MarkdownizeMode, NormalizedInstanceManifest, NormalizedUnitManifestEntry,
        NormalizedUnitObject, UnitStatus, persist_normalized_instance,
    };
    use kio_pipeline::prepare::{UnitType, hash_bytes, unit_ref};
    use kio_pipeline::task::{TaskDescriptor, TaskStatus, TaskType, normalized_task_output_ref};

    use super::validated_online_promotions;

    fn done_task(output_ref: String, raw_hash: String) -> TaskDescriptor {
        TaskDescriptor {
            task_id: "task_01TEST".to_owned(),
            task_type: TaskType::Markdownize,
            mode: Some(MarkdownizeMode::Full),
            input_path: "report.pdf".to_owned(),
            input_hash: raw_hash,
            previous_raw_hash: None,
            parent_run_id: None,
            changed_unit_keys: vec!["page:1".to_owned()],
            output_ref,
            unit_keys: Some(vec!["page:1".to_owned()]),
            status: TaskStatus::Done,
            attempts: 0,
            next_retry_at: None,
            deadline: None,
            heartbeat_at: None,
            fallback_reason: Some("online_adapter_done".to_owned()),
            created_at: "2026-07-13T00:00:00Z".to_owned(),
            bbox_annotation_enabled: Some(true),
            hold_reason: None,
            reserved_usd: None,
            reserved_month: None,
            reservation_id: None,
        }
    }

    #[test]
    fn accepts_only_complete_done_online_instances() {
        let dir = tempfile::tempdir().unwrap();
        Repository::init(dir.path()).unwrap();
        let kio_dir = dir.path().join(".kio");
        let raw_hash = hash_bytes(b"raw");
        let profile_hash = hash_bytes(b"profile");
        let prepared_hash = hash_bytes(b"prepared");
        let unit_key = "page:1".to_owned();
        let manifest = NormalizedInstanceManifest {
            raw_hash: raw_hash.clone(),
            tool_profile_hash: profile_hash.clone(),
            r#gen: 0,
            parent_gen: None,
            run_id: "run_01TEST".to_owned(),
            units: vec![NormalizedUnitManifestEntry {
                order: 0,
                unit_key: unit_key.clone(),
                unit_ref: unit_ref(&unit_key),
                unit_type: UnitType::Page,
                status: UnitStatus::Done,
                prepared_hash: prepared_hash.clone(),
                preparation_profile_hash: profile_hash.clone(),
                error_kind: None,
                unit_object_hash: None,
            }],
            generated_at: "2026-07-13T00:00:00Z".to_owned(),
        };
        let unit = NormalizedUnitObject {
            unit_key,
            unit_type: UnitType::Page,
            raw_hash: raw_hash.clone(),
            prepared_hash,
            preparation_profile_hash: profile_hash.clone(),
            tool_profile_hash: profile_hash.clone(),
            r#gen: 0,
            mode: MarkdownizeMode::Full,
            markdown: "promoted mock text".to_owned(),
            owned_image_hashes: BTreeSet::new(),
            metadata: BTreeMap::new(),
            reused_from: None,
            generated_at: "2026-07-13T00:00:00Z".to_owned(),
        };
        persist_normalized_instance(&kio_dir, &manifest, &[unit]).unwrap();
        let output_ref = normalized_task_output_ref(&raw_hash, &profile_hash, 0);
        let task = done_task(output_ref, raw_hash);
        let planned = validated_online_promotions(&kio_dir, std::slice::from_ref(&task)).unwrap();
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].tool_profile_hash, profile_hash);
        assert!(planned[0].bbox_annotation_enabled);

        let mut partial = task;
        partial.status = TaskStatus::Partial;
        assert!(
            validated_online_promotions(&kio_dir, &[partial])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn rejects_completed_online_instance_without_bbox_policy_stamp() {
        let dir = tempfile::tempdir().unwrap();
        Repository::init(dir.path()).unwrap();
        let kio_dir = dir.path().join(".kio");
        let raw_hash = hash_bytes(b"raw");
        let profile_hash = hash_bytes(b"profile");
        let prepared_hash = hash_bytes(b"prepared");
        let manifest = NormalizedInstanceManifest {
            raw_hash: raw_hash.clone(),
            tool_profile_hash: profile_hash.clone(),
            r#gen: 0,
            parent_gen: None,
            run_id: "run_current".to_owned(),
            units: vec![NormalizedUnitManifestEntry {
                order: 0,
                unit_key: "page:1".to_owned(),
                unit_ref: unit_ref("page:1"),
                unit_type: UnitType::Page,
                status: UnitStatus::Done,
                prepared_hash: prepared_hash.clone(),
                preparation_profile_hash: profile_hash.clone(),
                error_kind: None,
                unit_object_hash: None,
            }],
            generated_at: "2026-08-13T00:00:00Z".to_owned(),
        };
        let unit = NormalizedUnitObject {
            unit_key: "page:1".to_owned(),
            unit_type: UnitType::Page,
            raw_hash: raw_hash.clone(),
            prepared_hash,
            preparation_profile_hash: profile_hash.clone(),
            tool_profile_hash: profile_hash.clone(),
            r#gen: 0,
            mode: MarkdownizeMode::Full,
            markdown: "promoted mock text".to_owned(),
            owned_image_hashes: BTreeSet::new(),
            metadata: BTreeMap::new(),
            reused_from: None,
            generated_at: "2026-08-13T00:00:00Z".to_owned(),
        };
        persist_normalized_instance(&kio_dir, &manifest, &[unit]).unwrap();
        let output_ref = normalized_task_output_ref(&raw_hash, &profile_hash, 0);
        let mut task = done_task(output_ref, raw_hash);
        task.bbox_annotation_enabled = None;
        assert!(validated_online_promotions(&kio_dir, &[task]).is_err());
    }
}
