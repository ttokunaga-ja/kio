// Typed projection goldens preserve the existing command wire shapes.
use super::gc::{
    AutomaticGcReport, GcAttachmentFailure, GcAutomationPolicyReport, GcCompletedReport,
    GcErrorDetail, GcExecutionReport, GcExistingMarkerPending, GcMarkerExchangePending,
    GcNoCandidatesReport, GcOnIdleFailure, GcOnIdleSkip, GcPreflightRecovered, GcReport,
    GcResponse, GcRuntimeDeferredReport,
};
use super::snapshot::{SnapshotAutoResponse, SnapshotCreateReport, SnapshotCreateResponse};
use kio_core::dag::CommitStats;
use kio_core::gc::{
    GcInProgressMarker, GcIndexState, GcPlan, GcPlanLimits, GcPlanStats, GcPolicy, GcSweepPhase,
};
use kio_core::{ExitCode, KioError};
use serde::Serialize;
use serde_json::{Value, json};

fn assert_projection<T: Serialize>(payload: T, legacy: Value) {
    let actual = serde_json::to_value(payload).expect("project concrete response DTO");
    assert_eq!(actual, legacy);
    assert_eq!(
        serde_json::to_vec(&actual).unwrap(),
        serde_json::to_vec(&legacy).unwrap()
    );
}

fn no_candidates() -> GcExecutionReport {
    GcExecutionReport::NoCandidates(GcNoCandidatesReport {
        status: "skipped".to_owned(),
        reason: "no_candidates".to_owned(),
        candidate_count: 0,
        candidate_tree_count: 0,
        estimated_bytes: 0,
    })
}

fn no_candidates_legacy() -> Value {
    json!({"status": "skipped", "reason": "no_candidates", "candidate_count": 0, "candidate_tree_count": 0, "estimated_bytes": 0})
}

fn runtime_report(phase: GcSweepPhase) -> GcExecutionReport {
    let pre = matches!(phase, GcSweepPhase::Sweeping | GcSweepPhase::Finalizing)
        .then_some(GcIndexState::Absent);
    GcExecutionReport::RuntimeDeferred(GcRuntimeDeferredReport {
        status: "deferred".to_owned(),
        reason: "max_runtime_seconds".to_owned(),
        recovery_pending: true,
        sweep_id: "01K73J8M00AAAAAAAAAAAAAAAA".to_owned(),
        phase,
        candidate_count: 2,
        candidate_tree_count: 1,
        estimated_bytes: u64::MAX,
        index_initial: GcIndexState::Absent,
        index_pre_sweep: pre,
        index_final: None,
    })
}

fn runtime_legacy(phase: GcSweepPhase) -> Value {
    let pre = matches!(phase, GcSweepPhase::Sweeping | GcSweepPhase::Finalizing)
        .then_some(GcIndexState::Absent);
    json!({
        "status": "deferred", "reason": "max_runtime_seconds", "recovery_pending": true,
        "sweep_id": "01K73J8M00AAAAAAAAAAAAAAAA", "phase": phase,
        "candidate_count": 2_usize, "candidate_tree_count": 1_usize, "estimated_bytes": u64::MAX,
        "index_initial": GcIndexState::Absent, "index_pre_sweep": pre, "index_final": null,
    })
}

#[test]
fn gc_execution_projection_preserves_no_candidates_and_completed_nullable_index_states() {
    assert_projection(no_candidates(), no_candidates_legacy());
    for present in [false, true] {
        let initial = if present {
            GcIndexState::Present {
                generation: "01K73J8M00AAAAAAAAAAAAAAAA".to_owned(),
                identity: "synthetic-index-identity-1".to_owned(),
            }
        } else {
            GcIndexState::Absent
        };
        // None locks the existing marker Option projection, independent of sweep reachability.
        let pre = present.then(|| GcIndexState::Present {
            generation: "01K73J8M00AAAAAAAAAAAAAAAB".to_owned(),
            identity: "synthetic-index-identity-2".to_owned(),
        });
        let final_state = present.then(|| GcIndexState::Present {
            generation: "01K73J8M00AAAAAAAAAAAAAAAC".to_owned(),
            identity: "synthetic-index-identity-3".to_owned(),
        });
        let legacy = json!({
            "status": "completed", "sweep_id": "01K73J8M00AAAAAAAAAAAAAAAA", "candidate_count": 2_usize,
            "candidate_tree_count": 1_usize, "estimated_bytes": u64::MAX,
            "index_initial": initial, "index_pre_sweep": pre, "index_final": final_state,
        });
        assert_projection(
            GcExecutionReport::Completed(GcCompletedReport {
                status: "completed".to_owned(),
                sweep_id: "01K73J8M00AAAAAAAAAAAAAAAA".to_owned(),
                candidate_count: 2,
                candidate_tree_count: 1,
                estimated_bytes: u64::MAX,
                index_initial: initial,
                index_pre_sweep: pre,
                index_final: final_state,
            }),
            legacy,
        );
    }
}

