use crate::approvals;
use crate::commands::AdapterArgs;
use crate::commands::AdapterCommand;
use crate::commands::BatchArgs;
use crate::commands::BatchCommand;
use crate::context::AppContext;
use crate::egress::SECRETS_TIER_B_HOLD;
use crate::egress::secrets_send_approved;
use crate::embedding::enrichment::bind_embedding_work;
use crate::embedding::enrichment::embedding_task_output_ref;
use crate::embedding::enrichment::plan_embed_batch;
use crate::embedding::enrichment::retained_chunks_without_embedding;
use crate::embedding::enrichment::retained_history_chunks;
use crate::embedding::transport::LaneOverride;
use crate::embedding::transport::declared_embedding_profile;
use crate::embedding::transport::embedding_execution;
use crate::embedding::transport::poll_batch_embedding_jobs;
use crate::embedding::transport::resolve_invocation_lane;
use crate::historical_reindex::retained_history_instances;
use crate::index::chunk_ledger::ensure_purge_epoch_initialized;
use crate::index::chunk_ledger::sqlite_path;
use crate::index::pipeline::online_markdownize_profile;
use crate::index::pipeline::online_markdownize_profile_for;
use crate::index::pipeline::online_output_ref;
use crate::invocation::Invocation;
use crate::ledger_access::SYNC_RESEND_AUTHORIZED_REASON;
use crate::ledger_access::ledger_db_path;
use crate::ledger_access::ledger_snapshot_to_kio;
use crate::ledger_access::open_ledger_db;
use crate::ledger_access::open_optional_ledger_db;
use crate::ledger_access::recover_stale_sync_rows;
use crate::markdownize::batch::poll_batch_markdownize_jobs;
use crate::markdownize::execution::OnlineMarkdownizePrecondition;
use crate::markdownize::execution::classify_online_markdownize_precondition;
use crate::object_persistence::adapter_to_kio;
use crate::object_persistence::index_to_kio;
use crate::object_persistence::pipeline_to_kio;
use crate::online_task::targets_standard_online_markdownize;
use crate::response::batch::*;
use crate::response::ledger::*;
use crate::response::{CommandResponse, Completion, ResponseBody, project_legacy};
use crate::scope_access::scope_id;
use crate::search::query::snapshot_chunking_config_hash;
use crate::search::scope::current_embedding_policy;
use crate::settings::batch_recovery_context;
use crate::settings::require_repo_tool_lock;
use crate::settings::user_config_toml_path;
use crate::task_execution::ExecOutcome;
use crate::task_execution::execute_pending_tasks;
use crate::task_execution::retry_kind_from_reason;
use crate::task_execution::task_retry_allowed;
use crate::task_execution::task_retry_due;
use crate::task_execution::{batch_completion, batch_error_code};
use crate::trust;
use kio_adapter::catalog::DeclaredEmbeddingProfile;
use kio_adapter::tool_lock::AdapterRuntimeSettings;
use kio_core::ExitCode;
use kio_core::KioError;
use kio_core::Result;
use kio_core::control::ControlHandle;
use kio_core::scope::Repository;
use kio_index::fts::ExistingSourceIndexOpenMode;
use kio_index::fts::FtsSchemaConfig;
use kio_index::fts::FtsTokenizer;
use kio_index::fts::open_existing_source_index_connection;
use kio_index::registry::RegistryDb;
use kio_pipeline::budget::read_budget_policy;
use kio_pipeline::ledger::LedgerDb;
use kio_pipeline::ledger::LedgerReadSnapshot;
use kio_pipeline::ledger::TaskKey as LedgerTaskKey;
use kio_pipeline::ledger::ops::AbandonExecution;
use kio_pipeline::ledger::ops::AbandonResolution;
use kio_pipeline::ledger::ops::AbandonSelector;
use kio_pipeline::ledger::ops::authorize_unknown_sync_resend;
use kio_pipeline::ledger::ops::execute_abandon;
use kio_pipeline::ledger::ops::get_batch_request;
use kio_pipeline::ledger::ops::recovery_settle_unknown;
use kio_pipeline::ledger::ops::reset_contract_violations;
use kio_pipeline::ledger::ops::resolve_abandon_selector;
use kio_pipeline::ledger::ops::uuid_v7_timestamp_millis;
use kio_pipeline::markdownize::NormalizedInstanceManifest;
use kio_pipeline::markdownize::UnitStatus;
use kio_pipeline::markdownize::load_validated_normalized_instance_with_control;
use kio_pipeline::task::RESULT_UNKNOWN_REASON;
use kio_pipeline::task::TaskDescriptor;
use kio_pipeline::task::TaskOutputRef;
use kio_pipeline::task::TaskStatus;
use kio_pipeline::task::TaskStore;
use kio_pipeline::task::TaskType;
use kio_pipeline::task::retry_policy;
use kio_pipeline::task::validate_task_output_ref;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

