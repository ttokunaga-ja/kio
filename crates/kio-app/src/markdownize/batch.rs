use crate::batch::command::provider_task_ledger_key;
use crate::embedding::transport::batch_provider_scope_matches;
use crate::embedding::transport::lane_adjusted_pricing;
use crate::index::pipeline::online_markdownize_profile_for;
use crate::index::pipeline::online_output_ref;
use crate::ledger_access::RETIRED_NON_LIVE;
use crate::ledger_access::declared_billable_kind_strings;
use crate::ledger_access::warn_uncovered_pricing_kinds_once;
use crate::markdownize::execution::OnlineMarkdownizePrecondition;
use crate::markdownize::execution::PreparedOnlineMarkdownizeInput;
use crate::markdownize::execution::TaskExecutionFailure;
use crate::markdownize::execution::adapter_unit_kind;
use crate::markdownize::execution::classify_online_markdownize_precondition;
use crate::markdownize::execution::materialize_online_markdownize_response;
use crate::markdownize::execution::task_failure_from_adapter;
use crate::object_persistence::adapter_to_kio;
use crate::object_persistence::pipeline_to_kio;
use crate::online_task::targets_standard_online_markdownize;
use crate::scope_access::raw_ingest_is_purge_blocked;
use crate::settings::batch_recovery_context;
use crate::task_execution::ExecOutcome;
use crate::task_execution::known_markdownize_completion;
use crate::task_execution::rate_limit_retry_at;
use crate::task_execution::retry_reason;
use crate::task_execution::scheduled_retry_at;
use kio_adapter::BatchOcrMaterializationError;
use kio_adapter::batch_client::BatchJobStatus;
use kio_adapter::batch_client::BatchOutputLine;
use kio_adapter::batch_client::MistralBatchClient;
use kio_adapter::batch_client::batch_input_line;
use kio_adapter::batch_client::batch_upload_filename;
use kio_adapter::batch_client::configured_mistral_batch_client_with_control;
use kio_adapter::batch_client::ocr_batch_body;
use kio_adapter::catalog::convert_office_to_pdf_with_control;
use kio_adapter::catalog::resolve_standard_online_markdownize_profile_with_bbox_with_control;
use kio_adapter::materialize_mistral_batch_ocr_body;
use kio_adapter::office_convert::is_office_media;
use kio_adapter::tool_lock::AdapterRuntimeSettings;
use kio_adapter::traits::PreferredRequestKind;
use kio_adapter::types::AdapterProfile;
use kio_adapter::types::PreparedUnitHint;
use kio_core::KioError;
use kio_core::Result;
use kio_core::control::ControlHandle;
use kio_core::scope::Repository;
#[cfg(debug_assertions)]
use kio_core::test_control::DebugTestControl;
use kio_pipeline::ledger::BatchRequestRow;
use kio_pipeline::ledger::BatchState;
use kio_pipeline::ledger::LedgerDb;
use kio_pipeline::ledger::Outcome;
use kio_pipeline::ledger::RequestKind;
use kio_pipeline::ledger::TaskKey as LedgerTaskKey;
use kio_pipeline::ledger::ops::BilledAmount;
use kio_pipeline::ledger::ops::TerminalWrite;
use kio_pipeline::ledger::ops::batch_poll_candidates;
use kio_pipeline::ledger::ops::get_batch_request;
use kio_pipeline::ledger::ops::phase2a_record_provider_scope;
use kio_pipeline::ledger::ops::phase2a_record_upload_id;
use kio_pipeline::ledger::ops::phase2b_record_job_create_started;
use kio_pipeline::ledger::ops::phase2b_record_job_created;
use kio_pipeline::ledger::ops::recovery_finish_cleanup;
use kio_pipeline::ledger::ops::resolve_billing_from_reported_usage;
use kio_pipeline::ledger::ops::terminal_transaction;
use kio_pipeline::task::RetryErrorKind;
use kio_pipeline::task::TaskDescriptor;
use kio_pipeline::task::TaskStatus;
use kio_pipeline::task::TaskStore;
use kio_pipeline::task::TaskType;
use kio_pipeline::task::retry_policy;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Mistral OCR Batch lane (04 §5.8 / 07 §5.5): submit + poll/collect
// ---------------------------------------------------------------------------

/// Poll cadence for an in-flight batch job (task `next_retry_at` extension per
/// submit/poll pass).
const BATCH_POLL_INTERVAL_MS: u64 = 60_000;

/// `fallback_reason` stamped on a Pending task whose send this pass took the
/// Batch lane (job created, awaiting collect). Deliberately NOT one of the
/// retry-failure reasons: `retry_kind_from_reason` maps it to its
/// ContractViolation default, but no code path reads a PENDING task's reason
/// as a retry kind (`task_retry_allowed` is Failed-only), and
/// `is_reservation_bearing_send_failure`'s sweep set does not include it — an
/// in-flight batch task must never be retired as a stale send failure while
/// its provider job is live.
pub(crate) const BATCH_SUBMITTED_REASON: &str = "batch_submitted";

/// The configured Batch client, if the Batch lane is available at all.
///
/// Lane availability deliberately equals "`configured_mistral_batch_client()`
/// returned a client". 07 §5.5 frames the lane choice as the adapter's
/// `preferred_request_kind()` declaration AND a configured client, but the
/// concrete `MistralOcrMarkdownizeAdapter` type is module-private to
/// `kio-adapter` (no catalog constructor returns it as a `dyn
/// MarkdownizeAdapter`), so this crate cannot instantiate one to ask. The
/// frozen resolver embodies the declaration instead: it returns `Some`
/// exactly when the hermetic mock seam (`KIO_TEST_MISTRAL_BATCH`) is present
/// — the test path — or (once the adapter-side env client lands) when the
/// Batch-declaring Mistral profile is configured for production.
pub(crate) fn batch_markdownize_lane_client(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    ledger: &LedgerDb,
) -> Result<Option<Box<dyn MistralBatchClient>>> {
    configured_mistral_batch_client_with_control(
        adapter_settings,
        || batch_recovery_context(ledger),
        ledger.control(),
    )
    .map_err(adapter_to_kio)
}

