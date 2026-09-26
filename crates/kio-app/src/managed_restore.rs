//! Application boundary for journaled restores of an ancestor into the current
//! managed working tree.  Export remains destination-only; this module is the
//! only path that can materialize historical bytes under the managed root.

use std::path::PathBuf;

use kio_core::scope::{ManagedRestorePlan, ManagedRestoreRequest, Repository, now_utc_seconds};
use kio_core::{ExitCode, KioError, Result};
use serde_json::{Value, json};

use crate::commands::RestoreArgs;
use crate::management::open_existing_managed_root;
use crate::{
    binding_for_repo, context_working_directory, current_embedding_policy,
    mark_replica_rebuilding_or_log, mark_replica_unavailable_or_log, pipeline_to_kio,
    rebuild_step3_index, recover_index_generation, set_exit_override, validate_repo_tool_lock,
    write_through_projection_full,
};

pub(super) fn run(args: RestoreArgs) -> Result<Value> {
    // This is intentionally the strict, bound managed-root open.  In
    // particular, restore never invokes the recovery opener, so --preview has
    // no publication, index, ledger, or working-tree side effect.
    let repo = open_existing_managed_root(&context_working_directory()?)?;
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
        now_utc_seconds(),
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
        return Ok(plan_json("preview", &plan, None));
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
        mark_replica_rebuilding_or_log(repo.kio_dir(), commit_hash);
    }
    let mut result = plan_json("applied", &plan, Some(&outcome));
    if outcome.noop {
        result["projection_status"] = json!("unchanged");
        return Ok(result);
    }
    match rebuild_projection(&repo, Some((&policy, &plan))) {
        Ok(projection) => {
            result["projection_status"] = json!("ready");
            result["projection"] = projection;
        }
        Err(error) => {
            // The restored commit and its durable provenance are already
            // valid.  Cache rebuild failure must be visible and fail closed,
            // but never rolls HEAD back or rewrites the restored files.
            mark_replica_unavailable_or_log(repo.kio_dir());
            result["status"] = json!("applied_projection_failed");
            result["projection_status"] = json!("failed");
            result["projection_error"] = json!({
                "error_code": error.error_code(),
                "message": error.message(),
            });
            set_exit_override(&mut result, ExitCode::PartialFailure);
        }
    }
    Ok(result)
}

/// `repair recover-restore` is intentionally distinct from ordinary
/// publication recovery.  A pending restore journal governs working bytes and
/// must be resolved by core before any generic publication-recovery pathway.
pub(super) fn recover() -> Result<Value> {
    let repo = Repository::open_for_recovery(context_working_directory()?)?;
    validate_repo_tool_lock(&repo)?;
    // Recovery has to be possible when the policy that permitted the original
    // restore has since denied a path.  Validate the retained management
    // authority, but do not require current ignore eligibility merely to put
    // bytes back or finish the already-journaled publication.
    kio_core::management::validate_live_chain(&binding_for_repo(&repo)?)?;
    let recovered = repo.recover_managed_restore()?;
    let mut result = json!({
        "operation": "managed_restore_recovery",
        "status": if recovered { "recovered" } else { "no_pending_restore" },
        "recovered": recovered,
        "egress": false,
    });
    if recovered {
        if let Some(head) = repo.head_commit_hash()? {
            mark_replica_rebuilding_or_log(repo.kio_dir(), &head);
        }
        match rebuild_projection(&repo, None) {
            Ok(projection) => {
                result["projection_status"] = json!("ready");
                result["projection"] = projection;
            }
            Err(error) => {
                mark_replica_unavailable_or_log(repo.kio_dir());
                result["status"] = json!("recovered_projection_failed");
                result["projection_status"] = json!("failed");
                result["projection_error"] = json!({
                    "error_code": error.error_code(),
                    "message": error.message(),
                });
                set_exit_override(&mut result, ExitCode::PartialFailure);
            }
        }
    }
    Ok(result)
}

/// Rebuild only local, reproducible derived state after a restore.  This uses
/// immutable CAS plus the restored HEAD; it neither polls nor submits work and
/// does not call the online-capable repair entrypoint.
fn rebuild_projection(
    repo: &Repository,
    policy: Option<(
        &kio_pipeline::policy::CurrentPolicyEvaluator,
        &ManagedRestorePlan,
    )>,
) -> Result<Value> {
    let _lock = repo.lock_store()?;
    if let Some((policy, plan)) = policy {
        ensure_scope_allowed(policy)?;
        ensure_planned_paths_allowed(policy, plan)?;
    }
    let report = rebuild_step3_index(repo)?;
    recover_index_generation(repo.kio_dir())?;
    write_through_projection_full(repo.kio_dir()).map_err(|reason| {
        KioError::new(
            "KIO-E-MANAGED-RESTORE-PROJECTION-001",
            "restored HEAD committed but local replica projection failed",
            json!({ "reason": reason }),
            ExitCode::PartialFailure,
        )
    })?;
    Ok(json!({
        "status": "ready",
        "rebuilt_chunks": report.rebuilt_chunks,
        "rebuilt_tree_entries": report.rebuilt_tree_entries,
        "skipped_units": report.skipped_units,
        "egress": false,
    }))
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

fn plan_json(
    status: &str,
    plan: &ManagedRestorePlan,
    outcome: Option<&kio_core::scope::ManagedRestoreOutcome>,
) -> Value {
    json!({
        "operation": "managed_restore",
        "status": status,
        "source_commit": plan.source_commit(),
        "expected_head": plan.expected_head(),
        "new_commit_hash": if plan.is_noop() { None } else { Some(plan.new_commit_hash()) },
        "new_head": outcome.and_then(|value| value.commit_hash.as_deref()),
        "new_tree_hash": plan.new_tree_hash(),
        "changes": plan.changes(),
        "provenance": {
            "source_commit": plan.source_commit(),
            "parent_commit": plan.expected_head(),
            "commit_type": "restored",
        },
        "apply": outcome.map(|value| json!({
            "noop": value.noop,
            "commit_hash": value.commit_hash,
            "tree_hash": value.tree_hash,
            "restored_paths": value.restored_paths,
        })),
        "egress": false,
    })
}
