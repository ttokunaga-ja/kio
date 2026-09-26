//! Phase 4 receipt-first shallow sweep CLI orchestration.
//!
//! CAS mutation remains in `kio_core::gc::GcSweepSession`; this module only
//! owns the explicit confirmation, store-lock and SQLite generation boundary.

use std::io::Write;
use std::time::{Duration, Instant};

use kio_core::gc::{
    GcAutomationBinding, GcAutomationMode, GcInProgressMarker, GcIndexRotation,
    GcIndexRotationRole, GcIndexState, GcPlan, GcReceiptPublication, GcSweepPhase, GcSweepSession,
    SnapshotAutoBinding,
};
use kio_core::scope::{
    Repository, format_utc_seconds, new_ulid, now_utc_seconds, parse_utc_seconds,
};
#[cfg(debug_assertions)]
use kio_core::test_control::GcFault;
use kio_core::{ExitCode, KioError, Result};
use kio_index::fts::{
    BoundGcIndexMetadata, FtsSchemaConfig, FtsTokenizer, GcIndexRotationAttestation,
    PreparedGcIndexCleanup, cleanup_stale_bound_gc_index_rotations,
    exchange_prepared_bound_gc_index, prepare_bound_gc_index_rotation,
    read_bound_gc_index_metadata, read_bound_gc_index_rotation_attestation,
    remove_prepared_bound_gc_index,
};
use serde::Serialize;
use serde_json::{Value, json};

/// A soft monotonic execution budget.  The executor consults it only after a
/// durable, resumable state transition, so an invocation always makes forward
/// progress and never abandons an in-flight receipt/tree/index operation.
struct ExecutionBudget {
    deadline: Option<Instant>,
    test_checkpoint_limit: Option<u64>,
    completed_checkpoints: u64,
}

impl ExecutionBudget {
    fn new(deadline: Option<Instant>) -> Self {
        Self {
            deadline,
            test_checkpoint_limit: test_runtime_checkpoint_limit(),
            completed_checkpoints: 0,
        }
    }

    fn should_defer_after_checkpoint(&mut self) -> bool {
        self.completed_checkpoints = self.completed_checkpoints.saturating_add(1);
        let synthetic_limit = self
            .test_checkpoint_limit
            .is_some_and(|limit| self.completed_checkpoints >= limit.max(1));
        let elapsed = self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline);
        synthetic_limit || elapsed
    }
}

pub(super) enum AutomaticGcPreflight {
    Proceed {
        recovered: bool,
        binding: GcAutomationBinding,
    },
    Deferred(Value),
}

/// Resume a marker before an ordinary writer acquires the store lock.  This
/// path is enabled only by the automatic mode associated with this trigger;
/// ordinary writers retain their active-marker barrier.
pub(super) fn preflight_automatic(
    repository: &Repository,
    trigger: &str,
) -> Result<AutomaticGcPreflight> {
    let session = GcSweepSession::bind_repository(repository)?;
    let binding = session.automation_binding()?;
    preflight_automatic_bound(&session, &binding, None, trigger)
}

/// Scheduler variant of automatic-GC preflight.  The caller has already
/// retained the selected scope and `.kio` capabilities; never re-bind the
/// public repository path between the scheduler observation and a possibly
/// destructive recovery.
pub(super) fn preflight_automatic_bound(
    session: &GcSweepSession,
    expected_binding: &GcAutomationBinding,
    expected_snapshot_auto: Option<&SnapshotAutoBinding>,
    trigger: &str,
) -> Result<AutomaticGcPreflight> {
    let started = Instant::now();
    session.assert_public_identity()?;
    let binding = session.automation_binding()?;
    if &binding != expected_binding {
        return Err(config_changed_after_publication());
    }
    let config = binding.config;
    match config.mode {
        GcAutomationMode::ManualOnly => Ok(AutomaticGcPreflight::Proceed {
            recovered: false,
            binding,
        }),
        GcAutomationMode::OnIdle if trigger != "snapshot_auto" => {
            // `on_idle` is deliberately a scheduler-only handoff.  In
            // particular, an ordinary writer must not consume its recovery
            // marker before taking its normal writer barrier.
            Ok(AutomaticGcPreflight::Proceed {
                recovered: false,
                binding,
            })
        }
        GcAutomationMode::OnIdle => {
            let expected_snapshot_auto = expected_snapshot_auto.ok_or_else(|| {
                KioError::schema("on-idle automatic GC requires a snapshot.auto authority binding")
            })?;
            require_snapshot_auto_binding(session, Some(expected_snapshot_auto))?;
            let max_runtime_seconds = required_max_runtime_seconds(config)?;
            let deadline = automatic_deadline(started, max_runtime_seconds)?;
            if session.read_marker()?.is_none() {
                return Ok(AutomaticGcPreflight::Proceed {
                    recovered: false,
                    binding,
                });
            }
            let mut budget = ExecutionBudget::new(Some(deadline));
            let report = resume_session_with_budget(
                session,
                fixed_now()?,
                &mut budget,
                Some(expected_binding),
                Some(expected_snapshot_auto),
            )?;
            session.assert_public_identity()?;
            if &session.automation_binding()? != expected_binding {
                return Err(config_changed_after_publication());
            }
            require_snapshot_auto_binding(session, Some(expected_snapshot_auto))?;
            let report =
                decorate_automatic_report(report, trigger, "on_idle", max_runtime_seconds, true);
            if is_runtime_deferred(&report) {
                Ok(AutomaticGcPreflight::Deferred(report))
            } else {
                Ok(AutomaticGcPreflight::Proceed {
                    recovered: true,
                    binding,
                })
            }
        }
        GcAutomationMode::AfterIndex => {
            let max_runtime_seconds = required_max_runtime_seconds(config)?;
            let deadline = automatic_deadline(started, max_runtime_seconds)?;
            if session.read_marker()?.is_none() {
                return Ok(AutomaticGcPreflight::Proceed {
                    recovered: false,
                    binding,
                });
            }
            let mut budget = ExecutionBudget::new(Some(deadline));
            let report = resume_session_with_budget(
                session,
                fixed_now()?,
                &mut budget,
                Some(expected_binding),
                None,
            )?;
            session.assert_public_identity()?;
            if &session.automation_binding()? != expected_binding {
                return Err(config_changed_after_publication());
            }
            let report = decorate_automatic_report(
                report,
                trigger,
                "after_index",
                max_runtime_seconds,
                true,
            );
            if is_runtime_deferred(&report) {
                Ok(AutomaticGcPreflight::Deferred(report))
            } else {
                // Recovery itself does not authorize a later destructive
                // handoff under a changed policy; retain the pre-publication
                // complete `[gc]` authority binding for the post-publication
                // comparison.
                Ok(AutomaticGcPreflight::Proceed {
                    recovered: true,
                    binding,
                })
            }
        }
    }
}

