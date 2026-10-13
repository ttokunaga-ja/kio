use crate::batch::command::run_ledger_reconcile;
use crate::commands::LedgerArgs;
use crate::commands::LedgerCommand;
use crate::invocation::Invocation;
use crate::object_persistence::pipeline_to_kio;
use crate::response::ledger::*;
use crate::response::{CommandResponse, Completion, ResponseBody, project_legacy};
use crate::scope_access::registry_duplicate_guard;
use crate::settings::data_home;
use kio_adapter::tool_lock::AdapterRuntimeSettings;
use kio_adapter::types::AdapterProfile;
use kio_core::ExitCode;
use kio_core::KioError;
use kio_core::Result;
use kio_core::control::ControlHandle;
use kio_pipeline::budget::BudgetCaps;
use kio_pipeline::budget::utc_month;
use kio_pipeline::ledger::BatchRequestRow;
use kio_pipeline::ledger::BatchState;
use kio_pipeline::ledger::LedgerDb;
use kio_pipeline::ledger::LedgerReadSnapshot;
use kio_pipeline::ledger::LedgerSnapshotError;
use kio_pipeline::ledger::Outcome;
use kio_pipeline::ledger::RequestKind;
use kio_pipeline::ledger::TaskKey as LedgerTaskKey;
use kio_pipeline::ledger::ops::BilledAmount;
use kio_pipeline::ledger::ops::BudgetCapConfig;
use kio_pipeline::ledger::ops::TerminalWrite;
use kio_pipeline::ledger::ops::get_batch_request;
use kio_pipeline::ledger::ops::recovery_settle_unknown;
use kio_pipeline::ledger::ops::sync_recovery_candidates;
use kio_pipeline::ledger::ops::terminal_transaction;
use kio_pipeline::task::RESULT_UNKNOWN_REASON;
use kio_pipeline::task::TaskStatus;
use kio_pipeline::task::TaskStore;
use serde_json::Value;
use serde_json::json;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

pub(crate) fn run_ledger(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    args: LedgerArgs,
    control: &ControlHandle,
) -> Result<Value> {
    match args.command {
        Some(LedgerCommand::Reconcile(_)) => run_ledger_reconcile(adapter_settings, control),
        command => run_ledger_lifecycle(LedgerArgs { command }, control),
    }
}