/// Check only the locally declared sync credential. This never constructs an
/// HTTP client or contacts the provider. Test adapters keep their established
/// failure semantics because their mock execution can model a post-admission
/// auth failure rather than an absent production credential.
pub(crate) fn sync_markdownize_credential_available(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    _control: &ControlHandle,
) -> Result<bool> {
    #[cfg(debug_assertions)]
    if markdownize_test_credential_available(_control.debug()) {
        return Ok(true);
    }
    Ok(adapter_settings
        .resolve_role_api_key("markdown")
        .map_err(adapter_to_kio)?
        .is_some_and(|credential| !credential.trim().is_empty()))
}

#[cfg(debug_assertions)]
pub(crate) fn markdownize_test_credential_available(control: &DebugTestControl) -> bool {
    control.cli.markdownize_adapter.known().is_some()
        || matches!(
            control.adapters.mistral_ocr,
            kio_core::test_control::Selector::Known(_)
        )
}

/// Which lane one Pending online-markdownize task's send takes this pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MarkdownizeSendLane {
    Sync,
    Batch,
    /// An open `request_kind='batch'` ledger row exists but no batch client is
    /// configured this pass (e.g. the mock seam vanished): neither lane can
    /// act without either racing §5.8 recovery or hiding provider residue, so
    /// the task is left untouched (counted in-flight) for reconcile/abandon.
    DeferToRecovery,
}

pub(crate) fn markdownize_send_lane(
    batch_client_present: bool,
    open_row: Option<&BatchRequestRow>,
    realtime: bool,
) -> MarkdownizeSendLane {
    let open_row_kind = open_row
        .filter(|row| row.state.is_inflight())
        .map(|row| row.request_kind);
    match (batch_client_present, open_row_kind) {
        // A prior synchronous request may already have reached a non-idempotent
        // provider. It is recover-only; never reuse its reservation to resend.
        (_, Some(RequestKind::Sync)) => MarkdownizeSendLane::DeferToRecovery,
        // An open batch row owns its provider job regardless of `--realtime`:
        // switching lanes mid-flight would strand that job outside §5.8
        // recovery. Realtime only chooses the lane for a NEW send.
        (false, Some(RequestKind::Batch)) => MarkdownizeSendLane::DeferToRecovery,
        (true, Some(RequestKind::Batch)) => MarkdownizeSendLane::Batch,
        (false, None) => MarkdownizeSendLane::Sync,
        // 2026-07-23 ruling: OCR spending is batch-only, WITHOUT a bbox
        // carve-out — and bbox annotation defaults to ON
        // (`effective_bbox_annotation_policy`), so a bbox exception here
        // would silently keep production on the sync lane. `ocr_batch_body`
        // carries `bbox_annotation_format` exactly like the sync request.
        //
        // 2026-07-24 ruling AMENDS that: `--realtime` is an explicit, opt-in
        // request for immediate turnaround that accepts the sync lane's double
        // rate ($4 vs $2 per 1,000 pages), and it moves OCR and embedding
        // together — never one lane each.
        (true, None) if realtime => MarkdownizeSendLane::Sync,
        (true, None) => MarkdownizeSendLane::Batch,
    }
}

/// The model string a batch job is created with (`create_job`'s second
/// argument — the model rides the JOB, not the input line, 07 §5.5).
fn batch_job_model(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    control: &ControlHandle,
) -> String {
    let declared = adapter_settings
        .declared_adapter("markdown")
        .and_then(|declared| declared.model)
        .unwrap_or_else(|| "mistral-ocr-latest".to_owned());
    if !declared.ends_with("-latest") {
        return declared;
    }
    // A mutable alias must be resolved to a versioned pin before the call
    // (07 §6). The alias resolver lives on the module-private sync OCR
    // client, unreachable from this crate; under the hermetic mock seam use
    // the same pin the KIO_TEST_MISTRAL_OCR mock resolves to so tests stay
    // network-free and profile-consistent. The real lane's alias resolution
    // is expected from the adapter-side EnvMistralBatchClient (in progress);
    // until it lands `configured_mistral_batch_client()` returns None in
    // production, so the alias fallback below can only reach a mock.
    if mistral_batch_mock_enabled(control) {
        "mistral-ocr-2505".to_owned()
    } else {
        declared
    }
}

#[cfg(debug_assertions)]
fn mistral_batch_mock_enabled(control: &ControlHandle) -> bool {
    matches!(
        control.debug().adapters.mistral_batch,
        kio_core::test_control::Selector::Known(_)
    )
}

#[cfg(not(debug_assertions))]
fn mistral_batch_mock_enabled(_control: &ControlHandle) -> bool {
    false
}

/// `next_retry_at` for the next batch poll (`now + 60s`, KIO_FIXED_NOW-aware
/// via `now_utc_seconds`).
pub(crate) fn batch_poll_retry_at(control: &ControlHandle) -> String {
    rate_limit_retry_at(&control.now_utc_seconds(), Some(BATCH_POLL_INTERVAL_MS))
}