/// Attach the automatic GC outcome to an already-durable index/snapshot
/// result.  A GC failure cannot roll that publication back, so preserve the
/// command payload, mark its publication complete, and request an observable
/// non-success exit instead of returning a bare error envelope.
pub(super) fn attach_after_success(
    repository: &Repository,
    trigger: &str,
    recovered_before_publication: bool,
    binding: GcAutomationBinding,
    mut output: Value,
) -> Value {
    match run_after_success(repository, trigger, recovered_before_publication, &binding) {
        Ok(report) => {
            let deferred = is_runtime_deferred(&report);
            if let Some(object) = output.as_object_mut() {
                object.insert("gc".to_owned(), report);
                if deferred {
                    object.insert("publication_status".to_owned(), json!("completed"));
                    object.insert("error_code".to_owned(), json!("KIO-E-GC-RUNTIME-LIMIT-001"));
                }
            }
            if deferred {
                super::set_exit_override(&mut output, ExitCode::PartialFailure);
            }
        }
        Err(error) => {
            let exit = if error.exit_code() == ExitCode::PermanentFailure {
                ExitCode::PermanentFailure
            } else {
                ExitCode::PartialFailure
            };
            if let Some(object) = output.as_object_mut() {
                object.insert("publication_status".to_owned(), json!("completed"));
                object.insert("error_code".to_owned(), json!(error.error_code()));
                object.insert(
                    "gc".to_owned(),
                    json!({
                        "status": "failed",
                        "trigger": trigger,
                        "error": error.to_error_json(),
                    }),
                );
            }
            super::set_exit_override(&mut output, exit);
        }
    }
    output
}

pub(super) fn deferred_before_publication(trigger: &str, report: Value) -> Value {
    json!({
        "status": "deferred",
        "publication_status": "not_started",
        "error_code": "KIO-E-GC-RUNTIME-LIMIT-001",
        "message": "automatic GC recovery reached max_runtime_seconds before publication",
        "trigger": trigger,
        "gc": report,
        "__exit_code": ExitCode::PartialFailure.code(),
    })
}

fn run_after_success(
    repository: &Repository,
    trigger: &str,
    recovered_before_publication: bool,
    expected_binding: &GcAutomationBinding,
) -> Result<Value> {
    let started = Instant::now();
    wait_at_post_publication_test_barrier();
    let session = GcSweepSession::bind_repository(repository)?;
    let binding = session.automation_binding()?;
    if &binding != expected_binding {
        return Err(config_changed_after_publication());
    }
    let config = binding.config;
    match config.mode {
        GcAutomationMode::ManualOnly => Ok(json!({
            "status": "disabled",
            "reason": "manual_only",
            "trigger": trigger,
            "mode": "manual_only",
        })),
        GcAutomationMode::OnIdle => Ok(json!({
            "status": "skipped",
            "reason": "on_idle_requires_snapshot_auto",
            "trigger": trigger,
            "mode": "on_idle",
        })),
        GcAutomationMode::AfterIndex => {
            let now = fixed_now()?;
            let max_runtime_seconds = required_max_runtime_seconds(config)?;
            let deadline = automatic_deadline(started, max_runtime_seconds)?;
            let mut budget = ExecutionBudget::new(Some(deadline));
            let report = if session.read_marker()?.is_some() {
                resume_session_with_budget(
                    &session,
                    now,
                    &mut budget,
                    Some(expected_binding),
                    None,
                )?
            } else {
                let preview = session.plan_at(now)?;
                if preview.candidates.is_empty() {
                    return Ok(json!({
                        "status": "skipped",
                        "reason": "no_candidates",
                        "trigger": trigger,
                        "mode": "after_index",
                        "max_runtime_seconds": max_runtime_seconds,
                        "recovered_before_publication": recovered_before_publication,
                        "candidate_count": 0,
                        "candidate_tree_count": 0,
                        "estimated_bytes": 0,
                    }));
                }
                start_session_with_budget(
                    &session,
                    now,
                    preview,
                    &mut budget,
                    Some(expected_binding),
                    None,
                    None,
                )?
            };
            Ok(decorate_automatic_report(
                report,
                trigger,
                "after_index",
                max_runtime_seconds,
                recovered_before_publication,
            ))
        }
    }
}

/// Run the scheduler-only automatic sweep after a successful automatic
/// snapshot publication.  The scheduler has already released its writer
/// barrier; GC takes its own descriptor-bound lock so the potentially long
/// receipt/tree/index sequence is never nested inside snapshot publication.
pub(super) fn run_on_idle_after_snapshot_bound(
    session: &GcSweepSession,
    expected_binding: &GcAutomationBinding,
    expected_snapshot_auto: &SnapshotAutoBinding,
    expected_index: &BoundGcIndexMetadata,
    recovered_before_publication: bool,
) -> Result<Value> {
    let started = Instant::now();
    wait_at_post_publication_test_barrier();
    session.assert_public_identity()?;
    let binding = session.automation_binding()?;
    if &binding != expected_binding || binding.config.mode != GcAutomationMode::OnIdle {
        return Err(config_changed_after_publication());
    }
    require_snapshot_auto_binding(session, Some(expected_snapshot_auto))?;
    require_automatic_index_binding(session, Some(expected_index))?;
    let max_runtime_seconds = required_max_runtime_seconds(binding.config)?;
    let now = fixed_now()?;
    let mut budget = ExecutionBudget::new(Some(automatic_deadline(started, max_runtime_seconds)?));
    let report = if session.read_marker()?.is_some() {
        resume_session_with_budget(
            session,
            now,
            &mut budget,
            Some(expected_binding),
            Some(expected_snapshot_auto),
        )?
    } else {
        // This is a preview only. `start_session_with_budget` re-plans under
        // the GC lock and rejects a changed plan before publishing a marker.
        let preview = session.plan_at(now)?;
        start_session_with_budget(
            session,
            now,
            preview,
            &mut budget,
            Some(expected_binding),
            Some(expected_index),
            Some(expected_snapshot_auto),
        )?
    };
    session.assert_public_identity()?;
    if &session.automation_binding()? != expected_binding {
        return Err(config_changed_after_publication());
    }
    require_snapshot_auto_binding(session, Some(expected_snapshot_auto))?;
    Ok(decorate_automatic_report(
        report,
        "snapshot_auto",
        "on_idle",
        max_runtime_seconds,
        recovered_before_publication,
    ))
}

