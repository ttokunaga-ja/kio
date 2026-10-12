use crate::commands::IndexArgs;
use crate::egress::SECRETS_TIER_B_HOLD;
use crate::egress::admit_embedding_transport;
use crate::egress::embedding_online_allowed;
use crate::egress::embedding_secrets_allowed;
use crate::egress::prune_terminal_embedding_secret_holds;
use crate::embedding::transport::EMBEDDING_ADAPTER_KIND;
use crate::embedding::transport::EMBEDDING_BATCH_SIZE;
use crate::embedding::transport::active_embedding_send_lane;
use crate::embedding::transport::chunk_embedding_hash;
use crate::embedding::transport::declared_embedding_profile;
use crate::embedding::transport::effective_invocation_lane;
use crate::embedding::transport::embedding_billed_from_usage;
use crate::embedding::transport::embedding_execution;
use crate::embedding::transport::embedding_is_online;
use crate::embedding::transport::estimate_embedding_cost;
use crate::embedding::transport::filter_embeddable_by_task_state;
use crate::embedding::transport::publish_embedding_object;
use crate::embedding::transport::publish_image_embedding_object;
use crate::embedding::transport::run_embedding_adapter;
use crate::embedding::transport::submit_embedding_batch_jobs;
use crate::embedding_owners;
use crate::embedding_owners::RetainedEmbeddingChunk;
use crate::historical_reindex::RetainedNormalizedInstance;
use crate::historical_reindex::retained_history_instances;
use crate::image_authority;
use crate::index::chunk_ledger::rotate_index_generation_unconditionally;
use crate::index::chunk_ledger::sqlite_path;
use crate::index::rebuild::held_secret_embedding_chunk_ids;
use crate::invocation::Invocation;
use crate::ledger_access::RETIRED_NON_LIVE;
use crate::ledger_access::TaskChargeOutcome;
use crate::ledger_access::budget_cap_config;
use crate::ledger_access::open_ledger_db;
use crate::ledger_access::release_task_charge_if_open;
use crate::ledger_access::reserve_or_reuse_task_charge;
use crate::ledger_access::settle_task_charge_success;
use crate::ledger_access::settle_task_charge_unknown;
use crate::ledger_access::sync_result_unknown_fenced;
use crate::ledger_access::task_ledger_key;
use crate::markdownize::batch::retire_online_task;
use crate::markdownize::execution::TaskExecutionFailure;
use crate::object_persistence::adapter_to_kio;
use crate::object_persistence::index_to_kio;
use crate::object_persistence::pipeline_to_kio;
use crate::search::projection::mark_replica_unavailable_or_log;
use crate::search::projection::publish_in_place_delta;
use crate::search::projection::replica_now_ms;
use crate::search::projection::replica_scope_stamp;
use crate::search::projection::write_through_projection_or_log;
use crate::search::projection::write_through_projection_or_log_for_repo;
use crate::search::query::ensure_snapshot_tree_entries;
use crate::search::query::snapshot_chunking_config_hash;
use crate::search::scope::aggregator_path;
use crate::search::scope::current_embedding_policy;
use crate::settings::user_config_toml_path;
use crate::task_execution::ExecOutcome;
use crate::task_execution::rate_limit_retry_at;
use crate::task_execution::retry_kind_from_reason;
use crate::task_execution::retry_reason;
use crate::task_execution::scheduled_retry_at;
use crate::task_execution::task_retry_allowed;
use kio_adapter::catalog::DeclaredEmbeddingProfile;
use kio_adapter::catalog::EmbeddingExecution;
use kio_adapter::catalog::embedding_adapter_profile_with_control;
use kio_adapter::tool_lock::AdapterRuntimeSettings;
use kio_adapter::traits::PreferredRequestKind;
use kio_adapter::types::EmbeddingContent;
use kio_adapter::types::EmbeddingInputType;
use kio_adapter::types::EmbeddingItem;
use kio_adapter::types::ExecutionMode;
use kio_adapter::types::MAX_EMBEDDING_ITEM_BYTES;
use kio_core::KioError;
use kio_core::Result;
use kio_core::cas::ContentObjectKind;
use kio_core::control::ControlHandle;
use kio_core::scope::Repository;
use kio_core::scope::new_ulid;
use kio_index::EmbeddingDistance;
use kio_index::EmbeddingModality;
use kio_index::EmbeddingTargetType;
use kio_index::embedding_store;
use kio_index::embedding_store::f32_to_le_bytes;
use kio_index::fts::ExistingSourceIndexOpenMode;
use kio_index::fts::FtsSchemaConfig;
use kio_index::fts::FtsTokenizer;
use kio_index::fts::open_existing_source_index_connection;
use kio_pipeline::budget::read_budget_policy;
use kio_pipeline::budget::utc_month;
use kio_pipeline::ledger::BatchState;
use kio_pipeline::ledger::LedgerDb;
use kio_pipeline::ledger::Outcome;
use kio_pipeline::ledger::RequestKind;
use kio_pipeline::ledger::TaskKey as LedgerTaskKey;
use kio_pipeline::ledger::ops::AbandonResolution;
use kio_pipeline::ledger::ops::AbandonSelector;
use kio_pipeline::ledger::ops::BilledAmount;
use kio_pipeline::ledger::ops::TerminalWrite;
use kio_pipeline::ledger::ops::get_batch_request;
use kio_pipeline::ledger::ops::resolve_abandon_selector;
use kio_pipeline::ledger::ops::terminal_transaction;
use kio_pipeline::policy::CurrentPolicyEvaluator;
use kio_pipeline::task::EmbeddingWorkKey;
use kio_pipeline::task::HoldReason;
use kio_pipeline::task::RetryErrorKind;
use kio_pipeline::task::TaskDescriptor;
use kio_pipeline::task::TaskStatus;
use kio_pipeline::task::TaskStore;
use kio_pipeline::task::TaskType;
use kio_pipeline::task::hold_reason_for_reason;
use kio_pipeline::task::retry_policy;
use kio_pipeline::task::task_can_complete_from_materialized_output;
use kio_pipeline::task::task_can_enter_secret_hold;
use kio_search::object_uri::extract_related_images;
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

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
/// (04 §4.3 / 07 §5.3). Enqueues one `TaskType::Embedding` task per pending
/// `(chunk_id, embedding_hash)` contextual work item,
/// then — if the online opt-in and budget allow — embeds them (batched), writing
/// the `embeddings` rows (source of truth) and `chunk_vec` (derived KNN copy),
/// charging the cost ledger under `adapter_kind="embedding"`. Offline leaves tasks
/// Pending (surfaced by `index_status`). No-op when no embedding adapter is
/// configured (keeps the default index path unchanged).
pub(crate) fn generate_scope_embeddings(
    ctx: &Invocation<'_>,
    repo: &Repository,
    args: &IndexArgs,
) -> Result<ExecOutcome> {
    let adapter_settings = ctx.settings;

    // Embedding online opt-in is the embedding adapter's own (L4), not a
    // ride-along on the markdownize approval. `--offline` forces enqueue-only;
    // N7: `--online` now reaches the embedding adapter too (was ignored).
    let online =
        embedding_online_allowed(adapter_settings, repo, args.offline, args.online, args.yes)?;
    // R11-2: return the enrichment outcome (was discarded) so `run_index` can disclose
    // it and raise the exit code on auth/budget-pause.
    run_embedding_enrichment(ctx, repo, online, false, false)
}

/// Core enrichment pass shared by `kio index` (inline) and `kio batch
/// resume/retry`. Without the resume path, embedding tasks left Pending by a
/// rate limit could never complete (`batch resume` only executed Markdownize
/// tasks). Returns the chunks embedded (executed) and failed this pass (R9-7).
pub(crate) fn run_embedding_enrichment(
    ctx: &Invocation<'_>,
    repo: &Repository,
    online: bool,
    override_budget: bool,
    allow_auth_revive: bool,
) -> Result<ExecOutcome> {
    let adapter_settings = ctx.settings;

    run_embedding_enrichment_for_instances(
        Invocation {
            settings: adapter_settings,
            context: ctx.context,
            control: ctx.control,
            locks: ctx.locks,
        },
        repo,
        online,
        override_budget,
        allow_auth_revive,
        None,
        None,
    )
}

/// Historical reindex drives only the selected snapshot's embedding work. In
/// particular it must not reconcile/retire task rows belonging to non-selected
/// history merely because they are absent from this deliberately narrow set.
pub(crate) fn run_historical_embedding_enrichment(
    ctx: &Invocation<'_>,
    repo: &Repository,
    online: bool,
    selected_instances: &[RetainedNormalizedInstance],
    selected_tree_config_hash: &str,
) -> Result<ExecOutcome> {
    let adapter_settings = ctx.settings;

    run_embedding_enrichment_for_instances(
        Invocation {
            settings: adapter_settings,
            context: ctx.context,
            control: ctx.control,
            locks: ctx.locks,
        },
        repo,
        online,
        false,
        false,
        Some(selected_instances),
        Some(selected_tree_config_hash),
    )
}