pub(crate) fn run_batch(ctx: &Invocation<'_>, args: BatchArgs) -> Result<Value> {
    let adapter_settings = ctx.settings;

    validate_batch_args(&args)?;
    // Same lane contract as `run_index`: resume/retry drive real online sends,
    // so the invocation lane has to be resolved before any of them is planned.
    let lane_override = match &args.command {
        Some(BatchCommand::Resume(resume)) => LaneOverride::new(resume.realtime, resume.batch),
        Some(BatchCommand::Retry(retry)) => LaneOverride::new(retry.realtime, retry.batch),
        Some(BatchCommand::Abandon(_)) | None => LaneOverride::default(),
    };
    let repo = ctx.open_recovery_repository()?;
    resolve_invocation_lane(
        lane_override,
        Some(&repo.kio_dir().join("config.toml")),
        ctx.control,
    )?;
    // O3: `batch resume` / `batch retry` read-modify-write `tasks.jsonl` and drive
    // online sends, so hold the folder store lock end-to-end — the same guard M1
    // wired onto index/repair/reindex. Without it two concurrent `batch resume`
    // runs interleave the ledger and double-send held tasks. Reentrant with any
    // inner auto-snapshot; losers fail fast with KIO-E-STORE-LOCKED-001 (exit 3).
    let _lock = repo.lock_store_in_session(repo.lock_session())?;
    // Persisted recovery work is executable authority. Revalidate the repository's
    // current tool lock after acquiring the store lock and before reading or
    // mutating tasks, so resume/retry cannot bypass the same adapter policy used by
    // index and repair paths.
    require_repo_tool_lock(&repo)?;
    // Ledger-only mutations must not poll or collect unrelated provider work
    // before their own confirmation. They still run under the same repository
    // lock and current tool-lock validation as every other batch mutation.
    match &args.command {
        Some(BatchCommand::Retry(retry)) if retry.reset_violations.is_some() => {
            return run_batch_reset_violations(
                ctx,
                retry.reset_violations.as_deref().expect("validated above"),
                retry.yes,
            );
        }
        Some(BatchCommand::Retry(retry)) if retry.resend_unknown.is_some() => {
            return run_batch_resend_unknown(
                ctx,
                &repo,
                retry.resend_unknown.as_deref().expect("validated above"),
                retry.yes,
            );
        }
        Some(BatchCommand::Abandon(abandon)) => {
            return run_batch_abandon(ctx, &abandon.selector, abandon.yes);
        }
        _ => {}
    }
    // CL45/item 5: reconcile any stale `request_kind='sync'` cost-ledger.sqlite
    // rows left by a crashed prior run — applies uniformly to resume/retry/
    // abandon, all of which reach this point before their own dispatch.
    let batch_offline = matches!(&args.command, Some(BatchCommand::Resume(resume)) if resume.offline)
        || matches!(&args.command, Some(BatchCommand::Retry(retry)) if retry.offline);
    let store = TaskStore::new_with_control(repo.kio_dir(), repo.control().clone());
    if !batch_offline && let Some(ledger) = open_optional_ledger_db(ctx.control)? {
        recover_stale_sync_rows(&ledger, Some(&store), &scope_id(repo.kio_dir())?)?;
    }
    // LC39/LC40 (see `ensure_purge_epoch_initialized`'s doc comment).
    ensure_purge_epoch_initialized(repo.kio_dir(), repo.control())?;
    // 04 §5.8 相 3: poll/collect this scope's in-flight Batch markdownize jobs
    // BEFORE dispatch, so a Success job lands its task Done ahead of the
    // Resume/Retry pass below (whose promotion step then publishes it), and a
    // still-queued job extends its poll schedule. Folded into the Resume/Retry
    // ExecOutcome so "in-flight のみ残" maps to the batch exit-3 semantics.
    let mut batch_poll_outcome = ExecOutcome::default();
    if !batch_offline && let Some(ledger) = open_optional_ledger_db(ctx.control)? {
        batch_poll_outcome = poll_batch_markdownize_jobs(adapter_settings, &repo, &store, &ledger)?;
        batch_poll_outcome.add(poll_batch_embedding_jobs(ctx, &repo, &ledger)?);
    }
    // N1: a Tier B online hold is only lifted by an explicit `--send-secrets`
    // approval, never by a plain `batch resume`. Without this, resume's
    // Paused -> Pending flip would silently un-hold the candidate-secret task.
    let online_profile = online_markdownize_profile_for(&repo)?;
    let secrets_approved = secrets_send_approved(
        adapter_settings,
        &repo,
        approvals::AdapterRole::Markdown,
        &online_profile.adapter_id,
        &online_profile.tool_profile_hash,
    );
    match args.command {
        Some(BatchCommand::Resume(resume)) => {
            // QA30: same mutual-exclusion validation `kio index`/`kio repair`
            // apply to their own `--online`/`--offline`.
            if resume.online && resume.offline {
                return Err(KioError::invalid_usage(
                    "--online and --offline are mutually exclusive",
                ));
            }
            // A recheck is an operator-requested state transition based on the
            // *current* cap. Validate that policy before unpausing anything: a
            // malformed replacement config must leave persisted budget holds
            // intact rather than turning them into Pending work which a later
            // invocation could send under an unknown policy.
            if resume.recheck_budget {
                read_budget_policy(
                    user_config_toml_path(ctx.control),
                    repo.kio_dir().join("config.toml"),
                )
                .map_err(pipeline_to_kio)?;
            }
            let changed = store
                .update_matching(|task| {
                    let held_secret = task.fallback_reason.as_deref() == Some(SECRETS_TIER_B_HOLD);
                    // QA2 (step4b-contract-tests-p3a.md §A, 04 §5.2): this generic
                    // flip already covers an auth hold (`hold_reason=auth`) — any
                    // non-budget, non-secret Paused task, auth included — so
                    // `batch resume` is the release for auth_error too, not just
                    // budget/tier_b.
                    if task.status == TaskStatus::Paused
                        && (resume.override_budget
                            || resume.recheck_budget
                            || task.fallback_reason.as_deref() != Some("budget_exceeded"))
                        && (!held_secret
                            || (secrets_approved
                                && targets_standard_online_markdownize(
                                    store.kio_dir(),
                                    task,
                                    &online_output_ref(&online_profile.adapter_id),
                                )))
                    {
                        task.status = TaskStatus::Pending;
                        task.fallback_reason = None;
                        task.hold_reason = None;
                        true
                    } else {
                        false
                    }
                })
                .map_err(pipeline_to_kio)?;
            let mut outcome = execute_pending_tasks(
                Invocation {
                    settings: adapter_settings,
                    context: ctx.context,
                    control: ctx.control,
                    locks: ctx.locks,
                },
                &repo,
                &store,
                resume.override_budget,
                true,
                resume.online,
                resume.offline,
            )?;
            outcome.add(batch_poll_outcome);
            let completion = batch_completion(&outcome);
            project_legacy(CommandResponse {
                body: ResponseBody::Batch(BatchResponse::Resume(BatchResumeResponse {
                    status: "resumed".to_owned(),
                    recheck_budget: resume.recheck_budget,
                    override_budget: resume.override_budget,
                    counts: BatchCounts {
                        tasks_updated: changed,
                        tasks_executed: outcome.executed,
                        tasks_attempted: outcome.attempted(),
                        tasks_failed: outcome.failed,
                        tasks_paused: outcome.paused,
                        tasks_inflight: outcome.inflight,
                        tasks_inflight_unreadable: outcome.held_unreadable,
                    },
                    error_code: batch_error_code(completion),
                })),
                completion,
            })
        }
        Some(BatchCommand::Retry(retry_args)) => {
            // CL62-CL68/§M note-4: `--reset-violations <selector>` is a distinct
            // sub-mode — it targets exactly one cost-ledger.sqlite batch_requests
            // row (by intent_token or task-key selector, same form as `abandon`),
            // not the bulk tasks.jsonl retry scan below. Its dispatch returned
            // above so confirmation precedes provider polling.
            // QA30: same mutual-exclusion validation `kio index`/`kio repair`
            // apply to their own `--online`/`--offline`.
            if retry_args.online && retry_args.offline {
                return Err(KioError::invalid_usage(
                    "--online and --offline are mutually exclusive",
                ));
            }
            // R9-4: `batch retry` also recovers Partial online markdownize tasks
            // (docs/04 §5.2 `partial -> done`). A Partial task has some Done and
            // some Failed units; re-drive it (Pending) with `unit_keys` scoped to
            // the still-Failed units and the placeholder output_ref restored so
            // execute_pending_markdownize_tasks selects it. On full success it
            // becomes Done, ending the silent data gap index_status showed as
            // fully enriched.
            let incomplete_reenqueued = reenqueue_incomplete_markdownize_tasks(&store)?;
            let changed = store
                .update_matching(|task| {
                    if task.status == TaskStatus::Failed
                        // A validated online response with failed units has an immutable
                        // normalized manifest. Its unit-level retry plan owns recovery;
                        // the generic task-error retry must not erase that distinction.
                        && task.fallback_reason.as_deref() != Some("online_adapter_done")
                        && task_retry_allowed(task)
                        && task_retry_due(task, ctx.control)
                    {
                        task.status = TaskStatus::Pending;
                        task.next_retry_at = None;
                        true
                    } else {
                        false
                    }
                })
                .map_err(pipeline_to_kio)?;
            // `batch retry` never overrides the budget cap (retry re-schedules,
            // it does not bypass caps — `batch resume --override-budget` does), and never
            // revives an auth_error task (CT2-TASK-005: `max_attempts=0` is this command's
            // contract; `batch resume` is where repaired credentials take effect).
            let mut outcome = execute_pending_tasks(
                Invocation {
                    settings: adapter_settings,
                    context: ctx.context,
                    control: ctx.control,
                    locks: ctx.locks,
                },
                &repo,
                &store,
                false,
                false,
                retry_args.online,
                retry_args.offline,
            )?;
            outcome.add(batch_poll_outcome);
            let completion = batch_completion(&outcome);
            project_legacy(CommandResponse {
                body: ResponseBody::Batch(BatchResponse::Retry(BatchRetryResponse {
                    status: "retry scheduled".to_owned(),
                    counts: BatchCounts {
                        tasks_updated: changed + incomplete_reenqueued,
                        tasks_executed: outcome.executed,
                        tasks_attempted: outcome.attempted(),
                        tasks_failed: outcome.failed,
                        tasks_paused: outcome.paused,
                        tasks_inflight: outcome.inflight,
                        tasks_inflight_unreadable: outcome.held_unreadable,
                    },
                    error_code: batch_error_code(completion),
                })),
                completion,
            })
        }
        Some(BatchCommand::Abandon(_)) => unreachable!("handled before polling"),
        None => Err(KioError::not_implemented("batch command")),
    }
}