fn config_changed_after_publication() -> KioError {
    KioError::new(
        "KIO-E-GC-CONFIG-CHANGED-001",
        "GC configuration changed during the automatic handoff; automatic GC was not started",
        json!({}),
        ExitCode::PartialFailure,
    )
}

#[cfg(debug_assertions)]
fn wait_at_post_publication_test_barrier() {
    let ready_path = crate::debug_test_control()
        .expect("command dispatch installs debug test control")
        .cli
        .gc_post_publication_ready;
    let Some(ready_path) = ready_path else {
        return;
    };
    if std::fs::write(&ready_path, b"ready").is_err() {
        return;
    }
    let release_path = ready_path.with_extension("release");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !release_path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(not(debug_assertions))]
fn wait_at_post_publication_test_barrier() {}

fn required_max_runtime_seconds(config: kio_core::gc::GcAutomationConfig) -> Result<u64> {
    config
        .max_runtime_seconds
        .ok_or_else(|| KioError::schema("gc max_runtime_seconds is required for automatic GC mode"))
}

fn automatic_deadline(started: Instant, max_runtime_seconds: u64) -> Result<Instant> {
    started
        .checked_add(Duration::from_secs(max_runtime_seconds))
        .ok_or_else(|| KioError::schema("gc max_runtime_seconds exceeds monotonic clock range"))
}

fn decorate_automatic_report(
    mut report: Value,
    trigger: &str,
    mode: &str,
    max_runtime_seconds: u64,
    recovered_before_publication: bool,
) -> Value {
    if let Some(object) = report.as_object_mut() {
        object.insert("trigger".to_owned(), json!(trigger));
        object.insert("mode".to_owned(), json!(mode));
        object.insert("max_runtime_seconds".to_owned(), json!(max_runtime_seconds));
        object.insert(
            "recovered_before_publication".to_owned(),
            json!(recovered_before_publication),
        );
    }
    report
}

fn is_runtime_deferred(report: &Value) -> bool {
    report.get("status").and_then(Value::as_str) == Some("deferred")
        && report.get("reason").and_then(Value::as_str) == Some("max_runtime_seconds")
}

pub(super) fn run(
    dry_run: bool,
    yes: bool,
    prune_unreachable: bool,
    json_mode: bool,
) -> Result<Value> {
    if prune_unreachable {
        // This is intentionally a separate read-only operation.  Do not bind
        // the retention planner/session or inspect its recovery executor.
        return kio_core::gc::UnreachableObjectInventory::bind_current()?.inventory();
    }
    let now = fixed_now()?;
    // Do not canonicalize the public cwd before binding: canonicalization
    // follows a symlink and would erase the evidence that the supplied scope
    // was unsafe.  `GcSweepSession::bind` opens this original absolute name
    // no-follow, retains descriptor capabilities, and then verifies identity.
    let root = crate::context_working_directory()?;
    // Bind the scope read-only before probing the optional marker so an
    // ordinary non-scope is an invalid-usage error, never an I/O-shaped
    // missing-marker failure.  This has no HEAD self-heal side effect.
    let preview_session = GcSweepSession::bind(root.clone())?;

    if let Some(marker) = preview_session.read_marker()? {
        if dry_run {
            return Ok(recovery_pending_value(&marker));
        }
        // Recovery confirmation must show the complete frozen marker, not a
        // synthesized count summary: it is the exact durable state that will
        // be validated and resumed.
        confirm(&marker, marker.candidates.len(), true, yes, json_mode)?;
        return resume(root, now);
    }

    let preview_plan = preview_session.plan_at(now)?;
    if dry_run {
        return serde_json::to_value(preview_plan)
            .map_err(|error| KioError::schema(error.to_string()));
    }
    let preview_candidate_count = preview_plan.candidates.len();
    // Invocation authorization is independent of candidate count: a JSON or
    // non-TTY caller must always state `--yes` when asking for the mutating
    // command, rather than relying on a currently empty plan.
    // Print the complete plan (policy, candidates, exclusions and both truth
    // bindings) before asking.  The subsequent locked re-plan remains the
    // only mutation authority.
    confirm(
        &preview_plan,
        preview_candidate_count,
        false,
        yes,
        json_mode,
    )?;
    // An authorized empty plan has no destructive effect and needs no marker.
    if preview_candidate_count == 0 {
        return serde_json::to_value(preview_plan)
            .map_err(|error| KioError::schema(error.to_string()));
    }
    // A narrow integration-test seam for the exact preview-to-lock window.
    // It is inert unless explicitly named and has no role in production
    // mutation authority: the locked replan below remains mandatory.
    maybe_wait_at_test_prelock_barrier();
    start(root, now, preview_plan)
}

fn start(root: std::path::PathBuf, now: i64, preview: GcPlan) -> Result<Value> {
    let mut budget = ExecutionBudget::new(None);
    start_with_budget(root, now, preview, &mut budget)
}

