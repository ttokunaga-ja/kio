//! Validation and deterministic planning for Markdownize promotion.
//!
//! The executors own adapter calls and task lifecycle. This module re-opens
//! typed normalized instances and verifies their immutable unit identities before
//! binding them into HEAD. Complete tasks and resource-held partial tasks publish
//! only verified done units.
use crate::index::command::hash_repository_scan_input;
use crate::index::command::repository_scan_policy_allows_file;
use crate::index::rebuild::rebuild_step3_index;
use crate::object_persistence::adapter_to_kio;
use crate::object_persistence::pipeline_to_kio;
use crate::scope_access::ensure_raw_ingest_allowed;
use crate::scope_access::is_store_not_found;
use crate::search::projection::mark_replica_rebuilding_or_log;
use crate::settings::bbox_annotation_enabled;
use kio_adapter::catalog::active_local_ocr_execution_with_control;
use kio_adapter::catalog::resolve_standard_online_markdownize_profile_with_bbox_with_control;
use kio_adapter::catalog::standard_online_markdownize_profile;
use kio_adapter::tool_lock::AdapterRuntimeSettings;
use kio_adapter::types::AdapterProfile;
use kio_core::control::ControlHandle;
use kio_core::dag::CommitType;
use kio_core::dag::NormalizeRef;
use kio_core::disk_records::{DiskRecords, RecordSorter, RecordWriter, SortedRecordCursor};
use kio_core::scope::PendingNormalizeRef;
use kio_pipeline::task::TaskStore;
use kio_pipeline::task::TaskType;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

use std::fs;
use std::path::{Path, PathBuf};