fn run_embedding_enrichment_for_instances(
    invocation: Invocation<'_>,
    repo: &Repository,
    online: bool,
    override_budget: bool,
    allow_auth_revive: bool,
    selected_instances: Option<&[RetainedNormalizedInstance]>,
    selected_tree_config_hash: Option<&str>,
) -> Result<ExecOutcome> {
    let ctx = &invocation;
    let adapter_settings = invocation.settings;

    let reconcile_scope_tasks = selected_instances.is_none();
    let Some(execution) = embedding_execution(adapter_settings, invocation.control) else {
        return Ok(ExecOutcome::default());
    };
    let profile = declared_embedding_profile(execution);
    // Non-multimodal is rejected at materialize_tool_lock; never reach embed here.
    if profile.modality != "multimodal" {
        return Ok(ExecOutcome::default());
    }
    // Local computation and enqueue-only passes do not own billing mutations.
    // Reconciliation below still completes unbilled materialized tasks; stamped
    // reservations stay retained until an online recovery has ledger authority.
    let ledger = if online && embedding_is_online(execution) {
        Some(open_ledger_db(invocation.control)?)
    } else {
        None
    };
    let db_path = sqlite_path(repo.kio_dir());
    if !db_path.exists() {
        return Ok(ExecOutcome::default());
    }
    let conn = open_existing_source_index_connection(
        &db_path,
        ExistingSourceIndexOpenMode::ReadWrite,
        &FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        },
    )
    .map_err(index_to_kio)?;
    let Some(head) = repo.head_commit_hash()? else {
        return Ok(ExecOutcome::default());
    };
    // Ordinary enrichment selects only the association bound by immutable
    // HEAD. A historical caller supplies its explicitly selected tree's hash;
    // neither path may consult mutable config.toml.
    let chunking_config_hash = match selected_tree_config_hash {
        Some(hash) => hash.to_owned(),
        None => snapshot_chunking_config_hash(repo, &head)?,
    };
    let retained_instances_owned;
    let retained_instances = if let Some(selected) = selected_instances {
        selected
    } else {
        // A bare `kio snapshot` can advance HEAD without projecting source
        // `tree_entries`. Enrichment is writer work, so materialize that source
        // cache relation here before its live-chunk JOIN.
        ensure_snapshot_tree_entries(repo, &conn, &head)?;
        retained_instances_owned = retained_history_instances(repo, &head)?;
        &retained_instances_owned
    };
    let current_policy = current_embedding_policy(repo)?;
    let retained_chunks = retained_history_chunks(
        &conn,
        repo,
        retained_instances,
        &chunking_config_hash,
        Some(&current_policy),
    )?;
    let projection_instances_owned;
    let projection_instances = if selected_instances.is_some() {
        projection_instances_owned = retained_history_instances(repo, &head)?;
        &projection_instances_owned
    } else {
        retained_instances
    };
    let projection_chunks = embedding_owners::collect_retained_embedding_chunks(
        repo,
        &conn,
        projection_instances,
        None,
        Some(&current_policy),
    )?;
    let selected_contexts = selected_embedding_contexts(&projection_chunks, &current_policy)?;
    let projection_chunks = bind_embedding_work(projection_chunks, &profile)?;
    let retained_chunks = bind_embedding_work(retained_chunks, &profile)?;
    let active_work_refs = retained_chunks
        .iter()
        .map(|chunk| chunk.work_key.output_ref())
        .collect::<BTreeSet<_>>();
    // The online sync ledger identifies a provider call by the exact
    // contextualized embedding input, not the task's bare text hash. Keep the
    // same identity available to crash reconciliation, where an embedded
    // chunk is intentionally absent from `pending`.
    let active_embedding_hashes_by_work_ref = retained_chunks
        .iter()
        .map(|chunk| {
            (
                chunk.work_key.output_ref(),
                chunk.work_key.embedding_hash().to_owned(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let task_store = TaskStore::new_with_control(repo.kio_dir(), repo.control().clone());
    validate_embedding_task_bindings(&task_store, &retained_chunks)?;
    let secrets_approved = embedding_secrets_allowed(adapter_settings, repo, &profile)?;
    let mut pending = retained_chunks_without_embedding(&conn, retained_chunks.clone(), &profile)?;
    let existing_tasks = task_store.all().map_err(pipeline_to_kio)?;
    // Fresh bookkeeping must never revive, complete, or restamp an existing
    // task (including retired records); those retain their recovery path.
    let all_task_refs = existing_tasks
        .iter()
        .map(|task| task.output_ref.as_str())
        .collect::<BTreeSet<_>>();
    let existing_task_refs = existing_tasks
        .iter()
        .filter(|task| {
            task.task_type == TaskType::Embedding
                && task.fallback_reason.as_deref() != Some(RETIRED_NON_LIVE)
        })
        .map(|task| task.output_ref.as_str())
        .collect::<BTreeSet<_>>();
    let mut pending_keys = pending
        .iter()
        .map(|chunk| chunk.work_key.clone())
        .collect::<BTreeSet<_>>();
    let mut fresh_cached = Vec::new();
    for chunk in &retained_chunks {
        let output_ref = chunk.work_key.output_ref();
        let secret_hold = secret_embedding_hold_required(
            execution.execution_mode(),
            secrets_approved,
            chunk.requires_secret_approval,
        );
        if !pending_keys.contains(&chunk.work_key)
            && !all_task_refs.contains(output_ref.as_str())
            && !secret_hold
        {
            // The exact contextual vector/association is already materialized.
            // A new chunk identity needs a Done record, not another execution.
            fresh_cached.push(chunk.clone());
        } else if (!existing_task_refs.contains(output_ref.as_str()) || secret_hold)
            && pending_keys.insert(chunk.work_key.clone())
        {
            // Cache presence never authorizes a currently secret-tainted alias.
            pending.push(chunk.clone());
        }
    }
    let now = invocation.control.now_utc_seconds();
    if !fresh_cached.is_empty() {
        current_policy.revalidate().map_err(pipeline_to_kio)?;
        enqueue_embedding_tasks(&task_store, repo, &fresh_cached, online, &now)?;
        let mut transitions = BTreeMap::new();
        record_embedding_transitions(&mut transitions, &fresh_cached, embedding_done_transition());
        // Only these genuinely fresh, selected refs are completed. In
        // particular, a historical pass cannot retire unrelated scope work.
        apply_embedding_transitions(&task_store, &transitions, &now, &BTreeMap::new())?;
    }
    // R12-3: reconcile task accounting for chunks that ARE embedded but whose task a
    // crash stranded Pending/Running (chunk_vec committed per batch, the task Done
    // write-back deferred to after the loop — R11-5). Must run BEFORE the
    // empty-`pending` early return: the r12k crash left every chunk embedded with 64
    // tasks still Pending, so `pending` is empty yet index_status reported phantom
    // pending enrichment forever. Idempotent; no adapter call, no re-charge.
    if reconcile_scope_tasks {
        reconcile_committed_embedding_tasks(
            repo,
            &task_store,
            ledger.as_ref(),
            EmbeddingReconcileContext {
                active_work_refs: &active_work_refs,
                active_embedding_hashes_by_work_ref: &active_embedding_hashes_by_work_ref,
                pending: &pending,
                now: &now,
                profile_hash: &profile.profile_hash,
                allow_auth_revive,
            },
        )?;
    }
    // Image objects are embedded on their own schedule, before the chunk
    // early-return below: a corpus can have every chunk embedded and still have
    // images outstanding — the first index after an image-capable adapter is
    // configured is exactly that case. Counted separately from `executed`,
    // which is a chunk-member count the task accounting is keyed to.
    let embedding_context = EmbeddingExecutionContext {
        repo,
        execution,
        profile: &profile,
        current_policy: &current_policy,
        projection: EmbeddingProjection {
            repo,
            policy: &current_policy,
            selected_contexts: &selected_contexts,
            invalidated: std::cell::Cell::new(false),
        },
    };
    let mut projection_held = BTreeSet::new();
    if secret_embedding_hold_required(execution.execution_mode(), secrets_approved, true) {
        projection_held.extend(held_secret_embedding_chunk_ids(
            repo.kio_dir(),
            repo.control(),
        )?);
        projection_held.extend(
            projection_chunks
                .iter()
                .filter(|chunk| chunk.requires_secret_approval)
                .map(|chunk| chunk.chunk_id.clone()),
        );
    }
    refresh_embedding_projection(
        ctx,
        repo,
        &conn,
        &projection_chunks,
        &profile,
        &embedding_context.projection,
        &projection_held,
    )?;
    run_image_embedding_enrichment(
        Invocation {
            settings: adapter_settings,
            context: ctx.context,
            control: ctx.control,
            locks: ctx.locks,
        },
        &embedding_context,
        &conn,
        retained_instances,
        &retained_chunks,
        secrets_approved,
        online,
    )?;
    prune_terminal_embedding_secret_holds(repo)?;
    if pending.is_empty() {
        return Ok(ExecOutcome::default());
    }
    // N1a: partition off chunks whose raw file is a Tier B (candidate-secret)
    // document without a `--send-secrets` approval. Their embedding tasks are held
    // (Paused `secrets_tier_b_hold`, visible in `kio status`) and never enter the
    // send pipeline, so `index --online` / `batch resume` cannot ship their text
    // to the embedding API. Approval moves them back into `sendable`.
    let (held, sendable): (Vec<EmbeddableChunk>, Vec<EmbeddableChunk>) =
        pending.into_iter().partition(|chunk| {
            // R19-1: hold ANY secret (Tier B, or a lifted Tier A) from the embedding
            // API unless `--send-secrets`. Non-lifted Tier A never produces chunks
            // (excluded at ingest), so `.is_some()` newly gates only lifted Tier A.
            secret_embedding_hold_required(
                execution.execution_mode(),
                secrets_approved,
                chunk.requires_secret_approval,
            )
        });
    hold_secret_embedding_tasks(&task_store, repo, ledger.as_ref(), &profile, &held, &now)?;
    if sendable.is_empty() {
        return Ok(ExecOutcome::default());
    }
    enqueue_embedding_tasks(&task_store, repo, &sendable, online, &now)?;
    if !online {
        // Offline: tasks stay Pending; `index_status` reports them (05 §1.7).
        return Ok(ExecOutcome::default());
    }

    // L2(ii)/L7: skip chunks whose embedding task is sticky budget-Paused (unless
    // override) or a Failed task that is not retry-eligible (unelapsed backoff or
    // non-retryable) — the same lifecycle semantics markdownize already honors.
    let embeddable = filter_embeddable_by_task_state(
        &task_store,
        sendable,
        override_budget,
        invocation.control,
    )?;
    if embeddable.is_empty() {
        return Ok(ExecOutcome::default());
    }

    let mut outcome = ExecOutcome::default();
    let budget_caps = read_budget_policy(
        user_config_toml_path(invocation.control),
        repo.kio_dir().join("config.toml"),
    )
    .map_err(pipeline_to_kio)?;
    let month = utc_month(&now);
    let scope_id = repo.scope_identity()?.scope_id;

    // R11-5: accumulate every chunk's task-store transition in memory and write it
    // back in ONE `update_matching` after the loop, instead of a full all()+
    // replace_all per 32-chunk batch. The per-batch form cost O(T) each × T/32
    // batches = O(T²/32), turning a few-thousand-chunk initial embedding into a
    // multi-minute hang.
    //
    // R17-7: the reuse-based crash safety here is precise about WHICH crash window it
    // covers. A crash AFTER `send_embed_group` COMPLETES but before this deferred
    // write-back is fully absorbed: `send_embed_group` already wrote the embeddings
    // row + chunk_vec and F8 already reserved the charge, so re-driving the chunk hits
    // the free content-addressed reuse path (§5.5: text_hash hit → no API call, no
    // re-charge), no unrecorded completion is double-billed, and the per-chunk map
    // keeps a reuse "done" from being contaminated by a sibling send "failed" (L6).
    // This does NOT extend to a crash INTERNAL to `send_embed_group`, in the narrow
    // window AFTER the API bills but BEFORE the embeddings row / chunk_vec commit:
    // there the embeddings are unwritten, so §5.5 reuse misses and the chunk re-enters
    // `to_send`, yet its stale RateLimit/Quota `fallback_reason` makes
    // `reservation_covers_resend` skip the re-charge — so the resend double-bills the
    // API while the reservation stays at 1× = a BOUNDED per-chunk under-charge. That is
    // exactly the triple-fault R16 DEFERRED (window = bill→chunk_vec commit; damage =
    // one chunk's cost), left standing on purpose: closing it would need a per-chunk
    // pre-send persisted marker, re-introducing R11-5's O(T²) full-tasks.jsonl write.
    // The asymmetry with markdownize is deliberate: markdownize persists status=Running
    // + `fallback_reason=None` BEFORE its send, so a crash makes it re-reserve; embedding
    // keeps the per-batch write-back (O(T²) avoidance) and instead absorbs the NORMAL
    // (post-completion) crash via content-addressed reuse. R11-2: the loop also tallies
    // paused / auth / failed outcomes.
    // 07 §5.3 の 2026-07-24 訂正: the Batch lane is the default (half rate). It
    // SUBMITS and returns; results are collected by `poll_batch_embedding_jobs`
    // from `kio batch resume` or any later write command. `--realtime` (and a
    // `[adapter] lane = "realtime"` config) selects the synchronous path below,
    // which completes inside this invocation at double the rate.
    // 05 §1.8 write-through. Accumulated across every batch and flushed once
    // per exit path: the replica is a separate database, so folding the write
    // into the per-group loop would cost one transaction per group (2,321 of
    // them in the dogfood fixture) to record a set the pass already knows.
    //
    // The generation is read BEFORE the first vector lands: it names the state
    // the delta is a change TO, which is the state the replica has to already
    // be in for the delta to complete it (R25-3). Nothing rotates during the
    // loop, so one read at the top is the whole answer.
    let replica_before = replica_scope_stamp(repo.kio_dir()).map(|(_, generation)| generation);
    let mut replica = kio_index::aggregator::ScopeDelta::default();
    // Provider Batch is an online transport. Local embeddings must take the
    // synchronous local path even when Batch is the invocation default.
    if embedding_is_online(execution) && effective_invocation_lane() == PreferredRequestKind::Batch
    {
        let mut batch_transitions: BTreeMap<String, EmbeddingTransition> = BTreeMap::new();
        if let Some(submitted) = submit_embedding_batch_jobs(
            ctx,
            repo,
            execution,
            &conn,
            ledger
                .as_ref()
                .expect("online batch embedding requires an initialized ledger"),
            &profile,
            &embeddable,
            &scope_id,
            &budget_caps,
            override_budget,
            &current_policy,
            &embedding_context.projection,
            &mut batch_transitions,
            &mut replica,
        )? {
            // Submitted members stay Pending on purpose (no transition): the
            // job is in flight and `index_status` must keep reporting them
            // until collect.
            let no_reservations: BTreeMap<String, (f64, String, String)> = BTreeMap::new();
            apply_embedding_transitions(&task_store, &batch_transitions, &now, &no_reservations)?;
            // Submitting sends nothing to the index, but the reuse links it
            // settled on the way past did — this lane's early return must carry
            // them too, rotation included: a content-addressed reuse hit writes
            // `chunk_vec` with no adapter call and no rebuild, and a vector
            // appearing is exactly the kind of change LC25 retires cursors for.
            current_policy.revalidate().map_err(pipeline_to_kio)?;
            publish_in_place_delta(ctx, repo.kio_dir(), replica_before.as_deref(), &replica)?;
            return Ok(submitted);
        }
        // Lane unavailable — fall through to the synchronous loop below, which
        // prices itself at the sync rate (`active_embedding_send_lane`).
    }

    let mut transitions: BTreeMap<String, EmbeddingTransition> = BTreeMap::new();
    // R18-1: per-chunk FRESH F8 reservation `(usd, ledger_month)` for the freshly-charged
    // chunks this pass, stamped in the single end-of-pass write-back so a later non-live
    // reclaim (R18-1) can cancel the exact phantom.
    let mut reserved_by_ref: BTreeMap<String, (f64, String, String)> = BTreeMap::new();

    // R16-7: embedding tasks whose PREVIOUS recorded failure was a non-billable
    // rejection (RateLimit / QuotaExceeded — refused before the backend processes or
    // bills the request). Their earlier F8 reservation already covers this resend, so
    // the per-batch charge below excludes their chars. Without this a rate-limited
    // chunk (RateLimit = unbounded retries) re-reserves its chars on every `batch
    // retry`, and one logical enrichment bills N times, exhausting the device month cap
    // and falsely pausing unrelated tasks (R15-2's harm on the retry path). Loaded once
    // at pass start (transitions are written back only after the loop), so it reflects
    // the PREVIOUS pass's outcome — exactly the "prior failure" the gate needs.
    //
    // Fresh / NetworkError / crash-stranded chunks are absent from this set and charge
    // afresh: a NetworkError send may have been billed server-side, and a crash mid-send
    // leaves the task Pending with its embedding uncommitted, so re-driving re-reserves
    // conservatively (bounded by max_attempts; a succeeded-but-uncommitted chunk instead
    // re-drives through §5.5 content-addressed reuse with NO re-charge). Reserving once
    // for the task's lifetime is deliberately NOT done — it would let real spend exceed
    // the reserved cap on server-side-billed retries (the R15-5 silent cap bypass).
    let embedding_tasks_by_ref = task_store
        .all()
        .map_err(pipeline_to_kio)?
        .into_iter()
        .filter(|task| task.task_type == TaskType::Embedding)
        .map(|task| (task.output_ref.clone(), task))
        .collect::<BTreeMap<_, _>>();

    'embedding_batches: for batch in embeddable.chunks(EMBEDDING_BATCH_SIZE) {
        // Split content-addressed reuse (no API call, free) from chunks that
        // require a live adapter call (CT3-EMBED-006).
        let plan = match plan_embed_batch(&conn, &profile, batch) {
            Ok(plan) => plan,
            Err(failure) => {
                record_embedding_transitions(
                    &mut transitions,
                    batch.iter(),
                    embedding_fail_transition(failure.retry_kind),
                );
                count_embedding_failure(&mut outcome, failure.retry_kind, batch.len());
                break;
            }
        };
        // L6: reuse links are free and always succeed → link + complete them up
        // front so an API failure on the *sent* portion can never contaminate an
        // already-materialized (chunk_vec written) chunk into a stuck Failed task.
        if !plan.reuse.is_empty() {
            match link_reused_chunks(
                &conn,
                &profile,
                &plan.reuse,
                &embedding_context.projection,
                &mut replica,
            ) {
                Ok(()) => {
                    record_embedding_transitions(
                        &mut transitions,
                        plan.reuse.iter().map(|(chunk, _)| *chunk),
                        embedding_done_transition(),
                    );
                    outcome.executed += plan.reuse.len();
                }
                Err(failure) => {
                    record_embedding_transitions(
                        &mut transitions,
                        plan.reuse.iter().map(|(chunk, _)| *chunk),
                        embedding_fail_transition(failure.retry_kind),
                    );
                    count_embedding_failure(&mut outcome, failure.retry_kind, plan.reuse.len());
                    break 'embedding_batches;
                }
            }
        }
        if plan.to_send.is_empty() {
            continue;
        }
        // Each `to_send` GROUP (one deduplicated content identity —
        // `embedding_hash`, fanned out to every member chunk sharing that exact
        // contextualized input) is ONE ledger reservation, keyed by that exact
        // `embedding_hash` — never by a representative task's bare `text_hash`:
        // the latter can collide across distinct filename contexts. A group's
        // members share ONE estimate
        // (`group.representative.text.chars().count()`). The reservation itself
        // is intentionally delayed until the group is about to make its one
        // provider call, so an earlier failure cannot strand rows for groups
        // that never left Kio.
        struct GroupCharge {
            key: LedgerTaskKey,
            candidate_usd: f64,
        }
        let mut charge_by_group: Vec<GroupCharge> = Vec::with_capacity(plan.to_send.len());
        for group in &plan.to_send {
            let output_ref = embedding_task_output_ref(group.representative);
            if !embedding_tasks_by_ref.contains_key(&output_ref) {
                return Err(KioError::schema(
                    "embedding send has no matching task reservation owner",
                ));
            }
            let key = task_ledger_key(
                &scope_id,
                EMBEDDING_ADAPTER_KIND,
                &group.embedding_hash,
                &profile.profile_hash,
            );
            // R24 (3 系統一致): price the string actually sent. The send below
            // contextualizes the chunk text, so estimating the bare text
            // undercounts by the context prefix — and `usage: None` means the
            // shortfall is never corrected at settle time (QA17).
            let candidate_usd = estimate_embedding_cost(
                adapter_settings,
                &embedding_store::contextualized_embedding_input(
                    embedding_store::chunk_embedding_context(&group.representative.raw_path)
                        .as_deref(),
                    &group.representative.text,
                ),
                active_embedding_send_lane(),
            );
            // Reservation and admission deliberately happen in the send loop
            // below, immediately before this exact group reaches the provider.
            // A later group must never reserve merely because an earlier one is
            // about to be sent.
            charge_by_group.push(GroupCharge { key, candidate_usd });
        }
        // One HTTP call per group, so this row's charge is the token count the
        // provider reported for THIS row. `:batchEmbedContents` bills and
        // reports per CALL, so any call spanning several rows can only be split
        // by guesswork; the Batch lane avoids that by making the job the task
        // (07 §5.5), and the sync lane avoids it by making the call the row.
        // The realtime lane is opt-in and never the default (2026-07-25
        // ruling), so its extra round trips buy exact per-row billing at a cost
        // no default path pays.
        for (index, charge) in charge_by_group.iter().enumerate() {
            let group = &plan.to_send[index];
            let (intent_token, reserved_usd) = if embedding_is_online(execution) {
                let ledger = ledger
                    .as_ref()
                    .expect("online embedding requires an initialized ledger");
                let existing = get_batch_request(ledger, &charge.key).map_err(pipeline_to_kio)?;
                if sync_result_unknown_fenced(existing.as_ref()) {
                    record_embedding_transitions(
                        &mut transitions,
                        group.members.iter().copied(),
                        embedding_fail_transition(RetryErrorKind::ResultUnknown),
                    );
                    count_embedding_failure(
                        &mut outcome,
                        RetryErrorKind::ResultUnknown,
                        group.members.len(),
                    );
                    continue;
                }
                if existing.as_ref().is_some_and(|row| row.state.is_inflight()) {
                    if existing
                        .as_ref()
                        .is_some_and(|row| row.request_kind != RequestKind::Sync)
                    {
                        // A batch-lane row has provider-side recovery/poll
                        // authority. A realtime fallback must not steal or
                        // resend it.
                        outcome.inflight += group.members.len();
                        continue;
                    }
                    // A prior process may have sent this group before dying.
                    // Settle its unqueryable result and fence it instead of
                    // reusing that reservation for another provider request.
                    release_task_charge_if_open(ledger, &charge.key)?;
                    record_embedding_transitions(
                        &mut transitions,
                        group.members.iter().copied(),
                        embedding_fail_transition(RetryErrorKind::ResultUnknown),
                    );
                    count_embedding_failure(
                        &mut outcome,
                        RetryErrorKind::ResultUnknown,
                        group.members.len(),
                    );
                    continue;
                }
                validate_embedding_aliases(
                    &current_policy,
                    &profile,
                    group.members.iter().copied(),
                )
                .map_err(|_| {
                    KioError::schema("embedding owner policy changed before reservation")
                })?;
                admit_embedding_transport(
                    adapter_settings,
                    repo,
                    execution,
                    &profile,
                    &current_policy,
                    group
                        .members
                        .iter()
                        .any(|chunk| chunk.requires_secret_approval),
                )?;
                let caps = budget_cap_config(&budget_caps, &scope_id, EMBEDDING_ADAPTER_KIND);
                let bypass_cap_denial = override_budget || !budget_caps.hard_stop;
                match reserve_or_reuse_task_charge(
                    ctx,
                    ledger,
                    &charge.key,
                    charge.candidate_usd,
                    &caps,
                    bypass_cap_denial,
                    RequestKind::Sync,
                )? {
                    TaskChargeOutcome::BudgetExceeded => {
                        record_embedding_transitions(
                            &mut transitions,
                            group.members.iter().copied(),
                            embedding_pause_transition(),
                        );
                        outcome.paused += group.members.len();
                        continue;
                    }
                    TaskChargeOutcome::Reserved {
                        intent_token,
                        estimated_usd,
                    }
                    | TaskChargeOutcome::Reused {
                        intent_token,
                        estimated_usd,
                    } => (Some(intent_token), estimated_usd),
                }
            } else {
                (None, 0.0)
            };
            // QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): a provider
            // idempotency key identifies one HTTP call, and now every call is
            // one group, so each send carries its OWN reservation's token
            // rather than a representative standing in for the batch.
            match send_embed_group(
                adapter_settings,
                &conn,
                &embedding_context,
                group,
                intent_token.clone(),
                &mut replica,
                invocation.control,
            ) {
                Ok(usage) => {
                    // No reservation was made offline, so there is nothing to
                    // settle and no `reservation_id` for recovery to walk.
                    if let Some(intent_token) = intent_token.as_ref() {
                        settle_task_charge_success(
                            ledger
                                .as_ref()
                                .expect("online embedding requires an initialized ledger"),
                            &charge.key,
                            intent_token,
                            embedding_billed_from_usage(
                                adapter_settings,
                                usage.as_ref(),
                                active_embedding_send_lane(),
                                reserved_usd,
                                invocation.control,
                                invocation.locks,
                            ),
                        )?;
                        // R18-1's stamp, still needed so `reconcile_committed_embedding_tasks`
                        // et al. can find this group's ledger selector via
                        // `task.reservation_id` without reconstructing the key from
                        // possibly-since-changed config.
                        reserved_by_ref.insert(
                            embedding_task_output_ref(group.representative),
                            (reserved_usd, month.clone(), intent_token.clone()),
                        );
                    }
                    record_embedding_transitions(
                        &mut transitions,
                        group.members.iter().copied(),
                        embedding_done_transition(),
                    );
                    outcome.executed += group.members.len();
                }
                Err(failure) => {
                    let known_rejection = matches!(
                        failure.retry_kind,
                        RetryErrorKind::AuthError
                            | RetryErrorKind::RateLimit
                            | RetryErrorKind::QuotaExceeded
                    );
                    if let Some(intent_token) = intent_token.as_ref() {
                        if known_rejection {
                            terminal_transaction(
                                ledger.as_ref().expect("online embedding requires ledger"),
                                &TerminalWrite {
                                    key: &charge.key,
                                    outcome: Outcome::SubmitRejected,
                                    billed: BilledAmount {
                                        usd: 0.0,
                                        estimated: false,
                                    },
                                    ledger_batch_job_id: intent_token,
                                    next_state: BatchState::Terminal,
                                    error: Some(retry_reason(failure.retry_kind)),
                                    increment_contract_violation: false,
                                    attempts_delta: 1,
                                    clear_intent_token: true,
                                    intent_token_guard: Some(intent_token),
                                    reseat_submission_seq: false,
                                },
                            )
                            .map_err(pipeline_to_kio)?;
                        } else {
                            settle_task_charge_unknown(
                                ledger.as_ref().expect("online embedding requires ledger"),
                                &charge.key,
                                intent_token,
                                reserved_usd,
                            )?;
                        }
                    }
                    let result_unknown = embedding_is_online(execution) && !known_rejection;
                    let kind = if result_unknown {
                        RetryErrorKind::ResultUnknown
                    } else {
                        failure.retry_kind
                    };
                    record_embedding_transitions(
                        &mut transitions,
                        group.members.iter().copied(),
                        if result_unknown {
                            embedding_fail_transition(kind)
                        } else {
                            embedding_failure_transition(&failure)
                        },
                    );
                    count_embedding_failure(&mut outcome, kind, group.members.len());
                    break 'embedding_batches;
                }
            }
        }
    }
    // R11-5: single write-back for the whole pass. Its return is the retry-eligible
    // failed count (needs per-task `attempts`), feeding the batch exit-code 3-vs-4
    // split (R11-2). `fallback_reason` per chunk (done/paused/failed) is preserved.
    outcome.failed_retryable +=
        apply_embedding_transitions(&task_store, &transitions, &now, &reserved_by_ref)?;
    // 05 §1.8 write-through, after the loop's `break` paths as well as its
    // normal exit: a pass that gave up halfway still linked every vector it got
    // to, and those belong in the corpus.
    //
    // R25-4/R25-12: this lane writes `chunk_vec` straight into the live index.
    // Under `kio index` a rebuild has already rotated, but the same function is
    // reached from `kio batch resume` when the batch lane is unavailable, and
    // there nothing else rotates at all — leaving both the cursor contract and
    // the replica's only staleness signal on a stamp that never moves.
    current_policy.revalidate().map_err(pipeline_to_kio)?;
    publish_in_place_delta(ctx, repo.kio_dir(), replica_before.as_deref(), &replica)?;
    if outcome.executed > 0 && replica.is_empty() {
        write_through_projection_or_log_for_repo(ctx, repo);
    }
    Ok(outcome)
}

/// A local `offline_api` may be an authenticated HTTP peer. Secret approval is
/// therefore a transport property, rather than an online-provider property.
pub(crate) fn secret_embedding_hold_required(
    execution_mode: ExecutionMode,
    secrets_approved: bool,
    requires_secret_approval: bool,
) -> bool {
    execution_mode != ExecutionMode::DeterministicLibrary
        && !secrets_approved
        && requires_secret_approval
}

/// R11-5: one deferred task-store transition for an embedding chunk, applied in a
/// single pass by [`apply_embedding_transitions`].
#[derive(Clone, Copy)]
pub(crate) struct EmbeddingTransition {
    status: TaskStatus,
    reason: &'static str,
    failure_kind: Option<RetryErrorKind>,
    // QA3: the provider's `Retry-After` (ms) for a `"rate_limit"` transition —
    // `apply_embedding_transitions` reads this to build `next_retry_at` via
    // `rate_limit_retry_at`. `None` for every other transition.
    retry_after_ms: Option<u64>,
}

pub(crate) fn embedding_done_transition() -> EmbeddingTransition {
    EmbeddingTransition {
        status: TaskStatus::Done,
        reason: "embedding_adapter_done",
        failure_kind: None,
        retry_after_ms: None,
    }
}

pub(crate) fn embedding_pause_transition() -> EmbeddingTransition {
    EmbeddingTransition {
        status: TaskStatus::Paused,
        reason: "budget_exceeded",
        failure_kind: None,
        retry_after_ms: None,
    }
}

pub(crate) fn embedding_fail_transition(kind: RetryErrorKind) -> EmbeddingTransition {
    EmbeddingTransition {
        status: TaskStatus::Failed,
        reason: retry_reason(kind),
        failure_kind: Some(kind),
        retry_after_ms: None,
    }
}

/// The disposition a provider-side failure gives the affected chunks.
///
/// QA2/QA3 (04 §5.2/§5.3): `rate_limit` and `auth_error` each land a dedicated
/// non-Failed transition (mirroring the markdownize send handler's 3-way
/// branch); every other kind takes the generic Failed transition. Shared by the
/// synchronous send handler and — since G2 — the Batch submit lane, so the two
/// cannot drift apart.
pub(crate) fn embedding_failure_transition(failure: &TaskExecutionFailure) -> EmbeddingTransition {
    match failure.retry_kind {
        RetryErrorKind::RateLimit => embedding_rate_limit_transition(failure.retry_after_ms),
        RetryErrorKind::AuthError => embedding_auth_pause_transition(),
        _ => embedding_fail_transition(failure.retry_kind),
    }
}

/// QA3 (step4b-contract-tests-p3a.md §A, 04 §5.2/§5.3): a rate-limited
/// embedding send stays Pending (never Failed/Paused) — mirrors the
/// markdownize send handler's RateLimit branch. `apply_embedding_transitions`
/// reads `retry_after_ms` to honor the provider's `Retry-After` header (else
/// the synthetic +2s fallback), and does not bump `attempts`.
fn embedding_rate_limit_transition(retry_after_ms: Option<u64>) -> EmbeddingTransition {
    EmbeddingTransition {
        status: TaskStatus::Pending,
        reason: "rate_limit",
        failure_kind: None,
        retry_after_ms,
    }
}

/// QA2 (step4b-contract-tests-p3a.md §A, 04 §5.2): an auth-failed embedding
/// send lands Paused(hold_reason=auth) (never Failed) — mirrors the
/// markdownize send handler's AuthError branch. `apply_embedding_transitions`
/// does not bump `attempts` and clears `next_retry_at`; `hold_reason` is
/// auto-stamped by the existing QA1 `hold_reason_for_reason` block.
fn embedding_auth_pause_transition() -> EmbeddingTransition {
    EmbeddingTransition {
        status: TaskStatus::Paused,
        reason: "auth_error",
        failure_kind: None,
        retry_after_ms: None,
    }
}

/// R19-3: terminalize a non-live embedding task REVERSIBLY — non-retryable (no
/// `failure_kind`, so no backoff/attempt bump), but `retired_non_live` so the enqueue
/// guard re-creates a fresh task if the exact chunk_id reappears (revert/restore).
fn embedding_retired_non_live_transition() -> EmbeddingTransition {
    EmbeddingTransition {
        status: TaskStatus::Failed,
        reason: RETIRED_NON_LIVE,
        failure_kind: None,
        retry_after_ms: None,
    }
}

/// R11-2: tally one failed embedding batch into the pass outcome — `failed` always,
/// and `auth_failed` when the error needs user re-auth (exit 5). `failed_retryable`
/// is finalized later in [`apply_embedding_transitions`] (it needs per-task attempts).
pub(crate) fn count_embedding_failure(
    outcome: &mut ExecOutcome,
    kind: RetryErrorKind,
    count: usize,
) {
    outcome.failed += count;
    if kind == RetryErrorKind::AuthError {
        outcome.auth_failed += count;
    }
}

pub(crate) fn record_embedding_transitions<'a>(
    transitions: &mut BTreeMap<String, EmbeddingTransition>,
    chunks: impl IntoIterator<Item = &'a EmbeddableChunk>,
    transition: EmbeddingTransition,
) {
    for chunk in chunks {
        transitions.insert(embedding_task_output_ref(chunk), transition);
    }
}

/// R11-5: apply all accumulated embedding-task transitions in ONE `update_matching`.
/// Returns the count of failures recorded retry-eligible (retryable kind with
/// `attempts` remaining) so the caller can split the batch exit code 3 vs 4 (R11-2).
pub(crate) fn apply_embedding_transitions(
    task_store: &TaskStore,
    transitions: &BTreeMap<String, EmbeddingTransition>,
    now: &str,
    // R18-1: the FRESH per-chunk F8 reservation `(usd, ledger_month)` to stamp onto each
    // freshly-charged embedding task in this same single write-back (no extra
    // tasks.jsonl pass — respects R11-5/R17-7's O(T²) avoidance). Only freshly-charged
    // chunks are present; a RateLimit/Quota resend covered by a prior reservation is
    // absent and keeps its existing stamp. The stamp lets `reconcile_committed_embedding_tasks`
    // reclaim the exact phantom if the chunk later goes non-live (R18-1).
    reserved: &BTreeMap<String, (f64, String, String)>,
) -> Result<usize> {
    if transitions.is_empty() {
        return Ok(0);
    }
    let mut failed_retryable = 0usize;
    task_store
        .update_matching(|task| {
            if task.task_type != TaskType::Embedding {
                return false;
            }
            let Some(transition) = transitions.get(&task.output_ref) else {
                return false;
            };
            task.status = transition.status;
            task.fallback_reason = Some(transition.reason.to_owned());
            // QA1 (step4b-contract-tests-p3a.md §A): the closed hold_reason
            // enum accompanies every Paused task (currently only
            // `embedding_pause_transition`'s `budget_exceeded`).
            task.hold_reason = (transition.status == TaskStatus::Paused)
                .then(|| hold_reason_for_reason(transition.reason))
                .flatten();
            if let Some((usd, month, reservation_id)) = reserved.get(&task.output_ref) {
                task.reserved_usd = Some(*usd);
                task.reserved_month = Some(month.clone());
                task.reservation_id = Some(reservation_id.clone());
            }
            if let Some(kind) = transition.failure_kind {
                let attempts_after = task.attempts.saturating_add(1);
                let policy = retry_policy(kind);
                let retryable = policy.retryable
                    && policy
                        .max_attempts
                        .map(|max| attempts_after < max)
                        .unwrap_or(true);
                task.attempts = attempts_after;
                task.next_retry_at =
                    retryable.then(|| scheduled_retry_at(now, &policy.backoff, attempts_after));
                if retryable {
                    failed_retryable += 1;
                }
            } else if transition.reason == "rate_limit" {
                // QA3 (04 §5.3): max_attempts=∞ — `attempts` is NOT consumed.
                // `next_retry_at` honors the provider's `Retry-After` (or the
                // synthetic +2s fallback when absent).
                task.next_retry_at = Some(rate_limit_retry_at(now, transition.retry_after_ms));
                failed_retryable += 1;
            } else if transition.reason == "auth_error" {
                // QA2: a pause is not a retry-budget event — `attempts` is NOT
                // consumed. `hold_reason` is already auto-stamped above via
                // `hold_reason_for_reason`.
                task.next_retry_at = None;
            }
            true
        })
        .map_err(pipeline_to_kio)?;
    Ok(failed_retryable)
}

/// A batch split into free content-addressed reuse and chunks needing an API call.
pub(crate) struct EmbedBatchPlan<'a> {
    /// (chunk, existing content-vector bytes) — link `chunk_vec`, no adapter call.
    pub(crate) reuse: Vec<(&'a EmbeddableChunk, Vec<u8>)>,
    /// One adapter item per content identity, fanned out to every member chunk.
    pub(crate) to_send: Vec<EmbeddingSendGroup<'a>>,
}

/// The execution identity and current admission authority shared by chunk and
/// image embedding call edges. Keeping them together prevents either path from
/// accidentally using a profile or policy from a different execution.
struct EmbeddingExecutionContext<'a> {
    repo: &'a Repository,
    execution: EmbeddingExecution,
    profile: &'a DeclaredEmbeddingProfile,
    current_policy: &'a CurrentPolicyEvaluator,
    projection: EmbeddingProjection<'a>,
}

