use crate::egress::SECRETS_TIER_B_HOLD;
use crate::embedding::admission::EMBEDDING_ADAPTER_KIND;
use crate::embedding::executor::EmbeddingTransition;
use crate::embedding::executor::apply_embedding_transitions;
use crate::embedding::executor::embedding_done_transition;
use crate::embedding::executor::embedding_retired_non_live_transition;
use crate::embedding_owners;
use crate::embedding_owners::RetainedEmbeddingChunk;
use crate::historical_reindex::RetainedNormalizedInstance;
use crate::ledger_access::RETIRED_NON_LIVE;
use crate::ledger_access::release_task_charge_if_open;
use crate::ledger_access::task_ledger_key;
use crate::markdownize::batch::retire_online_task;
use crate::markdownize::execution::TaskExecutionFailure;
use crate::object_persistence::index_to_kio;
use crate::object_persistence::pipeline_to_kio;
use crate::task_execution::retry_kind_from_reason;
use crate::task_execution::task_retry_allowed;
use crate::task_execution::task_retry_due;
use kio_adapter::catalog::DeclaredEmbeddingProfile;
use kio_core::KioError;
use kio_core::Result;
use kio_core::control::ControlHandle;
use kio_core::scope::Repository;
use kio_core::scope::new_ulid;
use kio_index::EmbeddingDistance;
use kio_index::EmbeddingModality;
use kio_index::EmbeddingTargetType;
use kio_index::embedding_store;
use kio_pipeline::ledger::LedgerDb;
use kio_pipeline::ledger::ops::AbandonResolution;
use kio_pipeline::ledger::ops::AbandonSelector;
use kio_pipeline::ledger::ops::resolve_abandon_selector;
use kio_pipeline::policy::CurrentPolicyEvaluator;
use kio_pipeline::task::EmbeddingWorkKey;
use kio_pipeline::task::HoldReason;
use kio_pipeline::task::RetryErrorKind;
use kio_pipeline::task::TaskDescriptor;
use kio_pipeline::task::TaskStatus;
use kio_pipeline::task::TaskStore;
use kio_pipeline::task::TaskType;
use kio_pipeline::task::task_can_complete_from_materialized_output;
use kio_pipeline::task::task_can_enter_secret_hold;
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

pub(crate) const EMBEDDING_BATCH_SIZE: usize = 32;

/// Context-qualified unit of task lifecycle work. Raw retained candidates are
/// bound only after the current embedding profile is known.
#[derive(Clone)]
pub(crate) struct EmbeddableChunk {
    retained: RetainedEmbeddingChunk,
    pub(crate) work_key: EmbeddingWorkKey,
}

impl std::ops::Deref for EmbeddableChunk {
    type Target = RetainedEmbeddingChunk;
    fn deref(&self) -> &Self::Target {
        &self.retained
    }
}

impl AsRef<RetainedEmbeddingChunk> for EmbeddableChunk {
    fn as_ref(&self) -> &RetainedEmbeddingChunk {
        &self.retained
    }
}
impl AsRef<RetainedEmbeddingChunk> for RetainedEmbeddingChunk {
    fn as_ref(&self) -> &RetainedEmbeddingChunk {
        self
    }
}

pub(crate) fn bind_embedding_work(
    candidates: Vec<RetainedEmbeddingChunk>,
    profile: &DeclaredEmbeddingProfile,
) -> Result<Vec<EmbeddableChunk>> {
    let mut work = BTreeMap::new();
    for retained in candidates {
        let hash = chunk_embedding_hash(&retained, profile)?;
        let work_key = EmbeddingWorkKey::new(&retained.chunk_id, &hash)
            .ok_or_else(|| KioError::schema("invalid contextual embedding work identity"))?;
        work.entry(work_key.clone())
            .or_insert(EmbeddableChunk { retained, work_key });
    }
    Ok(work.into_values().collect())
}

pub(crate) fn retained_history_chunks(
    conn: &Connection,
    repo: &Repository,
    retained_instances: &[RetainedNormalizedInstance],
    chunking_config_hash: &str,
    current_policy: Option<&CurrentPolicyEvaluator>,
) -> Result<Vec<RetainedEmbeddingChunk>> {
    embedding_owners::collect_retained_embedding_chunks(
        repo,
        conn,
        retained_instances,
        Some(chunking_config_hash),
        current_policy,
    )
}

/// HEAD aliases win over retained history; path order breaks ties. The
/// collector has already authenticated every candidate against its exact
/// pinned normalized unit. Creation provenance is never modified.
pub(crate) fn selected_embedding_contexts(
    candidates: &[RetainedEmbeddingChunk],
    policy: &CurrentPolicyEvaluator,
) -> Result<BTreeMap<String, Option<String>>> {
    policy.revalidate().map_err(pipeline_to_kio)?;
    let mut selected = BTreeMap::<String, &RetainedEmbeddingChunk>::new();
    for candidate in candidates {
        if !policy
            .allows_path(&candidate.raw_path)
            .map_err(pipeline_to_kio)?
        {
            continue;
        }
        selected
            .entry(candidate.chunk_id.clone())
            .and_modify(|old| {
                if (!candidate.is_head_owner, candidate.raw_path.as_bytes())
                    < (!old.is_head_owner, old.raw_path.as_bytes())
                {
                    *old = candidate;
                }
            })
            .or_insert(candidate);
    }
    Ok(selected
        .into_iter()
        .map(|(id, candidate)| {
            (
                id,
                embedding_store::chunk_embedding_context(&candidate.raw_path),
            )
        })
        .collect())
}