/// Submit one online-markdownize task on the Batch lane (04 §5.8 手順 2-3).
///
/// The ledger write order is the §5.8 norm exactly: `provider_scope_id`
/// immediately BEFORE the upload → `upload_id` immediately after → a
/// standalone durable `job_create_started_at` → `create_job` →
/// `batch_job_id` + state=1. A crash (or provider error) between any two
/// steps leaves a row the §5.8 recovery walk can reconcile — that is why a
/// mid-submit adapter error returns the failure WITHOUT rolling any of those
/// records back.
///
/// Outer `Err` = local store/ledger fault (propagates); inner `Err` = the
/// adapter-boundary failure to feed the shared send-failure mapping.
#[allow(clippy::too_many_arguments)]
pub(crate) fn submit_batch_markdownize_job(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    client: &dyn MistralBatchClient,
    ledger: &LedgerDb,
    key: &LedgerTaskKey,
    intent_token: &str,
    task: &TaskDescriptor,
    prepared_input: &PreparedOnlineMarkdownizeInput,
) -> Result<std::result::Result<(), TaskExecutionFailure>> {
    let conn = &ledger;
    let Some(row) = get_batch_request(conn, key).map_err(pipeline_to_kio)? else {
        return Err(KioError::schema(
            "batch submit reached without a phase-1 batch_requests row",
        ));
    };
    if row.intent_token.as_deref() != Some(intent_token) {
        // Claim superseded between reserve and submit — nothing was sent.
        return Ok(Ok(()));
    }
    if row.batch_job_id.is_some() {
        // Already created (a reused in-flight attempt) — the poll lane owns it.
        return Ok(Ok(()));
    }
    if row.provider_scope_id.is_some() {
        // Phase 2a already started under THIS token by an earlier crashed
        // pass. Re-uploading would duplicate provider residue under one
        // token; §5.8 recovery (upload matching / reconcile) owns the row
        // until it is cleaned or self-described. Conservative: skip.
        return Ok(Ok(()));
    }
    // Same input material as the sync lane (07 §5.1): Office media is sent as
    // its converted PDF; storage identity stays the ORIGINAL file's raw_hash.
    let (effective_media_type, send_bytes): (String, Vec<u8>) =
        if is_office_media(&prepared_input.media_type) {
            match convert_office_to_pdf_with_control(
                &prepared_input.bytes,
                &prepared_input.media_type,
                ledger.control(),
            ) {
                Ok(converted) => ("application/pdf".to_owned(), converted),
                Err(error) => return Ok(Err(task_failure_from_adapter(error))),
            }
        } else {
            (
                prepared_input.media_type.clone(),
                prepared_input.bytes.clone(),
            )
        };
    // A unit-scoped retry sends only the still-failed subset's pages, exactly
    // like the sync lane's `restrict_to_hint_pages` (R15-5); a full send (or
    // OCR-from-scratch discovery, empty prepared units) sends no `pages`.
    let retry_units: Option<BTreeSet<String>> = task
        .unit_keys
        .as_ref()
        .map(|keys| keys.iter().cloned().collect());
    let pages: Option<Vec<u32>> = match &retry_units {
        Some(keys) => {
            let mut orders = Vec::new();
            for unit in prepared_input
                .prepared_units
                .iter()
                .filter(|unit| keys.contains(&unit.unit_key))
            {
                let Ok(order) = u32::try_from(unit.order) else {
                    return Ok(Err(TaskExecutionFailure {
                        retry_kind: RetryErrorKind::InvalidInput,
                        retry_after_ms: None,
                    }));
                };
                orders.push(order);
            }
            if orders.is_empty() {
                // The precondition check already validated the subset against
                // the prepared units; an empty projection means the task is
                // stale — never send an unbounded full-document job for it.
                return Ok(Err(TaskExecutionFailure {
                    retry_kind: RetryErrorKind::InvalidInput,
                    retry_after_ms: None,
                }));
            }
            Some(orders)
        }
        None => None,
    };
    let body = ocr_batch_body(
        &effective_media_type,
        &send_bytes,
        pages.as_deref(),
        task.bbox_annotation_enabled.unwrap_or(false),
    );
    // custom_id = the task's FULL input_hash (帰属 key for output-line
    // matching — 04 §5.8; the ledger row + job metadata carry the rest).
    let mut jsonl = batch_input_line(&task.input_hash, &body).into_bytes();
    jsonl.push(b'\n');
    let filename = batch_upload_filename(intent_token);
    // The 5-key metadata contract shared with the adapter-side inventory
    // (10 §7.5.2 帰属規範): intent_token + the 4-tuple task key, nothing else.
    let metadata = json!({
        "intent_token": intent_token,
        "scope_id": key.scope_id,
        "adapter_kind": key.adapter_kind,
        "input_hash": key.input_hash,
        "tool_profile_hash": key.tool_profile_hash,
    });
    let provider_scope_id = match client.provider_scope_id() {
        Ok(value) => value,
        Err(error) => return Ok(Err(task_failure_from_adapter(error))),
    };
    if !phase2a_record_provider_scope(conn, key, intent_token, &provider_scope_id)
        .map_err(pipeline_to_kio)?
    {
        return Ok(Ok(())); // CAS miss: claim superseded — nothing sent.
    }
    let upload_id = match client.upload_batch_input(&jsonl, &filename) {
        Ok(value) => value,
        Err(error) => return Ok(Err(task_failure_from_adapter(error))),
    };
    if !phase2a_record_upload_id(conn, key, intent_token, &upload_id).map_err(pipeline_to_kio)? {
        return Ok(Ok(()));
    }
    if !phase2b_record_job_create_started(conn, key, intent_token).map_err(pipeline_to_kio)? {
        return Ok(Ok(()));
    }
    let job = match client.create_job(
        &upload_id,
        &batch_job_model(adapter_settings, ledger.control()),
        &metadata,
    ) {
        Ok(value) => value,
        Err(error) => return Ok(Err(task_failure_from_adapter(error))),
    };
    phase2b_record_job_created(conn, key, intent_token, &job.job_id).map_err(pipeline_to_kio)?;
    Ok(Ok(()))
}

/// §5.8 残骸掃除, cleanup-first: delete the upload (404 = success per the
/// trait contract), and only then NULL the `intent_token`
/// (`recovery_finish_cleanup`). A failed deletion leaves the token in place —
/// the filename-embedded token is the upload's only discovery key, and the
/// next pass (or reconcile/abandon) retries.
///
/// The stored provider scope is the exact authority for deletion. A missing
/// scope can only finish local cleanup when no upload was ever recorded:
/// when phase 2a started but `upload_id` was never recorded (the
/// upload-id-unrecorded crash window), an upload MAY exist that only the
/// `list_uploads` filename-token matching can find — clearing the token here
/// would make it permanently undiscoverable (04 §5.8 confirmed-absent note),
/// so this case keeps the token and defers to reconcile/abandon.
pub(crate) fn cleanup_batch_residue(
    ledger: &LedgerDb,
    client: &dyn MistralBatchClient,
    key: &LedgerTaskKey,
    intent_token: &str,
    upload_id: Option<&str>,
    stored_provider_scope: Option<&str>,
) -> Result<()> {
    match upload_id {
        Some(upload_id) => {
            if !batch_provider_scope_matches(stored_provider_scope, || client.provider_scope_id())
                || client.delete_upload(upload_id).is_err()
            {
                return Ok(());
            }
        }
        None if stored_provider_scope.is_some() => return Ok(()),
        // No phase 2a authority and no upload: cancellation before any send
        // needs no provider contact and must not strand a safe terminal row.
        None => {}
    }
    recovery_finish_cleanup(ledger, key, intent_token).map_err(pipeline_to_kio)?;
    Ok(())
}

