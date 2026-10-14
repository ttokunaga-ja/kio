use crate::approvals;
use crate::batch::command::partial_retry_plan_from_instance;
use crate::batch::command::reclaim_orphaned_running_tasks;
use crate::commands::IndexArgs;
use crate::egress::SECRETS_TIER_B_HOLD;
use crate::egress::embedding_online_allowed;
use crate::egress::persistent_network_allowed;
use crate::egress::secrets_send_approved;
use crate::embedding::enrichment::run_embedding_enrichment;
use crate::embedding::transport::effective_invocation_lane;
use crate::embedding::transport::lane_adjusted_pricing;
use crate::embedding::transport::realtime_lane_requested;
use crate::index::command::OFFICE_CONVERSION_UNAVAILABLE_REASON;
use crate::index::command::record_unsupported_if_changed;
use crate::index::pipeline::IndexPipelineResult;
use crate::index::pipeline::offline_markdownize_enqueue_status;
use crate::index::pipeline::offline_output_ref;
use crate::index::pipeline::online_markdownize_profile_for;
use crate::index::pipeline::online_output_ref;
use crate::invocation::Invocation;
use crate::ledger_access::RETIRED_NON_LIVE;
use crate::ledger_access::TaskChargeOutcome;
use crate::ledger_access::budget_cap_config;
use crate::ledger_access::capture_search_budget_observation;
use crate::ledger_access::declared_billable_kind_strings;
use crate::ledger_access::ledger_db_path;
use crate::ledger_access::ledger_snapshot_to_kio;
use crate::ledger_access::mark_sync_result_unknown_task;
use crate::ledger_access::open_ledger_db;
use crate::ledger_access::release_task_charge_if_open;
use crate::ledger_access::reserve_or_reuse_task_charge;
use crate::ledger_access::settle_task_charge_success;
use crate::ledger_access::settle_task_charge_unknown;
use crate::ledger_access::sync_result_unknown_fenced;
use crate::ledger_access::task_ledger_key;
use crate::ledger_access::warn_uncovered_pricing_kinds_once;
use crate::markdownize::batch::BATCH_SUBMITTED_REASON;
use crate::markdownize::batch::MarkdownizeSendLane;
use crate::markdownize::batch::batch_markdownize_lane_client;
use crate::markdownize::batch::batch_poll_retry_at;
use crate::markdownize::batch::markdownize_send_lane;
use crate::markdownize::batch::retire_online_task;
use crate::markdownize::batch::submit_batch_markdownize_job;
use crate::markdownize::batch::sync_markdownize_credential_available;
use crate::markdownize::execution::OnlineMarkdownizePrecondition;
use crate::markdownize::execution::TaskExecutionFailure;
use crate::markdownize::execution::classify_online_markdownize_precondition;
use crate::markdownize::execution::execute_online_markdownize_task;
use crate::markdownize::execution::prorated_markdownize_cost;
use crate::markdownize::execution::task_descriptor;
use crate::object_persistence::pipeline_to_kio;
use crate::online_task::targets_standard_online_markdownize;
use crate::promotion::finish_pending_online_promotion;
use crate::promotion::promote_completed_online_markdownize;
use crate::promotion::recover_pending_online_promotion;
use crate::response::status::{BudgetStatusResponse, PausedTasksByHoldReason};
use crate::settings::bbox_annotation_enabled;
use crate::settings::user_config_toml_path;
use kio_adapter::catalog::active_local_ocr_execution_with_control;
use kio_adapter::catalog::local_ocr_handles_media_type;
use kio_adapter::office_convert::is_office_media;
use kio_adapter::office_convert::resolve_office_converter_with_control;
use kio_adapter::tool_lock::AdapterRuntimeSettings;
use kio_adapter::traits::PreferredRequestKind;
use kio_core::KioError;
use kio_core::Result;
use kio_core::control::ControlHandle;
use kio_core::scope::Repository;
use kio_pipeline::budget::BudgetCapKind;
use kio_pipeline::budget::BudgetCaps;
use kio_pipeline::budget::BudgetEstimate;
use kio_pipeline::budget::budget_warning;
use kio_pipeline::budget::estimate_local_baseline_cost;
use kio_pipeline::budget::evaluate_budget_with_caps;
use kio_pipeline::budget::read_budget_policy;
use kio_pipeline::budget::utc_month;
use kio_pipeline::ledger::BatchState;
use kio_pipeline::ledger::LedgerDb;
use kio_pipeline::ledger::LedgerReadSnapshot;
use kio_pipeline::ledger::Outcome;
use kio_pipeline::ledger::RequestKind;
use kio_pipeline::ledger::TaskKey as LedgerTaskKey;
use kio_pipeline::ledger::ops::BilledAmount;
use kio_pipeline::ledger::ops::TerminalWrite;
use kio_pipeline::ledger::ops::get_batch_request;
use kio_pipeline::ledger::ops::ledger_month_total;
use kio_pipeline::ledger::ops::resolve_billing_from_reported_usage;
use kio_pipeline::ledger::ops::terminal_transaction;
use kio_pipeline::markdownize::MarkdownizeMode;
use kio_pipeline::scan::ScanCandidate;
use kio_pipeline::scan::classify_secret;
use kio_pipeline::task::HoldReason;
use kio_pipeline::task::LEDGER_INITIALIZATION_REQUIRED_REASON;
use kio_pipeline::task::RESULT_UNKNOWN_REASON;
use kio_pipeline::task::RetryErrorKind;
use kio_pipeline::task::TaskDescriptor;
use kio_pipeline::task::TaskStatus;
use kio_pipeline::task::TaskStore;
use kio_pipeline::task::TaskType;
use kio_pipeline::task::retry_policy;
use kio_pipeline::unsupported::UNSUPPORTED_REASON_RESOLVED;
use kio_pipeline::unsupported::UnsupportedInputDisposition;
use kio_pipeline::unsupported::UnsupportedInputStore;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

/// Classify an accepted, already-persisted online result for batch reporting.
/// Its immutable manifest is the source of retryability; settlement has already
/// happened before this function and is deliberately not repeated.
pub(crate) fn known_markdownize_completion(
    store: &TaskStore,
    task_id: &str,
) -> Result<ExecOutcome> {
    let task = store
        .all()
        .map_err(pipeline_to_kio)?
        .into_iter()
        .find(|task| task.task_id == task_id)
        .ok_or_else(|| KioError::schema("completed markdownize task disappeared"))?;
    if task.status == TaskStatus::Done {
        return Ok(ExecOutcome {
            executed: 1,
            ..ExecOutcome::default()
        });
    }
    let mut outcome = ExecOutcome {
        failed: 1,
        ..ExecOutcome::default()
    };
    if matches!(task.status, TaskStatus::Partial | TaskStatus::Failed)
        && !partial_retry_plan_from_instance(store, &task)?
            .retryable_units
            .is_empty()
    {
        outcome.failed_retryable = 1;
    }
    Ok(outcome)
}

/// R9-7: outcome counts from an enrichment pass. `batch retry`/`resume` need more
/// than the success count to be honest: a driven task can fail (an online send was
/// attempted, rate-limit / auth consumed) yet the old JSON reported only
/// `tasks_executed`, so an orchestrator could not see the attempt. `executed`
/// keeps its meaning (tasks completed OK == `tasks_executed`); `failed` is tasks
/// that transitioned to Failed this pass; `attempted()` = executed + failed
/// (budget-paused / held tasks that issued no adapter call are excluded).
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ExecOutcome {
    pub(crate) executed: usize,
    pub(crate) failed: usize,
    /// R11-2: tasks whose online send this pass was halted by the budget cap
    /// (transitioned to Paused/budget_exceeded). Excluded from `attempted()` — no
    /// adapter call was issued. Drives docs/04 §5.6 exit 6.
    pub(crate) paused: usize,
    /// R11-2: subset of `failed` whose error was auth/authorization (docs/04 §5.6
    /// exit 5 — needs user re-auth, distinct from a retryable transient).
    pub(crate) auth_failed: usize,
    /// R11-2: subset of `failed` that is retry-eligible (retryable kind + attempts
    /// left). `failed - failed_retryable` is the permanently-failed remainder; the
    /// split drives the batch exit-code choice 3 (some retryable) vs 4 (all permanent).
    pub(crate) failed_retryable: usize,
    /// Batch-lane tasks left in flight by this pass (04 §5.8): freshly
    /// submitted jobs and polled still-QUEUED/RUNNING jobs. Excluded from
    /// `attempted()` (nothing terminal happened), but counted as
    /// retryable-remaining for the batch exit choice — "in-flight のみ残"
    /// exits 3 exactly like a retryable failure would, since a later
    /// `kio batch resume` is what makes progress.
    pub(crate) inflight: usize,
    /// Subset of `inflight` held because the provider's response could NOT be
    /// read — not because the job is still running. 04 §5.8's unknown rule says
    /// to hold and retry such a row, which is right, but a bare hold is
    /// indistinguishable from a healthy queued job and so hides a permanent
    /// fault behind an ordinary-looking count.
    ///
    /// Two real defects reached production behind exactly that silence (a
    /// result parser that could not read the live shape, then a size bound that
    /// rejected any job past ~20 members): in both cases the provider had every
    /// vector ready and `tasks_inflight` simply never dropped. A non-zero value
    /// here means "stop waiting and look", not "wait longer".
    pub(crate) held_unreadable: usize,
}