pub(crate) fn run_ledger_lifecycle(args: LedgerArgs, control: &ControlHandle) -> Result<Value> {
    match args.command {
        Some(LedgerCommand::Init { resume }) => {
            let path = ledger_db_path(control);
            let _ledger = if resume {
                LedgerDb::resume_initialization_with_control(&path, control.clone())
            } else {
                LedgerDb::initialize_with_control(&path, control.clone())
            }
            .map_err(pipeline_to_kio)?;
            project_legacy(CommandResponse {
                body: ResponseBody::Ledger(LedgerResponse::Initialized(LedgerPathResponse {
                    status: "initialized".to_owned(),
                    path,
                })),
                completion: Completion::Complete,
            })
        }
        Some(LedgerCommand::Status) => {
            match open_search_budget_snapshot(&ledger_db_path(control), control) {
                Ok(Some(_ledger)) => project_legacy(CommandResponse {
                    body: ResponseBody::Ledger(LedgerResponse::Status(LedgerPathResponse {
                        status: "ready".to_owned(),
                        path: ledger_db_path(control),
                    })),
                    completion: Completion::Complete,
                }),
                Ok(None) => project_legacy(CommandResponse {
                    body: ResponseBody::Ledger(LedgerResponse::Status(LedgerPathResponse {
                        status: "uninitialized".to_owned(),
                        path: ledger_db_path(control),
                    })),
                    completion: Completion::Complete,
                }),
                Err(error) => Err(error),
            }
        }
        Some(LedgerCommand::Backup(args)) => {
            if !args.to.is_absolute() {
                return Err(KioError::invalid_usage(
                    "ledger backup --to must be an absolute directory",
                ));
            }
            kio_core::private_fs::verify_private_directory_with_control(&args.to, control)?;
            let ledger = open_ledger_db(control)?;
            let backup = ledger.backup().map_err(pipeline_to_kio)?;
            backup
                .publish_create_only_with_control(&args.to, control)
                .map_err(pipeline_to_kio)?;
            project_legacy(CommandResponse {
                body: ResponseBody::Ledger(LedgerResponse::Backup(LedgerPathResponse {
                    status: "backed_up".to_owned(),
                    path: args.to,
                })),
                completion: Completion::Complete,
            })
        }
        Some(LedgerCommand::Restore(args)) => {
            if !args.from.is_absolute() {
                return Err(KioError::invalid_usage(
                    "ledger restore --from must be an absolute directory",
                ));
            }
            let path = ledger_db_path(control);
            let backup =
                kio_pipeline::ledger::LedgerBackup::load_with_control(&args.from, control.clone())
                    .map_err(pipeline_to_kio)?;
            let _ledger =
                LedgerDb::restore_missing_database_with_control(&path, &backup, control.clone())
                    .map_err(pipeline_to_kio)?;
            project_legacy(CommandResponse {
                body: ResponseBody::Ledger(LedgerResponse::Restore(LedgerPathResponse {
                    status: "restored".to_owned(),
                    path,
                })),
                completion: Completion::Complete,
            })
        }
        Some(LedgerCommand::Recover) => {
            let path = ledger_db_path(control);
            let recovery = LedgerDb::recover_pending_write_with_control(&path, control.clone())
                .map_err(pipeline_to_kio)?;
            project_legacy(CommandResponse {
                body: ResponseBody::Ledger(LedgerResponse::Recover(LedgerRecoverResponse {
                    status: "ready".to_owned(),
                    recovery,
                    path,
                })),
                completion: Completion::Complete,
            })
        }
        None => Err(KioError::invalid_usage(
            "ledger requires init, status, backup, restore, recover, or reconcile",
        )),
        Some(LedgerCommand::Reconcile(_)) => unreachable!("reconcile requires runtime settings"),
    }
}

/// `$XDG_DATA_HOME/kio/cost-ledger.sqlite` (04 §5.4 / CL70) — the device-global
/// ledger, sole store for every reservation/charge this CLI records.
pub(crate) fn ledger_db_path(control: &ControlHandle) -> PathBuf {
    data_home(control).join("kio/cost-ledger.sqlite")
}

/// Capture the device ledger once for a search response's budget-status
/// observation. A completely absent ledger and authority set is the one normal
/// uninitialized state; spending is unknown. Every unsafe or unstable state is a
/// command-level failure and must not be reported as `budget_paused=false`.
pub(crate) fn open_search_budget_snapshot(
    path: &Path,
    control: &ControlHandle,
) -> Result<Option<LedgerReadSnapshot>> {
    match LedgerReadSnapshot::open_with_control(path, control.clone()) {
        Ok(snapshot) => Ok(Some(snapshot)),
        Err(LedgerSnapshotError::Missing) => Ok(None),
        Err(error) => Err(pipeline_to_kio(
            kio_pipeline::PipelineError::ledger_snapshot(path.display().to_string(), error),
        )),
    }
}

/// Capture the ledger snapshot and the month it is allowed to describe as one
/// observation. A rollover while opening a snapshot would otherwise pair a
/// September clock read with an October snapshot (or the reverse), so retry the
/// whole observation once. Two consecutive rollover crossings are surfaced as
/// retryable partial work rather than looping around a changing wall clock.
pub(crate) fn capture_search_budget_observation(
    path: &Path,
    control: &ControlHandle,
) -> Result<(String, Option<LedgerReadSnapshot>)> {
    capture_search_budget_observation_with(path, || control.now_utc_seconds(), control)
}

pub(crate) fn capture_search_budget_observation_with<F>(
    path: &Path,
    mut clock: F,
    control: &ControlHandle,
) -> Result<(String, Option<LedgerReadSnapshot>)>
where
    F: FnMut() -> String,
{
    const MAX_ATTEMPTS: usize = 2;
    for _ in 0..MAX_ATTEMPTS {
        let before_month = utc_month(&clock());
        let snapshot = open_search_budget_snapshot(path, control)?;
        let after_month = utc_month(&clock());
        if before_month == after_month {
            return Ok((before_month, snapshot));
        }
    }
    Err(KioError::new(
        "KIO-E-LEDGER-SNAPSHOT-ROLLOVER-001",
        "cost ledger month changed while capturing a budget snapshot; retry the command",
        json!({ "ledger_path": path, "reason": "month_rollover" }),
        ExitCode::PartialFailure,
    ))
}

