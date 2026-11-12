use kio_adapter::types::{
    EmbeddingCompleted, EmbeddingLogicalJobId, EmbeddingMemberOutcome, EmbeddingMemberResult,
    EmbeddingUsage,
};
use kio_pipeline::ledger::embedding_job::{
    EmbeddingDeviceClaimOutcome, EmbeddingDeviceClaimRequest, EmbeddingReservationOutcome,
    EmbeddingReserveRequest,
};
use kio_pipeline::{
    ledger::{
        BatchState, LedgerDb, Outcome, RequestKind, TaskKey,
        embedding_job::{EmbeddingJob, EmbeddingJobBinding, EmbeddingJobMember},
        ops,
    },
    task::EmbeddingTaskTarget,
};

fn hash(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}
fn key() -> TaskKey {
    TaskKey::new("scope_contract", "embedding", hash('a'), hash('b'))
}
fn binding() -> EmbeddingJobBinding {
    EmbeddingJobBinding {
        logical_job_id: EmbeddingLogicalJobId("logical_contract".to_owned()),
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
    }
}
fn reserve(db: &LedgerDb) -> EmbeddingJob {
    ops::reserve_embedding_job(
        db,
        &EmbeddingReserveRequest {
            key: &key(),
            candidate_usd: 1.0,
            caps: &ops::BudgetCapConfig {
                device_cap: 100.0,
                folder_cap: Some(100.0),
                device_per_adapter_cap: Some(100.0),
            },
            request_kind: RequestKind::Sync,
            timeout_seconds: Some(300),
            bypass_cap_denial: false,
            binding: &binding(),
        },
    )
    .map(expect_job)
    .unwrap()
}
fn completed() -> EmbeddingCompleted {
    EmbeddingCompleted {
        members: vec![EmbeddingMemberOutcome {
            id: "member1".to_owned(),
            result: EmbeddingMemberResult::Vector {
                vector: vec![1.0, 0.0, 0.0],
            },
        }],
        dimensions: 3,
        distance: "cosine".to_owned(),
        modality: "multimodal".to_owned(),
        embedding_profile_hash: hash('b'),
        usage: EmbeddingUsage::Estimated,
    }
}

