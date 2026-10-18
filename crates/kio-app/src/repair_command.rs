use crate::batch::command::confirm_repair_prune;
use crate::commands::RepairArgs;
use crate::commands::RepairOperation;
use crate::egress::embedding_online_allowed;
use crate::embedding::admission::LaneOverride;
use crate::embedding::admission::declared_embedding_profile;
use crate::embedding::admission::embedding_execution;
use crate::embedding::admission::resolve_invocation_lane;
use crate::embedding::batch::poll_batch_embedding_jobs;
use crate::embedding::executor::run_embedding_enrichment;
use crate::index::chunk_ledger::ensure_purge_epoch_initialized;
use crate::index::chunk_ledger::recover_index_generation;
use crate::index::rebuild::rebuild_step3_index;
use crate::index::rebuild::skipped_disclosure;
use crate::invocation::Invocation;
use crate::invocation::non_success_error_code;
use crate::ledger_access::open_optional_ledger_db;
use crate::ledger_access::recover_stale_sync_rows;
use crate::managed_restore;
use crate::management;
use crate::markdownize::batch::poll_batch_markdownize_jobs;
use crate::object_persistence::index_to_kio;
use crate::promotion::clear_promotion_state;
use crate::promotion::maybe_inject_promotion_fault;
use crate::promotion::recover_pending_online_promotion;
use crate::response::repair::*;
use crate::response::{CommandResponse, Completion, ResponseBody};
use crate::scope_access::scope_id;
use crate::search::projection::log_aggregator_degraded;
use crate::search::projection::mark_replica_rebuilding_or_log;
use crate::search::projection::mark_replica_unavailable_or_log;
use crate::search::projection::materialize_repaired_snapshot_entries;
use crate::search::projection::write_through_projection;
use crate::search::projection::write_through_projection_or_log_for_repo;
use crate::search::scope::aggregator_path;
use crate::settings::rebuild_embedding_profile_from_tool_lock;
use crate::settings::validate_repo_tool_lock;
use crate::task_execution::ExecOutcome;
use crate::task_execution::enrichment_completion;
use crate::verify_objects;
use kio_core::ExitCode;
use kio_core::KioError;
use kio_core::Result;
use kio_core::purge::PurgeState;
use kio_core::scope::Repository;
use kio_index::registry::RegistryDb;
use kio_index::registry::RegistryEntry;
use kio_pipeline::task::TaskStore;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::PathBuf;

/// R24b (3 系統一致): `--yes` only means anything alongside `--prune-orphans`;
/// accepting it alone would let a user believe they had pre-authorized a prune
/// this invocation never performs. clap cannot express the constraint between
/// two boolean flags (see `RepairVerifyObjectsArgs::yes`), so it lives here.
///
/// Kept out of `run_repair`'s body so the store lock stays within the window
/// QB11 (`step4b_p3b_contract.rs`) scans for.
fn reject_inert_repair_yes(args: &RepairArgs) -> Result<()> {
    if let RepairOperation::VerifyObjects(verify) = &args.operation
        && verify.yes
        && !verify.prune_orphans
    {
        return Err(KioError::invalid_usage(
            "repair verify-objects --yes requires --prune-orphans",
        ));
    }
    Ok(())
}

pub(crate) fn run_repair(ctx: &Invocation<'_>, args: RepairArgs) -> Result<CommandResponse> {
    if matches!(&args.operation, RepairOperation::VerifyObjects(_)) {
        return kio_core::cas::VerifiedCasReadBudget::new()
            .run(u64::MAX, |reads| {
                run_repair_with_reads(ctx, args, Some(reads))
            })
            .map(|(value, _)| value);
    }
    run_repair_with_reads(ctx, args, None)
}