fn start_with_budget(
    root: std::path::PathBuf,
    now: i64,
    preview: GcPlan,
    budget: &mut ExecutionBudget,
) -> Result<Value> {
    // Bind before acquiring the lock.  The session's forthcoming bound lock
    // API uses this retained `.kio` descriptor, so a replacement of the
    // public cwd cannot create or remove a victim `.kio/.lock`.
    let session = GcSweepSession::bind(root)?;
    start_session_with_budget(&session, now, preview, budget, None, None, None)
}

fn start_session_with_budget(
    session: &GcSweepSession,
    now: i64,
    preview: GcPlan,
    budget: &mut ExecutionBudget,
    expected_binding: Option<&GcAutomationBinding>,
    expected_index: Option<&BoundGcIndexMetadata>,
    expected_snapshot_auto: Option<&SnapshotAutoBinding>,
) -> Result<Value> {
    let _lock = session.acquire_store_lock()?;
    require_automation_binding(session, expected_binding)?;
    require_automatic_index_binding(session, expected_index)?;
    require_snapshot_auto_binding(session, expected_snapshot_auto)?;
    let locked = session.plan_at(now)?;
    // The locked plan's retention policy must still be precisely the policy
    // that authorized this invocation immediately before marker publication.
    require_automation_binding(session, expected_binding)?;
    require_automatic_index_binding(session, expected_index)?;
    require_snapshot_auto_binding(session, expected_snapshot_auto)?;
    if !preview.mutation_equivalent(&locked) {
        return Err(plan_changed());
    }
    // A locked empty replan has no mutation to authorize. This follows plan
    // equivalence so a changed plan always remains fail-closed, and precedes
    // the platform gate and marker publication.
    if locked.candidates.is_empty() {
        return Ok(no_candidates_execution_value());
    }
    if session.read_marker()?.is_some() {
        return Err(plan_changed());
    }
    // A platform without a descriptor-bound SQLite rotation must reject the
    // operation before publication.  In particular, do not manufacture an
    // active marker/receipts on Windows and then discover that finalization
    // can never complete.
    session.ensure_index_rotation_supported()?;
    let marker = marker_from_plan(session, &locked, now)?;
    // The marker is the mutation boundary. Re-read every retained automatic
    // authority immediately before publishing it so a same-path scheduler
    // configuration change cannot authorize a destructive on-idle handoff.
    require_automation_binding(session, expected_binding)?;
    require_automatic_index_binding(session, expected_index)?;
    require_snapshot_auto_binding(session, expected_snapshot_auto)?;
    session.publish_marker(&marker)?;
    inject_fault("after_marker_fsync")?;
    if budget.should_defer_after_checkpoint() {
        return Ok(runtime_limit_value(&marker));
    }
    execute_locked(session, marker, now, budget)
}

fn no_candidates_execution_value() -> Value {
    json!({
        "status": "skipped",
        "reason": "no_candidates",
        "candidate_count": 0,
        "candidate_tree_count": 0,
        "estimated_bytes": 0,
    })
}

fn resume(root: std::path::PathBuf, now: i64) -> Result<Value> {
    let mut budget = ExecutionBudget::new(None);
    resume_with_budget(root, now, &mut budget)
}

fn resume_with_budget(
    root: std::path::PathBuf,
    now: i64,
    budget: &mut ExecutionBudget,
) -> Result<Value> {
    let session = GcSweepSession::bind(root)?;
    resume_session_with_budget(&session, now, budget, None, None)
}

fn resume_session_with_budget(
    session: &GcSweepSession,
    now: i64,
    budget: &mut ExecutionBudget,
    expected_binding: Option<&GcAutomationBinding>,
    expected_snapshot_auto: Option<&SnapshotAutoBinding>,
) -> Result<Value> {
    let _lock = session.acquire_store_lock()?;
    require_automation_binding(session, expected_binding)?;
    require_snapshot_auto_binding(session, expected_snapshot_auto)?;
    session.ensure_index_rotation_supported()?;
    let marker = session.read_marker()?.ok_or_else(plan_changed)?;
    let marker_can_be_discarded = session.marker_can_be_discarded_after_fresh_replan(&marker)?;
    // Marker phase is not evidence of durable progress: a crash can happen
    // after moving to `receipting` but before the first receipt.  Only core's
    // descriptor-bound progress inventory decides whether no irreversible
    // receipt/tree action occurred.  In that case a stale frozen marker may
    // be discarded after a locked fresh plan; otherwise it remains recovery
    // authority and the core validator below must accept it.
    if marker_can_be_discarded {
        let fresh = session.plan_at(now)?;
        if fresh.plan_digest != marker.plan_digest || fresh.truth_digest != marker.truth_digest {
            session.remove_marker(&marker)?;
            return Err(plan_changed());
        }
    }
    // This evaluates current refs, immutable commits and all sharing rules;
    // it permits only matching receipts produced by this frozen marker.
    // Recovery authorization is frozen at the marker start time.  Using the
    // invocation clock here would let an otherwise valid crash cross a
    // retention bucket boundary and become permanently unresumable.
    session.validate_frozen_marker_current_truth(&marker)?;
    require_snapshot_auto_binding(session, expected_snapshot_auto)?;
    execute_locked(session, marker, now, budget)
}

fn require_automation_binding(
    session: &GcSweepSession,
    expected: Option<&GcAutomationBinding>,
) -> Result<()> {
    if let Some(expected) = expected
        && &session.automation_binding()? != expected
    {
        return Err(config_changed_after_publication());
    }
    Ok(())
}

fn require_snapshot_auto_binding(
    session: &GcSweepSession,
    expected: Option<&SnapshotAutoBinding>,
) -> Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let observed = session.snapshot_auto_binding()?;
    if &observed != expected || !observed.config.is_some_and(|config| config.enabled) {
        return Err(config_changed_after_publication());
    }
    Ok(())
}