/// Generate chunk embeddings for the scope after the SQLite index is rebuilt
/// A batch split into free content-addressed reuse and groups needing a call.
pub(crate) struct EmbedBatchPlan<'a> {
    /// (chunk, existing content-vector bytes) — link `chunk_vec`, no adapter call.
    pub(crate) reuse: Vec<(&'a EmbeddableChunk, Vec<u8>)>,
    /// One adapter item per content identity, fanned out to every member chunk.
    pub(crate) to_send: Vec<EmbeddingSendGroup<'a>>,
}

#[derive(Clone)]
pub(crate) struct EmbeddingSendGroup<'a> {
    pub(crate) embedding_hash: String,
    pub(crate) representative: &'a EmbeddableChunk,
    pub(crate) members: Vec<&'a EmbeddableChunk>,
}

/// Classify a batch into reuse vs. to-send by probing the content-addressed
/// `embeddings` store (CT3-EMBED-006). No writes, no adapter calls.
pub(crate) fn plan_embed_batch<'a>(
    conn: &Connection,
    profile: &DeclaredEmbeddingProfile,
    batch: &'a [EmbeddableChunk],
) -> std::result::Result<EmbedBatchPlan<'a>, TaskExecutionFailure> {
    let mut reuse = Vec::new();
    let mut missing = BTreeMap::<String, Vec<&EmbeddableChunk>>::new();
    for chunk in batch {
        let embedding_hash =
            chunk_embedding_hash(chunk, profile).map_err(|_| TaskExecutionFailure {
                retry_kind: RetryErrorKind::ContractViolation,
                retry_after_ms: None,
            })?;
        match embedding_store::content_vector(conn, &embedding_hash) {
            Ok(Some(bytes)) => reuse.push((chunk, bytes)),
            Ok(None) => missing.entry(embedding_hash).or_default().push(chunk),
            Err(_) => {
                return Err(TaskExecutionFailure {
                    retry_kind: RetryErrorKind::ContractViolation,
                    retry_after_ms: None,
                });
            }
        }
    }
    let to_send = missing
        .into_iter()
        .map(|(embedding_hash, members)| EmbeddingSendGroup {
            representative: members[0],
            embedding_hash,
            members,
        })
        .collect();
    Ok(EmbedBatchPlan { reuse, to_send })
}

/// Retained current-config chunks that have no usable current-profile vector.
pub(crate) fn retained_chunks_without_embedding(
    conn: &Connection,
    candidates: Vec<EmbeddableChunk>,
    profile: &DeclaredEmbeddingProfile,
) -> Result<Vec<EmbeddableChunk>> {
    let mut pending = Vec::new();
    for chunk in candidates {
        let embedding_hash = chunk_embedding_hash(&chunk, profile)?;
        let has_current_profile = embedding_store::content_vector(conn, &embedding_hash)
            .map_err(index_to_kio)?
            .is_some();
        if has_current_profile {
            continue;
        }
        pending.push(chunk);
    }
    Ok(pending)
}

pub(crate) fn validate_embedding_task_bindings(
    task_store: &TaskStore,
    chunks: &[EmbeddableChunk],
) -> Result<()> {
    let by_ref = chunks
        .iter()
        .map(|chunk| (chunk.work_key.output_ref(), chunk))
        .collect::<BTreeMap<_, _>>();
    for task in task_store.all().map_err(pipeline_to_kio)? {
        if task.task_type != TaskType::Embedding {
            continue;
        }
        if let Some(chunk) = by_ref.get(&task.output_ref)
            && (task.input_hash != chunk.text_hash
                || task.changed_unit_keys != [chunk.chunk_id.clone()])
        {
            return Err(KioError::schema(
                "embedding task does not match authenticated contextual work",
            ));
        }
    }
    Ok(())
}

pub(crate) fn embedding_task_output_ref(chunk: &EmbeddableChunk) -> String {
    chunk.work_key.output_ref()
}

pub(crate) struct EmbeddingReconcileContext<'a> {
    pub(crate) active_work_refs: &'a BTreeSet<String>,
    /// Exact current sync ledger identity per live chunk. This includes the
    /// contextualized input and avoids reconstructing a ledger selector from
    /// a task's bare text hash.
    pub(crate) active_embedding_hashes_by_work_ref: &'a BTreeMap<String, String>,
    pub(crate) pending: &'a [EmbeddableChunk],
    pub(crate) now: &'a str,
    /// The active embedding profile's `tool_profile_hash` — one of the ledger
    /// `TaskKey`'s 4 fields (04 §5.4 L768: "input_hash = §5.5 のタスク同一性
    /// キーと同じ組"); this reconcile pass needs it to look up/release a task's
    /// ledger row by key.
    pub(crate) profile_hash: &'a str,
    pub(crate) allow_auth_revive: bool,
}