fn run_repair_with_reads(
    ctx: &Invocation<'_>,
    args: RepairArgs,
    reads: Option<&kio_core::cas::VerifiedCasReadCapability>,
) -> Result<CommandResponse> {
    reject_inert_repair_yes(&args)?;
    if let RepairOperation::CancelChildInitialization(cancel) = args.operation {
        if cancel.preview == cancel.yes || (cancel.yes && cancel.operation.is_none()) {
            return Err(KioError::invalid_usage(
                "repair cancel-child-initialization requires exactly one of --preview or --yes; --yes requires --operation",
            ));
        }
        let working_directory = ctx.cwd().to_path_buf();
        let root = cancel
            .path
            .map(|path| working_directory.join(path))
            .unwrap_or(working_directory);
        let parent =
            management::open_existing_managed_root(&root, ctx.control)?.in_lock_session(ctx.locks);
        return management::cancel_child_initialization(
            &parent,
            cancel.preview,
            cancel.operation.as_deref(),
        );
    }
    if matches!(args.operation, RepairOperation::RecoverRestore) {
        return managed_restore::recover(ctx);
    }
    if matches!(
        args.operation,
        RepairOperation::All | RepairOperation::Replica
    ) {
        return run_device_repair(ctx, matches!(args.operation, RepairOperation::All));
    }
    let parsed = parsed_repair(&args);
    let mode = parsed.mode;
    // PB25: `registry-prune` operates on the device-global scope-registry,
    // not any one scope's `.kio` — it must not require the CWD to be inside a
    // scope at all (unlike every other repair mode).
    if let RepairOperation::RegistryPrune(prune) = &args.operation {
        let plan = verify_objects::registry_prune_plan_with_control(ctx.control)?;
        confirm_repair_prune(ctx, "registry-prune", &plan.target_lines(), prune.yes)?;
        let report = verify_objects::registry_prune_apply_with_control(&plan, ctx.control)?;
        return Ok(CommandResponse {
            body: ResponseBody::Repair(RepairResponse::RegistryPrune(report)),
            completion: Completion::Complete,
        });
    }
    let repo = match Repository::open_recovery_in_session(ctx.cwd(), ctx.control.clone(), ctx.locks)
    {
        Ok(repo) => repo,
        Err(error)
            if matches!(args.operation, RepairOperation::VerifyObjects(_))
                && error.error_code() == "KIO-E-STORE-NAMESPACE-001" =>
        {
            let report = verify_objects::VerifyObjectsReport::namespace_unavailable(&error);
            return Ok(CommandResponse {
                body: ResponseBody::Repair(RepairResponse::VerifyObjects(RepairVerifyResponse {
                    report,
                    prune_orphans: None,
                    error_code: Some("KIO-E-STORE-NAMESPACE-001".to_owned()),
                })),
                completion: Completion::Partial,
            });
        }
        Err(error) => return Err(error),
    };
    let repo = match reads {
        Some(reads) => repo.with_verified_reads(reads)?,
        None => repo,
    };
    if let RepairOperation::RebuildDb(rebuild) = &args.operation {
        // `rebuild-db`'s post-rebuild enrichment can send, so it takes the same
        // invocation lane every other sending command does — scope config
        // included, which is why this waits for the repository.
        resolve_invocation_lane(
            LaneOverride::new(rebuild.realtime, rebuild.batch),
            Some(&repo.kio_dir().join("config.toml")),
            ctx.control,
        )?;
    }
    // `repair verify-objects` is the recovery observer for an interrupted
    // receipt-first GC sweep. It must report the bounded incomplete state,
    // not take the ordinary writer lock (whose shared active-GC barrier is
    // correctly used by all mutation-capable repair modes). `verify_objects`
    // itself returns before any repair when this marker is present.
    if matches!(
        mode,
        RepairMode::VerifyObjects | RepairMode::VerifyObjectsPruneOrphans
    ) && kio_core::gc::read_active_marker_with_control(repo.kio_dir(), repo.control())?.is_some()
    {
        let report = verify_objects::verify_objects(&repo)?;
        return Ok(CommandResponse {
            body: ResponseBody::Repair(RepairResponse::VerifyObjects(RepairVerifyResponse {
                report,
                prune_orphans: None,
                error_code: Some("KIO-E-GC-SWEEP-ACTIVE-001".to_owned()),
            })),
            completion: Completion::Partial,
        });
    }
    // M1(a): serialize the DB rebuild against concurrent index/repair/reindex.
    let _lock = repo.lock_store_in_session(repo.lock_session())?;
    if mode == RepairMode::VerifyObjectsPruneOrphans
        && let Some(prune) = verify_objects::prune_orphans_preflight(&repo)?
    {
        return Ok(CommandResponse {
            body: ResponseBody::Repair(RepairResponse::PruneBlocked(RepairPruneBlockedResponse {
                prune_orphans: prune,
                error_code: "KIO-E-PRUNE-ORPHANS-BLOCKED-001".to_owned(),
            })),
            completion: Completion::Partial,
        });
    }
    repo.recover_publication()?;
    validate_repo_tool_lock(&repo)?;
    if mode == RepairMode::VerifyObjects || mode == RepairMode::VerifyObjectsPruneOrphans {
        let report = verify_objects::verify_objects(&repo)?;
        let projection_ready = if let Some(repaired_head) = report.repaired_commit_hash.as_deref() {
            // A repaired commit keeps the tree but advances HEAD. Mark the old
            // replica corpus unavailable first; only publish Ready after the
            // source cache gains bindings for the new commit identity.
            mark_replica_rebuilding_or_log(ctx, repo.kio_dir(), repaired_head);
            match materialize_repaired_snapshot_entries(&repo, repaired_head) {
                Ok(()) => true,
                Err(error) => {
                    log_aggregator_degraded(
                        ctx,
                        &format!("repair projection materialize {repaired_head} failed: {error}"),
                    );
                    false
                }
            }
        } else {
            true
        };
        if projection_ready {
            // Verification can recover a raw object and record a repaired
            // commit, which advances HEAD without taking the rebuild path
            // below. The replica write remains a non-fatal cache operation.
            write_through_projection_or_log_for_repo(ctx, &repo);
        }
        let has_findings = report.has_remaining_findings();
        let purge_incomplete = report
            .remaining_findings
            .iter()
            .any(|finding| finding.kind == "purge_incomplete");
        let mut output = RepairVerifyResponse {
            report,
            prune_orphans: None,
            error_code: None,
        };
        let mut completion = Completion::Complete;
        if has_findings {
            output.error_code = Some(
                if purge_incomplete {
                    "KIO-E-PURGE-INCOMPLETE-001"
                } else {
                    "KIO-E-STORE-CORRUPT-001"
                }
                .to_owned(),
            );
            // PB15: never prune on top of an unverified/corrupt store.
            return Ok(CommandResponse {
                body: ResponseBody::Repair(RepairResponse::VerifyObjects(output)),
                completion: Completion::Partial,
            });
        }
        // PB12-17: `--prune-orphans` runs only after a clean verify pass.
        if mode == RepairMode::VerifyObjectsPruneOrphans {
            let skip_prompt = matches!(
                &args.operation,
                RepairOperation::VerifyObjects(verify) if verify.yes
            );
            // H2-4 (R24b, 3/3 一致・2 件 fatal): the plan the user approves IS
            // the set that gets deleted. This used to count with one call and
            // delete with a second that re-derived the targets, so anything
            // that became an orphan in between was removed without ever having
            // been shown. A blocked plan also stops here rather than falling
            // through to a destructive call that might no longer be blocked.
            let plan = verify_objects::prune_orphans_plan(&repo)?;
            if !plan.is_blocked() {
                confirm_repair_prune(
                    ctx,
                    "verify-objects --prune-orphans",
                    &plan.target_lines(),
                    skip_prompt,
                )?;
            }
            let prune = verify_objects::prune_orphans_apply(&repo, plan)?;
            if prune.status == "blocked" {
                output.error_code = Some("KIO-E-PRUNE-ORPHANS-BLOCKED-001".to_owned());
                completion = Completion::Partial;
            }
            output.prune_orphans = Some(prune);
        }
        return Ok(CommandResponse {
            body: ResponseBody::Repair(RepairResponse::VerifyObjects(output)),
            completion,
        });
    }
    let (response, completion) =
        run_rebuild_db_locked(ctx, &repo, parsed.online, parsed.offline, true)?;
    Ok(CommandResponse {
        body: ResponseBody::Repair(RepairResponse::RebuildDb(response)),
        completion,
    })
}