#[test]
fn gc_runtime_projection_preserves_phase_wire_and_recovery_fields() {
    for phase in [
        GcSweepPhase::Prepared,
        GcSweepPhase::Receipting,
        GcSweepPhase::Sweeping,
        GcSweepPhase::Finalizing,
    ] {
        assert_projection(runtime_report(phase.clone()), runtime_legacy(phase));
    }
}

#[test]
fn gc_automatic_projection_preserves_execution_flattening_and_policy_absence() {
    for (status, reason, mode) in [
        ("disabled", "manual_only", "manual_only"),
        ("skipped", "on_idle_requires_snapshot_auto", "on_idle"),
    ] {
        assert_projection(
            GcReport::AutomationPolicy(GcAutomationPolicyReport {
                status: status.to_owned(),
                reason: reason.to_owned(),
                trigger: "snapshot".to_owned(),
                mode: mode.to_owned(),
            }),
            json!({"status": status, "reason": reason, "trigger": "snapshot", "mode": mode}),
        );
    }
    for deferred in [false, true] {
        let execution = if deferred {
            runtime_report(GcSweepPhase::Receipting)
        } else {
            no_candidates()
        };
        let mut legacy = if deferred {
            runtime_legacy(GcSweepPhase::Receipting)
        } else {
            no_candidates_legacy()
        };
        legacy["trigger"] = json!("index");
        legacy["mode"] = json!("after_index");
        legacy["max_runtime_seconds"] = json!(u64::MAX);
        legacy["recovered_before_publication"] = json!(true);
        assert_projection(
            GcReport::AutomaticExecution(AutomaticGcReport {
                execution,
                trigger: "index".to_owned(),
                mode: "after_index".to_owned(),
                max_runtime_seconds: u64::MAX,
                recovered_before_publication: true,
            }),
            legacy,
        );
    }
}

#[test]
fn gc_attachment_failure_projection_preserves_partial_and_permanent_error_contexts() {
    for exit in [
        ExitCode::PartialFailure,
        ExitCode::PermanentFailure,
        ExitCode::AuthError,
        ExitCode::BudgetExceeded,
    ] {
        let error = KioError::new(
            "KIO-E-GC-INDEX-BINDING-001",
            "index changed during the on-idle GC handoff",
            json!({"opaque": [null, {"detail": "日本語", "attempted": u64::MAX}]}),
            exit,
        );
        let legacy =
            json!({"status": "failed", "trigger": "snapshot", "error": error.to_error_json()});
        assert_projection(
            GcReport::AttachmentFailure(GcAttachmentFailure {
                status: "failed".to_owned(),
                trigger: "snapshot".to_owned(),
                error: GcErrorDetail {
                    error_code: error.error_code().to_owned(),
                    message: error.message().to_owned(),
                    context: error.context().clone(),
                },
            }),
            legacy,
        );
    }
}