pub(crate) fn ledger_snapshot_to_kio(path: &str, error: LedgerSnapshotError) -> KioError {
    match error {
        // A successfully-owned snapshot cannot become Missing while querying.
        // Treat that impossible transition as integrity failure rather than
        // silently switching to the no-ledger/zero-total outcome.
        LedgerSnapshotError::Missing => KioError::new(
            "KIO-E-LEDGER-SNAPSHOT-UNSAFE-001",
            "cost ledger snapshot disappeared after it was captured",
            json!({
                "ledger_path": path,
                "reason": "unsafe_integrity",
                "detail": "owned ledger snapshot reported Missing after capture",
            }),
            ExitCode::PermanentFailure,
        ),
        LedgerSnapshotError::UnsafeIntegrity(detail) => KioError::new(
            "KIO-E-LEDGER-SNAPSHOT-UNSAFE-001",
            "cost ledger could not be read without violating integrity safeguards",
            json!({
                "ledger_path": path,
                "reason": "unsafe_integrity",
                "detail": detail,
            }),
            ExitCode::PermanentFailure,
        ),
        LedgerSnapshotError::UnstableBusy(detail) => KioError::new(
            "KIO-E-LEDGER-SNAPSHOT-001",
            "cost ledger could not be read from a stable snapshot; retry the command",
            json!({
                "ledger_path": path,
                "reason": "unstable_or_busy",
                "detail": detail,
            }),
            ExitCode::PartialFailure,
        ),
    }
}

/// Open the device-global `cost-ledger.sqlite`. Every ledger-touching command (`kio status`,
/// `kio batch *`, `kio index`, `kio search --vector`/`--hybrid` page 1, `kio
/// purge`) opens the ledger through this one function, so every command uses
/// the current SQLite store. `LedgerDb::open` fails closed
/// if retired JSONL ledger files are present, leaving their bytes untouched.
pub(crate) fn open_ledger_db(control: &ControlHandle) -> Result<LedgerDb> {
    LedgerDb::open_existing_with_control(ledger_db_path(control), control.clone())
        .map_err(pipeline_to_kio)
}

/// Absence is optional only for callers that do not authorize paid work.
/// Partial, replaced or corrupt state always keeps its original error.
pub(crate) fn open_optional_ledger_db(control: &ControlHandle) -> Result<Option<LedgerDb>> {
    match LedgerDb::open_existing_with_control(ledger_db_path(control), control.clone()) {
        Ok(ledger) => Ok(Some(ledger)),
        Err(kio_pipeline::PipelineError::Contract {
            code: "KIO-E-LEDGER-UNINITIALIZED-001",
            ..
        }) => Ok(None),
        Err(error) => Err(pipeline_to_kio(error)),
    }
}

const SYNC_RESULT_UNKNOWN_LEDGER_ERROR: &str = "unknown_settled";
pub(crate) const SYNC_RESEND_AUTHORIZED_REASON: &str = "unknown_resend_authorized";

/// Mark exactly one task terminal after a durable sync unknown-outcome fence.
/// The ledger remains the crash-safe authority: if this JSONL update is
/// interrupted, `sync_result_unknown_fenced` repeats it before any send.
pub(crate) fn mark_sync_result_unknown_task(store: &TaskStore, task_id: &str) -> Result<bool> {
    store
        .update_matching(|task| {
            if task.task_id != task_id {
                return false;
            }
            task.status = TaskStatus::Failed;
            task.fallback_reason = Some(RESULT_UNKNOWN_REASON.to_owned());
            task.hold_reason = None;
            task.heartbeat_at = None;
            task.next_retry_at = None;
            task.attempts = task.attempts.saturating_add(1);
            task.clear_reservation();
            true
        })
        .map_err(pipeline_to_kio)
        .map(|changed| changed > 0)
}