fn require_automatic_index_binding(
    session: &GcSweepSession,
    expected: Option<&BoundGcIndexMetadata>,
) -> Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let kio = session.retained_kio_handle()?;
    let observed = read_bound_gc_index_metadata(&kio, &bound_fts_config())
        .map_err(super::index_to_kio)?
        .ok_or_else(|| {
            KioError::new(
                "KIO-E-GC-INDEX-BINDING-001",
                "indexed scope disappeared during the on-idle GC handoff",
                json!({}),
                ExitCode::PermanentFailure,
            )
        })?;
    session.assert_public_identity()?;
    if &observed != expected {
        return Err(KioError::new(
            "KIO-E-GC-INDEX-BINDING-001",
            "index changed during the on-idle GC handoff",
            json!({}),
            ExitCode::PermanentFailure,
        ));
    }
    Ok(())
}

fn execute_locked(
    session: &GcSweepSession,
    mut marker: GcInProgressMarker,
    _now: i64,
    budget: &mut ExecutionBudget,
) -> Result<Value> {
    if marker.phase == GcSweepPhase::Prepared {
        marker.phase = GcSweepPhase::Receipting;
        session.advance_marker(&marker)?;
        if budget.should_defer_after_checkpoint() {
            return Ok(runtime_limit_value(&marker));
        }
    }
    if marker.phase == GcSweepPhase::Receipting {
        for (index, frozen) in marker.candidates.iter().enumerate() {
            let candidate = kio_core::gc::GcCandidate {
                commit_hash: frozen.commit_hash.clone(),
                tree_hash: frozen.tree_hash.clone(),
                // The session does not act on this field; frozen marker
                // validation binds the canonical commit before receipt write.
                commit_type: kio_core::dag::CommitType::Auto,
                created_at: marker.started_at.clone(),
                policy: "shallow".to_owned(),
                size_bytes: frozen.size_bytes,
            };
            let publication = session.create_receipt(&candidate, marker.started_at.clone())?;
            if index == 0 {
                inject_fault("after_first_receipt")?;
            }
            if publication == GcReceiptPublication::NewlyPublished
                && budget.should_defer_after_checkpoint()
            {
                return Ok(runtime_limit_value(&marker));
            }
        }
        inject_fault("after_all_receipts_before_tree_delete")?;
        // Freeze the exact inode/state/content observations for every
        // operation-owned receipt before the marker can authorize Sweeping.
        // Recovery and each physical tree mutation re-check this digest, so a
        // same-bytes receipt replacement cannot silently rewrite audit truth.
        marker = session.bind_operation_receipts(&marker)?;
        marker.phase = GcSweepPhase::Sweeping;
        session.advance_marker(&marker)?;
        if budget.should_defer_after_checkpoint() {
            return Ok(runtime_limit_value(&marker));
        }
    }
    if marker.phase == GcSweepPhase::Sweeping {
        // Rotation happens before the first potential physical tree deletion.
        if ensure_pre_sweep_index_rotation(session, &mut marker, budget)? {
            return Ok(runtime_limit_value(&marker));
        }
        inject_fault("after_pre_sweep_rotation")?;
        for (index, tree) in marker.trees.iter().enumerate() {
            // A logical generation is not enough to bind the SQLite source:
            // reject a same-generation primary-file replacement before every
            // irreversible tree retirement.
            let attested_index_file = ensure_sweep_index_binding(session, &marker)?;
            // SAFETY: `ensure_sweep_index_binding` has just descriptor-bound
            // the public SQLite leaf and compared its strict, transactionally
            // persisted rotation attestation with this exact frozen marker.
            let permit = unsafe {
                session.authorize_tree_removal_after_index_attestation(
                    &marker,
                    attested_index_file.as_ref(),
                )?
            };
            let removed = session.remove_candidate_tree(&permit, &marker, tree)?;
            if index == 0 {
                inject_fault("after_first_tree_delete")?;
            }
            if removed && budget.should_defer_after_checkpoint() {
                return Ok(runtime_limit_value(&marker));
            }
        }
        inject_fault("after_all_trees_before_final_rotation")?;
        marker.phase = GcSweepPhase::Finalizing;
        session.advance_marker(&marker)?;
        if budget.should_defer_after_checkpoint() {
            return Ok(runtime_limit_value(&marker));
        }
    }
    if marker.phase == GcSweepPhase::Finalizing {
        // `index_final` is an audit record, never a permission slip.  Every
        // finalizing invocation (including crash recovery from a marker that
        // already records a generation) performs and fsyncs a fresh bound
        // rotation before marker completion.  This closes the interval where
        // an attacker could restore a stale index then leave a forged or old
        // `index_final` value that merely happens to match it.
        ensure_final_index_rotation(session, &mut marker)?;
        inject_fault("after_final_rotation_before_marker_removal")?;
        let completed_index_state = index_state_bound(session)?;
        if marker.index_final.as_ref() != Some(&completed_index_state) {
            return Err(index_binding_changed(
                "between the final rotation and marker completion",
            ));
        }
        session.remove_marker(&marker)?;
    }
    Ok(json!({
        "status": "completed",
        "sweep_id": marker.sweep_id,
        "candidate_count": marker.candidates.len(),
        "candidate_tree_count": marker.trees.len(),
        "estimated_bytes": marker.estimated_bytes,
        "index_initial": marker.index_initial,
        "index_pre_sweep": marker.index_pre_sweep,
        "index_final": marker.index_final,
    }))
}

fn marker_from_plan(
    session: &GcSweepSession,
    plan: &GcPlan,
    now: i64,
) -> Result<GcInProgressMarker> {
    GcInProgressMarker::from_plan(
        plan,
        new_ulid(std::path::Path::new(".kio")),
        format_utc_seconds(now),
        index_state_bound(session)?,
    )
}

fn bound_fts_config() -> FtsSchemaConfig {
    FtsSchemaConfig {
        tokenizer: FtsTokenizer::Trigram,
    }
}

fn index_state_bound(session: &GcSweepSession) -> Result<GcIndexState> {
    let kio = session.retained_kio_handle()?;
    let metadata =
        read_bound_gc_index_metadata(&kio, &bound_fts_config()).map_err(super::index_to_kio)?;
    session.assert_public_identity()?;
    let Some(metadata) = metadata else {
        return Ok(GcIndexState::Absent);
    };
    Ok(GcIndexState::Present {
        generation: metadata.metadata.index_generation,
        identity: metadata.file_identity,
    })
}

