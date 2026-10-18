use crate::egress::admit_embedding_transport;
use crate::egress::embedding_secrets_allowed;
use crate::embedding::admission::EMBEDDING_ADAPTER_KIND;
use crate::embedding::admission::declared_embedding_profile;
use crate::embedding::admission::embedding_billed_from_usage;
use crate::embedding::admission::embedding_execution;
use crate::embedding::admission::estimate_embedding_cost;
use crate::embedding::admission::secret_embedding_hold_required;
use crate::embedding::admission::validate_embedding_aliases;
use crate::embedding::executor::EmbeddingProjection;
use crate::embedding::executor::EmbeddingTransition;
use crate::embedding::executor::count_embedding_failure;
use crate::embedding::executor::embedding_done_transition;
use crate::embedding::executor::embedding_fail_transition;
use crate::embedding::executor::embedding_failure_transition;
use crate::embedding::executor::embedding_pause_transition;
use crate::embedding::executor::link_reused_chunks;
use crate::embedding::executor::persist_group_vector;
use crate::embedding::executor::record_embedding_transitions;
use crate::embedding::executor::refresh_embedding_projection;
use crate::embedding::plan::EmbeddableChunk;
use crate::embedding::plan::EmbeddingSendGroup;
use crate::embedding::plan::bind_embedding_work;
use crate::embedding::plan::chunk_embedding_hash;
use crate::embedding::plan::plan_embed_batch;
use crate::embedding::plan::selected_embedding_contexts;
use crate::embedding_owners;
use crate::historical_reindex::retained_history_instances;
use crate::index::chunk_ledger::sqlite_path;
use crate::index::rebuild::held_secret_embedding_chunk_ids;
use crate::invocation::Invocation;
use crate::ledger_access::TaskChargeOutcome;
use crate::ledger_access::budget_cap_config;
use crate::ledger_access::reserve_or_reuse_task_charge;
use crate::ledger_access::task_ledger_key;
use crate::markdownize::batch::settle_batch_charge_terminal;
use crate::markdownize::execution::task_failure_from_adapter;
use crate::object_persistence::adapter_to_kio;
use crate::object_persistence::index_to_kio;
use crate::object_persistence::pipeline_to_kio;
use crate::search::projection::publish_in_place_delta;
use crate::search::projection::replica_scope_stamp;
use crate::search::projection::write_through_projection_or_log_for_repo;
use crate::search::scope::current_embedding_policy;
use crate::settings::batch_recovery_context;
use crate::task_execution::ExecOutcome;
use kio_adapter::catalog::DeclaredEmbeddingProfile;
use kio_adapter::catalog::EmbeddingExecution;
use kio_adapter::catalog::embedding_preferred_request_kind_with_control;
use kio_adapter::traits::PreferredRequestKind;
use kio_core::ExitCode;
use kio_core::KioError;
use kio_core::Result;
use kio_core::cas::is_hash;
use kio_core::scope::Repository;
use kio_index::embedding_store;
use kio_index::fts::ExistingSourceIndexOpenMode;
use kio_index::fts::FtsSchemaConfig;
use kio_index::fts::FtsTokenizer;
use kio_index::fts::open_existing_source_index_connection;
use kio_pipeline::budget::BudgetCaps;
use kio_pipeline::ledger::BatchRequestRow;
use kio_pipeline::ledger::BatchState;
use kio_pipeline::ledger::LedgerDb;
use kio_pipeline::ledger::Outcome;
use kio_pipeline::ledger::RequestKind;
use kio_pipeline::ledger::ops::BilledAmount;
use kio_pipeline::ledger::ops::batch_poll_candidates;
use kio_pipeline::ledger::ops::get_batch_request;
use kio_pipeline::ledger::ops::phase2a_record_provider_scope;
use kio_pipeline::ledger::ops::phase2b_record_job_create_started;
use kio_pipeline::ledger::ops::phase2b_record_job_created;
use kio_pipeline::ledger::ops::recovery_finish_cleanup;
use kio_pipeline::policy::CurrentPolicyEvaluator;
use kio_pipeline::task::RetryErrorKind;
use rusqlite::Connection;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

// Embedding Batch lane (07 §5.3 の 2026-07-24 訂正 / §5.7, 04 §5.8)
//
// The Gemini Developer API bills an embedding batch at half the sync rate, so
// the Batch lane is the default. Unlike the OCR lane it takes its input
// INLINE, so there is no upload phase (相 2a) and no provider residue to
// sweep — see `kio_adapter::gemini_batch_client`.
//
// **1 job = 1 task** (07 §5.5's v1 contract) is preserved by making the JOB
// itself the task: the batch row's `input_hash` is a digest of the job's
// member set. Embedding's natural task granularity is the dedup group, and a
// realistic scope has thousands of them (2,321 in the dogfood fixture), so
// one job per group would mean thousands of provider jobs. One row per job
// keeps the §5.8 state machine, its crash recovery, and `kio batch abandon`
// working unchanged.
// ===========================================================================

