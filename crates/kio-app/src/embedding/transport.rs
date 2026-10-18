use crate::approvals;
use crate::egress::persistent_network_allowed_for;
use crate::embedding::admission::EMBEDDING_ADAPTER_KIND;
use crate::embedding::admission::declared_embedding_profile;
use crate::embedding::admission::embedding_billed_from_usage;
use crate::embedding::admission::embedding_execution;
use crate::embedding::admission::embedding_is_deterministic;
use crate::embedding::admission::embedding_is_online;
use crate::embedding::admission::embedding_transport_allowed;
use crate::embedding::admission::estimate_embedding_cost;
use crate::embedding::admission::query_embedding_send_lane;
use crate::invocation::Invocation;
use crate::ledger_access::TASK_SYNC_EFFECTIVE_TIMEOUT_SECONDS;
use crate::ledger_access::budget_cap_config;
use crate::ledger_access::open_ledger_db;
use crate::ledger_access::settle_task_charge_success;
use crate::ledger_access::settle_task_charge_unknown;
use crate::markdownize::execution::TaskExecutionFailure;
use crate::markdownize::execution::task_failure_from_adapter;
use crate::object_persistence::pipeline_to_kio;
use crate::scope_access::maybe_wait_at_test_search_final_consent_barrier;
use crate::search::scope::ExecScope;
use crate::search::scope::revalidate_current_policy_map;
use crate::settings::user_config_toml_path;
use kio_adapter::catalog::EmbeddingExecution;
use kio_adapter::catalog::run_embedding_with_control;
use kio_adapter::tool_lock::AdapterRuntimeSettings;
use kio_adapter::types::EmbeddingInputType;
use kio_adapter::types::EmbeddingItem;
use kio_core::Result;
use kio_core::control::ControlHandle;
use kio_core::scope::Repository;
use kio_pipeline::budget::read_budget_policy;
use kio_pipeline::ledger::TaskKey as LedgerTaskKey;
use kio_pipeline::ledger::ops::ClaimOutcome;
use kio_pipeline::ledger::ops::device_input_hash;
use kio_pipeline::policy::CurrentPolicyEvaluator;
use kio_pipeline::task::RetryErrorKind;
use std::collections::BTreeMap;
#[cfg(debug_assertions)]
use std::fs::OpenOptions;
#[cfg(debug_assertions)]
use std::io::Write;
use std::sync::Arc;

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