/// Read-only observer check used by fsck while a sweep marker is active.
/// Structural marker validation alone cannot prove that the pre-delete
/// generation was actually published: bind the current public SQLite leaf to
/// the exact source/target state that recovery would accept, without advancing
/// or cleaning any rotation state.
pub(super) fn validate_marker_index_binding_for_observer(
    session: &GcSweepSession,
    marker: &GcInProgressMarker,
) -> Result<()> {
    let live = index_state_bound(session)?;
    let valid = if let Some(rotation) = &marker.index_rotation {
        live == rotation.source || live == rotation.target
    } else {
        let expected = match marker.phase {
            GcSweepPhase::Prepared | GcSweepPhase::Receipting => &marker.index_initial,
            GcSweepPhase::Sweeping => marker
                .index_pre_sweep
                .as_ref()
                .unwrap_or(&marker.index_initial),
            GcSweepPhase::Finalizing => marker
                .index_final
                .as_ref()
                .or(marker.index_pre_sweep.as_ref())
                .ok_or_else(|| index_binding_changed("during fsck marker validation"))?,
        };
        &live == expected
    };
    if !valid {
        return Err(index_binding_changed("during fsck marker validation"));
    }
    if marker.phase == GcSweepPhase::Sweeping && marker.index_rotation.is_none() {
        let _ = validate_pre_sweep_index_attestation(session, marker)?;
    }
    Ok(())
}

fn ensure_pre_sweep_index_rotation(
    session: &GcSweepSession,
    marker: &mut GcInProgressMarker,
    budget: &mut ExecutionBudget,
) -> Result<bool> {
    if matches!(marker.index_initial, GcIndexState::Absent) {
        if index_state_bound(session)? == GcIndexState::Absent {
            marker.index_pre_sweep = Some(GcIndexState::Absent);
            session.advance_marker(marker)?;
            return Ok(budget.should_defer_after_checkpoint());
        }
        return Err(index_binding_changed("before the pre-sweep rotation"));
    }
    if marker.index_rotation.is_some()
        && complete_marked_index_rotation(
            session,
            marker,
            GcIndexRotationRole::PreSweep,
            Some(budget),
        )?
    {
        return Ok(true);
    }
    // A completed pre-sweep rotation is already the durable barrier required
    // for recovery. Re-rotating it would both churn cursors and hide a forged
    // marker by manufacturing a fresh attestation before validation.
    if marker.index_pre_sweep.is_some() {
        let _ = ensure_sweep_index_binding(session, marker)?;
        return Ok(false);
    }
    cleanup_stale_index_rotations(session, None)?;
    let target = {
        // Persist the exact target before SQLite mutation. A crash after
        // the rotation can then resume only when the retained database is
        // still either the frozen initial state or this target, never an
        // arbitrary replacement generation.
        let source_state = marker
            .index_pre_sweep
            .as_ref()
            .unwrap_or(&marker.index_initial);
        let GcIndexState::Present {
            generation: initial,
            identity,
        } = source_state
        else {
            unreachable!()
        };
        let generation = new_ulid(std::path::Path::new(".kio"));
        let temp_leaf = format!(".gc-index-{}-pre", new_ulid(std::path::Path::new(".kio")));
        let kio = session.retained_kio_handle()?;
        let prepared = prepare_bound_gc_index_rotation(
            &kio,
            &temp_leaf,
            &generation,
            (initial, identity),
            &GcIndexRotationAttestation {
                sweep_id: marker.sweep_id.clone(),
                role: "pre_sweep".to_owned(),
                plan_digest: marker.plan_digest.clone(),
                source_generation: initial.clone(),
                target_generation: generation.clone(),
            },
            &bound_fts_config(),
        )
        .map_err(|_| index_binding_changed("before the pre-sweep rotation"))?;
        inject_fault("after_private_prepare")?;
        let target = GcIndexState::Present {
            generation: prepared.target.metadata.index_generation,
            identity: prepared.target.file_identity,
        };
        marker.index_rotation = Some(GcIndexRotation {
            role: GcIndexRotationRole::PreSweep,
            temp_leaf,
            private_dir_identity: prepared.private_dir_identity,
            source: source_state.clone(),
            source_state_digest: prepared.source_state_digest,
            target: target.clone(),
        });
        session.advance_marker(marker)?;
        inject_fault("after_rotation_marker_persist")?;
        if budget.should_defer_after_checkpoint() {
            return Ok(true);
        }
        target
    };
    let _ = target;
    complete_marked_index_rotation(session, marker, GcIndexRotationRole::PreSweep, Some(budget))
}

fn ensure_final_index_rotation(
    session: &GcSweepSession,
    marker: &mut GcInProgressMarker,
) -> Result<()> {
    if marker.index_rotation.is_some() {
        let _ = complete_marked_index_rotation(session, marker, GcIndexRotationRole::Final, None)?;
    }
    cleanup_stale_index_rotations(session, None)?;
    // A retry after the final-rotation fault point can only advance from the
    // already-recorded final generation.  On the first finalization this is
    // the pre-sweep generation.  Either way, never use `None`: the exact
    // descriptor identity remains an expected precondition for every write.
    let expected = marker
        .index_final
        .as_ref()
        .or(marker.index_pre_sweep.as_ref())
        .ok_or_else(|| index_binding_changed("before the final rotation"))?;
    match expected {
        GcIndexState::Absent => {
            if index_state_bound(session)? != GcIndexState::Absent {
                return Err(index_binding_changed("before the final rotation"));
            }
            marker.index_final = Some(GcIndexState::Absent);
            session.advance_marker(marker)
        }
        GcIndexState::Present {
            generation: expected_generation,
            identity: expected_identity,
        } => {
            let generation = new_ulid(std::path::Path::new(".kio"));
            let temp_leaf = format!(".gc-index-{}-final", new_ulid(std::path::Path::new(".kio")));
            let kio = session.retained_kio_handle()?;
            let prepared = prepare_bound_gc_index_rotation(
                &kio,
                &temp_leaf,
                &generation,
                (expected_generation, expected_identity),
                &GcIndexRotationAttestation {
                    sweep_id: marker.sweep_id.clone(),
                    role: "final".to_owned(),
                    plan_digest: marker.plan_digest.clone(),
                    source_generation: expected_generation.clone(),
                    target_generation: generation.clone(),
                },
                &bound_fts_config(),
            )
            .map_err(|_| index_binding_changed("during the final rotation"))?;
            inject_fault("after_private_prepare")?;
            let target = GcIndexState::Present {
                generation: prepared.target.metadata.index_generation,
                identity: prepared.target.file_identity,
            };
            marker.index_rotation = Some(GcIndexRotation {
                role: GcIndexRotationRole::Final,
                temp_leaf,
                private_dir_identity: prepared.private_dir_identity,
                source: expected.clone(),
                source_state_digest: prepared.source_state_digest,
                target,
            });
            session.advance_marker(marker)?;
            inject_fault("after_rotation_marker_persist")?;
            let _ =
                complete_marked_index_rotation(session, marker, GcIndexRotationRole::Final, None)?;
            Ok(())
        }
    }
}