#[test]
fn o1_private_journal_survives_task_loss_and_terminal_token_clear() {
    let temp = tempfile::tempdir().unwrap();
    let db = LedgerDb::initialize(temp.path().join("device/cost-ledger.sqlite")).unwrap();
    let job = reserve(&db);
    assert!(!temp.path().join("tasks.jsonl").exists());
    ops::stage_embedding_result(&db, &job, &completed()).unwrap();
    let receipt = ops::terminal_transaction(
        &db,
        &ops::TerminalWrite {
            key: &job.key,
            outcome: Outcome::Succeeded,
            billed: ops::BilledAmount {
                usd: 1.0,
                estimated: true,
            },
            ledger_batch_job_id: &job.intent_token,
            next_state: BatchState::Completed,
            error: None,
            increment_contract_violation: false,
            attempts_delta: 1,
            clear_intent_token: true,
            intent_token_guard: Some(&job.intent_token),
            reseat_submission_seq: false,
        },
    )
    .unwrap();
    assert!(receipt.recorded);
    let row = ops::get_batch_request(&db, &job.key).unwrap().unwrap();
    assert!(row.intent_token.is_none());
    assert!(row.batch_job_id.is_none());
    let recovered = ops::embedding_job(&db, &row).unwrap().unwrap();
    assert_eq!(recovered.binding, job.binding);
    assert_eq!(recovered.known_result, Some(completed()));
    ops::finish_embedding_publication(&db, &recovered).unwrap();
    ops::finish_embedding_publication(&db, &recovered).unwrap();
    assert!(ops::embedding_job(&db, &row).unwrap().is_none());
    let connection = rusqlite::Connection::open_with_flags(
        temp.path().join("device/cost-ledger.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 4);
}

#[test]
fn o1_private_journal_rejects_result_tampering_before_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let db = LedgerDb::initialize(temp.path().join("device/cost-ledger.sqlite")).unwrap();
    let job = reserve(&db);
    ops::stage_embedding_result(&db, &job, &completed()).unwrap();
    let path = std::fs::read_dir(temp.path().join("device"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.to_string_lossy().ends_with(".result.json"))
        .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    value["result"]["members"][0]["id"] = serde_json::json!("foreign_member");
    std::fs::write(&path, serde_jcs::to_vec(&value).unwrap()).unwrap();
    let row = ops::get_batch_request(&db, &job.key).unwrap().unwrap();
    assert!(ops::embedding_job(&db, &row).is_err());
    assert!(
        ops::cost_ledger_rows_for_key(&db, &job.key)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn o1_private_journal_rejects_duplicate_or_cross_profile_members_before_reservation() {
    let temp = tempfile::tempdir().unwrap();
    let db = LedgerDb::initialize(temp.path().join("device/cost-ledger.sqlite")).unwrap();
    let mut bad = binding();
    bad.members.push(bad.members[0].clone());
    let caps = ops::BudgetCapConfig {
        device_cap: 100.0,
        folder_cap: None,
        device_per_adapter_cap: None,
    };
    assert!(
        ops::reserve_embedding_job(
            &db,
            &EmbeddingReserveRequest {
                key: &key(),
                candidate_usd: 1.0,
                caps: &caps,
                request_kind: RequestKind::Sync,
                timeout_seconds: Some(300),
                bypass_cap_denial: false,
                binding: &bad
            }
        )
        .is_err()
    );
    let mut wrong = key();
    wrong.tool_profile_hash = hash('e');
    assert!(
        ops::reserve_embedding_job(
            &db,
            &EmbeddingReserveRequest {
                key: &wrong,
                candidate_usd: 1.0,
                caps: &caps,
                request_kind: RequestKind::Sync,
                timeout_seconds: Some(300),
                bypass_cap_denial: false,
                binding: &binding()
            }
        )
        .is_err()
    );
    assert!(ops::get_batch_request(&db, &key()).unwrap().is_none());
}

#[test]
fn o1_existing_known_job_preserves_original_binding_against_new_tasks() {
    let temp = tempfile::tempdir().unwrap();
    let db = LedgerDb::initialize(temp.path().join("device/cost-ledger.sqlite")).unwrap();
    let original = reserve(&db);
    ops::stage_embedding_result(&db, &original, &completed()).unwrap();
    let mut changed = binding();
    changed.logical_job_id.0 = "new_replanned_tasks".to_owned();
    changed.members[0].id = "new_member".to_owned();
    let observed = ops::reserve_embedding_job(
        &db,
        &EmbeddingReserveRequest {
            key: &key(),
            candidate_usd: 1.0,
            caps: &ops::BudgetCapConfig {
                device_cap: 100.0,
                folder_cap: None,
                device_per_adapter_cap: None,
            },
            request_kind: RequestKind::Sync,
            timeout_seconds: Some(300),
            bypass_cap_denial: false,
            binding: &changed,
        },
    )
    .map(expect_job)
    .unwrap();
    assert_eq!(observed.binding, original.binding);
    assert_eq!(observed.intent_token, original.intent_token);
    assert_eq!(observed.known_result, Some(completed()));
}

fn settle_success(db: &LedgerDb, job: &EmbeddingJob) {
    ops::terminal_transaction(
        db,
        &ops::TerminalWrite {
            key: &job.key,
            outcome: Outcome::Succeeded,
            billed: ops::BilledAmount {
                usd: 1.0,
                estimated: true,
            },
            ledger_batch_job_id: &job.intent_token,
            next_state: BatchState::Completed,
            error: None,
            increment_contract_violation: false,
            attempts_delta: 1,
            clear_intent_token: true,
            intent_token_guard: Some(&job.intent_token),
            reseat_submission_seq: false,
        },
    )
    .unwrap();
}

#[test]
fn o1_terminal_known_result_is_recovered_before_a_new_reservation() {
    let temp = tempfile::tempdir().unwrap();
    let db = LedgerDb::initialize(temp.path().join("device/cost-ledger.sqlite")).unwrap();
    let original = reserve(&db);
    ops::stage_embedding_result(&db, &original, &completed()).unwrap();
    settle_success(&db, &original);
    let recovered = reserve(&db);
    assert_eq!(
        recovered.submission_seq, original.submission_seq,
        "known settled results must remain reachable before another attempt"
    );
    assert_eq!(recovered.intent_token, original.intent_token);
    assert_eq!(recovered.known_result, Some(completed()));
    assert_eq!(ops::cost_ledger_rows_for_key(&db, &key()).unwrap().len(), 1);
}

#[test]
fn o1_old_paid_attempt_cleanup_uses_its_terminal_audit_after_next_reservation() {
    let temp = tempfile::tempdir().unwrap();
    let db = LedgerDb::initialize(temp.path().join("device/cost-ledger.sqlite")).unwrap();
    let original = reserve(&db);
    ops::recovery_settle_unknown(&db, &original.key, &original.intent_token, 1.0, true).unwrap();
    assert!(ops::authorize_unknown_sync_resend(&db, &original.key).unwrap());
    let next = reserve(&db);
    ops::finish_embedding_publication(&db, &original)
        .expect("old terminal cost receipt remains authority after the live row advances");
    let row = ops::get_batch_request(&db, &next.key).unwrap().unwrap();
    assert_eq!(
        ops::embedding_job(&db, &row).unwrap().unwrap().intent_token,
        next.intent_token
    );
    let bindings = std::fs::read_dir(temp.path().join("device"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.to_string_lossy().ends_with(".binding.json"))
        .count();
    assert_eq!(bindings, 1);
}

#[test]
fn o1_device_terminal_prune_does_not_leave_unreachable_binding_artifacts() {
    let temp = tempfile::tempdir().unwrap();
    let db = LedgerDb::initialize(temp.path().join("device/cost-ledger.sqlite")).unwrap();
    let device_key = TaskKey::new("device", "embedding", hash('d'), hash('b'));
    let mut query = binding();
    query.members[0].target = EmbeddingTaskTarget::Query {
        query_hash: hash('d'),
        profile_hash: hash('b'),
        input_hash: hash('d'),
    };
    let outcome = ops::claim_device_embedding_job(
        &db,
        &EmbeddingDeviceClaimRequest {
            key: &device_key,
            now_ms: db.now_millis(),
            candidate_usd: 1.0,
            timeout_seconds: 300,
            device_cap: 100.0,
            per_adapter_cap: None,
            binding: &query,
        },
    )
    .unwrap();
    let EmbeddingDeviceClaimOutcome::Claimed(job) = outcome else {
        panic!("expected fresh query claim")
    };
    settle_success(&db, &job);
    let future = db.now_millis() + 40 * 86400 * 1000;
    let report = ops::execute_bounded_sweep(
        &db,
        &ops::SweepPlan {
            own_key_stale: vec![],
            general_stale: vec![],
            prune: vec![device_key.clone()],
        },
        future,
    )
    .unwrap();
    assert_eq!(report.pruned, vec![device_key]);
    let bindings = std::fs::read_dir(temp.path().join("device"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.to_string_lossy().ends_with(".binding.json"))
        .count();
    assert_eq!(
        bindings, 0,
        "prune must clean its authenticated body-free query binding"
    );
}

fn expect_job(outcome: EmbeddingReservationOutcome) -> EmbeddingJob {
    match outcome {
        EmbeddingReservationOutcome::Reserved(job)
        | EmbeddingReservationOutcome::KnownResult(job)
        | EmbeddingReservationOutcome::InFlight(job) => job,
        EmbeddingReservationOutcome::BudgetExceeded => panic!("unexpected budget denial"),
    }
}

#[test]
fn o1_fresh_query_claim_cleans_previous_same_month_terminal_binding_after_process_loss() {
    let temp = tempfile::tempdir().unwrap();
    let db = LedgerDb::initialize(temp.path().join("device/cost-ledger.sqlite")).unwrap();
    let query_key = TaskKey::new("device", "embedding", hash('d'), hash('b'));
    let mut query = binding();
    query.members[0].target = EmbeddingTaskTarget::Query {
        query_hash: hash('d'),
        profile_hash: hash('b'),
        input_hash: hash('d'),
    };
    let claim = |ledger: &LedgerDb| {
        let outcome = ops::claim_device_embedding_job(
            ledger,
            &EmbeddingDeviceClaimRequest {
                key: &query_key,
                now_ms: ledger.now_millis(),
                candidate_usd: 1.0,
                timeout_seconds: 300,
                device_cap: 100.0,
                per_adapter_cap: None,
                binding: &query,
            },
        )
        .unwrap();
        let EmbeddingDeviceClaimOutcome::Claimed(job) = outcome else {
            panic!("expected fresh query claim")
        };
        job
    };
    let original = claim(&db);
    settle_success(&db, &original);
    // Normal settlement committed; process loss prevents the publication cleanup.
    let reopened = LedgerDb::open_existing(db.path()).unwrap();
    let next = claim(&reopened);
    assert_eq!(next.submission_seq, original.submission_seq + 1);
    assert_ne!(next.intent_token, original.intent_token);
    let row = ops::get_batch_request(&reopened, &query_key)
        .unwrap()
        .unwrap();
    assert_eq!(
        ops::embedding_job(&reopened, &row)
            .unwrap()
            .unwrap()
            .intent_token,
        next.intent_token
    );
    let bindings = std::fs::read_dir(temp.path().join("device"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.to_string_lossy().ends_with(".binding.json"))
        .count();
    assert_eq!(
        bindings, 1,
        "a same-month terminal query binding must not become unreachable"
    );
}

#[test]
fn o1_fresh_document_reservation_cleans_terminal_binding_without_staged_result() {
    let temp = tempfile::tempdir().unwrap();
    let db = LedgerDb::initialize(temp.path().join("device/cost-ledger.sqlite")).unwrap();
    let original = reserve(&db);
    settle_success(&db, &original);
    // No staged result remains (e.g. process loss during completed cleanup).
    let reopened = LedgerDb::open_existing(db.path()).unwrap();
    let next = reserve(&reopened);
    assert_eq!(next.submission_seq, original.submission_seq + 1);
    assert_ne!(next.intent_token, original.intent_token);
    let row = ops::get_batch_request(&reopened, &key()).unwrap().unwrap();
    assert_eq!(
        ops::embedding_job(&reopened, &row)
            .unwrap()
            .unwrap()
            .intent_token,
        next.intent_token
    );
    let bindings = std::fs::read_dir(temp.path().join("device"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.to_string_lossy().ends_with(".binding.json"))
        .count();
    assert_eq!(
        bindings, 1,
        "replacing a terminal row must retain cleanup authority for its old binding"
    );
}

#[test]
fn o1_unknown_sync_settlement_queues_original_binding_before_sequence_reseat() {
    let temp = tempfile::tempdir().unwrap();
    let db = LedgerDb::initialize(temp.path().join("device/cost-ledger.sqlite")).unwrap();
    let original = reserve(&db);
    ops::recovery_settle_unknown(&db, &original.key, &original.intent_token, 1.0, true).unwrap();
    let reopened = LedgerDb::open_existing(db.path()).unwrap();
    assert!(ops::authorize_unknown_sync_resend(&reopened, &original.key).unwrap());
    let next = reserve(&reopened);
    assert_eq!(next.submission_seq, original.submission_seq + 2);
    let row = ops::get_batch_request(&reopened, &key()).unwrap().unwrap();
    assert_eq!(
        ops::embedding_job(&reopened, &row)
            .unwrap()
            .unwrap()
            .intent_token,
        next.intent_token
    );
    let costs = ops::cost_ledger_rows_for_key(&reopened, &original.key).unwrap();
    assert_eq!(costs.len(), 1);
    assert_eq!(costs[0].outcome, Outcome::UnknownSettled);
    assert_eq!(costs[0].submission_seq, original.submission_seq + 1);
    let bindings = std::fs::read_dir(temp.path().join("device"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.to_string_lossy().ends_with(".binding.json"))
        .count();
    assert_eq!(
        bindings, 1,
        "unknown settlement must retain cleanup authority before reseating its row"
    );
}

#[test]
fn o1_unknown_query_settlement_cleans_original_binding_after_process_loss() {
    let temp = tempfile::tempdir().unwrap();
    let db = LedgerDb::initialize(temp.path().join("device/cost-ledger.sqlite")).unwrap();
    let query_key = TaskKey::new("device", "embedding", hash('d'), hash('b'));
    let mut query = binding();
    query.members[0].target = EmbeddingTaskTarget::Query {
        query_hash: hash('d'),
        profile_hash: hash('b'),
        input_hash: hash('d'),
    };
    let claim = |ledger: &LedgerDb| {
        let outcome = ops::claim_device_embedding_job(
            ledger,
            &EmbeddingDeviceClaimRequest {
                key: &query_key,
                now_ms: ledger.now_millis(),
                candidate_usd: 1.0,
                timeout_seconds: 300,
                device_cap: 100.0,
                per_adapter_cap: None,
                binding: &query,
            },
        )
        .unwrap();
        let EmbeddingDeviceClaimOutcome::Claimed(job) = outcome else {
            panic!("expected fresh query claim")
        };
        job
    };
    let original = claim(&db);
    ops::recovery_settle_unknown(&db, &query_key, &original.intent_token, 1.0, true).unwrap();
    let reopened = LedgerDb::open_existing(db.path()).unwrap();
    let next = claim(&reopened);
    assert_eq!(next.submission_seq, original.submission_seq + 2);
    let row = ops::get_batch_request(&reopened, &query_key)
        .unwrap()
        .unwrap();
    assert_eq!(
        ops::embedding_job(&reopened, &row)
            .unwrap()
            .unwrap()
            .intent_token,
        next.intent_token
    );
    let bindings = std::fs::read_dir(temp.path().join("device"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.to_string_lossy().ends_with(".binding.json"))
        .count();
    assert_eq!(
        bindings, 1,
        "a reseated query row must not lose its original cleanup identity"
    );
}
