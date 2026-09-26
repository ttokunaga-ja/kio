use std::cell::Cell;

use kio_adapter::gemini_batch_client::{
    GeminiBatchClient, GeminiBatchEmbedInput, GeminiBatchJobRecord, GeminiBatchPoll,
    GeminiBatchState, batch_display_name,
};
use kio_pipeline::ledger::ops::{
    cost_ledger_rows_for_key, get_batch_request, phase1_intent, phase2a_record_provider_scope,
    phase2b_record_job_created, recovery_settle_unknown,
};
use kio_pipeline::ledger::{LedgerDb, RequestKind, TaskKey};

struct CleanupClient {
    scope: String,
    record: GeminiBatchJobRecord,
    unreadable: bool,
    polls: Cell<usize>,
}

impl GeminiBatchClient for CleanupClient {
    fn provider_scope_id(&self) -> kio_adapter::Result<String> {
        Ok(self.scope.clone())
    }
    fn create_embedding_job(
        &self,
        _: &str,
        _: u32,
        _: &str,
        _: &[GeminiBatchEmbedInput],
    ) -> kio_adapter::Result<GeminiBatchJobRecord> {
        panic!("cleanup must never submit a new request")
    }
    fn poll_job(&self, name: &str) -> kio_adapter::Result<GeminiBatchPoll> {
        assert_eq!(name, "batches/recorded-job");
        self.polls.set(self.polls.get() + 1);
        if self.unreadable {
            return Err(kio_adapter::AdapterError::Network("unavailable".into()));
        }
        Ok(GeminiBatchPoll {
            record: self.record.clone(),
            results: None,
        })
    }
    fn list_jobs(&self) -> kio_adapter::Result<Vec<GeminiBatchJobRecord>> {
        panic!("known-job cleanup must use the exact durable job id")
    }
}

#[test]
fn terminal_cleanup_requires_attributed_terminal_inline_job_and_preserves_charge() {
    for case in [
        "terminal",
        "running",
        "foreign_scope",
        "foreign_job",
        "foreign_token",
        "file_output",
        "unreadable",
        "inflight",
        "missing_job",
    ] {
        let root = tempfile::tempdir().unwrap();
        let ledger = LedgerDb::initialize(root.path().join("device/ledger.sqlite")).unwrap();
        let key = TaskKey::new("scope", super::EMBEDDING_ADAPTER_KIND, "input", "profile");
        let intent = phase1_intent(&ledger, &key, RequestKind::Batch, 1.0, None).unwrap();
        phase2a_record_provider_scope(&ledger, &key, &intent.intent_token, "gemini:project")
            .unwrap();
        if case != "missing_job" {
            phase2b_record_job_created(&ledger, &key, &intent.intent_token, "batches/recorded-job")
                .unwrap();
        }
        if case != "inflight" {
            recovery_settle_unknown(&ledger, &key, &intent.intent_token, 1.0, false).unwrap();
        }
        let before = get_batch_request(&ledger, &key).unwrap().unwrap();
        let charges = cost_ledger_rows_for_key(&ledger, &key).unwrap();
        let client = CleanupClient {
            scope: if case == "foreign_scope" {
                "gemini:other"
            } else {
                "gemini:project"
            }
            .into(),
            record: GeminiBatchJobRecord {
                name: if case == "foreign_job" {
                    "batches/other"
                } else {
                    "batches/recorded-job"
                }
                .into(),
                state: if case == "running" {
                    GeminiBatchState::Running
                } else {
                    GeminiBatchState::Succeeded
                },
                display_name: batch_display_name(if case == "foreign_token" {
                    "different-intent"
                } else {
                    &intent.intent_token
                }),
                responses_file: (case == "file_output").then(|| "files/provider-output".into()),
            },
            unreadable: case == "unreadable",
            polls: Cell::new(0),
        };
        // There is deliberately no scope SQLite DB or vector projection here:
        // releasing a completed operational cleanup must not depend on either.
        super::recover_terminal_embedding_batch_cleanup(&ledger, &client, "scope").unwrap();
        let after = get_batch_request(&ledger, &key).unwrap().unwrap();
        if case == "terminal" {
            assert!(after.intent_token.is_none());
            let mut expected = before.clone();
            expected.intent_token = None;
            assert_eq!(after, expected);
            super::recover_terminal_embedding_batch_cleanup(&ledger, &client, "scope").unwrap();
            assert_eq!(client.polls.get(), 1, "cleaned rows must not poll again");
        } else {
            assert_eq!(after, before, "{case} must remain pending");
        }
        assert_eq!(
            cost_ledger_rows_for_key(&ledger, &key).unwrap(),
            charges,
            "{case} must not change billing"
        );
        if matches!(case, "foreign_scope" | "inflight" | "missing_job") {
            assert_eq!(client.polls.get(), 0, "{case} must not contact a provider");
        }
    }
}