fn complete_marked_index_rotation(
    session: &GcSweepSession,
    marker: &mut GcInProgressMarker,
    role: GcIndexRotationRole,
    mut budget: Option<&mut ExecutionBudget>,
) -> Result<bool> {
    let rotation = marker
        .index_rotation
        .clone()
        .ok_or_else(|| index_binding_changed("during durable rotation"))?;
    if rotation.role != role {
        return Err(index_binding_changed("during durable rotation"));
    }
    let (
        GcIndexState::Present {
            identity: source, ..
        },
        GcIndexState::Present {
            identity: target, ..
        },
    ) = (&rotation.source, &rotation.target)
    else {
        return Err(index_binding_changed("during durable rotation"));
    };
    let kio = session.retained_kio_handle()?;
    match index_state_bound(session)? {
        state if state == rotation.source => {
            exchange_prepared_bound_gc_index(
                &kio,
                &rotation.temp_leaf,
                &rotation.private_dir_identity,
                source,
                &rotation.source_state_digest,
                target,
            )
            .map_err(|_| index_binding_changed("during durable rotation"))?;
            inject_fault("after_index_exchange")?;
            if budget
                .as_deref_mut()
                .is_some_and(ExecutionBudget::should_defer_after_checkpoint)
            {
                return Ok(true);
            }
        }
        state if state == rotation.target => {}
        _ => return Err(index_binding_changed("during durable rotation")),
    }
    // A prior crash may already have durably cleaned the exchanged old-source
    // leaf.  That is the only third recovery state accepted here.
    let cleanup = remove_prepared_bound_gc_index(
        &kio,
        &rotation.temp_leaf,
        &rotation.private_dir_identity,
        source,
    )
    .map_err(|_| index_binding_changed("during durable rotation"))?;
    inject_fault("after_temp_cleanup_before_marker_advance")?;
    if cleanup == PreparedGcIndexCleanup::Removed
        && budget
            .as_deref_mut()
            .is_some_and(ExecutionBudget::should_defer_after_checkpoint)
    {
        return Ok(true);
    }
    match role {
        GcIndexRotationRole::PreSweep => marker.index_pre_sweep = Some(rotation.target),
        GcIndexRotationRole::Final => marker.index_final = Some(rotation.target),
    }
    marker.index_rotation = None;
    session.advance_marker(marker)?;
    Ok(budget.is_some_and(|budget| budget.should_defer_after_checkpoint()))
}

fn cleanup_stale_index_rotations(session: &GcSweepSession, keep: Option<&str>) -> Result<()> {
    let kio = session.retained_kio_handle()?;
    cleanup_stale_bound_gc_index_rotations(&kio, keep)
        .map_err(|_| index_binding_changed("during private rotation cleanup"))
}

fn index_binding_changed(when: &str) -> KioError {
    KioError::new(
        "KIO-E-STORE-CORRUPT-001",
        format!("GC source index changed {when}"),
        json!({}),
        ExitCode::PermanentFailure,
    )
}

fn ensure_sweep_index_binding(
    session: &GcSweepSession,
    marker: &GcInProgressMarker,
) -> Result<Option<std::fs::File>> {
    let expected = marker
        .index_pre_sweep
        .as_ref()
        .ok_or_else(|| index_binding_changed("before tree retirement"))?;
    if &index_state_bound(session)? != expected {
        return Err(index_binding_changed("before tree retirement"));
    }
    validate_pre_sweep_index_attestation(session, marker)
}

/// A matching generation and inode alone prove only which database is live.
/// This transactionally-written private-copy record proves that this exact
/// database was produced for the frozen receipt operation before any tree can
/// be retired.
fn validate_pre_sweep_index_attestation(
    session: &GcSweepSession,
    marker: &GcInProgressMarker,
) -> Result<Option<std::fs::File>> {
    let (
        GcIndexState::Present {
            generation: source_generation,
            ..
        },
        Some(GcIndexState::Present {
            generation: target_generation,
            ..
        }),
    ) = (&marker.index_initial, marker.index_pre_sweep.as_ref())
    else {
        // There is no SQLite database to attest in the deliberately indexless
        // case. The marker/state equality check above remains authoritative.
        return Ok(None);
    };
    let kio = session.retained_kio_handle()?;
    let Some((live, attestation, attested_file)) =
        read_bound_gc_index_rotation_attestation(&kio, &bound_fts_config())
            .map_err(|_| index_binding_changed("before tree retirement"))?
    else {
        return Err(index_binding_changed("before tree retirement"));
    };
    session.assert_public_identity()?;
    let expected_live = marker.index_pre_sweep.as_ref().expect("matched above");
    if live.metadata.index_generation != *target_generation
        || live.file_identity
            != *match expected_live {
                GcIndexState::Present { identity, .. } => identity,
                GcIndexState::Absent => unreachable!(),
            }
    {
        return Err(index_binding_changed("before tree retirement"));
    }
    if attestation
        != (GcIndexRotationAttestation {
            sweep_id: marker.sweep_id.clone(),
            role: "pre_sweep".to_owned(),
            plan_digest: marker.plan_digest.clone(),
            source_generation: source_generation.clone(),
            target_generation: target_generation.clone(),
        })
    {
        return Err(index_binding_changed("before tree retirement"));
    }
    Ok(Some(attested_file))
}

