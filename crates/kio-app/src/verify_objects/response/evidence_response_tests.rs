use super::verification_output;
use super::{EvidenceStatus, EvidenceVerification};
use crate::response::evidence::{
    EvidenceBatchResponse, EvidenceBatchRow, EvidenceBatchSummary, EvidenceResponse,
    EvidenceRetargetResponse, EvidenceStatusCounts,
};
use crate::response::{CommandResponse, Completion, ResponseBody};
use kio_core::ExitCode;
use kio_search::evidence::EvidencePointer;
use serde_json::{Value, json};

fn assert_bytes(actual: Value, legacy: Value) {
    assert_eq!(actual, legacy);
    assert_eq!(
        serde_json::to_vec(&actual).unwrap(),
        serde_json::to_vec(&legacy).unwrap()
    );
}

#[test]
fn evidence_verify_projection_preserves_existing_statuses_optional_error_and_details() {
    for (status, text, code, details, exit) in [
        (
            EvidenceStatus::Alive,
            "alive",
            None,
            json!({"commit_shallow":false,"manifest_missing":false}),
            ExitCode::Success,
        ),
        (
            EvidenceStatus::Tombstoned,
            "tombstoned",
            Some("KIO-E-PURGE-TOMBSTONED-001"),
            json!({"reason":"privacy","paths":[]}),
            ExitCode::Success,
        ),
        (
            EvidenceStatus::NotFound,
            "not_found",
            Some("KIO-E-PURGE-NOT-FOUND-001"),
            json!({"raw_hash":"raw","scope_path":"scope/.kio"}),
            ExitCode::Success,
        ),
        (
            EvidenceStatus::ScopeUnreachable,
            "scope_unreachable",
            Some("KIO-E-EVIDENCE-SCOPE-UNREACHABLE-001"),
            json!({"scope_id":"scope","scope_path":null}),
            ExitCode::Success,
        ),
        (
            EvidenceStatus::RegistryDuplicate,
            "registry_duplicate",
            Some("KIO-E-REGISTRY-DUP-001"),
            json!({"scope_id":"scope","paths":["a","b"]}),
            ExitCode::PartialFailure,
        ),
        (
            EvidenceStatus::Unverifiable,
            "unverifiable",
            None,
            json!({"reason":"commit_shallow"}),
            ExitCode::PartialFailure,
        ),
        (
            EvidenceStatus::Unverifiable,
            "unverifiable",
            None,
            json!({"reason":"manifest_missing"}),
            ExitCode::PermanentFailure,
        ),
    ] {
        let mut expected = json!({"status":text,"details":details});
        if let Some(code) = code {
            expected["error_code"] = json!(code);
        }
        let response =
            verification_output(EvidenceVerification::new(status, code, details, exit)).unwrap();
        let expected_completion = match exit {
            ExitCode::Success => Completion::Complete,
            ExitCode::PartialFailure => Completion::Partial,
            ExitCode::PermanentFailure => Completion::Permanent,
            _ => unreachable!(),
        };
        assert_eq!(response.completion, expected_completion);
        let actual = serde_json::to_value(response.body).unwrap();
        assert_bytes(actual, expected);
    }
}

#[test]
fn evidence_verify_strict_tombstoned_not_found_and_scope_unreachable_keep_completion() {
    for (status, text, code, exit) in [
        (
            EvidenceStatus::Tombstoned,
            "tombstoned",
            "KIO-E-PURGE-TOMBSTONED-001",
            ExitCode::PermanentFailure,
        ),
        (
            EvidenceStatus::NotFound,
            "not_found",
            "KIO-E-PURGE-NOT-FOUND-001",
            ExitCode::PermanentFailure,
        ),
        (
            EvidenceStatus::ScopeUnreachable,
            "scope_unreachable",
            "KIO-E-EVIDENCE-SCOPE-UNREACHABLE-001",
            ExitCode::PartialFailure,
        ),
    ] {
        let details = json!({"scope_id":"scope","opaque_context":{"hint":null,"items":[]}});
        let response = verification_output(EvidenceVerification::new(
            status,
            Some(code),
            details.clone(),
            exit,
        ))
        .unwrap();
        let expected_completion = match exit {
            ExitCode::PermanentFailure => Completion::Permanent,
            ExitCode::PartialFailure => Completion::Partial,
            _ => unreachable!(),
        };
        assert_eq!(response.completion, expected_completion);
        assert_bytes(
            serde_json::to_value(response.body).unwrap(),
            json!({"status":text,"error_code":code,"details":details}),
        );
    }
}