/// Groups carried by one embedding batch job. Well under the adapter's inline
/// bounds (`MAX_INLINE_REQUESTS` = 2048, 16 MiB serialized), leaving room for
/// long chunks: 6,000 chars max per chunk (`[chunking] max_chars`) × 512 ≈ 3 MB
/// of text before JSON overhead.
const EMBEDDING_BATCH_JOB_MAX_MEMBERS: usize = 512;

/// How many non-success terminals one embedding batch task (= one job's member
/// set) may accumulate before it is failed instead of re-sent.
///
/// R24: a job that keeps ending in a non-success terminal state used to be
/// re-submitted without bound — every `batch resume` settled the failure,
/// cleared the row, and immediately created a new job with a fresh reservation.
/// A failed send reports no usage — measured settlement (I4/I12) covers the
/// success path only — so on this path the reservation IS the settled charge,
/// and a permanently-failing member set recorded its full estimate once per
/// pass until the budget cap hard-stopped the scope.
///
/// Counted on `contract_violation_count`, NOT on `attempts`, specifically
/// because **`kio batch retry --reset-violations <selector> --yes` already exists as the operator escape
/// hatch for it** (CL62-CL68) and nothing resets `attempts`. Bounding on
/// `attempts` would have made an exhausted member set permanently
/// unsubmittable — a dead end with no way out. 07 §5.3 already routes a
/// response that yields no usable output through the contract-violation rules,
/// so a FAILED/EXPIRED/CANCELLED job counting here is the established meaning.
const EMBEDDING_BATCH_JOB_MAX_FAILURES: i64 = 3;

