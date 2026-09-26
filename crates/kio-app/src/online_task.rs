//! Typed ownership checks for standard online markdownize task lifecycles.

use std::path::Path;

use kio_pipeline::task::{TaskDescriptor, TaskOutputRef, validate_task_output_ref};

/// Identify a markdownize task that the standard online executor owns across both
/// output-ref phases. Fresh tasks carry the online placeholder; a Partial task requeued
/// for a unit-scoped retry must retain its normalized-instance reference so the complete
/// prepared-unit manifest remains loadable. Every online markdownize task carries an
/// explicit `bbox_annotation_enabled` policy stamp, and `unit_keys` is stamped when
/// such a normalized task is made retryable.
pub(crate) fn targets_standard_online_markdownize(
    kio_dir: &Path,
    task: &TaskDescriptor,
    placeholder: &str,
) -> bool {
    match validate_task_output_ref(kio_dir, task) {
        Ok(TaskOutputRef::Online { .. }) => {
            task.bbox_annotation_enabled.is_some() && task.output_ref == placeholder
        }
        Ok(TaskOutputRef::NormalizedInstance { .. }) => {
            task.bbox_annotation_enabled.is_some()
                && (task.unit_keys.is_some()
                    || task.fallback_reason.as_deref() == Some("online_adapter_done"))
        }
        // An `offline_api` task is deliberately invisible here. Every caller of
        // this predicate drives the ONLINE lane — the network opt-in, the
        // ledger reservation, the batch send, the auth revive — and a local
        // pipeline's task must not be picked up by any of them.
        Ok(TaskOutputRef::Offline { .. } | TaskOutputRef::Embedding { .. }) | Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use kio_pipeline::markdownize::MarkdownizeMode;
    use kio_pipeline::task::{TaskStatus, TaskType};

    use super::*;

    fn online_task(bbox_annotation_enabled: Option<bool>) -> TaskDescriptor {
        TaskDescriptor {
            task_id: "task_current".to_owned(),
            task_type: TaskType::Markdownize,
            mode: Some(MarkdownizeMode::Full),
            input_path: "report.pdf".to_owned(),
            input_hash: format!("sha256:{}", "a".repeat(64)),
            previous_raw_hash: None,
            parent_run_id: None,
            changed_unit_keys: Vec::new(),
            output_ref: "online:mistral_ocr_markdownize".to_owned(),
            unit_keys: None,
            status: TaskStatus::Pending,
            attempts: 0,
            next_retry_at: None,
            deadline: None,
            heartbeat_at: None,
            fallback_reason: None,
            created_at: "2026-08-13T00:00:00Z".to_owned(),
            bbox_annotation_enabled,
            hold_reason: None,
            reserved_usd: None,
            reserved_month: None,
            reservation_id: None,
        }
    }

    #[test]
    fn online_placeholder_requires_an_explicit_bbox_policy_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let kio_dir = dir.path().join(".kio");
        assert!(!targets_standard_online_markdownize(
            &kio_dir,
            &online_task(None),
            "online:mistral_ocr_markdownize"
        ));
        assert!(targets_standard_online_markdownize(
            &kio_dir,
            &online_task(Some(false)),
            "online:mistral_ocr_markdownize"
        ));
    }
}