#[test]
fn evidence_batch_projection_preserves_input_order_one_based_lines_and_all_six_counts() {
    let rows = [
        (EvidenceStatus::Alive, "alive", None, ExitCode::Success),
        (
            EvidenceStatus::Tombstoned,
            "tombstoned",
            Some("KIO-E-PURGE-TOMBSTONED-001"),
            ExitCode::PermanentFailure,
        ),
        (
            EvidenceStatus::NotFound,
            "not_found",
            Some("KIO-E-PURGE-NOT-FOUND-001"),
            ExitCode::PermanentFailure,
        ),
        (
            EvidenceStatus::ScopeUnreachable,
            "scope_unreachable",
            Some("KIO-E-EVIDENCE-SCOPE-UNREACHABLE-001"),
            ExitCode::PartialFailure,
        ),
        (
            EvidenceStatus::Unverifiable,
            "unverifiable",
            None,
            ExitCode::PartialFailure,
        ),
        (
            EvidenceStatus::RegistryDuplicate,
            "registry_duplicate",
            Some("KIO-E-REGISTRY-DUP-001"),
            ExitCode::PartialFailure,
        ),
    ];
    let mut counts = EvidenceStatusCounts::default();
    let mut results = Vec::new();
    let mut expected_rows = Vec::new();
    for (index, (status, text, code, exit)) in rows.into_iter().enumerate() {
        counts.record(status);
        let details = match status {
            EvidenceStatus::Alive => json!({"commit_shallow":false,"manifest_missing":false}),
            EvidenceStatus::Tombstoned => json!({"reason":"privacy"}),
            EvidenceStatus::NotFound => json!({"raw_hash":"raw","scope_path":"scope/.kio"}),
            EvidenceStatus::ScopeUnreachable | EvidenceStatus::RegistryDuplicate => {
                json!({"scope_id":"scope"})
            }
            EvidenceStatus::Unverifiable => json!({"reason":"commit_shallow"}),
        };
        let result = EvidenceVerification::new(status, code, details.clone(), exit).result;
        results.push(EvidenceBatchRow {
            line: index + 1,
            result,
        });
        let mut expected_result = json!({"status":text,"details":details});
        if let Some(code) = code {
            expected_result["error_code"] = json!(code);
        }
        expected_rows.push(json!({"line":index+1,"result":expected_result}));
    }
    let actual = serde_json::to_value(EvidenceBatchResponse {
        schema: "kio.evidence.batch-verify".to_owned(),
        schema_version: 1,
        input_sha256: "input".to_owned(),
        strict: true,
        results,
        summary: EvidenceBatchSummary {
            total: 6,
            status_counts: counts,
        },
        verified_count: 6,
    })
    .unwrap();
    assert_bytes(
        actual,
        json!({"schema":"kio.evidence.batch-verify","schema_version":1,"input_sha256":"input","strict":true,
        "results":expected_rows,"summary":{"total":6,"status_counts":{"alive":1,"tombstoned":1,"not_found":1,
        "scope_unreachable":1,"unverifiable":1,"registry_duplicate":1}},"verified_count":6}),
    );
}

#[test]
fn evidence_batch_projection_keeps_zero_counts_and_does_not_decorate_individual_rows() {
    let verification = EvidenceVerification::new(
        EvidenceStatus::Alive,
        None,
        json!({"commit_shallow":false}),
        ExitCode::Success,
    );
    let mut counts = EvidenceStatusCounts::default();
    counts.record(EvidenceStatus::Alive);
    let response = CommandResponse {
        body: ResponseBody::Evidence(EvidenceResponse::Batch(EvidenceBatchResponse {
            schema: "kio.evidence.batch-verify".to_owned(),
            schema_version: 1,
            input_sha256: "input".to_owned(),
            strict: false,
            results: vec![EvidenceBatchRow {
                line: 1,
                result: verification.result,
            }],
            summary: EvidenceBatchSummary {
                total: 1,
                status_counts: counts,
            },
            verified_count: 1,
        })),
        completion: Completion::Complete,
    };
    assert_eq!(response.completion, Completion::Complete);
    let actual = serde_json::to_value(response.body).unwrap();
    assert_bytes(
        actual,
        json!({"schema":"kio.evidence.batch-verify","schema_version":1,"input_sha256":"input","strict":false,
        "results":[{"line":1,"result":{"status":"alive","details":{"commit_shallow":false}}}],
        "summary":{"total":1,"status_counts":{"alive":1,"tombstoned":0,"not_found":0,"scope_unreachable":0,"unverifiable":0,"registry_duplicate":0}},"verified_count":1}),
    );
}

#[test]
fn evidence_batch_completion_preserves_strict_permanent_priority_and_retryable_nonstrict() {
    // The existing producer tracks only these supported categories. No auth/budget case.
    for (strict, permanent, retryable, expected) in [
        (true, true, true, Completion::Permanent),
        (true, true, false, Completion::Permanent),
        (true, false, true, Completion::Partial),
        (true, false, false, Completion::Complete),
        (false, false, true, Completion::Partial),
        (false, false, false, Completion::Complete),
    ] {
        assert_eq!(
            crate::response::evidence::batch_completion(strict, permanent, retryable),
            expected
        );
    }
}

