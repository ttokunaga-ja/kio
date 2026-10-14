//! Application boundary for journaled restores of an ancestor into the current
//! managed working tree.  Export remains destination-only; this module is the
//! only path that can materialize historical bytes under the managed root.

use crate::response::mutation::{RestoreProvenance, RestoreResponse};
use crate::response::repair::*;
use crate::response::{CommandResponse, Completion, ResponseBody};
use std::path::PathBuf;

use kio_core::scope::{ManagedRestorePlan, ManagedRestoreRequest, Repository};
use kio_core::{ExitCode, KioError, Result};
use serde_json::json;

use crate::commands::RestoreArgs;
use crate::index::chunk_ledger::recover_index_generation;
use crate::index::rebuild::rebuild_step3_index;
use crate::management::binding_for_repo;
use crate::management::open_existing_managed_root;
use crate::object_persistence::pipeline_to_kio;
use crate::search::projection::mark_replica_rebuilding_or_log;
use crate::search::projection::mark_replica_unavailable_or_log;
use crate::search::projection::write_through_projection_full;
use crate::search::scope::current_embedding_policy;
use crate::settings::validate_repo_tool_lock;

pub(super) fn run(
    ctx: &crate::invocation::Invocation<'_>,
    args: RestoreArgs,
) -> Result<CommandResponse> {
    // This is intentionally the strict, bound managed-root open.  In
    // particular, restore never invokes the recovery opener, so --preview has
    // no publication, index, ledger, or working-tree side effect.
    let repo = open_existing_managed_root(ctx.cwd(), ctx.control)?.in_lock_session(ctx.locks);
    validate_repo_tool_lock(&repo)?;
    let source_commit = repo.resolve_commit(&args.source)?;
    let policy = current_embedding_policy(&repo)?;
    ensure_scope_allowed(&policy)?;

    let requested_paths = requested_paths(&repo, &source_commit, &args, &policy)?;
    let delete_missing = argument_paths(&args.delete_missing, "--delete-missing")?;
    for path in requested_paths.iter().chain(delete_missing.iter()) {
        ensure_path_allowed(&policy, path)?;
    }

    let plan = repo.plan_restore(ManagedRestoreRequest::new(
        source_commit.clone(),
        Some(requested_paths),
        delete_missing,
        args.message
            .clone()
            .unwrap_or_else(|| format!("restore {source_commit}")),
        ctx.control.now_utc_seconds(),
    ))?;
    if let Some(expected) = args.expected_head.as_deref() {
        if !kio_core::cas::is_hash(expected) {
            return Err(KioError::invalid_usage(
                "--expected-head must be a full commit hash",
            ));
        }
        if expected != plan.expected_head() {
            return Err(KioError::new(
                "KIO-E-MANAGED-RESTORE-CONFLICT-001",
                "current HEAD differs from --expected-head",
                json!({ "expected_head": expected, "actual_head": plan.expected_head() }),
                ExitCode::PartialFailure,
            ));
        }
    }
    ensure_planned_paths_allowed(&policy, &plan)?;

    if args.preview {
        return Ok(CommandResponse {
            body: ResponseBody::Restore(plan_response("preview", &plan, None)),
            completion: Completion::Complete,
        });
    }

    let outcome = repo.apply_restore(&plan, || {
        // Core invokes this after it has acquired the writer lock, before each
        // working-tree mutation, and immediately before HEAD publication.
        // Reloading through the evaluator catches management, ignore, and
        // scope authority changes that occurred after planning.
        ensure_scope_allowed(&policy)?;
        ensure_planned_paths_allowed(&policy, &plan)
    })?;
    if let Some(commit_hash) = outcome.commit_hash.as_deref() {
        // A restored HEAD has no guarantee that its historical normalized
        // outputs are current.  Mark the projection unavailable instead of
        // claiming a synchronous rebuild that could run adapters or egress.
        mark_replica_rebuilding_or_log(ctx, repo.kio_dir(), commit_hash);
    }
    let mut result = plan_response("applied", &plan, Some(&outcome));
    let mut completion = Completion::Complete;
    if outcome.noop {
        result.projection_status = Some("unchanged".to_owned());
        return Ok(CommandResponse {
            body: ResponseBody::Restore(result),
            completion,
        });
    }
    match rebuild_projection(ctx, &repo, Some((&policy, &plan))) {
        Ok(projection) => {
            result.projection_status = Some("ready".to_owned());
            result.projection = Some(projection);
        }
        Err(error) => {
            // The restored commit and its durable provenance are already
            // valid.  Cache rebuild failure must be visible and fail closed,
            // but never rolls HEAD back or rewrites the restored files.
            mark_replica_unavailable_or_log(ctx, repo.kio_dir());
            result.status = "applied_projection_failed".to_owned();
            result.projection_status = Some("failed".to_owned());
            result.projection_error = Some(ManagedRestoreProjectionError {
                error_code: error.error_code().to_owned(),
                message: error.message().to_owned(),
            });
            completion = Completion::Partial;
        }
    }
    Ok(CommandResponse {
        body: ResponseBody::Restore(result),
        completion,
    })
}

