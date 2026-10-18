use crate::commands::IndexArgs;
use crate::egress::admit_embedding_transport;
use crate::egress::embedding_online_allowed;
use crate::egress::embedding_secrets_allowed;
use crate::egress::prune_terminal_embedding_secret_holds;
use crate::embedding::admission::EMBEDDING_ADAPTER_KIND;
use crate::embedding::admission::active_embedding_send_lane;
use crate::embedding::admission::declared_embedding_profile;
use crate::embedding::admission::effective_invocation_lane;
use crate::embedding::admission::embedding_billed_from_usage;
use crate::embedding::admission::embedding_execution;
use crate::embedding::admission::embedding_is_online;
use crate::embedding::admission::estimate_embedding_cost;
use crate::embedding::admission::secret_embedding_hold_required;
use crate::embedding::admission::validate_embedding_aliases;
use crate::embedding::batch::submit_embedding_batch_jobs;
use crate::embedding::input::referenced_authorized_image_hashes_for_chunks;
use crate::embedding::input::verified_image_mime;
use crate::embedding::plan::EMBEDDING_BATCH_SIZE;
use crate::embedding::plan::EmbeddableChunk;
use crate::embedding::plan::EmbeddingReconcileContext;
use crate::embedding::plan::EmbeddingSendGroup;
use crate::embedding::plan::bind_embedding_work;
use crate::embedding::plan::chunk_embedding_hash;
use crate::embedding::plan::embedding_task_output_ref;
use crate::embedding::plan::enqueue_embedding_tasks;
use crate::embedding::plan::filter_embeddable_by_task_state;
use crate::embedding::plan::hold_secret_embedding_tasks;
use crate::embedding::plan::plan_embed_batch;
use crate::embedding::plan::reconcile_committed_embedding_tasks;
use crate::embedding::plan::retained_chunks_without_embedding;
use crate::embedding::plan::retained_history_chunks;
use crate::embedding::plan::selected_embedding_contexts;
use crate::embedding::plan::validate_embedding_task_bindings;
use crate::embedding::transport::run_embedding_adapter;
use crate::embedding_owners;
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
use crate::task_execution::retry_reason;
use crate::task_execution::scheduled_retry_at;
use kio_adapter::catalog::DeclaredEmbeddingProfile;
use kio_adapter::catalog::EmbeddingExecution;
use kio_adapter::catalog::embedding_adapter_profile_with_control;
use kio_adapter::tool_lock::AdapterRuntimeSettings;
use kio_adapter::traits::PreferredRequestKind;
use kio_adapter::types::EmbeddingContent;
use kio_adapter::types::EmbeddingInputType;
use kio_adapter::types::EmbeddingItem;
use kio_adapter::types::MAX_EMBEDDING_ITEM_BYTES;
use kio_core::ExitCode;
use kio_core::KioError;
use kio_core::Result;
use kio_core::cas::ContentObjectKind;
use kio_core::control::ControlHandle;
use kio_core::scope::Repository;
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
use kio_pipeline::ledger::Outcome;
use kio_pipeline::ledger::RequestKind;
use kio_pipeline::ledger::TaskKey as LedgerTaskKey;
use kio_pipeline::ledger::ops::BilledAmount;
use kio_pipeline::ledger::ops::TerminalWrite;
use kio_pipeline::ledger::ops::get_batch_request;
use kio_pipeline::ledger::ops::terminal_transaction;
use kio_pipeline::policy::CurrentPolicyEvaluator;
use kio_pipeline::task::RetryErrorKind;
use kio_pipeline::task::TaskStatus;
use kio_pipeline::task::TaskStore;
use kio_pipeline::task::TaskType;
use kio_pipeline::task::hold_reason_for_reason;
use kio_pipeline::task::retry_policy;
use rusqlite::Connection;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

/// Generate scope chunk embeddings after the SQLite index is rebuilt.
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

/// One deferred task-store transition for an embedding chunk.
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
pub(crate) fn embedding_retired_non_live_transition() -> EmbeddingTransition {
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

/// The execution identity and current admission authority shared by chunk and
/// image embedding call edges.
pub(crate) struct EmbeddingExecutionContext<'a> {
    repo: &'a Repository,
    pub(crate) execution: EmbeddingExecution,
    pub(crate) profile: &'a DeclaredEmbeddingProfile,
    pub(crate) current_policy: &'a CurrentPolicyEvaluator,
    pub(crate) projection: EmbeddingProjection<'a>,
}

/// A frozen policy and the deterministic scalar choice derived from retained
/// owner bindings. Revalidation is required at every live publication edge.
pub(crate) struct EmbeddingProjection<'a> {
    pub(crate) repo: &'a Repository,
    pub(crate) policy: &'a CurrentPolicyEvaluator,
    pub(crate) selected_contexts: &'a BTreeMap<String, Option<String>>,
    pub(crate) invalidated: std::cell::Cell<bool>,
}

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

/// A vector writer must not mutate source scalars while an old Ready header
/// can remain observable. Unlike best-effort repair logging, failure here
/// aborts before the source mutation.
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

/// Link `chunk_vec` for reuse hits without an adapter call.
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
pub(crate) fn send_embed_group(
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

/// Embed typed-owned image objects cited by live chunks into `image_vec`.
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

/// Publish a vector to immutable CAS before writing source or replica rows.
pub(crate) fn publish_embedding_object(
    repo: &Repository,
    embedding_hash: &str,
    text_hash: &str,
    vector: &[f32],
    profile: &DeclaredEmbeddingProfile,
    context: Option<&str>,
) -> Result<()> {
    let object = kio_core::cas::EmbeddingObject {
        spec_version: 1,
        target_type: "chunk".to_owned(),
        target_hash: text_hash.to_owned(),
        profile_hash: profile.profile_hash.clone(),
        modality: profile.modality.clone(),
        dimensions: profile.dimensions,
        distance: profile.distance.clone(),
        context: context.map(str::to_owned),
        vector: vector.to_vec(),
    };
    publish_verified_embedding(repo, embedding_hash, &object)
}

/// Image vectors obey the same CAS-before-projection publication contract.
pub(crate) fn publish_image_embedding_object(
    repo: &Repository,
    embedding_hash: &str,
    image_hash: &str,
    vector: &[f32],
    profile: &DeclaredEmbeddingProfile,
) -> Result<()> {
    let object = kio_core::cas::EmbeddingObject {
        spec_version: 1,
        target_type: "image".to_owned(),
        target_hash: image_hash.to_owned(),
        profile_hash: profile.profile_hash.clone(),
        modality: profile.modality.clone(),
        dimensions: profile.dimensions,
        distance: profile.distance.clone(),
        context: None,
        vector: vector.to_vec(),
    };
    publish_verified_embedding(repo, embedding_hash, &object)
}

fn publish_verified_embedding(
    repo: &Repository,
    expected_hash: &str,
    object: &kio_core::cas::EmbeddingObject,
) -> Result<()> {
    let actual_hash = repo.object_store().write_embedding(object)?;
    if actual_hash != expected_hash {
        return Err(KioError::new(
            "KIO-E-STORE-CORRUPT-001",
            "embedding CAS identity differs from its requested projection key",
            json!({ "expected_hash":expected_hash, "actual_hash":actual_hash }),
            ExitCode::PermanentFailure,
        ));
    }
    Ok(())
}