/// The locked, source-SQLite half of `repair rebuild-db`.  Keeping it separate
/// lets device-global repair use the identical recovery path while forcing it
/// offline, without widening the scoped command's CLI surface.
fn run_rebuild_db_locked(
    ctx: &Invocation<'_>,
    repo: &Repository,
    online_requested: bool,
    offline: bool,
    poll_remote_jobs: bool,
) -> Result<(RepairRebuildResponse, Completion)> {
    let adapter_settings = ctx.settings;

    // CL45/item 5: reconcile any stale `request_kind='sync'` cost-ledger.sqlite
    // rows left by a crashed prior run — `--rebuild-db` only (CL32/CL45 do not
    // list `--verify-objects`, handled by the early return above).
    if !offline && let Some(ledger) = open_optional_ledger_db(ctx.control)? {
        recover_stale_sync_rows(
            &ledger,
            Some(&TaskStore::new_with_control(
                repo.kio_dir(),
                repo.control().clone(),
            )),
            &scope_id(repo.kio_dir())?,
        )?;
    }
    if poll_remote_jobs {
        // 04 §5.8 相 3: poll/collect in-flight Batch jobs at the scoped
        // write-command entry point. Device-global `repair all` disables this
        // entire block: its offline contract forbids provider polling and
        // remote-residue cleanup as well as new paid sends.
        if let Some(ledger) = open_optional_ledger_db(ctx.control)? {
            poll_batch_markdownize_jobs(
                adapter_settings,
                repo,
                &TaskStore::new_with_control(repo.kio_dir(), repo.control().clone()),
                &ledger,
            )?;
            poll_batch_embedding_jobs(ctx, repo, &ledger)?;
        }
    }
    // LC39/LC40 (see `ensure_purge_epoch_initialized`'s doc comment).
    ensure_purge_epoch_initialized(repo.kio_dir(), repo.control())?;
    let promotion_rebuild_pending = recover_pending_online_promotion(repo)?;
    let db = repo.kio_dir().join("index/sqlite.db");
    // `rebuild_sqlite_index` drops and rebuilds chunks/FTS/tree_entries in place
    // while preserving the `embeddings` rows and re-deriving `chunk_vec` from them
    // (04 §4.3). It is not pre-deleted here so vector search survives the rebuild.
    let report = rebuild_step3_index(ctx, repo)?;
    // LC42-LC44 (item 2), same ordering rationale as `run_index`'s call.
    recover_index_generation(repo.kio_dir(), repo.control())?;
    if promotion_rebuild_pending {
        maybe_inject_promotion_fault("after_index_swap", ctx.control)?;
        clear_promotion_state(repo.kio_dir())?;
    }
    // L1: after a DB rebuild, re-drive enrichment so any chunk lacking an
    // embedding (e.g. a rebuild that produced new chunk rows, or a scope whose
    // enrichment never ran) is enqueued/embedded rather than silently reported as
    // fully enriched. `rebuild_sqlite_index` already preserved existing
    // embeddings, so reuse keeps this near-free; offline it only enqueues.
    // QA29 (step4b-contract-tests-p3a.md §I, 06-cli-spec.md §1 L52-55, 07 §3
    // L220-222): `--online`/`--offline` now reach the post-rebuild enrichment
    // pass instead of the hard-coded `(false, false, false)` this used to
    // pass unconditionally.
    // The rebuilt projection was selected by the persisted current tool-lock,
    // not by whichever adapter happens to be active now.  In particular a
    // text-only lock must stay text-only even if a local adapter is available;
    // and an adapter from another profile must never repopulate the fresh DB
    // with a second vector space.  `index` is the explicit operation that may
    // materialize a new lock/profile.
    let locked_embedding_profile = rebuild_embedding_profile_from_tool_lock(repo.kio_dir())?;
    let enrichment_authorized = locked_embedding_profile.as_ref().is_some_and(|locked| {
        embedding_execution(adapter_settings, ctx.control)
            .map(declared_embedding_profile)
            .is_some_and(|active| {
                active.dimensions == locked.dimensions
                    && active.distance == locked.distance
                    && active.modality == locked.modality
                    && active.profile_hash == locked.profile_hash
            })
    });
    let embedding_online = if enrichment_authorized {
        embedding_online_allowed(adapter_settings, repo, offline, online_requested, false)?
    } else {
        false
    };
    // R11-2: keep the enrichment ExecOutcome (was discarded) — disclose it and let an
    // auth/budget-pause raise the exit while the rebuild JSON still prints to stdout.
    let enrichment = if enrichment_authorized {
        run_embedding_enrichment(ctx, repo, embedding_online, false, false)?
    } else {
        ExecOutcome::default()
    };
    let output = RepairRebuildResponse {
        status: "rebuilt".to_owned(),
        rebuilt_chunks: report.rebuilt_chunks,
        rebuilt_tree_entries: report.rebuilt_tree_entries,
        sqlite_db: db,
        embedding_tasks_executed: enrichment.executed,
        embedding_tasks_failed: enrichment.failed,
        paused_tasks: enrichment.paused,
        skipped: skipped_disclosure(&report, repo.kio_dir()),
    };
    Ok((output, enrichment_completion(&enrichment)))
}