/// Enforce batch sub-mode constraints for direct app callers as well as Clap.
/// In particular, reset is a ledger mutation, so its transport/lane flags are
/// never silently ignored.
pub(crate) fn validate_batch_args(args: &BatchArgs) -> Result<()> {
    let Some(BatchCommand::Retry(retry)) = args.command.as_ref() else {
        return Ok(());
    };

    let ledger_mutation = retry.reset_violations.is_some() || retry.resend_unknown.is_some();
    if retry.reset_violations.is_some() && retry.resend_unknown.is_some() {
        return Err(KioError::invalid_usage(
            "--reset-violations and --resend-unknown are mutually exclusive",
        ));
    }
    if retry.yes && !ledger_mutation {
        return Err(KioError::invalid_usage(
            "--yes is valid only with batch retry --reset-violations or --resend-unknown",
        ));
    }
    if ledger_mutation && !retry.yes {
        return Err(KioError::invalid_usage(
            "--reset-violations and --resend-unknown require --yes",
        ));
    }
    if ledger_mutation && (retry.online || retry.offline || retry.realtime || retry.batch) {
        return Err(KioError::invalid_usage(
            "--reset-violations and --resend-unknown cannot be combined with --online, --offline, --realtime, or --batch",
        ));
    }
    Ok(())
}

pub(crate) fn run_adapter(ctx: &Invocation<'_>, args: AdapterArgs) -> Result<Value> {
    match args.command {
        Some(AdapterCommand::Approve(args)) => {
            approvals::approve(ctx, args, &approvals::ApprovalControl::default())
        }
        Some(AdapterCommand::Status(args)) => approvals::status(ctx, args),
        command => run_adapter_lifecycle(ctx.context, command, ctx.control),
    }
}

pub(crate) fn run_adapter_lifecycle(
    ctx: &AppContext,
    command: Option<AdapterCommand>,
    control: &ControlHandle,
) -> Result<Value> {
    match command {
        Some(AdapterCommand::Revoke(args)) => approvals::revoke(ctx, args, control),
        Some(AdapterCommand::Trust(command)) => {
            trust::run(&trust::directory_path(control), command, control)
        }
        None => Err(KioError::invalid_usage(
            "adapter requires approve, revoke, status, or trust",
        )),
        _ => unreachable!("content adapter commands require runtime settings"),
    }
}

/// `kio ledger reconcile` (QA14/QA15, step4b-contract-tests-p3a.md L307-338,
/// 10-operations.md §7.5.2 / 04-pipeline.md §5.8). Unlike `kio batch *`/
/// `kio adapter *`, this command is device-global — it does not operate on
/// the current directory's `.kio` scope (there may not even be one), so it
/// takes no `Repository::open_current()` and holds no `.kio/.lock` (the same
/// device-global-read posture `stalled_batch_status` already has;
/// unlike that function, this one also writes, but every write it makes
/// funnels through the same CAS/savepoint-guarded primitives every other
/// ledger writer uses, so no additional locking is needed here). Runnable
/// whether or not a restore was ever detected — see `LedgerCommand::Reconcile`'s
/// doc comment: idempotent, and useful standalone.
pub(crate) fn run_ledger_reconcile(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    control: &ControlHandle,
) -> Result<Value> {
    let ledger = open_ledger_db(control)?;
    let conn = &ledger;

    // Design step (a): integrity check, run as reconcile's own first action
    // regardless of whether `LedgerDb::open` already ran one this process
    // (it only does so "on detection" of a restore — reconcile always
    // checks). Table/index shape is already verified by `open`
    // (`ensure_schema`), so it is not re-checked here.
    let integrity = conn.integrity_check().map_err(pipeline_to_kio)?;
    if integrity != "ok" {
        // The restore-reconcile marker (if any) is deliberately left in
        // place — reconcile did not complete, so the gate must keep refusing
        // new submissions until a clean run succeeds.
        return Err(pipeline_to_kio(kio_pipeline::PipelineError::corrupt(
            "cost-ledger.sqlite",
            format!(
                "PRAGMA integrity_check reported corruption \
                 (10-operations.md §7.5.2): {integrity}"
            ),
        )));
    }

    // Design step (b): the existing sync-row crash recovery
    // (`recover_stale_sync_rows`), swept across every scope_id that carries
    // a sync row — this device-global command has no single owning scope to
    // scope that sweep to (see `distinct_scope_ids_with_sync_rows`'s doc
    // comment).
    let sync_scope_ids = kio_pipeline::ledger::ops::distinct_scope_ids_with_sync_rows(conn)
        .map_err(pipeline_to_kio)?;
    let mut sync_settled = 0_u64;
    for scope_id in &sync_scope_ids {
        sync_settled += recover_stale_sync_rows(&ledger, None, scope_id)?;
    }

    // The provider inventory drives both the batch-row recovery walk (design
    // step c: existing local rows -> does the provider have this job?) and
    // the orphan/unknown-attribution walk (steps d+e: provider jobs/uploads
    // -> is there a local row for this?) — fetched once, shared by both.
    // Batch send lane (07 §5.5 / 10 §7.5.2): `configured_inventories` itself
    // consults the configured Batch client (mock seam or the real Mistral
    // lane) and maps job metadata's 5-key contract onto these attribution
    // records — no CLI-side merge needed.
    let inventories = kio_adapter::batch_inventory::configured_inventories_with_control(
        adapter_settings,
        || batch_recovery_context(&ledger),
        control,
    )
    .map_err(adapter_to_kio)?;

    let batch_walk = run_batch_recovery_walk(conn, &inventories)?;
    let orphan_walk = run_orphan_attribution_walk(conn, &inventories)?;

    // Provider inventory can reconcile known intents; it cannot prove that
    // billing history lost from an older database never existed. This path
    // must never clear a history-loss gate or lower the durable checkpoint.

    project_legacy(CommandResponse {
        body: ResponseBody::Ledger(LedgerResponse::Reconcile(LedgerReconcileResponse {
            status: "reconciled".to_owned(),
            integrity,
            sync_settled,
            batch_found: batch_walk.found,
            batch_found_keys: batch_walk.found_keys,
            batch_settled_unknown: batch_walk.settled_unknown,
            batch_settled_unknown_keys: batch_walk.settled_unknown_keys,
            unlistable: batch_walk.unlistable,
            unlistable_keys: batch_walk.unlistable_keys,
            phase2b_not_started: batch_walk.not_job_matchable,
            phase2b_not_started_keys: batch_walk.not_job_matchable_keys,
            orphans: orphan_walk.orphans,
            unknown: orphan_walk.unknown,
            unknown_uploads: orphan_walk.unknown_uploads,
        })),
        completion: Completion::Complete,
    })
}

/// `scope_id/adapter_kind/input_hash/tool_profile_hash` — the same 4-tuple
/// selector shape `parse_batch_selector` accepts, used here only for
/// human/machine-readable reporting (never re-parsed).
fn task_key_selector_string(key: &LedgerTaskKey) -> String {
    format!(
        "{}/{}/{}/{}",
        key.scope_id, key.adapter_kind, key.input_hash, key.tool_profile_hash
    )
}