/// 04 §5.8 相 3 terminal for a batch row, WITHOUT clearing `intent_token`
/// (cleanup-first: the token clears only via [`cleanup_batch_residue`] once
/// the upload deletion is confirmed). `ledger_batch_job_id` follows the §5.4
/// DDL 3-way rule: the REAL job id whenever it is known.
#[allow(clippy::too_many_arguments)]
pub(crate) fn settle_batch_charge_terminal(
    ledger: &LedgerDb,
    key: &LedgerTaskKey,
    intent_token: &str,
    ledger_batch_job_id: &str,
    outcome: Outcome,
    next_state: BatchState,
    error: Option<&str>,
    billed: BilledAmount,
    increment_contract_violation: bool,
    // §5.8: NULL 化は残骸掃除の完了時のみ. For the OCR lane that means never in
    // the collect Tx (an upload still has to be deleted). The embedding lane
    // submits INLINE, so it leaves no provider residue at all and terminal IS
    // cleanup-complete — the same reasoning §5.4 applies to sync rows. Leaving
    // the token set there would permanently block re-submission of that task
    // key ("旧 token の消し込み完了後にのみ再投入可").
    clear_intent_token: bool,
) -> Result<()> {
    terminal_transaction(
        ledger,
        &TerminalWrite {
            key,
            outcome,
            billed,
            ledger_batch_job_id,
            next_state,
            error,
            increment_contract_violation,
            attempts_delta: 1,
            clear_intent_token,
            intent_token_guard: Some(intent_token),
            reseat_submission_seq: false,
        },
    )
    .map_err(pipeline_to_kio)
    .map(|_| ())
}

/// What one polled batch row did to this pass's `ExecOutcome`.
pub(crate) enum BatchPollDisposition {
    Collected(ExecOutcome),
    InFlight,
    FailedPermanent,
    FailedRetryable,
    Retired,
}

/// 04 §5.8 相 3 (poll + collect) for this scope's in-flight batch markdownize
/// jobs — rows with a durably-known `batch_job_id` (`state IN (0,1)`; state=0
/// rows are reconcile-self-described crash windows, see
/// `ops::batch_poll_candidates`). Runs from `kio batch *` and the other
/// write-command entry points alongside `recover_stale_sync_rows`; polling is
/// receive/cleanup of an EXISTING request, never a new send, so it needs no
/// network opt-in (04 §5.8 回復 note). Rows whose job id is still unknown
/// stay owned by the §5.8 recovery walk (`kio ledger reconcile`) — no overlap
/// by construction.
pub(crate) fn poll_batch_markdownize_jobs(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    store: &TaskStore,
    ledger: &LedgerDb,
) -> Result<ExecOutcome> {
    let mut counts = ExecOutcome::default();
    let Some(client) = batch_markdownize_lane_client(adapter_settings, ledger)? else {
        return Ok(counts);
    };
    let scope_id = repo.scope_identity()?.scope_id;
    let rows = batch_poll_candidates(ledger, &scope_id, "markdownize").map_err(pipeline_to_kio)?;
    if !rows.is_empty() {
        let online_profile = online_markdownize_profile_for(repo)?;
        let placeholder_ref = online_output_ref(&online_profile.adapter_id);
        let tasks = store.all().map_err(pipeline_to_kio)?;
        for row in rows {
            let disposition = poll_one_batch_markdownize_row(
                adapter_settings,
                repo,
                store,
                ledger,
                client.as_ref(),
                &row,
                &online_profile,
                &placeholder_ref,
                &tasks,
            )?;
            match disposition {
                BatchPollDisposition::Collected(completion) => counts.add(completion),
                BatchPollDisposition::InFlight => counts.inflight += 1,
                BatchPollDisposition::FailedPermanent | BatchPollDisposition::Retired => {
                    counts.failed += 1;
                }
                BatchPollDisposition::FailedRetryable => {
                    counts.failed += 1;
                    counts.failed_retryable += 1;
                }
            }
        }
    }
    // §5.8 cleanup replay: this scope's terminal rows whose residue cleanup
    // did not complete (delete_upload failed, or a crash landed between the
    // terminal Tx and the cleanup) retry deletion here — "削除失敗・クラッシュ
    // は次回回復が再試行し、全削除の完了をもって intent_token を NULL 化する".
    for row in kio_pipeline::ledger::ops::recovery_candidates(ledger).map_err(pipeline_to_kio)? {
        if row.key.scope_id != scope_id
            || row.key.adapter_kind != "markdownize"
            || !row.state.is_terminal()
        {
            continue;
        }
        let Some(token) = row.intent_token.clone() else {
            continue;
        };
        cleanup_batch_residue(
            ledger,
            client.as_ref(),
            &row.key,
            &token,
            row.upload_id.as_deref(),
            row.provider_scope_id.as_deref(),
        )?;
    }
    Ok(counts)
}