/// `repair recover-restore` is intentionally distinct from ordinary
/// publication recovery.  A pending restore journal governs working bytes and
/// must be resolved by core before any generic publication-recovery pathway.
pub(super) fn recover(ctx: &crate::invocation::Invocation<'_>) -> Result<CommandResponse> {
    let repo = Repository::open_recovery_in_session(ctx.cwd(), ctx.control.clone(), ctx.locks)?;
    validate_repo_tool_lock(&repo)?;
    // Recovery has to be possible when the policy that permitted the original
    // restore has since denied a path.  Validate the retained management
    // authority, but do not require current ignore eligibility merely to put
    // bytes back or finish the already-journaled publication.
    kio_core::management::validate_live_chain(&binding_for_repo(&repo)?)?;
    let recovered = repo.recover_managed_restore()?;
    let mut result = ManagedRestoreRecoveryResponse {
        operation: "managed_restore_recovery".to_owned(),
        status: if recovered {
            "recovered"
        } else {
            "no_pending_restore"
        }
        .to_owned(),
        recovered,
        egress: false,
        projection_status: None,
        projection: None,
        projection_error: None,
    };
    let mut completion = Completion::Complete;
    if recovered {
        if let Some(head) = repo.head_commit_hash()? {
            mark_replica_rebuilding_or_log(ctx, repo.kio_dir(), &head);
        }
        match rebuild_projection(ctx, &repo, None) {
            Ok(projection) => {
                result.projection_status = Some("ready".to_owned());
                result.projection = Some(projection);
            }
            Err(error) => {
                mark_replica_unavailable_or_log(ctx, repo.kio_dir());
                result.status = "recovered_projection_failed".to_owned();
                result.projection_status = Some("failed".to_owned());
                result.projection_error = Some(ManagedRestoreProjectionError {
                    error_code: error.error_code().to_owned(),
                    message: error.message().to_owned(),
                });
                completion = Completion::Partial;
            }
        }
    }
    Ok(CommandResponse {
        body: ResponseBody::Repair(RepairResponse::RecoverRestore(result)),
        completion,
    })
}

/// Rebuild only local, reproducible derived state after a restore.  This uses
/// immutable CAS plus the restored HEAD; it neither polls nor submits work and
/// does not call the online-capable repair entrypoint.
fn rebuild_projection(
    ctx: &crate::invocation::Invocation<'_>,
    repo: &Repository,
    policy: Option<(
        &kio_pipeline::policy::CurrentPolicyEvaluator,
        &ManagedRestorePlan,
    )>,
) -> Result<ManagedRestoreProjection> {
    let _lock = repo.lock_store_in_session(repo.lock_session())?;
    if let Some((policy, plan)) = policy {
        ensure_scope_allowed(policy)?;
        ensure_planned_paths_allowed(policy, plan)?;
    }
    let report = rebuild_step3_index(ctx, repo)?;
    recover_index_generation(repo.kio_dir(), repo.control())?;
    write_through_projection_full(repo.kio_dir(), ctx.control).map_err(|reason| {
        KioError::new(
            "KIO-E-MANAGED-RESTORE-PROJECTION-001",
            "restored HEAD committed but local replica projection failed",
            json!({ "reason": reason }),
            ExitCode::PartialFailure,
        )
    })?;
    Ok(ManagedRestoreProjection {
        status: "ready".to_owned(),
        rebuilt_chunks: report.rebuilt_chunks,
        rebuilt_tree_entries: report.rebuilt_tree_entries,
        skipped_units: report.skipped_units,
        egress: false,
    })
}