fn fixed_now() -> Result<i64> {
    let now = now_utc_seconds();
    parse_utc_seconds(&now)
        .ok_or_else(|| KioError::schema("current time is not canonical UTC seconds"))
}

fn runtime_limit_value(marker: &GcInProgressMarker) -> Value {
    json!({
        "status": "deferred",
        "reason": "max_runtime_seconds",
        "recovery_pending": true,
        "sweep_id": marker.sweep_id,
        "phase": marker.phase,
        "candidate_count": marker.candidates.len(),
        "candidate_tree_count": marker.trees.len(),
        "estimated_bytes": marker.estimated_bytes,
        "index_initial": marker.index_initial,
        "index_pre_sweep": marker.index_pre_sweep,
        "index_final": marker.index_final,
    })
}

#[cfg(debug_assertions)]
fn test_runtime_checkpoint_limit() -> Option<u64> {
    crate::debug_test_control()
        .expect("command dispatch installs debug test control")
        .cli
        .gc_runtime_checkpoints
        .known()
        .copied()
}

#[cfg(not(debug_assertions))]
fn test_runtime_checkpoint_limit() -> Option<u64> {
    None
}

fn plan_changed() -> KioError {
    KioError::new(
        "KIO-E-GC-PLAN-CHANGED-001",
        "GC plan changed before the locked recheck; re-run kio gc",
        json!({}),
        ExitCode::PartialFailure,
    )
}

fn recovery_pending_value(marker: &GcInProgressMarker) -> Value {
    json!({
        "status": "recovery_pending",
        "recovery_pending": true,
        "marker": marker,
        "candidate_count": marker.candidates.len(),
        "candidate_tree_count": marker.trees.len(),
        "estimated_bytes": marker.estimated_bytes,
    })
}

fn confirm<T: Serialize>(
    preview: &T,
    candidate_count: usize,
    recovery_pending: bool,
    yes: bool,
    json_mode: bool,
) -> Result<()> {
    if yes {
        return Ok(());
    }
    if json_mode || !crate::context::is_interactive()? {
        return Err(KioError::invalid_usage(
            "kio gc requires --yes in JSON or non-interactive mode",
        ));
    }
    let mut stderr = std::io::stderr().lock();
    let rendered = serde_json::to_string_pretty(preview)
        .map_err(|error| KioError::schema(error.to_string()))?;
    writeln!(stderr, "GC shallow-sweep preview:\n{rendered}")
        .map_err(|error| KioError::io(error.to_string(), "stderr"))?;
    write!(
        stderr,
        "Create shallow receipts and remove planned tree objects? [y/N] "
    )
    .map_err(|error| KioError::io(error.to_string(), "stderr"))?;
    stderr
        .flush()
        .map_err(|error| KioError::io(error.to_string(), "stderr"))?;
    if crate::context::confirm("Create shallow receipts and remove planned tree objects? [y/N]")? {
        return Ok(());
    }
    Err(KioError::new(
        "KIO-E-GC-CONFIRMATION-REJECTED-001",
        "GC shallow sweep confirmation was rejected",
        json!({"candidate_count":candidate_count,"recovery_pending":recovery_pending}),
        ExitCode::ConfirmationRejected,
    ))
}

fn inject_fault(point: &str) -> Result<()> {
    if gc_fault_enabled(point) {
        return Err(KioError::new(
            "KIO-E-GC-TEST-INTERRUPTED-001",
            "GC test fault injection interrupted the sweep",
            json!({"point":point}),
            ExitCode::Interrupted,
        ));
    }
    Ok(())
}

#[cfg(debug_assertions)]
fn gc_fault_enabled(point: &str) -> bool {
    let expected = match point {
        "after_marker_fsync" => GcFault::AfterMarkerFsync,
        "after_first_receipt" => GcFault::AfterFirstReceipt,
        "after_all_receipts_before_tree_delete" => GcFault::AfterAllReceiptsBeforeTreeDelete,
        "after_pre_sweep_rotation" => GcFault::AfterPreSweepRotation,
        "after_first_tree_delete" => GcFault::AfterFirstTreeDelete,
        "after_all_trees_before_final_rotation" => GcFault::AfterAllTreesBeforeFinalRotation,
        "after_final_rotation_before_marker_removal" => {
            GcFault::AfterFinalRotationBeforeMarkerRemoval
        }
        "after_private_prepare" => GcFault::AfterPrivatePrepare,
        "after_rotation_marker_persist" => GcFault::AfterRotationMarkerPersist,
        "after_index_exchange" => GcFault::AfterIndexExchange,
        "after_temp_cleanup_before_marker_advance" => GcFault::AfterTempCleanupBeforeMarkerAdvance,
        "after_marker_stage_fsync" => GcFault::AfterMarkerStageFsync,
        "after_receipt_stage_fsync" => GcFault::AfterReceiptStageFsync,
        "after_tree_quarantine" => GcFault::AfterTreeQuarantine,
        "after_tree_retirement_capture" => GcFault::AfterTreeRetirementCapture,
        _ => return false,
    };
    crate::debug_test_control()
        .expect("command dispatch installs debug test control")
        .core
        .gc_fault
        .known()
        == Some(&expected)
}

#[cfg(not(debug_assertions))]
fn gc_fault_enabled(_: &str) -> bool {
    false
}

fn maybe_wait_at_test_prelock_barrier() {
    #[cfg(debug_assertions)]
    let ready_path = crate::debug_test_control()
        .expect("command dispatch installs debug test control")
        .cli
        .gc_prelock_ready;
    #[cfg(not(debug_assertions))]
    let ready_path: Option<std::path::PathBuf> = None;
    let Some(ready_path) = ready_path else {
        return;
    };
    if std::fs::write(&ready_path, b"ready").is_err() {
        return;
    }
    let release_path = ready_path.with_extension("release");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !release_path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