#[test]
fn gc_on_idle_projection_preserves_skip_failure_and_recovered_distinct_shapes() {
    for reason in [
        "baseline_recorded",
        "working_set_changed",
        "idle_threshold_not_reached",
    ] {
        assert_projection(
            GcReport::OnIdleSkip(GcOnIdleSkip {
                status: "skipped".to_owned(),
                reason: reason.to_owned(),
                mode: "on_idle".to_owned(),
            }),
            json!({"status": "skipped", "reason": reason, "mode": "on_idle"}),
        );
    }
    for recovery_pending in [false, true] {
        let error = KioError::new(
            "KIO-E-GC-CONFIG-CHANGED-001",
            "configuration changed",
            json!({}),
            ExitCode::PartialFailure,
        );
        assert_projection(
            GcReport::OnIdleFailure(GcOnIdleFailure {
                status: "failed".to_owned(),
                reason: "gc_failed".to_owned(),
                mode: "on_idle".to_owned(),
                recovery_pending,
                error: GcErrorDetail {
                    error_code: error.error_code().to_owned(),
                    message: error.message().to_owned(),
                    context: error.context().clone(),
                },
            }),
            json!({"status": "failed", "reason": "gc_failed", "mode": "on_idle", "recovery_pending": recovery_pending, "error": error.to_error_json()}),
        );
    }
    assert_projection(
        GcReport::PreflightRecovered(GcPreflightRecovered {
            status: "completed".to_owned(),
            trigger: "preflight_recovery".to_owned(),
            mode: "after_index".to_owned(),
            recovered_before_publication: true,
        }),
        json!({"status": "completed", "trigger": "preflight_recovery", "mode": "after_index", "recovered_before_publication": true}),
    );
}

fn manual_base() -> SnapshotCreateResponse {
    SnapshotCreateResponse {
        status: "noop".to_owned(),
        message: "snapshot 日本語".to_owned(),
        tree_hash: "a".repeat(64),
        commit_hash: None,
        commit: None,
        stats: CommitStats {
            files_added: 0,
            files_modified: 0,
            files_deleted: 0,
        },
    }
}

fn manual_legacy() -> Value {
    json!({"status": "noop", "message": "snapshot 日本語", "tree_hash": "a".repeat(64), "commit_hash": null, "commit": null,
        "stats": {"files_added": 0_u64, "files_modified": 0_u64, "files_deleted": 0_u64}})
}

#[test]
fn snapshot_manual_report_projection_preserves_base_flattening_and_gc_overwrite_fields() {
    let mut success = manual_legacy();
    success["gc"] = json!({"status": "disabled", "reason": "manual_only", "trigger": "snapshot", "mode": "manual_only"});
    assert_projection(
        SnapshotCreateReport {
            base: manual_base(),
            gc: GcReport::AutomationPolicy(GcAutomationPolicyReport {
                status: "disabled".to_owned(),
                reason: "manual_only".to_owned(),
                trigger: "snapshot".to_owned(),
                mode: "manual_only".to_owned(),
            }),
            publication_status: None,
            error_code: None,
        },
        success,
    );
    let mut deferred = manual_legacy();
    let mut gc = runtime_legacy(GcSweepPhase::Prepared);
    gc["trigger"] = json!("snapshot");
    gc["mode"] = json!("after_index");
    gc["max_runtime_seconds"] = json!(1_u64);
    gc["recovered_before_publication"] = json!(false);
    deferred["gc"] = gc;
    deferred["publication_status"] = json!("completed");
    deferred["error_code"] = json!("KIO-E-GC-RUNTIME-LIMIT-001");
    assert_projection(
        SnapshotCreateReport {
            base: manual_base(),
            gc: GcReport::AutomaticExecution(AutomaticGcReport {
                execution: runtime_report(GcSweepPhase::Prepared),
                trigger: "snapshot".to_owned(),
                mode: "after_index".to_owned(),
                max_runtime_seconds: 1,
                recovered_before_publication: false,
            }),
            publication_status: Some("completed".to_owned()),
            error_code: Some("KIO-E-GC-RUNTIME-LIMIT-001".to_owned()),
        },
        deferred,
    );
    let error = KioError::new(
        "KIO-E-GC-INDEX-BINDING-001",
        "index changed during the on-idle GC handoff",
        json!({}),
        ExitCode::PermanentFailure,
    );
    let mut failed = manual_legacy();
    failed["gc"] =
        json!({"status": "failed", "trigger": "snapshot", "error": error.to_error_json()});
    failed["publication_status"] = json!("completed");
    failed["error_code"] = json!(error.error_code());
    assert_projection(
        SnapshotCreateReport {
            base: manual_base(),
            gc: GcReport::AttachmentFailure(GcAttachmentFailure {
                status: "failed".to_owned(),
                trigger: "snapshot".to_owned(),
                error: GcErrorDetail {
                    error_code: error.error_code().to_owned(),
                    message: error.message().to_owned(),
                    context: error.context().clone(),
                },
            }),
            publication_status: Some("completed".to_owned()),
            error_code: Some(error.error_code().to_owned()),
        },
        failed,
    );
}