/// Device-global repair deliberately enumerates the registry before opening any
/// scope.  The registry connection is dropped before locks are taken: a repair
/// must never hold the device database while it waits on a scope writer.
fn run_device_repair(ctx: &Invocation<'_>, rebuild_source: bool) -> Result<CommandResponse> {
    let entries = {
        let registry = RegistryDb::open_default_with_control(ctx.control).map_err(index_to_kio)?;
        registry.all_entries().map_err(index_to_kio)?
    };
    let mut entries: Vec<_> = entries.into_iter().filter(|entry| entry.indexed).collect();
    entries.sort_by(|left, right| {
        left.root_path
            .cmp(&right.root_path)
            .then_with(|| left.scope_id.cmp(&right.scope_id))
    });

    let mut failures = Vec::new();
    let mut live = Vec::new();
    for entry in entries {
        match Repository::open_read_only_in_session(
            &entry.root_path,
            ctx.control.clone(),
            ctx.locks,
        )
        .and_then(|repo| validate_device_registry_entry(&entry, &repo).map(|()| repo))
        {
            Ok(repo) => live.push((entry, repo)),
            // Exact-current validation is command-wide, including aggregate
            // repair. Validate every selected store before resetting the
            // replica or repairing a healthy sibling, and never translate a
            // version mismatch into `failed_scopes` partial success.
            Err(error) if error.error_code() == "KIO-E-STORE-VERSION-001" => {
                return Err(error);
            }
            Err(error) => failures.push(device_repair_failure(&entry, "open", &error)),
        }
    }
    // This command is the explicit recovery boundary for the disposable
    // device replica. Reset it once after the command-wide format gate and
    // before projecting any scope so rows from a scope that later fails repair
    // cannot remain searchable.
    kio_index::aggregator::Aggregator::recreate_with_control(
        &aggregator_path(ctx.control),
        ctx.control,
    )
    .map_err(|error| {
        KioError::new(
            "KIO-E-AGGREGATOR-001",
            "the device search replica could not be recreated",
            json!({ "reason": error.to_string() }),
            ExitCode::PartialFailure,
        )
    })?;
    let mut duplicate_ids = BTreeSet::new();
    let mut counts = BTreeMap::new();
    for (entry, _) in &live {
        *counts.entry(entry.scope_id.clone()).or_insert(0usize) += 1;
    }
    for (scope_id, count) in counts {
        if count > 1 {
            duplicate_ids.insert(scope_id);
        }
    }
    let mut successes = Vec::new();
    for (entry, repo) in live {
        if duplicate_ids.contains(&entry.scope_id) {
            let error = KioError::new(
                "KIO-E-REGISTRY-DUP-001",
                "multiple live registry roots share a scope ID",
                json!({ "scope_id": entry.scope_id }),
                ExitCode::PermanentFailure,
            );
            failures.push(device_repair_failure(&entry, "identity", &error));
            continue;
        }
        match run_one_device_repair(
            ctx,
            &repo,
            &entry,
            if rebuild_source {
                DeviceRepairOperation::All
            } else {
                DeviceRepairOperation::Replica
            },
        ) {
            Ok(value) => {
                let status = match &value {
                    ScopeRepairResult::All(output) => output.rebuild.status.clone(),
                    ScopeRepairResult::Replica(output) => output.status.clone(),
                };
                successes.push(RepairedScope {
                    scope_id: entry.scope_id,
                    path: entry.root_path,
                    status,
                    result: value,
                });
            }
            Err(error) if error.error_code() == "KIO-E-STORE-VERSION-001" => {
                return Err(error);
            }
            Err(error) => failures.push(device_repair_failure(
                &entry,
                device_repair_stage(&error, rebuild_source),
                &error,
            )),
        }
    }
    successes.sort_by(|a, b| a.path.cmp(&b.path));
    failures.sort_by(|a, b| a.path.cmp(&b.path));
    let mut output = DeviceRepairResponse {
        status: if failures.is_empty() && successes.is_empty() {
            "up_to_date"
        } else if failures.is_empty() {
            "repaired"
        } else if successes.is_empty() {
            "failed"
        } else {
            "partial"
        }
        .to_owned(),
        operation: if rebuild_source { "all" } else { "replica" }.to_owned(),
        summary: RepairSummary {
            repaired: successes.len(),
            failed: failures.len(),
        },
        repaired_scopes: successes,
        failed_scopes: failures,
        error_code: None,
    };
    let mut completion = Completion::Complete;
    if !output.failed_scopes.is_empty() {
        let failures = &output.failed_scopes;
        let homogeneous = output.repaired_scopes.is_empty()
            && failures.iter().all(|failure| {
                failure.error_code == failures[0].error_code
                    && failure.exit_code == failures[0].exit_code
            });
        if homogeneous {
            let exit = failures[0].exit_code;
            let code = failures[0].error_code.clone();
            return Err(KioError::new(
                code,
                "device repair failed for every registered scope",
                serde_json::to_value(&output)
                    .map_err(|error| KioError::schema(error.to_string()))?,
                exit_code_from_i64(i64::from(exit)),
            ));
        }
        let (code, outcome) = if output.repaired_scopes.is_empty() {
            let retryable = failures
                .iter()
                .any(|failure| matches!(failure.exit_code, 1 | 3 | 5 | 6 | 7));
            (
                "KIO-E-REPAIR-ALL-FAILED-001",
                if retryable {
                    Completion::Partial
                } else {
                    Completion::Permanent
                },
            )
        } else {
            ("KIO-E-REPAIR-PARTIAL-001", Completion::Partial)
        };
        output.error_code = Some(code.to_owned());
        completion = outcome;
    }
    Ok(CommandResponse {
        body: ResponseBody::Repair(RepairResponse::Device(output)),
        completion,
    })
}

