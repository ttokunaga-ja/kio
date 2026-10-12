use crate::approvals;
use crate::egress::SECRETS_TIER_B_HOLD;
use crate::egress::admit_embedding_transport;
use crate::egress::embedding_secrets_allowed;
use crate::egress::persistent_network_allowed_for;
use crate::embedding::enrichment::EmbeddableChunk;
use crate::embedding::enrichment::EmbeddingProjection;
use crate::embedding::enrichment::EmbeddingSendGroup;
use crate::embedding::enrichment::EmbeddingTransition;
use crate::embedding::enrichment::bind_embedding_work;
use crate::embedding::enrichment::count_embedding_failure;
use crate::embedding::enrichment::embedding_done_transition;
use crate::embedding::enrichment::embedding_fail_transition;
use crate::embedding::enrichment::embedding_failure_transition;
use crate::embedding::enrichment::embedding_pause_transition;
use crate::embedding::enrichment::embedding_task_output_ref;
use crate::embedding::enrichment::link_reused_chunks;
use crate::embedding::enrichment::persist_group_vector;
use crate::embedding::enrichment::plan_embed_batch;
use crate::embedding::enrichment::record_embedding_transitions;
use crate::embedding::enrichment::refresh_embedding_projection;
use crate::embedding::enrichment::secret_embedding_hold_required;
use crate::embedding::enrichment::selected_embedding_contexts;
use crate::embedding::enrichment::validate_embedding_aliases;
use crate::embedding::enrichment::validate_embedding_task_bindings;
use crate::embedding_owners;
use crate::embedding_owners::RetainedEmbeddingChunk;
use crate::historical_reindex::retained_history_instances;
use crate::index::chunk_ledger::sqlite_path;
use crate::index::rebuild::held_secret_embedding_chunk_ids;
use crate::invocation::Invocation;
use crate::ledger_access::RETIRED_NON_LIVE;
use crate::ledger_access::TASK_SYNC_EFFECTIVE_TIMEOUT_SECONDS;
use crate::ledger_access::TaskChargeOutcome;
use crate::ledger_access::budget_cap_config;
use crate::ledger_access::open_ledger_db;
use crate::ledger_access::reserve_or_reuse_task_charge;
use crate::ledger_access::settle_task_charge_success;
use crate::ledger_access::settle_task_charge_unknown;
use crate::ledger_access::task_ledger_key;
use crate::ledger_access::warn_uncovered_pricing_kinds_once;
use crate::markdownize::batch::settle_batch_charge_terminal;
use crate::markdownize::execution::TaskExecutionFailure;
use crate::markdownize::execution::task_failure_from_adapter;
use crate::object_persistence::adapter_to_kio;
use crate::object_persistence::index_to_kio;
use crate::object_persistence::pipeline_to_kio;
use crate::scope_access::maybe_wait_at_test_search_final_consent_barrier;
use crate::search::projection::publish_in_place_delta;
use crate::search::projection::replica_scope_stamp;
use crate::search::projection::write_through_projection_or_log_for_repo;
use crate::search::scope::ExecScope;
use crate::search::scope::current_embedding_policy;
use crate::search::scope::revalidate_current_policy_map;
use crate::settings::batch_recovery_context;
use crate::settings::user_config_toml_path;
use crate::task_execution::ExecOutcome;
use crate::task_execution::task_retry_allowed;
use crate::task_execution::task_retry_due;
use kio_adapter::catalog::DeclaredEmbeddingProfile;
use kio_adapter::catalog::EmbeddingExecution;
use kio_adapter::catalog::active_embedding_execution_with_control;
use kio_adapter::catalog::declared_embedding_profile_for;
use kio_adapter::catalog::embedding_adapter_profile_with_control;
use kio_adapter::catalog::embedding_preferred_request_kind_with_control;
use kio_adapter::catalog::run_embedding_with_control;
use kio_adapter::tool_lock::AdapterRuntimeSettings;
use kio_adapter::traits::PreferredRequestKind;
use kio_adapter::types::EmbeddingInputType;
use kio_adapter::types::EmbeddingItem;
use kio_adapter::types::ExecutionMode;
use kio_core::ExitCode;
use kio_core::KioError;
use kio_core::Result;
use kio_core::cas::is_hash;
use kio_core::control::ControlHandle;
use kio_core::scope::Repository;
use kio_index::EmbeddingDistance;
use kio_index::EmbeddingModality;
use kio_index::EmbeddingTargetType;
use kio_index::embedding_store;
use kio_index::fts::ExistingSourceIndexOpenMode;
use kio_index::fts::FtsSchemaConfig;
use kio_index::fts::FtsTokenizer;
use kio_index::fts::open_existing_source_index_connection;
use kio_pipeline::budget::BudgetCaps;
use kio_pipeline::budget::read_budget_policy;
use kio_pipeline::ledger::BatchRequestRow;
use kio_pipeline::ledger::BatchState;
use kio_pipeline::ledger::LedgerDb;
use kio_pipeline::ledger::Outcome;
use kio_pipeline::ledger::RequestKind;
use kio_pipeline::ledger::TaskKey as LedgerTaskKey;
use kio_pipeline::ledger::ops::BilledAmount;
use kio_pipeline::ledger::ops::ClaimOutcome;
use kio_pipeline::ledger::ops::batch_poll_candidates;
use kio_pipeline::ledger::ops::device_input_hash;
use kio_pipeline::ledger::ops::get_batch_request;
use kio_pipeline::ledger::ops::phase2a_record_provider_scope;
use kio_pipeline::ledger::ops::phase2b_record_job_create_started;
use kio_pipeline::ledger::ops::phase2b_record_job_created;
use kio_pipeline::ledger::ops::recovery_finish_cleanup;
use kio_pipeline::ledger::ops::resolve_billing_from_reported_usage;
use kio_pipeline::policy::CurrentPolicyEvaluator;
use kio_pipeline::task::RetryErrorKind;
use kio_pipeline::task::TaskDescriptor;
use kio_pipeline::task::TaskStatus;
use kio_pipeline::task::TaskStore;
use kio_pipeline::task::TaskType;
use rusqlite::Connection;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

