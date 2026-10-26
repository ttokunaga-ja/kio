// Typed projection goldens preserve the existing command wire shapes.
use super::gc::{
    AutomaticGcReport, GcAutomationPolicyReport, GcExecutionReport, GcPreflightRecovered, GcReport,
    GcRuntimeDeferredReport, PrepublicationDeferredResponse,
};
use super::index::{
    AdapterExecutionMode, ChildPreviewRow, ChildPublishedRow, IndexPreviewResponse,
    IndexPublishedResponse, LargeFile, NetworkTransmissionPolicy, PreviewChildDiscovery,
    PublishedChildDiscovery, SensitiveCandidate,
};
use super::skipped_units::SkippedDisclosure;
use kio_core::dag::{CommitObject, CommitStats, CommitType};
use kio_core::gc::{GcIndexState, GcSweepPhase};
use kio_pipeline::scan::{ChildScopeDiscovery, CostPreview, ScanCandidate};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::PathBuf;

fn assert_projection<T: Serialize>(payload: T, legacy: Value) {
    let actual = serde_json::to_value(payload).expect("project concrete response DTO");
    assert_eq!(actual, legacy);
    assert_eq!(
        serde_json::to_vec(&actual).unwrap(),
        serde_json::to_vec(&legacy).unwrap()
    );
}

fn empty_preview() -> IndexPreviewResponse {
    IndexPreviewResponse {
        status: "preview".to_owned(),
        root: PathBuf::from("scope/日本語"),
        scope: "scope-fixture".to_owned(),
        estimated_file_count: 0,
        estimated_size_bytes: 0,
        large_files: Vec::new(),
        effective_ignore: ".kioignore + built-in secrets Tier A".to_owned(),
        excluded_candidates: Vec::new(),
        sensitive_candidates: Vec::new(),
        network_transmission_policy: NetworkTransmissionPolicy {
            default: "disabled until adapter approve".to_owned(),
            yes_grants_network: false,
        },
        adapter_execution_mode: AdapterExecutionMode {
            markdownize: "deterministic_library baseline".to_owned(),
            embedding: "not generated without online opt-in".to_owned(),
        },
        estimated_cost: None,
        budget_cap: None,
        estimated_completion:
            "baseline completes in this run; online enhancement depends on budget and opt-in"
                .to_owned(),
        candidates: Vec::new(),
        child_scopes: None,
        estimated_aggregate_file_count: None,
        estimated_aggregate_size_bytes: None,
        estimated_aggregate_cost_usd: None,
        child_scope_discovery: None,
    }
}

fn empty_preview_legacy() -> Value {
    json!({
        "status": "preview", "root": PathBuf::from("scope/日本語"), "scope": "scope-fixture",
        "estimated_file_count": 0_usize, "estimated_size_bytes": 0_u64, "large_files": [],
        "effective_ignore": ".kioignore + built-in secrets Tier A", "excluded_candidates": [], "sensitive_candidates": [],
        "network_transmission_policy": {"default": "disabled until adapter approve", "yes_grants_network": false},
        "adapter_execution_mode": {"markdownize": "deterministic_library baseline", "embedding": "not generated without online opt-in"},
        "estimated_cost": null, "budget_cap": null,
        "estimated_completion": "baseline completes in this run; online enhancement depends on budget and opt-in", "candidates": [],
    })
}

#[test]
fn index_preview_projection_preserves_empty_arrays_null_cost_and_absent_parent_fields() {
    assert_projection(empty_preview(), empty_preview_legacy());
}