/// QA15 design step (c) — `kio ledger reconcile`'s batch-row recovery walk:
/// the FIRST production wiring of `recovery_candidates` (previously
/// test-only, per this session's recon — see the implementation report). For
/// each candidate row: an inventory covering its `provider_scope_id` that
/// lists a job matching the row's `intent_token` -> `found` (self-describes
/// `batch_job_id` via `recovery_mark_found` if not already recorded); no
/// match, but the recovery deadline AND visibility grace period have both
/// elapsed (04 §5.8 confirmed-absent) -> `unknown_settled`
/// (`recovery_settle_unknown`); a row whose `provider_scope_id` has no
/// configured inventory at all -> `unlistable` (10 §7.5.2: "どちらにも無い
/// scope は原理的に走査できない" — no state change, retried on a later run).
/// Rows with `provider_scope_id IS NULL` (相 2b 未着手) are excluded from
/// job-matching entirely per 04 §5.8 L1063-1066 ("job 不存在は記録から確定
/// している") and reported separately (`not_job_matchable`) — no
/// upload-cleanup machinery exists yet for them, so reconcile reports them
/// rather than attempting anything.
struct BatchRecoveryWalkReport {
    found: u64,
    found_keys: Vec<String>,
    settled_unknown: u64,
    settled_unknown_keys: Vec<String>,
    unlistable: u64,
    unlistable_keys: Vec<String>,
    not_job_matchable: u64,
    not_job_matchable_keys: Vec<String>,
}

fn run_batch_recovery_walk(
    conn: &LedgerDb,
    inventories: &[kio_adapter::batch_inventory::ProviderInventory],
) -> Result<BatchRecoveryWalkReport> {
    let now_ms = kio_pipeline::ledger::time::now_millis();
    let candidates =
        kio_pipeline::ledger::ops::recovery_candidates(conn).map_err(pipeline_to_kio)?;
    let mut report = BatchRecoveryWalkReport {
        found: 0,
        found_keys: Vec::new(),
        settled_unknown: 0,
        settled_unknown_keys: Vec::new(),
        unlistable: 0,
        unlistable_keys: Vec::new(),
        not_job_matchable: 0,
        not_job_matchable_keys: Vec::new(),
    };
    for row in candidates {
        let selector = task_key_selector_string(&row.key);
        let Some(provider_scope_id) = row.provider_scope_id.clone() else {
            report.not_job_matchable += 1;
            report.not_job_matchable_keys.push(selector);
            continue;
        };
        let Some(inventory) = inventories
            .iter()
            .find(|inventory| inventory.provider_scope_id == provider_scope_id)
        else {
            report.unlistable += 1;
            report.unlistable_keys.push(selector);
            continue;
        };
        let matched_job = row.intent_token.as_deref().and_then(|token| {
            inventory
                .jobs
                .iter()
                .find(|job| job.intent_token.as_deref() == Some(token))
        });
        if let Some(job) = matched_job {
            kio_pipeline::ledger::ops::recovery_mark_found(conn, &row.key, &job.job_id)
                .map_err(pipeline_to_kio)?;
            report.found += 1;
            report.found_keys.push(selector);
            continue;
        }
        let deadline_passed = kio_pipeline::ledger::ops::recovery_deadline_passed(
            &row,
            now_ms,
            kio_pipeline::ledger::ops::DEFAULT_RECOVERY_DEADLINE_MS,
        );
        let grace_elapsed = kio_pipeline::ledger::ops::visibility_grace_period_elapsed(
            row.job_create_started_at,
            now_ms,
            kio_pipeline::ledger::ops::DEFAULT_VISIBILITY_GRACE_PERIOD_MS,
        );
        if deadline_passed
            && grace_elapsed
            && let Some(token) = row.intent_token.clone()
        {
            // `provider_scope_id` is `Some` in this branch by
            // construction (the early-continue guard above), so upload
            // cleanup can never already be confirmed complete here —
            // mirrors `execute_abandon`'s own
            // `row.provider_scope_id.is_none()` rule for the same
            // `cleanup_already_complete` argument.
            recovery_settle_unknown(conn, &row.key, &token, row.estimated_usd, false)
                .map_err(pipeline_to_kio)?;
            report.settled_unknown += 1;
            report.settled_unknown_keys.push(selector);
        }
        // else: neither found nor confirmed-absent yet — 04 §5.8's "unknown"
        // bullet ("何も変更せず保持し、次回再試行する"): leave untouched,
        // retried on a later `kio ledger reconcile` run.
    }
    Ok(report)
}

/// 10 §7.5.2 L660-663's "ローカル構成の scope" set: every `scope_registry` row
/// whose `root_path/.kio/scope.json` can be read AND whose `scope_id` equals
/// the registry row's own — "実地検証 (読取 + scope_id 一致) できた scope_id
/// 集合". An unreadable or mismatched row is excluded (its jobs fall to
/// `unknown`, not `orphans` — 10 §7.5.2: "未再登録 scope の job は unknown 側
/// に落ち、再登録後の再実行で orphan 候補へ移る", which is exactly what
/// happens the next time this function runs after re-registration — the
/// walk is idempotent and self-correcting, not a one-shot classification).
/// Registry-open/read failure degrades to an empty set (everything falls to
/// `unknown`) rather than failing the whole reconcile — the registry is a
/// recoverable search cache (03-data-model.md §4), never truth.
fn verified_local_scope_ids(control: &ControlHandle) -> BTreeSet<String> {
    let mut verified = BTreeSet::new();
    let Ok(registry) = RegistryDb::open_default_with_control(control) else {
        return verified;
    };
    let Ok(entries) = registry.all_entries() else {
        return verified;
    };
    for entry in entries {
        let kio_dir = PathBuf::from(&entry.root_path).join(".kio");
        if let Ok(found_scope_id) = scope_id(&kio_dir)
            && found_scope_id == entry.scope_id
        {
            verified.insert(entry.scope_id);
        }
    }
    verified
}

pub(crate) struct OrphanAttributionReport {
    orphans: Vec<OrphanJob>,
    pub(crate) unknown: Vec<UnknownJob>,
    pub(crate) unknown_uploads: Vec<UnknownUpload>,
}