/// A terminal sync `unknown_settled` row is a durable no-resend fence. The
/// explicit recovery command changes this exact error to its one-shot
/// authorization sentinel; every other terminal result follows its ordinary
/// lifecycle.
pub(crate) fn sync_result_unknown_fenced(row: Option<&BatchRequestRow>) -> bool {
    row.is_some_and(|row| {
        row.request_kind == RequestKind::Sync
            && row.state.is_terminal()
            && row.error.as_deref() == Some(SYNC_RESULT_UNKNOWN_LEDGER_ERROR)
    })
}

/// CL45/§5.4 sync crash recovery — the write-command-entry pass 04 §5.8's
/// batch recovery (CL32) runs alongside, but explicitly excludes
/// `request_kind='sync'` rows from (item 5 of this session's task: the
/// ledger-side settlement primitive, `recovery_settle_unknown`, is already
/// implemented and already reused for exactly this purpose by
/// `settle_task_charge_unknown` — this function is the write-entry caller
/// that was still missing).
///
/// No Adapter in this codebase exposes a post-hoc "query a past sync call's
/// result by provider request id" capability (confirmed by grep of
/// `kio-adapter`'s traits — the same fact `settle_task_charge_unknown`'s own
/// doc comment already records), so CL45's "batch_job_id 記録済みで照会可能"
/// branch is unreachable today: every stale sync row this finds settles via
/// the "未記録・照会不能" branch — `estimated_usd` billed with `estimated=1`,
/// `state=3`, `intent_token` cleared in the same Tx (CL47: sync rows have no
/// upload/job residue to wait on, unlike batch rows).
///
/// Scoped to `scope_id` (a sync row belongs to one scope; only that scope's
/// `.kio/.lock` holder reconciles it) and gated on `stale_after_at`
/// (`sync_recovery_candidates`'s own doc comment) so a genuinely live
/// concurrent sync call from another process is never raced.
pub(crate) fn recover_stale_sync_rows(
    ledger: &LedgerDb,
    store: Option<&TaskStore>,
    scope_id: &str,
) -> Result<u64> {
    let now_ms = kio_pipeline::ledger::time::now_millis();
    let candidates = sync_recovery_candidates(ledger, scope_id, now_ms).map_err(pipeline_to_kio)?;
    let mut settled = 0_u64;
    for row in candidates {
        let Some(intent_token) = row.intent_token.as_deref() else {
            // A state 0/1 row always carries an intent_token by construction
            // (`phase1_intent` always sets one on the same INSERT/UPDATE that
            // sets state=0); skip defensively rather than panic if this
            // invariant is ever violated.
            continue;
        };
        recovery_settle_unknown(ledger, &row.key, intent_token, row.estimated_usd, true)
            .map_err(pipeline_to_kio)?;
        // `reservation_id` is the precise task-side binding to this intent.
        // If a crash follows settlement, the durable row fence above prevents
        // a reclaimed Pending task from being re-sent before this update replays.
        if let Some(store) = store {
            store
                .update_matching(|task| {
                    if task.reservation_id.as_deref() != Some(intent_token) {
                        return false;
                    }
                    task.status = TaskStatus::Failed;
                    task.fallback_reason = Some(RESULT_UNKNOWN_REASON.to_owned());
                    task.hold_reason = None;
                    task.heartbeat_at = None;
                    task.next_retry_at = None;
                    task.attempts = task.attempts.saturating_add(1);
                    task.clear_reservation();
                    true
                })
                .map_err(pipeline_to_kio)?;
        }
        settled += 1;
    }
    Ok(settled)
}

/// R19-3: terminal `fallback_reason` for a task retired because its content-addressed
/// target (chunk_id or (path,hash)) is currently NON-LIVE. Unlike `invalid_input`
/// (a permanent, genuinely-bad-input failure), this retirement is REVERSIBLE: the
/// identity can reappear (git revert / undo / restore of the exact bytes), so the
/// `enqueue_*` idempotency guards treat it as non-blocking and let a fresh task be
/// created. It is still non-retryable AS THE OLD TASK (`retry_kind_from_reason` maps it
/// to InvalidInput), so it is never re-driven and never counts as pending work.
pub(crate) const RETIRED_NON_LIVE: &str = "retired_non_live";

