use kio_adapter::types::{EmbeddingLogicalJobId, EmbeddingOutcome, EmbeddingRemoteHandle};
use kio_pipeline::{
    ledger::{
        BatchState, LedgerDb, Outcome, RequestKind, TaskKey,
        embedding_job::{
            EmbeddingJob, EmbeddingJobBinding, EmbeddingJobMember, EmbeddingReservationOutcome,
            EmbeddingReserveRequest,
        },
        ops,
    },
    task::EmbeddingTaskTarget,
};

fn hash(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}
fn reserve(db: &LedgerDb, local: bool) -> EmbeddingJob {
    let key = TaskKey::new("scope_pending", "embedding", hash('a'), hash('b'));
    let binding = EmbeddingJobBinding {
        logical_job_id: EmbeddingLogicalJobId("logical_pending".to_owned()),
        local,
        dimensions: 3,
        distance: "cosine".to_owned(),
        modality: "multimodal".to_owned(),
        members: vec![EmbeddingJobMember {
            id: "member1".to_owned(),
            target: EmbeddingTaskTarget::Chunk {
                chunk_hash: hash('c'),
                embedding_hash: hash('a'),
                profile_hash: hash('b'),
                input_hash: hash('d'),
            },
        }],
    };
    let caps = ops::BudgetCapConfig {
        device_cap: 100.0,
        folder_cap: None,
        device_per_adapter_cap: None,
    };
    let outcome = ops::reserve_embedding_job(
        db,
        &EmbeddingReserveRequest {
            key: &key,
            candidate_usd: if local { 0.0 } else { 1.0 },
            caps: &caps,
            request_kind: if local {
                RequestKind::Sync
            } else {
                RequestKind::Batch
            },
            timeout_seconds: Some(300),
            bypass_cap_denial: false,
            binding: &binding,
        },
    )
    .unwrap();
    let EmbeddingReservationOutcome::Reserved(job) = outcome else {
        panic!("expected fresh reservation")
    };
    job
}
fn pending(name: &str) -> EmbeddingOutcome {
    EmbeddingOutcome::Pending {
        remote_handle: EmbeddingRemoteHandle(name.to_owned()),
    }
}
fn prepare_remote(db: &LedgerDb, job: &EmbeddingJob) {
    assert!(
        ops::phase2a_record_provider_scope(
            db,
            &job.key,
            &job.intent_token,
            "credential_bound_scope"
        )
        .unwrap()
    );
    assert!(ops::phase2b_record_job_create_started(db, &job.key, &job.intent_token).unwrap());
}

#[test]
fn o1_typed_pending_cannot_attach_fake_remote_handle_to_local_sync_job() {
    let temp = tempfile::tempdir().unwrap();
    let db = LedgerDb::initialize(temp.path().join("device/cost-ledger.sqlite")).unwrap();
    let job = reserve(&db, true);
    let before = ops::get_batch_request(&db, &job.key).unwrap().unwrap();
    let outcome = ops::record_embedding_pending(&db, &job, &pending("batches/fake_local"));
    let after = ops::get_batch_request(&db, &job.key).unwrap().unwrap();
    assert_eq!(
        after, before,
        "local execution cannot write a remote handle or change request state"
    );
    assert!(outcome.is_err());
    assert!(
        ops::cost_ledger_rows_for_key(&db, &job.key)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn o1_typed_pending_preserves_existing_remote_handle_and_is_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let db = LedgerDb::initialize(temp.path().join("device/cost-ledger.sqlite")).unwrap();
    let job = reserve(&db, false);
    prepare_remote(&db, &job);
    assert!(ops::record_embedding_pending(&db, &job, &pending("batches/original")).unwrap());
    let before = ops::get_batch_request(&db, &job.key).unwrap().unwrap();
    let conflicting = ops::record_embedding_pending(&db, &job, &pending("batches/foreign"));
    assert_eq!(
        ops::get_batch_request(&db, &job.key).unwrap().unwrap(),
        before,
        "a second Pending outcome cannot overwrite the original handle"
    );
    assert!(conflicting.is_err());
    assert!(!ops::record_embedding_pending(&db, &job, &pending("batches/original")).unwrap());
}

#[test]
fn o1_typed_pending_stale_original_job_is_a_cas_miss_without_sql_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let db = LedgerDb::initialize(temp.path().join("device/cost-ledger.sqlite")).unwrap();
    let original = reserve(&db, false);
    prepare_remote(&db, &original);
    assert!(ops::record_embedding_pending(&db, &original, &pending("batches/original")).unwrap());
    ops::terminal_transaction(
        &db,
        &ops::TerminalWrite {
            key: &original.key,
            outcome: Outcome::Succeeded,
            billed: ops::nonbillable_charge(),
            ledger_batch_job_id: "batches/original",
            next_state: BatchState::Completed,
            error: None,
            increment_contract_violation: false,
            attempts_delta: 1,
            clear_intent_token: true,
            intent_token_guard: Some(&original.intent_token),
            reseat_submission_seq: false,
        },
    )
    .unwrap();
    let fresh = reserve(&db, false);
    prepare_remote(&db, &fresh);
    let before = ops::get_batch_request(&db, &fresh.key).unwrap().unwrap();
    assert!(!ops::record_embedding_pending(&db, &original, &pending("batches/stale")).unwrap());
    assert_eq!(
        ops::get_batch_request(&db, &fresh.key).unwrap().unwrap(),
        before
    );
}