/// A frozen policy and the deterministic scalar choice derived from retained
/// owner bindings. Revalidation is required at every live publication edge.
pub(crate) struct EmbeddingProjection<'a> {
    pub(crate) repo: &'a Repository,
    pub(crate) policy: &'a CurrentPolicyEvaluator,
    pub(crate) selected_contexts: &'a BTreeMap<String, Option<String>>,
    pub(crate) invalidated: std::cell::Cell<bool>,
}

pub(crate) fn validate_embedding_aliases<'a>(
    policy: &CurrentPolicyEvaluator,
    profile: &DeclaredEmbeddingProfile,
    chunks: impl IntoIterator<Item = &'a EmbeddableChunk>,
) -> std::result::Result<(), TaskExecutionFailure> {
    let denied = || TaskExecutionFailure {
        retry_kind: RetryErrorKind::AuthError,
        retry_after_ms: None,
    };
    policy.revalidate().map_err(|_| denied())?;
    for chunk in chunks {
        if chunk.work_key.chunk_id() != chunk.chunk_id
            || chunk_embedding_hash(chunk, profile).map_err(|_| TaskExecutionFailure {
                retry_kind: RetryErrorKind::ContractViolation,
                retry_after_ms: None,
            })? != chunk.work_key.embedding_hash()
        {
            return Err(TaskExecutionFailure {
                retry_kind: RetryErrorKind::ContractViolation,
                retry_after_ms: None,
            });
        }
        if !policy.allows_path(&chunk.raw_path).map_err(|_| denied())? {
            return Err(denied());
        }
    }
    Ok(())
}