/// R12-3: complete embedding tasks stranded `Pending`/`Running` by a crash between
/// the per-batch `chunk_vec` commit and the deferred task-store write-back (R11-5).
/// A chunk that already carries its embedding is "live but not in `pending`" — so
/// `live_chunks_without_embedding` never revisits it and no recovery command
/// (index / batch resume/retry / repair) ever reconciles its task, leaving
/// `index_status` reporting phantom pending enrichment permanently. Mark such tasks
/// `Done` (idempotent, no adapter call, no re-charge — the vector is already stored,
/// so search/data are unaffected; only task accounting + the Agent contract heal).
pub(crate) fn reconcile_committed_embedding_tasks(
    repo: &Repository,
    task_store: &TaskStore,
    ledger: Option<&LedgerDb>,
    context: EmbeddingReconcileContext<'_>,
) -> Result<()> {
    let pending_ids: BTreeSet<String> = context
        .pending
        .iter()
        .map(|chunk| chunk.work_key.output_ref())
        .collect();
    let live_ids = context.active_work_refs;
    // R18-1: release the ledger reservation of a NON-LIVE embedding task whose send
    // is now stranded. Once the chunk is edited/deleted (non-live) the task can
    // never be retried, so an open ledger row would eat the embedding
    // per-adapter/device cap for the rest of the month and falsely pause unrelated
    // embeddings — the embedding twin of the markdownize R17-3/R18-2 fix. This
    // in-place pass runs BEFORE the transitions loop below (which then re-reads and
    // skips these now-terminal tasks). Only Pending/Running/Failed STAMPED tasks are
    // eligible: a Done task's stamp is REAL spend (its vector is stored), never a
    // charge to release.
    let reservation_scope_id = repo.scope_identity()?.scope_id;
    #[derive(Clone, Copy)]
    enum ReconcileReservationAction {
        Revive,
        Complete,
        Retire,
    }
    let mut actions = BTreeMap::<String, ReconcileReservationAction>::new();
    for task in task_store.all().map_err(pipeline_to_kio)? {
        if task.task_type != TaskType::Embedding {
            continue;
        }
        // Unbilled local work can reconcile without a central ledger. A
        // retained reservation, however, cannot be discarded until its ledger
        // is available to settle it conservatively.
        if ledger.is_none() && task.reservation_claim().is_some() {
            continue;
        }
        let budget_paused = task.status == TaskStatus::Paused
            && task.fallback_reason.as_deref() == Some("budget_exceeded");
        // QA2 (step4b-contract-tests-p3a.md §A): an auth failure now lands
        // Paused (never Failed) — admit it here the SAME way `budget_paused`
        // is admitted, otherwise a Paused(auth) task on a non-live chunk
        // strands its open reservation forever (the R18-1/R22-3 bug class).
        let auth_paused = task.status == TaskStatus::Paused
            && retry_kind_from_reason(task.fallback_reason.as_deref()) == RetryErrorKind::AuthError;
        if !budget_paused
            && !auth_paused
            && !matches!(
                task.status,
                TaskStatus::Pending | TaskStatus::Running | TaskStatus::Failed
            )
        {
            continue;
        }
        // QA2: Paused-based now (was Failed-based) — an auth failure never
        // lands Failed anymore, so no Failed-auth compatibility path is needed
        // (再 init 方針).
        let auth_revive_candidate = context.allow_auth_revive && auth_paused;
        let Some(work_key) = EmbeddingWorkKey::parse(&task.output_ref) else {
            continue;
        };
        let chunk_id = task.output_ref.as_str();
        let live = live_ids.contains(chunk_id);
        let live_embedded = live && !pending_ids.contains(chunk_id);
        let materialized_completion =
            live_embedded && task_can_complete_from_materialized_output(&task);
        if task.reservation_claim().is_none()
            && !auth_revive_candidate
            && !budget_paused
            && !materialized_completion
        {
            continue;
        }
        let action = if live && !live_embedded {
            if auth_revive_candidate {
                ReconcileReservationAction::Revive
            } else {
                continue;
            }
        } else if live_embedded {
            ReconcileReservationAction::Complete
        } else {
            ReconcileReservationAction::Retire
        };
        let key = match task.reservation_id.as_deref() {
            // A task stamped by an earlier send may name a contextualized
            // input that no longer exists at this path. Its intent token is
            // the durable selector for that old attempt; never substitute the
            // current path's hash (or the bare task text hash) for it.
            Some(intent_token) => match resolve_abandon_selector(
                ledger.expect("a stamped reservation requires its ledger"),
                &AbandonSelector::IntentToken(intent_token.to_owned()),
            )
            .map_err(pipeline_to_kio)?
            {
                AbandonResolution::Found(key) => Some(key),
                // The sync terminal transaction clears the row's intent token.
                // That proves there is no longer an open reservation to
                // release, but must not strand a vector-complete task after a
                // crash before its deferred Done transition.
                AbandonResolution::NotFound => None,
                AbandonResolution::Ambiguous => continue,
            },
            None => {
                let Some(input_hash) = context.active_embedding_hashes_by_work_ref.get(chunk_id)
                else {
                    // A non-live, unstamped task has no durable contextualized
                    // identity. Leave it for explicit stale recovery rather
                    // than guessing from the bare task text hash.
                    continue;
                };
                Some(task_ledger_key(
                    &reservation_scope_id,
                    EMBEDDING_ADAPTER_KIND,
                    input_hash,
                    context.profile_hash,
                ))
            }
        };
        // `tasks.jsonl` is local portable metadata; an intent token in it is
        // only a lookup hint. It must never expand this scope's authority to
        // mutate a row belonging to another scope or adapter.
        if let Some(key) = key {
            if key.scope_id != reservation_scope_id
                || key.adapter_kind != EMBEDDING_ADAPTER_KIND
                || key.input_hash != work_key.embedding_hash()
            {
                continue;
            }
            if let Some(ledger) = ledger {
                release_task_charge_if_open(ledger, &key)?;
            }
        }
        actions.insert(task.task_id.clone(), action);
    }
    task_store
        .update_matching(|task| {
            let Some(action) = actions.get(&task.task_id).copied() else {
                return false;
            };
            task.clear_reservation();
            match action {
                ReconcileReservationAction::Revive => {
                    task.status = TaskStatus::Pending;
                    task.fallback_reason = None;
                    task.hold_reason = None;
                    task.attempts = 0;
                    task.next_retry_at = None;
                    task.heartbeat_at = None;
                }
                ReconcileReservationAction::Complete => {
                    task.status = TaskStatus::Done;
                    task.fallback_reason = Some("embedding_adapter_done".to_owned());
                    task.next_retry_at = None;
                    task.heartbeat_at = None;
                }
                ReconcileReservationAction::Retire => retire_online_task(task),
            }
            true
        })
        .map_err(pipeline_to_kio)?;
    let mut transitions: BTreeMap<String, EmbeddingTransition> = BTreeMap::new();
    for task in task_store.all().map_err(pipeline_to_kio)? {
        if task.task_type != TaskType::Embedding {
            continue;
        }
        // Unbilled local work can reconcile without a central ledger. A
        // retained reservation, however, cannot be discarded until its ledger
        // is available to settle it conservatively.
        if ledger.is_none() && task.reservation_claim().is_some() {
            continue;
        }
        // R22-3: `Paused` (a `secrets_tier_b_hold`, or a sticky `budget_exceeded`) was the
        // ONE status this non-live sweep never visited, so a held chunk that was later
        // edited or deleted left its task Paused forever — `compute_index_status` counts it
        // as pending enrichment, so editing a Tier B file N times accreted N orphan holds
        // and monotonically decayed `enriched_ratio`. Admit Paused here; the `paused` guard
        // below keeps it out of the Done arm (a hold was never sent, so it must never be
        // reported as embedded).
        let paused = matches!(task.status, TaskStatus::Paused);
        if !matches!(
            task.status,
            TaskStatus::Pending | TaskStatus::Running | TaskStatus::Paused
        ) {
            continue;
        }
        let Some(_work_key) = EmbeddingWorkKey::parse(&task.output_ref) else {
            continue;
        };
        let chunk_id = task.output_ref.as_str();
        // A genuinely un-embedded LIVE chunk stays pending (real outstanding work).
        if pending_ids.contains(chunk_id) {
            continue;
        }
        // R15-7: the chunk is NOT live at HEAD — the file was deleted or re-chunked
        // (a new gen), so this task's chunk no longer exists. It can never be driven to
        // completion (`live_chunks_without_embedding` never revisits it), so leaving it
        // Pending/Running permanently pollutes `index_status`/`status.tasks` with phantom
        // pending enrichment. Terminalize it — but REVERSIBLY (R19-3 `retired_non_live`):
        // no adapter call, no re-charge; and if the exact chunk_id later reappears
        // (git revert / restore), the enqueue guard re-creates a fresh task instead of
        // this being a permanent silent hole in vector search.
        if !live_ids.contains(chunk_id) {
            transitions.insert(
                task.output_ref.clone(),
                embedding_retired_non_live_transition(),
            );
            continue;
        }
        // R22-3: a LIVE Paused task keeps its pause. Reaching here means the chunk is live
        // and already has a vector (its content-hash twin was embedded), but a held chunk's
        // text was never sent — marking it `embedding_adapter_done` would both fake the
        // audit trail and let the exact approval release path find nothing to release.
        if paused {
            continue;
        }
        // Live AND already embedded (live but NOT pending) → complete it (R12-3).
        transitions.insert(task.output_ref.clone(), embedding_done_transition());
    }
    // No fresh reservations to stamp on the reconcile path (it charges nothing).
    apply_embedding_transitions(task_store, &transitions, context.now, &BTreeMap::new())?;
    Ok(())
}