// ===========================================================================
// K4 — embedding catalog wiring (adapter selection, index generation, query
// embedding).
// ===========================================================================

pub(crate) const EMBEDDING_ADAPTER_KIND: &str = "embedding";
/// Chunks planned, reserved, and sent per pass of the sync send loop.
///
/// NOT the adapter-call granularity: `send_embed_group` makes one call per
/// group, so that a row settles on a token count the provider reported for
/// that row. This only bounds how much work one plan/reserve cycle takes on.
pub(crate) const EMBEDDING_BATCH_SIZE: usize = 32;

pub(crate) fn embedding_execution(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    control: &ControlHandle,
) -> Option<EmbeddingExecution> {
    active_embedding_execution_with_control(adapter_settings, control)
}

pub(crate) fn declared_embedding_profile(
    execution: EmbeddingExecution,
) -> DeclaredEmbeddingProfile {
    declared_embedding_profile_for(execution)
}

/// Whether the active embedding execution is the remote, billable API lane
/// (07 §3).
///
/// An `OfflineApi` may still be an authenticated local HTTPS peer, so this is
/// deliberately not a transport-safety predicate. That predicate is
/// [`embedding_transport_allowed`]. The cost ledger and batch lane each need
/// this remote-versus-local distinction and nothing finer.
pub(crate) fn embedding_is_online(execution: EmbeddingExecution) -> bool {
    execution.is_online()
}

/// The evaluator runs in this process and never opens a transport. It is the
/// only embedding execution exempt from network grants and `--offline`.
pub(crate) fn embedding_is_deterministic(execution: EmbeddingExecution) -> bool {
    embedding_mode_is_in_process(execution.execution_mode())
}

fn embedding_mode_is_in_process(mode: ExecutionMode) -> bool {
    mode == ExecutionMode::DeterministicLibrary
}

pub(crate) fn embedding_mode_transport_allowed(mode: ExecutionMode, offline: bool) -> bool {
    embedding_mode_is_in_process(mode) || !offline
}

pub(crate) fn embedding_transport_allowed(execution: EmbeddingExecution, offline: bool) -> bool {
    embedding_mode_transport_allowed(execution.execution_mode(), offline)
}

/// Run the active embedding adapter over a batch of items, returning one vector
/// per item.
pub(crate) fn run_embedding_adapter(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    execution: EmbeddingExecution,
    items: Vec<EmbeddingItem>,
    input_type: EmbeddingInputType,
    // QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): the caller's
    // ledger `intent_token` for this send — see
    // `kio_adapter::types::EmbeddingRequest::idempotency_token`'s doc.
    idempotency_token: Option<String>,
    control: &ControlHandle,
) -> std::result::Result<kio_adapter::catalog::AdoptedEmbeddingOutcome, TaskExecutionFailure> {
    run_embedding_with_control(
        std::sync::Arc::clone(adapter_settings),
        execution,
        items,
        input_type,
        idempotency_token,
        control,
    )
    .map_err(task_failure_from_adapter)
}