fn exit_code_from_i64(code: i64) -> ExitCode {
    match code {
        0 => ExitCode::Success,
        1 => ExitCode::Failure,
        2 => ExitCode::InvalidUsage,
        3 => ExitCode::PartialFailure,
        4 => ExitCode::PermanentFailure,
        5 => ExitCode::AuthError,
        6 => ExitCode::BudgetExceeded,
        7 => ExitCode::Interrupted,
        8 => ExitCode::IncompatibleProfile,
        9 => ExitCode::ConfirmationRejected,
        _ => ExitCode::Failure,
    }
}

fn device_repair_failure(entry: &RegistryEntry, stage: &str, error: &KioError) -> FailedScope {
    FailedScope {
        scope_id: entry.scope_id.clone(),
        path: entry.root_path.clone(),
        stage: stage.to_owned(),
        reason: device_repair_reason(stage, error).to_owned(),
        error_code: error.error_code().to_owned(),
        message: error.message().to_owned(),
        context: error.context().clone(),
        exit_code: error.exit_code().code(),
    }
}

fn validate_device_registry_entry(entry: &RegistryEntry, repo: &Repository) -> Result<()> {
    let actual_scope_id = scope_id(repo.kio_dir())?;
    let registered_root = PathBuf::from(&entry.root_path)
        .canonicalize()
        .map_err(|error| KioError::io(error.to_string(), entry.root_path.clone()))?;
    let registered_kio = PathBuf::from(&entry.kio_path)
        .canonicalize()
        .map_err(|error| KioError::io(error.to_string(), entry.kio_path.clone()))?;
    let actual_root = repo
        .root()
        .canonicalize()
        .map_err(|error| KioError::io(error.to_string(), repo.root().display().to_string()))?;
    let actual_kio = repo
        .kio_dir()
        .canonicalize()
        .map_err(|error| KioError::io(error.to_string(), repo.kio_dir().display().to_string()))?;
    if actual_scope_id != entry.scope_id
        || registered_root != actual_root
        || registered_kio != actual_kio
    {
        return Err(KioError::new(
            "KIO-E-REGISTRY-STALE-001",
            "registered scope does not match the on-disk scope",
            json!({
                "registered_scope_id": entry.scope_id,
                "on_disk_scope_id": actual_scope_id,
                "registered_root_path": entry.root_path,
                "on_disk_root_path": actual_root,
                "registered_kio_path": entry.kio_path,
                "on_disk_kio_path": actual_kio,
            }),
            ExitCode::PermanentFailure,
        ));
    }
    Ok(())
}