// ---------------------------------------------------------------------------
// cost-ledger.sqlite task-charge helpers (04-pipeline.md §5.4's sync degenerate
// 2-phase — CL41-47/CL56-61 in tasks/step4b-contract-tests-ledger.md). Replaces
// the retired JSONL `budget::CostLedger`/`ReservationLedger` F8 reservation flow
// (2026-07-21) — see the implementation report for the behavior changes this
// entails (a "markdownize"/"embedding" TASK's online adapter call is now itself
// one `batch_requests` sync row per §G, instead of a JSONL charge plus a
// side-channel reservation-lifecycle ledger).
// ---------------------------------------------------------------------------

/// Effective `timeout_seconds` this session applies to a markdownize/embedding
/// TASK sync row's `stale_after_at` (CL49's formula, 07-adapter-spec.md §7's
/// documented `[adapter.policy]` default). This implementation's device
/// query-embedding row (§H, `compute_query_embedding`) resolves the effective
/// timeout per §M note-2's config-layer rule; TASK rows use this fixed default
/// instead — the same "conservative fixed constant, not a live adapter-config
/// re-read" posture `estimate_online_markdownize_cost`/`estimate_embedding_cost`
/// already use for the *amount* side of the charge. Flagged in the
/// implementation report as unfinished per-scope timeout resolution for task rows.
pub(crate) const TASK_SYNC_EFFECTIVE_TIMEOUT_SECONDS: i64 = 300;

/// One markdownize/embedding TASK's `cost-ledger.sqlite` row identity — every
/// online adapter call this module drives (as opposed to the device
/// query-embedding row, §H) is a `request_kind='sync'` row under this key (04
/// §5.4 L768 / CL42). `adapter_kind` must be one of
/// `kio_pipeline::ledger::ops::PER_ADAPTER_KIND_ENUM` (CL61) — this module uses
/// the literals `"markdownize"` / `EMBEDDING_ADAPTER_KIND` directly rather than
/// the retired `adapter_kind_budget_key` helper, whose
/// `AdapterKind::Markdownize => "markdown"` mapping was the retired JSONL
/// ledger's key spelling (the unrelated tool-lock JSON schema hardcodes its own
/// literal `"markdown"` key directly in `materialize_tool_lock` and never used
/// that helper — CL61's "markdown"→"markdownize" rename is scoped to the
/// budget/ledger namespace only).
pub(crate) fn task_ledger_key(
    scope_id: &str,
    adapter_kind: &'static str,
    input_hash: &str,
    tool_profile_hash: &str,
) -> LedgerTaskKey {
    LedgerTaskKey::new(scope_id, adapter_kind, input_hash, tool_profile_hash)
}

/// CL57's third condition / CL61: `[budget.per_adapter]`'s device-layer-only cap
/// for `adapter_kind` (`caps.device_per_adapter`'s keys are already validated
/// against the closed enum by `budget::read_budget_config`).
fn per_adapter_cap_for(caps: &BudgetCaps, adapter_kind: &str) -> Option<f64> {
    caps.device_per_adapter.get(adapter_kind).copied()
}

#[must_use]
pub(crate) fn budget_cap_config(
    caps: &BudgetCaps,
    scope_id: &str,
    adapter_kind: &'static str,
) -> BudgetCapConfig {
    let _ = scope_id; // folder cap is scope-independent in shape (CL56); kept for call-site symmetry
    BudgetCapConfig {
        device_cap: caps.device_monthly_usd_cap,
        folder_cap: caps.folder_monthly_usd_cap,
        device_per_adapter_cap: per_adapter_cap_for(caps, adapter_kind),
    }
}

/// Outcome of [`reserve_or_reuse_task_charge`].
pub(crate) enum TaskChargeOutcome {
    /// A brand-new phase 1 (or the `candidate_usd == 0` exemption, CL58) landed.
    Reserved {
        intent_token: String,
        estimated_usd: f64,
    },
    /// An earlier attempt's reservation is still open and covers this resend
    /// (CL42/CL44) — the ledger was not touched.
    Reused {
        intent_token: String,
        estimated_usd: f64,
    },
    /// The device/folder/per_adapter cap would be exceeded (CL56-58).
    BudgetExceeded,
}