/// The `fallback_reason`s [`compute_query_embedding_page1`] can report through
/// `VectorAvailability::Unavailable` beyond the pre-existing
/// `query_embedding_unavailable` (05 §1.7).
pub(crate) enum QueryEmbeddingOutcome {
    Vector(Vec<f32>),
    /// CL54: a live (non-stale) in-flight claim from another process already
    /// holds this exact query's device row — never send, never overwrite its
    /// token.
    InFlight,
    /// PC7 (05 §1.1 L32 / 07-adapter-spec.md §5.3): the adapter's response
    /// failed its acceptance check (e.g. a dimension mismatch) — a distinct
    /// technical failure from a plain adapter error.
    ContractViolation,
    /// PC6 (05 §1.1 L50-53 / 07 §3 L144-146): the final pre-attempt re-read
    /// found the send consent gate no longer satisfied (a revoke completed
    /// between the caller's precheck and this re-read) — refuse the send.
    /// Maps to
    /// `VectorAvailability::Unauthorized`, not `Unavailable`, at the caller.
    NotAuthorized,
}

/// vector|hybrid search page 1's query embedding call, wired onto the sync
/// degenerate 2-phase device row (04 §5.4 §H, CL48-55 in
/// tasks/step4b-contract-tests-ledger.md): a bounded sweep of stale/prunable
/// device rows first (CL52 — own key unconditionally, pruning >= 128 of the
/// shared 256-row cap), then phase 1 claim (`device_claim`: CL54 in-flight
/// text-fallback, or a fresh/reclaimed-and-fresh `stale_after_at` reservation),
/// the response's provider request id recorded durably BEFORE the terminal Tx
/// (CL43 — this Adapter integration reports no real id, so the intent_token is
/// used, the DDL's own documented fallback), and a terminal settlement
/// (success billed at the reservation estimate, or `unknown_settled` on
/// adapter failure — CL45/CL69's "no post-hoc query capability" posture, same
/// as the task-charge helpers above). R23-02: device rows are NOT exempt from
/// the device/per_adapter(embedding) cap — `ops::device_claim` now performs
/// that judgement itself, inside the same claim Tx, and returns `Denied` on
/// exceedance (04 §5.4: "device cap / per_adapter (embedding) の合算には通常
/// どおり含まれる — 判定式は不変"). Only folder cap does not apply, matching
/// the same clause's "folder cap 判定 ... には現れず" — this claim has no
/// single owning `.kio` folder to read one from (`exec_scopes` may span zero,
/// one, or many), so only the device-layer config (`~/.config/kio/config.toml`)
/// is consulted, never a folder's `.kio/config.toml`.
///
/// Only called for a FRESH page 1 (no cursor) — a cursor replay (page 2+) calls
/// the unmetered [`compute_query_embedding`] unchanged, exactly as before this
/// session's ledger migration (item 2 of the implementation instructions scopes
/// the device-row protocol to "page 1" specifically; 04 §5.4 §H's own text is
/// page-1-scoped: "vector|hybrid 検索 page 1 の query embedding 呼出").
///
/// The caller supplied the policy-filtered scopes and their retained repository
/// handles. This function selects one authorized scope, holds its writer lease,
/// and performs the final exact device+scope grant check before the adapter call.
pub(crate) fn compute_query_embedding_page1(
    invocation: &Invocation<'_>,
    query: &str,
    exec_scopes: &[ExecScope],
    search_repositories: &BTreeMap<String, Repository>,
    tool_id: &str,
    offline: bool,
    current_policies: &BTreeMap<String, CurrentPolicyEvaluator>,
) -> Result<Option<QueryEmbeddingOutcome>> {
    let adapter_settings = invocation.settings;

    if query.chars().count() < 2 {
        return Ok(None);
    }
    let Some(execution) = embedding_execution(adapter_settings, invocation.control) else {
        return Ok(None);
    };
    if embedding_is_deterministic(execution) {
        revalidate_current_policy_map(current_policies, false)?;
        return local_query_embedding(adapter_settings, execution, query, invocation.control);
    }
    if !embedding_transport_allowed(execution, offline) {
        return Ok(None);
    }
    let profile = declared_embedding_profile(execution);

    // PC6: the final authorization check is deliberately before *any* writable
    // source ledger action. A revoked auto/hybrid candidate therefore resolves to its
    // normal text fallback without opening the ledger, refreshing write-seq,
    // sweeping stale rows, or creating a claim. Once the check succeeds, later
    // claim/reservation/send/settlement outcomes remain the normal fresh-page-1
    // metered path.
    // Select one currently-authorized scope deterministically, then acquire only
    // that scope's retained writer lease. Rechecking the exact device+scope grant
    // while the lease is held makes a concurrent revoke either win before this
    // request or wait until this in-flight request completes.
    let mut candidates = exec_scopes
        .iter()
        .filter_map(|exec| {
            search_repositories
                .get(&exec.target.scope_id)
                .map(|repo| (exec.target.scope_id.as_str(), repo))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(scope_id, _)| *scope_id);
    let mut authorizing_repo = None;
    for (_, repo) in candidates {
        if persistent_network_allowed_for(
            adapter_settings,
            repo,
            approvals::AdapterRole::Embedding,
            tool_id,
            &profile.profile_hash,
        )? {
            authorizing_repo = Some(repo);
            break;
        }
    }
    let Some(authorizing_repo) = authorizing_repo else {
        return Ok(Some(QueryEmbeddingOutcome::NotAuthorized));
    };
    maybe_wait_at_test_search_final_consent_barrier(invocation.control);
    let _authorizing_scope_lock =
        authorizing_repo.lock_store_in_session(authorizing_repo.lock_session())?;
    revalidate_current_policy_map(current_policies, false)?;
    if !persistent_network_allowed_for(
        adapter_settings,
        authorizing_repo,
        approvals::AdapterRole::Embedding,
        tool_id,
        &profile.profile_hash,
    )? {
        return Ok(Some(QueryEmbeddingOutcome::NotAuthorized));
    }

    if !embedding_is_online(execution) {
        return local_query_embedding(adapter_settings, execution, query, invocation.control);
    }

    let ledger = open_ledger_db(invocation.control)?;
    let conn = &ledger;
    let key = LedgerTaskKey::new(
        LedgerTaskKey::DEVICE_SCOPE_ID,
        EMBEDDING_ADAPTER_KIND,
        device_input_hash(query),
        profile.profile_hash.clone(),
    );

    // CL52: bounded sweep BEFORE the claim — own key's stale row (if any) is
    // swept unconditionally (outside the 256 cap), pruning gets >= 128 of the
    // shared cap, decided by `allocate_sweep_capacity`'s fixed, deterministic
    // rule. Never queries the provider (CL53 — search stays responsive).
    let now = kio_pipeline::ledger::time::now_millis();

    // §M note-2: the "effective timeout" is max(participating scopes' resolved
    // `timeout_seconds`) — currently a no-op maximum, since
    // `adapter.policy.timeout_seconds` other than the documented default (300)
    // is loud-rejected at config load (`kio_core::scope`'s R12-2 decision: real
    // per-adapter timeout wiring is a separate, larger change no scope's config
    // can currently deviate from), so every participating scope's resolved
    // value is always exactly 300 and the max is trivially 300 too.
    //
    // R23-02: resolve the device-layer cap config (no folder cap — this claim
    // has no single owning `.kio` folder). `read_budget_policy` requires a
    // second (folder) path argument that this call has no legitimate value
    // for; a deliberately-nonexistent sentinel path reads back as an empty
    // `ParsedBudgetConfig::default()` (`NotFound` → defaults, `budget.rs`'s own
    // documented behavior), and `budget_cap_config`'s resulting `folder_cap` is
    // never read below — only `device_cap`/`device_per_adapter_cap` are used.
    let device_config_path = user_config_toml_path(invocation.control);
    let no_folder_cap_sentinel =
        device_config_path.with_file_name("__kio_device_claim_no_folder_cap__.toml");
    let budget_caps = read_budget_policy(&device_config_path, &no_folder_cap_sentinel)
        .map_err(pipeline_to_kio)?;
    let device_cap_config = budget_cap_config(
        &budget_caps,
        LedgerTaskKey::DEVICE_SCOPE_ID,
        EMBEDDING_ADAPTER_KIND,
    );
    let claim = kio_pipeline::ledger::ops::sweep_then_device_claim(
        conn,
        &key,
        now,
        estimate_embedding_cost(adapter_settings, query, query_embedding_send_lane()),
        TASK_SYNC_EFFECTIVE_TIMEOUT_SECONDS,
        device_cap_config.device_cap,
        device_cap_config.device_per_adapter_cap,
    )
    .map_err(pipeline_to_kio)?;
    let intent_token = match claim {
        ClaimOutcome::InFlight => return Ok(Some(QueryEmbeddingOutcome::InFlight)),
        // R23-02: cap exceeded — no reservation was made; fall back to text
        // search via the existing bare-`None` branch at the search command's
        // own call site (`vector_unavailable_reason` stays at its default
        // `"query_embedding_unavailable"`). A dedicated `"budget_denied"`
        // reason would need a new `QueryEmbeddingOutcome` variant, which would
        // widen the exhaustive match at that call site (main.rs ~L1771) —
        // that call site is outside this item's assigned main.rs range and is
        // owned by a parallel agent for this fix wave, so it is left
        // unmodified and this is flagged in the report.
        ClaimOutcome::Denied(_layer) => return Ok(None),
        ClaimOutcome::Claimed(outcome) => outcome.intent_token,
    };

    // O2 regression seam: mark that the query is about to be SENT to the embedding
    // endpoint, so a test can prove `--text` never reaches this path. No-op unless
    // the env var is set (mirrors the KIO_TEST_* adapter seams).
    record_query_embed_trace(query, invocation.control);
    let items = vec![EmbeddingItem::text("query", query)];
    // QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): a single-item
    // sync query embed with its own single ledger reservation. Both sync sites
    // are now 1 token to 1 HTTP call — `send_embed_group` reaches it by
    // sending one group per call rather than by being inherently single.
    match run_embedding_adapter(
        adapter_settings,
        execution,
        items,
        EmbeddingInputType::Query,
        Some(intent_token.clone()),
        invocation.control,
    ) {
        Ok(outcome) => {
            // This adapter returns usage and vectors but no provider request
            // identifier or result-lookup capability. Keep batch_job_id absent;
            // a local intent token is not a queryable provider request ID.
            // I12: `:batchEmbedContents` reports `usageMetadata.promptTokenCount`
            // as a per-CALL total, which is the granularity it bills at, so this
            // settles on the measured number. The reservation estimate remains
            // the fallback for a response that carried no usable report.
            let billed = embedding_billed_from_usage(
                adapter_settings,
                outcome.usage.as_ref(),
                query_embedding_send_lane(),
                estimate_embedding_cost(adapter_settings, query, query_embedding_send_lane()),
                invocation.control,
                invocation.locks,
            );
            settle_task_charge_success(&ledger, &key, &intent_token, billed)?;
            Ok(outcome
                .vectors
                .into_iter()
                .next()
                .map(|vector| QueryEmbeddingOutcome::Vector(vector.vector)))
        }
        Err(failure) => {
            let billed_usd =
                estimate_embedding_cost(adapter_settings, query, query_embedding_send_lane());
            settle_task_charge_unknown(&ledger, &key, &intent_token, billed_usd)?;
            // PC7: classify a contract-violation adapter failure distinctly
            // from a generic technical unavailability.
            if failure.retry_kind == RetryErrorKind::ContractViolation {
                Ok(Some(QueryEmbeddingOutcome::ContractViolation))
            } else {
                Ok(None)
            }
        }
    }
}

/// The non-remote counterpart of [`compute_query_embedding_page1`]: embed the
/// query and return the vector with no ledger row. The caller has already
/// admitted an `OfflineApi` under an exact grant and writer lease; the
/// deterministic evaluator needs neither because it is in-process.
///
/// The failure classification is deliberately identical to the online path's —
/// a contract violation is still a contract violation when it comes from a
/// local server, and 05 §1.1 keeps `embedding_contract_violation` subject to
/// `fail_behavior` regardless of where the vector came from.
fn local_query_embedding(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    execution: EmbeddingExecution,
    query: &str,
    control: &ControlHandle,
) -> Result<Option<QueryEmbeddingOutcome>> {
    // O2 seam: the query is about to be embedded. That `--text` must never
    // reach this point holds for a local adapter exactly as for a remote one.
    record_query_embed_trace(query, control);
    let items = vec![EmbeddingItem::text("query", query)];
    // No `idempotency_token`: that identifies a ledger reservation, and this
    // path makes none.
    match run_embedding_adapter(
        adapter_settings,
        execution,
        items,
        EmbeddingInputType::Query,
        None,
        control,
    ) {
        Ok(outcome) => Ok(outcome
            .vectors
            .into_iter()
            .next()
            .map(|vector| QueryEmbeddingOutcome::Vector(vector.vector))),
        Err(failure) if failure.retry_kind == RetryErrorKind::ContractViolation => {
            Ok(Some(QueryEmbeddingOutcome::ContractViolation))
        }
        Err(_) => Ok(None),
    }
}

/// Append the query to the file named by `KIO_TEST_QUERY_EMBED_TRACE`, if set, at
/// the point the query embedding is sent (O2 test seam only; no-op in production).
#[cfg(debug_assertions)]
fn record_query_embed_trace(query: &str, control: &ControlHandle) {
    let trace_path = control.debug().cli.query_embed_trace.clone();
    if let Some(path) = trace_path {
        let mut line = query.to_owned();
        line.push('\n');
        let _ = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut file| file.write_all(line.as_bytes()));
    }
}

