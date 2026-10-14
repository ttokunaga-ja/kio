use super::adapter::*;
use super::batch::*;
use super::ledger::*;
use super::repair::*;
use super::skipped_units::{PublishedSkippedUnit, SkippedDisclosure, SkippedUnit};
use super::{CommandResponse, Completion, ResponseBody};
use kio_core::{ExitCode, KioError};
use kio_pipeline::ledger::PendingWriteRecovery;
use serde::Serialize;
use serde_json::{Value, json};
use std::path::PathBuf;

fn assert_projection<T: Serialize>(payload: T, legacy: Value) {
    let actual = serde_json::to_value(payload).expect("concrete response projection");
    assert_eq!(actual, legacy);
    assert_eq!(
        serde_json::to_vec(&actual).unwrap(),
        serde_json::to_vec(&legacy).unwrap()
    );
}
fn counts() -> BatchCounts {
    BatchCounts {
        tasks_updated: 1,
        tasks_executed: 2,
        tasks_attempted: 5,
        tasks_failed: 3,
        tasks_paused: 4,
        tasks_inflight: 6,
        tasks_inflight_unreadable: 1,
    }
}
#[test]
fn batch_resume_retry_projection_preserves_counts_flags_and_owned_codes() {
    for error_code in [
        None,
        Some("KIO-E-BATCH-PARTIAL-001"),
        Some("KIO-E-BATCH-TASK-FAILED-001"),
    ] {
        let mut retry = json!({"status":"retry scheduled","tasks_updated":1,"tasks_executed":2,
            "tasks_attempted":5,"tasks_failed":3,"tasks_paused":4,"tasks_inflight":6,
            "tasks_inflight_unreadable":1});
        if let Some(code) = error_code {
            retry["error_code"] = json!(code);
        }
        assert_projection(
            BatchResponse::Retry(BatchRetryResponse {
                status: "retry scheduled".into(),
                counts: counts(),
                error_code: error_code.map(str::to_owned),
            }),
            retry.clone(),
        );
        retry["status"] = json!("resumed");
        retry["recheck_budget"] = json!(true);
        retry["override_budget"] = json!(false);
        assert_projection(
            BatchResponse::Resume(BatchResumeResponse {
                status: "resumed".into(),
                recheck_budget: true,
                override_budget: false,
                counts: counts(),
                error_code: error_code.map(str::to_owned),
            }),
            retry,
        );
    }
}
#[test]
fn batch_selection_projection_preserves_key_absence_on_unresolved_no_target() {
    assert_projection(
        BatchResponse::Abandon(BatchSelectionResponse::NoTarget(BatchNoTargetResponse {
            status: "no_target".into(),
        })),
        json!({"status":"no_target"}),
    );
    for status in ["no_target", "abandoned", "reset", "unchanged"] {
        let row = BatchSelectedResponse {
            status: status.into(),
            scope_id: "s".into(),
            adapter_kind: "embedding".into(),
            input_hash: "h".into(),
        };
        let response = if matches!(status, "reset" | "unchanged") {
            BatchResponse::ResetViolations(BatchSelectionResponse::Selected(row))
        } else {
            BatchResponse::Abandon(BatchSelectionResponse::Selected(row))
        };
        assert_projection(
            response,
            json!({"status":status,"scope_id":"s",
            "adapter_kind":"embedding","input_hash":"h"}),
        );
    }
}
#[test]
fn batch_resend_projection_preserves_advisory_text_and_no_target_shape() {
    assert_projection(
        BatchResponse::ResendUnknown(BatchResendResponse::NoTarget(BatchNoTargetResponse {
            status: "no_target".into(),
        })),
        json!({"status":"no_target"}),
    );
    let next_step = "run an explicit online resume; it will perform a fresh reservation and current policy, grant, and budget checks before any provider request";
    let warning = "the prior synchronous request may already have been charged";
    for authorization_created in [false, true] {
        assert_projection(
            BatchResponse::ResendUnknown(BatchResendResponse::Authorized(
                BatchResendAuthorizedResponse {
                    status: "resend_authorized".into(),
                    authorization_created,
                    tasks_queued: 2,
                    scope_id: "s".into(),
                    adapter_kind: "embedding".into(),
                    input_hash: "h".into(),
                    next_step: next_step.into(),
                    warning: warning.into(),
                },
            )),
            json!({"status":"resend_authorized","authorization_created":authorization_created,
                "tasks_queued":2,"scope_id":"s","adapter_kind":"embedding","input_hash":"h",
                "next_step":next_step,"warning":warning}),
        );
    }
}
#[test]
fn stalled_batch_projection_preserves_nullable_fields_and_float() {
    for populated in [false, true] {
        let token = populated.then(|| "intent".to_owned());
        let error = populated.then(|| "reason".to_owned());
        let issued = populated.then_some(9_i64);
        assert_projection(
            StalledBatchStatus {
                scope_id: "s".into(),
                adapter_kind: "embedding".into(),
                input_hash: "h".into(),
                tool_profile_hash: "p".into(),
                intent_token: token.clone(),
                error: error.clone(),
                estimated_usd: 0.0125,
                issued_at_ms: issued,
            },
            json!({"scope_id":"s","adapter_kind":"embedding","input_hash":"h",
                "tool_profile_hash":"p","intent_token":token,"error":error,
                "estimated_usd":0.0125,"issued_at_ms":issued}),
        );
    }
}
#[test]
fn adapter_approval_projection_preserves_branch_owned_nulls_and_absence() {
    for resume in [None, Some("resume-id".to_owned())] {
        assert_projection(
            AdapterResponse::ApprovalPreview(ApprovalPreviewResponse {
                status: "preview".into(),
                scope_id: "s".into(),
                adapters: vec![],
                send_secrets: false,
                resume: resume.clone(),
            }),
            json!({"status":"preview","scope_id":"s",
                "adapters":[],"send_secrets":false,"resume":resume}),
        );
    }
    assert_projection(
        AdapterResponse::Approved(ApprovedResponse {
            status: "approved".into(),
            approvals: vec![
                ApprovalOutcome::AlreadyApproved(AlreadyApproved {
                    tool_id: "a".into(),
                    status: "already_approved".into(),
                }),
                ApprovalOutcome::ResumedAlreadyApproved(ResumedAlreadyApproved {
                    tool_id: "b".into(),
                    status: "already_approved".into(),
                    grant_id: "g1".into(),
                }),
                ApprovalOutcome::Approved(ApprovedApproval {
                    tool_id: "c".into(),
                    status: "approved".into(),
                    grant_id: "g2".into(),
                    secrets_grant_id: None,
                }),
                ApprovalOutcome::Approved(ApprovedApproval {
                    tool_id: "d".into(),
                    status: "approved".into(),
                    grant_id: "g3".into(),
                    secrets_grant_id: Some("secret-grant-reference".into()),
                }),
            ],
        }),
        json!({"status":"approved","approvals":[
            {"tool_id":"a","status":"already_approved"},
            {"tool_id":"b","status":"already_approved","grant_id":"g1"},
            {"tool_id":"c","status":"approved","grant_id":"g2","secrets_grant_id":null},
            {"tool_id":"d","status":"approved","grant_id":"g3","secrets_grant_id":"secret-grant-reference"}]}),
    );
}
#[test]
fn adapter_revoke_projection_preserves_nullable_tool_and_empty_lists() {
    for (status, tool_id) in [
        ("no_target", None),
        ("revoked", Some("embedding".to_owned())),
    ] {
        assert_projection(
            AdapterResponse::Revoked(ApprovalRevokeResponse {
                status: status.into(),
                tool_id: tool_id.clone(),
                all: tool_id.is_none(),
                revoked_tool_ids: vec![],
                revoked_grant_ids: vec![],
                pending_removed: false,
            }),
            json!({"status":status,
                "tool_id":tool_id,"all":tool_id.is_none(),"revoked_tool_ids":[],
                "revoked_grant_ids":[],"pending_removed":false}),
        );
    }
}
#[test]
fn adapter_status_projection_preserves_arbitrary_persisted_json_leaves() {
    for pending in [None, Some(json!({"extra":[null,7],"tool_id":false}))] {
        let rows = vec![json!({"extra":"preserved"}), json!([1, null]), json!(false)];
        assert_projection(
            AdapterResponse::Status(ApprovalStatusResponse {
                scope_id: "s".into(),
                grants: vec![],
                effective: vec![EffectiveApproval {
                    grant_id: "g".into(),
                    permitted: true,
                    reason: None,
                }],
                scope_approvals: rows.clone(),
                pending: pending.clone(),
            }),
            json!({"scope_id":"s",
                "grants":[],"effective":[{"grant_id":"g","permitted":true,"reason":null}],
                "scope_approvals":rows,"pending":pending}),
        );
    }
}
#[test]
fn trust_projection_preserves_current_operation_strings_record_and_sentinels() {
    let legacy_record = json!({"version":1,"generation":4,"status":"active","digest":"digest",
        "snapshot":"snapshot","registration_id":"registration"});
    for operation in ["registered", "rotated", "preview", "revoked", "status"] {
        // Deserialize only the existing domain record, not a response DTO.
        let record = serde_json::from_value(legacy_record.clone()).unwrap();
        assert_projection(
            AdapterResponse::Trust(TrustResponse::Record(TrustRecordResponse {
                operation: operation.into(),
                trust: record,
            })),
            json!({"operation":operation,"trust":legacy_record}),
        );
    }
    for operation in ["revoked", "status"] {
        assert_projection(
            TrustResponse::Unregistered(TrustUnregisteredResponse {
                operation: operation.into(),
                trust: "unregistered".into(),
            }),
            json!({"operation":operation,"trust":"unregistered"}),
        );
    }
    assert_projection(
        TrustResponse::Pending(TrustPendingResponse {
            operation: "status".into(),
            trust: "registration_pending".into(),
            registration_id: "r".into(),
        }),
        json!({"operation":"status","trust":"registration_pending","registration_id":"r"}),
    );
}
#[test]
fn ledger_lifecycle_projection_preserves_paths_and_all_recovery_variants() {
    let path = PathBuf::from("ledger/日本語/cost-ledger.sqlite");
    for status in [
        "initialized",
        "ready",
        "uninitialized",
        "backed_up",
        "restored",
    ] {
        assert_projection(
            LedgerPathResponse {
                status: status.into(),
                path: path.clone(),
            },
            json!({"status":status,"path":path}),
        );
    }
    for (recovery, wire) in [
        (PendingWriteRecovery::NoPending, "no_pending"),
        (PendingWriteRecovery::Aborted, "aborted"),
        (
            PendingWriteRecovery::RestoredBeforeCheckpoint,
            "restored_before_checkpoint",
        ),
        (
            PendingWriteRecovery::FinalizedCommitted,
            "finalized_committed",
        ),
        (
            PendingWriteRecovery::PublishedAfterCheckpoint,
            "published_after_checkpoint",
        ),
    ] {
        assert_projection(
            LedgerResponse::Recover(LedgerRecoverResponse {
                status: "ready".into(),
                recovery,
                path: path.clone(),
            }),
            json!({"status":"ready","recovery":wire,"path":path}),
        );
    }
}
fn attribution_key() -> AttributionTaskKey {
    AttributionTaskKey {
        scope_id: "s".into(),
        adapter_kind: "embedding".into(),
        input_hash: "h".into(),
        tool_profile_hash: "p".into(),
    }
}
#[test]
fn ledger_reconcile_projection_preserves_finite_rows_nulls_and_array_order() {
    let fetch = "read-only-safe: fetching this job's result does not create new state";
    let deletion = "delete only after confirming no other Kio instance shares this provider scope";
    let unverified = "task_key.scope_id is not a locally-verified registered scope";
    let absent = "job metadata carries no task_key (unreadable or absent)";
    let key =
        json!({"scope_id":"s","adapter_kind":"embedding","input_hash":"h","tool_profile_hash":"p"});
    assert_projection(
        LedgerResponse::Reconcile(LedgerReconcileResponse {
            status: "reconciled".into(),
            integrity: "ok".into(),
            sync_settled: 1,
            batch_found: 2,
            batch_found_keys: vec!["second".into(), "first".into()],
            batch_settled_unknown: 3,
            batch_settled_unknown_keys: vec![],
            unlistable: 4,
            unlistable_keys: vec![],
            phase2b_not_started: 5,
            phase2b_not_started_keys: vec![],
            orphans: vec![OrphanJob {
                job_id: "orphan".into(),
                task_key: attribution_key(),
                guidance: OrphanGuidance {
                    result_fetch: fetch.into(),
                    deletion: deletion.into(),
                },
            }],
            unknown: vec![
                UnknownJob {
                    job_id: "unverified".into(),
                    task_key: Some(attribution_key()),
                    reason: unverified.into(),
                },
                UnknownJob {
                    job_id: "absent".into(),
                    task_key: None,
                    reason: absent.into(),
                },
            ],
            unknown_uploads: vec![
                UnknownUpload {
                    upload_id: "u1".into(),
                    filename_token: None,
                },
                UnknownUpload {
                    upload_id: "u2".into(),
                    filename_token: Some("token".into()),
                },
            ],
        }),
        json!({"status":"reconciled","integrity":"ok","sync_settled":1,"batch_found":2,
        "batch_found_keys":["second","first"],"batch_settled_unknown":3,"batch_settled_unknown_keys":[],
        "unlistable":4,"unlistable_keys":[],"phase2b_not_started":5,"phase2b_not_started_keys":[],
        "orphans":[{"job_id":"orphan","task_key":key,"guidance":{"result_fetch":fetch,"deletion":deletion}}],
        "unknown":[{"job_id":"unverified","task_key":key,"reason":unverified},
            {"job_id":"absent","task_key":null,"reason":absent}],
        "unknown_uploads":[{"upload_id":"u1","filename_token":null},{"upload_id":"u2","filename_token":"token"}]}),
    );
}
fn rebuild() -> RepairRebuildResponse {
    RepairRebuildResponse {
        status: "rebuilt".into(),
        rebuilt_chunks: 2,
        rebuilt_tree_entries: 3,
        sqlite_db: PathBuf::from("scope/日本語/.kio/index/sqlite.db"),
        embedding_tasks_executed: 4,
        embedding_tasks_failed: 5,
        paused_tasks: 6,
        skipped: SkippedDisclosure {
            skipped_units: vec![],
            skipped_units_guidance: None,
        },
    }
}
#[test]
fn repair_rebuild_projection_preserves_skipped_empty_and_populated_disclosure() {
    let path = PathBuf::from("scope/日本語/.kio/index/sqlite.db");
    let base = json!({"status":"rebuilt","rebuilt_chunks":2,"rebuilt_tree_entries":3,"sqlite_db":path,
        "embedding_tasks_executed":4,"embedding_tasks_failed":5,"paused_tasks":6,"skipped_units":[]});
    assert_projection(RepairResponse::RebuildDb(rebuild()), base.clone());
    let mut dto = rebuild();
    dto.skipped = SkippedDisclosure {
        skipped_units: vec![PublishedSkippedUnit {
            unit: SkippedUnit {
                raw_hash: "raw".into(),
                path: "日本語.md".into(),
                r#gen: 9,
                reason: "missing".into(),
            },
            searchable: false,
            guidance: "guidance".into(),
        }],
        skipped_units_guidance: Some("aggregate guidance".into()),
    };
    let mut expected = base;
    expected["skipped_units"] = json!([{"raw_hash":"raw","path":"日本語.md","gen":9,"reason":"missing",
        "searchable":false,"guidance":"guidance"}]);
    expected["skipped_units_guidance"] = json!("aggregate guidance");
    assert_projection(RepairResponse::RebuildDb(dto), expected);
}
#[test]
fn repair_verify_projection_preserves_nullable_repaired_commit_and_optional_decorations() {
    use crate::verify_objects::VerifyObjectsReport;
    let error = KioError::new(
        "KIO-E-STORE-NAMESPACE-001",
        "not accessible",
        json!({"namespace":"raw"}),
        ExitCode::PartialFailure,
    );
    let mut report = VerifyObjectsReport::namespace_unavailable(&error);
    let expected = json!({"status":"verification_blocked","checked":{"raw":0,"chunks":0,"trees":0,
        "commits":0,"normalized_instances":0,"normalized_units":0,"embeddings":0,"manifests":0,"toollocks":0},
        "repaired_raw_count":0,"repaired_commit_hash":null,"dead_by_tombstone_count":0,
        "dead_by_erase_receipt_count":0,"purge_explained_missing_history_count":0,
        "remaining_findings":[{"kind":"object_namespace_unavailable","object_hash":"",
            "reason":"objects/raw: not accessible","affected_commits":[]}],
        "findings_truncated":false,"external_pointers_may_be_affected":true,"error_code":"KIO-E-STORE-NAMESPACE-001"});
    assert_projection(
        RepairResponse::VerifyObjects(RepairVerifyResponse {
            report,
            prune_orphans: None,
            error_code: Some("KIO-E-STORE-NAMESPACE-001".into()),
        }),
        expected.clone(),
    );
    report = VerifyObjectsReport::namespace_unavailable(&error);
    report.repaired_commit_hash = Some("repaired-head".into());
    let mut with_head = expected;
    with_head["repaired_commit_hash"] = json!("repaired-head");
    with_head.as_object_mut().unwrap().remove("error_code");
    assert_projection(
        RepairVerifyResponse {
            report,
            prune_orphans: None,
            error_code: None,
        },
        with_head,
    );
}
#[test]
fn repair_prune_preflight_projection_contains_no_verify_report_fields() {
    let prune = crate::verify_objects::PruneOrphansReport {
        status: "blocked".into(),
        blocked_by: Some("active_gc".into()),
        ..Default::default()
    };
    assert_projection(
        RepairResponse::PruneBlocked(RepairPruneBlockedResponse {
            prune_orphans: prune,
            error_code: "KIO-E-PRUNE-ORPHANS-BLOCKED-001".into(),
        }),
        json!({"prune_orphans":{"status":"blocked","blocked_by":"active_gc",
            "pruned_prepared_count":0,"pruned_image_count":0,"pruned_open_cache_count":0,
            "pruned_staging_root_count":0},"error_code":"KIO-E-PRUNE-ORPHANS-BLOCKED-001"}),
    );
    assert_projection(
        RepairResponse::RegistryPrune(crate::verify_objects::RegistryPruneReport {
            pruned_count: 7,
        }),
        json!({"pruned_count":7}),
    );
}
#[test]
fn repair_device_projection_preserves_scope_failure_context_and_replica_shape() {
    let context = json!(["arbitrary", {"nested":null}]);
    assert_projection(
        RepairResponse::Device(DeviceRepairResponse {
            status: "partial".into(),
            operation: "replica".into(),
            repaired_scopes: vec![RepairedScope {
                scope_id: "s".into(),
                path: "scope/日本語".into(),
                status: "replica_rebuilt".into(),
                result: ScopeRepairResult::Replica(ScopeRepairReplica {
                    status: "replica_rebuilt".into(),
                }),
            }],
            failed_scopes: vec![FailedScope {
                scope_id: "f".into(),
                path: "missing".into(),
                stage: "open".into(),
                reason: "unreachable".into(),
                error_code: "KIO-E-STORE-IO-001".into(),
                message: "missing".into(),
                context: context.clone(),
                exit_code: 7,
            }],
            summary: RepairSummary {
                repaired: 1,
                failed: 1,
            },
            error_code: Some("KIO-E-REPAIR-PARTIAL-001".into()),
        }),
        json!({"status":"partial","operation":"replica",
            "repaired_scopes":[{"scope_id":"s","path":PathBuf::from("scope/日本語"),"status":"replica_rebuilt",
                "result":{"status":"replica_rebuilt"}}],
            "failed_scopes":[{"scope_id":"f","path":PathBuf::from("missing"),"stage":"open","reason":"unreachable",
                "error_code":"KIO-E-STORE-IO-001","message":"missing","context":context,"exit_code":7}],
            "summary":{"repaired":1,"failed":1},"error_code":"KIO-E-REPAIR-PARTIAL-001"}),
    );
}
#[test]
fn repair_cancel_projection_preserves_nullable_preview_and_resumed_branch() {
    assert_projection(
        CancelChildInitializationResponse::Preview(CancelChildPreview {
            preview: true,
            operations: vec![CancelChildRow {
                operation: "o".into(),
                path: "child".into(),
                stage: "stage".into(),
                cancelable: true,
                reason: None,
                current_policy_allows: None,
            }],
            pending_cancellation: None,
        }),
        json!({"preview":true,"operations":[{"operation":"o","path":PathBuf::from("child"),
            "stage":PathBuf::from("stage"),"cancelable":true,"reason":null,"current_policy_allows":null}],
            "pending_cancellation":null}),
    );
    for resumed in [false, true] {
        assert_projection(
            CancelChildInitializationResponse::Applied(CancelChildApplied {
                operation: "o".into(),
                cancelled: true,
                resumed,
            }),
            json!({"operation":"o","cancelled":true,"resumed":resumed}),
        );
    }
}
#[test]
fn repair_restore_recovery_projection_preserves_no_pending_and_projection_failure_omission() {
    assert_projection(
        RepairResponse::RecoverRestore(ManagedRestoreRecoveryResponse {
            operation: "managed_restore_recovery".into(),
            status: "no_pending_restore".into(),
            recovered: false,
            egress: false,
            projection_status: None,
            projection: None,
            projection_error: None,
        }),
        json!({"operation":"managed_restore_recovery","status":"no_pending_restore","recovered":false,"egress":false}),
    );
    assert_projection(
        RepairResponse::RecoverRestore(ManagedRestoreRecoveryResponse {
            operation: "managed_restore_recovery".into(),
            status: "recovered_projection_failed".into(),
            recovered: true,
            egress: false,
            projection_status: Some("failed".into()),
            projection: None,
            projection_error: Some(ManagedRestoreProjectionError {
                error_code: "code".into(),
                message: "message".into(),
            }),
        }),
        json!({"operation":"managed_restore_recovery","status":"recovered_projection_failed","recovered":true,"egress":false,
            "projection_status":"failed","projection_error":{"error_code":"code","message":"message"}}),
    );
}
#[test]
fn c3d_legacy_projection_keeps_completion_out_of_body_and_preserves_independent_code() {
    for (completion, _marker) in [
        (Completion::Complete, None),
        (Completion::Partial, Some(3)),
        (Completion::Permanent, Some(4)),
        (Completion::AuthRequired, Some(5)),
        (Completion::BudgetBlocked, Some(6)),
    ] {
        let expected = json!({"status":"no_target"});
        let response = CommandResponse {
            body: ResponseBody::Batch(BatchResponse::Abandon(BatchSelectionResponse::NoTarget(
                BatchNoTargetResponse {
                    status: "no_target".into(),
                },
            ))),
            completion,
        };
        assert_eq!(response.completion, completion);
        let actual = serde_json::to_value(response.body).unwrap();
        assert_eq!(actual, expected);
        assert_eq!(
            serde_json::to_vec(&actual).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
    }
    let expected = json!({"status":"retry scheduled","tasks_updated":1,"tasks_executed":2,
        "tasks_attempted":5,"tasks_failed":3,"tasks_paused":4,"tasks_inflight":6,
        "tasks_inflight_unreadable":1,"error_code":"KIO-E-BATCH-PARTIAL-001"});
    let response = CommandResponse {
        body: ResponseBody::Batch(BatchResponse::Retry(BatchRetryResponse {
            status: "retry scheduled".into(),
            counts: counts(),
            error_code: Some("KIO-E-BATCH-PARTIAL-001".into()),
        })),
        completion: Completion::AuthRequired,
    };
    assert_eq!(response.completion, Completion::AuthRequired);
    let actual = serde_json::to_value(response.body).unwrap();
    assert_eq!(actual, expected);
    assert_eq!(
        serde_json::to_vec(&actual).unwrap(),
        serde_json::to_vec(&expected).unwrap()
    );
}
#[test]
fn adapter_projection_reuses_populated_grants_and_retains_domain_omission() {
    use crate::grants::{GrantBinding, GrantOperation, GrantRecord, GrantState};
    let binding = GrantBinding {
        scope_id: "s".into(),
        canonical_root: "scope/日本語".into(),
        directory_identity: kio_core::management::DirectoryIdentity::Unix {
            device: 1,
            inode: 2,
        },
        membership_hash: "membership".into(),
        tool_id: "tool".into(),
        execution_mode: "online_api".into(),
        tool_profile_hash: "profile".into(),
        destination: "destination".into(),
        credential_binding: "env:REFERENCE".into(),
        trust_binding: "trust-digest".into(),
        operation: GrantOperation::Network,
    };
    let expected_binding = json!({"scope_id":"s","canonical_root":PathBuf::from("scope/日本語"),
        "directory_identity":{"platform":"unix","device":"0000000000000001","inode":"0000000000000002"},
        "membership_hash":"membership","tool_id":"tool","execution_mode":"online_api",
        "tool_profile_hash":"profile","destination":"destination","credential_binding":"env:REFERENCE",
        "trust_binding":"trust-digest","operation":"network"});
    assert_projection(
        AdapterResponse::ApprovalPreview(ApprovalPreviewResponse {
            status: "preview".into(),
            scope_id: "s".into(),
            adapters: vec![binding.clone()],
            send_secrets: true,
            resume: None,
        }),
        json!({"status":"preview","scope_id":"s","adapters":[expected_binding],"send_secrets":true,"resume":null}),
    );
    for activated_at in [None, Some("activated".to_owned())] {
        let mut grant = json!({"schema_version":1,"grant_id":"g","approval_id":"a","device_instance_id":"d",
            "state":"active","binding":expected_binding,"created_at":"created"});
        if let Some(date) = &activated_at {
            grant["activated_at"] = json!(date);
        }
        let record = GrantRecord {
            schema_version: 1,
            grant_id: "g".into(),
            approval_id: "a".into(),
            device_instance_id: "d".into(),
            state: GrantState::Active,
            binding: binding.clone(),
            created_at: "created".into(),
            activated_at,
            revoked_at: None,
        };
        assert_projection(
            ApprovalStatusResponse {
                scope_id: "s".into(),
                grants: vec![record],
                effective: vec![],
                scope_approvals: vec![],
                pending: None,
            },
            json!({"scope_id":"s","grants":[grant],
                "effective":[],"scope_approvals":[],"pending":null}),
        );
    }
}
#[test]
fn repair_device_all_projection_preserves_nested_rebuild_and_empty_aggregate() {
    let result = ScopeRepairResult::All(ScopeRepairAll {
        rebuild: rebuild(),
        replica: "ready".into(),
    });
    let expected_result = json!({"status":"rebuilt","rebuilt_chunks":2,"rebuilt_tree_entries":3,
        "sqlite_db":PathBuf::from("scope/日本語/.kio/index/sqlite.db"),"embedding_tasks_executed":4,
        "embedding_tasks_failed":5,"paused_tasks":6,"skipped_units":[],"replica":"ready"});
    assert_projection(
        DeviceRepairResponse {
            status: "repaired".into(),
            operation: "all".into(),
            repaired_scopes: vec![RepairedScope {
                scope_id: "s".into(),
                path: "scope/日本語".into(),
                status: "rebuilt".into(),
                result,
            }],
            failed_scopes: vec![],
            summary: RepairSummary {
                repaired: 1,
                failed: 0,
            },
            error_code: None,
        },
        json!({"status":"repaired","operation":"all","repaired_scopes":[
            {"scope_id":"s","path":"scope/日本語","status":"rebuilt","result":expected_result}],
            "failed_scopes":[],"summary":{"repaired":1,"failed":0}}),
    );
    assert_projection(
        DeviceRepairResponse {
            status: "up_to_date".into(),
            operation: "all".into(),
            repaired_scopes: vec![],
            failed_scopes: vec![],
            summary: RepairSummary {
                repaired: 0,
                failed: 0,
            },
            error_code: None,
        },
        json!({"status":"up_to_date","operation":"all","repaired_scopes":[],
            "failed_scopes":[],"summary":{"repaired":0,"failed":0}}),
    );
}
#[test]
fn repair_recovered_projection_keeps_original_raw_skipped_unit_shape() {
    assert_projection(
        ManagedRestoreRecoveryResponse {
            operation: "managed_restore_recovery".into(),
            status: "recovered".into(),
            recovered: true,
            egress: false,
            projection_status: Some("ready".into()),
            projection: Some(ManagedRestoreProjection {
                status: "ready".into(),
                rebuilt_chunks: 2,
                rebuilt_tree_entries: 3,
                skipped_units: vec![SkippedUnit {
                    raw_hash: "raw".into(),
                    path: "file".into(),
                    r#gen: 1,
                    reason: "missing".into(),
                }],
                egress: false,
            }),
            projection_error: None,
        },
        json!({"operation":"managed_restore_recovery","status":"recovered",
                "recovered":true,"egress":false,"projection_status":"ready","projection":{"status":"ready",
                    "rebuilt_chunks":2,"rebuilt_tree_entries":3,"skipped_units":[{"raw_hash":"raw","path":"file",
                        "gen":1,"reason":"missing"}],"egress":false}}),
    );
}
#[test]
fn repair_cancel_projection_preserves_blocked_preview_and_pending_marker() {
    assert_projection(
        CancelChildPreview {
            preview: true,
            operations: vec![CancelChildRow {
                operation: "o".into(),
                path: "child".into(),
                stage: "stage".into(),
                cancelable: false,
                reason: Some("blocked".into()),
                current_policy_allows: Some(false),
            }],
            pending_cancellation: Some(PendingCancellation {
                operation: "o".into(),
                phase: "quarantined".into(),
            }),
        },
        json!({"preview":true,"operations":[{"operation":"o","path":PathBuf::from("child"),
            "stage":PathBuf::from("stage"),"cancelable":false,"reason":"blocked","current_policy_allows":false}],
            "pending_cancellation":{"operation":"o","phase":"quarantined"}}),
    );
}