/// QA15 design steps (d)+(e) — `kio ledger reconcile`'s orphan/unknown-
/// attribution walk (10 §7.5.2): the OTHER direction of matching from
/// `run_batch_recovery_walk` ("does the provider have a job with no local
/// row", not "does my row have a matching provider job"). Read-only —
/// nothing here mutates `batch_requests`/`cost_ledger`, so rerunning
/// reconcile produces an identical report (10 §7.5.2: "報告は冪等"). Nothing
/// is auto-resubmitted or auto-deleted, matching 10 §7.5.2's explicit rule.
pub(crate) fn run_orphan_attribution_walk(
    conn: &LedgerDb,
    inventories: &[kio_adapter::batch_inventory::ProviderInventory],
) -> Result<OrphanAttributionReport> {
    let verified_scopes = verified_local_scope_ids(conn.control());
    let mut orphans = Vec::new();
    let mut unknown = Vec::new();
    let mut unknown_uploads = Vec::new();

    for inventory in inventories {
        for job in &inventory.jobs {
            if job_is_accounted_for(conn, job)? {
                continue;
            }
            match &job.task_key {
                Some(task_key) if verified_scopes.contains(&task_key.scope_id) => {
                    orphans.push(OrphanJob { job_id: job.job_id.clone(), task_key: AttributionTaskKey { scope_id: task_key.scope_id.clone(), adapter_kind: task_key.adapter_kind.clone(), input_hash: task_key.input_hash.clone(), tool_profile_hash: task_key.tool_profile_hash.clone() }, guidance: OrphanGuidance { result_fetch: "read-only-safe: fetching this job's result does not create new state".to_owned(), deletion: "delete only after confirming no other Kio instance shares this provider scope".to_owned() } });
                }
                Some(task_key) => {
                    unknown.push(UnknownJob {
                        job_id: job.job_id.clone(),
                        task_key: Some(AttributionTaskKey {
                            scope_id: task_key.scope_id.clone(),
                            adapter_kind: task_key.adapter_kind.clone(),
                            input_hash: task_key.input_hash.clone(),
                            tool_profile_hash: task_key.tool_profile_hash.clone(),
                        }),
                        reason: "task_key.scope_id is not a locally-verified registered scope"
                            .to_owned(),
                    });
                }
                None => {
                    unknown.push(UnknownJob {
                        job_id: job.job_id.clone(),
                        task_key: None,
                        reason: "job metadata carries no task_key (unreadable or absent)"
                            .to_owned(),
                    });
                }
            }
        }
        for upload in &inventory.uploads {
            let accounted_for = match &upload.filename_token {
                Some(token) => matches!(
                    resolve_abandon_selector(conn, &AbandonSelector::IntentToken(token.clone()))
                        .map_err(pipeline_to_kio)?,
                    AbandonResolution::Found(_)
                ),
                None => false,
            };
            if !accounted_for {
                unknown_uploads.push(UnknownUpload {
                    upload_id: upload.upload_id.clone(),
                    filename_token: upload.filename_token.clone(),
                });
            }
        }
    }

    Ok(OrphanAttributionReport {
        orphans,
        unknown,
        unknown_uploads,
    })
}

pub(crate) fn provider_task_ledger_key(
    key: &kio_adapter::batch_inventory::ProviderTaskKey,
) -> LedgerTaskKey {
    LedgerTaskKey::new(
        key.scope_id.clone(),
        key.adapter_kind.clone(),
        key.input_hash.clone(),
        key.tool_profile_hash.clone(),
    )
}

/// Whether some local `batch_requests` row already accounts for `job` — by
/// its `task_key`'s exact 4-tuple PRIMARY KEY match first (10 §7.5.2's
/// primary attribution rule: "job の帰属は...job metadata の task key 4 組が
/// 担う"), then, secondarily, by `intent_token` (04 §5.8's found/
/// confirmed-absent matching key — a defense-in-depth check for a job whose
/// `task_key` this fixture/provider never populated but whose token still
/// resolves).
fn job_is_accounted_for(
    conn: &LedgerDb,
    job: &kio_adapter::batch_inventory::ProviderJobRecord,
) -> Result<bool> {
    if let Some(task_key) = &job.task_key {
        let key = provider_task_ledger_key(task_key);
        if get_batch_request(conn, &key)
            .map_err(pipeline_to_kio)?
            .is_some()
        {
            return Ok(true);
        }
    }
    if let Some(token) = &job.intent_token
        && let AbandonResolution::Found(_) =
            resolve_abandon_selector(conn, &AbandonSelector::IntentToken(token.clone()))
                .map_err(pipeline_to_kio)?
    {
        return Ok(true);
    }
    Ok(false)
}

/// `kio batch abandon` / `--reset-violations`'s shared selector grammar (06
/// §1 L44/L26-30, CL62): an `intent_token` (no `/`), or a `scope_id/adapter_kind/
/// input_hash[/tool_profile_hash]` task-key path (3-tuple accepted; resolution
/// rejects it if ambiguous).
fn parse_batch_selector(input: &str) -> Result<AbandonSelector> {
    if !input.contains('/') {
        return Ok(AbandonSelector::IntentToken(input.to_owned()));
    }
    let parts: Vec<&str> = input.split('/').collect();
    match parts.as_slice() {
        [scope_id, adapter_kind, input_hash] => Ok(AbandonSelector::ThreeTuple {
            scope_id: (*scope_id).to_owned(),
            adapter_kind: (*adapter_kind).to_owned(),
            input_hash: (*input_hash).to_owned(),
        }),
        [scope_id, adapter_kind, input_hash, tool_profile_hash] => Ok(AbandonSelector::TaskKey(
            LedgerTaskKey::new(*scope_id, *adapter_kind, *input_hash, *tool_profile_hash),
        )),
        _ => Err(KioError::invalid_usage(
            "selector must be an intent_token or scope_id/adapter_kind/input_hash[/tool_profile_hash]",
        )),
    }
}

/// Batch mutations need explicit `--yes` in non-interactive mode. A piped
/// response is never an approval: it is not reviewable at the terminal.
fn confirm_batch_mutation(ctx: &Invocation<'_>, prompt: &str, yes: bool) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !ctx.interaction.is_interactive() {
        return Err(batch_confirmation_rejected(
            "batch mutation requires --yes in non-interactive mode",
        ));
    }
    if confirm_batch_action(ctx, &format!("{prompt} [y/N]"))? {
        Ok(())
    } else {
        Err(batch_confirmation_rejected(
            "batch mutation confirmation was rejected",
        ))
    }
}

/// Shared terminal prompt for commands whose individual handlers decide
/// whether non-interactive confirmation is permitted.
fn confirm_batch_action(ctx: &Invocation<'_>, prompt: &str) -> Result<bool> {
    ctx.interaction.confirm(prompt)
}

/// 06 §1 requires a confirmation prompt before `repair verify-objects
/// --prune-orphans` and `repair registry-prune` delete anything. Both first
/// run their walk in preview mode, so `count` is what WOULD be removed.
///
/// Nothing to remove is an idempotent success with no prompt — the same
/// "対象なしの冪等成功" convention `kio adapter revoke` uses.
/// How many targets the prompt spells out before collapsing the rest to a
/// count. Enough to make a small prune fully reviewable without turning a large
/// one into an unreadable wall.
const REPAIR_PRUNE_PROMPT_MAX_LINES: usize = 20;

/// 06 §1's confirmation for a destructive `repair` operation.
///
/// H2-3 (R24b, 3/3 系統一致): the spec says 削除対象を先に列挙して見せてから問う
/// — show WHAT will be deleted, then ask. This used to take only a count, so
/// the user approved a number and never saw the objects behind it.
pub(crate) fn confirm_repair_prune(
    ctx: &Invocation<'_>,
    what: &str,
    targets: &[String],
    yes: bool,
) -> Result<()> {
    let count = targets.len() as u64;
    if count == 0 || yes {
        return Ok(());
    }
    if !ctx.interaction.is_interactive() {
        return Err(KioError::new(
            "KIO-E-CONFIRM-REJECTED-001",
            format!("repair {what} requires confirmation; pass --yes in non-interactive mode"),
            json!({ "target_count": count }),
            ExitCode::ConfirmationRejected,
        ));
    }
    let mut preview = format!("repair {what} will permanently remove {count} item(s):\n");
    for line in targets.iter().take(REPAIR_PRUNE_PROMPT_MAX_LINES) {
        preview.push_str("  ");
        preview.push_str(line);
        preview.push('\n');
    }
    if let Some(rest) = count.checked_sub(REPAIR_PRUNE_PROMPT_MAX_LINES as u64)
        && rest > 0
    {
        preview.push_str(&format!("  … and {rest} more\n"));
    }
    preview.push_str("Proceed?");
    if confirm_batch_action(ctx, &preview)? {
        return Ok(());
    }
    Err(KioError::new(
        "KIO-E-CONFIRM-REJECTED-001",
        format!("repair {what} confirmation was rejected"),
        json!({ "target_count": count }),
        ExitCode::ConfirmationRejected,
    ))
}