#[cfg(not(debug_assertions))]
fn record_query_embed_trace(_query: &str, _control: &ControlHandle) {}

/// The tool-lock `embedding` entry (07 §6) for the active seam, or `None` when no
/// embedding adapter is configured.
pub(crate) fn embedding_tool_lock_entry(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    control: &ControlHandle,
) -> Result<Option<Value>> {
    let Some(execution) = embedding_execution(adapter_settings, control) else {
        return Ok(None);
    };
    let profile = declared_embedding_profile(execution);
    // `kind`/`mode` used to be literals, which recorded an offline adapter as
    // having run online. They are properties of the adapter, so they come from
    // its profile — the lock is provenance, and provenance that always says
    // "online" is not provenance (07 §6).
    let execution_mode =
        embedding_adapter_profile_with_control(adapter_settings, execution, control)
            .map_err(adapter_to_kio)?
            .execution_mode;
    let (kind, mode) = embedding_execution_labels(execution_mode);
    Ok(Some(json!({
        "tool_id": profile.tool_id,
        "profile_hash": profile.profile_hash,
        "dimensions": profile.dimensions,
        "distance": profile.distance,
        "modality": profile.modality,
        "kind": kind,
        "mode": mode,
    })))
}

pub(crate) fn embedding_execution_labels(mode: ExecutionMode) -> (&'static str, &'static str) {
    match mode {
        ExecutionMode::OnlineApi => ("online_api", "online"),
        ExecutionMode::OfflineApi => ("offline_api", "offline"),
        ExecutionMode::DeterministicLibrary => ("deterministic_library", "deterministic"),
    }
}