/// CL41-44/CL56-61: reserve (phase 1, check-then-reserve, atomic with the cap
/// check) — or, if a live (non-terminal) row already exists for `key`, reuse its
/// `intent_token` without any ledger write at all. This single check replaces the
/// retired JSONL design's task-side `fallback_reason`-driven "RateLimit/
/// QuotaExceeded resend reuses the prior reservation" special case (formerly
/// R16-7): under the SQLite ledger, a non-terminal row IS the live reservation —
/// `phase1_intent`'s own cleanup guard refuses a second phase 1 while one is open
/// (04 §5.8 順序規範) — so "does an open row already exist for this task key" is
/// the whole rule, independent of which retryable error kind left it open.
///
/// `bypass_cap_denial` folds together the two documented reasons a Denied cap
/// check must still proceed to a real reservation (docs/04 §5.4):
/// `kio batch resume --override-budget` (applied "symmetrically" to
/// markdownize/embedding — a one-shot, per-invocation override), and
/// `[budget] hard_stop = false` (F5's persistent soft-stop config: "record the
/// charge and continue over cap" instead of pausing). Both are applied HERE at
/// the call site rather than inside `ops::check_then_reserve` itself: that
/// function's own `BudgetCapConfig`/CL56-61 contract tests pin an
/// unconditional check-then-reserve with no override or hard_stop parameter of
/// its own (this session's read of `kio_pipeline::ledger::ops`'s existing,
/// already-contract-tested API surface — extending it is out of this item's
/// scope, flagged in the report), so a Denied result under either bypass
/// condition falls through to the same `phase1_intent` the Allowed/
/// ExemptZeroCost arms use, skipping only the cap comparison — the
/// reservation itself, and everything downstream of it (settlement,
/// `intent_token`, `stale_after_at`), is unchanged.
pub(crate) fn reserve_or_reuse_task_charge(
    ctx: &Invocation<'_>,
    ledger: &LedgerDb,
    key: &LedgerTaskKey,
    candidate_usd: f64,
    caps: &BudgetCapConfig,
    bypass_cap_denial: bool,
    // Batch-lane wiring (04 §5.8 相 1): the fresh reservation's
    // `batch_requests.request_kind`. Every pre-existing caller passes
    // `RequestKind::Sync` (behavior unchanged — `stale_after_at` computed from
    // the sync effective timeout exactly as before); the Batch send lane
    // passes `RequestKind::Batch`, for which `phase1_intent` records no
    // `stale_after_at`/`job_create_started_at` (§5.8's own recovery-deadline
    // mechanism covers batch rows instead — see `ops::phase1_intent`'s doc).
    request_kind: RequestKind,
) -> Result<TaskChargeOutcome> {
    // QA67 (step4b-contract-tests-p3a.md §T, PB24): online task phase 1 must
    // fail-closed on a live registry scope_id duplicate (10 §3 L297-299) —
    // two `.kio` clones sharing this device-global `batch_requests` row's PK
    // (scope_id, adapter_kind, input_hash, tool_profile_hash) mix up which
    // clone owns the reservation's recovery/settlement/billing attribution.
    registry_duplicate_guard(ctx, &key.scope_id)?;
    let sync_timeout =
        (request_kind == RequestKind::Sync).then_some(TASK_SYNC_EFFECTIVE_TIMEOUT_SECONDS);
    use kio_pipeline::ledger::ops::ReserveOrReuseOutcome;
    let outcome = kio_pipeline::ledger::ops::reserve_or_reuse(
        ledger,
        key,
        candidate_usd,
        caps,
        request_kind,
        sync_timeout,
        bypass_cap_denial,
    )
    .map_err(pipeline_to_kio)?;
    Ok(match outcome {
        ReserveOrReuseOutcome::Reused {
            intent_token,
            estimated_usd,
        } => TaskChargeOutcome::Reused {
            intent_token,
            estimated_usd,
        },
        ReserveOrReuseOutcome::Reserved {
            intent_token,
            estimated_usd,
        } => TaskChargeOutcome::Reserved {
            intent_token,
            estimated_usd,
        },
        ReserveOrReuseOutcome::BudgetExceeded => TaskChargeOutcome::BudgetExceeded,
    })
}