fn device_repair_stage(error: &KioError, rebuild_source: bool) -> &'static str {
    match error.error_code() {
        "KIO-E-PURGE-JOURNAL-ACTIVE-001" => "purge_preflight",
        "KIO-E-AGGREGATOR-001" => "replica",
        "KIO-E-REGISTRY-DUP-001" | "KIO-E-REGISTRY-STALE-001" => "identity",
        _ if rebuild_source => "source",
        _ => "replica",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceRepairOperation {
    All,
    Replica,
}

fn device_repair_reason(stage: &str, error: &KioError) -> &'static str {
    match error.error_code() {
        "KIO-E-PURGE-JOURNAL-ACTIVE-001" => "purge_journal_active",
        "KIO-E-REGISTRY-DUP-001" => "registry_duplicate",
        "KIO-E-REGISTRY-STALE-001" => "registry_stale",
        "KIO-E-STORE-LOCKED-001" => "locked",
        "KIO-E-STORE-CORRUPT-001" => "store_corrupt",
        "KIO-E-PURGE-INCOMPLETE-001" => "purge_incomplete",
        "KIO-E-AGGREGATOR-001" => "replica_error",
        "KIO-E-STORE-IO-001" | "KIO-E-CONFIG-USAGE-001" if stage == "open" => "unreachable",
        _ => "repair_failed",
    }
}