/// A vector writer must not mutate source scalars while an old Ready header
/// can remain observable. Unlike best-effort repair logging, failure here
/// aborts before the source mutation.
pub(crate) fn invalidate_embedding_projection(repo: &Repository) -> Result<()> {
    let scope_id = repo.scope_identity()?.scope_id;
    let mut replica = kio_index::aggregator::Aggregator::open_with_control(
        &aggregator_path(repo.control()),
        repo.control(),
    )
    .map_err(index_to_kio)?;
    if let Some(mut header) = replica.scope_header(&scope_id).map_err(index_to_kio)? {
        header.index_status = kio_index::aggregator::AggIndexStatus::Rebuilding;
        replica
            .update_scope_header(&scope_id, &header, replica_now_ms())
            .map_err(index_to_kio)?;
    }
    rotate_index_generation_unconditionally(repo.kio_dir(), repo.control())
}

fn invalidate_embedding_projection_once(projection: &EmbeddingProjection<'_>) -> Result<()> {
    if !projection.invalidated.get() {
        invalidate_embedding_projection(projection.repo)?;
        projection.invalidated.set(true);
    }
    Ok(())
}

/// Scalar equality alone cannot certify publication: a prior writer may have
/// committed vectors and stopped while the replica was still Rebuilding.
/// Cache probe failures take the ordinary best-effort publication path below.
pub(crate) fn embedding_replica_projection_is_current(repo: &Repository) -> bool {
    let Some((scope_id, generation)) = replica_scope_stamp(repo.kio_dir()) else {
        return false;
    };
    let Ok(head) = repo.head_commit_hash() else {
        return false;
    };
    let config = match head.as_deref() {
        Some(head) => match snapshot_chunking_config_hash(repo, head) {
            Ok(config) => Some(config),
            Err(_) => return false,
        },
        None => None,
    };
    let Ok(replica) = kio_index::aggregator::Aggregator::open_with_control(
        &aggregator_path(repo.control()),
        repo.control(),
    ) else {
        return false;
    };
    let Ok(Some(header)) = replica.scope_header(&scope_id) else {
        return false;
    };
    header.index_status == kio_index::aggregator::AggIndexStatus::Ready
        && header.index_generation == generation
        && header.current_snapshot_commit == head
        && header.current_chunking_config_hash == config
}

