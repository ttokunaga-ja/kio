use super::Completion;
use crate::task_execution::{
    ExecOutcome, batch_completion, batch_error_code, enrichment_completion,
};

#[test]
fn batch_completion_uses_touched_outcomes_and_semantic_priority() {
    for (outcome, expected) in [
        (ExecOutcome::default(), Completion::Complete),
        (
            ExecOutcome {
                executed: 2,
                ..Default::default()
            },
            Completion::Complete,
        ),
        (
            ExecOutcome {
                failed: 2,
                ..Default::default()
            },
            Completion::Permanent,
        ),
        (
            ExecOutcome {
                failed: 2,
                failed_retryable: 1,
                ..Default::default()
            },
            Completion::Partial,
        ),
        (
            ExecOutcome {
                failed: 2,
                inflight: 1,
                ..Default::default()
            },
            Completion::Partial,
        ),
        (
            ExecOutcome {
                inflight: 2,
                held_unreadable: 1,
                ..Default::default()
            },
            Completion::Partial,
        ),
        (
            ExecOutcome {
                failed: 2,
                failed_retryable: 1,
                paused: 1,
                inflight: 1,
                ..Default::default()
            },
            Completion::BudgetBlocked,
        ),
        (
            ExecOutcome {
                failed: 2,
                auth_failed: 1,
                failed_retryable: 1,
                paused: 1,
                inflight: 1,
                ..Default::default()
            },
            Completion::AuthRequired,
        ),
    ] {
        assert_eq!(batch_completion(&outcome), expected);
    }
    // No stored history is consulted: a pass touching nothing completes.
    assert_eq!(
        batch_completion(&ExecOutcome::default()),
        Completion::Complete
    );
    assert_eq!(
        batch_error_code(Completion::Partial).as_deref(),
        Some("KIO-E-BATCH-PARTIAL-001")
    );
    assert_eq!(
        batch_error_code(Completion::Permanent).as_deref(),
        Some("KIO-E-BATCH-TASK-FAILED-001")
    );
    for complete in [
        Completion::Complete,
        Completion::AuthRequired,
        Completion::BudgetBlocked,
    ] {
        assert_eq!(batch_error_code(complete), None);
    }
}
#[test]
fn enrichment_completion_only_promotes_auth_and_budget() {
    for (outcome, expected) in [
        (ExecOutcome::default(), Completion::Complete),
        (
            ExecOutcome {
                failed: 2,
                ..Default::default()
            },
            Completion::Complete,
        ),
        (
            ExecOutcome {
                failed: 2,
                failed_retryable: 1,
                inflight: 1,
                ..Default::default()
            },
            Completion::Complete,
        ),
        (
            ExecOutcome {
                failed: 2,
                paused: 1,
                ..Default::default()
            },
            Completion::BudgetBlocked,
        ),
        (
            ExecOutcome {
                failed: 2,
                auth_failed: 1,
                paused: 1,
                ..Default::default()
            },
            Completion::AuthRequired,
        ),
    ] {
        assert_eq!(enrichment_completion(&outcome), expected);
    }
}