/// QA18/QA19 (step4b-contract-tests-p3a.md §F): `AdapterProfile.billable_kinds`
/// (the typed `BillableUnitKind` enum) as the string keys
/// `resolve_billing_from_reported_usage`'s `declared_billable_kinds` parameter
/// expects (`kio-pipeline`'s ledger, which has no dependency on the specific
/// adapter-profile-construction code in this crate, keys billing resolution
/// off plain strings — see `ledger::ops::BILLABLE_UNIT_KINDS`).
pub(crate) fn declared_billable_kind_strings(
    profile: &AdapterProfile,
) -> std::collections::BTreeSet<String> {
    profile
        .billable_kinds
        .iter()
        .map(|kind| match kind {
            kio_adapter::types::BillableUnitKind::Pages => "pages",
            kio_adapter::types::BillableUnitKind::TokensIn => "tokens_in",
            kio_adapter::types::BillableUnitKind::TokensOut => "tokens_out",
        })
        .map(str::to_owned)
        .collect()
}

/// QA19 (step4b-contract-tests-p3a.md §F, 07 §4 L266-269): record a
/// one-per-run `level=warn` when a `billable_units[].kind` the Adapter
/// reported has no `tools.toml` `[pricing]` entry ("単価未被覆の kind"). The
/// settle call site already degrades that charge to `estimated`
/// (`resolve_billing_from_reported_usage` itself, unconditionally) — this
/// only makes the pricing gap observable instead of a silent under/over-bill.
/// Deduped per `(adapter_kind, sorted kinds)` for the whole process, mirroring
/// the adapter-activation warning path. A no-op for an empty `kinds` (the common
/// case: fully-covered or no billable_units report at all).
pub(crate) fn warn_uncovered_pricing_kinds_once(
    adapter_kind: &str,
    kinds: &[String],
    control: &ControlHandle,
    session: &kio_core::scope::LockSession,
) {
    if kinds.is_empty() {
        return;
    }
    use std::sync::{Mutex, OnceLock};
    static WARNED: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    let warned = WARNED.get_or_init(|| Mutex::new(std::collections::HashSet::new()));
    let dedup_key = format!("{adapter_kind}:{}", kinds.join(","));
    let mut guard = warned
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !guard.insert(dedup_key) {
        return;
    }
    drop(guard);
    let _ = control.append_warn_log_in_session(
        session,
        "KIO-W-ADAPTER-PRICING-UNCOVERED-001",
        "billable_units reported a kind with no tools.toml [pricing] entry; \
         billing degraded to estimated",
        json!({ "adapter_kind": adapter_kind, "kinds": kinds }),
    );
}