/// Refresh cached scalar choices before task completion is considered. Full
/// publication is required because an owner-policy change can remove vectors;
/// the additive fast-path delta cannot describe that mutation.
pub(crate) fn refresh_embedding_projection(
    ctx: &Invocation<'_>,
    repo: &Repository,
    conn: &Connection,
    chunks: &[EmbeddableChunk],
    profile: &DeclaredEmbeddingProfile,
    projection: &EmbeddingProjection<'_>,
    held: &BTreeSet<String>,
) -> Result<()> {
    projection.policy.revalidate().map_err(pipeline_to_kio)?;
    let mut expected = BTreeMap::new();
    for chunk in chunks {
        if held.contains(&chunk.chunk_id) || !selected_embedding_member(chunk, projection) {
            continue;
        }
        if let Some(bytes) = embedding_store::content_vector(conn, chunk.work_key.embedding_hash())
            .map_err(index_to_kio)?
        {
            expected.insert(chunk.chunk_id.clone(), bytes);
        }
    }
    let existing = conn
        .prepare("SELECT chunk_id, embedding FROM chunk_vec")
        .map_err(|e| KioError::schema(e.to_string()))?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|e| KioError::schema(e.to_string()))?
        .collect::<std::result::Result<BTreeMap<_, _>, _>>()
        .map_err(|e| KioError::schema(e.to_string()))?;
    if expected == existing {
        if !embedding_replica_projection_is_current(repo) {
            projection.policy.revalidate().map_err(pipeline_to_kio)?;
            write_through_projection_or_log_for_repo(ctx, repo);
        }
        return Ok(());
    }
    // Invalidate readiness and cursor generation before the first scalar write.
    invalidate_embedding_projection(repo)?;
    let summary = embedding_store::EmbeddingProfileSummary {
        dimensions: profile.dimensions,
        distance: profile.distance.clone(),
        modality: profile.modality.clone(),
        profile_hash: profile.profile_hash.clone(),
    };
    embedding_store::rebuild_chunk_vec_for_contexts(
        conn,
        Some(&summary),
        held,
        projection.selected_contexts,
    )
    .map_err(index_to_kio)?;
    projection.policy.revalidate().map_err(pipeline_to_kio)?;
    write_through_projection_or_log_for_repo(ctx, repo);
    Ok(())
}