#[test]
fn index_preview_projection_preserves_domain_candidates_costs_and_parent_child_shapes() {
    let candidate = ScanCandidate {
        input_path: "large 日本語.pdf".to_owned(),
        media_type: "application/pdf".to_owned(),
        size_bytes: u64::MAX,
        raw_hash: None,
        ignored: true,
        quarantine_reason: Some("tier_a".to_owned()),
    };
    let cost = CostPreview {
        estimated_usd: 0.125,
        budget_cap_usd: Some(0.0625),
        budget_warning: Some("warning 日本語".to_owned()),
        estimated_markdownize_usd: 0.03125,
        estimated_embedding_usd: 0.09375,
    };
    let planned = ChildScopeDiscovery {
        path: "子".to_owned(),
        status: "planned".to_owned(),
        reason: None,
        error_code: None,
        message: None,
    };
    let late_vcs = ChildScopeDiscovery {
        path: "vcs".to_owned(),
        status: "skipped_vcs".to_owned(),
        reason: Some("vcs_marker_added_after_discovery".to_owned()),
        error_code: None,
        message: None,
    };
    let mut legacy = empty_preview_legacy();
    legacy["large_files"] =
        json!([{"path": candidate.input_path, "size_bytes": candidate.size_bytes}]);
    legacy["excluded_candidates"] = json!([candidate.input_path]);
    legacy["sensitive_candidates"] = json!([{"path": candidate.input_path, "reason": candidate.quarantine_reason, "ignored": candidate.ignored}]);
    legacy["candidates"] = json!([candidate]);
    legacy["estimated_cost"] = json!(cost);
    legacy["budget_cap"] = json!(cost.budget_cap_usd);
    legacy["child_scopes"] = json!([
        {"path": "子", "status": "planned", "estimated_file_count": 2_usize, "estimated_size_bytes": u64::MAX, "estimated_cost_usd": 0.25},
        {"path": "vcs", "status": "skipped_vcs", "reason": "vcs_marker_added_after_discovery"},
    ]);
    legacy["estimated_aggregate_file_count"] = json!(2_usize);
    legacy["estimated_aggregate_size_bytes"] = json!(u64::MAX);
    legacy["estimated_aggregate_cost_usd"] = json!(0.375);
    legacy["child_scope_discovery"] =
        json!({"page_size": 128_usize, "has_more": true, "preview": true});
    let mut payload = empty_preview();
    payload.large_files = vec![LargeFile {
        path: candidate.input_path.clone(),
        size_bytes: candidate.size_bytes,
    }];
    payload.excluded_candidates = vec![candidate.input_path.clone()];
    payload.sensitive_candidates = vec![SensitiveCandidate {
        path: candidate.input_path.clone(),
        reason: candidate.quarantine_reason.clone().unwrap(),
        ignored: candidate.ignored,
    }];
    payload.candidates = vec![candidate];
    payload.estimated_cost = Some(cost);
    payload.budget_cap = Some(0.0625);
    payload.child_scopes = Some(vec![
        ChildPreviewRow {
            discovery: planned,
            estimated_file_count: Some(2),
            estimated_size_bytes: Some(u64::MAX),
            estimated_cost_usd: Some(0.25),
        },
        ChildPreviewRow {
            discovery: late_vcs,
            estimated_file_count: None,
            estimated_size_bytes: None,
            estimated_cost_usd: None,
        },
    ]);
    payload.estimated_aggregate_file_count = Some(2);
    payload.estimated_aggregate_size_bytes = Some(u64::MAX);
    payload.estimated_aggregate_cost_usd = Some(0.375);
    payload.child_scope_discovery = Some(PreviewChildDiscovery {
        page_size: 128,
        has_more: true,
        preview: true,
    });
    assert_projection(payload, legacy);
}

fn published() -> IndexPublishedResponse {
    IndexPublishedResponse {
        status: "noop".to_owned(),
        approval_method: "existing".to_owned(),
        network_allowed: false,
        network_opt_in: false,
        pending_online_tasks: 0,
        paused_tasks: 0,
        failed_files: 0,
        normalized_files: 0,
        pending_files: 0,
        skipped_oversized_files: 0,
        skipped_unrecognized_binary_files: 0,
        embedding_tasks_executed: 0,
        embedding_tasks_failed: 0,
        tree_hash: "a".repeat(64),
        commit_hash: None,
        commit: None,
        budget_status: "known".to_owned(),
        budget_reason: None,
        budget_warning: None,
        skipped: SkippedDisclosure {
            skipped_units: Vec::new(),
            skipped_units_guidance: None,
        },
        error_code: None,
        gc: None,
        publication_status: None,
        child_scopes: None,
        child_scope_discovery: None,
    }
}

fn published_legacy() -> Value {
    json!({
        "status": "noop", "approval_method": "existing", "network_allowed": false, "network_opt_in": false,
        "pending_online_tasks": 0_usize, "paused_tasks": 0_usize, "failed_files": 0_usize, "normalized_files": 0_usize, "pending_files": 0_usize,
        "skipped_oversized_files": 0_usize, "skipped_unrecognized_binary_files": 0_usize, "embedding_tasks_executed": 0_usize, "embedding_tasks_failed": 0_usize,
        "tree_hash": "a".repeat(64), "commit_hash": null, "commit": null, "budget_status": "known", "budget_reason": null, "budget_warning": null, "skipped_units": [],
    })
}

