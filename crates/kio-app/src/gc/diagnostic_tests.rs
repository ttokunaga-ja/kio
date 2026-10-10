use super::*;
use crate::test_support::{DiagnosticCall, DiagnosticInteraction, assert_diagnostic_io_error};
use std::sync::Arc;

const PROMPT: &str = "Create shallow receipts and remove planned tree objects? [y/N]";

fn run(sink: &Arc<DiagnosticInteraction>, yes: bool, json_mode: bool) -> Result<()> {
    confirm(&sink.context(), &json!({"z": 1}), 1, false, yes, json_mode)
}

#[test]
fn gc_preview_diagnostic_write_failure_prevents_confirmation() {
    for write_at in [1, 2] {
        let sink = Arc::new(DiagnosticInteraction::new(Some(write_at), false));
        let error = run(&sink, false, false).unwrap_err();
        assert_diagnostic_io_error(&error, "write");
        assert_eq!(sink.calls().len(), write_at);
        assert!(
            sink.calls()
                .iter()
                .all(|call| matches!(call, DiagnosticCall::Write(_)))
        );
    }
}

#[test]
fn gc_preview_diagnostic_flush_failure_prevents_confirmation() {
    let sink = Arc::new(DiagnosticInteraction::new(None, true));
    let error = run(&sink, false, false).unwrap_err();
    assert_diagnostic_io_error(&error, "flush");
    assert_eq!(
        sink.calls(),
        vec![
            DiagnosticCall::Write("GC shallow-sweep preview:\n{\n  \"z\": 1\n}\n".into()),
            DiagnosticCall::Write(format!("{PROMPT} ")),
            DiagnosticCall::Flush,
        ]
    );
}

#[test]
fn gc_preview_diagnostics_preserve_bytes_flush_and_double_prompt() {
    let sink = Arc::new(DiagnosticInteraction::new(None, false));
    run(&sink, false, false).unwrap();
    assert_eq!(
        sink.calls(),
        vec![
            DiagnosticCall::Write("GC shallow-sweep preview:\n{\n  \"z\": 1\n}\n".into()),
            DiagnosticCall::Write(format!("{PROMPT} ")),
            DiagnosticCall::Flush,
            DiagnosticCall::Confirm(PROMPT.into()),
        ]
    );
    assert_eq!(
        sink.bytes(),
        format!("GC shallow-sweep preview:\n{{\n  \"z\": 1\n}}\n{PROMPT} {PROMPT}\n").as_bytes()
    );
}

#[test]
fn gc_preview_diagnostics_are_skipped_for_yes_and_json_mode() {
    let sink = Arc::new(DiagnosticInteraction::new(Some(1), true));
    run(&sink, true, false).unwrap();
    assert!(sink.calls().is_empty());
    assert_eq!(
        run(&sink, false, true).unwrap_err().exit_code(),
        ExitCode::InvalidUsage
    );
    assert!(sink.calls().is_empty());
}