/// N1a: enqueue a held `Embedding` task (Paused `secrets_tier_b_hold`) per Tier B
/// chunk that lacks one, so the hold is visible in `kio status` without ever
/// entering the send pipeline. Idempotent — a chunk that already has an embedding
/// task (held or otherwise) is left untouched.
pub(crate) fn hold_secret_embedding_tasks(
    task_store: &TaskStore,
    repo: &Repository,
    ledger: Option<&LedgerDb>,
    profile: &DeclaredEmbeddingProfile,
    held: &[EmbeddableChunk],
    now: &str,
) -> Result<()> {
    if held.is_empty() {
        return Ok(());
    }
    validate_embedding_task_bindings(task_store, held)?;
    let all_tasks = task_store.all().map_err(pipeline_to_kio)?;
    // R22-2: `existing` used to mean "any non-retired task ⇒ already classified", which
    // silently assumed a chunk's secret classification never changes after its task is
    // created. It does: renaming a plain file INTO a Tier B name (`notes.md` →
    // `credentials_backup.md`) re-partitions the chunk into `held` on the very next index
    // (the partition reads the live `te.path`, R20-1), yet this guard skipped the demotion,
    // so the task stayed Pending/`network_opt_in_required` forever while `quarantine.jsonl`
    // recorded a "hold". Nothing was ever sent (the send set is recomputed from content each
    // pass), but the N1a disclosure contract broke and no recovery command converged.
    // Only an ALREADY-HELD task means "correctly classified"; everything else is demoted.
    let already_held = all_tasks
        .iter()
        .filter(|task| task.task_type == TaskType::Embedding)
        .filter(|task| {
            task.status == TaskStatus::Paused
                && task.fallback_reason.as_deref() == Some(SECRETS_TIER_B_HOLD)
        })
        .map(|task| task.output_ref.clone())
        .collect::<BTreeSet<_>>();
    // A `Done` task's vector is real spend that already exists in `embeddings`; demoting it
    // to a hold would fake outstanding work and strand the stored vector. R20-10 keeps the
    // held chunk out of `chunk_vec`, which is the disclosure that matters here.
    let done = all_tasks
        .iter()
        .filter(|task| task.task_type == TaskType::Embedding)
        .filter(|task| task.status == TaskStatus::Done)
        .map(|task| task.output_ref.clone())
        .collect::<BTreeSet<_>>();
    // R21-7: a task retired because its chunk went non-live must NOT block a fresh HOLD
    // when the content-addressed chunk_id reappears under a Tier B name (git revert /
    // restore of the exact bytes). `enqueue_embedding_tasks` (the sendable sibling) got
    // this R19-3/R20-2 revive; the held branch was left behind, so a restored-under-a-
    // secret-name chunk got no hold task — invisible in `index_status` (R20-7 excludes
    // retired_non_live) and re-embeddable only via `--send-secrets`.
    let revivable = all_tasks
        .iter()
        .filter(|task| task.task_type == TaskType::Embedding)
        .filter(|task| task.fallback_reason.as_deref() == Some(RETIRED_NON_LIVE))
        .map(|task| task.output_ref.clone())
        .collect::<BTreeSet<_>>();
    let existing_nonretired = all_tasks
        .iter()
        .filter(|task| task.task_type == TaskType::Embedding)
        .filter(|task| task.fallback_reason.as_deref() != Some(RETIRED_NON_LIVE))
        .map(|task| task.output_ref.clone())
        .collect::<BTreeSet<_>>();
    // A secret hold is a policy overlay, not permission to erase a terminal retry
    // decision. Preserve non-retryable and exhausted failures in place so a
    // hold/unhold cycle cannot turn them back into fresh sendable work.
    let terminal_failed = all_tasks
        .iter()
        .filter(|task| task.task_type == TaskType::Embedding)
        .filter(|task| {
            task.status == TaskStatus::Failed
                && task.fallback_reason.as_deref() != Some(RETIRED_NON_LIVE)
                && !task_retry_allowed(task)
        })
        .map(|task| task.output_ref.clone())
        .collect::<BTreeSet<_>>();
    // Live, non-retired, not-yet-held, not Done → demote in place to a Paused hold.
    let demotable = all_tasks
        .iter()
        .filter(|task| task.task_type == TaskType::Embedding)
        .filter(|task| {
            task_can_enter_secret_hold(task)
                && !already_held.contains(&task.output_ref)
                && !done.contains(&task.output_ref)
                && !terminal_failed.contains(&task.output_ref)
                && task.fallback_reason.as_deref() != Some(RETIRED_NON_LIVE)
        })
        .map(|task| task.output_ref.clone())
        .collect::<BTreeSet<_>>();
    let mut to_revive: BTreeSet<String> = BTreeSet::new();
    // Demoted holds carry the chunk's CURRENT (secret) path, so `kio status` names the file
    // that is actually being held rather than whatever it was called when first indexed.
    let mut to_demote: BTreeMap<String, String> = BTreeMap::new();
    for chunk in held {
        let output_ref = embedding_task_output_ref(chunk);
        if already_held.contains(&output_ref) || done.contains(&output_ref) {
            continue;
        }
        if revivable.contains(&output_ref) {
            to_revive.insert(output_ref);
            continue;
        }
        if terminal_failed.contains(&output_ref) {
            continue;
        }
        if demotable.contains(&output_ref) {
            to_demote.insert(output_ref, chunk.raw_path.clone());
            continue;
        }
        if existing_nonretired.contains(&output_ref) {
            continue;
        }
        let task = TaskDescriptor {
            task_id: format!("task_{}", new_ulid(repo.canonical_root())),
            task_type: TaskType::Embedding,
            mode: None,
            input_path: chunk.raw_path.clone(),
            input_hash: chunk.text_hash.clone(),
            previous_raw_hash: None,
            parent_run_id: None,
            changed_unit_keys: vec![chunk.chunk_id.clone()],
            output_ref,
            unit_keys: None,
            status: TaskStatus::Paused,
            attempts: 0,
            next_retry_at: None,
            deadline: None,
            heartbeat_at: None,
            fallback_reason: Some(SECRETS_TIER_B_HOLD.to_owned()),
            created_at: now.to_owned(),
            bbox_annotation_enabled: None,
            // QA1 (step4b-contract-tests-p3a.md §A): the closed hold_reason
            // enum accompanies every Paused task.
            hold_reason: Some(HoldReason::TierBApproval),
            reserved_usd: None,
            reserved_month: None,
            reservation_id: None,
        };
        task_store.append(&task).map_err(pipeline_to_kio)?;
    }
    // Revive retired-non-live tasks in place to a Paused hold, reusing their slot so no
    // duplicate output_ref is created. Their `reserved_*` were cleared at retirement.
    if !to_revive.is_empty() {
        task_store
            .update_matching(|task| {
                if task.task_type == TaskType::Embedding
                    && task.fallback_reason.as_deref() == Some(RETIRED_NON_LIVE)
                    && to_revive.contains(&task.output_ref)
                {
                    task.status = TaskStatus::Paused;
                    task.fallback_reason = Some(SECRETS_TIER_B_HOLD.to_owned());
                    task.hold_reason = Some(HoldReason::TierBApproval);
                    task.attempts = 0;
                    task.next_retry_at = None;
                    task.heartbeat_at = None;
                    task.clear_reservation();
                    true
                } else {
                    false
                }
            })
            .map_err(pipeline_to_kio)?;
    }
    // R22-2: demote an existing non-held task (Pending / Running / Failed / budget-Paused)
    // whose chunk is now classified secret. The hold takes precedence over budget and
    // network reasons (the same precedence `enqueue_online_placeholder_task` applies at
    // creation). A demoted task's still-open ledger reservation now buys nothing — its
    // send is blocked by the hold — so release it (settles `unknown_settled` at the
    // reservation estimate if a row is still open, a no-op otherwise).
    if !to_demote.is_empty() {
        let reservation_scope_id = repo.scope_identity()?.scope_id;
        for task in &all_tasks {
            if task.task_type == TaskType::Embedding && to_demote.contains_key(&task.output_ref) {
                let key = task_ledger_key(
                    &reservation_scope_id,
                    EMBEDDING_ADAPTER_KIND,
                    &task.input_hash,
                    &profile.profile_hash,
                );
                if let Some(ledger) = ledger {
                    release_task_charge_if_open(ledger, &key)?;
                }
            }
        }
        task_store
            .update_matching(|task| {
                if task.task_type != TaskType::Embedding {
                    return false;
                }
                let Some(current_path) = to_demote.get(&task.output_ref) else {
                    return false;
                };
                // Apply the policy hold even without a ledger, while retaining
                // any existing claim for explicit ledger recovery.
                if ledger.is_some() {
                    task.clear_reservation();
                }
                task.status = TaskStatus::Paused;
                task.fallback_reason = Some(SECRETS_TIER_B_HOLD.to_owned());
                task.hold_reason = Some(HoldReason::TierBApproval);
                task.input_path = current_path.clone();
                task.attempts = 0;
                task.next_retry_at = None;
                task.heartbeat_at = None;
                true
            })
            .map_err(pipeline_to_kio)?;
    }
    Ok(())
}