fn inactive_auto() -> SnapshotAutoResponse {
    SnapshotAutoResponse {
        operation: "snapshot_auto".to_owned(),
        status: "skipped".to_owned(),
        reason: "disabled".to_owned(),
        publication_status: "not_started".to_owned(),
        snapshot_status: "not_started".to_owned(),
        eligibility_reason: None,
        eligible: false,
        change_count: None,
        next_eligible_at: None,
        commit_hash: None,
        tree_hash: None,
        stats: None,
        working_set_digest: None,
        idle_observed_since: None,
        idle_observed_seconds: None,
        idle_threshold_seconds: None,
        idle_eligible: false,
        recovered_gc: false,
        recovery_pending: false,
        gc: None,
        error_code: None,
    }
}

fn inactive_auto_legacy() -> Value {
    json!({
        "operation": "snapshot_auto", "status": "skipped", "reason": "disabled", "publication_status": "not_started", "snapshot_status": "not_started",
        "eligibility_reason": null, "eligible": false, "change_count": null, "next_eligible_at": null, "commit_hash": null, "tree_hash": null,
        "stats": null, "working_set_digest": null, "idle_observed_since": null, "idle_observed_seconds": null, "idle_threshold_seconds": null,
        "idle_eligible": false, "recovered_gc": false, "recovery_pending": false, "gc": null,
    })
}

#[test]
fn snapshot_auto_projection_preserves_inactive_nulls_and_absent_error_code() {
    for reason in ["disabled", "not_indexed"] {
        let mut payload = inactive_auto();
        payload.reason = reason.to_owned();
        let mut legacy = inactive_auto_legacy();
        legacy["reason"] = json!(reason);
        assert_projection(payload, legacy);
    }
}

#[test]
fn snapshot_auto_projection_preserves_observation_stats_and_on_idle_skip() {
    for first_run in [false, true] {
        let status = if first_run {
            "baseline_recorded"
        } else {
            "not_idle"
        };
        let reason = if first_run {
            "first_observation"
        } else {
            "working_set_changed"
        };
        let eligibility_reason = if first_run {
            "first_run"
        } else {
            "change_threshold"
        };
        let gc_reason = if first_run {
            "baseline_recorded"
        } else {
            "working_set_changed"
        };
        let next_eligible_at = (!first_run).then(|| "2026-10-10T00:01:00Z".to_owned());
        let mut payload = inactive_auto();
        payload.status = status.to_owned();
        payload.reason = reason.to_owned();
        payload.publication_status = "completed".to_owned();
        payload.snapshot_status = "completed".to_owned();
        payload.eligibility_reason = Some(eligibility_reason.to_owned());
        payload.eligible = true;
        payload.change_count = Some(u64::MAX);
        payload.next_eligible_at = next_eligible_at.clone();
        payload.commit_hash = Some("b".repeat(64));
        payload.tree_hash = Some("a".repeat(64));
        payload.stats = Some(CommitStats {
            files_added: u64::MAX,
            files_modified: 1,
            files_deleted: 2,
        });
        payload.working_set_digest = Some("sha256:".to_owned() + &"d".repeat(64));
        payload.idle_observed_since = Some("2026-10-10T00:00:00Z".to_owned());
        payload.idle_observed_seconds = Some(0);
        payload.idle_threshold_seconds = Some(u64::MAX);
        payload.recovered_gc = true;
        payload.gc = Some(GcReport::OnIdleSkip(GcOnIdleSkip {
            status: "skipped".to_owned(),
            reason: gc_reason.to_owned(),
            mode: "on_idle".to_owned(),
        }));
        let mut legacy = inactive_auto_legacy();
        legacy["status"] = json!(status);
        legacy["reason"] = json!(reason);
        legacy["publication_status"] = json!("completed");
        legacy["snapshot_status"] = json!("completed");
        legacy["eligibility_reason"] = json!(eligibility_reason);
        legacy["eligible"] = json!(true);
        legacy["change_count"] = json!(u64::MAX);
        legacy["next_eligible_at"] = json!(next_eligible_at);
        legacy["commit_hash"] = json!("b".repeat(64));
        legacy["tree_hash"] = json!("a".repeat(64));
        legacy["stats"] =
            json!({"files_added": u64::MAX, "files_modified": 1_u64, "files_deleted": 2_u64});
        legacy["working_set_digest"] = json!("sha256:".to_owned() + &"d".repeat(64));
        legacy["idle_observed_since"] = json!("2026-10-10T00:00:00Z");
        legacy["idle_observed_seconds"] = json!(0_u64);
        legacy["idle_threshold_seconds"] = json!(u64::MAX);
        legacy["recovered_gc"] = json!(true);
        legacy["gc"] = json!({"status": "skipped", "reason": gc_reason, "mode": "on_idle"});
        assert_projection(payload, legacy);
    }
}

