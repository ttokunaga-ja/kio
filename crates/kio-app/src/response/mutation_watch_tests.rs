use super::mutation::*;
use super::skipped_units::SkippedUnit;
use super::watch::*;
use super::{CommandResponse, Completion, ResponseBody};
use kio_core::purge::{PurgePhase, PurgeReason, TombstoneMode};
use kio_core::scope::{ManagedRestoreChange, ManagedRestoreOutcome};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::PathBuf;

fn assert_projection<T: Serialize>(payload: T, legacy: Value) {
    let actual = serde_json::to_value(payload).expect("concrete payload projection");
    assert_eq!(actual, legacy);
    assert_eq!(
        serde_json::to_vec(&actual).unwrap(),
        serde_json::to_vec(&legacy).unwrap()
    );
}
fn export_success(status: &str, populated: bool) -> ExportSuccess {
    ExportSuccess {
        status: status.into(),
        source_kind: "commit".into(),
        source_commit: "head".into(),
        destination: "output/日本語".into(),
        exported_count: usize::from(populated),
        overwritten_count: u64::from(populated),
        files: if populated {
            vec![ExportedFile {
                path: "output/日本語/file.md".into(),
                path_at_commit: "file.md".into(),
                raw_hash: "raw".into(),
                overwritten: true,
            }]
        } else {
            vec![]
        },
    }
}
fn expected_export(status: &str, populated: bool) -> Value {
    json!({"status":status,"source_kind":"commit","source_commit":"head","destination":PathBuf::from("output/日本語"),
        "exported_count":usize::from(populated),"overwritten_count":u64::from(populated),
        "files":if populated { vec![json!({"path":PathBuf::from("output/日本語/file.md"),"path_at_commit":"file.md",
            "raw_hash":"raw","overwritten":true})] } else { vec![] }})
}
#[test]
fn export_success_projection_preserves_native_paths_and_omits_failure_fields() {
    for populated in [false, true] {
        assert_projection(
            ExportResponse::Exported(export_success("exported", populated)),
            expected_export("exported", populated),
        );
    }
}
#[test]
fn export_partial_projection_keeps_zero_published_files_and_closed_failure_row() {
    for populated in [false, true] {
        let mut expected = expected_export("partial", populated);
        expected["error_code"] = json!("KIO-E-COMMIT-EXPORT-PARTIAL-001");
        expected["failed"] = json!([{"path":PathBuf::from("output/日本語/failed.md"),"path_at_commit":"failed.md",
            "raw_hash":"failed-raw","error_code":"KIO-E-STORE-IO-001","message":"write failed"}]);
        assert_projection(
            ExportResponse::Partial(ExportPartial {
                report: export_success("partial", populated),
                error_code: "KIO-E-COMMIT-EXPORT-PARTIAL-001".into(),
                failed: vec![ExportFailure {
                    path: "output/日本語/failed.md".into(),
                    path_at_commit: "failed.md".into(),
                    raw_hash: "failed-raw".into(),
                    error_code: "KIO-E-STORE-IO-001".into(),
                    message: "write failed".into(),
                }],
            }),
            expected,
        );
    }
}
fn restore(status: &str, noop: bool, applied: bool) -> RestoreResponse {
    RestoreResponse {
        operation: "managed_restore".into(),
        status: status.into(),
        source_commit: "source".into(),
        expected_head: "parent".into(),
        new_commit_hash: (!noop).then(|| "new".into()),
        new_head: (applied && !noop).then(|| "new".into()),
        new_tree_hash: "tree".into(),
        changes: if noop {
            vec![]
        } else {
            vec![ManagedRestoreChange {
                path: "file.md".into(),
                old_raw_hash: None,
                new_raw_hash: Some("raw".into()),
            }]
        },
        provenance: RestoreProvenance {
            source_commit: "source".into(),
            parent_commit: "parent".into(),
            commit_type: "restored".into(),
        },
        apply: applied.then(|| ManagedRestoreOutcome {
            noop,
            commit_hash: (!noop).then(|| "new".into()),
            tree_hash: "tree".into(),
            restored_paths: if noop { vec![] } else { vec!["file.md".into()] },
        }),
        egress: false,
        projection_status: None,
        projection: None,
        projection_error: None,
    }
}
fn expected_restore(status: &str, noop: bool, applied: bool) -> Value {
    json!({"operation":"managed_restore","status":status,"source_commit":"source","expected_head":"parent",
        "new_commit_hash":if noop {None} else {Some("new")},"new_head":if applied && !noop {Some("new")} else {None},
        "new_tree_hash":"tree","changes":if noop {vec![]} else {vec![json!({"path":"file.md","old_raw_hash":null,"new_raw_hash":"raw"})]},
        "provenance":{"source_commit":"source","parent_commit":"parent","commit_type":"restored"},
        "apply":if applied {Some(json!({"noop":noop,"commit_hash":if noop {None} else {Some("new")},
            "tree_hash":"tree","restored_paths":if noop {vec![]} else {vec!["file.md"]}}))} else {None},"egress":false})
}
#[test]
fn restore_preview_projection_preserves_nulls_changes_and_absent_projection_fields() {
    for noop in [false, true] {
        assert_projection(
            restore("preview", noop, false),
            expected_restore("preview", noop, false),
        );
    }
}
#[test]
fn restore_applied_noop_projection_preserves_null_head_and_unchanged_status_only() {
    let mut dto = restore("applied", true, true);
    dto.projection_status = Some("unchanged".into());
    let mut expected = expected_restore("applied", true, true);
    expected["projection_status"] = json!("unchanged");
    assert_projection(dto, expected);
}
#[test]
fn restore_ready_projection_preserves_raw_skipped_units_without_disclosure_keys() {
    let mut dto = restore("applied", false, true);
    dto.projection_status = Some("ready".into());
    dto.projection = Some(RestoreProjection {
        status: "ready".into(),
        rebuilt_chunks: 2,
        rebuilt_tree_entries: 3,
        skipped_units: vec![SkippedUnit {
            raw_hash: "raw".into(),
            path: "file.md".into(),
            r#gen: 1,
            reason: "missing".into(),
        }],
        egress: false,
    });
    let mut expected = expected_restore("applied", false, true);
    expected["projection_status"] = json!("ready");
    expected["projection"] = json!({"status":"ready","rebuilt_chunks":2,"rebuilt_tree_entries":3,
        "skipped_units":[{"raw_hash":"raw","path":"file.md","gen":1,"reason":"missing"}],"egress":false});
    assert_projection(dto, expected);
}
#[test]
fn restore_failed_projection_keeps_applied_head_and_finite_error_row() {
    let mut dto = restore("applied_projection_failed", false, true);
    dto.projection_status = Some("failed".into());
    dto.projection_error = Some(RestoreProjectionError {
        error_code: "KIO-E-MANAGED-RESTORE-PROJECTION-001".into(),
        message: "cache failed".into(),
    });
    let mut expected = expected_restore("applied_projection_failed", false, true);
    expected["projection_status"] = json!("failed");
    expected["projection_error"] =
        json!({"error_code":"KIO-E-MANAGED-RESTORE-PROJECTION-001","message":"cache failed"});
    assert_projection(dto, expected);
}
fn deleted_counts() -> DeletedCounts {
    DeletedCounts {
        raw_objects: 1,
        chunk_objects: 2,
        chunk_ledger_rows: 3,
        sqlite_chunks: 4,
        sqlite_associations: 5,
        sqlite_vectors: 6,
        sqlite_orphan_embeddings: 7,
        normalized_instances: 8,
        normalized_views: 9,
        prepared_objects: 10,
        image_objects: 11,
        tasks: 12,
        reservations: 13,
        manifest_rows: 14,
        unsupported_rows: 15,
        quarantine_rows: 16,
        cache_directories: 17,
        staging_descriptors: 18,
        manifest_objects: 19,
        normalized_unit_objects: 20,
    }
}
fn expected_counts() -> Value {
    json!({"raw_objects":1,"chunk_objects":2,"chunk_ledger_rows":3,"sqlite_chunks":4,"sqlite_associations":5,
        "sqlite_vectors":6,"sqlite_orphan_embeddings":7,"normalized_instances":8,"normalized_views":9,
        "prepared_objects":10,"image_objects":11,"tasks":12,"reservations":13,"manifest_rows":14,"unsupported_rows":15,
        "quarantine_rows":16,"cache_directories":17,"staging_descriptors":18,"manifest_objects":19,"normalized_unit_objects":20})
}
fn purge(status: &str, commit: Option<&str>, warning: bool) -> PurgeSuccess {
    PurgeSuccess {
        status: status.into(),
        purged_in_commit: commit.map(str::to_owned),
        reason: PurgeReason::Privacy,
        target_raw_count: 2,
        deleted_counts: deleted_counts(),
        shared_artifacts_preserved: SharedArtifactsPreserved {
            prepared_objects: 21,
            image_objects: 22,
            normalized_unit_objects: 23,
        },
        tombstone_mode: TombstoneMode::Default,
        tombstone_count: 24,
        erase_receipt_count: 25,
        logs_scrubbed: true,
        log_files_scrubbed: 26,
        log_rows_removed: 27,
        log_fields_masked: 28,
        guarantee: "removed from KIO-managed history".into(),
        not_covered: vec![
            "external backups and Time Machine".into(),
            "exported or manually copied files".into(),
            "cloud-sync past versions".into(),
            "logs outside Kio management".into(),
        ],
        working_tree_warning: warning.then(|| WorkingTreeWarning {
            live_alias_count: 2,
            message: warning_text().into(),
        }),
    }
}
fn warning_text() -> &'static str {
    "warning: 2 working-tree file(s) still contain the exact bytes being purged. The next `kio index` will re-discover and re-ingest them, making their pointer alive again. To permanently exclude them, delete the file(s) or add them to `.kioignore`."
}
fn expected_purge(status: &str, commit: Option<&str>, warning: bool) -> Value {
    let mut expected = json!({"status":status,"purged_in_commit":commit,"reason":"privacy","target_raw_count":2,
        "deleted_counts":expected_counts(),"shared_artifacts_preserved":{"prepared_objects":21,"image_objects":22,"normalized_unit_objects":23},
        "tombstone_mode":"default","tombstone_count":24,"erase_receipt_count":25,"logs_scrubbed":true,
        "log_files_scrubbed":26,"log_rows_removed":27,"log_fields_masked":28,"guarantee":"removed from KIO-managed history",
        "not_covered":["external backups and Time Machine","exported or manually copied files","cloud-sync past versions","logs outside Kio management"]});
    if warning {
        expected["working_tree_warning"] = json!({"live_alias_count":2,"message":warning_text()});
    }
    expected
}
#[test]
fn purge_success_projection_preserves_nullable_commit_all_counts_and_warning_omission() {
    for commit in [None, Some("purged-commit")] {
        for warning in [false, true] {
            assert_projection(
                PurgeResponse::Purged(purge("purged", commit, warning)),
                expected_purge("purged", commit, warning),
            );
        }
    }
}
#[test]
fn purge_incomplete_projection_preserves_phase_and_logs_scrubbed_boundary() {
    for (phase, wire, scrubbed) in [
        (PurgePhase::Prepared, "prepared", false),
        (PurgePhase::Tombstoned, "tombstoned", false),
        (PurgePhase::Deleted, "deleted", true),
        (PurgePhase::Committed, "committed", true),
    ] {
        let mut report = purge("purge_incomplete", None, true);
        report.logs_scrubbed = scrubbed;
        let mut expected = expected_purge("purge_incomplete", None, true);
        expected["logs_scrubbed"] = json!(scrubbed);
        expected["error_code"] = json!("KIO-E-PURGE-INCOMPLETE-001");
        expected["message"] =
            json!("purge is incomplete and will resume on the next identical command");
        expected["failure_phase"] = json!(wire);
        expected["cause_error_code"] = json!("KIO-E-STORE-IO-001");
        assert_projection(
            PurgeResponse::Incomplete(PurgeIncomplete {
                report,
                error_code: "KIO-E-PURGE-INCOMPLETE-001".into(),
                message: "purge is incomplete and will resume on the next identical command".into(),
                failure_phase: phase,
                cause_error_code: "KIO-E-STORE-IO-001".into(),
            }),
            expected,
        );
    }
}
#[test]
fn purge_counter_storage_defaults_remain_loadable_and_serialize_all_fields() {
    let mut old_counts = expected_counts();
    for optional_default in [
        "staging_descriptors",
        "manifest_objects",
        "normalized_unit_objects",
    ] {
        old_counts.as_object_mut().unwrap().remove(optional_default);
    }
    let loaded: DeletedCounts = serde_json::from_value(old_counts).unwrap();
    let mut expected = expected_counts();
    for optional_default in [
        "staging_descriptors",
        "manifest_objects",
        "normalized_unit_objects",
    ] {
        expected[optional_default] = json!(0);
    }
    assert_projection(loaded, expected);
    let loaded: SharedArtifactsPreserved =
        serde_json::from_value(json!({"prepared_objects":1,"image_objects":2})).unwrap();
    assert_projection(
        loaded,
        json!({"prepared_objects":1,"image_objects":2,"normalized_unit_objects":0}),
    );
}
fn observation() -> WatchObservation {
    WatchObservation {
        version: 1,
        instance: "instance".into(),
        root: "scope/日本語".into(),
        scope_id: "s".into(),
        pid: 123,
        backend: "native".into(),
        backlog: 4,
        degraded: true,
        last_success_ms: None,
        last_failure: Some("retry queued".into()),
        observed_at_ms: 5,
        reconcile_interval_seconds: 6,
    }
}
fn expected_observation() -> Value {
    json!({"version":1,"instance":"instance","root":PathBuf::from("scope/日本語"),"scope_id":"s","pid":123,
        "backend":"native","backlog":4,"degraded":true,"last_success_ms":null,"last_failure":"retry queued",
        "observed_at_ms":5,"reconcile_interval_seconds":6})
}
#[test]
fn watch_lifecycle_projection_preserves_nullable_observation_and_degraded_state() {
    for status in ["running", "stopped", "stop_requested"] {
        assert_projection(
            WatchLifecycleResponse {
                status: status.into(),
                root: "scope/日本語".into(),
                last_observation: None,
            },
            json!({"status":status,"root":PathBuf::from("scope/日本語"),"last_observation":null}),
        );
        assert_projection(
            WatchLifecycleResponse {
                status: status.into(),
                root: "scope/日本語".into(),
                last_observation: Some(observation()),
            },
            json!({"status":status,"root":PathBuf::from("scope/日本語"),"last_observation":expected_observation()}),
        );
    }
}
#[test]
fn watch_observation_storage_retains_strict_schema_and_nullable_history() {
    let mut row = expected_observation();
    row["last_success_ms"] = json!(7);
    row["last_failure"] = Value::Null;
    let loaded: WatchObservation = serde_json::from_value(row.clone()).unwrap();
    assert_projection(loaded, row.clone());
    row["unexpected"] = json!(true);
    assert!(serde_json::from_value::<WatchObservation>(row).is_err());
}
#[test]
fn watch_service_projection_keeps_installed_only_native_registration_field() {
    assert_projection(
        WatchResponse::ServiceInstalled(WatchServiceInstalledResponse {
            status: "installed".into(),
            service_id: "service".into(),
            root: "scope/日本語".into(),
            native_registration: "enabled_not_started".into(),
        }),
        json!({"status":"installed","service_id":"service","root":PathBuf::from("scope/日本語"),"native_registration":"enabled_not_started"}),
    );
    for status in ["start_requested", "stopped", "uninstalled"] {
        assert_projection(
            WatchResponse::ServiceLifecycle(WatchServiceLifecycleResponse {
                status: status.into(),
                service_id: "service".into(),
                root: "scope/日本語".into(),
            }),
            json!({"status":status,"service_id":"service","root":PathBuf::from("scope/日本語")}),
        );
    }
}
#[test]
fn watch_service_status_projection_preserves_phase_native_booleans_and_instance() {
    for (phase, wire) in [
        (ServicePhase::Installing, "installing"),
        (ServicePhase::Installed, "installed"),
    ] {
        for (registered, active) in [(false, false), (false, true), (true, false), (true, true)] {
            assert_projection(
                WatchResponse::ServiceStatus(WatchServiceStatusResponse {
                    status: "service".into(),
                    service_id: "service".into(),
                    root: "scope/日本語".into(),
                    install_phase: phase,
                    native_registration: NativeServiceState { registered, active },
                    instance: WatchLifecycleResponse {
                        status: "running".into(),
                        root: "scope/日本語".into(),
                        last_observation: Some(observation()),
                    },
                }),
                json!({"status":"service","service_id":"service","root":PathBuf::from("scope/日本語"),"install_phase":wire,
                    "native_registration":{"registered":registered,"active":active},
                    "instance":{"status":"running","root":PathBuf::from("scope/日本語"),"last_observation":expected_observation()}}),
            );
        }
    }
}
#[test]
fn watch_runner_projection_has_no_root_or_child_exit_field() {
    assert_projection(
        WatchResponse::ServiceRunner(WatchServiceRunnerResponse {
            status: "watcher_exited".into(),
            service_id: "service".into(),
        }),
        json!({"status":"watcher_exited","service_id":"service"}),
    );
}
#[test]
fn mutation_legacy_projection_preserves_partial_body_and_marker_outside_payload() {
    let body = ResponseBody::Export(ExportResponse::Partial(ExportPartial {
        report: export_success("partial", false),
        error_code: "KIO-E-COMMIT-EXPORT-PARTIAL-001".into(),
        failed: vec![ExportFailure {
            path: "failed".into(),
            path_at_commit: "file".into(),
            raw_hash: "raw".into(),
            error_code: "KIO-E-STORE-IO-001".into(),
            message: "failed".into(),
        }],
    }));
    let mut expected = expected_export("partial", false);
    expected["error_code"] = json!("KIO-E-COMMIT-EXPORT-PARTIAL-001");
    expected["failed"] = json!([{"path":PathBuf::from("failed"),"path_at_commit":"file","raw_hash":"raw",
        "error_code":"KIO-E-STORE-IO-001","message":"failed"}]);
    let response = CommandResponse {
        body,
        completion: Completion::Partial,
    };
    assert_eq!(response.completion, Completion::Partial);
    let actual = serde_json::to_value(response.body).unwrap();
    assert_eq!(actual, expected);
    assert_eq!(
        serde_json::to_vec(&actual).unwrap(),
        serde_json::to_vec(&expected).unwrap()
    );
}
#[test]
fn watch_success_with_degraded_observation_projects_as_complete() {
    let body = ResponseBody::Watch(WatchResponse::Foreground(WatchLifecycleResponse {
        status: "stopped".into(),
        root: "scope/日本語".into(),
        last_observation: Some(observation()),
    }));
    let response = CommandResponse {
        body,
        completion: Completion::Complete,
    };
    assert_eq!(response.completion, Completion::Complete);
    let actual = serde_json::to_value(response.body).unwrap();
    let expected = json!({"status":"stopped","root":PathBuf::from("scope/日本語"),"last_observation":expected_observation()});
    assert_eq!(actual, expected);
    assert_eq!(
        serde_json::to_vec(&actual).unwrap(),
        serde_json::to_vec(&expected).unwrap()
    );
}
#[test]
fn purge_completed_terminal_projection_keeps_zeroed_nonterminal_counters() {
    let mut report = purge("purged", Some("completed-head"), false);
    report.deleted_counts = DeletedCounts::default();
    report.shared_artifacts_preserved = SharedArtifactsPreserved::default();
    report.tombstone_count = 2;
    report.erase_receipt_count = 3;
    report.log_files_scrubbed = 0;
    report.log_rows_removed = 0;
    report.log_fields_masked = 0;
    let mut expected = expected_purge("purged", Some("completed-head"), false);
    for count in expected["deleted_counts"]
        .as_object_mut()
        .unwrap()
        .values_mut()
    {
        *count = json!(0);
    }
    expected["shared_artifacts_preserved"] =
        json!({"prepared_objects":0,"image_objects":0,"normalized_unit_objects":0});
    expected["tombstone_count"] = json!(2);
    expected["erase_receipt_count"] = json!(3);
    expected["log_files_scrubbed"] = json!(0);
    expected["log_rows_removed"] = json!(0);
    expected["log_fields_masked"] = json!(0);
    assert_projection(PurgeResponse::Purged(report), expected);
}
#[test]
fn mutation_domain_projection_preserves_reason_and_tombstone_mode_wire_strings() {
    for (reason, wire) in [
        (PurgeReason::Legal, "legal"),
        (PurgeReason::Privacy, "privacy"),
        (PurgeReason::Misingest, "misingest"),
        (PurgeReason::Copyright, "copyright"),
        (PurgeReason::Other, "other"),
    ] {
        assert_projection(reason, json!(wire));
    }
    assert_projection(TombstoneMode::Default, json!("default"));
    assert_projection(TombstoneMode::Erase, json!("erase"));
}