/// G2: record one Batch-submit provider failure against the job's member
/// chunks, using the same classification and the same 3-way disposition the
/// synchronous send handler applies.
///
/// The reservation is deliberately LEFT IN PLACE (the F8 reserve-before-send
/// posture): whether the provider actually created the job is unknown from
/// here, so releasing it could double-charge on the next pass. 04 §5.8's
/// recovery walk resolves it — found → the poll lane collects it, confirmed
/// absent → the reservation is released.
fn record_batch_submit_failure(
    outcome: &mut ExecOutcome,
    transitions: &mut BTreeMap<String, EmbeddingTransition>,
    job_groups: &[EmbeddingSendGroup<'_>],
    error: kio_adapter::AdapterError,
) {
    let failure = task_failure_from_adapter(error);
    record_embedding_transitions(
        transitions,
        job_groups
            .iter()
            .flat_map(|group| group.members.iter().copied()),
        embedding_failure_transition(&failure),
    );
    count_embedding_failure(
        outcome,
        failure.retry_kind,
        job_groups.iter().map(|group| group.members.len()).sum(),
    );
}

/// Total `promptTokenCount` a job's result lines reported, or `None` when not
/// every line carried one.
///
/// All-or-nothing on purpose. A partial sum would silently under-state the
/// charge, and the ledger's own rule (CL28) is that an incomplete usage report
/// degrades to the conservative reservation rather than being trusted in part.
/// Returning `None` is what routes it there.
fn reported_prompt_tokens(
    results: &[kio_adapter::gemini_batch_client::GeminiBatchEmbedOutput],
) -> Option<u64> {
    if results.is_empty() {
        return None;
    }
    results
        .iter()
        .map(|result| result.prompt_tokens)
        .try_fold(0u64, |total, tokens| Some(total + tokens?))
}

/// The batch row's `input_hash` for a job: a digest over the job's member
/// content identities, in a canonical order.
///
/// This is what makes "1 job = 1 task" hold for a lane whose task granularity
/// is finer than a job. Two runs that select the same member set land on the
/// same row (so a crash-stranded reservation is reused, not duplicated); a
/// different selection is a different task, and any member already embedded by
/// the earlier job drops out through content-addressed reuse before it is ever
/// selected again.
const EMBEDDING_BATCH_RESULT_CONTRACT_ERROR: &str =
    "embedding batch result violates the complete response contract";

fn embedding_batch_results_match_job(
    results: &[kio_adapter::gemini_batch_client::GeminiBatchEmbedOutput],
    input_hash: &str,
) -> bool {
    let keys = results
        .iter()
        .map(|result| result.key.clone())
        .collect::<BTreeSet<_>>();
    !results.is_empty()
        && keys.len() == results.len()
        && results
            .iter()
            .all(|result| is_hash(&result.key) && result.values.is_some() && result.error.is_none())
        && embedding_job_input_hash(&keys) == input_hash
}

pub(crate) fn embedding_job_input_hash(embedding_hashes: &BTreeSet<String>) -> String {
    let joined = embedding_hashes
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    kio_core::cas::hash_bytes(joined.as_bytes())
}

/// Validate the entire response before the caller can persist its first line.
/// An old profile is settlement-only; never reinterpret its dimensions under
/// the currently configured vector space.
pub(crate) fn validated_embedding_batch_vectors(
    results: &[kio_adapter::gemini_batch_client::GeminiBatchEmbedOutput],
    input_hash: &str,
    current_profile: Option<&DeclaredEmbeddingProfile>,
) -> Result<BTreeMap<String, Vec<f32>>> {
    if !embedding_batch_results_match_job(results, input_hash) {
        return Err(KioError::schema(EMBEDDING_BATCH_RESULT_CONTRACT_ERROR));
    }
    let Some(profile) = current_profile else {
        return Ok(BTreeMap::new());
    };
    results
        .iter()
        .map(|result| {
            normalize_embedding_vector(
                result
                    .values
                    .as_deref()
                    .expect("complete result was checked"),
                profile,
            )
            .map(|vector| (result.key.clone(), vector))
        })
        .collect()
}

/// 07 §5.3 (3)/(4): validate the provider's raw values, convert deterministically
/// to unit-norm f32, then re-validate the RESULT — an underflowed zero vector or
/// an overflowed infinity must never reach the index. The sync lane gets this
/// inside the adapter; the Batch lane's values arrive raw from a job, so the
/// same discipline is applied here.
fn normalize_embedding_vector(
    values: &[f32],
    profile: &DeclaredEmbeddingProfile,
) -> Result<Vec<f32>> {
    let dimensions = u32::try_from(profile.dimensions).unwrap_or(u32::MAX);
    kio_adapter::types::validate_cosine_vector(values, dimensions).map_err(adapter_to_kio)?;
    let norm = values
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt();
    let normalized = values
        .iter()
        .map(|value| (f64::from(*value) / norm) as f32)
        .collect::<Vec<f32>>();
    kio_adapter::types::validate_cosine_vector(&normalized, dimensions).map_err(adapter_to_kio)?;
    Ok(normalized)
}

/// Submit this pass's pending embeddings as provider batch jobs and return.
///
/// Nothing is collected here: the results arrive at a later `poll_batch_embedding_jobs`
/// (from `kio batch resume` or any write command), the same shape the OCR lane
/// uses. Member tasks stay Pending, so `index_status` keeps reporting them.
#[allow(clippy::too_many_arguments)]
pub(crate) fn submit_embedding_batch_jobs(
    ctx: &Invocation<'_>,
    repo: &Repository,
    execution: EmbeddingExecution,
    conn: &Connection,
    ledger: &LedgerDb,
    profile: &DeclaredEmbeddingProfile,
    embeddable: &[EmbeddableChunk],
    scope_id: &str,
    budget_caps: &BudgetCaps,
    override_budget: bool,
    current_policy: &CurrentPolicyEvaluator,
    projection: &EmbeddingProjection<'_>,
    transitions: &mut BTreeMap<String, EmbeddingTransition>,
    replica: &mut kio_index::aggregator::ScopeDelta,
) -> Result<Option<ExecOutcome>> {
    let adapter_settings = ctx.settings;

    let mut outcome = ExecOutcome::default();
    let contract_holds = kio_pipeline::ledger::ops::unreset_contract_violations_for_scope_adapter(
        ledger,
        scope_id,
        EMBEDDING_ADAPTER_KIND,
    )
    .map_err(pipeline_to_kio)?;
    let plan = plan_embed_batch(conn, profile, embeddable).map_err(|failure| {
        KioError::schema(format!(
            "embedding batch planning failed: {:?}",
            failure.retry_kind
        ))
    })?;
    // The durable batch breaker precedes client resolution: losing the batch
    // client must not turn a blocked batch into an automatic sync resend.
    if !contract_holds.is_empty() && !plan.to_send.is_empty() {
        let attempts = contract_holds
            .iter()
            .map(|row| {
                json!({
                    "input_hash": row.key.input_hash,
                    "tool_profile_hash": row.key.tool_profile_hash,
                    "submission_seq": row.submission_seq,
                    "selector": format!("{}/{}/{}/{}", row.key.scope_id, row.key.adapter_kind, row.key.input_hash, row.key.tool_profile_hash),
                })
            })
            .collect::<Vec<_>>();
        return Err(KioError::new(
            "KIO-E-EMBED-BATCH-CONTRACT-HOLD-001",
            "new embedding batch submissions are held after a malformed result; run kio batch retry --reset-violations <selector> --yes for each blocked attempt",
            json!({"scope_id": scope_id, "adapter_kind": EMBEDDING_ADAPTER_KIND, "blocked_attempts": attempts}),
            ExitCode::PermanentFailure,
        ));
    }
    // The lazy context is loaded only for a configured real client. Invalid
    // recovery authority is a hard hold, never permission to switch lanes.
    let Some(client) = kio_adapter::gemini_batch_client::resolve_gemini_batch_client_with_control(
        adapter_settings,
        || batch_recovery_context(ledger),
        ctx.control,
    )
    .map_err(adapter_to_kio)?
    else {
        // The lane is unavailable (no batch client resolvable). Report that to
        // the caller so it can fall back to the synchronous lane, exactly as
        // `markdownize_send_lane`'s `(false, None) => Sync` rule does — leaving
        // the work pending forever would be worse than paying the sync rate.
        return Ok(None);
    };
    // Reuse links are free and always succeed — settle them here exactly as the
    // sync lane does, so an unavailable provider cannot strand an already
    // materialized chunk.
    if !plan.reuse.is_empty() {
        match link_reused_chunks(conn, profile, &plan.reuse, projection, replica) {
            Ok(()) => {
                record_embedding_transitions(
                    transitions,
                    plan.reuse.iter().map(|(chunk, _)| *chunk),
                    embedding_done_transition(),
                );
                outcome.executed += plan.reuse.len();
            }
            Err(failure) => {
                record_embedding_transitions(
                    transitions,
                    plan.reuse.iter().map(|(chunk, _)| *chunk),
                    embedding_fail_transition(failure.retry_kind),
                );
                count_embedding_failure(&mut outcome, failure.retry_kind, plan.reuse.len());
                return Ok(Some(outcome));
            }
        }
    }
    if plan.to_send.is_empty() {
        return Ok(Some(outcome));
    }
    let caps = budget_cap_config(budget_caps, scope_id, EMBEDDING_ADAPTER_KIND);
    let bypass_cap_denial = override_budget || !budget_caps.hard_stop;
    for job_groups in plan.to_send.chunks(EMBEDDING_BATCH_JOB_MAX_MEMBERS) {
        let member_identities = job_groups
            .iter()
            .map(|group| group.embedding_hash.clone())
            .collect::<BTreeSet<_>>();
        let key = task_ledger_key(
            scope_id,
            EMBEDDING_ADAPTER_KIND,
            &embedding_job_input_hash(&member_identities),
            &profile.profile_hash,
        );
        // R24 (6 系統一致, KNOWN GAP の是正): bound the retries. A job that ends
        // in a non-success terminal settles the row and NULLs its
        // `intent_token`, and `phase1_intent`'s ON CONFLICT clears
        // `batch_job_id` — so without this gate the very next pass re-reserved
        // the same task key and created another job, forever, recording an
        // estimate every time. Neither counter is in that ON CONFLICT SET list,
        // so both survive re-reservation; this uses `contract_violation_count`
        // because `kio batch retry --reset-violations <selector> --yes` can clear it and nothing clears
        // `attempts` (see `EMBEDDING_BATCH_JOB_MAX_FAILURES`).
        let existing = get_batch_request(ledger, &key).map_err(pipeline_to_kio)?;
        if existing.as_ref().is_some_and(|row| {
            !row.state.is_inflight()
                && row.contract_violation_count >= EMBEDDING_BATCH_JOB_MAX_FAILURES
        }) {
            record_embedding_transitions(
                transitions,
                job_groups
                    .iter()
                    .flat_map(|group| group.members.iter().copied()),
                embedding_fail_transition(RetryErrorKind::ContractViolation),
            );
            count_embedding_failure(
                &mut outcome,
                RetryErrorKind::ContractViolation,
                job_groups
                    .iter()
                    .map(|group| group.members.len())
                    .sum::<usize>(),
            );
            continue;
        }
        // This is an already-admitted request. Its poll/recovery owner may
        // continue after a policy change; do not turn a resubmission scan into
        // a fresh admission boundary for it.
        if existing
            .as_ref()
            .is_some_and(|row| row.state.is_inflight() && row.batch_job_id.is_some())
        {
            continue;
        }
        // R24 (3 系統一致): build the inputs BEFORE the reservation so the
        // estimate prices the string actually sent. Estimating
        // `representative.text` while sending
        // `contextualized_embedding_input(context, text)` undercounts by the
        // whole context prefix. Settlement now corrects that from the reported
        // tokens (I4/I12), but the RESERVATION is what the budget cap gates on
        // and what a failed send settles at, so an undercount here still lets
        // spending through the cap that the cap was set to stop (I10).
        let inputs = job_groups
            .iter()
            .map(|group| {
                let context =
                    embedding_store::chunk_embedding_context(&group.representative.raw_path);
                kio_adapter::gemini_batch_client::GeminiBatchEmbedInput {
                    // The provider echoes this back in `metadata.key`; the
                    // collect side keys on it to find the chunk again.
                    key: group.embedding_hash.clone(),
                    text: embedding_store::contextualized_embedding_input(
                        context.as_deref(),
                        &group.representative.text,
                    ),
                }
            })
            .collect::<Vec<_>>();
        validate_embedding_aliases(
            current_policy,
            profile,
            job_groups
                .iter()
                .flat_map(|group| group.members.iter().copied()),
        )
        .map_err(|_| KioError::schema("embedding owner policy changed before batch admission"))?;
        // One reservation for the whole job — the cap is checked once against
        // the job's total, which is also what the provider will bill.
        let candidate_usd: f64 = inputs
            .iter()
            .map(|input| {
                estimate_embedding_cost(adapter_settings, &input.text, PreferredRequestKind::Batch)
            })
            .sum();
        // Final check before this new job's reservation/admission. Existing
        // submitted jobs are polled elsewhere and deliberately do not pass
        // through this new-submission boundary.
        admit_embedding_transport(
            adapter_settings,
            repo,
            execution,
            profile,
            current_policy,
            job_groups
                .iter()
                .flat_map(|group| group.members.iter())
                .any(|chunk| chunk.requires_secret_approval),
        )?;
        let charge = reserve_or_reuse_task_charge(
            ctx,
            ledger,
            &key,
            candidate_usd,
            &caps,
            bypass_cap_denial,
            RequestKind::Batch,
        )?;
        let (intent_token, _reserved_usd) = match charge {
            TaskChargeOutcome::BudgetExceeded => {
                record_embedding_transitions(
                    transitions,
                    job_groups
                        .iter()
                        .flat_map(|group| group.members.iter().copied()),
                    embedding_pause_transition(),
                );
                outcome.paused += job_groups
                    .iter()
                    .map(|group| group.members.len())
                    .sum::<usize>();
                continue;
            }
            TaskChargeOutcome::Reserved {
                intent_token,
                estimated_usd,
            }
            | TaskChargeOutcome::Reused {
                intent_token,
                estimated_usd,
            } => (intent_token, estimated_usd),
        };
        // 相 2a is degenerate for an inline batch (no upload), so the provider
        // scope is recorded immediately before job creation instead.
        //
        // G2: a provider-side failure here fails THIS job, not the invocation.
        // These calls used to `?` straight out of the function, so one
        // transient 429 aborted `kio index` for the whole scope — and, worse,
        // did it after 相 2b was recorded, leaving the row stranded. The
        // synchronous lane has always classified and recorded per chunk
        // instead; this makes the Batch lane behave the same way. The stranded
        // row is not a leak: §5.8's recovery walk owns it (G1 wired the Gemini
        // provider into that walk, which is why this can safely give up here).
        let provider_scope_id = match client.provider_scope_id() {
            Ok(id) => id,
            Err(error) => {
                record_batch_submit_failure(&mut outcome, transitions, job_groups, error);
                continue;
            }
        };
        if !phase2a_record_provider_scope(ledger, &key, &intent_token, &provider_scope_id)
            .map_err(pipeline_to_kio)?
        {
            continue;
        }
        if !phase2b_record_job_create_started(ledger, &key, &intent_token)
            .map_err(pipeline_to_kio)?
        {
            continue;
        }
        let job = match client.create_embedding_job(
            kio_adapter::catalog::ADOPTED_EMBEDDING_MODEL_PIN,
            u32::try_from(profile.dimensions).unwrap_or(u32::MAX),
            &kio_adapter::gemini_batch_client::batch_display_name(&intent_token),
            &inputs,
        ) {
            Ok(job) => job,
            Err(error) => {
                record_batch_submit_failure(&mut outcome, transitions, job_groups, error);
                continue;
            }
        };
        phase2b_record_job_created(ledger, &key, &intent_token, &job.name)
            .map_err(pipeline_to_kio)?;
        outcome.inflight += job_groups
            .iter()
            .map(|group| group.members.len())
            .sum::<usize>();
    }
    Ok(Some(outcome))
}

/// 04 §5.8 相 3 for this scope's in-flight embedding batch jobs.
///
/// Polling receives an EXISTING request, so like the OCR lane it needs no
/// network opt-in. Rows whose job id is still unknown belong to the §5.8
/// recovery walk (`kio ledger reconcile`), not here — `batch_poll_candidates`
/// only returns rows with a durable `batch_job_id`.
pub(crate) fn poll_batch_embedding_jobs(
    ctx: &Invocation<'_>,
    repo: &Repository,
    ledger: &LedgerDb,
) -> Result<ExecOutcome> {
    let adapter_settings = ctx.settings;

    let mut outcome = ExecOutcome::default();
    let Some(execution) = embedding_execution(adapter_settings, ctx.control) else {
        return Ok(outcome);
    };
    // 07 §5.5: the lane is a property of the adapter. An `offline_api` adapter
    // prefers Sync because a local server has no provider job queue to submit
    // to, and the half-price rationale that puts Gemini on Batch does not
    // exist for it. There can be no in-flight rows to poll.
    if embedding_preferred_request_kind_with_control(adapter_settings, execution, ctx.control)
        .map_err(adapter_to_kio)?
        != PreferredRequestKind::Batch
    {
        return Ok(outcome);
    }
    let profile = declared_embedding_profile(execution);
    // The lazy context is loaded only for a configured real client. Invalid
    // recovery authority is a hard hold, never permission to switch lanes.
    let Some(client) = kio_adapter::gemini_batch_client::resolve_gemini_batch_client_with_control(
        adapter_settings,
        || batch_recovery_context(ledger),
        ctx.control,
    )
    .map_err(adapter_to_kio)?
    else {
        return Ok(outcome);
    };
    let scope_id = repo.scope_identity()?.scope_id;
    recover_terminal_embedding_batch_cleanup(ctx, ledger, client.as_ref(), &scope_id)?;
    let rows = batch_poll_candidates(ledger, &scope_id, EMBEDDING_ADAPTER_KIND)
        .map_err(pipeline_to_kio)?;
    if rows.is_empty() {
        return Ok(outcome);
    }
    let db_path = sqlite_path(repo.kio_dir());
    if !db_path.exists() {
        return Ok(outcome);
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
        return Ok(outcome);
    };
    // Batch collection is likewise scoped by the required HEAD TreeObject
    // field, never by a mutable working-tree config.toml.
    let instances = retained_history_instances(repo, &head)?;
    let chunks =
        embedding_owners::collect_retained_embedding_chunks(repo, &conn, &instances, None, None)?;
    let current_policy = current_embedding_policy(repo)?;
    let selected_contexts = selected_embedding_contexts(&chunks, &current_policy)?;
    let chunks = bind_embedding_work(chunks, &profile)?;
    let by_embedding_hash = chunks
        .iter()
        .map(|chunk| (chunk.work_key.embedding_hash().to_owned(), chunk))
        .collect::<BTreeMap<_, _>>();
    let mut held = BTreeSet::new();
    if secret_embedding_hold_required(
        execution.execution_mode(),
        embedding_secrets_allowed(adapter_settings, repo, &profile)?,
        true,
    ) {
        held.extend(held_secret_embedding_chunk_ids(
            repo.kio_dir(),
            repo.control(),
        )?);
        held.extend(
            chunks
                .iter()
                .filter(|chunk| chunk.requires_secret_approval)
                .map(|chunk| chunk.chunk_id.clone()),
        );
    }
    let projection = EmbeddingProjection {
        repo,
        policy: &current_policy,
        selected_contexts: &selected_contexts,
        invalidated: std::cell::Cell::new(false),
    };
    refresh_embedding_projection(ctx, repo, &conn, &chunks, &profile, &projection, &held)?;
    // 05 §1.8 write-through, accumulated across every collected job and flushed
    // once below — see the sync lane's identical accumulator, including why the
    // generation is read here rather than at the flush.
    let replica_before = replica_scope_stamp(repo.kio_dir()).map(|(_, generation)| generation);
    let mut replica = kio_index::aggregator::ScopeDelta::default();
    let mut source_written = false;
    for row in rows {
        let (Some(job_name), Some(intent_token)) =
            (row.batch_job_id.as_ref(), row.intent_token.as_ref())
        else {
            continue;
        };
        // G2: 04 §5.8 unknown — 何も変更せず保持し、次回再試行する。This used to
        // `?` out of the whole function, so ONE unreachable row blocked
        // collection for every other row in the scope (head-of-line blocking)
        // and reported a transient network error as a permanent config fault.
        // The OCR lane has always held-and-retried here; this matches it.
        // I8: ONE fetch. The state and the results live in the same document,
        // so asking twice downloaded a finished job's payload twice and let the
        // two reads take different byte bounds — which is how every job past
        // ~20 members came to be silently unreadable.
        let Some(poll) = poll_attributed_embedding_batch_job(ctx, client.as_ref(), &row, None)
        else {
            outcome.inflight += 1;
            outcome.held_unreadable += 1;
            continue;
        };
        let job = poll.record;
        if !job.state.is_terminal() {
            outcome.inflight += 1;
            continue;
        }
        if job.state != kio_adapter::gemini_batch_client::GeminiBatchState::Succeeded {
            // Failed / cancelled / expired: terminate the row so the members
            // become eligible for a fresh submission, and record the estimate
            // (the provider may still have billed part of the job).
            settle_batch_charge_terminal(
                ledger,
                &row.key,
                intent_token,
                job_name,
                // §5.8 相 3: a provider-side non-success terminal. `Expired`
                // is the closed-enum outcome for "the job ended without
                // usable output" (the OCR lane uses it the same way).
                Outcome::Expired,
                BatchState::Terminal,
                Some("embedding batch job did not succeed"),
                BilledAmount {
                    usd: row.estimated_usd,
                    estimated: true,
                },
                // R24: count it. This is the durable counter the submit side
                // bounds re-sends on, and `kio batch retry --reset-violations <selector> --yes` is its
                // operator escape hatch.
                true,
                true,
            )?;
            outcome.failed += 1;
            continue;
        }
        // G2: same §5.8 unknown rule. The job says SUCCEEDED but the response
        // carried no results — settling on that would either discard the
        // vectors (Succeeded with nothing persisted) or burn a retry
        // (Terminal). Hold the row instead; the next pass re-reads it.
        let Some(results) = poll.results else {
            ctx.interaction.diagnostic(&format!(
                "kio: holding {job_name}: job succeeded but carried no results"
            ));
            outcome.inflight += 1;
            outcome.held_unreadable += 1;
            continue;
        };
        // The row binds only a digest of the original member set. Establish
        // the complete bijection before the first CAS or source-table write.
        let validated_vectors = match validated_embedding_batch_vectors(
            &results,
            &row.key.input_hash,
            (row.key.tool_profile_hash == profile.profile_hash).then_some(&profile),
        ) {
            Ok(vectors) => vectors,
            Err(_) => {
                settle_batch_charge_terminal(
                    ledger,
                    &row.key,
                    intent_token,
                    job_name,
                    Outcome::ContractViolation,
                    BatchState::Terminal,
                    Some(EMBEDDING_BATCH_RESULT_CONTRACT_ERROR),
                    BilledAmount {
                        usd: row.estimated_usd,
                        estimated: true,
                    },
                    true,
                    true,
                )?;
                outcome.failed += 1;
                continue;
            }
        };
        let mut persisted = 0usize;
        // R24 (4 系統一致): 07 §5.3 (1) requires the result ids to be a BIJECTION
        // onto the input ids, and this loop checked neither direction — it
        // iterated the results and `continue`d past anything it could not
        // resolve, then settled `Succeeded` unconditionally. A provider that
        // returned 511 of 512 embeddings therefore completed the row, cleared
        // its `intent_token`, and left the missing chunk with no vector and no
        // failed task: a silent, permanent hole in the index.
        //
        // The bijection is checkable with no schema change because the task key
        // IS the member digest (design A): re-derive the digest from the
        // results that actually resolved and compare it with the row's own
        // `input_hash`.
        for result in &results {
            let Some(chunk) = by_embedding_hash.get(&result.key) else {
                // The chunk is gone (purged / reconfigured) since submission —
                // nothing to write, and nothing to fail either.
                continue;
            };
            let Some(normalized) = validated_vectors.get(&result.key) else {
                continue;
            };
            let embedding_hash = chunk_embedding_hash(chunk, &profile)?;
            // Re-derive the group from the CURRENT chunk set rather than
            // trusting the submitted membership: a chunk added since
            // submission that shares this content identity must be linked too.
            let members = chunks
                .iter()
                .filter(|candidate| {
                    candidate.work_key.embedding_hash() == embedding_hash
                        && current_policy
                            .allows_path(&candidate.raw_path)
                            .unwrap_or(false)
                })
                .collect::<Vec<_>>();
            let group = EmbeddingSendGroup {
                embedding_hash,
                representative: chunk,
                members,
            };
            persist_group_vector(
                &conn,
                &profile,
                &group,
                normalized,
                &held,
                &projection,
                &mut replica,
            )
            .map_err(|failure| {
                KioError::schema(format!(
                    "embedding batch collect failed to persist: {:?}",
                    failure.retry_kind
                ))
            })?;
            source_written = true;
            persisted += group.members.len();
        }
        // Membership was authenticated before persistence. A retired profile
        // or denied context can settle under its original job binding without
        // becoming a current live vector.
        let reported = reported_prompt_tokens(&results).map(|tokens| {
            kio_adapter::types::AdapterUsage::BillableUnits {
                billable_units: vec![kio_adapter::types::BillableUnit {
                    kind: kio_adapter::types::BillableUnitKind::TokensIn,
                    count: tokens,
                }],
            }
        });
        settle_batch_charge_terminal(
            ledger,
            &row.key,
            intent_token,
            job_name,
            Outcome::Succeeded,
            BatchState::Completed,
            None,
            embedding_billed_from_usage(
                adapter_settings,
                reported.as_ref(),
                PreferredRequestKind::Batch,
                row.estimated_usd,
                ctx.control,
                ctx.locks,
            ),
            false,
            true,
        )?;
        outcome.executed += persisted;
    }
    // A collected batch writes vectors straight into the LIVE index, and
    // `run_batch` — unlike `run_index`/`run_reindex` — never rebuilds it
    // afterward, so `index_generation` would otherwise still name the
    // pre-embedding state. That retires every outstanding search cursor, which
    // is what LC25 requires of any change that can make a replay rank
    // differently. `publish_in_place_delta` owns both halves — it rotates and
    // then stamps the replica with what the index now carries.
    //
    // Its empty-delta no-op replaces the former `outcome.executed > 0` test,
    // and narrows it: a group whose every member is secrets-held writes a
    // `content_vectors` row (so `executed` counts it) but links nothing into
    // `chunk_vec`, and search reads `chunk_vec`. No replay can rank
    // differently, so LC25 asks for no rotation — the old trigger retired
    // cursors for a change no search could see.
    current_policy.revalidate().map_err(pipeline_to_kio)?;
    publish_in_place_delta(ctx, repo.kio_dir(), replica_before.as_deref(), &replica)?;
    if source_written && replica.is_empty() {
        write_through_projection_or_log_for_repo(ctx, repo);
    }
    Ok(outcome)
}

/// Provider scope is the configured local identity, not a network lookup.
/// Missing, unreadable, or changed authority never permits recovery effects.
pub(crate) fn batch_provider_scope_matches(
    stored: Option<&str>,
    current: impl FnOnce() -> kio_adapter::Result<String>,
) -> bool {
    let Some(stored) = stored.filter(|scope| !scope.is_empty()) else {
        return false;
    };
    current().is_ok_and(|current| current == stored)
}

/// Both collection and terminal cleanup use the same exact Gemini attribution.
/// Collection additionally binds the active vector profile before any poll.
pub(crate) fn poll_attributed_embedding_batch_job<
    C: kio_adapter::gemini_batch_client::GeminiBatchClient + ?Sized,
>(
    ctx: &Invocation<'_>,
    client: &C,
    row: &BatchRequestRow,
    required_profile: Option<&str>,
) -> Option<kio_adapter::gemini_batch_client::GeminiBatchPoll> {
    if required_profile.is_some_and(|profile| row.key.tool_profile_hash != profile)
        || !batch_provider_scope_matches(row.provider_scope_id.as_deref(), || {
            client.provider_scope_id()
        })
    {
        ctx.interaction
            .diagnostic("kio: holding embedding batch: provider scope or profile does not match");
        return None;
    }
    let job_name = row.batch_job_id.as_deref()?;
    let intent_token = row.intent_token.as_deref()?;
    let poll = match client.poll_job(job_name) {
        Ok(poll) => poll,
        Err(error) => {
            ctx.interaction.diagnostic(&format!(
                "kio: holding {job_name}: poll unreadable: {error}"
            ));
            return None;
        }
    };
    if poll.record.name != job_name
        || poll.record.display_name
            != kio_adapter::gemini_batch_client::batch_display_name(intent_token)
    {
        ctx.interaction.diagnostic(&format!(
            "kio: holding {job_name}: returned job attribution does not match"
        ));
        return None;
    }
    Some(poll)
}

/// Finish only the narrow, no-residue cleanup case for an already-terminal
/// Gemini embedding job. This never creates, cancels, or resubmits a job.
pub(crate) fn recover_terminal_embedding_batch_cleanup<
    C: kio_adapter::gemini_batch_client::GeminiBatchClient + ?Sized,
>(
    ctx: &Invocation<'_>,
    ledger: &LedgerDb,
    client: &C,
    scope_id: &str,
) -> Result<()> {
    for row in kio_pipeline::ledger::ops::recovery_candidates(ledger).map_err(pipeline_to_kio)? {
        if row.key.scope_id != scope_id
            || row.key.adapter_kind != EMBEDDING_ADAPTER_KIND
            || !row.state.is_terminal()
        {
            continue;
        }
        let Some(intent_token) = row.intent_token.as_deref() else {
            continue;
        };
        let Some(poll) = poll_attributed_embedding_batch_job(ctx, client, &row, None) else {
            continue;
        };
        let job = poll.record;
        let job_name = &job.name;
        if !job.state.is_terminal() || job.responses_file.is_some() {
            ctx.interaction.diagnostic(&format!(
                "kio: holding {job_name}: terminal cleanup attribution or output is not safe"
            ));
            continue;
        }
        recovery_finish_cleanup(ledger, &row.key, intent_token).map_err(pipeline_to_kio)?;
    }
    Ok(())
}
