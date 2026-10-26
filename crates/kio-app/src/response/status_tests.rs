use super::batch::StalledBatchStatus;
use super::status::{BudgetStatusResponse, PausedTasksByHoldReason, StatusResponse};
use super::{CommandResponse, Completion, ResponseBody};
use kio_core::scope::FileStatus;
use kio_pipeline::budget::BudgetCapKind;
use kio_pipeline::unsupported::UnsupportedInputDisposition;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn assert_projection<T: Serialize>(payload: T, legacy: Value) {
    let actual = serde_json::to_value(payload).expect("project status DTO");
    assert_eq!(actual, legacy);
    assert_eq!(
        serde_json::to_vec(&actual).unwrap(),
        serde_json::to_vec(&legacy).unwrap()
    );
}

fn uninitialized_budget() -> BudgetStatusResponse {
    BudgetStatusResponse {
        budget_status: "unknown".to_owned(),
        budget_reason: Some("KIO-E-LEDGER-UNINITIALIZED-001".to_owned()),
        month: "2026-10".to_owned(),
        device_monthly_usd_cap: 20.0,
        folder_monthly_usd_cap: None,
        device_spent_usd: None,
        folder_spent_usd: None,
        device_remaining_usd: None,
        folder_remaining_usd: None,
        cap_kind: None,
        device_per_adapter: BTreeMap::new(),
        hard_stop: true,
        warn_at_percent: 80,
        warned: false,
        warning: None,
        ledger_status: Some("uninitialized".to_owned()),
    }
}

fn legacy_uninitialized_budget() -> Value {
    json!({"budget_status":"unknown","budget_reason":"KIO-E-LEDGER-UNINITIALIZED-001","month":"2026-10","device_monthly_usd_cap":20.0,"folder_monthly_usd_cap":null,
        "device_spent_usd":null,"folder_spent_usd":null,"device_remaining_usd":null,
        "folder_remaining_usd":null,"cap_kind":null,"device_per_adapter":{},"hard_stop":true,
        "warn_at_percent":80,"warned":false,"warning":null,"ledger_status":"uninitialized"})
}

fn empty_status(quarantine: Vec<Value>) -> StatusResponse {
    StatusResponse {
        scope_path: PathBuf::from("scope/日本語/.kio"),
        files: Vec::new(),
        head_shallow: false,
        paused_by_hold_reason: PausedTasksByHoldReason {
            budget: 0,
            auth: 0,
            tier_b_approval: 0,
            ledger_initialization_required: 0,
        },
        tasks: Vec::new(),
        quarantine,
        unsupported_inputs: Vec::new(),
        unsupported_inputs_complete: true,
        budget: uninitialized_budget(),
        stalled_batch: Vec::new(),
        active_purge_journal: None,
        active_gc_sweep: None,
    }
}

fn legacy_empty_status(quarantine: Vec<Value>) -> Value {
    json!({"scope_path":PathBuf::from("scope/日本語/.kio"),"files":[],"head_shallow":false,
        "paused_by_hold_reason":{"budget":0,"auth":0,"tier_b_approval":0,"ledger_initialization_required":0},
        "tasks":[],"quarantine":quarantine,"unsupported_inputs":[],"unsupported_inputs_complete":true,
        "budget":legacy_uninitialized_budget(),"stalled_batch":[],"active_purge_journal":null,"active_gc_sweep":null})
}

#[test]
fn status_projection_preserves_canonical_quarantine_rows_and_order() {
    let rows = vec![
        json!({"path":"秘密.txt","reason":"secrets_tier_a","recorded_at":"2026-10-13T00:00:00Z","approval_method":"quarantine"}),
        json!({"path":"other.txt","reason":"tier_b","recorded_at":"2026-10-13T00:00:01Z","approval_method":"hold"}),
    ];
    assert_projection(empty_status(rows.clone()), legacy_empty_status(rows));
}

#[test]
fn status_projection_preserves_empty_arrays_and_explicit_nullable_fields() {
    assert_projection(empty_status(Vec::new()), legacy_empty_status(Vec::new()));
}

#[test]
fn status_projection_preserves_extra_missing_nonstring_scalar_and_null_quarantine_rows() {
    // Projection consumes the reader's output; it must not validate or regroup it.
    for row in [
        json!({"path":"extra.txt","reason":"hold","recorded_at":"now","approval_method":"hold","extra":{"nested":[1,null]}}),
        json!({"path":"missing.txt"}),
        json!({"path":7,"reason":false}),
        json!("scalar"),
        json!(42),
        json!(true),
        json!([1, "row"]),
        Value::Null,
    ] {
        assert_projection(
            empty_status(vec![row.clone()]),
            legacy_empty_status(vec![row]),
        );
    }
}