impl ExecOutcome {
    pub(crate) fn attempted(self) -> usize {
        self.executed + self.failed
    }

    pub(crate) fn add(&mut self, other: ExecOutcome) {
        self.executed += other.executed;
        self.failed += other.failed;
        self.paused += other.paused;
        self.auth_failed += other.auth_failed;
        self.failed_retryable += other.failed_retryable;
        self.inflight += other.inflight;
        self.held_unreadable += other.held_unreadable;
    }
}

pub(crate) fn batch_completion(outcome: &ExecOutcome) -> crate::response::Completion {
    use crate::response::Completion;
    if outcome.auth_failed > 0 {
        Completion::AuthRequired
    } else if outcome.paused > 0 {
        Completion::BudgetBlocked
    } else if outcome.failed_retryable > 0 || outcome.inflight > 0 {
        Completion::Partial
    } else if outcome.failed > 0 {
        Completion::Permanent
    } else {
        Completion::Complete
    }
}
pub(crate) fn batch_error_code(completion: crate::response::Completion) -> Option<String> {
    use crate::response::Completion;
    match completion {
        Completion::Partial => Some("KIO-E-BATCH-PARTIAL-001".to_owned()),
        Completion::Permanent => Some("KIO-E-BATCH-TASK-FAILED-001".to_owned()),
        _ => None,
    }
}
pub(crate) fn enrichment_completion(outcome: &ExecOutcome) -> crate::response::Completion {
    use crate::response::Completion;
    if outcome.auth_failed > 0 {
        Completion::AuthRequired
    } else if outcome.paused > 0 {
        Completion::BudgetBlocked
    } else {
        Completion::Complete
    }
}

pub(crate) fn execute_pending_tasks(
    invocation: Invocation<'_>,
    repo: &Repository,
    store: &TaskStore,
    override_budget: bool,
    // R22-6(a): `batch resume` means "carry on with the work", so it may revive a task the
    // operator has since unblocked by fixing credentials. `batch retry` may NOT — CT2-TASK-005
    // (docs/04 §5.6, `auth_error: max_attempts=0`) makes non-retryability of an auth failure a
    // contract of that command specifically.
    allow_auth_revive: bool,
    // QA30 (step4b-contract-tests-p3a.md §I, 06-cli-spec.md §1 L21-30,
    // 07-adapter-spec.md §3 L220-222): `kio batch resume`/`kio batch retry`'s
    // own `--online`/`--offline` one-shot embedding opt-in for this pass.
    // Mutually exclusive, validated by the caller before this is reached.
    online: bool,
    offline: bool,
) -> Result<ExecOutcome> {
    let ctx = &invocation;
    let adapter_settings = invocation.settings;

    let mut outcome = ExecOutcome::default();
    // Q3: under the folder store lock, any Running task is an orphan from a crashed
    // run — reclaim it to Pending so this pass re-executes it.
    reclaim_orphaned_running_tasks(store)?;
    if recover_pending_online_promotion(repo)? {
        finish_pending_online_promotion(ctx, repo)?;
    }
    // Markdownize and embedding opt-ins are per-adapter (07 §3, L4): gate each
    // adapter on its own approval rather than one blanket check.
    if !offline && persistent_network_allowed(adapter_settings, repo)? {
        outcome.add(execute_pending_markdownize_tasks(
            ctx,
            repo,
            store,
            override_budget,
            allow_auth_revive,
        )?);
    }
    // Online Markdownize changes durable search truth only after the complete
    // output is rebound into one auto commit and SQLite is rebuilt. Do this before
    // embedding enrichment so the same batch pass can see/promote the new chunks.
    if promote_completed_online_markdownize(ctx, repo)? {
        finish_pending_online_promotion(ctx, repo)?;
    }
    // Embedding tasks are executed by the same enrichment pass `kio index` uses;
    // without this, rate-limited Pending embedding tasks could never be completed
    // by `batch resume` / `batch retry`. `override_budget` reaches the budget
    // judgement (L2) and the embedding opt-in is the embedding adapter's own (L4).
    // QA30: `online`/`offline` (this pass's flags) replace the previous
    // hard-coded `(false, false, false)` — `batch resume`/`retry` carry no
    // same-invocation `--yes`/`--approve`-equivalent flag, so
    // `online_confirmed` stays `false` (matching `kio index`'s own
    // `--online`-without-`--approve` behavior: `--online` alone still needs
    // an existing approval row).
    let embedding_online =
        embedding_online_allowed(adapter_settings, repo, offline, online, false)?;
    outcome.add(run_embedding_enrichment(
        ctx,
        repo,
        embedding_online,
        override_budget,
        allow_auth_revive,
    )?);
    Ok(outcome)
}