fn batch_confirmation_rejected(message: &str) -> KioError {
    KioError::new(
        "KIO-E-BATCH-CONFIRMATION-REJECTED-001",
        message,
        json!({}),
        ExitCode::ConfirmationRejected,
    )
}

/// `kio batch abandon <selector>` (06 §1 / 04 §5.8 恒久 unknown 脱出路,
/// CL62-CL68). Runs inside `run_batch`'s already-acquired `.kio/.lock`.
fn run_batch_abandon(ctx: &Invocation<'_>, selector_input: &str, yes: bool) -> Result<Value> {
    let selector = parse_batch_selector(selector_input)?;
    let ledger = open_ledger_db(ctx.control)?;
    let resolution = resolve_abandon_selector(&ledger, &selector).map_err(pipeline_to_kio)?;
    match resolution {
        // CL66: no matching row (terminal-confirmed / device-row pruned /
        // already abandoned-and-cleaned / never existed) is an idempotent
        // exit-0 success — no confirmation prompt needed, nothing to abandon.
        AbandonResolution::NotFound => project_legacy(CommandResponse {
            body: ResponseBody::Batch(BatchResponse::Abandon(BatchSelectionResponse::NoTarget(
                BatchNoTargetResponse {
                    status: "no_target".to_owned(),
                },
            ))),
            completion: Completion::Complete,
        }),
        AbandonResolution::Ambiguous => Err(KioError::invalid_usage(
            "abandon selector is ambiguous (multiple tool_profile_hash rows match this \
             scope/adapter/input_hash) — specify the intent_token or the full 4-tuple",
        )),
        AbandonResolution::Found(key) => {
            confirm_batch_mutation(
                ctx,
                &format!(
                    "Abandon in-flight batch request for {}/{}/{}?",
                    key.scope_id, key.adapter_kind, key.input_hash
                ),
                yes,
            )?;
            let execution = execute_abandon(&ledger, &key).map_err(pipeline_to_kio)?;
            project_legacy(CommandResponse {
                body: ResponseBody::Batch(BatchResponse::Abandon(
                    BatchSelectionResponse::Selected(BatchSelectedResponse {
                        status: (match execution {
                            AbandonExecution::NoTarget => "no_target",
                            AbandonExecution::Abandoned => "abandoned",
                        })
                        .to_owned(),
                        scope_id: key.scope_id,
                        adapter_kind: key.adapter_kind,
                        input_hash: key.input_hash,
                    }),
                )),
                completion: Completion::Complete,
            })
        }
    }
}

/// `kio batch retry --reset-violations <selector>` (06 §1 L26-30, §M note-6).
fn run_batch_reset_violations(
    ctx: &Invocation<'_>,
    selector_input: &str,
    yes: bool,
) -> Result<Value> {
    let selector = parse_batch_selector(selector_input)?;
    let ledger = open_ledger_db(ctx.control)?;
    let resolution = resolve_abandon_selector(&ledger, &selector).map_err(pipeline_to_kio)?;
    match resolution {
        AbandonResolution::NotFound => project_legacy(CommandResponse {
            body: ResponseBody::Batch(BatchResponse::ResetViolations(
                BatchSelectionResponse::NoTarget(BatchNoTargetResponse {
                    status: "no_target".to_owned(),
                }),
            )),
            completion: Completion::Complete,
        }),
        AbandonResolution::Ambiguous => Err(KioError::invalid_usage(
            "--reset-violations selector is ambiguous (multiple tool_profile_hash rows match \
             this scope/adapter/input_hash) — specify the intent_token or the full 4-tuple",
        )),
        AbandonResolution::Found(key) => {
            confirm_batch_mutation(
                ctx,
                &format!(
                    "Reset contract_violation_count for {}/{}/{}?",
                    key.scope_id, key.adapter_kind, key.input_hash
                ),
                yes,
            )?;
            // §M note-6: count==0 (or an in-flight state 0/1 row, excluded by
            // the SQL) is a no-op success; only a terminal, non-zero-count row
            // actually changes.
            let did_reset = reset_contract_violations(&ledger, &key).map_err(pipeline_to_kio)?;
            project_legacy(CommandResponse {
                body: ResponseBody::Batch(BatchResponse::ResetViolations(
                    BatchSelectionResponse::Selected(BatchSelectedResponse {
                        status: (if did_reset { "reset" } else { "unchanged" }).to_owned(),
                        scope_id: key.scope_id,
                        adapter_kind: key.adapter_kind,
                        input_hash: key.input_hash,
                    }),
                )),
                completion: Completion::Complete,
            })
        }
    }
}