#[test]
fn status_budget_projection_preserves_initialized_caps_warning_and_omitted_ledger_status() {
    for (folder_cap, folder_remaining, cap_kind, warning) in [
        (None, None, BudgetCapKind::Device, None),
        (
            Some(5.0),
            Some(1.0),
            BudgetCapKind::Folder,
            Some("budget warning"),
        ),
    ] {
        let per_adapter = BTreeMap::from([("embedding".to_owned(), 3.0)]);
        let legacy = json!({"budget_status":"known","budget_reason":null,"month":"2026-10","device_monthly_usd_cap":20.0,
            "folder_monthly_usd_cap":folder_cap,"device_spent_usd":18.0,"folder_spent_usd":4.0,
            "device_remaining_usd":2.0,"folder_remaining_usd":folder_remaining,"cap_kind":cap_kind,
            "device_per_adapter":per_adapter,"hard_stop":true,"warn_at_percent":80,
            "warned":warning.is_some(),"warning":warning});
        assert_projection(
            BudgetStatusResponse {
                budget_status: "known".to_owned(),
                budget_reason: None,
                month: "2026-10".to_owned(),
                device_monthly_usd_cap: 20.0,
                folder_monthly_usd_cap: folder_cap,
                device_spent_usd: Some(18.0),
                folder_spent_usd: Some(4.0),
                device_remaining_usd: Some(2.0),
                folder_remaining_usd: folder_remaining,
                cap_kind: Some(cap_kind),
                device_per_adapter: per_adapter,
                hard_stop: true,
                warn_at_percent: 80,
                warned: warning.is_some(),
                warning: warning.map(str::to_owned),
                ledger_status: None,
            },
            legacy,
        );
    }
}

#[test]
fn status_stalled_batch_projection_preserves_nullable_fields_and_signed_timestamp_type() {
    for (intent, error, issued_at_ms) in [
        (None, None, None),
        (
            Some("intent"),
            Some("cleanup pending"),
            Some(1780000000000_i64),
        ),
    ] {
        let legacy = json!({"scope_id":"scope","adapter_kind":"embedding","input_hash":"input",
            "tool_profile_hash":"tool","intent_token":intent,"error":error,"estimated_usd":0.25,"issued_at_ms":issued_at_ms});
        assert_projection(
            StalledBatchStatus {
                scope_id: "scope".to_owned(),
                adapter_kind: "embedding".to_owned(),
                input_hash: "input".to_owned(),
                tool_profile_hash: "tool".to_owned(),
                intent_token: intent.map(str::to_owned),
                error: error.map(str::to_owned),
                estimated_usd: 0.25,
                issued_at_ms,
            },
            legacy,
        );
    }
}

#[test]
fn status_projection_preserves_domain_file_omission_and_unsupported_input_records() {
    let mut dto = empty_status(Vec::new());
    let mut legacy = legacy_empty_status(Vec::new());
    dto.files = vec![
        FileStatus {
            path: None,
            relative_path: "shallow.txt".to_owned(),
            status: None,
            raw_hash: None,
        },
        FileStatus {
            path: Some(PathBuf::from("scope/new.txt")),
            relative_path: "new.txt".to_owned(),
            status: Some("new".to_owned()),
            raw_hash: Some("raw".to_owned()),
        },
    ];
    legacy["files"] = json!([{"relative_path":"shallow.txt"},{"path":PathBuf::from("scope/new.txt"),"relative_path":"new.txt","status":"new","raw_hash":"raw"}]);
    dto.unsupported_inputs = vec![UnsupportedInputDisposition {
        path: "clip.bin".to_owned(),
        raw_hash: "hash".to_owned(),
        media_type: "application/octet-stream".to_owned(),
        size_bytes: 7,
        reason: "unsupported".to_owned(),
    }];
    legacy["unsupported_inputs"] = json!([{"path":"clip.bin","raw_hash":"hash","media_type":"application/octet-stream","size_bytes":7,"reason":"unsupported"}]);
    assert_projection(dto, legacy);
}