fn selected_embedding_member(
    chunk: &EmbeddableChunk,
    projection: &EmbeddingProjection<'_>,
) -> bool {
    projection.selected_contexts.get(&chunk.chunk_id)
        == Some(&embedding_store::chunk_embedding_context(&chunk.raw_path))
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

/// Link `chunk_vec` for reuse hits (content vector already stored, no adapter).
pub(crate) fn link_reused_chunks(
    conn: &Connection,
    profile: &DeclaredEmbeddingProfile,
    reuse: &[(&EmbeddableChunk, Vec<u8>)],
    projection: &EmbeddingProjection<'_>,
    replica: &mut kio_index::aggregator::ScopeDelta,
) -> std::result::Result<(), TaskExecutionFailure> {
    validate_embedding_aliases(
        projection.policy,
        profile,
        reuse.iter().map(|(chunk, _)| *chunk),
    )?;
    for (chunk, bytes) in reuse {
        if !selected_embedding_member(chunk, projection) {
            continue;
        }
        invalidate_embedding_projection_once(projection).map_err(|_| TaskExecutionFailure {
            retry_kind: RetryErrorKind::ContractViolation,
            retry_after_ms: None,
        })?;
        let linked =
            embedding_store::link_chunk_vec(conn, &chunk.chunk_id, bytes, profile.dimensions)
                .map_err(|_| TaskExecutionFailure {
                    retry_kind: RetryErrorKind::ContractViolation,
                    retry_after_ms: None,
                })?;
        // A content-addressed reuse hit writes `chunk_vec` with no adapter call
        // and no rebuild, so it moves the vector corpus as surely as a send
        // does — and just as invisibly to a reader-driven refresh (05 §1.8).
        if linked {
            replica.vectors_added.push((
                chunk.chunk_id.clone(),
                embedding_store::f32_from_le_bytes(bytes),
            ));
        }
    }
    Ok(())
}

/// Send ONE group on the sync lane, write its `embeddings` + `chunk_vec`, and
/// return the usage the provider reported for that call.
///
/// One group per call is what makes the returned usage settleable as-is:
/// `:batchEmbedContents` reports a single `promptTokenCount` per CALL, so a
/// call spanning several ledger rows can only be divided among them by
/// guesswork. Both lanes now keep the billing unit and the accounting unit the
/// same object — the Batch lane by making the job the task (07 §5.5), this one
/// by making the call the row. The realtime lane is opt-in and never the
/// default (2026-07-25 ruling), so the round trips it costs are never on a
/// default path.
fn send_embed_group(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    conn: &Connection,
    embedding: &EmbeddingExecutionContext<'_>,
    group: &EmbeddingSendGroup<'_>,
    // QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): this group's own
    // reservation token. A provider idempotency key identifies one HTTP call,
    // and one call is now one group, so there is nothing to pick a
    // representative from.
    idempotency_token: Option<String>,
    replica: &mut kio_index::aggregator::ScopeDelta,
    control: &ControlHandle,
) -> std::result::Result<Option<kio_adapter::types::AdapterUsage>, TaskExecutionFailure> {
    // Contextual-embedding addendum (07 §5.3, 2026-07-24): the adapter input is
    // the humanized filename context prepended to the chunk body. Every member
    // of a group shares one `embedding_hash` = one `(text_hash, context,
    // profile)`, so the representative's context is the whole group's context.
    validate_embedding_aliases(
        embedding.current_policy,
        embedding.profile,
        group.members.iter().copied(),
    )?;
    let context = embedding_store::chunk_embedding_context(&group.representative.raw_path);
    let items = vec![EmbeddingItem::text(
        group.embedding_hash.clone(),
        embedding_store::contextualized_embedding_input(
            context.as_deref(),
            &group.representative.text,
        ),
    )];
    // Online sends were admitted immediately before their ledger reservation;
    // local HTTP has no reservation, so admission must be at this call edge.
    if !embedding_is_online(embedding.execution) {
        admit_embedding_transport(
            adapter_settings,
            embedding.repo,
            embedding.execution,
            embedding.profile,
            embedding.current_policy,
            group
                .members
                .iter()
                .any(|chunk| chunk.requires_secret_approval),
        )
        .map_err(|_| TaskExecutionFailure {
            retry_kind: RetryErrorKind::AuthError,
            retry_after_ms: None,
        })?;
    }
    let outcome = run_embedding_adapter(
        adapter_settings,
        embedding.execution,
        items,
        EmbeddingInputType::MarkdownChunk,
        idempotency_token,
        control,
    )?;
    let Some(vector) = outcome
        .vectors
        .iter()
        .find(|vector| vector.id == group.embedding_hash)
    else {
        return Err(TaskExecutionFailure {
            retry_kind: RetryErrorKind::ContractViolation,
            retry_after_ms: None,
        });
    };
    persist_group_vector(
        conn,
        embedding.profile,
        group,
        &vector.vector,
        &BTreeSet::new(),
        &embedding.projection,
        replica,
    )?;
    Ok(outcome.usage)
}

/// Write one group's content vector and fan it out to every member chunk.
///
/// Shared by BOTH lanes: the sync path calls it with the vector the adapter
/// just returned, the Batch path with the vector collected from a finished
/// provider job. 07 §5.3's acceptance checks must not exist in two versions,
/// so neither may they diverge here.
///
/// `replica` accumulates what to write through to the device replica (05 §1.8).
/// It is threaded down to here — rather than reconstructed by each lane — for
/// the same reason the acceptance checks are: this is the single point that
/// knows which chunks actually gained a vector, and both lanes write into the
/// LIVE index without rebuilding it, so nothing downstream re-reads the scope.
/// The caller flushes once, with `write_through_delta`.
pub(crate) fn persist_group_vector(
    conn: &Connection,
    profile: &DeclaredEmbeddingProfile,
    group: &EmbeddingSendGroup<'_>,
    vector: &[f32],
    held: &BTreeSet<String>,
    projection: &EmbeddingProjection<'_>,
    replica: &mut kio_index::aggregator::ScopeDelta,
) -> std::result::Result<(), TaskExecutionFailure> {
    let repo = projection.repo;
    let chunk = group.representative;
    if chunk_embedding_hash(chunk, profile).map_err(|_| TaskExecutionFailure {
        retry_kind: RetryErrorKind::ContractViolation,
        retry_after_ms: None,
    })? != group.embedding_hash
        || chunk.work_key.embedding_hash() != group.embedding_hash
    {
        return Err(TaskExecutionFailure {
            retry_kind: RetryErrorKind::ContractViolation,
            retry_after_ms: None,
        });
    }
    let bytes = f32_to_le_bytes(vector);
    let context_key = embedding_store::chunk_embedding_context(&chunk.raw_path);
    // CAS first, table second (04 §4.3: `objects/` → `embeddings` → `chunk_vec`).
    // The order is what makes a crash between them recoverable in the direction
    // that matters: an object with no row is replayed by the next rebuild, while
    // a row with no object is a vector `rebuild-db` cannot restore.
    publish_embedding_object(
        repo,
        &group.embedding_hash,
        &chunk.text_hash,
        vector,
        profile,
        context_key.as_deref(),
    )
    .map_err(|_| TaskExecutionFailure {
        retry_kind: RetryErrorKind::ContractViolation,
        retry_after_ms: None,
    })?;
    projection
        .policy
        .revalidate()
        .map_err(|_| TaskExecutionFailure {
            retry_kind: RetryErrorKind::AuthError,
            retry_after_ms: None,
        })?;
    invalidate_embedding_projection_once(projection).map_err(|_| TaskExecutionFailure {
        retry_kind: RetryErrorKind::ContractViolation,
        retry_after_ms: None,
    })?;
    embedding_store::write_chunk_embedding_source(
        conn,
        &group.embedding_hash,
        &chunk.text_hash,
        &bytes,
        profile.dimensions,
        &profile.distance,
        &profile.modality,
        &profile.profile_hash,
        context_key.as_deref(),
    )
    .map_err(|_| TaskExecutionFailure {
        retry_kind: RetryErrorKind::ContractViolation,
        retry_after_ms: None,
    })?;
    validate_embedding_aliases(projection.policy, profile, group.members.iter().copied())?;
    let linked = embedding_store::link_chunk_vecs_to_content_vector(
        conn,
        &group.embedding_hash,
        group
            .members
            .iter()
            .filter(|member| selected_embedding_member(member, projection))
            .map(|member| member.chunk_id.as_str()),
        held,
    )
    .map_err(|_| TaskExecutionFailure {
        retry_kind: RetryErrorKind::ContractViolation,
        retry_after_ms: None,
    })?;
    // The ids the link REPORTED, never `group.members`. A secrets-held member
    // (R20-10) is deliberately kept out of `chunk_vec` so it cannot be reached
    // by vector search without `--send-secrets`, and a width-mismatched member
    // is dropped as incompatible. Mirroring either rule here would put it in
    // two places, and the day they drifted the replica would expose a held
    // chunk (03 §4 invariant 8).
    replica.vectors_added.extend(
        linked
            .into_iter()
            .map(|chunk_id| (chunk_id, vector.to_vec())),
    );
    Ok(())
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

/// Embed typed-owned image objects cited by live chunks into `image_vec`
/// (04 §4.3).
///
/// # Where the work list comes from
///
/// The chunk body supplies the citation relation, while the authenticated
/// immutable normalized unit supplies the ownership relation. Both are
/// required: a free-form `kio://` URI cannot select same-scope CAS bytes.
///
/// # Why it is gated
///
/// An adapter that declares `modality: "multimodal"` has said something about
/// the vector SPACE, not about what it can be handed. The adopted online
/// adapter declares multimodal and reads only `EmbeddingItem::text`, so giving
/// it an image item embeds the empty string and stores a vector that is wrong
/// without being detectably wrong. `image_object` is the flag that means "this
/// implementation reads `path`/`mime`".
///
/// # Cost
///
/// A local image-capable adapter has no billable ledger operation, but it is
/// still an authenticated HTTP peer. Its caller passes execution admission:
/// exact device+scope approval for HTTP adapters, or the in-process evaluator.
fn run_image_embedding_enrichment(
    invocation: Invocation<'_>,
    embedding: &EmbeddingExecutionContext<'_>,
    conn: &Connection,
    retained_instances: &[RetainedNormalizedInstance],
    eligible_chunks: &[EmbeddableChunk],
    secrets_approved: bool,
    execution_allowed: bool,
) -> Result<usize> {
    let ctx = &invocation;
    let adapter_settings = invocation.settings;

    if !execution_allowed {
        return Ok(0);
    }
    // No online image lane currently has the required ledger/reservation
    // protocol. Never turn a newly advertised capability into unaccounted
    // external spending merely because the adapter accepts image objects.
    if embedding_is_online(embedding.execution) {
        return Ok(0);
    }
    let adapter_profile = embedding_adapter_profile_with_control(
        adapter_settings,
        embedding.execution,
        invocation.control,
    )
    .map_err(adapter_to_kio)?;
    if !adapter_profile
        .capability_flags
        .iter()
        .any(|flag| flag == kio_adapter::catalog::IMAGE_OBJECT_CAPABILITY)
    {
        return Ok(0);
    }
    let scope_id = embedding.repo.scope_identity()?.scope_id;
    let image_authority = image_authority::retained_image_authority(
        embedding.repo,
        retained_instances,
        embedding.current_policy,
    )?;
    let referenced = referenced_authorized_image_hashes_for_chunks(
        eligible_chunks,
        &scope_id,
        &image_authority,
        secrets_approved,
    );
    if referenced.is_empty() {
        return Ok(0);
    }
    let already = embedding_store::embedded_image_hashes(conn, &embedding.profile.profile_hash)
        .map_err(index_to_kio)?;
    // BTreeSet iteration is ordered, so the send order is a function of the
    // corpus and not of hash-map seed — two runs over one corpus embed in the
    // same order.
    let pending = referenced
        .into_iter()
        .filter(|hash| !already.contains(hash))
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return Ok(0);
    }
    let mut embedded = 0;
    let mut replica_marked_unavailable = false;
    let result = (|| -> Result<()> {
        let store = embedding.repo.object_store();
        // The local adapter deliberately makes one HTTP request per image.
        // Keep the app-side authorization and bounded CAS read at that same
        // granularity: a policy change cannot admit later images, and a large
        // corpus never accumulates all source bytes in memory.
        for image_hash in &pending {
            match store.inspect_content_object(ContentObjectKind::Image, image_hash) {
                Ok(_) => {}
                Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => continue,
                Err(error) => return Err(error),
            }
            let bytes = store.read_content_object_bytes(
                ContentObjectKind::Image,
                image_hash,
                u64::try_from(MAX_EMBEDDING_ITEM_BYTES)
                    .map_err(|_| KioError::schema("embedding image cap exceeds u64"))?,
            )?;
            if kio_core::cas::hash_bytes(&bytes) != *image_hash {
                return Err(KioError::schema(
                    "verified image payload hash changed while reading",
                ));
            }
            let mime = verified_image_mime(&bytes)?;
            // OCR-extracted figures and directly ingested images share this
            // send boundary. Magic bytes alone are insufficient: validate the
            // bounded decode before an adapter can process any pixels.
            let _ = crate::image_ingest::validate_source_image_payload(&bytes, mime)?;

            // Re-read policy, exact network grant, and secret consent before
            // every authenticated local HTTP request. A secret alias is
            // conservative: it requires this embedding tool's paired grant
            // even if a public alias names the same immutable image object.
            admit_embedding_transport(
                adapter_settings,
                embedding.repo,
                embedding.execution,
                embedding.profile,
                embedding.current_policy,
                image_authority.requires_secret_approval(image_hash),
            )?;
            let Ok(outcome) = run_embedding_adapter(
                adapter_settings,
                embedding.execution,
                vec![EmbeddingItem {
                    id: image_hash.clone(),
                    content: EmbeddingContent::Image {
                        bytes,
                        mime: mime.to_owned(),
                    },
                }],
                EmbeddingInputType::ImageObject,
                // No reservation was made, so there is no intent token to carry.
                None,
                invocation.control,
            ) else {
                // A failed image embed must not fail the index pass. Chunk
                // enrichment is load-bearing; this image remains pending for
                // the next run while later independent images can progress.
                continue;
            };
            let Some(vector) = outcome.vectors.into_iter().next() else {
                return Err(KioError::schema("image embedding response was empty"));
            };
            if vector.id != *image_hash {
                return Err(KioError::schema(
                    "image embedding response id did not match request",
                ));
            }
            let embedding_hash = embedding_store::embedding_hash(
                EmbeddingTargetType::Image,
                &vector.id,
                embedding.profile.dimensions,
                EmbeddingDistance::Cosine,
                // 04 §4.3 / U11: image rows are `multimodal` like every other row.
                // `EmbeddingModality::Image` would name a second vector space,
                // which 03 §7 does not have.
                EmbeddingModality::Multimodal,
                &embedding.profile.profile_hash,
                None,
            )
            .map_err(index_to_kio)?;
            // CAS first, table second. This keeps a crash recoverable by a
            // later rebuild instead of leaving a SQLite reference to no object.
            if !replica_marked_unavailable {
                // Mark before the first visible source mutation. If a later
                // image or authorization check fails, readers cannot mistake
                // a prior image vector for a complete replica projection.
                mark_replica_unavailable_or_log(ctx, embedding.repo.kio_dir());
                replica_marked_unavailable = true;
            }
            publish_image_embedding_object(
                embedding.repo,
                &embedding_hash,
                &vector.id,
                &vector.vector,
                embedding.profile,
            )?;
            embedding_store::write_image_embedding(
                conn,
                &embedding_hash,
                &vector.id,
                &embedding_store::f32_to_le_bytes(&vector.vector),
                embedding.profile.dimensions,
                &embedding.profile.distance,
                &embedding.profile.modality,
                &embedding.profile.profile_hash,
            )
            .map_err(index_to_kio)?;
            embedded += 1;
        }
        Ok(())
    })();
    if embedded > 0 {
        // `ScopeDelta` only represents chunk-vector additions. Image vectors
        // also need their image-reference relation, so publish a complete
        // projection after rotating the source generation.
        rotate_index_generation_unconditionally(
            embedding.repo.kio_dir(),
            embedding.repo.control(),
        )?;
        write_through_projection_or_log(ctx, embedding.repo.kio_dir());
    }
    result?;
    Ok(embedded)
}

/// Infer only image types whose magic bytes are unambiguous. The adapter must
/// not choose a decoder from a path or caller-supplied MIME.
pub(crate) fn verified_image_mime(bytes: &[u8]) -> Result<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Ok("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Ok("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Ok("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Ok("image/webp")
    } else {
        Err(KioError::schema(
            "image object has unsupported or unverifiable MIME",
        ))
    }
}

/// Every image object hash referenced by a live chunk body and owned by a
/// currently eligible retained unit. Markdown is only a citation relation;
/// immutable typed ownership is what permits a local CAS read or vector row.
pub(crate) fn referenced_authorized_image_hashes_for_chunks(
    chunks: &[impl AsRef<RetainedEmbeddingChunk>],
    scope_id: &str,
    authority: &image_authority::ImageAuthorityMap,
    secrets_approved: bool,
) -> BTreeSet<String> {
    let mut hashes = BTreeSet::new();
    for chunk in chunks
        .iter()
        .map(AsRef::as_ref)
        .filter(|chunk| secrets_approved || !chunk.requires_secret_approval)
    {
        for image in extract_related_images(&chunk.text) {
            if let Some(hash) =
                authorized_local_image_object_hash(scope_id, authority, &image.image_uri)
                && (secrets_approved || !authority.requires_secret_approval(&hash))
            {
                hashes.insert(hash);
            }
        }
    }
    hashes
}

/// Only a URI naming this scope may cause a local image CAS read, vector row,
/// or projection association. Foreign-scope Markdown links remain intact for
/// their explicit resolver, but never alias same-hash bytes in this scope.
pub(crate) fn local_image_object_hash(scope_id: &str, image_uri: &str) -> Option<String> {
    let object = kio_search::object_uri::parse_object_uri(image_uri).ok()?;
    (object.is_image() && object.scope_id() == scope_id).then(|| object.hash().to_owned())
}

/// A same-scope URI is necessary but insufficient for a local image-object
/// operation. Its hash must also occur in a retained unit's typed ownership
/// set and have a currently allowed owner alias.
pub(crate) fn authorized_local_image_object_hash(
    scope_id: &str,
    authority: &image_authority::ImageAuthorityMap,
    image_uri: &str,
) -> Option<String> {
    let hash = local_image_object_hash(scope_id, image_uri)?;
    authority.permits(&hash).then_some(hash)
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

/// R12-3: complete embedding tasks stranded `Pending`/`Running` by a crash between
/// the per-batch `chunk_vec` commit and the deferred task-store write-back (R11-5).
/// A chunk that already carries its embedding is "live but not in `pending`" — so
/// `live_chunks_without_embedding` never revisits it and no recovery command
/// (index / batch resume/retry / repair) ever reconciles its task, leaving
/// `index_status` reporting phantom pending enrichment permanently. Mark such tasks
/// `Done` (idempotent, no adapter call, no re-charge — the vector is already stored,
/// so search/data are unaffected; only task accounting + the Agent contract heal).
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