/// Explicitly authorize one fresh synchronous request after an unknown result.
/// This command never contacts an adapter: it only changes the ledger's
/// one-shot fence and requeues the exact current task for a later online pass.
fn run_batch_resend_unknown(
    ctx: &Invocation<'_>,
    repo: &Repository,
    selector_input: &str,
    yes: bool,
) -> Result<Value> {
    let adapter_settings = ctx.settings;

    let selector = parse_batch_selector(selector_input)?;
    let ledger = open_ledger_db(ctx.control)?;
    let resolution = resolve_abandon_selector(&ledger, &selector).map_err(pipeline_to_kio)?;
    let key = match resolution {
        AbandonResolution::Found(key) => key,
        other => {
            return match other {
                AbandonResolution::NotFound => project_legacy(CommandResponse {
                    body: ResponseBody::Batch(BatchResponse::ResendUnknown(
                        BatchResendResponse::NoTarget(BatchNoTargetResponse {
                            status: "no_target".to_owned(),
                        }),
                    )),
                    completion: Completion::Complete,
                }),
                AbandonResolution::Ambiguous => Err(KioError::invalid_usage(
                    "--resend-unknown selector is ambiguous; specify the intent_token or full 4-tuple",
                )),
                AbandonResolution::Found(_) => unreachable!(),
            };
        }
    };
    let current_scope_id = repo.scope_identity()?.scope_id;
    if key.scope_id != current_scope_id {
        return Err(KioError::invalid_usage(
            "--resend-unknown selector does not belong to the current repository scope",
        ));
    }
    let markdown_profile = online_markdownize_profile_for(repo)?;
    let embedding_profile =
        embedding_execution(adapter_settings, ctx.control).map(declared_embedding_profile);
    let store = TaskStore::new_with_control(repo.kio_dir(), repo.control().clone());
    let tasks = store.all().map_err(pipeline_to_kio)?;
    let matching_task_ids = match key.adapter_kind.as_str() {
        "markdownize" if key.tool_profile_hash == markdown_profile.tool_profile_hash => tasks
            .iter()
            .filter(|task| {
                task.task_type == TaskType::Markdownize
                    && task.input_hash == key.input_hash
                    && task.status == TaskStatus::Failed
                    && task.fallback_reason.as_deref() == Some(RESULT_UNKNOWN_REASON)
                    && targets_standard_online_markdownize(
                        store.kio_dir(),
                        task,
                        &online_output_ref(&markdown_profile.adapter_id),
                    )
                    && matches!(
                        classify_online_markdownize_precondition(repo, task),
                        OnlineMarkdownizePrecondition::Send(_)
                    )
            })
            .map(|task| task.task_id.clone())
            .collect::<Vec<_>>(),
        "embedding"
            if embedding_profile
                .as_ref()
                .is_some_and(|profile| profile.profile_hash == key.tool_profile_hash) =>
        {
            current_embedding_unknown_group_task_ids(
                repo,
                &key,
                embedding_profile
                    .as_ref()
                    .expect("profile was checked immediately above"),
                &tasks,
            )?
        }
        _ => {
            return Err(KioError::invalid_usage(
                "--resend-unknown selector does not match an active adapter profile in this scope",
            ));
        }
    };
    if matching_task_ids.is_empty() {
        return Err(KioError::invalid_usage(
            "--resend-unknown requires a current eligible result_unknown task matching the selected ledger row",
        ));
    }
    if key.adapter_kind == "markdownize" && matching_task_ids.len() != 1 {
        return Err(KioError::invalid_usage(
            "--resend-unknown found multiple current markdownize tasks for one ledger row; reconcile duplicate tasks before authorizing a fresh request",
        ));
    }
    confirm_batch_mutation(
        ctx,
        &format!(
            "Authorize a fresh charged sync request after an unknown prior result for {}/{}/{}?",
            key.scope_id, key.adapter_kind, key.input_hash
        ),
        yes,
    )?;
    // Audit precedes the ledger CAS transition. If the process dies before it,
    // no fresh send is authorized; if it dies after it, the idempotent ledger
    // marker lets this exact task-store update safely replay.
    ctx.control.append_event_log_in_session(
        ctx.locks,
        "KIO-I-SYNC-RESULT-UNKNOWN-RESEND-001",
        "operator authorized one fresh sync request after unknown result",
        json!({
            "scope_id": key.scope_id,
            "adapter_kind": key.adapter_kind,
            "input_hash": key.input_hash,
            "tool_profile_hash": key.tool_profile_hash,
            "task_count": matching_task_ids.len(),
        }),
    )?;
    let newly_authorized = authorize_unknown_sync_resend(&ledger, &key).map_err(pipeline_to_kio)?;
    let matching = matching_task_ids.into_iter().collect::<BTreeSet<_>>();
    let queued = store
        .update_matching(|task| {
            if !matching.contains(&task.task_id) {
                return false;
            }
            task.status = TaskStatus::Pending;
            task.fallback_reason = Some(
                if task.task_type == TaskType::Markdownize {
                    SYNC_RESEND_AUTHORIZED_REASON
                } else {
                    "ready_for_online_adapter"
                }
                .to_owned(),
            );
            task.hold_reason = None;
            task.next_retry_at = None;
            task.heartbeat_at = None;
            task.clear_reservation();
            true
        })
        .map_err(pipeline_to_kio)?;
    project_legacy(CommandResponse { body: ResponseBody::Batch(BatchResponse::ResendUnknown(BatchResendResponse::Authorized(BatchResendAuthorizedResponse {
        status: "resend_authorized".to_owned(), authorization_created: newly_authorized, tasks_queued: queued,
        scope_id: key.scope_id, adapter_kind: key.adapter_kind, input_hash: key.input_hash,
        next_step: "run an explicit online resume; it will perform a fresh reservation and current policy, grant, and budget checks before any provider request".to_owned(),
        warning: "the prior synchronous request may already have been charged".to_owned(),
    }))), completion: Completion::Complete })
}

/// Resolve the sole current embedding content group attached to a result-unknown
/// ledger row. The ledger input hash is the group's canonical embedding hash
/// (text + filename context + active profile), not a representative task's text
/// hash. Recompute that identity from the current retained, policy-allowed chunks;
/// never infer it from a task-input hash prefix or a stale task record.
fn current_embedding_unknown_group_task_ids(
    repo: &Repository,
    key: &LedgerTaskKey,
    profile: &DeclaredEmbeddingProfile,
    tasks: &[TaskDescriptor],
) -> Result<Vec<String>> {
    let db_path = sqlite_path(repo.kio_dir());
    if !db_path.exists() {
        return Err(KioError::invalid_usage(
            "--resend-unknown embedding target has no current source index",
        ));
    }
    let Some(head) = repo.head_commit_hash()? else {
        return Err(KioError::invalid_usage(
            "--resend-unknown embedding target has no current HEAD",
        ));
    };
    let conn = open_existing_source_index_connection(
        &db_path,
        ExistingSourceIndexOpenMode::ReadOnly,
        &FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        },
    )
    .map_err(index_to_kio)?;
    let chunking_config_hash = snapshot_chunking_config_hash(repo, &head)?;
    let retained_instances = retained_history_instances(repo, &head)?;
    let policy = current_embedding_policy(repo)?;
    let retained = retained_history_chunks(
        &conn,
        repo,
        &retained_instances,
        &chunking_config_hash,
        Some(&policy),
    )?;
    let retained = bind_embedding_work(retained, profile)?;
    let pending = retained_chunks_without_embedding(&conn, retained, profile)?;
    let plan = plan_embed_batch(&conn, profile, &pending)
        .map_err(|_| KioError::invalid_usage("cannot establish current embedding content group"))?;

    let mut tasks_by_ref = BTreeMap::<String, Vec<&TaskDescriptor>>::new();
    for task in tasks
        .iter()
        .filter(|task| task.task_type == TaskType::Embedding)
    {
        tasks_by_ref
            .entry(task.output_ref.clone())
            .or_default()
            .push(task);
    }
    let mut candidate_groups = Vec::<Vec<String>>::new();
    for group in plan.to_send {
        if group.embedding_hash != key.input_hash {
            continue;
        }
        let mut group_unknown = Vec::new();
        let mut well_formed = true;
        for member in group.members {
            let output_ref = embedding_task_output_ref(member);
            let Some(member_tasks) = tasks_by_ref.get(&output_ref) else {
                well_formed = false;
                break;
            };
            // Duplicate task rows make the representative and its charge owner
            // non-canonical. Do not choose one implicitly for a paid recovery.
            if member_tasks.len() != 1 {
                well_formed = false;
                break;
            }
            let task = member_tasks[0];
            if task.input_hash != member.text_hash
                || task.changed_unit_keys != [member.chunk_id.clone()]
            {
                return Err(KioError::schema(
                    "embedding recovery task does not match authenticated work",
                ));
            }
            if task.status == TaskStatus::Failed
                && task.fallback_reason.as_deref() == Some(RESULT_UNKNOWN_REASON)
            {
                group_unknown.push(task.task_id.clone());
            }
        }
        if well_formed && !group_unknown.is_empty() {
            candidate_groups.push(group_unknown);
        }
    }
    match candidate_groups.len() {
        1 => Ok(candidate_groups.pop().expect("one group was checked")),
        0 => Err(KioError::invalid_usage(
            "--resend-unknown requires one current eligible embedding content group",
        )),
        _ => Err(KioError::invalid_usage(
            "--resend-unknown embedding ledger key matches multiple current content groups; refuse ambiguous fresh authorization",
        )),
    }
}