/// No provider state or output is actionable until its durable job identity,
/// intent, and complete task key have all round-tripped unchanged.
pub(crate) fn poll_attributed_markdownize_batch_job(
    client: &dyn MistralBatchClient,
    row: &BatchRequestRow,
) -> Option<kio_adapter::batch_client::BatchJobRecord> {
    if !batch_provider_scope_matches(row.provider_scope_id.as_deref(), || {
        client.provider_scope_id()
    }) {
        return None;
    }
    let job_id = row.batch_job_id.as_deref()?;
    let intent_token = row.intent_token.as_deref()?;
    let job = client.get_job(job_id).ok()?;
    let task_key: kio_adapter::batch_inventory::ProviderTaskKey =
        serde_json::from_value(job.metadata.clone()).ok()?;
    if job.job_id != job_id
        || job.metadata.get("intent_token").and_then(Value::as_str) != Some(intent_token)
        || provider_task_ledger_key(&task_key) != row.key
    {
        return None;
    }
    Some(job)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn poll_one_batch_markdownize_row(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    store: &TaskStore,
    ledger: &LedgerDb,
    client: &dyn MistralBatchClient,
    row: &BatchRequestRow,
    online_profile: &AdapterProfile,
    placeholder_ref: &str,
    tasks: &[TaskDescriptor],
) -> Result<BatchPollDisposition> {
    let Some(job_id) = row.batch_job_id.clone() else {
        return Ok(BatchPollDisposition::InFlight);
    };
    let Some(intent_token) = row.intent_token.clone() else {
        // A state 0/1 row always carries a token by construction — defensive.
        return Ok(BatchPollDisposition::InFlight);
    };
    if row.key.tool_profile_hash != online_profile.tool_profile_hash {
        // A row reserved under a different (older) profile configuration:
        // this pass cannot attribute it to a live task; reconcile/abandon own
        // it (04 §5.8 unknown — leave untouched).
        return Ok(BatchPollDisposition::InFlight);
    }
    if !batch_provider_scope_matches(row.provider_scope_id.as_deref(), || {
        client.provider_scope_id()
    }) {
        return Ok(BatchPollDisposition::InFlight);
    }
    let task = tasks.iter().find(|task| {
        task.task_type == TaskType::Markdownize
            && task.input_hash == row.key.input_hash
            && matches!(
                task.status,
                TaskStatus::Pending | TaskStatus::Failed | TaskStatus::Running
            )
            && targets_standard_online_markdownize(store.kio_dir(), task, placeholder_ref)
    });
    let Some(task) = task else {
        // tasks.jsonl is loss-tolerated (04 §5.8): confirming + persisting an
        // output without a descriptor is the ledger-only recovery path
        // (gen/unit re-derivation), deliberately not wired in lane v1 —
        // `kio ledger reconcile` / `kio batch abandon` remain the owners.
        return Ok(BatchPollDisposition::InFlight);
    };
    let Some(job) = poll_attributed_markdownize_batch_job(client, row) else {
        return Ok(BatchPollDisposition::InFlight);
    };
    if job.status.is_in_flight() {
        extend_batch_poll_schedule(store, &task.task_id, repo.control())?;
        return Ok(BatchPollDisposition::InFlight);
    }
    if job.status != BatchJobStatus::Success {
        return settle_failed_batch_job(
            store,
            ledger,
            client,
            row,
            &intent_token,
            &job_id,
            task,
            &job.status,
        );
    }
    let Some(output_file_id) = job.output_file_id.clone() else {
        // SUCCESS without an output handle is a provider anomaly — treat as
        // still-unknown and re-poll rather than guessing.
        return Ok(BatchPollDisposition::InFlight);
    };
    if !batch_provider_scope_matches(row.provider_scope_id.as_deref(), || {
        client.provider_scope_id()
    }) {
        return Ok(BatchPollDisposition::InFlight);
    }
    let lines = match client.fetch_output(&output_file_id) {
        Ok(lines) => lines,
        Err(_) => return Ok(BatchPollDisposition::InFlight),
    };
    let line = lines
        .into_iter()
        .find(|line| line.custom_id == row.key.input_hash);
    collect_batch_markdownize_output(
        adapter_settings,
        repo,
        store,
        ledger,
        client,
        row,
        &intent_token,
        &job_id,
        task,
        online_profile,
        line,
    )
}

/// Terminal (non-success) provider job status → reject terminal (04 §5.8 相 3
/// reject 同形). Billed 0 confirmed (`usd=0`, `estimated=0`): Mistral does not
/// bill failed/cancelled batch entries (2026-07-03 verification premise;
/// orchestrator ruling for this lane — conservative in the "no phantom spend
/// held against the cap" direction; a resubmission is a human-driven fresh
/// phase 1). Outcome mapping inside the closed §5.4 enum: TIMEOUT_EXCEEDED is
/// the provider's job-expiry shape → `expired`; FAILED/CANCELLED/other →
/// `submit_rejected` (the provider-refused-the-work bucket — the enum has no
/// dedicated provider-job-failed value). `contract_violation_count` is NOT
/// incremented (a provider-side failure is not a §3.2 acceptance reject).
#[allow(clippy::too_many_arguments)]
fn settle_failed_batch_job(
    store: &TaskStore,
    ledger: &LedgerDb,
    client: &dyn MistralBatchClient,
    row: &BatchRequestRow,
    intent_token: &str,
    job_id: &str,
    task: &TaskDescriptor,
    status: &BatchJobStatus,
) -> Result<BatchPollDisposition> {
    let (outcome, error, billed) = match status {
        // Provider TIMEOUT_EXCEEDED: no output to read a real usage from —
        // the same unknown-cost epistemic state as a KIO-deadline expiry, so
        // it books the RESERVED estimate (04 §5.8 "expired = estimated 記帳",
        // over-count-safe) instead of assuming zero.
        BatchJobStatus::TimeoutExceeded => (
            Outcome::Expired,
            "expired",
            BilledAmount {
                usd: row.estimated_usd,
                estimated: true,
            },
        ),
        // FAILED / CANCELLED: the provider terminated the entry without
        // completing it; Mistral does not bill uncompleted batch entries, so
        // these settle at zero (comment pinned by contract test b4).
        BatchJobStatus::Cancelled => (
            Outcome::SubmitRejected,
            "cancelled",
            BilledAmount {
                usd: 0.0,
                estimated: false,
            },
        ),
        _ => (
            Outcome::SubmitRejected,
            "failed",
            BilledAmount {
                usd: 0.0,
                estimated: false,
            },
        ),
    };
    settle_batch_charge_terminal(
        ledger,
        &row.key,
        intent_token,
        job_id,
        outcome,
        BatchState::Terminal,
        Some(error),
        billed,
        false,
        false,
    )?;
    cleanup_batch_residue(
        ledger,
        client,
        &row.key,
        intent_token,
        row.upload_id.as_deref(),
        row.provider_scope_id.as_deref(),
    )?;
    mark_batch_task_failed_permanent(store, &task.task_id)?;
    Ok(BatchPollDisposition::FailedPermanent)
}

/// Provider job failure → the task joins the existing PERMANENT failure
/// mapping ("既存の失敗写像に合流"): `invalid_input` is the codebase's
/// non-retryable reason bucket (`retry_policy(InvalidInput).max_attempts=0`),
/// so `batch retry` does not auto-revive it — resubmission is a deliberate
/// human step (fresh phase 1 after re-index / explicit re-enqueue).
fn mark_batch_task_failed_permanent(store: &TaskStore, task_id: &str) -> Result<usize> {
    store
        .update_matching(|candidate| {
            if candidate.task_id == task_id {
                candidate.status = TaskStatus::Failed;
                candidate.fallback_reason =
                    Some(retry_reason(RetryErrorKind::InvalidInput).to_owned());
                candidate.hold_reason = None;
                candidate.heartbeat_at = None;
                candidate.attempts = candidate.attempts.saturating_add(1);
                candidate.next_retry_at = None;
                candidate.clear_reservation();
                true
            } else {
                false
            }
        })
        .map_err(pipeline_to_kio)
}

/// Extend a Pending batch task's poll schedule by one interval. A Failed task
/// (e.g. a crashed submit awaiting reconcile self-description) keeps its own
/// retry-policy schedule untouched.
fn extend_batch_poll_schedule(
    store: &TaskStore,
    task_id: &str,
    control: &ControlHandle,
) -> Result<()> {
    let poll_at = batch_poll_retry_at(control);
    store
        .update_matching(|candidate| {
            if candidate.task_id == task_id && candidate.status == TaskStatus::Pending {
                candidate.next_retry_at = Some(poll_at.clone());
                true
            } else {
                false
            }
        })
        .map_err(pipeline_to_kio)?;
    Ok(())
}

/// The Success + fetched-output half of one polled row: verify + materialize
/// through the SAME response half the sync lane uses, then run the §5.8 相 3
/// terminal Tx (成功 = usage-based charge like the sync settle; §3.2 reject =
/// contract_violation with the durable count), then cleanup-first residue
/// deletion, then the task transition.
#[allow(clippy::too_many_arguments)]
fn collect_batch_markdownize_output(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    store: &TaskStore,
    ledger: &LedgerDb,
    client: &dyn MistralBatchClient,
    row: &BatchRequestRow,
    intent_token: &str,
    job_id: &str,
    task: &TaskDescriptor,
    online_profile: &AdapterProfile,
    line: Option<BatchOutputLine>,
) -> Result<BatchPollDisposition> {
    let estimated = BilledAmount {
        usd: row.estimated_usd,
        estimated: true,
    };
    let Some(line) = line else {
        // A SUCCESS job whose output carries no line for our custom_id: a
        // transfer gap indistinguishable from provider data loss (04 §5.8's
        // 転送欠落 note). Conservative: settle at the reservation estimate
        // (over-count-safe), clean up, and leave the task permanently failed
        // for a human-driven fresh submission.
        settle_batch_charge_terminal(
            ledger,
            &row.key,
            intent_token,
            job_id,
            Outcome::UnknownSettled,
            BatchState::Terminal,
            Some("unknown_settled"),
            estimated,
            false,
            false,
        )?;
        cleanup_batch_residue(
            ledger,
            client,
            &row.key,
            intent_token,
            row.upload_id.as_deref(),
            row.provider_scope_id.as_deref(),
        )?;
        mark_batch_task_failed_permanent(store, &task.task_id)?;
        return Ok(BatchPollDisposition::FailedPermanent);
    };
    if line.status_code != 200 {
        // Per-entry provider failure inside a SUCCESS job — same reject
        // terminal as a failed job (Mistral does not bill failed entries).
        settle_batch_charge_terminal(
            ledger,
            &row.key,
            intent_token,
            job_id,
            Outcome::SubmitRejected,
            BatchState::Terminal,
            Some("failed"),
            BilledAmount {
                usd: 0.0,
                estimated: false,
            },
            false,
            false,
        )?;
        cleanup_batch_residue(
            ledger,
            client,
            &row.key,
            intent_token,
            row.upload_id.as_deref(),
            row.provider_scope_id.as_deref(),
        )?;
        mark_batch_task_failed_permanent(store, &task.task_id)?;
        return Ok(BatchPollDisposition::FailedPermanent);
    }
    // §5.8 相 3: persist 直前の tombstone 再検査 — purge 済みなら出力破棄、
    // reject 終端と同形 (error='purged')。課金は provider 報告値だが、この
    // lane の 報告値 (page count) は body の parse 成功が前提 — parse 前の
    // ここでは保守的に reservation estimate で確定する (over-count-safe)。
    if raw_ingest_is_purge_blocked(repo, &task.input_hash) {
        settle_batch_charge_terminal(
            ledger,
            &row.key,
            intent_token,
            job_id,
            Outcome::Purged,
            BatchState::Terminal,
            Some("purged"),
            estimated,
            false,
            false,
        )?;
        cleanup_batch_residue(
            ledger,
            client,
            &row.key,
            intent_token,
            row.upload_id.as_deref(),
            row.provider_scope_id.as_deref(),
        )?;
        retire_batch_task(store, &task.task_id)?;
        return Ok(BatchPollDisposition::Retired);
    }
    // Re-materialize the same input the submit used (the same precondition
    // gate the sync send runs): the output is persisted under the SUBMIT-time
    // identity (input_hash), so an edited/removed working file retires the
    // task instead of persisting under a stale identity.
    let prepared_input = match classify_online_markdownize_precondition(repo, task) {
        OnlineMarkdownizePrecondition::Send(prepared_input) => prepared_input,
        OnlineMarkdownizePrecondition::AwaitConversion
        | OnlineMarkdownizePrecondition::ConversionFailed => {
            // Transient local-material gap (e.g. office converter hiccup):
            // leave the row open and re-collect on a later pass.
            extend_batch_poll_schedule(store, &task.task_id, repo.control())?;
            return Ok(BatchPollDisposition::InFlight);
        }
        OnlineMarkdownizePrecondition::Retire => {
            // Superseded (edited/deleted since submit). The provider DID
            // complete the job; without a persistable target the charge is
            // settled at the conservative reservation estimate
            // (unknown_settled — the codebase's uniform non-success terminal)
            // and the residue is cleaned up.
            settle_batch_charge_terminal(
                ledger,
                &row.key,
                intent_token,
                job_id,
                Outcome::UnknownSettled,
                BatchState::Terminal,
                Some("unknown_settled"),
                estimated,
                false,
                false,
            )?;
            cleanup_batch_residue(
                ledger,
                client,
                &row.key,
                intent_token,
                row.upload_id.as_deref(),
                row.provider_scope_id.as_deref(),
            )?;
            retire_batch_task(store, &task.task_id)?;
            return Ok(BatchPollDisposition::Retired);
        }
    };
    // Persist identity: the resolved online profile (same resolution the sync
    // lane records instances under — seam-aware, network-free under the OCR
    // mock env). If resolution fails (e.g. no credentials in a passive
    // write-command entry poll), defer — never guess an identity.
    let resolved_profile = match resolve_standard_online_markdownize_profile_with_bbox_with_control(
        std::sync::Arc::clone(adapter_settings),
        &row.key.scope_id,
        task.bbox_annotation_enabled.unwrap_or(false),
        repo.control(),
    ) {
        Ok(profile) => profile,
        Err(_) => {
            extend_batch_poll_schedule(store, &task.task_id, repo.control())?;
            return Ok(BatchPollDisposition::InFlight);
        }
    };
    let retry_units: Option<BTreeSet<String>> = task
        .unit_keys
        .as_ref()
        .map(|keys| keys.iter().cloned().collect());
    // Keep the batch retry projection at the task boundary: only units that
    // were actually submitted may satisfy this collection. The adapter owns
    // parsing, image decoding/persistence, and provider-to-Kio conversion.
    let prepared_hints = if prepared_input.prepared_units.is_empty() && retry_units.is_none() {
        None
    } else {
        Some(
            prepared_input
                .prepared_units
                .iter()
                .filter(|unit| {
                    retry_units
                        .as_ref()
                        .is_none_or(|keys| keys.contains(&unit.unit_key))
                })
                .map(|unit| PreparedUnitHint {
                    unit_key: unit.unit_key.clone(),
                    prepared_hash: unit.prepared_hash.clone(),
                    unit_kind: adapter_unit_kind(unit.unit_type),
                    order: unit.order,
                })
                .collect::<Vec<_>>(),
        )
    };
    let converted = materialize_mistral_batch_ocr_body(
        &line.body,
        prepared_hints.as_deref(),
        &prepared_input.media_type,
        &task.input_hash,
        &row.key.scope_id,
        repo.kio_dir(),
        task.bbox_annotation_enabled.unwrap_or(false),
    );
    let (response, effective_hints) = match converted {
        Ok(converted) => converted,
        Err(BatchOcrMaterializationError::Contract(error)) => {
            // §3.2 acceptance reject (04 §5.8 相 3): terminal with the durable
            // contract_violation count. The provider usage report is part of
            // the very body that failed to parse, so the charge degrades to
            // the reservation estimate (§5.4 事前検証の縮退と同形). The
            // reason string lands in the event log — a silent reject cost a
            // live probe to diagnose on the 2026-07-23 OCR run (same lesson
            // as the offline `let _ = error` fix).
            repo.control().append_event_log_in_session(
                repo.lock_session(),
                "KIO-E-ADAPTER-CONTRACT-001",
                "batch OCR output failed acceptance; settled as contract_violation",
                json!({ "input_path": task.input_path, "detail": error.to_string() }),
            )?;
            return settle_batch_contract_reject(
                store,
                ledger,
                client,
                row,
                intent_token,
                job_id,
                task,
                estimated,
            );
        }
        Err(BatchOcrMaterializationError::Persistence(error)) => {
            // The provider result was already fetched and passed all response
            // validation. Keep the ledger reservation and collect it again;
            // treating local image CAS trouble as a contract reject would
            // discard a known billed result and schedule a new send.
            repo.control().append_event_log_in_session(
                repo.lock_session(),
                "KIO-E-BATCH-RESULT-STORE-001",
                "batch OCR image persistence deferred; retaining known provider result",
                json!({ "input_path": task.input_path, "detail": error.to_string() }),
            )?;
            extend_batch_poll_schedule(store, &task.task_id, repo.control())?;
            return Ok(BatchPollDisposition::InFlight);
        }
    };
    let usage = response.usage.clone();
    let materialized = materialize_online_markdownize_response(
        repo,
        task,
        prepared_input.prepared_units.clone(),
        &prepared_input.media_type,
        &prepared_input.bytes,
        retry_units.as_ref(),
        resolved_profile.tool_profile_hash.clone(),
        response,
        &effective_hints,
    );
    let outcome = match materialized {
        Ok(outcome) => outcome,
        Err(failure) if failure.retry_kind == RetryErrorKind::ContractViolation => {
            repo.control().append_event_log_in_session(repo.lock_session(),
                "KIO-E-ADAPTER-CONTRACT-001",
                "batch OCR materialize failed acceptance; settled as contract_violation",
                json!({ "input_path": task.input_path, "detail": format!("materialize reject: {:?}", failure.retry_kind) }),
            )?;
            return settle_batch_contract_reject(
                store,
                ledger,
                client,
                row,
                intent_token,
                job_id,
                task,
                estimated,
            );
        }
        Err(_failure) => {
            // Persist-side fault (I/O etc.): the ledger row stays open and the
            // fetched output is re-collected on a later pass (the §5.8 crash
            // window between collect and the terminal Tx, replayed).
            extend_batch_poll_schedule(store, &task.task_id, repo.control())?;
            return Ok(BatchPollDisposition::InFlight);
        }
    };
    // Success terminal: bill exactly as the sync settle does — the adapter
    // usage report resolved through declared kinds + tools.toml pricing,
    // degrading to the reservation estimate — but at the BATCH rate. This
    // collector only ever runs for a batch job, and it used to read the
    // declaration raw, i.e. at the sync rate.
    let markdownize_pricing =
        lane_adjusted_pricing(adapter_settings, "markdown", PreferredRequestKind::Batch);
    let (billed, uncovered_pricing_kinds) = resolve_billing_from_reported_usage(
        usage.as_ref(),
        &declared_billable_kind_strings(online_profile),
        &markdownize_pricing,
        row.estimated_usd,
    );
    warn_uncovered_pricing_kinds_once(
        "markdownize",
        &uncovered_pricing_kinds,
        repo.control(),
        repo.lock_session(),
    );
    settle_batch_charge_terminal(
        ledger,
        &row.key,
        intent_token,
        job_id,
        Outcome::Succeeded,
        BatchState::Completed,
        None,
        billed,
        false,
        false,
    )?;
    cleanup_batch_residue(
        ledger,
        client,
        &row.key,
        intent_token,
        row.upload_id.as_deref(),
        row.provider_scope_id.as_deref(),
    )?;
    let task_id = task.task_id.clone();
    store
        .update_matching(|candidate| {
            if candidate.task_id == task_id {
                candidate.status = outcome.status;
                candidate.output_ref = outcome.output_ref.clone();
                candidate.fallback_reason = Some("online_adapter_done".to_owned());
                candidate.hold_reason = None;
                candidate.heartbeat_at = None;
                candidate.next_retry_at = None;
                candidate.clear_reservation();
                if matches!(outcome.status, TaskStatus::Partial | TaskStatus::Failed) {
                    // Mirrors the sync Ok arm: every known incomplete response,
                    // including an all-Failed manifest, consumed one re-drive.
                    candidate.attempts = candidate.attempts.saturating_add(1);
                }
                candidate.mode = Some(outcome.mode);
                candidate.previous_raw_hash = outcome.previous_raw_hash.clone();
                candidate.parent_run_id = outcome.parent_run_id.clone();
                candidate.changed_unit_keys = outcome.changed_unit_keys.clone();
                true
            } else {
                false
            }
        })
        .map_err(pipeline_to_kio)?;
    Ok(BatchPollDisposition::Collected(
        known_markdownize_completion(store, &task_id)?,
    ))
}

/// §3.2 acceptance reject at collect time (04 §5.8 相 3): terminal Tx with
/// `error='contract_violation'` + the durable `contract_violation_count`
/// increment, charge at the conservative reservation estimate, cleanup-first
/// residue deletion, and the task joins the ordinary contract_violation retry
/// mapping (retryable once — a fresh phase 1 under a new token/seq).
#[allow(clippy::too_many_arguments)]
fn settle_batch_contract_reject(
    store: &TaskStore,
    ledger: &LedgerDb,
    client: &dyn MistralBatchClient,
    row: &BatchRequestRow,
    intent_token: &str,
    job_id: &str,
    task: &TaskDescriptor,
    estimated: BilledAmount,
) -> Result<BatchPollDisposition> {
    settle_batch_charge_terminal(
        ledger,
        &row.key,
        intent_token,
        job_id,
        Outcome::ContractViolation,
        BatchState::Terminal,
        Some("contract_violation"),
        estimated,
        true,
        false,
    )?;
    cleanup_batch_residue(
        ledger,
        client,
        &row.key,
        intent_token,
        row.upload_id.as_deref(),
        row.provider_scope_id.as_deref(),
    )?;
    let policy = retry_policy(RetryErrorKind::ContractViolation);
    let attempts_after = task.attempts.saturating_add(1);
    let next_retry_at = (policy.retryable
        && policy
            .max_attempts
            .map(|max| attempts_after < max)
            .unwrap_or(true))
    .then(|| {
        scheduled_retry_at(
            &ledger.control().now_utc_seconds(),
            &policy.backoff,
            attempts_after,
        )
    });
    let reason = retry_reason(RetryErrorKind::ContractViolation).to_owned();
    let task_id = task.task_id.clone();
    let retryable = next_retry_at.is_some();
    store
        .update_matching(|candidate| {
            if candidate.task_id == task_id {
                candidate.status = TaskStatus::Failed;
                candidate.fallback_reason = Some(reason.clone());
                candidate.hold_reason = None;
                candidate.heartbeat_at = None;
                candidate.attempts = attempts_after;
                candidate.next_retry_at = next_retry_at.clone();
                candidate.clear_reservation();
                true
            } else {
                false
            }
        })
        .map_err(pipeline_to_kio)?;
    Ok(if retryable {
        BatchPollDisposition::FailedRetryable
    } else {
        BatchPollDisposition::FailedPermanent
    })
}

/// Retire a batch task whose collect-time precondition failed (superseded /
/// purged) — same shape as the send-time `Retire` path.
fn retire_batch_task(store: &TaskStore, task_id: &str) -> Result<usize> {
    store
        .update_matching(|candidate| {
            if candidate.task_id == task_id {
                retire_online_task(candidate);
                candidate.attempts = candidate.attempts.saturating_add(1);
                true
            } else {
                false
            }
        })
        .map_err(pipeline_to_kio)
}

pub(crate) fn retire_online_task(task: &mut TaskDescriptor) {
    task.status = TaskStatus::Failed;
    task.fallback_reason = Some(RETIRED_NON_LIVE.to_owned());
    // A Failed task is not a hold — this shared helper can now retire a
    // Paused(auth) row too (QA2/QA3's supersede-on-edit / R18-2 orphan-sweep
    // callers admit Paused), so it must clear `hold_reason` itself.
    task.hold_reason = None;
    task.next_retry_at = None;
    task.heartbeat_at = None;
    task.clear_reservation();
}