/// CL18/CL26/CL47: settle a task's sync row as a successful terminal charge,
/// billed at `billed` — the caller resolves this via
/// `resolve_billing_from_reported_usage` (QA17/QA18/QA19), which degrades to
/// `estimated=1` at the reservation estimate whenever the Adapter has no real
/// usage to report (this codebase's Adapters mostly don't yet —
/// 07-adapter-spec.md's Batch trait does not exist here yet — confirmed by
/// grep, `ledger::ops`'s own module doc; Mistral OCR's markdownize response
/// is the one exception with a real page-count self-report). `intent_token`
/// clears in the same Tx — sync rows always clear immediately on ANY terminal
/// Tx, unlike batch rows (CL47).
///
/// R23-03 (04 §5.4: "device 行の全ての状態遷移 UPDATE ... は `WHERE
/// intent_token = <自 token>` の条件付き (CAS) で行う"): CAS-guarded on our own
/// `intent_token`. This function is shared by both scope-keyed task charges
/// (serialized by `.kio/.lock` — the guard is a harmless no-op there, since no
/// concurrent writer can have changed the row) and the device query-embedding
/// row (§H — NOT lock-serialized; a concurrent stale-sweep recoverer can
/// reclaim this exact row under a NEW token while this call's own adapter
/// call is still in flight). A 0-row CAS miss means our claim was already
/// reclaimed — record nothing (the reclaimer's own `unknown_settled` already
/// covers this attempt); the vector this call already received from the
/// adapter is still returned to the caller for THIS search's own result,
/// independent of billing (same 04 §5.4 clause: "受信済み vector を当該検索の
/// 結果に使うことは課金と独立に可").
pub(crate) fn settle_task_charge_success(
    ledger: &LedgerDb,
    key: &LedgerTaskKey,
    intent_token: &str,
    billed: BilledAmount,
) -> Result<()> {
    terminal_transaction(
        ledger,
        &TerminalWrite {
            key,
            outcome: Outcome::Succeeded,
            billed,
            // DDL's 3-way `batch_job_id` rule (04 §5.4): a sync call with no
            // provider request id falls back to the attempt's own intent_token.
            ledger_batch_job_id: intent_token,
            next_state: BatchState::Completed,
            error: None,
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

/// CL45's "cannot confirm the outcome" settlement (`outcome='unknown_settled'`,
/// billed at the conservative reservation estimate — "over-count safer than
/// under-count"). Used both for a task's definitively-failed send (retry budget
/// exhausted, or a non-retryable error kind) and for releasing a task's charge
/// without ever resending (superseded by an edit, deleted, purged, demoted to a
/// secrets hold, …). This codebase's Adapters have no post-hoc result-query
/// capability at all (CL45: an Adapter without one "常に...unknown 精算になる"),
/// so `unknown_settled` is this module's uniform non-success terminal outcome —
/// see the implementation report for how this differs from the retired JSONL
/// design's surgical reclaim-to-zero of specifically-non-billable error kinds
/// (RateLimit/QuotaExceeded/AuthError): every other terminal outcome in the
/// closed 8-value enum (CL26) either requires data this module never has
/// (`contract_violation`/`purged`/`submit_rejected`/`fallback_to_full` all
/// presuppose an Adapter-reported structured signal this sync integration does
/// not produce) or is a distinct CLI-driven action (`abandoned`, CL64).
pub(crate) fn settle_task_charge_unknown(
    ledger: &LedgerDb,
    key: &LedgerTaskKey,
    intent_token: &str,
    estimated_usd: f64,
) -> Result<()> {
    recovery_settle_unknown(ledger, key, intent_token, estimated_usd, true)
        .map_err(pipeline_to_kio)
        .map(|_| ())
}

/// If a live (non-terminal) sync row exists for `key`, settle it unknown
/// (`settle_task_charge_unknown`). A no-op returning `false` when no such row
/// exists (already terminal-and-cleaned, or never reserved) — idempotent,
/// mirroring CL66's "target-less op is a success" posture. Used by every
/// "release this task's charge without resending" call site (supersede,
/// purge-adjacent demotes, auth-revive, non-live retirement, …) that the retired
/// JSONL design drove through `settle_task_reservation`.
///
/// Batch-lane rows (04 §5.8): a `request_kind='batch'` row that reached phase
/// 2a (`provider_scope_id` recorded) may hold provider residue (upload/job)
/// whose ONLY discovery key is the filename-embedded `intent_token` — the
/// settlement here must NOT clear the token in the same Tx (cleanup-first;
/// same `provider_scope_id IS NULL` rule as `execute_abandon`'s note-5).
/// Reconcile/abandon complete the residue cleanup and the eventual NULL-out.
pub(crate) fn release_task_charge_if_open(ledger: &LedgerDb, key: &LedgerTaskKey) -> Result<bool> {
    let Some(row) = get_batch_request(ledger, key).map_err(pipeline_to_kio)? else {
        return Ok(false);
    };
    if !row.state.is_inflight() {
        return Ok(false);
    }
    let Some(intent_token) = row.intent_token.clone() else {
        return Ok(false);
    };
    let cleanup_already_complete =
        row.request_kind != RequestKind::Batch || row.provider_scope_id.is_none();
    recovery_settle_unknown(
        ledger,
        key,
        &intent_token,
        row.estimated_usd,
        cleanup_already_complete,
    )
    .map_err(pipeline_to_kio)?;
    Ok(true)
}