/// `kio status`'s stalled-batch section (CL37/CL68): permanently-unresolvable
/// `batch_requests` rows (settled but residue cleanup still pending), each with
/// its `intent_token` so it can be passed straight to `kio batch abandon`.
pub(crate) fn stalled_batch_status(
    ledger: Option<&LedgerReadSnapshot>,
    control: &ControlHandle,
) -> Result<Vec<StalledBatchStatus>> {
    let Some(ledger) = ledger else {
        return Ok(Vec::new());
    };
    let ledger_path = ledger_db_path(control);
    let rows = ledger
        .stalled_rows()
        .map_err(|error| ledger_snapshot_to_kio(&ledger_path.display().to_string(), error))?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let issued_at_ms = row
                .intent_token
                .as_deref()
                .and_then(uuid_v7_timestamp_millis);
            StalledBatchStatus {
                scope_id: row.key.scope_id,
                adapter_kind: row.key.adapter_kind,
                input_hash: row.key.input_hash,
                tool_profile_hash: row.key.tool_profile_hash,
                intent_token: row.intent_token,
                error: row.error,
                estimated_usd: row.estimated_usd,
                issued_at_ms,
            }
        })
        .collect())
}

/// Q3: reclaim orphaned `Running` tasks back to `Pending`. A task is flipped to
/// `Running` only by `execute_pending_markdownize_tasks`, immediately before an
/// online send, and back to Done/Partial/Failed once it returns. Kio is
/// single-user and every executor (`batch resume` / `batch retry` / index
/// enrichment) holds the folder store lock end-to-end, so any `Running` task
/// observed while holding that lock is necessarily an orphan from a process that
/// died mid-send (`Running` had no outbound transition, an absorbing state).
/// Reclaim it to `Pending` — clearing the stale `heartbeat_at` — so it is retried
/// instead of being stuck forever and dragging `enriched_ratio` below 1.0.
pub(crate) fn reclaim_orphaned_running_tasks(store: &TaskStore) -> Result<usize> {
    store
        .update_matching(|task| {
            if task.status == TaskStatus::Running {
                task.status = TaskStatus::Pending;
                task.heartbeat_at = None;
                true
            } else {
                false
            }
        })
        .map_err(pipeline_to_kio)
}

/// R9-4 / R10-4: convert incomplete, known online Markdownize results back into
/// retryable Pending tasks. Both Partial and all-Failed responses are normalized
/// immutable instances whose manifest records each failed unit's real `error_kind`.
/// Recovery uses that unit plan only: generic Failed-task retry must never flatten a
/// known normalized response into a synthetic task-level failure. A non-retryable unit
/// (invalid_input / ...) remains terminal; finite budgets stop repeated paid requests,
/// while the explicitly declared per-unit `rate_limit` policy remains unlimited.
fn reenqueue_incomplete_markdownize_tasks(store: &TaskStore) -> Result<usize> {
    let online_ref = online_output_ref(&online_markdownize_profile().adapter_id);
    // Collect each Partial task's retry plan first — this needs manifest I/O, so it
    // is done outside the `update_matching` closure.
    let mut plan_by_id: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for task in store.all().map_err(pipeline_to_kio)? {
        if !matches!(task.status, TaskStatus::Partial | TaskStatus::Failed)
            || task.task_type != TaskType::Markdownize
            || task.fallback_reason.as_deref() != Some("online_adapter_done")
            || !targets_standard_online_markdownize(store.kio_dir(), &task, &online_ref)
        {
            continue;
        }
        let plan = partial_retry_plan_from_instance(store, &task)?;
        // Gate 1 (R10-4): no retryable Failed unit -> leave the task incomplete.
        if plan.retryable_units.is_empty() {
            continue;
        }
        // Gate 2 (R10-4): the retry budget for the governing kind is spent -> leave
        // the task incomplete (static). `None` means at least one retryable unit has an
        // unlimited budget, so the cap never trips.
        if plan.max_attempts.is_some_and(|max| task.attempts >= max) {
            continue;
        }
        plan_by_id.insert(task.task_id.clone(), plan.retryable_units);
    }
    if plan_by_id.is_empty() {
        return Ok(0);
    }
    store
        .update_matching(|task| {
            let Some(retryable) = plan_by_id.get(&task.task_id) else {
                return false;
            };
            task.status = TaskStatus::Pending;
            task.unit_keys = Some(retryable.clone());
            // Preserve the typed normalized-instance reference. The retry precondition
            // needs its complete manifest to recover every provider-discovered unit;
            // replacing this with the online placeholder made OCR-from-scratch partial
            // tasks impossible to resume.
            task.fallback_reason = None;
            task.next_retry_at = None;
            // R10-4/R19-5: the re-drive is charged against the retry budget in the
            // executor's result handling (a single accounting point) — on `Err` (full
            // send failure) OR `Ok(Partial)` (units still missing). Pre-incrementing here
            // double-counted with the `Err` path, exhausting `max_attempts` twice as fast.
            true
        })
        .map_err(pipeline_to_kio)
}

/// The retryable subset of a Partial normalized instance's Failed units plus the
/// governing retry budget (R10-4). Used by `reenqueue_incomplete_markdownize_tasks`.
pub(crate) struct PartialRetryPlan {
    /// Failed unit_keys whose recorded `error_kind` is retryable (docs/04 §5.2).
    pub(crate) retryable_units: Vec<String>,
    /// The smallest finite `max_attempts` among those units' kinds, or `None` when at
    /// least one kind has an unlimited retry budget.
    pub(crate) max_attempts: Option<u32>,
}

/// Read a normalized instance's manifest (`output_ref/manifest.json`) and build its
/// [`PartialRetryPlan`]: the still-Failed units filtered to the retryable ones (by
/// their recorded `error_kind`) and the retry-budget cap. A missing manifest or a
/// fully-Done instance yields an empty plan.
pub(crate) fn partial_retry_plan_from_instance(
    task_store: &TaskStore,
    task: &TaskDescriptor,
) -> Result<PartialRetryPlan> {
    let empty = PartialRetryPlan {
        retryable_units: Vec::new(),
        max_attempts: Some(0),
    };
    let identity = validate_task_output_ref(task_store.kio_dir(), task).map_err(pipeline_to_kio)?;
    let TaskOutputRef::NormalizedInstance {
        raw_hash,
        tool_profile_hash,
        r#gen,
        ..
    } = identity
    else {
        return Ok(empty);
    };
    let manifest = load_validated_normalized_instance_with_control(
        task_store.kio_dir(),
        &raw_hash,
        &tool_profile_hash,
        r#gen,
        task_store.control(),
    )
    .map_err(pipeline_to_kio)?
    .manifest;
    Ok(partial_retry_plan_from_manifest(&manifest, task.attempts))
}

/// Select only units that still have their own retry budget remaining. The task
/// attempt counter is shared across a unit-scoped task, so it is conservative:
/// an exhausted finite unit is excluded even when a sibling has an unlimited
/// policy. This prevents a `rate_limit` sibling from extending NetworkError's
/// declared five-attempt budget without adding new per-unit mutable state.
pub(crate) fn partial_retry_plan_from_manifest(
    manifest: &NormalizedInstanceManifest,
    task_attempts: u32,
) -> PartialRetryPlan {
    let mut retryable_units = Vec::new();
    let mut finite_max: Option<u32> = None;
    let mut saw_unlimited = false;
    for entry in &manifest.units {
        if entry.status == UnitStatus::Done {
            continue;
        }
        let policy = retry_policy(retry_kind_from_reason(entry.error_kind.as_deref()));
        if !policy.retryable || policy.max_attempts.is_some_and(|max| task_attempts >= max) {
            continue;
        }
        retryable_units.push(entry.unit_key.clone());
        match policy.max_attempts {
            None => saw_unlimited = true,
            Some(max) => finite_max = Some(finite_max.map_or(max, |current| current.min(max))),
        }
    }
    let max_attempts = if retryable_units.is_empty() {
        Some(0)
    } else if saw_unlimited {
        None
    } else {
        finite_max
    };
    PartialRetryPlan {
        retryable_units,
        max_attempts,
    }
}