/// Execute Pending online markdownize tasks, honoring `override_budget` (L2 i).
/// R9-7: returns success + failure counts so `batch` JSON can report driven
/// attempts, not only successes.
fn execute_pending_markdownize_tasks(
    ctx: &Invocation<'_>,
    repo: &Repository,
    store: &TaskStore,
    override_budget: bool,
    allow_auth_revive: bool,
) -> Result<ExecOutcome> {
    let adapter_settings = ctx.settings;

    let budget_caps = read_budget_policy(
        user_config_toml_path(ctx.control),
        repo.kio_dir().join("config.toml"),
    )
    .map_err(pipeline_to_kio)?;
    let ledger = open_ledger_db(ctx.control)?;
    let month = utc_month(&ctx.control.now_utc_seconds());
    let scope_id = repo.scope_identity()?.scope_id;
    // N1a (defense in depth): even a Pending online markdownize task must not be
    // sent when its input is a Tier B (candidate-secret) file and the scope is not
    // `--send-secrets`-approved — in case the hold was cleared by some other path.
    let online_profile = online_markdownize_profile_for(repo)?;
    let secrets_approved = secrets_send_approved(
        adapter_settings,
        repo,
        approvals::AdapterRole::Markdown,
        &online_profile.adapter_id,
        &online_profile.tool_profile_hash,
    );
    let output_ref = online_output_ref(&online_profile.adapter_id);
    // R22-6(a), updated for QA2 (step4b-contract-tests-p3a.md §A, 04 §5.2):
    // R21-6 gave the embedding pipeline a live-AuthError revive but never wired
    // the markdownize twin, so a 401/403 left its task stranded — with its
    // reservation eating the month's cap. Under QA2 an auth failure lands
    // `Paused(hold_reason=auth)` (never `Failed`), and `run_batch`'s Resume arm
    // already generically flips ANY non-budget non-secret Paused task back to
    // Pending BEFORE this function runs — so that flip is normally what revives
    // it. This block is the belt-and-braces for a caller that reaches
    // `execute_pending_markdownize_tasks` WITHOUT going through that flip (and
    // for any Paused(auth) row the flip's own iteration order left unrevived
    // this same pass), and it still uniquely owns the ledger release: 401/403
    // is refused before billing (R20-3), so releasing its still-open ledger row
    // (if any) settles it `unknown_settled` rather than leaving it stranded
    // open. Only a task whose precondition is not `Retire` (file present,
    // unedited, within the cap) is revived; a genuinely stale one is retired by
    // the loop below as before.
    let markdownize_adapter_kind = "markdownize";
    let markdown_profile_hash = online_profile.tool_profile_hash.clone();
    let auth_revivable = if allow_auth_revive {
        store
            .all()
            .map_err(pipeline_to_kio)?
            .into_iter()
            .filter(|task| {
                task.task_type == TaskType::Markdownize
                    && targets_standard_online_markdownize(store.kio_dir(), task, &output_ref)
                    && task.status == TaskStatus::Paused
                    && retry_kind_from_reason(task.fallback_reason.as_deref())
                        == RetryErrorKind::AuthError
                    && !matches!(
                        classify_online_markdownize_precondition(repo, task),
                        OnlineMarkdownizePrecondition::Retire
                    )
            })
            .map(|task| task.task_id)
            .collect::<BTreeSet<_>>()
    } else {
        BTreeSet::new()
    };
    if !auth_revivable.is_empty() {
        for task in store.all().map_err(pipeline_to_kio)? {
            if auth_revivable.contains(&task.task_id) {
                let key = task_ledger_key(
                    &scope_id,
                    markdownize_adapter_kind,
                    &task.input_hash,
                    &markdown_profile_hash,
                );
                release_task_charge_if_open(&ledger, &key)?;
            }
        }
        store
            .update_matching(|task| {
                if !auth_revivable.contains(&task.task_id) {
                    return false;
                }
                task.clear_reservation();
                task.status = TaskStatus::Pending;
                task.fallback_reason = Some("ready_for_online_adapter".to_owned());
                task.hold_reason = None;
                task.attempts = 0;
                task.next_retry_at = None;
                task.heartbeat_at = None;
                true
            })
            .map_err(pipeline_to_kio)?;
    }
    let tasks = store
        .all()
        .map_err(pipeline_to_kio)?
        .into_iter()
        .filter(|task| {
            task.status == TaskStatus::Pending
                && task.task_type == TaskType::Markdownize
                && targets_standard_online_markdownize(store.kio_dir(), task, &output_ref)
                // Honor an unelapsed retry backoff even for a Pending task
                // (Step2c I2); `batch retry` already gates on this, so this is
                // a defensive belt-and-braces guard.
                && task_retry_due(task, ctx.control)
                // R19-1: the send-time defensive re-check must exclude ANY unapproved
                // secret (Tier B or a lifted Tier A), not just Tier B.
                && (secrets_approved
                    || classify_secret(&task.input_path).is_none())
        })
        .collect::<Vec<_>>();
    let mut counts = ExecOutcome::default();
    // Batch send lane (04 §5.8 / 07 §5.5): resolved once per pass — `Some`
    // exactly when this pass can drive upload → job-create against a
    // configured (mock or real) Batch client.
    let batch_client = batch_markdownize_lane_client(adapter_settings, &ledger)?;
    for task in tasks {
        let task_id = task.task_id.clone();
        let key = task_ledger_key(
            &scope_id,
            markdownize_adapter_kind,
            &task.input_hash,
            &markdown_profile_hash,
        );
        // R15-2: verify the network-free preconditions BEFORE reserving the cost. The
        // reservation lands under a `BEGIN IMMEDIATE` Tx BEFORE the send, but a stale
        // (edited-after-enqueue), text-native, or unpreparable task is superseded by
        // R14-2 inside the executor WITHOUT ever calling the adapter — so charging
        // first is a phantom charge that double-bills and can exhaust the markdownize
        // cap, falsely pausing the valid task. Fail the task here (non-retryable
        // invalid_input; the recovery is a re-index, not a retry) instead of charging
        // + entering the executor. This mirrors the executor's own R14-2/R9-2 guards,
        // hoisted ahead of the charge.
        let prepared_input = match classify_online_markdownize_precondition(repo, &task) {
            OnlineMarkdownizePrecondition::Send(prepared_input) => prepared_input,
            OnlineMarkdownizePrecondition::AwaitConversion => {
                // A live Office document remains honest Pending work until its bounded
                // local conversion contract exists. Nothing was sent or charged.
                continue;
            }
            OnlineMarkdownizePrecondition::Retire => {
                // R18-2: this task may carry a still-open reservation from a PRIOR
                // failed send (its file was deleted/edited after that attempt).
                // Release it here — settling `unknown_settled` at the reservation
                // estimate if a row is still open, a no-op otherwise — BEFORE the
                // retirement below; no NEW charge is reserved this pass (the
                // precondition failed before the charge).
                release_task_charge_if_open(&ledger, &key)?;
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
                    .map_err(pipeline_to_kio)?;
                counts.failed += 1;
                continue;
            }
            OnlineMarkdownizePrecondition::ConversionFailed => {
                // 07 §5.1: mirrors the generic `TaskExecutionFailure` retry-kind
                // handling below (the `_ => {...}` arm after the adapter send),
                // applied here because the failure was discovered BEFORE any
                // adapter call — the same contract_violation retry accounting
                // (04 §5.3: retryable, one retry), but no ledger charge was ever
                // reserved this pass (mirrors `Retire` immediately above: only a
                // still-open reservation from a PRIOR attempt is released).
                release_task_charge_if_open(&ledger, &key)?;
                let policy = retry_policy(RetryErrorKind::ContractViolation);
                let attempts_after = task.attempts.saturating_add(1);
                let next_retry_at = (policy.retryable
                    && policy
                        .max_attempts
                        .map(|max| attempts_after < max)
                        .unwrap_or(true))
                .then(|| {
                    scheduled_retry_at(
                        &ctx.control.now_utc_seconds(),
                        &policy.backoff,
                        attempts_after,
                    )
                });
                let reason = retry_reason(RetryErrorKind::ContractViolation).to_owned();
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
                counts.failed += 1;
                if next_retry_at.is_some() {
                    counts.failed_retryable += 1;
                }
                continue;
            }
        };
        let file_size = prepared_input.bytes.len() as u64;
        // R11-6: prorate the reserved cost of a UNIT-SCOPED retry by the fraction of
        // the document actually re-sent — `unit_keys` names the still-failed units,
        // and `execute_online_markdownize_task` requests only those. A full send
        // (`unit_keys == None`) still bills the whole document. Without this, a
        // 1-page retry of a 500-page PDF re-billed all 500 pages. Only used as the
        // *candidate* for a FRESH reservation below — CL42/CL44: an already-open row
        // from a prior attempt (`reserve_or_reuse_task_charge`'s reuse path) is
        // billed at ITS OWN stored `estimated_usd`, not recomputed here.
        let candidate_usd = prorated_markdownize_cost(
            &task,
            file_size,
            prepared_input.prepared_units.len(),
            task.bbox_annotation_enabled.unwrap_or(false),
        );
        let caps = budget_cap_config(&budget_caps, &scope_id, markdownize_adapter_kind);
        // F5: `hard_stop = false` (soft-stop) bypasses a cap denial exactly like
        // `--override-budget` does — see `reserve_or_reuse_task_charge`'s doc.
        let bypass_cap_denial = override_budget || !budget_caps.hard_stop;
        // Lane selection (04 §5.8 / 07 §5.5): decided BEFORE the reservation so
        // phase 1 records the matching `request_kind` ('batch' rows are what the
        // §5.8 recovery/poll machinery keys on).
        let existing_row = get_batch_request(&ledger, &key).map_err(pipeline_to_kio)?;
        if sync_result_unknown_fenced(existing_row.as_ref()) {
            mark_sync_result_unknown_task(store, &task_id)?;
            counts.failed += 1;
            continue;
        }
        if existing_row
            .as_ref()
            .is_some_and(|row| row.state.is_terminal() && row.intent_token.is_some())
        {
            // §5.8 順序規範 (cleanup-first): a terminal row whose residue
            // cleanup is still outstanding blocks re-submission — a fresh
            // phase 1 would be refused (KIO-E-BATCH-CLEANUP-PENDING-001), so
            // skip the task this pass instead of failing the whole command.
            // The stalled row is visible in `kio status`; the poll pass's
            // cleanup replay, `kio ledger reconcile`, or `kio batch abandon`
            // complete the cleanup and unblock the resend.
            counts.inflight += 1;
            continue;
        }
        let lane = markdownize_send_lane(
            batch_client.is_some(),
            existing_row.as_ref(),
            realtime_lane_requested(),
        );
        if lane == MarkdownizeSendLane::DeferToRecovery {
            counts.inflight += 1;
            continue;
        }
        // A fresh sync send has no provider-side residue yet. Resolve the
        // declared credential locally before creating its phase-1 reservation:
        // an unset/empty credential cannot reach the HTTP client, so recording
        // a charge here would be a phantom. Do not apply this admission to an
        // existing sync row: that row may cover an earlier request whose result
        // remains uncertain, and must retain the normal unknown settlement.
        if lane == MarkdownizeSendLane::Sync
            && !sync_markdownize_credential_available(adapter_settings, ctx.control)?
        {
            let reason = retry_reason(RetryErrorKind::AuthError).to_owned();
            store
                .update_matching(|candidate| {
                    if candidate.task_id == task_id {
                        candidate.status = TaskStatus::Paused;
                        candidate.hold_reason = Some(HoldReason::Auth);
                        candidate.fallback_reason = Some(reason.clone());
                        candidate.heartbeat_at = None;
                        candidate.next_retry_at = None;
                        candidate.clear_reservation();
                        true
                    } else {
                        false
                    }
                })
                .map_err(pipeline_to_kio)?;
            counts.failed += 1;
            counts.auth_failed += 1;
            continue;
        }
        let request_kind = if lane == MarkdownizeSendLane::Batch {
            RequestKind::Batch
        } else {
            RequestKind::Sync
        };
        let charge = reserve_or_reuse_task_charge(
            ctx,
            &ledger,
            &key,
            candidate_usd,
            &caps,
            bypass_cap_denial,
            request_kind,
        )?;
        let (intent_token, reserved_usd) = match charge {
            TaskChargeOutcome::BudgetExceeded => {
                store
                    .update_matching(|candidate| {
                        if candidate.task_id == task_id {
                            candidate.status = TaskStatus::Paused;
                            candidate.fallback_reason = Some("budget_exceeded".to_owned());
                            candidate.hold_reason = Some(HoldReason::Budget);
                            true
                        } else {
                            false
                        }
                    })
                    .map_err(pipeline_to_kio)?;
                // R11-2: a Pending task budget-paused THIS pass → docs/04 §5.6 exit 6.
                counts.paused += 1;
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
        if lane == MarkdownizeSendLane::Batch {
            let client = batch_client
                .as_deref()
                .expect("Batch lane selection guarantees a configured client");
            match submit_batch_markdownize_job(
                adapter_settings,
                client,
                &ledger,
                &key,
                &intent_token,
                &task,
                &prepared_input,
            )? {
                Ok(()) => {
                    // Submitted (or already in flight): the task stays Pending
                    // with the poll cadence as its schedule — a later pass's
                    // `poll_batch_markdownize_jobs` collects it. The
                    // reservation stamps mirror the sync Running flip so purge
                    // and diagnostics can find the live ledger row.
                    let poll_at = batch_poll_retry_at(ctx.control);
                    store
                        .update_matching(|candidate| {
                            if candidate.task_id == task_id {
                                candidate.status = TaskStatus::Pending;
                                candidate.fallback_reason = Some(BATCH_SUBMITTED_REASON.to_owned());
                                candidate.hold_reason = None;
                                candidate.heartbeat_at = None;
                                candidate.next_retry_at = Some(poll_at.clone());
                                candidate.reservation_id = Some(intent_token.clone());
                                candidate.reserved_usd = Some(reserved_usd);
                                candidate.reserved_month = Some(month.clone());
                                true
                            } else {
                                false
                            }
                        })
                        .map_err(pipeline_to_kio)?;
                    counts.inflight += 1;
                }
                Err(error) => {
                    // 相 2a/2b records stay in place (= the §5.8 crash window a
                    // later recovery reconciles), so the ledger row is NEVER
                    // settled here — `settle_on_terminal: false`.
                    record_online_send_failure(
                        store,
                        &ledger,
                        &key,
                        &intent_token,
                        reserved_usd,
                        &task_id,
                        task.attempts,
                        &error,
                        &mut counts,
                        false,
                    )?;
                }
            }
            continue;
        }
        store
            .update_matching(|candidate| {
                if candidate.task_id == task_id {
                    candidate.status = TaskStatus::Running;
                    candidate.heartbeat_at = Some(ctx.control.now_utc_seconds());
                    candidate.fallback_reason = None;
                    // Stamp the live reservation's ledger selector (purge and any
                    // future diagnostics look it up by `intent_token`, not by
                    // reconstructing the ledger key from possibly-since-changed
                    // config — see `purge.rs`'s `delete_target_tasks`). `reserved_usd`/
                    // `reserved_month` accompany it only because
                    // `task::validate_task_descriptor` requires all three stamps
                    // present together or all absent; the ledger's own
                    // `batch_requests.estimated_usd` is the actual source of truth.
                    candidate.reservation_id = Some(intent_token.clone());
                    candidate.reserved_usd = Some(reserved_usd);
                    candidate.reserved_month = Some(month.clone());
                    true
                } else {
                    false
                }
            })
            .map_err(pipeline_to_kio)?;
        // QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): thread this
        // send's ledger `intent_token` (stable across a crash-window resend —
        // see `MarkdownizeRequest::idempotency_token`'s doc) into the adapter
        // boundary, so a provider that declares a sync idempotency key can be
        // required to carry it.
        match execute_online_markdownize_task(
            adapter_settings,
            repo,
            &task,
            prepared_input,
            Some(intent_token.clone()),
        ) {
            Ok(outcome) => {
                // QA17/QA18/QA19 (step4b-contract-tests-p3a.md §F): bill the
                // adapter's self-reported `usage` when it provided one
                // (Mistral OCR's real processed-page count), converted to
                // USD via tools.toml's declared `[pricing]` — degrading to
                // the reservation estimate exactly as before when `usage` is
                // absent/malformed/uncovered (`resolve_billing_from_reported_usage`'s
                // existing CL27/CL28 rules).
                let markdownize_profile = online_markdownize_profile_for(repo)?;
                // This arm IS the synchronous send, so it settles at the
                // standard rate. Reading the declaration raw made that true by
                // accident and made the batch arm below wrong.
                let markdownize_pricing =
                    lane_adjusted_pricing(adapter_settings, "markdown", PreferredRequestKind::Sync);
                let (billed, uncovered_pricing_kinds) = resolve_billing_from_reported_usage(
                    outcome.usage.as_ref(),
                    &declared_billable_kind_strings(&markdownize_profile),
                    &markdownize_pricing,
                    reserved_usd,
                );
                warn_uncovered_pricing_kinds_once(
                    "markdownize",
                    &uncovered_pricing_kinds,
                    ctx.control,
                    ctx.locks,
                );
                settle_task_charge_success(&ledger, &key, &intent_token, billed)?;
                store
                    .update_matching(|candidate| {
                        if candidate.task_id == task_id {
                            candidate.status = outcome.status;
                            candidate.output_ref = outcome.output_ref.clone();
                            candidate.fallback_reason = Some("online_adapter_done".to_owned());
                            candidate.heartbeat_at = None;
                            candidate.clear_reservation();
                            // R19-5: a unit-scoped retry that still returns Partial (units
                            // missing) consumed one re-drive of the retry budget — advance
                            // `attempts` HERE, the single accounting point, so it converges
                            // on `max_attempts` (the Err arm below does the same). Pre-R19-5
                            // this lived in the incomplete-manifest reenqueue path and
                            // double-counted with the Err arm.
                            if matches!(outcome.status, TaskStatus::Partial | TaskStatus::Failed) {
                                candidate.attempts = candidate.attempts.saturating_add(1);
                            }
                            // R13-1: record the normalization-run provenance
                            // (docs/04:474-479) so `status` and the next incremental
                            // decision (consecutive count) can see mode / previous /
                            // changed units. `Full` clears any prior incremental mark.
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
                counts.add(known_markdownize_completion(store, &task_id)?);
            }
            Err(error) => {
                record_sync_markdownize_failure(
                    store,
                    &ledger,
                    &key,
                    &intent_token,
                    reserved_usd,
                    &task_id,
                    task.attempts,
                    &error,
                    &mut counts,
                )?;
            }
        }
    }
    Ok(counts)
}

/// Settle a provider-confirmed sync rejection as zero-cost before any retry.
/// Only typed adapter outcomes may take this path; transport text is never
/// interpreted as a rejection.
fn settle_sync_definitive_rejection(
    ledger: &LedgerDb,
    key: &LedgerTaskKey,
    intent_token: &str,
    kind: RetryErrorKind,
) -> Result<()> {
    let error = match kind {
        RetryErrorKind::AuthError => "sync_auth_rejected",
        RetryErrorKind::RateLimit => "sync_rate_limited",
        RetryErrorKind::QuotaExceeded => "sync_quota_rejected",
        _ => return Err(KioError::schema("sync rejection kind is not definitive")),
    };
    terminal_transaction(
        ledger,
        &TerminalWrite {
            key,
            outcome: Outcome::SubmitRejected,
            billed: BilledAmount {
                usd: 0.0,
                estimated: false,
            },
            ledger_batch_job_id: intent_token,
            next_state: BatchState::Terminal,
            error: Some(error),
            increment_contract_violation: false,
            attempts_delta: 1,
            clear_intent_token: true,
            intent_token_guard: Some(intent_token),
            reseat_submission_seq: false,
        },
    )
    .map_err(pipeline_to_kio)
    .map(|_| ())
}

/// Record a post-send sync failure. Non-idempotent transport, persistence, and
/// response-contract failures are outcome-unknown: settle conservatively and
/// fence the task until `batch retry --resend-unknown ... --yes` authorizes one
/// fresh reservation. Explicit provider rejections are settled at zero and may
/// follow their normal backoff/auth recovery using a fresh reservation.
#[allow(clippy::too_many_arguments)]
fn record_sync_markdownize_failure(
    store: &TaskStore,
    ledger: &LedgerDb,
    key: &LedgerTaskKey,
    intent_token: &str,
    reserved_usd: f64,
    task_id: &str,
    task_attempts: u32,
    error: &TaskExecutionFailure,
    counts: &mut ExecOutcome,
) -> Result<()> {
    match error.retry_kind {
        RetryErrorKind::AuthError | RetryErrorKind::RateLimit | RetryErrorKind::QuotaExceeded => {
            settle_sync_definitive_rejection(ledger, key, intent_token, error.retry_kind)?;
            let policy = retry_policy(error.retry_kind);
            let attempts_after = task_attempts.saturating_add(1);
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
            let reason = retry_reason(error.retry_kind).to_owned();
            store
                .update_matching(|task| {
                    if task.task_id != task_id {
                        return false;
                    }
                    task.clear_reservation();
                    task.heartbeat_at = None;
                    task.hold_reason = None;
                    task.fallback_reason = Some(reason.clone());
                    match error.retry_kind {
                        RetryErrorKind::AuthError => {
                            task.status = TaskStatus::Paused;
                            task.hold_reason = Some(HoldReason::Auth);
                            task.next_retry_at = None;
                        }
                        RetryErrorKind::RateLimit => {
                            task.status = TaskStatus::Pending;
                            task.next_retry_at = Some(rate_limit_retry_at(
                                &ledger.control().now_utc_seconds(),
                                error.retry_after_ms,
                            ));
                        }
                        RetryErrorKind::QuotaExceeded => {
                            task.status = TaskStatus::Failed;
                            task.attempts = attempts_after;
                            task.next_retry_at = next_retry_at.clone();
                        }
                        _ => unreachable!("definitive sync rejection was matched above"),
                    }
                    true
                })
                .map_err(pipeline_to_kio)?;
            counts.failed += 1;
            match error.retry_kind {
                RetryErrorKind::AuthError => counts.auth_failed += 1,
                RetryErrorKind::RateLimit | RetryErrorKind::QuotaExceeded
                    if next_retry_at.is_some() || error.retry_kind == RetryErrorKind::RateLimit =>
                {
                    counts.failed_retryable += 1;
                }
                _ => {}
            }
        }
        _ => {
            settle_task_charge_unknown(ledger, key, intent_token, reserved_usd)?;
            mark_sync_result_unknown_task(store, task_id)?;
            counts.failed += 1;
        }
    }
    Ok(())
}

/// QA2/QA3 (step4b-contract-tests-p3a.md §A, 04 §5.2/§5.3): the shared
/// send-failure mapping — a 3-way branch on `error.retry_kind`. rate_limit
/// and auth_error each get a dedicated non-Failed landing state; every other
/// kind keeps the Failed/backoff logic. Extracted from the sync send site
/// verbatim so the Batch submit lane joins the SAME classification
/// (04 §5.8 送信途中の Adapter エラー).
///
/// `settle_on_terminal`: `true` for the sync lane (unchanged behavior — a
/// definitive failure settles `unknown_settled` at the reservation estimate);
/// `false` for the Batch lane, whose phases 2a/2b records must stay open as
/// the §5.8 crash window a later recovery (poll / `kio ledger reconcile` /
/// abandon) reconciles — settling here would clear `intent_token` while
/// provider residue (upload/job) may still exist.
#[allow(clippy::too_many_arguments)]
fn record_online_send_failure(
    store: &TaskStore,
    ledger: &LedgerDb,
    key: &LedgerTaskKey,
    intent_token: &str,
    reserved_usd: f64,
    task_id: &str,
    task_attempts: u32,
    error: &TaskExecutionFailure,
    counts: &mut ExecOutcome,
    settle_on_terminal: bool,
) -> Result<()> {
    match error.retry_kind {
        RetryErrorKind::RateLimit => {
            // QA3 (04 §5.2 L682-683): "paused ではなく pending +
            // next_retry_at" — stays Pending, never Failed. `attempts`
            // is NOT consumed (max_attempts=∞, 04 §5.3). CL42/CL44/CL45:
            // a retry is coming — leave the ledger row open (same
            // rationale as the settles_now=false case below); the
            // reservation stamp is left untouched.
            let next_retry_at =
                rate_limit_retry_at(&ledger.control().now_utc_seconds(), error.retry_after_ms);
            let reason = retry_reason(RetryErrorKind::RateLimit).to_owned();
            store
                .update_matching(|candidate| {
                    if candidate.task_id == task_id {
                        candidate.status = TaskStatus::Pending;
                        candidate.fallback_reason = Some(reason.clone());
                        candidate.hold_reason = None;
                        candidate.heartbeat_at = None;
                        candidate.next_retry_at = Some(next_retry_at.clone());
                        true
                    } else {
                        false
                    }
                })
                .map_err(pipeline_to_kio)?;
            // R9-7: a driven task that failed still attempted an online
            // send — count it so `batch` JSON surfaces the attempt.
            counts.failed += 1;
            // R11-2/04 §5.6 exit 3: retryable work remains.
            counts.failed_retryable += 1;
        }
        RetryErrorKind::AuthError => {
            // QA2 (04 §5.2 L679-683): lands Paused(hold_reason=auth),
            // never Failed. `attempts` is NOT consumed (a pause is not
            // a retry-budget event — max_attempts=0 means "no retry",
            // not "budget already spent"), so a later revival (`batch
            // resume`) does not find the budget exhausted. Ledger:
            // EXACTLY the pre-QA2 definitive-failure behavior (settle
            // `unknown_settled` + clear the reservation) — a later
            // revival reserves fresh. Batch lane: no settle — the open
            // row (and any 2a/2b residue) belongs to §5.8 recovery.
            if settle_on_terminal {
                settle_task_charge_unknown(ledger, key, intent_token, reserved_usd)?;
            }
            let reason = retry_reason(RetryErrorKind::AuthError).to_owned();
            store
                .update_matching(|candidate| {
                    if candidate.task_id == task_id {
                        candidate.status = TaskStatus::Paused;
                        candidate.hold_reason = Some(HoldReason::Auth);
                        candidate.fallback_reason = Some(reason.clone());
                        candidate.heartbeat_at = None;
                        candidate.next_retry_at = None;
                        candidate.clear_reservation();
                        true
                    } else {
                        false
                    }
                })
                .map_err(pipeline_to_kio)?;
            counts.failed += 1;
            // R11-2/04 §5.6 exit 5: auth_error needs user action.
            counts.auth_failed += 1;
        }
        _ => {
            let policy = retry_policy(error.retry_kind);
            let attempts_after = task_attempts.saturating_add(1);
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
            // CL42/CL44/CL45: a retry is coming — leave the ledger row open (it
            // already covers the resend, `reserve_or_reuse_task_charge`'s reuse
            // path) rather than settling now. Only a definitive outcome (no more
            // retries left) settles `unknown_settled` at the reservation estimate
            // — this Adapter integration has no post-hoc result-query capability
            // (see `settle_task_charge_unknown`'s doc comment), so a permanently
            // failed send is never billed less than its reservation. Batch lane:
            // never settles here (see `settle_on_terminal`'s doc).
            let settles_now = settle_on_terminal && next_retry_at.is_none();
            if settles_now {
                settle_task_charge_unknown(ledger, key, intent_token, reserved_usd)?;
            }
            let reason = retry_reason(error.retry_kind).to_owned();
            store
                .update_matching(|candidate| {
                    if candidate.task_id == task_id {
                        candidate.status = TaskStatus::Failed;
                        candidate.fallback_reason = Some(reason.clone());
                        // A Failed task is not a hold.
                        candidate.hold_reason = None;
                        candidate.heartbeat_at = None;
                        candidate.attempts = candidate.attempts.saturating_add(1);
                        candidate.next_retry_at = next_retry_at.clone();
                        if settles_now {
                            candidate.clear_reservation();
                        }
                        true
                    } else {
                        false
                    }
                })
                .map_err(pipeline_to_kio)?;
            // R9-7: a driven task that failed still attempted an online send
            // (charge consumed) — count it so `batch` JSON surfaces the
            // attempt instead of reporting only successes.
            counts.failed += 1;
            // R11-2: a scheduled `next_retry_at` marks the task retry-eligible
            // (exit 3), else it counts toward all-permanent (exit 4).
            if next_retry_at.is_some() {
                counts.failed_retryable += 1;
            }
        }
    }
    Ok(())
}

// R11-5: the per-batch `update_embedding_tasks` / `complete_` / `pause_` /
// `fail_embedding_tasks` helpers were removed — each did a full tasks.jsonl
// all()+replace_all, and calling them once per 32-chunk batch was the O(N²) hang.
// The enrichment loop now accumulates `EmbeddingTransition`s and writes them back
// once via `apply_embedding_transitions`.

pub(crate) fn retry_reason(kind: RetryErrorKind) -> &'static str {
    match kind {
        RetryErrorKind::NetworkError => "network_error",
        RetryErrorKind::RateLimit => "rate_limit",
        RetryErrorKind::AuthError => "auth_error",
        RetryErrorKind::LocalPeerAuth => "local_peer_auth",
        RetryErrorKind::LocalPeerConfig => "local_peer_config",
        RetryErrorKind::QuotaExceeded => "quota_exceeded",
        RetryErrorKind::InvalidInput => "invalid_input",
        RetryErrorKind::ContractViolation => "contract_violation",
        RetryErrorKind::ResultUnknown => RESULT_UNKNOWN_REASON,
        RetryErrorKind::BudgetExceeded => "budget_exceeded",
    }
}

pub(crate) fn retry_kind_from_reason(reason: Option<&str>) -> RetryErrorKind {
    match reason {
        Some("network_error") => RetryErrorKind::NetworkError,
        Some("rate_limit") => RetryErrorKind::RateLimit,
        Some("auth_error") => RetryErrorKind::AuthError,
        Some("local_peer_auth") => RetryErrorKind::LocalPeerAuth,
        Some("local_peer_config") => RetryErrorKind::LocalPeerConfig,
        Some("quota_exceeded") => RetryErrorKind::QuotaExceeded,
        Some("invalid_input") => RetryErrorKind::InvalidInput,
        Some(RESULT_UNKNOWN_REASON) => RetryErrorKind::ResultUnknown,
        // R19-3: a non-live retirement is non-retryable as the old task (like
        // invalid_input), but the enqueue guards treat it as non-blocking so the
        // content-addressed identity can be re-enqueued if it reappears.
        Some(RETIRED_NON_LIVE) => RetryErrorKind::InvalidInput,
        Some("budget_exceeded") => RetryErrorKind::BudgetExceeded,
        _ => RetryErrorKind::ContractViolation,
    }
}

pub(crate) fn task_retry_allowed(task: &TaskDescriptor) -> bool {
    let kind = retry_kind_from_reason(task.fallback_reason.as_deref());
    let policy = retry_policy(kind);
    policy.retryable
        && policy
            .max_attempts
            .map(|max| task.attempts < max)
            .unwrap_or(true)
}

/// R19-2: an online-send failure carrying an F8 reservation (a NON-billable phantom for
/// RateLimit/Quota/Auth, possibly-billed for NetworkError). A retire+reclaim supersede/sweep
/// must target these REGARDLESS of retry-attempt exhaustion: QuotaExceeded exhausts its
/// finite `max_attempts` (unlike RateLimit's unlimited), so gating on `task_retry_allowed`
/// left an exhausted-quota phantom reservation stuck for the month — never reclaimed,
/// starving the per-adapter cap and falsely pausing legitimate tasks. R20-3: AuthError is
/// included for the same reason — it is non-retryable (`max_attempts:0`) so `task_retry_allowed`
/// excluded it, leaving its (non-billable, 401/403) phantom orphaned; the sweep must retire it
/// so `reclaim_entry_for` can cancel the phantom. The reclaim itself stays error-kind-aware
/// inside `reclaim_entry_for` (RateLimit/Quota/Auth reclaim; NetworkError retires but keeps
/// its reservation as it may have billed). A `retired_non_live` task maps to InvalidInput
/// here, so it is excluded (idempotent).
pub(crate) fn is_reservation_bearing_send_failure(task: &TaskDescriptor) -> bool {
    matches!(
        retry_kind_from_reason(task.fallback_reason.as_deref()),
        RetryErrorKind::RateLimit
            | RetryErrorKind::QuotaExceeded
            | RetryErrorKind::NetworkError
            | RetryErrorKind::AuthError
    )
}

pub(crate) fn task_retry_due(task: &TaskDescriptor, control: &ControlHandle) -> bool {
    task.next_retry_at
        .as_deref()
        .map(|retry_at| retry_at <= control.now_utc_seconds().as_str())
        .unwrap_or(true)
}

/// Absolute time a failed task becomes eligible for retry: `now` plus the
/// backoff derived from its `RetryPolicy.backoff` descriptor (Step2c I2).
/// `attempts` is the post-increment attempt count (>= 1). Falls back to `now`
/// when the clock string cannot be parsed.
pub(crate) fn scheduled_retry_at(now: &str, backoff: &str, attempts: u32) -> String {
    let delay = retry_backoff_seconds(backoff, attempts);
    kio_core::scope::parse_utc_seconds(now)
        .map(|secs| kio_core::scope::format_utc_seconds(secs + delay))
        .unwrap_or_else(|| now.to_owned())
}

/// QA3 (step4b-contract-tests-p3a.md §A, 04 §5.3): `next_retry_at` for a
/// rate-limited send. `Some(ms)` honors the provider's `Retry-After` header
/// exactly — rounded up to whole seconds, floored at 1s so a sub-second
/// header value still schedules strictly in the future. `None` (header
/// absent or unparseable) falls back to the synthetic +2s schedule via
/// `scheduled_retry_at`'s `"retry_after"` backoff descriptor — with this
/// wiring in place, that descriptor's only remaining caller is this
/// fallback, so `retry_backoff_seconds`'s "no server header" comment on it
/// is now literally true rather than merely defensive. Falls back to `now`
/// itself when the clock string cannot be parsed (same posture as
/// `scheduled_retry_at`).
pub(crate) fn rate_limit_retry_at(now: &str, retry_after_ms: Option<u64>) -> String {
    let Some(ms) = retry_after_ms else {
        return scheduled_retry_at(now, "retry_after", 1);
    };
    let delay_seconds = i64::try_from(ms.div_ceil(1000).max(1)).unwrap_or(i64::MAX);
    kio_core::scope::parse_utc_seconds(now)
        .map(|secs| kio_core::scope::format_utc_seconds(secs.saturating_add(delay_seconds)))
        .unwrap_or_else(|| now.to_owned())
}

/// Backoff delay (seconds) for a failed task's next retry, derived from the
/// `RetryPolicy.backoff` descriptor in `crates/kio-pipeline/src/task.rs`.
/// The canonical contract intentionally uses deterministic scheduling without
/// jitter (see `docs/04-pipeline.md` §5.3). `attempts` is the post-increment
/// attempt count (>= 1).
fn retry_backoff_seconds(backoff: &str, attempts: u32) -> i64 {
    // exp(base=Ns,cap=Ms,...): min(base * 2^(attempts-1), cap).
    let exponential = |base: i64, cap: i64| {
        let exponent = attempts.saturating_sub(1).min(16);
        base.saturating_mul(1_i64 << exponent).clamp(0, cap)
    };
    if let Some(inner) = backoff
        .strip_prefix("exp(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let base = duration_secs_field(inner, "base").unwrap_or(2);
        let cap = duration_secs_field(inner, "cap").unwrap_or(60);
        return exponential(base, cap);
    }
    if let Some(inner) = backoff
        .strip_prefix("fixed(")
        .and_then(|s| s.strip_suffix(')'))
    {
        return parse_duration_secs(inner).unwrap_or(3_600);
    }
    // "retry_after": the local placeholder has no server `Retry-After` header,
    // so reuse the exponential schedule (base 2s, cap 60s).
    if backoff == "retry_after" {
        return exponential(2, 60);
    }
    0
}

/// Parse a compact duration such as `2s`, `30m`, or `1h` into seconds.
fn parse_duration_secs(text: &str) -> Option<i64> {
    let text = text.trim();
    let split = text.find(|ch: char| !ch.is_ascii_digit())?;
    let (digits, unit) = text.split_at(split);
    let value = digits.parse::<i64>().ok()?;
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        _ => return None,
    };
    Some(value * multiplier)
}

/// Extract a `key=<duration>` field (e.g. `base=2s`) from a comma-separated
/// backoff descriptor body.
fn duration_secs_field(inner: &str, key: &str) -> Option<i64> {
    inner.split(',').find_map(|part| {
        let rest = part.trim().strip_prefix(key)?.strip_prefix('=')?;
        parse_duration_secs(rest)
    })
}

pub(crate) fn estimate_online_markdownize_cost(
    size_bytes: u64,
    bbox_annotation_enabled: bool,
) -> f64 {
    let unannotated = estimate_local_baseline_cost(size_bytes) * 10.0;
    let annotated = if bbox_annotation_enabled {
        unannotated * 1.25
    } else {
        unannotated
    };
    // The realtime (sync) lane bills exactly double the Batch lane per page
    // ($4 vs $2 per 1,000 pages), so a `--realtime` run must reserve double —
    // otherwise the budget cap guards half the money actually being spent.
    match effective_invocation_lane() {
        PreferredRequestKind::Sync => annotated * 2.0,
        PreferredRequestKind::Batch => annotated,
    }
}

pub(crate) fn media_type_for_cli_path(path: &Path) -> &'static str {
    // R21-4: lowercase the extension so an uppercase-extension text-native file
    // (`README.MD`, `NOTE.TXT`, `MAIN.RS`) is recognized as text/markdown/plain/code and
    // handled locally — not folded to octet-stream and shipped to online OCR (R9-2).
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "md" | "markdown" => "text/markdown",
        "txt" => "text/plain",
        "rs" | "py" | "js" | "ts" | "go" | "java" | "c" | "h" | "cpp" => "text/x-code",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        // R20-6: recognize OOXML office documents by their real MIME so they are treated as
        // non-text-native (routed to online OCR), not folded into octet-stream and given a
        // raw-bytes local passthrough that evidences the ZIP bytes as searchable text.
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
}

/// `kio status`'s paused count broken down by the closed `hold_reason` enum.
/// A current task record always stamps one matching reason for a paused task,
/// so the three named buckets always appear (even at 0) and there is no
/// compatibility/unknown bucket.
pub(crate) fn paused_tasks_by_hold_reason(tasks: &[TaskDescriptor]) -> PausedTasksByHoldReason {
    let mut budget = 0u64;
    let mut auth = 0u64;
    let mut tier_b_approval = 0u64;
    let mut ledger_initialization_required = 0u64;
    for task in tasks {
        if task.status != TaskStatus::Paused {
            continue;
        }
        match task.hold_reason {
            Some(HoldReason::Budget) => budget += 1,
            Some(HoldReason::Auth) => auth += 1,
            Some(HoldReason::TierBApproval) => tier_b_approval += 1,
            Some(HoldReason::LedgerInitializationRequired) => ledger_initialization_required += 1,
            None => unreachable!("TaskStore accepts only stamped paused tasks"),
        }
    }
    PausedTasksByHoldReason {
        budget,
        auth,
        tier_b_approval,
        ledger_initialization_required,
    }
}

/// R9-2: whether `media_type` is text-native (Markdown / plain text / code) — the
/// deterministic Adapter's domain (docs/07 §2.1). Text-native files are
/// markdownized locally and must never enqueue an online Mistral-OCR task: docs/07
/// §5.2 scopes the standard online Adapter to *non*-text-native PDF / DOCX / PPTX /
/// images. Sending a text file to OCR shipped its raw bytes to a third-party API
/// (privacy) and billed ~10x the baseline for work the deterministic pass already
/// did (redundant, and orphaned by F6).
pub(crate) fn is_text_native_media(media_type: &str) -> bool {
    matches!(media_type, "text/markdown" | "text/plain" | "text/x-code")
}

pub(crate) fn budget_status(
    repo: &Repository,
    ledger: Option<&LedgerReadSnapshot>,
) -> Result<BudgetStatusResponse> {
    let caps = read_budget_policy(
        user_config_toml_path(repo.control()),
        repo.kio_dir().join("config.toml"),
    )
    .map_err(pipeline_to_kio)?;
    let now = repo.control().now_utc_seconds();
    let month = utc_month(&now);
    let scope_id = repo.scope_identity()?.scope_id;
    let Some(ledger) = ledger else {
        return Ok(BudgetStatusResponse {
            month,
            device_monthly_usd_cap: caps.device_monthly_usd_cap,
            folder_monthly_usd_cap: caps.folder_monthly_usd_cap,
            device_spent_usd: None,
            folder_spent_usd: None,
            device_remaining_usd: None,
            folder_remaining_usd: None,
            cap_kind: None,
            device_per_adapter: caps.device_per_adapter,
            hard_stop: caps.hard_stop,
            warn_at_percent: caps.warn_at_percent,
            warned: false,
            warning: None,
            ledger_status: Some("uninitialized".to_owned()),
        });
    };
    let ledger_path = ledger_db_path(repo.control());
    let device_spent = ledger
        .month_total(None, None, &month)
        .map_err(|error| ledger_snapshot_to_kio(&ledger_path.display().to_string(), error))?;
    let folder_spent = ledger
        .month_total(Some(&scope_id), None, &month)
        .map_err(|error| ledger_snapshot_to_kio(&ledger_path.display().to_string(), error))?;
    let device_remaining = caps.device_monthly_usd_cap - device_spent;
    let folder_remaining = caps.folder_monthly_usd_cap.map(|cap| cap - folder_spent);
    let cap_kind = match folder_remaining {
        Some(folder) if folder <= device_remaining => BudgetCapKind::Folder,
        _ => BudgetCapKind::Device,
    };
    // F5: surface the non-blocking `warn_at_percent` warning and the active
    // policy (hard_stop / warn_at_percent) alongside the numbers.
    let warning = budget_warning(&caps, device_spent, folder_spent);
    Ok(BudgetStatusResponse {
        month,
        device_monthly_usd_cap: caps.device_monthly_usd_cap,
        folder_monthly_usd_cap: caps.folder_monthly_usd_cap,
        device_spent_usd: Some(device_spent),
        folder_spent_usd: Some(folder_spent),
        device_remaining_usd: Some(device_remaining),
        folder_remaining_usd: folder_remaining,
        cap_kind: Some(cap_kind),
        device_per_adapter: caps.device_per_adapter,
        // QA11 (step4b-contract-tests-p3a.md §D): folder per_adapter does not
        // exist (04 §5.4 — device-layer only), so `kio status` must not
        // present a constraint that is not real.
        hard_stop: caps.hard_stop,
        warn_at_percent: caps.warn_at_percent,
        warned: warning.is_some(),
        warning,
        ledger_status: None,
    })
}

/// F5: the current scope's non-blocking budget warning (or `None`), for embedding
/// in `index` / `batch` result JSON. Reads the caps and this month's ledger totals.
pub(crate) fn scope_budget_warning(repo: &Repository) -> Result<Option<String>> {
    let caps = read_budget_policy(
        user_config_toml_path(repo.control()),
        repo.kio_dir().join("config.toml"),
    )
    .map_err(pipeline_to_kio)?;
    let path = ledger_db_path(repo.control());
    let (month, snapshot) = capture_search_budget_observation(&path, repo.control())?;
    let Some(snapshot) = snapshot else {
        return Ok(None);
    };
    let scope_id = repo.scope_identity()?.scope_id;
    let device_spent = snapshot
        .month_total(None, None, &month)
        .map_err(|error| ledger_snapshot_to_kio(&path.display().to_string(), error))?;
    let folder_spent = snapshot
        .month_total(Some(&scope_id), None, &month)
        .map_err(|error| ledger_snapshot_to_kio(&path.display().to_string(), error))?;
    Ok(budget_warning(&caps, device_spent, folder_spent))
}

/// Read-only enqueue classification is distinct from mutable billing authority.
#[derive(Clone, Copy)]
pub(crate) enum EnqueueBudget {
    Uninitialized,
    Available { device: f64, folder: Option<f64> },
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn enqueue_online_placeholder_task(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    task_store: &TaskStore,
    candidate: &ScanCandidate,
    raw_hash: &str,
    scope_id: &str,
    network_allowed: bool,
    args: &IndexArgs,
    created_at: &str,
    result: &mut IndexPipelineResult,
    ledger: Option<&LedgerDb>,
    enqueue_budget: EnqueueBudget,
    budget_caps: &BudgetCaps,
    unsupported_store: &UnsupportedInputStore,
    unsupported_by_path: &mut BTreeMap<String, UnsupportedInputDisposition>,
    office_conversion_unavailable_paths: &mut BTreeSet<String>,
) -> Result<()> {
    // R9-2: text-native files (Markdown / plain text / code) are fully handled by
    // the deterministic Adapter (07 §2.1) and must never enqueue an online OCR task
    // (07 §5.2 scopes Mistral OCR to non-text-native PDF/DOCX/PPTX/images). Gate at
    // enqueue so a routine `index` never creates a redundant, privacy-leaking,
    // billed task for a `.md` / `.txt` / code file.
    if is_text_native_media(&candidate.media_type) {
        return Ok(());
    }
    // 07 §5.1: an Office (DOCX/PPTX) file whose converter is not resolvable
    // right now must NEVER get a doomed online task -- there is no converted
    // PDF to send. Record the skip (idempotent: `record_unsupported_if_changed`
    // no-ops when the same disposition is already the latest row for this
    // path) so `compute_index_status`'s `office_conversion_unavailable`
    // discloses WHY this file has no enrichment task, instead of a silent gap.
    // Once a converter resolves (a later `kio index`), fall through to the
    // normal enqueue path below and record the transition back to resolved.
    if is_office_media(&candidate.media_type) {
        if resolve_office_converter_with_control(repo.control()).is_none() {
            record_unsupported_if_changed(
                unsupported_store,
                unsupported_by_path,
                UnsupportedInputDisposition {
                    path: candidate.input_path.clone(),
                    raw_hash: raw_hash.to_owned(),
                    media_type: candidate.media_type.clone(),
                    size_bytes: candidate.size_bytes,
                    reason: OFFICE_CONVERSION_UNAVAILABLE_REASON.to_owned(),
                },
            )?;
            office_conversion_unavailable_paths.insert(candidate.input_path.clone());
            return Ok(());
        }
        if office_conversion_unavailable_paths.remove(&candidate.input_path) {
            record_unsupported_if_changed(
                unsupported_store,
                unsupported_by_path,
                UnsupportedInputDisposition {
                    path: candidate.input_path.clone(),
                    raw_hash: raw_hash.to_owned(),
                    media_type: candidate.media_type.clone(),
                    size_bytes: candidate.size_bytes,
                    reason: UNSUPPORTED_REASON_RESOLVED.to_owned(),
                },
            )?;
        }
    }
    let online_profile = online_markdownize_profile_for(repo)?;
    // Stage 3: a declared `offline_api` pipeline owns this enrichment instead
    // of the cloud OCR. Resolved once, here, so the task's `output_ref` and the
    // status decision below cannot disagree about which route it is on.
    let local_ocr = active_local_ocr_execution_with_control(adapter_settings, repo.control())
        .filter(|_| local_ocr_handles_media_type(&candidate.media_type));
    let (output_ref, secrets_approved) = match local_ocr {
        Some(execution) => {
            let profile = kio_adapter::local_ocr_markdownize::profile_for(execution);
            let approved = secrets_send_approved(
                adapter_settings,
                repo,
                approvals::AdapterRole::Markdown,
                &profile.adapter_id,
                &profile.tool_profile_hash,
            );
            (offline_output_ref(&profile.adapter_id), approved)
        }
        None => (
            online_output_ref(&online_profile.adapter_id),
            secrets_send_approved(
                adapter_settings,
                repo,
                approvals::AdapterRole::Markdown,
                &online_profile.adapter_id,
                &online_profile.tool_profile_hash,
            ),
        ),
    };
    let secrets_hold = !secrets_approved && classify_secret(&candidate.input_path).is_some();
    let effective_bbox_policy = bbox_annotation_enabled(repo)?;
    // R15-2: supersede any stale online markdownize task for THIS path whose
    // `input_hash` differs from the current content. The file was edited after that
    // task was enqueued, so it is stale: `batch resume` would R14-2-supersede a
    // Pending/Paused one at send time (adapter never called) but only AFTER the
    // pre-send reservation already landed, and left unretired these stale tasks
    // accumulate and eat the per-adapter markdownize cap, falsely pausing the
    // current (valid) task. Retire them here (non-retryable `invalid_input`; the
    // state machine has no "superseded" state and the recovery is this fresh
    // re-index, not a retry). Runs before the idempotency check so it fires even
    // when a same-hash task also exists.
    //
    // R17-3 (still applicable under the SQLite ledger): extend the retirement to a
    // RETRYABLE `Failed` task (gated by `task_retry_allowed`, so a
    // permanently-failed task is left alone) — a Failed(rate_limit/…) task keeps
    // its ledger row open across retries (`reserve_or_reuse_task_charge`'s reuse
    // path), so it must be explicitly released here or the stale row lingers,
    // eating the per-adapter markdownize cap. Released via
    // `release_task_charge_if_open` (settles `unknown_settled` at the reservation
    // estimate if a row is still open; a no-op otherwise) — under the same
    // `reservation_scope_id` the send path reserves with
    // (`repo.scope_identity()?.scope_id`), so the release nets out at the
    // folder-scoped totals too, not just the device total.
    let markdownize_adapter_kind = "markdownize";
    let reservation_scope_id = repo.scope_identity()?.scope_id;
    let mut stale_ids = BTreeSet::new();
    for task in task_store.all().map_err(pipeline_to_kio)? {
        let stale = task.task_type == TaskType::Markdownize
            && targets_standard_online_markdownize(task_store.kio_dir(), &task, &output_ref)
            && task.input_path == candidate.input_path
            && task.input_hash != raw_hash
            && (matches!(task.status, TaskStatus::Pending | TaskStatus::Paused)
                || (task.status == TaskStatus::Failed
                    && is_reservation_bearing_send_failure(&task)));
        if !stale {
            continue;
        }
        let key = task_ledger_key(
            &reservation_scope_id,
            markdownize_adapter_kind,
            &task.input_hash,
            &online_profile.tool_profile_hash,
        );
        if let Some(ledger) = ledger {
            release_task_charge_if_open(ledger, &key)?;
        }
        stale_ids.insert(task.task_id);
    }
    task_store
        .update_matching(|task| {
            if !stale_ids.contains(&task.task_id) {
                return false;
            }
            retire_online_task(task);
            true
        })
        .map_err(pipeline_to_kio)?;
    // Idempotency (Step2c I1, 04 §5.5): never enqueue a second online task for an
    // identity `(input_path, input_hash)` that already has an online task in any
    // live lifecycle state. Without the completed-state check, every unchanged
    // re-index appended a fresh task, and a later `batch resume` re-sent it to
    // the API and double-charged the ledger. A changed file produces a new
    // `input_hash`, so it still enqueues.
    //
    // The online task is identified differently across its lifecycle:
    //   * Pending/Paused (not yet executed) still carries the placeholder
    //     `output_ref`.
    //   * Done/Partial (executed) has had its `output_ref` rewritten to the
    //     normalized-instance path, but `execute_online_markdownize_task`
    //     stamps `fallback_reason = "online_adapter_done"`.
    //   * Failed/Running are also deduped: re-enqueueing a fresh Pending task
    //     (next_retry_at = None) would bypass the retry backoff gate and cause
    //     an extra API call. Failed tasks are owned by `kio batch retry`, which
    //     honors `next_retry_at` and the per-error retry budget.
    if let Some(existing) = task_store
        .all()
        .map_err(pipeline_to_kio)?
        .into_iter()
        .find(|task| {
            if task.input_path != candidate.input_path || task.input_hash != raw_hash {
                return false;
            }
            if task.bbox_annotation_enabled != Some(effective_bbox_policy) {
                return false;
            }
            match task.status {
                TaskStatus::Pending | TaskStatus::Paused => {
                    targets_standard_online_markdownize(task_store.kio_dir(), task, &output_ref)
                }
                TaskStatus::Done | TaskStatus::Partial => {
                    task.fallback_reason.as_deref() == Some("online_adapter_done")
                }
                // R19-3: a `retired_non_live` Failed task must NOT block a fresh enqueue
                // when the (path,hash) identity reappears (revert/restore). Other Failed
                // tasks stay deduped (owned by `batch retry`, which honors backoff).
                TaskStatus::Failed => {
                    targets_standard_online_markdownize(task_store.kio_dir(), task, &output_ref)
                        && task.fallback_reason.as_deref() != Some(RETIRED_NON_LIVE)
                }
                TaskStatus::Running => {
                    targets_standard_online_markdownize(task_store.kio_dir(), task, &output_ref)
                }
            }
        })
    {
        match existing.status {
            TaskStatus::Paused => result.paused_tasks += 1,
            TaskStatus::Pending => result.pending_online_tasks += 1,
            _ => {}
        }
        return Ok(());
    }

    let estimate = BudgetEstimate {
        scope_id: scope_id.to_owned(),
        task_type: TaskType::Markdownize,
        estimated_usd: estimate_online_markdownize_cost(
            candidate.size_bytes,
            effective_bbox_policy,
        ),
        adapter_id: Some(online_profile.adapter_id.clone()),
    };
    let budget = match enqueue_budget {
        EnqueueBudget::Available { device, folder } => {
            evaluate_budget_with_caps(&estimate, device, folder, matches_batch_override(args))
        }
        EnqueueBudget::Uninitialized => {
            evaluate_budget_with_caps(&estimate, f64::NEG_INFINITY, Some(f64::NEG_INFINITY), false)
        }
    };
    // R10-7: an online markdownize task can only ever be DRIVEN by `batch`, whose
    // gate is the persistent per-adapter opt-in (`persistent_network_allowed`). A
    // one-shot `--online --yes` sets per-invocation `network_allowed = true` but
    // leaves no persistent opt-in, so the task can never be sent — a silent dead-end.
    // Report the honest state (`network_opt_in_required`) rather than a false
    // `ready_for_online_adapter`. (Inline `--yes` sending, to match embedding, is
    // deferred with the F6 promotion wiring.)
    let markdownize_drivable =
        network_allowed && persistent_network_allowed(adapter_settings, repo)?;
    // N1a: a Tier B (candidate-secret) file without `--send-secrets` is held here
    // — a Paused task with `secrets_tier_b_hold`, visible in `kio status`, that
    // `batch resume` will not un-hold and `execute_pending_markdownize_tasks` will
    // not send. The hold takes precedence over budget/network reasons.
    let (status, reason) = if local_ocr.is_some() {
        offline_markdownize_enqueue_status(secrets_hold)
    } else if secrets_hold {
        (TaskStatus::Paused, Some(SECRETS_TIER_B_HOLD))
    } else if matches!(enqueue_budget, EnqueueBudget::Uninitialized) {
        (
            TaskStatus::Paused,
            Some(LEDGER_INITIALIZATION_REQUIRED_REASON),
        )
    } else if !budget.allowed && budget_caps.hard_stop {
        // F5: only a hard-stop cap pauses the task. Under soft-stop the online task
        // is enqueued normally and its over-cap charge is recorded at execution.
        (TaskStatus::Paused, Some("budget_exceeded"))
    } else if !markdownize_drivable {
        (TaskStatus::Pending, Some("network_opt_in_required"))
    } else {
        (TaskStatus::Pending, Some("ready_for_online_adapter"))
    };
    let mut task = task_descriptor(
        repo,
        TaskType::Markdownize,
        Some(MarkdownizeMode::Full),
        candidate,
        raw_hash,
        &output_ref,
        status,
        reason,
        created_at,
    );
    task.bbox_annotation_enabled = Some(effective_bbox_policy);
    task_store.append(&task).map_err(pipeline_to_kio)?;
    match status {
        TaskStatus::Paused => result.paused_tasks += 1,
        TaskStatus::Pending => result.pending_online_tasks += 1,
        _ => {}
    }
    Ok(())
}

fn matches_batch_override(_args: &IndexArgs) -> bool {
    false
}

/// This month's remaining `(device, folder)` budget for the `(scope, adapter)`
/// filter, shared by the enqueue-time pre-check
/// (`enqueue_online_placeholder_task`/`enqueue_embedding_tasks`'s siblings), the
/// send-time check-then-reserve (`reserve_or_reuse_task_charge`, which
/// independently re-checks atomically inside its own `BEGIN IMMEDIATE` Tx — this
/// function is for the READ-ONLY enqueue-time estimate, not a substitute for
/// that atomicity), and the status/warning reports (`budget_status` /
/// `scope_budget_warning`). `ledger_month_total` (CL59) already sums confirmed
/// `cost_ledger` charges plus unterminated `batch_requests` reservations — the
/// retired JSONL design's separate reclaim-ledger netting
/// (`net_monthly_spent`/R17-3/R18-3) has no equivalent here: this ledger never
/// records a charge until a definitive terminal Tx, so there is nothing to
/// reclaim after the fact (see the implementation report).
pub(crate) fn budget_remaining_for_adapter(
    ledger: &LedgerDb,
    budget_caps: &BudgetCaps,
    scope_id: &str,
    adapter_kind: &str,
) -> Result<(f64, Option<f64>)> {
    let month = utc_month(&ledger.control().now_utc_seconds());
    let conn = &ledger;
    let device_spent = ledger_month_total(conn, None, None, &month).map_err(pipeline_to_kio)?;
    let folder_spent =
        ledger_month_total(conn, Some(scope_id), None, &month).map_err(pipeline_to_kio)?;
    let device_adapter_spent =
        ledger_month_total(conn, None, Some(adapter_kind), &month).map_err(pipeline_to_kio)?;
    Ok(budget_remaining_from_totals(
        budget_caps,
        adapter_kind,
        device_spent,
        folder_spent,
        device_adapter_spent,
    ))
}

pub(crate) fn budget_remaining_from_totals(
    budget_caps: &BudgetCaps,
    adapter_kind: &str,
    device_spent: f64,
    folder_spent: f64,
    device_adapter_spent: f64,
) -> (f64, Option<f64>) {
    let mut device_remaining = budget_caps.device_monthly_usd_cap - device_spent;
    if let Some(adapter_cap) = budget_caps.device_per_adapter.get(adapter_kind) {
        device_remaining = device_remaining.min(adapter_cap - device_adapter_spent);
    }
    // QA11 (step4b-contract-tests-p3a.md §D): folder per_adapter does not
    // exist (04 §5.4 — "folder cap は total のみ"); this estimate must not
    // narrow `folder_remaining` for a constraint the Tx-atomic gate
    // (`check_then_reserve`/`BudgetCapConfig`) never applies either — a real
    // 2-condition gate (device cap, folder total cap) staying consistent
    // with a 3-condition estimate silently added a THIRD gate here that
    // could pause a task the atomic check would have allowed.
    let folder_remaining = budget_caps
        .folder_monthly_usd_cap
        .map(|cap| cap - folder_spent);
    (device_remaining, folder_remaining)
}