/// Enqueue one Pending `Embedding` task per pending chunk, skipping chunks that
/// already have any embedding task (idempotent re-index).
pub(crate) fn enqueue_embedding_tasks(
    task_store: &TaskStore,
    repo: &Repository,
    pending: &[EmbeddableChunk],
    online: bool,
    now: &str,
) -> Result<()> {
    validate_embedding_task_bindings(task_store, pending)?;
    let all_tasks = task_store.all().map_err(pipeline_to_kio)?;
    // R22-1: a Paused `secrets_tier_b_hold` must not block this enqueue either. Reaching
    // `enqueue_embedding_tasks` means the chunk landed in `sendable`, i.e. NO live path of
    // it is secret (the R21-1 dedup prefers a secret path, so one live secret path would
    // have kept the survivor in `held`) or the scope is `--send-secrets`-approved. The hold
    // is therefore stale: the file was renamed out of a Tier B name, or its secret twin was
    // deleted. Before this, `existing` swallowed the hold and `embeddable_task_state`
    // (R21-1's defense-in-depth) refused to re-drive it, so the chunk fell out of vector
    // search permanently — recoverable only via `--send-secrets`, which persists a
    // scope-wide approval to send every candidate secret. Release it in place instead.
    let hold_revivable = all_tasks
        .iter()
        .filter(|task| task.task_type == TaskType::Embedding)
        .filter(|task| {
            task.status == TaskStatus::Paused
                && task.fallback_reason.as_deref() == Some(SECRETS_TIER_B_HOLD)
        })
        .map(|task| task.output_ref.clone())
        .collect::<BTreeSet<_>>();
    let existing = all_tasks
        .iter()
        .filter(|task| task.task_type == TaskType::Embedding)
        // R19-3: a task retired because its chunk went non-live must NOT block a fresh
        // enqueue when the content-addressed chunk_id reappears (git revert / restore of
        // the exact bytes). Excluding it here lets a genuinely-live-again chunk be
        // re-embedded instead of being silently stuck out of vector search forever.
        .filter(|task| task.fallback_reason.as_deref() != Some(RETIRED_NON_LIVE))
        .filter(|task| !hold_revivable.contains(&task.output_ref))
        .map(|task| task.output_ref.clone())
        .collect::<BTreeSet<_>>();
    // R20-2: output_refs of retired-non-live tasks eligible to be REVIVED IN PLACE. R19-3
    // let a reappearing chunk enqueue past its retired task, but appending a NEW task left
    // the old retired one behind — TWO tasks sharing one `output_ref`. Because
    // `apply_embedding_transitions` and `reconcile_committed_embedding_tasks` key on
    // `output_ref` (not `task_id`), a later rate_limit send re-stamped BOTH and reconcile
    // then reclaimed the single phantom TWICE — a silent per-adapter cap under-count
    // (fail-open) that accumulated each revert cycle. Reviving the existing retired task in
    // place keeps exactly one task per output_ref, so no double-stamp / double-reclaim.
    let revivable = all_tasks
        .iter()
        .filter(|task| task.task_type == TaskType::Embedding)
        .filter(|task| task.fallback_reason.as_deref() == Some(RETIRED_NON_LIVE))
        .map(|task| task.output_ref.clone())
        .collect::<BTreeSet<_>>();
    let reason = if online {
        "ready_for_online_adapter"
    } else {
        "network_opt_in_required"
    };
    let mut to_revive: BTreeSet<String> = BTreeSet::new();
    // R22-1: released holds, mapped to the chunk's CURRENT (non-secret) path so the task's
    // `input_path` stops naming the file it was held under.
    let mut to_unhold: BTreeMap<String, String> = BTreeMap::new();
    for chunk in pending {
        let output_ref = embedding_task_output_ref(chunk);
        if existing.contains(&output_ref) {
            continue;
        }
        if hold_revivable.contains(&output_ref) {
            to_unhold.insert(output_ref, chunk.raw_path.clone());
            continue;
        }
        if revivable.contains(&output_ref) {
            to_revive.insert(output_ref);
            continue;
        }
        let task = TaskDescriptor {
            task_id: format!("task_{}", new_ulid(repo.canonical_root())),
            task_type: TaskType::Embedding,
            mode: None,
            input_path: chunk.raw_path.clone(),
            input_hash: chunk.text_hash.clone(),
            previous_raw_hash: None,
            parent_run_id: None,
            changed_unit_keys: vec![chunk.chunk_id.clone()],
            output_ref,
            unit_keys: None,
            status: TaskStatus::Pending,
            attempts: 0,
            next_retry_at: None,
            deadline: None,
            heartbeat_at: None,
            fallback_reason: Some(reason.to_owned()),
            created_at: now.to_owned(),
            bbox_annotation_enabled: None,
            hold_reason: None,
            reserved_usd: None,
            reserved_month: None,
            reservation_id: None,
        };
        task_store.append(&task).map_err(pipeline_to_kio)?;
    }
    // Revive retired-non-live tasks in place (Failed -> Pending), reusing their slot so no
    // duplicate output_ref is created. Their `reserved_*` were cleared at retirement, so
    // the revived task carries no phantom reservation.
    if !to_revive.is_empty() {
        task_store
            .update_matching(|task| {
                if task.task_type == TaskType::Embedding
                    && task.fallback_reason.as_deref() == Some(RETIRED_NON_LIVE)
                    && to_revive.contains(&task.output_ref)
                {
                    task.status = TaskStatus::Pending;
                    task.fallback_reason = Some(reason.to_owned());
                    task.hold_reason = None;
                    task.attempts = 0;
                    task.next_retry_at = None;
                    task.heartbeat_at = None;
                    task.clear_reservation();
                    true
                } else {
                    false
                }
            })
            .map_err(pipeline_to_kio)?;
    }
    // R22-1: release stale secrets holds in place (Paused -> Pending). The hold carried no
    // reservation (`main.rs` hold creation stamps none), so there is nothing to reclaim.
    // `embeddable_task_state`'s `SECRETS_TIER_B_HOLD => false` guard stays untouched: a hold
    // is still never re-driven while it IS a hold — it is cleared here only because the
    // content-addressed partition just proved the chunk has no live secret path.
    if !to_unhold.is_empty() {
        task_store
            .update_matching(|task| {
                if task.task_type == TaskType::Embedding
                    && task.status == TaskStatus::Paused
                    && task.fallback_reason.as_deref() == Some(SECRETS_TIER_B_HOLD)
                {
                    let Some(current_path) = to_unhold.get(&task.output_ref) else {
                        return false;
                    };
                    task.status = TaskStatus::Pending;
                    task.fallback_reason = Some(reason.to_owned());
                    task.hold_reason = None;
                    task.input_path = current_path.clone();
                    task.attempts = 0;
                    task.next_retry_at = None;
                    task.heartbeat_at = None;
                    true
                } else {
                    false
                }
            })
            .map_err(pipeline_to_kio)?;
    }
    Ok(())
}