fn requested_paths(
    repo: &Repository,
    source_commit: &str,
    args: &RestoreArgs,
    policy: &kio_pipeline::policy::CurrentPolicyEvaluator,
) -> Result<Vec<String>> {
    if !args.paths.is_empty() {
        return argument_paths(&args.paths, "--path");
    }

    // Whole-scope restore deliberately means every source path that the live
    // policy currently permits.  It does not infer deletion for current-only
    // paths: users must name each deletion with --delete-missing.
    let source = repo.read_commit(source_commit)?;
    let tree = repo.read_tree(&source.tree)?;
    let mut selected = Vec::new();
    for entry in tree.entries {
        if policy.allows_path(&entry.path).map_err(pipeline_to_kio)? {
            selected.push(entry.path);
        }
    }
    Ok(selected)
}

fn argument_paths(paths: &[PathBuf], option: &str) -> Result<Vec<String>> {
    paths
        .iter()
        .map(|path| {
            path.to_str()
                .map(str::to_owned)
                .ok_or_else(|| KioError::invalid_usage(format!("{option} path is not valid UTF-8")))
        })
        .collect()
}

fn ensure_scope_allowed(policy: &kio_pipeline::policy::CurrentPolicyEvaluator) -> Result<()> {
    policy.revalidate().map_err(pipeline_to_kio)?;
    if !policy.allows_scope().map_err(pipeline_to_kio)? {
        return Err(KioError::new(
            "KIO-E-MANAGED-RESTORE-POLICY-001",
            "managed restore is denied by the current scope policy",
            json!({}),
            ExitCode::InvalidUsage,
        ));
    }
    Ok(())
}

fn ensure_path_allowed(
    policy: &kio_pipeline::policy::CurrentPolicyEvaluator,
    path: &str,
) -> Result<()> {
    if !policy.allows_path(path).map_err(pipeline_to_kio)? {
        return Err(KioError::new(
            "KIO-E-MANAGED-RESTORE-POLICY-001",
            "managed restore path is denied by the current policy",
            json!({ "path": path }),
            ExitCode::InvalidUsage,
        ));
    }
    Ok(())
}

fn ensure_planned_paths_allowed(
    policy: &kio_pipeline::policy::CurrentPolicyEvaluator,
    plan: &ManagedRestorePlan,
) -> Result<()> {
    policy.revalidate().map_err(pipeline_to_kio)?;
    for change in plan.changes() {
        ensure_path_allowed(policy, &change.path)?;
    }
    Ok(())
}

fn plan_response(
    status: &str,
    plan: &ManagedRestorePlan,
    outcome: Option<&kio_core::scope::ManagedRestoreOutcome>,
) -> RestoreResponse {
    RestoreResponse {
        operation: "managed_restore".to_owned(),
        status: status.to_owned(),
        source_commit: plan.source_commit().to_owned(),
        expected_head: plan.expected_head().to_owned(),
        new_commit_hash: if plan.is_noop() {
            None
        } else {
            Some(plan.new_commit_hash().to_owned())
        },
        new_head: outcome.and_then(|value| value.commit_hash.clone()),
        new_tree_hash: plan.new_tree_hash().to_owned(),
        changes: plan.changes().to_vec(),
        provenance: RestoreProvenance {
            source_commit: plan.source_commit().to_owned(),
            parent_commit: plan.expected_head().to_owned(),
            commit_type: "restored".to_owned(),
        },
        apply: outcome.cloned(),
        egress: false,
        projection_status: None,
        projection: None,
        projection_error: None,
    }
}