// ===========================================================================
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

/// Embedding identity hash (03 §8.1) keyed on the chunk's `text_hash` so identical
/// content shares one `embeddings` row (content-based reuse).
/// Publish one vector into `objects/embeddings/` — the CAS truth the
/// `embeddings` table and `chunk_vec` are an acceleration layer over (04 §4.3,
/// 03 §8.1).
///
/// R25-6: the object type was declared in the spec and produced by nothing, so
/// `kio repair rebuild-db` snapshotted vectors out of the very database it was
/// about to replace. Losing `sqlite.db` meant buying every vector from the API
/// again. This is the write half of closing that.
///
/// The retained CAS write must succeed before any source or replica row is
/// published. A projection must never be the only copy of a provider result.
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

/// Published `gemini-embedding-2` STANDARD (sync) text input rate, per token
/// (07 §5.3 の 2026-07-24 訂正: $0.20 / 1M sync, $0.10 / 1M batch). Used when
/// `tools.toml` declares no `[embedding.*.pricing] tokens_in`; the declared
/// table is the normative source when it is present.
const EMBEDDING_SYNC_USD_PER_TOKEN: f64 = 0.000_000_2; // $0.20 / 1M

/// R24: the Batch lane is billed at half the standard rate. This is a
/// MULTIPLIER rather than a second absolute constant because the price itself
/// comes from `tools.toml`, whose `[pricing]` keys are a closed enum
/// (`pages` / `tokens_in` / `tokens_out`, `tool_lock::PRICING_KIND_ENUM`) with
/// no lane dimension — a declaration cannot state both lanes. The declared
/// price is therefore read as the STANDARD rate and the batch rate is
/// derived, so a user who corrects the published price keeps the 2:1 ratio the
/// provider actually bills.
///
/// The ratio is 2:1 for BOTH priced roles — OCR is $4 vs $2 per 1,000 pages and
/// embedding is $0.20 vs $0.10 per 1M tokens — so one multiplier covers them.
const BATCH_RATE_MULTIPLIER: f64 = 0.5;