#[test]
fn evidence_batch_mixed_projection_keeps_root_completion_and_undecorated_rows() {
    for (strict, completion, _exit) in [
        (true, Completion::Permanent, 4),
        (false, Completion::Partial, 3),
    ] {
        let tombstone = EvidenceVerification::new(
            EvidenceStatus::Tombstoned,
            Some("KIO-E-PURGE-TOMBSTONED-001"),
            json!({"reason":"privacy"}),
            if strict {
                ExitCode::PermanentFailure
            } else {
                ExitCode::Success
            },
        );
        let duplicate = EvidenceVerification::new(
            EvidenceStatus::RegistryDuplicate,
            Some("KIO-E-REGISTRY-DUP-001"),
            json!({"scope_id":"scope"}),
            ExitCode::PartialFailure,
        );
        let response = CommandResponse {
            body: ResponseBody::Evidence(EvidenceResponse::Batch(EvidenceBatchResponse {
                schema: "kio.evidence.batch-verify".to_owned(),
                schema_version: 1,
                input_sha256: "input".to_owned(),
                strict,
                results: vec![
                    EvidenceBatchRow {
                        line: 1,
                        result: tombstone.result,
                    },
                    EvidenceBatchRow {
                        line: 2,
                        result: duplicate.result,
                    },
                ],
                summary: EvidenceBatchSummary {
                    total: 2,
                    status_counts: EvidenceStatusCounts {
                        alive: 0,
                        tombstoned: 1,
                        not_found: 0,
                        scope_unreachable: 0,
                        unverifiable: 0,
                        registry_duplicate: 1,
                    },
                },
                verified_count: 2,
            })),
            completion,
        };
        assert_eq!(response.completion, completion);
        let actual = serde_json::to_value(response.body).unwrap();
        assert_bytes(
            actual,
            json!({"schema":"kio.evidence.batch-verify","schema_version":1,"input_sha256":"input","strict":strict,
            "results":[{"line":1,"result":{"status":"tombstoned","error_code":"KIO-E-PURGE-TOMBSTONED-001","details":{"reason":"privacy"}}},
                {"line":2,"result":{"status":"registry_duplicate","error_code":"KIO-E-REGISTRY-DUP-001","details":{"scope_id":"scope"}}}],
            "summary":{"total":2,"status_counts":{"alive":0,"tombstoned":1,"not_found":0,"scope_unreachable":0,"unverifiable":0,"registry_duplicate":1}},"verified_count":2}),
        );
    }
}

fn pointer(populated: bool) -> EvidencePointer {
    EvidencePointer {
        schema_version: 1,
        scope_id: "scope".to_owned(),
        scope_path: populated.then(|| "scope/.kio".to_owned()),
        commit: format!("sha256:{}", "a".repeat(64)),
        tree: populated.then(|| format!("sha256:{}", "b".repeat(64))),
        raw_hash: format!("sha256:{}", "c".repeat(64)),
        tool_profile_hash: format!("sha256:{}", "d".repeat(64)),
        chunk_hash: format!("sha256:{}", "e".repeat(64)),
        path_at_commit: populated.then(|| "日本語.md".to_owned()),
        heading_path: populated.then(|| vec!["章".to_owned()]),
        section_id: None,
        byte_start: populated.then_some(0),
        byte_end: populated.then_some(7),
    }
}

#[test]
fn evidence_retarget_projection_reuses_native_pointers_and_preserves_optional_omissions() {
    let old = pointer(false);
    let new = pointer(true);
    let target = new.commit.clone();
    // Pointer fields retain their native domain serialization and omissions.
    let expected = json!({"schema":"kio.evidence.retarget","schema_version":1,"status":"retargeted",
        "target_commit":target,"retargeted_from":old,"new_pointer":new,"match_method":"heading_path_exact"});
    let response = CommandResponse {
        body: ResponseBody::Evidence(EvidenceResponse::Retarget(Box::new(
            EvidenceRetargetResponse {
                schema: "kio.evidence.retarget".to_owned(),
                schema_version: 1,
                status: "retargeted".to_owned(),
                target_commit: target,
                retargeted_from: old,
                new_pointer: new,
                match_method: "heading_path_exact".to_owned(),
            },
        ))),
        completion: Completion::Complete,
    };
    assert_eq!(response.completion, Completion::Complete);
    let actual = serde_json::to_value(response.body).unwrap();
    assert!(actual["retargeted_from"].get("tree").is_none());
    assert!(actual["new_pointer"].get("section_id").is_none());
    assert_bytes(actual, expected);
}

#[test]
fn evidence_empty_batch_error_keeps_existing_code_message_and_nullable_line_context() {
    let error = super::super::parse_batch_pointers(b"").unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-EVIDENCE-BATCH-INPUT-001");
    assert_eq!(error.message(), "invalid evidence batch input");
    assert_eq!(error.context(), &json!({"line":null,"reason":"empty"}));
    assert_eq!(error.exit_code(), ExitCode::InvalidUsage);
}