/// L2(ii)/L7 target selection: drop chunks whose embedding task must not run in
/// this pass — a sticky budget-Paused task (unless `override_budget`), a Failed
/// task that is not retry-eligible (unelapsed `next_retry_at`, or non-retryable /
/// attempts exhausted), or (QA3, step4b-contract-tests-p3a.md §A) a rate-limited
/// Pending task whose `next_retry_at` backoff has not yet elapsed. Failed
/// retry-eligible tasks are left in; `batch retry` resets them to Pending before
/// the pass so they flow through normally.
pub(crate) fn filter_embeddable_by_task_state(
    task_store: &TaskStore,
    pending: Vec<EmbeddableChunk>,
    override_budget: bool,
    control: &ControlHandle,
) -> Result<Vec<EmbeddableChunk>> {
    validate_embedding_task_bindings(task_store, &pending)?;
    let tasks = task_store.all().map_err(pipeline_to_kio)?;
    let mut by_ref = BTreeMap::<String, Vec<&TaskDescriptor>>::new();
    for task in tasks
        .iter()
        .filter(|task| task.task_type == TaskType::Embedding)
    {
        by_ref
            .entry(task.output_ref.clone())
            .or_default()
            .push(task);
    }
    Ok(pending
        .into_iter()
        .filter(|chunk| {
            let output_ref = embedding_task_output_ref(chunk);
            match by_ref.get(&output_ref) {
                Some(tasks) => tasks
                    .iter()
                    .filter(|task| task.fallback_reason.as_deref() != Some(RETIRED_NON_LIVE))
                    .all(|task| embeddable_task_state(task, override_budget, control)),
                // No task row yet (should not happen post-enqueue): embed it.
                None => true,
            }
        })
        .collect())
}