/// A role's declared `[pricing]` table with `lane` applied to every entry.
///
/// This is the single point where a declared price becomes a charge, and it
/// exists because there was no such point: both settlement sites read
/// the declared pricing RAW. That was accidentally right for
/// markdownize — its declaration happened to hold the batch rate — which made
/// a `--realtime` OCR run settle at half what the provider bills. An
/// under-record is the direction a budget cap cannot protect against, since
/// the cap only ever sees the too-small number.
///
/// Reading every declaration as the standard rate makes the two roles obey one
/// rule (07 §5.3) instead of opposite conventions that have to be remembered
/// per role.
pub(crate) fn lane_adjusted_pricing(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    role: &str,
    lane: PreferredRequestKind,
) -> std::collections::BTreeMap<String, f64> {
    adapter_settings
        .declared_pricing(role)
        .into_iter()
        .map(|(kind, standard)| (kind, lane_rate(standard, lane)))
        .collect()
}

/// The online send lane resolved for THIS invocation.
///
/// 2026-07-24 ユーザー裁定: OCR と embedding は**必ず同一レーン**で送る — 片方
/// だけ Batch という組み合わせは作らない。したがってレーンは adapter ごとでは
/// なく invocation ごとに 1 つ決まり、この 1 セルがそれを持つ。
///
/// 解決順は network opt-in (07 §3) と同じ形: **CLI → scope config → user config
/// → 既定 (Batch)**。既定を Batch にするのは半額 (OCR $2/1,000 pages・embedding
/// $0.10/1M tokens) だからで、`--realtime` は倍額を承知の明示 opt-in である。
/// これは 2026-07-23 の「OCR 課金は Batch レーンのみ」裁定を明示的に上書きする。
///
/// 0 = 未解決 (= 既定の Batch として読む), 1 = Batch, 2 = Realtime.
static INVOCATION_LANE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