#[test]
fn snapshot_auto_projection_preserves_preflight_and_runtime_deferred_metadata() {
    for publication_status in ["not_started", "completed"] {
        let mut payload = inactive_auto();
        payload.status = "deferred".to_owned();
        let reason = if publication_status == "not_started" {
            "recovery_pending"
        } else {
            "max_runtime_seconds"
        };
        payload.reason = reason.to_owned();
        payload.publication_status = publication_status.to_owned();
        payload.recovered_gc = true;
        payload.recovery_pending = true;
        payload.error_code = Some("KIO-E-GC-RUNTIME-LIMIT-001".to_owned());
        payload.gc = Some(GcReport::AutomaticExecution(AutomaticGcReport {
            execution: runtime_report(GcSweepPhase::Receipting),
            trigger: "snapshot_auto".to_owned(),
            mode: "on_idle".to_owned(),
            max_runtime_seconds: 1,
            recovered_before_publication: true,
        }));
        let mut legacy = inactive_auto_legacy();
        legacy["status"] = json!("deferred");
        legacy["reason"] = json!(reason);
        legacy["publication_status"] = json!(publication_status);
        legacy["recovered_gc"] = json!(true);
        legacy["recovery_pending"] = json!(true);
        legacy["error_code"] = json!("KIO-E-GC-RUNTIME-LIMIT-001");
        if publication_status == "completed" {
            payload.change_count = Some(0);
            payload.working_set_digest = Some("sha256:".to_owned() + &"d".repeat(64));
            payload.idle_observed_since = Some("2026-10-10T00:00:00Z".to_owned());
            payload.idle_observed_seconds = Some(1);
            payload.idle_threshold_seconds = Some(1);
            payload.idle_eligible = true;
            legacy["change_count"] = json!(0_u64);
            legacy["working_set_digest"] = json!("sha256:".to_owned() + &"d".repeat(64));
            legacy["idle_observed_since"] = json!("2026-10-10T00:00:00Z");
            legacy["idle_observed_seconds"] = json!(1_u64);
            legacy["idle_threshold_seconds"] = json!(1_u64);
            legacy["idle_eligible"] = json!(true);
        }
        let mut gc = runtime_legacy(GcSweepPhase::Receipting);
        gc["trigger"] = json!("snapshot_auto");
        gc["mode"] = json!("on_idle");
        gc["max_runtime_seconds"] = json!(1_u64);
        gc["recovered_before_publication"] = json!(true);
        legacy["gc"] = gc;
        assert_projection(payload, legacy);
    }
}