use kio_adapter::tool_lock::{canonical_tool_lock_value, load_tool_lock, tool_lock_hash};
use kio_core::cas::{ContentObjectKind, canonical_json_bytes, is_hash, read_bounded_regular_file};
use kio_core::scope::Repository;
#[cfg(debug_assertions)]
use kio_core::test_control::PromotionFault;
use kio_core::{ExitCode, KioError};
use kio_pipeline::markdownize::{UnitStatus, load_validated_normalized_instance_with_control};
use kio_pipeline::task::{TaskDescriptor, TaskOutputRef, TaskStatus, validate_task_output_ref};
use kio_pipeline::{PipelineError, Result as PipelineResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::object_persistence::atomic_overwrite_file;

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

pub(crate) fn maybe_inject_promotion_fault(
    phase: &str,
    control: &ControlHandle,
) -> kio_core::Result<()> {
    if promotion_fault_enabled(phase, control) {
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
fn promotion_fault_enabled(phase: &str, control: &ControlHandle) -> bool {
    let expected = match phase {
        "before_head" => PromotionFault::BeforeHead,
        "after_head" => PromotionFault::AfterHead,
        "after_index_swap" => PromotionFault::AfterIndexSwap,
        _ => return false,
    };
    control.debug().cli.promotion_fault.known() == Some(&expected)
}

#[cfg(not(debug_assertions))]
fn promotion_fault_enabled(_: &str, _control: &ControlHandle) -> bool {
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

/// Tool ID and profile hash are canonical hash-bound identity. Execution kind
/// is only an annotation: it must agree with the implemented producer and can
/// never choose another recovery label by itself.
fn promotion_messages(state: &PromotionState) -> kio_core::Result<(&'static str, &'static str)> {
    validate_promotion_state(state)?;
    let markdown = &state.staged_tool_lock["markdown"];
    match (markdown["tool_id"].as_str(), markdown["kind"].as_str()) {
        (Some("mistral_ocr_markdownize"), Some("online_api")) => Ok((
            "online Markdownize promotion",
            "online Markdownize promotion commit created",
        )),
        (Some("paddleocr_vl_local"), Some("offline_api")) => Ok((
            "local Markdownize promotion",
            "local Markdownize promotion commit created",
        )),
        _ => Err(promotion_recovery_error(
            "invalid",
            "staged Markdownize producer and execution annotation are inconsistent or unsupported",
        )),
    }
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[cfg(test)]
pub(crate) fn validated_online_promotions(
    kio_dir: &Path,
    tasks: &[TaskDescriptor],
    control: &ControlHandle,
) -> PipelineResult<Vec<ValidatedOnlinePromotion>> {
    let mut promotions = Vec::new();
    for task in tasks.iter().filter(|task| is_online_promotion(task)) {
        promotions.push(validate_online_promotion(kio_dir, task, control)?);
    }
    promotions.sort_by(compare_online_promotions);
    Ok(promotions)
}

fn is_online_promotion(task: &TaskDescriptor) -> bool {
    task.task_type == TaskType::Markdownize
        && (task.status == TaskStatus::Done
            && task.fallback_reason.as_deref() == Some("online_adapter_done")
            || task.status == TaskStatus::Partial
                && task.bbox_annotation_enabled.is_some()
                && task.hold_reason == Some(kio_pipeline::task::HoldReason::ResourceLimit))
}

fn compare_online_promotions(
    left: &ValidatedOnlinePromotion,
    right: &ValidatedOnlinePromotion,
) -> std::cmp::Ordering {
    left.input_path
        .cmp(&right.input_path)
        .then_with(|| left.created_at.cmp(&right.created_at))
        .then_with(|| left.task_id.cmp(&right.task_id))
}

fn validate_online_promotion(
    kio_dir: &Path,
    task: &TaskDescriptor,
    control: &ControlHandle,
) -> PipelineResult<ValidatedOnlinePromotion> {
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
    let instance = load_validated_normalized_instance_with_control(
        kio_dir,
        &raw_hash,
        &tool_profile_hash,
        r#gen,
        control,
    )?;
    let complete = task.status == TaskStatus::Done
        && !instance.manifest.units.is_empty()
        && instance
            .manifest
            .units
            .iter()
            .all(|unit| unit.status == UnitStatus::Done)
        && instance.units.len() == instance.manifest.units.len();
    let resource_partial = resource_partial_can_publish(task, &instance);
    if !complete && !resource_partial {
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
    Ok(ValidatedOnlinePromotion {
        task_id: task.task_id.clone(),
        input_path: task.input_path.clone(),
        raw_hash,
        tool_profile_hash,
        r#gen,
        created_at: task.created_at.clone(),
        bbox_annotation_enabled,
    })
}

#[derive(Debug)]
struct MarkdownizePromotionPlan {
    normalize_by_path: DiskRecords<(String, PendingNormalizeRef)>,
    markdown_profile: AdapterProfile,
}

/// Re-derive the promotable online result set from persisted task/instance truth.
/// Mutable task status alone is never enough: `validate_online_promotion`
/// verifies the typed output and complete manifest/unit set, while this boundary
/// rechecks current scope policy, working bytes, HEAD membership, and the adapter's
/// currently resolved immutable profile.
fn plan_online_markdownize_promotion(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
) -> kio_core::Result<Option<MarkdownizePromotionPlan>> {
    let task_store = TaskStore::new_with_control(repo.kio_dir(), repo.control().clone());
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
    let stages = repo.object_store().record_stages()?;
    let mut head_entries = SortedRecordCursor::new(head_tree.entries.iter(), |entry| &entry.path);
    let mut current = RecordSorter::new(&stages, |a: &(ValidatedOnlinePromotion, bool), b| {
        compare_online_promotions(&a.0, &b.0)
    });
    let mut tasks = tasks;
    tasks.retain(is_online_promotion);
    tasks.sort_unstable_by(|a, b| a.input_path.cmp(&b.input_path));
    let mut policies_to_resolve = BTreeSet::new();
    let mut found = false;
    // Establish live HEAD and working identities before opening task output.
    for task in tasks {
        let Some(head_entry) = head_entries.at(&task.input_path)? else {
            continue;
        };
        if head_entry.raw_hash != task.input_hash
            || !repository_scan_policy_allows_file(repo, &task.input_path)
                .map_err(pipeline_to_kio)?
        {
            continue;
        }
        let Ok(verified) = hash_repository_scan_input(repo, &task.input_path) else {
            continue;
        };
        if verified.raw_hash != task.input_hash {
            continue;
        }
        ensure_raw_ingest_allowed(repo, &task.input_hash)?;
        let candidate = validate_online_promotion(repo.kio_dir(), &task, repo.control())
            .map_err(pipeline_to_kio)?;
        let already_bound = head_entry.normalize.as_ref().is_some_and(|normalize| {
            normalize.tool_profile_hash == candidate.tool_profile_hash
                && normalize.r#gen == candidate.r#gen
        });
        if !already_bound {
            policies_to_resolve.insert(candidate.bbox_annotation_enabled);
        }
        current.push((candidate, already_bound))?;
        found = true;
    }
    if !found {
        return Ok(None);
    }

    // A HEAD-bound result stays authoritative. Unbound results need a
    // profile resolved for this invocation and their persisted bbox policy.
    let adapter_scope_id = repo.scope_identity()?.scope_id;
    let resolved_profiles = policies_to_resolve
        .into_iter()
        .filter_map(|bbox_enabled| {
            resolve_standard_online_markdownize_profile_with_bbox_with_control(
                std::sync::Arc::clone(adapter_settings),
                &adapter_scope_id,
                bbox_enabled,
                repo.control(),
            )
            .ok()
            .map(|profile| (bbox_enabled, profile))
        })
        .collect::<BTreeMap<_, _>>();

    let mut normalizes = RecordWriter::new(&stages)?;
    let mut current = current.finish()?.into_iter().peekable();
    let mut last_hash = None;
    let mut latest_for_policy = [None, None];
    let mut resolved_for_policy = [None, None];
    while let Some(first) = current.next() {
        let first = first?;
        let path = first.0.input_path.clone();
        let mut resolved = None;
        let mut bound = None;
        let mut consider = |(candidate, already_bound): (ValidatedOnlinePromotion, bool)| {
            if resolved_profiles
                .get(&candidate.bbox_annotation_enabled)
                .is_some_and(|profile| profile.tool_profile_hash == candidate.tool_profile_hash)
            {
                resolved = Some(candidate);
            } else if already_bound {
                bound = Some(candidate);
            }
        };
        consider(first);
        while current
            .peek()
            .is_some_and(|row| row.as_ref().is_ok_and(|row| row.0.input_path == path))
        {
            consider(current.next().expect("peeked candidate")?);
        }
        let from_resolved_profile = resolved.is_some();
        let Some(candidate) = resolved.or(bound) else {
            continue;
        };
        let policy = usize::from(candidate.bbox_annotation_enabled);
        let profile_hash = candidate.tool_profile_hash;
        if from_resolved_profile {
            resolved_for_policy[policy] = Some(profile_hash.clone());
        }
        latest_for_policy[policy] = Some(profile_hash.clone());
        last_hash = Some(profile_hash.clone());

        let manifest_hash = crate::object_persistence::compute_manifest_hash(
            repo,
            &candidate.raw_hash,
            &profile_hash,
            candidate.r#gen,
        )?;
        normalizes.push(&(
            candidate.input_path,
            PendingNormalizeRef {
                expected_raw_hash: candidate.raw_hash,
                normalize: NormalizeRef {
                    tool_profile_hash: profile_hash,
                    r#gen: candidate.r#gen,
                    manifest_hash,
                },
            },
        ))?;
    }
    let Some(last_hash) = last_hash else {
        return Ok(None);
    };
    let effective_policy = usize::from(bbox_annotation_enabled(repo)?);
    let active_profile_hash = resolved_for_policy[effective_policy]
        .take()
        .or_else(|| latest_for_policy[effective_policy].take())
        .unwrap_or(last_hash);
    let normalize_by_path = normalizes.finish()?;
    let mut markdown_profile = standard_online_markdownize_profile();
    markdown_profile.tool_profile_hash = active_profile_hash;
    Ok(Some(MarkdownizePromotionPlan {
        normalize_by_path,
        markdown_profile,
    }))
}

/// Local completions come from persisted truth so a crash after Task
/// publication but before HEAD never requires another OCR request.
fn plan_local_markdownize_promotion(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
) -> kio_core::Result<Option<MarkdownizePromotionPlan>> {
    let Some(execution) = active_local_ocr_execution_with_control(adapter_settings, repo.control())
    else {
        return Ok(None);
    };
    let profile = kio_adapter::local_ocr_markdownize::profile_for(execution);
    let Some(head_hash) = repo.head_commit_hash()? else {
        return Ok(None);
    };
    let head = repo.read_commit(&head_hash)?;
    let tree = repo.read_tree(&head.tree)?;
    let mut tasks = TaskStore::new_with_control(repo.kio_dir(), repo.control().clone())
        .all()
        .map_err(pipeline_to_kio)?;
    tasks.sort_by(|left, right| {
        left.input_path
            .as_bytes()
            .cmp(right.input_path.as_bytes())
            .then_with(|| left.created_at.cmp(&right.created_at))
            .then_with(|| left.task_id.cmp(&right.task_id))
    });
    let mut entries = SortedRecordCursor::new(tree.entries.iter(), |entry| &entry.path);
    let stages = repo.object_store().record_stages()?;
    let mut normalizes = RecordWriter::new(&stages)?;
    let mut previous: Option<(String, PendingNormalizeRef)> = None;
    for task in tasks {
        if task.task_type != TaskType::Markdownize
            || !(task.status == TaskStatus::Done
                && task.fallback_reason.as_deref() == Some("local_adapter_done")
                || task.status == TaskStatus::Partial
                    && task.bbox_annotation_enabled.is_none()
                    && task.resource_limit.is_some())
        {
            continue;
        }
        let Some(entry) = entries.at(&task.input_path)? else {
            continue;
        };
        if entry.raw_hash != task.input_hash
            || !repository_scan_policy_allows_file(repo, &task.input_path)
                .map_err(pipeline_to_kio)?
        {
            continue;
        }
        let Ok(verified) = hash_repository_scan_input(repo, &task.input_path) else {
            continue;
        };
        if verified.raw_hash != task.input_hash {
            continue;
        }
        ensure_raw_ingest_allowed(repo, &task.input_hash)?;
        if let Some(normalize) = validated_local_normalize_ref(repo, &task, &profile)? {
            if let Some(row) = previous.take()
                && row.0 != task.input_path
            {
                normalizes.push(&row)?;
            }
            previous = Some((task.input_path, normalize));
        }
    }
    if let Some(row) = previous {
        normalizes.push(&row)?;
    }
    let normalize_by_path = normalizes.finish()?;
    if normalize_by_path.is_empty() {
        return Ok(None);
    }
    Ok(Some(MarkdownizePromotionPlan {
        normalize_by_path,
        markdown_profile: profile,
    }))
}

// A call-wide interruption has no accepted failed-unit response to persist.
// Its typed Task hold is sufficient; all published units still come exclusively
// from the fully verified immutable instance, regardless of the interruption site.
fn resource_partial_can_publish(
    task: &TaskDescriptor,
    instance: &kio_pipeline::markdownize::ValidatedNormalizedInstance,
) -> bool {
    task.status == TaskStatus::Partial
        && task.hold_reason == Some(kio_pipeline::task::HoldReason::ResourceLimit)
        && task.resource_limit.is_some()
        && !instance.units.is_empty()
}

pub(crate) fn validated_local_normalize_ref(
    repo: &Repository,
    task: &TaskDescriptor,
    profile: &AdapterProfile,
) -> kio_core::Result<Option<PendingNormalizeRef>> {
    if task.task_type != TaskType::Markdownize
        || !(task.status == TaskStatus::Done
            && task.fallback_reason.as_deref() == Some("local_adapter_done")
            || task.status == TaskStatus::Partial
                && task.bbox_annotation_enabled.is_none()
                && task.resource_limit.is_some())
    {
        return Ok(None);
    }
    let output = validate_task_output_ref(repo.kio_dir(), task).map_err(pipeline_to_kio)?;
    let TaskOutputRef::NormalizedInstance {
        raw_hash,
        tool_profile_hash,
        r#gen,
        ..
    } = output
    else {
        return Err(pipeline_to_kio(PipelineError::corrupt(
            task.output_ref.clone(),
            "completed local Markdownize task has no normalized-instance output",
        )));
    };
    if tool_profile_hash != profile.tool_profile_hash {
        return Ok(None);
    }
    if raw_hash != task.input_hash
        || task.reservation_id.is_some()
        || task.reserved_month.is_some()
        || task.reserved_usd.is_some()
    {
        return Err(pipeline_to_kio(PipelineError::corrupt(
            task.output_ref.clone(),
            "completed local Markdownize task has inconsistent raw or paid authority",
        )));
    }
    let instance = load_validated_normalized_instance_with_control(
        repo.kio_dir(),
        &raw_hash,
        &tool_profile_hash,
        r#gen,
        repo.control(),
    )
    .map_err(pipeline_to_kio)?;
    let complete = !instance.manifest.units.is_empty()
        && instance
            .manifest
            .units
            .iter()
            .all(|u| u.status == UnitStatus::Done)
        && instance.units.len() == instance.manifest.units.len();
    let resource_partial = resource_partial_can_publish(task, &instance);
    if !(task.status == TaskStatus::Done && complete || resource_partial) {
        return Err(pipeline_to_kio(PipelineError::corrupt(
            task.output_ref.clone(),
            "local task status differs from normalized units",
        )));
    }
    let manifest_hash = crate::object_persistence::compute_manifest_hash(
        repo,
        &raw_hash,
        &tool_profile_hash,
        r#gen,
    )?;
    Ok(Some(PendingNormalizeRef {
        expected_raw_hash: raw_hash,
        normalize: NormalizeRef {
            tool_profile_hash,
            r#gen,
            manifest_hash,
        },
    }))
}

fn plan_selected_markdownize_promotion(
    settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
) -> kio_core::Result<Option<MarkdownizePromotionPlan>> {
    if active_local_ocr_execution_with_control(settings, repo.control()).is_some() {
        plan_local_markdownize_promotion(settings, repo)
    } else {
        plan_online_markdownize_promotion(settings, repo)
    }
}

/// Replace only the Markdownize identity in tool-lock. The entry deliberately
/// carries no URL, command, arguments, credentials, or other execution authority.
fn promoted_markdown_tool_lock(
    repo: &Repository,
    profile: &AdapterProfile,
) -> kio_core::Result<Value> {
    const TOOL_LOCK_MAX_BYTES: u64 = 1024 * 1024;
    let path = repo.kio_dir().join("tool-lock.json");
    let bytes = read_bounded_regular_file(&path, TOOL_LOCK_MAX_BYTES)?;
    let mut value: Value =
        serde_json::from_slice(&bytes).map_err(|error| KioError::schema(error.to_string()))?;
    value["markdown"] = json!({
        "tool_id": profile.adapter_id,
        "profile_hash": profile.tool_profile_hash,
        "kind": profile.execution_mode,
        "capabilities": profile.capability_flags,
    });
    let bytes =
        serde_json::to_vec_pretty(&value).map_err(|error| KioError::schema(error.to_string()))?;
    load_tool_lock(&bytes).map_err(adapter_to_kio)?;
    Ok(value)
}

fn materialize_promoted_markdown_tool_lock(
    repo: &Repository,
    profile: &AdapterProfile,
) -> kio_core::Result<()> {
    let path = repo.kio_dir().join("tool-lock.json");
    let value = promoted_markdown_tool_lock(repo, profile)?;
    let bytes =
        serde_json::to_vec_pretty(&value).map_err(|error| KioError::schema(error.to_string()))?;
    atomic_overwrite_file(&path, &bytes)
}

/// A pipeline can produce its first local output before HEAD exists. Verify
/// the exact accepted refs rather than inferring provenance from configuration
/// or requiring the complete-only batch promotion planner to find an old HEAD.
fn has_accepted_local_pipeline_ref(
    repo: &Repository,
    refs: impl IntoIterator<Item = kio_core::Result<(String, PendingNormalizeRef)>>,
    profile: &AdapterProfile,
) -> kio_core::Result<bool> {
    let mut accepted = false;
    for row in refs {
        let (path, pending) = row?;
        if pending.normalize.tool_profile_hash != profile.tool_profile_hash {
            continue;
        }
        if !repository_scan_policy_allows_file(repo, &path).map_err(pipeline_to_kio)? {
            continue;
        }
        let Ok(current) = hash_repository_scan_input(repo, &path) else {
            continue;
        };
        if current.raw_hash != pending.expected_raw_hash {
            continue;
        }
        ensure_raw_ingest_allowed(repo, &pending.expected_raw_hash)?;
        let instance = load_validated_normalized_instance_with_control(
            repo.kio_dir(),
            &pending.expected_raw_hash,
            &profile.tool_profile_hash,
            pending.normalize.r#gen,
            repo.control(),
        )
        .map_err(pipeline_to_kio)?;
        let manifest_hash = crate::object_persistence::compute_manifest_hash(
            repo,
            &pending.expected_raw_hash,
            &profile.tool_profile_hash,
            pending.normalize.r#gen,
        )?;
        if manifest_hash != pending.normalize.manifest_hash {
            return Err(pipeline_to_kio(PipelineError::corrupt(
                path,
                "local pipeline normalized manifest differs from its pending reference",
            )));
        }
        // Partial output retains accepted Done units. This observation stamps
        // provenance without promoting incomplete tasks or altering their refs.
        accepted |= instance
            .manifest
            .units
            .iter()
            .any(|unit| unit.status == UnitStatus::Done);
    }
    Ok(accepted)
}

/// Reconcile already-Done online results into an ordinary index's single pending
/// snapshot. This prevents the deterministic baseline pass from demoting a
/// previously promoted file and avoids an extra promotion commit.
pub(super) fn apply_online_promotion_to_index(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    normalizes: &DiskRecords<(String, PendingNormalizeRef)>,
) -> kio_core::Result<Option<DiskRecords<(String, PendingNormalizeRef)>>> {
    let plan = plan_selected_markdownize_promotion(adapter_settings, repo)?;
    let produced_local_profile = if let Some(execution) =
        active_local_ocr_execution_with_control(adapter_settings, repo.control())
    {
        let profile = kio_adapter::local_ocr_markdownize::profile_for(execution);
        has_accepted_local_pipeline_ref(repo, normalizes.iter(), &profile)?.then_some(profile)
    } else {
        None
    };
    if let Some(profile) = plan
        .as_ref()
        .map(|plan| &plan.markdown_profile)
        .or(produced_local_profile.as_ref())
    {
        materialize_promoted_markdown_tool_lock(repo, profile)?;
    }
    Ok(plan.map(|plan| plan.normalize_by_path))
}

/// Publish one deterministic promotion commit for all accepted outputs completed
/// by a batch pass. The repository primitive changes only matching HEAD bindings;
/// unrelated deferred working-tree edits are never captured.
pub(super) fn promote_completed_online_markdownize(
    ctx: &crate::invocation::Invocation<'_>,
    repo: &Repository,
) -> kio_core::Result<bool> {
    let adapter_settings = ctx.settings;

    let Some(plan) = plan_online_markdownize_promotion(adapter_settings, repo)? else {
        return Ok(false);
    };
    publish_markdownize_promotion(ctx, repo, plan)
}

pub(super) fn promote_completed_local_markdownize(
    ctx: &crate::invocation::Invocation<'_>,
    repo: &Repository,
) -> kio_core::Result<bool> {
    let Some(plan) = plan_local_markdownize_promotion(ctx.settings, repo)? else {
        return Ok(false);
    };
    publish_markdownize_promotion(ctx, repo, plan)
}

fn publish_markdownize_promotion(
    ctx: &crate::invocation::Invocation<'_>,
    repo: &Repository,
    plan: MarkdownizePromotionPlan,
) -> kio_core::Result<bool> {
    let previous_head = repo
        .head_commit_hash()?
        .ok_or_else(|| KioError::invalid_usage("cannot promote in an unborn scope"))?;
    let staged_tool_lock = promoted_markdown_tool_lock(repo, &plan.markdown_profile)?;
    let mut state = stage_promotion(repo, &previous_head, staged_tool_lock)?;
    let (message, event_message) = promotion_messages(&state)?;
    maybe_inject_promotion_fault("before_head", ctx.control)?;
    let outcome = repo.promote_normalize_refs_with_staged_tool_lock(
        Some(message),
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
    mark_replica_rebuilding_or_log(ctx, repo.kio_dir(), commit_hash);
    state.target_head = Some(commit_hash.clone());
    persist_promotion_state(repo.kio_dir(), &state)?;
    publish_staged_tool_lock(repo.kio_dir(), &state)?;
    maybe_inject_promotion_fault("after_head", ctx.control)?;
    if let Some(commit_hash) = &outcome.commit_hash {
        ctx.control.append_event_log_in_session(
            ctx.locks,
            "KIO-I-COMMIT-CREATED-001",
            event_message,
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
    let (expected_message, _) = promotion_messages(&state)?;
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
        || commit.message != expected_message
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

pub(super) fn finish_pending_online_promotion(
    ctx: &crate::invocation::Invocation<'_>,
    repo: &Repository,
) -> kio_core::Result<()> {
    rebuild_step3_index(ctx, repo)?;
    maybe_inject_promotion_fault("after_index_swap", ctx.control)?;
    clear_promotion_state(repo.kio_dir())
}

#[cfg(test)]
mod tests {
    use kio_core::control::ControlHandle;
    use std::collections::{BTreeMap, BTreeSet};

    use kio_core::scope::Repository;
    use kio_pipeline::markdownize::{
        MarkdownizeMode, NormalizedInstanceManifest, NormalizedUnitManifestEntry,
        NormalizedUnitObject, UnitStatus, persist_normalized_instance,
    };
    use kio_pipeline::prepare::{UnitType, hash_bytes, unit_ref};
    use kio_pipeline::task::{TaskDescriptor, TaskStatus, TaskType, normalized_task_output_ref};

    use super::validated_online_promotions;

    fn canonical_tempdir() -> tempfile::TempDir {
        // Strict store ancestry checks require the resolved macOS temporary root.
        tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap()
    }

    fn done_task(output_ref: String, raw_hash: String) -> TaskDescriptor {
        TaskDescriptor {
            unit_retry_at: Default::default(),
            resource_limit: None,
            task_id: "task_01TEST".to_owned(),
            task_type: TaskType::Markdownize,
            embedding_target: None,
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
        let dir = canonical_tempdir();
        Repository::init(dir.path()).unwrap();
        let kio_dir = dir.path().join(".kio");
        let raw_hash = hash_bytes(b"raw");
        let profile_hash = hash_bytes(b"profile");
        let prepared_hash = hash_bytes(b"prepared");
        let unit_key = "page:1".to_owned();
        let manifest = NormalizedInstanceManifest {
            preparation_provenance: kio_pipeline::markdownize::PreparationProvenance::local(),
            raw_hash: raw_hash.clone(),
            tool_profile_hash: profile_hash.clone(),
            r#gen: 0,
            parent_gen: None,
            run_id: "run_01TEST".to_owned(),
            units: vec![NormalizedUnitManifestEntry {
                resource_limit: None,
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
        let planned = validated_online_promotions(
            &kio_dir,
            std::slice::from_ref(&task),
            &ControlHandle::default(),
        )
        .unwrap();
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].tool_profile_hash, profile_hash);
        assert!(planned[0].bbox_annotation_enabled);

        let mut local = task.clone();
        local.fallback_reason = Some("local_adapter_done".to_owned());
        local.bbox_annotation_enabled = None;
        let repo = Repository::open(dir.path()).unwrap();
        let mut profile = kio_adapter::local_ocr_markdownize::profile_for(
            kio_adapter::local_ocr_markdownize::LocalOcrExecution::Real,
        );
        profile.tool_profile_hash = profile_hash.clone();
        let local_ref = super::validated_local_normalize_ref(&repo, &local, &profile)
            .unwrap()
            .unwrap();
        assert_eq!(local_ref.expected_raw_hash, local.input_hash);
        assert_eq!(local_ref.normalize.tool_profile_hash, profile_hash);
        let lock = super::promoted_markdown_tool_lock(&repo, &profile).unwrap();
        assert_eq!(lock["markdown"]["tool_id"], profile.adapter_id);
        assert_eq!(lock["markdown"]["kind"], "offline_api");
        assert_eq!(
            lock["markdown"]["capabilities"],
            serde_json::json!(profile.capability_flags)
        );
        let mut different = profile.clone();
        different.tool_profile_hash = hash_bytes(b"other-local-profile");
        assert!(
            super::validated_local_normalize_ref(&repo, &local, &different)
                .unwrap()
                .is_none()
        );
        let mut partial_local = local.clone();
        partial_local.status = TaskStatus::Partial;
        assert!(
            super::validated_local_normalize_ref(&repo, &partial_local, &profile)
                .unwrap()
                .is_none()
        );
        let mut missing_local = local.clone();
        missing_local.output_ref =
            normalized_task_output_ref(&missing_local.input_hash, &profile.tool_profile_hash, 1);
        assert!(super::validated_local_normalize_ref(&repo, &missing_local, &profile).is_err());
        local.reservation_id = Some("must-remain-paid".to_owned());
        assert!(super::validated_local_normalize_ref(&repo, &local, &profile).is_err());

        let mut partial = task;
        partial.status = TaskStatus::Partial;
        assert!(
            validated_online_promotions(&kio_dir, &[partial], &ControlHandle::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn rejects_completed_online_instance_without_bbox_policy_stamp() {
        let dir = canonical_tempdir();
        Repository::init(dir.path()).unwrap();
        let kio_dir = dir.path().join(".kio");
        let raw_hash = hash_bytes(b"raw");
        let profile_hash = hash_bytes(b"profile");
        let prepared_hash = hash_bytes(b"prepared");
        let manifest = NormalizedInstanceManifest {
            preparation_provenance: kio_pipeline::markdownize::PreparationProvenance::local(),
            raw_hash: raw_hash.clone(),
            tool_profile_hash: profile_hash.clone(),
            r#gen: 0,
            parent_gen: None,
            run_id: "run_current".to_owned(),
            units: vec![NormalizedUnitManifestEntry {
                resource_limit: None,
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
        assert!(validated_online_promotions(&kio_dir, &[task], &ControlHandle::default()).is_err());
    }
    #[test]
    fn promotion_label_is_bound_to_producer_not_unhashed_execution_annotation() {
        let dir = canonical_tempdir();
        let repo = Repository::init(dir.path()).unwrap();
        let previous_head = hash_bytes(b"previous");
        for (profile, expected_message, wrong_kind) in [
            (
                kio_adapter::catalog::standard_online_markdownize_profile(),
                "online Markdownize promotion",
                "offline_api",
            ),
            (
                kio_adapter::local_ocr_markdownize::profile_for(
                    kio_adapter::local_ocr_markdownize::LocalOcrExecution::Real,
                ),
                "local Markdownize promotion",
                "online_api",
            ),
        ] {
            let lock = super::promoted_markdown_tool_lock(&repo, &profile).unwrap();
            let mut state = super::stage_promotion(&repo, &previous_head, lock).unwrap();
            assert_eq!(
                super::promotion_messages(&state).unwrap().0,
                expected_message
            );
            state.staged_tool_lock["markdown"]["kind"] = serde_json::json!(wrong_kind);
            assert_eq!(
                kio_adapter::tool_lock::tool_lock_hash(&state.staged_tool_lock).unwrap(),
                state.staged_tool_lock_hash,
                "kind annotation is deliberately outside canonical identity"
            );
            // Use the actual journal APIs: this schema-valid annotation flip
            // must not select the opposite route during recovery.
            super::persist_promotion_state(repo.kio_dir(), &state).unwrap();
            let loaded = super::load_promotion_state(repo.kio_dir())
                .unwrap()
                .unwrap();
            assert!(super::promotion_messages(&loaded).is_err());
            let recovery_error = super::recover_pending_online_promotion(&repo).unwrap_err();
            assert_eq!(recovery_error.error_code(), "KIO-E-PROMOTION-RECOVERY-001");
            assert!(
                recovery_error
                    .message()
                    .contains("producer and execution annotation")
            );
            state.staged_tool_lock["markdown"]["tool_id"] =
                serde_json::json!("unsupported_producer");
            assert!(
                super::promotion_messages(&state).is_err(),
                "hash-bound identity tampering must fail before label selection"
            );
        }
    }
    #[test]
    fn configured_local_pipeline_without_actual_refs_does_not_stamp_provenance() {
        let dir = canonical_tempdir();
        let repo = Repository::init(dir.path()).unwrap();
        let profile = kio_adapter::local_ocr_markdownize::profile_for(
            kio_adapter::local_ocr_markdownize::LocalOcrExecution::Real,
        );
        let path = repo.kio_dir().join("tool-lock.json");
        let before = std::fs::read(&path).unwrap();
        assert!(!super::has_accepted_local_pipeline_ref(&repo, [], &profile).unwrap());
        let baseline = BTreeMap::from([(
            "report.pdf".to_owned(),
            kio_core::scope::PendingNormalizeRef {
                expected_raw_hash: hash_bytes(b"raw"),
                normalize: kio_core::dag::NormalizeRef {
                    tool_profile_hash: hash_bytes(b"builtin-profile"),
                    r#gen: 0,
                    manifest_hash: hash_bytes(b"baseline-manifest"),
                },
            },
        )]);
        assert!(
            !super::has_accepted_local_pipeline_ref(&repo, baseline.into_iter().map(Ok), &profile)
                .unwrap()
        );
        assert_eq!(std::fs::read(path).unwrap(), before);
        assert!(
            repo.head_commit_hash().unwrap().is_none(),
            "fixture is an unborn first index"
        );
    }
}
