use super::*;
use crate::test_support::{DiagnosticCall, DiagnosticInteraction, assert_diagnostic_io_error};
use std::sync::Arc;

const PROMPT: &str = "Remove this content from KIO-managed history? [y/N]";
const WARNING: &str = "warning: 2 working-tree file(s) still contain the exact bytes being purged. The next `kio index` will re-discover and re-ingest them, making their pointer alive again. To permanently exclude them, delete the file(s) or add them to `.kioignore`.\n";

fn preview(mode: TombstoneMode, completed: bool, aliases: u64) -> PurgePreview {
    PurgePreview {
        plan: PurgePlan {
            head_commit: "head".into(),
            target_raw_hashes: vec!["raw".into()],
            historical_paths: vec![],
            reason: PurgeReason::Privacy,
            tombstone_mode: mode,
        },
        completed: completed.then(|| CompletedTerminal {
            purged_in_commit: "purged".into(),
            tombstone_count: 1,
            erase_receipt_count: 0,
        }),
        working_tree_alias_count: aliases,
    }
}

fn run(sink: &Arc<DiagnosticInteraction>, preview: &PurgePreview, yes: bool) -> Result<()> {
    confirm(&sink.context(), preview, yes)
}

fn expected_calls(mode: &str, completed: bool, aliases: bool) -> Vec<DiagnosticCall> {
    let mut calls = vec![DiagnosticCall::Write(format!(
        "Purge preview: 1 raw object(s); reason=privacy; terminal={mode}.\n"
    ))];
    if completed {
        calls.push(DiagnosticCall::Write(
            "This exact purge is already complete.\n".into(),
        ));
    }
    if aliases {
        calls.push(DiagnosticCall::Write(WARNING.into()));
    }
    calls.push(DiagnosticCall::Write(format!("{PROMPT} ")));
    calls.push(DiagnosticCall::Flush);
    calls.push(DiagnosticCall::Confirm(PROMPT.into()));
    calls
}

#[test]
fn purge_preview_diagnostic_write_failure_prevents_confirmation() {
    for write_at in 1..=4 {
        let sink = Arc::new(DiagnosticInteraction::new(Some(write_at), false));
        let error = run(&sink, &preview(TombstoneMode::Default, true, 2), false).unwrap_err();
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
fn purge_preview_diagnostic_flush_failure_prevents_confirmation() {
    let sink = Arc::new(DiagnosticInteraction::new(None, true));
    let error = run(&sink, &preview(TombstoneMode::Default, true, 2), false).unwrap_err();
    assert_diagnostic_io_error(&error, "flush");
    let mut expected = expected_calls("default tombstone", true, true);
    expected.pop();
    assert_eq!(sink.calls(), expected);
}

#[test]
fn purge_preview_diagnostics_preserve_bytes_flush_and_double_prompt() {
    for (mode, label, completed, aliases) in [
        (TombstoneMode::Default, "default tombstone", false, false),
        (TombstoneMode::Default, "default tombstone", true, true),
        (TombstoneMode::Erase, "fsck-only erase receipt", true, true),
    ] {
        let sink = Arc::new(DiagnosticInteraction::new(None, false));
        run(
            &sink,
            &preview(mode, completed, if aliases { 2 } else { 0 }),
            false,
        )
        .unwrap();
        assert_eq!(sink.calls(), expected_calls(label, completed, aliases));
        let mut expected =
            format!("Purge preview: 1 raw object(s); reason=privacy; terminal={label}.\n");
        if completed {
            expected.push_str("This exact purge is already complete.\n");
        }
        if aliases {
            expected.push_str(WARNING);
        }
        expected.push_str(&format!("{PROMPT} {PROMPT}\n"));
        assert_eq!(sink.bytes(), expected.as_bytes());
    }
}

#[test]
fn purge_preview_diagnostics_are_skipped_for_yes() {
    let sink = Arc::new(DiagnosticInteraction::new(Some(1), true));
    run(&sink, &preview(TombstoneMode::Default, true, 2), true).unwrap();
    assert!(sink.calls().is_empty());
}