fn run_one_device_repair(
    ctx: &Invocation<'_>,
    repo: &Repository,
    entry: &RegistryEntry,
    operation: DeviceRepairOperation,
) -> Result<ScopeRepairResult> {
    let _lock = repo.lock_store_in_session(repo.lock_session())?;
    // Revalidate after taking the lock so a path replaced between registry
    // enumeration and execution cannot be repaired under the stale identity.
    validate_device_registry_entry(entry, repo)?;
    // Fail closed before every source preflight: an old Ready projection must
    // never survive once an explicit global repair has started.
    mark_replica_unavailable_or_log(ctx, repo.kio_dir());
    if let Some(journal) =
        PurgeState::open_with_control(repo.kio_dir(), repo.control().clone())?.read_journal()?
    {
        return Err(KioError::new(
            "KIO-E-PURGE-JOURNAL-ACTIVE-001",
            "device repair is blocked while purge remains incomplete",
            json!({
                "component": "device_repair",
                "purge_id": journal.purge_id,
            }),
            ExitCode::PartialFailure,
        ));
    }
    if operation == DeviceRepairOperation::All {
        repo.recover_publication()?;
        validate_repo_tool_lock(repo)?;
        let verification = verify_objects::verify_objects(repo)?;
        if let Some(repaired) = verification.repaired_commit_hash.as_deref() {
            // `all` rebuilds the source SQLite immediately below, so unlike
            // standalone `verify-objects` it must not require the old cache to
            // materialize bindings for the repaired HEAD. This is what lets
            // `all` recover a missing/corrupt sqlite.db and canonical objects
            // in the same pass.
            mark_replica_rebuilding_or_log(ctx, repo.kio_dir(), repaired);
        }
        if verification.has_remaining_findings() {
            let purge_incomplete = verification
                .remaining_findings
                .iter()
                .any(|finding| finding.kind == "purge_incomplete");
            return Err(KioError::new(
                if purge_incomplete {
                    "KIO-E-PURGE-INCOMPLETE-001"
                } else {
                    "KIO-E-STORE-CORRUPT-001"
                },
                "object verification found remaining findings",
                serde_json::to_value(&verification).unwrap_or_else(|_| json!({})),
                ExitCode::PartialFailure,
            ));
        }
        let (result, completion) = run_rebuild_db_locked(ctx, repo, false, true, false)?;
        // This branch consumes completion as an error classification, retaining the original error context body.
        let exit = match completion {
            Completion::Complete => None,
            Completion::AuthRequired => Some(ExitCode::AuthError),
            Completion::BudgetBlocked => Some(ExitCode::BudgetExceeded),
            Completion::Partial => Some(ExitCode::PartialFailure),
            Completion::Permanent => Some(ExitCode::PermanentFailure),
        };
        if let Some(exit) = exit {
            return Err(KioError::new(
                non_success_error_code(exit),
                "source SQLite recovery completed with outstanding work",
                serde_json::to_value(&result)
                    .map_err(|error| KioError::schema(error.to_string()))?,
                exit,
            ));
        }
        write_through_projection(ctx, repo.kio_dir()).map_err(|reason| {
            KioError::new(
                "KIO-E-AGGREGATOR-001",
                reason,
                json!({ "component": "replica" }),
                ExitCode::PartialFailure,
            )
        })?;
        Ok(ScopeRepairResult::All(ScopeRepairAll {
            rebuild: result,
            replica: "ready".to_owned(),
        }))
    } else {
        // Replica recovery must leave the source store (including HEAD and
        // sqlite.db) untouched; only the device projection is regenerated.
        validate_repo_tool_lock(repo)?;
        write_through_projection(ctx, repo.kio_dir()).map_err(|reason| {
            KioError::new(
                "KIO-E-AGGREGATOR-001",
                reason,
                json!({ "component": "replica" }),
                ExitCode::PartialFailure,
            )
        })?;
        Ok(ScopeRepairResult::Replica(ScopeRepairReplica {
            status: "replica_rebuilt".to_owned(),
        }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepairMode {
    RebuildDb,
    VerifyObjects,
    /// PB12 (step4b-contract-tests-p2b.md §E, 10-operations.md §7.5.1
    /// L586-626): `--verify-objects --prune-orphans` — `--prune-orphans` is a
    /// modifier on `--verify-objects`, never valid alone or with
    /// `--rebuild-db`.
    VerifyObjectsPruneOrphans,
    /// PB25 (§H, 10 §3 L291-293): `kio repair registry-prune`.
    RegistryPrune,
}

/// QA29 (step4b-contract-tests-p3a.md §I): [`parsed_repair`]'s parsed
/// result — the primary mode plus `--rebuild-db`'s optional
/// `--online`/`--offline` sub-flags (06-cli-spec.md §1 L52-55).
struct ParsedRepair {
    mode: RepairMode,
    online: bool,
    offline: bool,
}

/// Fold the `repair` sub-command into the mode enum the command body uses.
///
/// C (2026-07-24): every rule this used to check by hand — exactly one
/// operation, `--prune-orphans` only under `verify-objects`, the send/lane
/// options only under `rebuild-db` — is now structural in the sub-command
/// declaration, so this is a total function with nothing left to reject.
fn parsed_repair(args: &RepairArgs) -> ParsedRepair {
    match &args.operation {
        RepairOperation::RegistryPrune(_) => ParsedRepair {
            mode: RepairMode::RegistryPrune,
            online: false,
            offline: false,
        },
        RepairOperation::RebuildDb(rebuild) => ParsedRepair {
            mode: RepairMode::RebuildDb,
            online: rebuild.online,
            offline: rebuild.offline,
        },
        RepairOperation::VerifyObjects(verify) => ParsedRepair {
            mode: if verify.prune_orphans {
                RepairMode::VerifyObjectsPruneOrphans
            } else {
                RepairMode::VerifyObjects
            },
            online: false,
            offline: false,
        },
        RepairOperation::All | RepairOperation::Replica => {
            unreachable!("device-global repair is dispatched before the scoped parser")
        }
        RepairOperation::RecoverRestore => {
            unreachable!("managed restore recovery is dispatched before the scoped parser")
        }
        RepairOperation::CancelChildInitialization(_) => {
            unreachable!("child initialization cancellation is dispatched before the scoped parser")
        }
    }
}