#[test]
fn index_published_projection_preserves_noop_nulls_empty_disclosure_and_absent_decorations() {
    assert_projection(published(), published_legacy());
}

#[test]
fn index_published_projection_preserves_commit_domain_payload_and_successful_gc() {
    let commit = CommitObject {
        commit_type: CommitType::Auto,
        created_at: "2026-10-10T00:00:00Z".to_owned(),
        message: "kio index auto snapshot".to_owned(),
        object_type: "commit".to_owned(),
        parent: None,
        stats: CommitStats {
            files_added: u64::MAX,
            files_modified: 1,
            files_deleted: 0,
        },
        tool_lock_hash: "c".repeat(64),
        tree: "a".repeat(64),
        purged_raws: Vec::new(),
        restore_provenance: None,
    };
    let mut legacy = published_legacy();
    legacy["status"] = json!("indexed");
    legacy["approval_method"] = json!("yes");
    legacy["network_allowed"] = json!(true);
    legacy["network_opt_in"] = json!(true);
    legacy["commit_hash"] = json!("b".repeat(64));
    legacy["commit"] = json!(commit);
    legacy["budget_warning"] = json!("non-blocking warning");
    legacy["gc"] = json!({"status": "disabled", "reason": "manual_only", "trigger": "index", "mode": "manual_only"});
    let mut payload = published();
    payload.status = "indexed".to_owned();
    payload.approval_method = "yes".to_owned();
    payload.network_allowed = true;
    payload.network_opt_in = true;
    payload.commit_hash = Some("b".repeat(64));
    payload.commit = Some(commit);
    payload.budget_warning = Some("non-blocking warning".to_owned());
    payload.gc = Some(GcReport::AutomationPolicy(GcAutomationPolicyReport {
        status: "disabled".to_owned(),
        reason: "manual_only".to_owned(),
        trigger: "index".to_owned(),
        mode: "manual_only".to_owned(),
    }));
    assert_projection(payload, legacy);
}

#[test]
fn index_published_projection_preserves_partial_auth_budget_metadata_without_exit_fields() {
    // Projection-only bodies: runtime Completion priority is tested separately.
    for (code, failed_files, paused, failed) in [
        (Some("KIO-E-INDEX-PARTIAL-001"), 1_usize, 0_usize, 0_usize),
        (None, 0, 0, 1), // Direct child enrichment auth has no invented error_code.
        (None, 0, 2, 0), // Direct child enrichment budget has no invented error_code.
        (Some("KIO-E-ADAPTER-AUTH-001"), 0, 0, 1),
        (Some("KIO-E-BUDGET-EXCEEDED-001"), 0, 2, 0),
        (Some("KIO-E-INDEX-PARTIAL-001"), 1, 2, 1),
    ] {
        let mut payload = published();
        payload.failed_files = failed_files;
        payload.paused_tasks = paused;
        payload.embedding_tasks_failed = failed;
        payload.error_code = code.map(str::to_owned);
        payload.gc = Some(GcReport::PreflightRecovered(GcPreflightRecovered {
            status: "completed".to_owned(),
            trigger: "preflight_recovery".to_owned(),
            mode: "after_index".to_owned(),
            recovered_before_publication: true,
        }));
        let mut legacy = published_legacy();
        legacy["failed_files"] = json!(failed_files);
        legacy["paused_tasks"] = json!(paused);
        legacy["embedding_tasks_failed"] = json!(failed);
        if let Some(code) = code {
            legacy["error_code"] = json!(code);
        }
        legacy["gc"] = json!({"status": "completed", "trigger": "preflight_recovery", "mode": "after_index", "recovered_before_publication": true});
        assert_projection(payload, legacy);
    }
}