#[test]
fn status_complete_projection_keeps_shallow_and_paused_inspection_successful() {
    for shallow in [false, true] {
        let mut dto = empty_status(Vec::new());
        let mut legacy = legacy_empty_status(Vec::new());
        dto.head_shallow = shallow;
        dto.unsupported_inputs_complete = !shallow;
        dto.paused_by_hold_reason = PausedTasksByHoldReason {
            budget: 1,
            auth: 2,
            tier_b_approval: 3,
            ledger_initialization_required: 4,
        };
        legacy["head_shallow"] = json!(shallow);
        legacy["unsupported_inputs_complete"] = json!(!shallow);
        legacy["paused_by_hold_reason"] =
            json!({"budget":1,"auth":2,"tier_b_approval":3,"ledger_initialization_required":4});
        let response = CommandResponse {
            body: ResponseBody::Status(Box::new(dto)),
            completion: Completion::Complete,
        };
        assert_eq!(response.completion, Completion::Complete);
        let actual = serde_json::to_value(response.body).unwrap();
        assert_eq!(actual, legacy);
        assert_eq!(
            serde_json::to_vec(&actual).unwrap(),
            serde_json::to_vec(&legacy).unwrap()
        );
        assert!(actual.get("__exit_code").is_none());
    }
}

#[test]
fn status_projection_preserves_existing_task_nullable_fields_and_empty_unit_arrays() {
    use kio_pipeline::task::{TaskDescriptor, TaskStatus, TaskType};
    let mut dto = empty_status(Vec::new());
    let mut legacy = legacy_empty_status(Vec::new());
    dto.tasks = vec![TaskDescriptor {
        task_id: "task".to_owned(),
        task_type: TaskType::Embedding,
        embedding_target: Some(kio_pipeline::task::EmbeddingTaskTarget::Chunk {
            chunk_hash: kio_core::cas::hash_bytes(b"status chunk"),
            embedding_hash: kio_core::cas::hash_bytes(b"status embedding"),
            profile_hash: kio_core::cas::hash_bytes(b"status profile"),
            input_hash: kio_core::cas::hash_bytes(b"status input"),
        }),
        mode: None,
        input_path: "doc.md".to_owned(),
        input_hash: "input".to_owned(),
        previous_raw_hash: None,
        parent_run_id: None,
        changed_unit_keys: Vec::new(),
        output_ref: "output".to_owned(),
        unit_keys: Some(Vec::new()),
        status: TaskStatus::Pending,
        attempts: 0,
        next_retry_at: None,
        deadline: None,
        heartbeat_at: None,
        fallback_reason: None,
        created_at: "now".to_owned(),
        bbox_annotation_enabled: None,
        hold_reason: None,
        reserved_usd: None,
        reserved_month: None,
        reservation_id: None,
    }];
    legacy["tasks"] = json!([{"task_id":"task","type":"embedding","mode":null,"input_path":"doc.md",
        "input_hash":"input","previous_raw_hash":null,"parent_run_id":null,"changed_unit_keys":[],
        "output_ref":"output","unit_keys":[],"status":"pending","attempts":0,"next_retry_at":null,
        "deadline":null,"heartbeat_at":null,"fallback_reason":null,"created_at":"now",
        "bbox_annotation_enabled":null,"hold_reason":null,"reserved_usd":null,"reserved_month":null,"reservation_id":null}]);
    legacy["tasks"][0]["embedding_target"] = json!({
        "kind": "chunk",
        "chunk_hash": kio_core::cas::hash_bytes(b"status chunk"),
        "embedding_hash": kio_core::cas::hash_bytes(b"status embedding"),
        "profile_hash": kio_core::cas::hash_bytes(b"status profile"),
        "input_hash": kio_core::cas::hash_bytes(b"status input"),
    });
    assert_projection(dto, legacy);
}

#[test]
fn status_projection_preserves_present_recovery_records_without_reinterpreting_them() {
    let mut dto = empty_status(Vec::new());
    let mut legacy = legacy_empty_status(Vec::new());
    let purge = json!({"schema_version":3,"purge_id":"purge","target_raw_hashes":[],"reason":"privacy",
        "tombstone_mode":"default","actor":"user","started_at":"now","target_epoch":1,
        "closure_hash":"closure","planned_commit":"commit","phase":"prepared"});
    let gc = json!({"version":1,"sweep_id":"sweep","started_at":"now","phase":"prepared",
        "plan_digest":"plan","truth_digest":"truth","stable_truth_digest":"stable",
        "baseline_receipts_digest":"baseline","operation_receipts_digest":null,"candidates":[],
        "trees":[],"estimated_bytes":0,"index_initial":{"kind":"absent"},
        "index_pre_sweep":null,"index_final":null,"index_rotation":null});
    dto.active_purge_journal = Some(serde_json::from_value(purge.clone()).unwrap());
    dto.active_gc_sweep = Some(serde_json::from_value(gc.clone()).unwrap());
    legacy["active_purge_journal"] = purge;
    legacy["active_gc_sweep"] = gc;
    assert_projection(dto, legacy);
}