#[test]
fn snapshot_auto_projection_preserves_gc_failure_code_and_explicit_recovery_flag() {
    for recovery_pending in [false, true] {
        let error = KioError::new(
            "KIO-E-GC-INDEX-BINDING-001",
            "index changed during the on-idle GC handoff",
            json!({}),
            ExitCode::PermanentFailure,
        );
        let mut payload = inactive_auto();
        payload.reason = "gc_failed".to_owned();
        payload.recovery_pending = recovery_pending;
        payload.error_code = Some(error.error_code().to_owned());
        payload.gc = Some(GcReport::OnIdleFailure(GcOnIdleFailure {
            status: "failed".to_owned(),
            reason: "gc_failed".to_owned(),
            mode: "on_idle".to_owned(),
            recovery_pending,
            error: GcErrorDetail {
                error_code: error.error_code().to_owned(),
                message: error.message().to_owned(),
                context: error.context().clone(),
            },
        }));
        let mut legacy = inactive_auto_legacy();
        legacy["reason"] = json!("gc_failed");
        legacy["recovery_pending"] = json!(recovery_pending);
        legacy["error_code"] = json!(error.error_code());
        legacy["gc"] = json!({"status": "failed", "reason": "gc_failed", "mode": "on_idle", "recovery_pending": recovery_pending, "error": error.to_error_json()});
        payload.change_count = Some(0);
        payload.next_eligible_at = Some("2026-10-10T00:01:00Z".to_owned());
        payload.working_set_digest = Some("sha256:".to_owned() + &"d".repeat(64));
        payload.idle_observed_since = Some("2026-10-10T00:00:00Z".to_owned());
        payload.idle_observed_seconds = Some(1);
        payload.idle_threshold_seconds = Some(1);
        payload.idle_eligible = true;
        legacy["change_count"] = json!(0_u64);
        legacy["next_eligible_at"] = json!("2026-10-10T00:01:00Z");
        legacy["working_set_digest"] = json!("sha256:".to_owned() + &"d".repeat(64));
        legacy["idle_observed_since"] = json!("2026-10-10T00:00:00Z");
        legacy["idle_observed_seconds"] = json!(1_u64);
        legacy["idle_threshold_seconds"] = json!(1_u64);
        legacy["idle_eligible"] = json!(true);
        assert_projection(payload, legacy);
    }
}

#[test]
fn gc_standalone_projection_preserves_marker_pending_and_plan_domain_shapes() {
    assert_projection(
        GcResponse::MarkerExchangePending(GcMarkerExchangePending {
            status: "recovery_pending".to_owned(),
            reason: "marker_exchange".to_owned(),
        }),
        json!({"status": "recovery_pending", "reason": "marker_exchange"}),
    );
    let marker = GcInProgressMarker {
        version: 1,
        sweep_id: "01K73J8M00AAAAAAAAAAAAAAAA".to_owned(),
        started_at: "2026-10-10T00:00:00Z".to_owned(),
        phase: GcSweepPhase::Prepared,
        plan_digest: "sha256:".to_owned() + &"a".repeat(64),
        truth_digest: "sha256:".to_owned() + &"b".repeat(64),
        stable_truth_digest: "sha256:".to_owned() + &"c".repeat(64),
        baseline_receipts_digest: "sha256:".to_owned() + &"d".repeat(64),
        operation_receipts_digest: None,
        candidates: Vec::new(),
        trees: Vec::new(),
        estimated_bytes: 0,
        index_initial: GcIndexState::Absent,
        index_pre_sweep: None,
        index_final: None,
        index_rotation: None,
    };
    let legacy = json!({"status": "recovery_pending", "recovery_pending": true, "marker": marker,
        "candidate_count": marker.candidates.len(), "candidate_tree_count": marker.trees.len(), "estimated_bytes": marker.estimated_bytes});
    assert_projection(
        GcResponse::ExistingMarkerPending(GcExistingMarkerPending {
            status: "recovery_pending".to_owned(),
            recovery_pending: true,
            candidate_count: marker.candidates.len(),
            candidate_tree_count: marker.trees.len(),
            estimated_bytes: marker.estimated_bytes,
            marker,
        }),
        legacy,
    );
    let plan = GcPlan {
        status: "dry_run".to_owned(),
        as_of: "2026-10-10T00:00:00Z".to_owned(),
        scope_path: "scope/日本語/.kio".to_owned(),
        policy: GcPolicy::default(),
        limits: GcPlanLimits::default(),
        stats: GcPlanStats::default(),
        stability_check_stats: GcPlanStats::default(),
        candidate_count: 0,
        candidate_tree_count: 0,
        estimated_bytes: 0,
        candidates: Vec::new(),
        exclusions: Vec::new(),
        object_kinds_planned: vec!["tree".to_owned()],
        truth_digest: "sha256:".to_owned() + &"a".repeat(64),
        stable_truth_digest: "sha256:".to_owned() + &"b".repeat(64),
        baseline_receipts_digest: "sha256:".to_owned() + &"c".repeat(64),
        plan_digest: "sha256:".to_owned() + &"d".repeat(64),
    };
    let legacy = json!(plan); // Current standalone run serializes the existing domain plan directly.
    assert_projection(GcResponse::Plan(plan), legacy);
    assert_projection(
        GcResponse::Execution(no_candidates()),
        no_candidates_legacy(),
    );
}