#[test]
fn index_published_projection_preserves_child_errors_gc_and_discovery_widths() {
    let mut payload = published();
    payload.child_scopes = Some(vec![
        ChildPublishedRow {
            discovery: ChildScopeDiscovery {
                path: "子".to_owned(),
                status: "indexed_partial".to_owned(),
                reason: None,
                error_code: Some("KIO-E-SEARCH-PARTIAL-001".to_owned()),
                message: Some("child index completed with a non-success exit".to_owned()),
            },
            gc: Some(GcReport::PreflightRecovered(GcPreflightRecovered {
                status: "completed".to_owned(),
                trigger: "preflight_recovery".to_owned(),
                mode: "after_index".to_owned(),
                recovered_before_publication: true,
            })),
        },
        ChildPublishedRow {
            discovery: ChildScopeDiscovery {
                path: "empty".to_owned(),
                status: "indexed".to_owned(),
                reason: None,
                error_code: None,
                message: None,
            },
            gc: None,
        },
    ]);
    payload.child_scope_discovery = Some(PublishedChildDiscovery {
        page_size: 128,
        has_more: true,
        continuation_committed: false,
        restarted_from_root: true,
        rows_total: u64::MAX,
        rows_omitted: u64::MAX - 2,
    });
    let mut legacy = published_legacy();
    legacy["child_scopes"] = json!([
        {"path": "子", "status": "indexed_partial", "error_code": "KIO-E-SEARCH-PARTIAL-001", "message": "child index completed with a non-success exit",
            "gc": {"status": "completed", "trigger": "preflight_recovery", "mode": "after_index", "recovered_before_publication": true}},
        {"path": "empty", "status": "indexed"},
    ]);
    legacy["child_scope_discovery"] = json!({"page_size": 128_usize, "has_more": true, "continuation_committed": false, "restarted_from_root": true,
        "rows_total": u64::MAX, "rows_omitted": u64::MAX - 2});
    assert_projection(payload, legacy);
}

#[test]
fn index_child_projection_preserves_current_non_success_fallback_codes() {
    for error_code in [
        "KIO-E-ADAPTER-AUTH-001",
        "KIO-E-BUDGET-EXCEEDED-001",
        "KIO-E-SEARCH-PARTIAL-001",
        "KIO-E-SEARCH-SCOPE-ALL-FAILED-001",
    ] {
        assert_projection(
            ChildPublishedRow {
                discovery: ChildScopeDiscovery {
                    path: "子".to_owned(),
                    status: "indexed_partial".to_owned(),
                    reason: None,
                    error_code: Some(error_code.to_owned()),
                    message: Some("child index completed with a non-success exit".to_owned()),
                },
                gc: None,
            },
            json!({"path": "子", "status": "indexed_partial", "error_code": error_code, "message": "child index completed with a non-success exit"}),
        );
    }
}

#[test]
fn index_prepublication_deferred_projection_preserves_unstarted_distinct_shape() {
    let report = GcReport::AutomaticExecution(AutomaticGcReport {
        execution: GcExecutionReport::RuntimeDeferred(GcRuntimeDeferredReport {
            status: "deferred".to_owned(),
            reason: "max_runtime_seconds".to_owned(),
            recovery_pending: true,
            sweep_id: "01K73J8M00AAAAAAAAAAAAAAAA".to_owned(),
            phase: GcSweepPhase::Prepared,
            candidate_count: 1,
            candidate_tree_count: 1,
            estimated_bytes: 1,
            index_initial: GcIndexState::Absent,
            index_pre_sweep: None,
            index_final: None,
        }),
        trigger: "index".to_owned(),
        mode: "after_index".to_owned(),
        max_runtime_seconds: 1,
        recovered_before_publication: true,
    });
    assert_projection(
        PrepublicationDeferredResponse {
            status: "deferred".to_owned(),
            publication_status: "not_started".to_owned(),
            error_code: "KIO-E-GC-RUNTIME-LIMIT-001".to_owned(),
            message: "automatic GC recovery reached max_runtime_seconds before publication"
                .to_owned(),
            trigger: "index".to_owned(),
            gc: report,
        },
        json!({
            "status": "deferred", "publication_status": "not_started", "error_code": "KIO-E-GC-RUNTIME-LIMIT-001",
            "message": "automatic GC recovery reached max_runtime_seconds before publication", "trigger": "index",
            "gc": {"status": "deferred", "reason": "max_runtime_seconds", "recovery_pending": true,
                "sweep_id": "01K73J8M00AAAAAAAAAAAAAAAA", "phase": "prepared", "candidate_count": 1_usize,
                "candidate_tree_count": 1_usize, "estimated_bytes": 1_u64, "index_initial": {"kind": "absent"},
                "index_pre_sweep": null, "index_final": null, "trigger": "index", "mode": "after_index",
                "max_runtime_seconds": 1_u64, "recovered_before_publication": true},
        }),
    );
}
