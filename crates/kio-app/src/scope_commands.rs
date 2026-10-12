use crate::batch::command::stalled_batch_status_json;
use crate::commands;
use crate::egress::quarantine_status_records;
use crate::index::chunk_ledger::ensure_purge_epoch_initialized;
use crate::index::command::current_unsupported_inputs;
use crate::invocation::Invocation;
use crate::ledger_access::ledger_db_path;
use crate::ledger_access::open_search_budget_snapshot;
use crate::management::ExplicitRoot;
use crate::management::initialize_explicit_root_in_session;
use crate::object_persistence::pipeline_to_kio;
use crate::response::CommandResponse;
use crate::response::Completion;
use crate::response::ResponseBody;
use crate::response::device::InitResponse;
use crate::response::project_legacy;
use crate::scope_access::register_scope;
use crate::settings::validate_repo_tool_lock;
use crate::task_execution::budget_status_json;
use crate::task_execution::paused_tasks_by_hold_reason;
use kio_core::KioError;
use kio_core::Result;
use kio_core::purge::PurgeState;
use kio_pipeline::task::TaskStore;
use kio_pipeline::unsupported::UnsupportedInputStore;
use serde_json::Value;
use serde_json::json;

pub(crate) fn run_init(ctx: &Invocation<'_>, args: commands::InitArgs) -> Result<Value> {
    let working_directory = ctx.cwd().to_path_buf();
    let requested = args.path.unwrap_or(working_directory.clone());
    let path = if requested.is_absolute() {
        requested
    } else {
        working_directory.join(requested)
    };
    // Management records are created only by the exclusive creation path.
    // Any pre-existing store must already have a valid live chain.
    let (repo, created) = match initialize_explicit_root_in_session(&path, ctx.control, ctx.locks)?
    {
        ExplicitRoot::Created(repo) => (repo, true),
        ExplicitRoot::Existing(repo) => (repo, false),
    };
    let repo = repo.in_lock_session(ctx.locks);
    // Existing stores may finish only a verified publication intent.
    // Missing/corrupt HEAD has no second ref from which to guess a tip.
    let (repaired, _existing_store_lock) = if created {
        (Vec::new(), None)
    } else {
        let lock = repo.lock_store_in_session(repo.lock_session())?;
        let repaired = if repo.recover_publication()? {
            vec!["publication".to_owned()]
        } else {
            Vec::new()
        };
        // A recovery-capable open deliberately permits a missing HEAD
        // long enough to replay an intent. With no valid intent, init
        // must still reject the missing/malformed canonical history.
        repo.head_commit_hash()?;
        (repaired, Some(lock))
    };
    // LC39/LC40: seed `.kio/purge/epoch` from scope creation onward, so
    // no read command ever fail-closes on a file that simply was never
    // created (see `ensure_purge_epoch_initialized`'s doc comment).
    ensure_purge_epoch_initialized(repo.kio_dir(), repo.control())?;
    // Register the scope in the device-local registry so multi-scope search
    // can enumerate it (05 §1.8). `indexed=false` until `kio index` runs.
    // The registry is a cache, never truth (03 §4): a write failure is a
    // warning, never a hard error.
    register_scope(ctx, &repo, false);
    let status = if created {
        "initialized"
    } else if repaired.is_empty() {
        "already initialized"
    } else {
        "repaired"
    };
    project_legacy(CommandResponse {
        body: ResponseBody::Init(InitResponse {
            status: status.to_owned(),
            repaired,
            path: repo.root().to_path_buf(),
            kio_path: repo.kio_dir().to_path_buf(),
        }),
        completion: Completion::Complete,
    })
}

pub(crate) fn run_status(ctx: &Invocation<'_>) -> Result<Value> {
    let repo = ctx.open_recovery_repository()?;
    validate_repo_tool_lock(&repo)?;
    let task_store = TaskStore::new_with_control(repo.kio_dir(), repo.control().clone());
    // R15-4: `status` is a pure read — a shallow HEAD (tree object gone)
    // degrades to listing files without a classification (`head_shallow`)
    // instead of dying on a raw KIO-E-STORE-NOT-FOUND-001.
    let status = repo.status()?;
    let (unsupported_inputs, unsupported_inputs_complete) = if status.head_shallow {
        UnsupportedInputStore::new(repo.kio_dir())
            .latest_by_path()
            .map_err(pipeline_to_kio)?;
        (Vec::new(), false)
    } else {
        (current_unsupported_inputs(&repo)?, true)
    };
    let tasks = task_store.all().map_err(pipeline_to_kio)?;
    let ledger_snapshot = open_search_budget_snapshot(&ledger_db_path(ctx.control), ctx.control)?;
    Ok(json!({
        "scope_path": repo.kio_dir(),
        "files": status.files,
        "head_shallow": status.head_shallow,
        // QA4 (step4b-contract-tests-p3a.md §A, 10 §1 L117): the
        // paused-task count broken down by hold_reason (budget/auth/
        // tier_b_approval), alongside the raw task list callers
        // previously had to filter client-side themselves.
        "paused_by_hold_reason": paused_tasks_by_hold_reason(&tasks),
        "tasks": tasks,
        "quarantine": quarantine_status_records(&repo)?,
        "unsupported_inputs": unsupported_inputs,
        "unsupported_inputs_complete": unsupported_inputs_complete,
        "budget": budget_status_json(&repo, ledger_snapshot.as_ref())?,
        // CL37/CL68: permanently-stalled (settled but residue-cleanup-
        // pending) cost-ledger.sqlite batch_requests rows, device-global
        // — not scoped to this folder, since the ledger itself is not.
        "stalled_batch": stalled_batch_status_json(ledger_snapshot.as_ref(), ctx.control)?,
        // 05 §3.5 / LC51: `status` is the one read command the §I barrier
        // does NOT reject on an active journal — it shows the journal as
        // state instead, for recovery visibility into a crashed purge.
        "active_purge_journal": PurgeState::open_with_control(repo.kio_dir(), repo.control().clone())?
            .read_journal()?
            .map(serde_json::to_value)
            .transpose()
            .map_err(|err| KioError::schema(err.to_string()))?,
        // GC has its own durable recovery marker.  As with purge,
        // status exposes it rather than attempting recovery or
        // collapsing an interrupted destructive operation into a
        // normal healthy state.
        "active_gc_sweep": kio_core::gc::read_active_marker_with_control(repo.kio_dir(), repo.control())?
            .map(serde_json::to_value)
            .transpose()
            .map_err(|err| KioError::schema(err.to_string()))?,
    }))
}
