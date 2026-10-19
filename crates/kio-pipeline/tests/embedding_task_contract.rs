//! Observable current embedding Task format contracts (04 §5.1/§5.2).

use kio_pipeline::task::{TaskDescriptor, TaskStatus, task_can_complete_from_materialized_output};
use serde_json::{Value, json};

fn markdownize_task() -> Value {
    json!({
        "task_id": "task_current_contract", "type": "markdownize", "mode": "full",
        "input_path": "report.pdf", "input_hash": format!("sha256:{}", "a".repeat(64)),
        "previous_raw_hash": null, "parent_run_id": null, "changed_unit_keys": [],
        "output_ref": "online:test_markdownize", "unit_keys": null,
        "status": "pending", "attempts": 0, "next_retry_at": null, "deadline": null,
        "heartbeat_at": null, "fallback_reason": null, "created_at": "2026-10-19T00:00:00Z",
        "bbox_annotation_enabled": true, "hold_reason": null,
        "reserved_usd": null, "reserved_month": null, "reservation_id": null
    })
}

#[test]
fn o1_markdownize_current_wire_keys_remain_exact() {
    let original = markdownize_task();
    let task: TaskDescriptor = serde_json::from_value(original.clone()).unwrap();
    assert_eq!(serde_json::to_value(task).unwrap(), original);
}

#[test]
fn o1_embedding_task_without_typed_target_is_rejected() {
    let mut old = markdownize_task();
    old["type"] = json!("embedding");
    old["mode"] = Value::Null;
    old["bbox_annotation_enabled"] = Value::Null;
    old["changed_unit_keys"] = json!([format!("sha256:{}", "b".repeat(64))]);
    old["output_ref"] = json!(format!(
        "embedding:sha256:{}/sha256:{}",
        "b".repeat(64),
        "c".repeat(64)
    ));
    assert!(
        serde_json::from_value::<TaskDescriptor>(old).is_err(),
        "pre-typed embedding tasks must not be interpreted as current tasks"
    );
}

#[test]
fn o1_skipped_status_is_distinct_and_cannot_complete_from_cached_output() {
    let skipped: TaskStatus = serde_json::from_value(json!("skipped"))
        .expect("policy/capability exclusion must have its own persisted status");
    assert!(!matches!(
        skipped,
        TaskStatus::Pending | TaskStatus::Done | TaskStatus::Failed
    ));
    let mut task: TaskDescriptor = serde_json::from_value(markdownize_task()).unwrap();
    task.status = skipped;
    task.fallback_reason = Some("image_capability_unavailable".to_owned());
    assert!(!task_can_complete_from_materialized_output(&task));
    assert_eq!(serde_json::to_value(skipped).unwrap(), json!("skipped"));
}