/// Whether an embedding task's current state permits embedding its chunk now.
fn embeddable_task_state(
    task: &TaskDescriptor,
    override_budget: bool,
    control: &ControlHandle,
) -> bool {
    match task.status {
        TaskStatus::Paused => match task.fallback_reason.as_deref() {
            // Sticky budget pause (L2 ii): only an explicit override re-includes it.
            Some("budget_exceeded") => override_budget,
            // R21-1: a secrets hold must NEVER be re-driven into the send pipeline by a
            // sendable chunk (e.g. a non-secret content-twin whose JOIN fan-out shares
            // this held task's output_ref) — only `--send-secrets` (which flips the task
            // back to Pending via the exact approval release path may. Defense-in-depth behind
            // the R21-1 JOIN dedup so the hold cannot be bypassed even if a same
            // output_ref collision re-appears from another source.
            Some(SECRETS_TIER_B_HOLD) => false,
            // QA2 (step4b-contract-tests-p3a.md §A): an auth hold is released
            // only by the dedicated revive pre-pass
            // (`reconcile_committed_embedding_tasks`'s `auth_revive_candidate`,
            // gated on `batch resume`'s `allow_auth_revive`) — never by being
            // re-driven directly from this generic sendable-chunk path.
            Some("auth_error") => false,
            // Any other Paused reason is safe to re-drive.
            _ => true,
        },
        // Failed embeddings are owned by `batch retry` (L7): skip unless the
        // backoff has elapsed AND the error is still retryable.
        TaskStatus::Failed => task_retry_due(task, control) && task_retry_allowed(task),
        // QA3 (step4b-contract-tests-p3a.md §A, 04 §876): a rate-limited
        // Pending chunk (`next_retry_at` in the future) must not be re-driven
        // before its backoff elapses — "next_retry_at 未来 … の embedding
        // タスクを持つ chunk は enrichment 対象から除外する". Mirrors the
        // markdownize send-selection loop's own `task_retry_due` guard on its
        // Pending arm. A fresh Pending task (`next_retry_at = None`) is
        // unaffected (`task_retry_due` defaults to due).
        TaskStatus::Pending => task_retry_due(task, control),
        _ => true,
    }
}

/// Content-addressed identity for a contextual chunk vector (03 §8.1).
pub(crate) fn chunk_embedding_hash(
    chunk: &RetainedEmbeddingChunk,
    profile: &DeclaredEmbeddingProfile,
) -> Result<String> {
    // Contextual-embedding addendum (07 §5.3, 2026-07-24): fold the chunk's
    // humanized filename context into the content-addressed identity, matching
    // the context prepended to the adapter input in `send_embed_group`. Two
    // chunks with identical bodies but different filenames therefore get
    // distinct `embedding_hash`es (and distinct vectors), never a shared one.
    let context = embedding_store::chunk_embedding_context(&chunk.raw_path);
    embedding_store::embedding_hash(
        EmbeddingTargetType::Chunk,
        &chunk.text_hash,
        profile.dimensions,
        EmbeddingDistance::Cosine,
        EmbeddingModality::Multimodal,
        &profile.profile_hash,
        context.as_deref(),
    )
    .map_err(index_to_kio)
}