const LANE_CELL_BATCH: u8 = 1;
const LANE_CELL_REALTIME: u8 = 2;

/// The `--realtime` / `--batch` pair as parsed off one command.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LaneOverride {
    realtime: bool,
    batch: bool,
}

impl LaneOverride {
    pub(crate) fn new(realtime: bool, batch: bool) -> Self {
        Self { realtime, batch }
    }
}

/// Resolve and record the invocation lane. Call once per command, before any
/// online send is planned or priced — both the OCR lane selection and the
/// reservation estimates read the result.
///
/// `scope_config` is the scope's `.kio/config.toml` when the command has a
/// repository open; `None` skips straight to the user config.
pub(crate) fn resolve_invocation_lane(
    over: LaneOverride,
    scope_config: Option<&Path>,
    control: &ControlHandle,
) -> Result<()> {
    if over.realtime && over.batch {
        return Err(KioError::invalid_usage(
            "--realtime and --batch are mutually exclusive",
        ));
    }
    let lane = if over.realtime {
        LANE_CELL_REALTIME
    } else if over.batch {
        LANE_CELL_BATCH
    } else {
        let configured = scope_config
            .and_then(kio_core::scope::read_adapter_lane)
            .or_else(|| kio_core::scope::read_adapter_lane(&user_config_toml_path(control)));
        match configured.as_deref() {
            Some("realtime") => LANE_CELL_REALTIME,
            _ => LANE_CELL_BATCH,
        }
    };
    INVOCATION_LANE.store(lane, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}

/// The lane BOTH online adapters send on this invocation. There is no
/// per-adapter answer by ruling — see [`INVOCATION_LANE`].
pub(crate) fn effective_invocation_lane() -> PreferredRequestKind {
    if INVOCATION_LANE.load(std::sync::atomic::Ordering::SeqCst) == LANE_CELL_REALTIME {
        PreferredRequestKind::Sync
    } else {
        PreferredRequestKind::Batch
    }
}

#[must_use]
pub(crate) fn realtime_lane_requested() -> bool {
    matches!(effective_invocation_lane(), PreferredRequestKind::Sync)
}

/// The lane the SYNCHRONOUS enrichment path sends on — always `Sync`.
///
/// R24 (3 系統一致) corrected this comment, which still claimed the Batch driver
/// "is still to land". It has landed: [`effective_invocation_lane`] chooses the
/// lane, and when it selects Batch the work goes to
/// [`submit_embedding_batch_jobs`] and never reaches the code that calls this.
///
/// So the callers of this function are, by construction, the ones already on the
/// synchronous lane — either because the invocation asked for `--realtime` or
/// because the batch lane was unavailable and the send fell back. Pricing them
/// at the sync rate is therefore correct, not a placeholder — and since I12 it
/// prices the SETTLEMENT too, not just the reservation, because
/// `embedding_billed_from_usage` resolves the measured tokens through this same
/// lane.
pub(crate) fn active_embedding_send_lane() -> PreferredRequestKind {
    PreferredRequestKind::Sync
}

/// The query lane is ALWAYS synchronous: a search cannot wait out a batch
/// job's turnaround (up to 24 h), so a query embedding is priced at the sync
/// rate regardless of what the enrichment path does.
pub(crate) fn query_embedding_send_lane() -> PreferredRequestKind {
    PreferredRequestKind::Sync
}

/// The per-token rate for `lane`.
///
/// R24 (5 系統一致): this used to `return declared` BEFORE looking at `lane`,
/// so declaring `tokens_in` in `tools.toml` — which the spec calls the
/// normative source, and which the dogfood config does declare — silently
/// collapsed the two lanes to one price. At the time the reservation estimate
/// was also the settled charge, so that recorded every Batch job at the sync
/// rate: a 2x over-record that consumes the budget cap twice as fast as the
/// provider actually bills. Settlement now prices measured tokens (I4/I12) —
/// through this same function — so a wrong lane here corrupts BOTH sides at
/// once rather than only the reservation.
pub(crate) fn embedding_usd_per_token(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    lane: PreferredRequestKind,
) -> f64 {
    let standard = adapter_settings
        .declared_pricing("embedding")
        .get("tokens_in")
        .copied()
        .unwrap_or(EMBEDDING_SYNC_USD_PER_TOKEN);
    lane_rate(standard, lane)
}

/// The lane split applied to a STANDARD per-token rate, whatever its source.
/// Split out from [`embedding_usd_per_token`] so the declared-pricing path is
/// testable: `register_declared_pricing` is a process-global `OnceLock`, so a
/// unit test cannot install a `tools.toml` price and observe the result.
pub(crate) fn lane_rate(standard: f64, lane: PreferredRequestKind) -> f64 {
    match lane {
        PreferredRequestKind::Batch => standard * BATCH_RATE_MULTIPLIER,
        PreferredRequestKind::Sync => standard,
    }
}

/// Rough token count for `text`.
///
/// The previous `chars / 4` assumed Latin script throughout and undercounts
/// CJK badly — a Japanese chunk is closer to one token per character, so a
/// mixed corpus was estimated (and, because `EmbeddingResponse.usage` is
/// `None`, *charged*) well under what it costs. Count CJK scalars as one token
/// each and the rest at the ~4-chars/token Latin ratio.
pub(crate) fn estimate_embedding_tokens(text: &str) -> f64 {
    let mut cjk = 0u64;
    let mut other = 0u64;
    for ch in text.chars() {
        if is_cjk_scalar(ch) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk as f64 + other as f64 / 4.0
}

/// Scalars that tokenize at roughly one token each: Han, kana, Hangul, and the
/// CJK punctuation/fullwidth blocks that sit between them.
fn is_cjk_scalar(ch: char) -> bool {
    matches!(ch as u32,
        0x3000..=0x30FF      // CJK symbols and punctuation, hiragana, katakana
        | 0x3130..=0x318F    // Hangul compatibility jamo
        | 0x3400..=0x4DBF    // CJK unified ideographs extension A
        | 0x4E00..=0x9FFF    // CJK unified ideographs
        | 0xAC00..=0xD7AF    // Hangul syllables
        | 0xF900..=0xFAFF    // CJK compatibility ideographs
        | 0xFF00..=0xFFEF    // Halfwidth and fullwidth forms
        | 0x20000..=0x2FA1F  // CJK extensions B-F and compatibility supplement
    )
}

/// Cost estimate for embedding `text` on `lane`.
///
/// This is the RESERVATION, and — per I12's fix — only the settlement fallback
/// for a send whose response carried no usable usage report. It used to be the
/// settled charge outright, on the since-falsified reasoning that the endpoint
/// returns no token count. It still has to be right: I10 measured this
/// estimator against 33 real jobs and found -32%..+52% error with 24% of jobs
/// UNDER-estimated, and the budget cap gates on the reservation, not on the
/// settlement.
pub(crate) fn estimate_embedding_cost(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    text: &str,
    lane: PreferredRequestKind,
) -> f64 {
    estimate_embedding_tokens(text) * embedding_usd_per_token(adapter_settings, lane)
}

/// Price one embedding send from the usage the adapter reported, degrading to
/// `fallback_estimated_usd` (the reservation) whenever that report is absent or
/// fails to cover every kind the adapter declares it bills for.
///
/// I12: one function for all three send sites — Batch collect, sync query, sync
/// chunk — because pricing one adapter in three places is exactly how the bug
/// happened. The Batch site learned to read `usage` while the two sync sites
/// went on asserting the endpoint had none to give, and nothing in the type
/// system objected. A new send site now has to pick a lane and a fallback, not
/// invent a pricing rule.
pub(crate) fn embedding_billed_from_usage(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    reported: Option<&kio_adapter::types::AdapterUsage>,
    lane: PreferredRequestKind,
    fallback_estimated_usd: f64,
    control: &ControlHandle,
    session: &kio_core::scope::LockSession,
) -> BilledAmount {
    let (billed, uncovered) = resolve_billing_from_reported_usage(
        reported,
        // The embedding adapter declares exactly one billable kind
        // (`gemini_embedding`'s profile: `TokensIn`). Its module is private, so
        // the set is restated rather than fabricated from an `AdapterProfile`
        // these call sites do not have.
        &std::collections::BTreeSet::from(["tokens_in".to_owned()]),
        // The SAME rate resolution the reservation used, fallback included.
        // Pricing the two sides from different sources is how a reservation and
        // its settlement drift apart.
        &std::collections::BTreeMap::from([(
            "tokens_in".to_owned(),
            embedding_usd_per_token(adapter_settings, lane),
        )]),
        fallback_estimated_usd,
    );
    warn_uncovered_pricing_kinds_once(EMBEDDING_ADAPTER_KIND, &uncovered, control, session);
    billed
}
