use super::*;
use rusqlite::params;
use std::fs;

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .canonicalize()
        .unwrap()
        .join("device/cost-ledger.sqlite");
    (dir, path)
}
fn companion(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}
fn error_code(error: PipelineError) -> String {
    match error {
        PipelineError::Contract { code, .. } => code.into(),
        other => panic!("{other}"),
    }
}
fn read_checkpoint(path: &Path) -> Checkpoint {
    decode(&fs::read(companion(path, ".checkpoint.json")).unwrap()).unwrap()
}
fn pending_write(authority: &Authority, before: &Checkpoint) -> PendingWrite {
    PendingWrite {
        version: 1,
        ledger_id: authority.ledger_id.clone(),
        era: authority.era.clone(),
        schema_version: 2,
        transaction_nonce: random_id().unwrap(),
        before: WriteState {
            seq: before.seq,
            security_token: before.security_token.clone(),
        },
        after: WriteState {
            seq: before.seq + 1,
            security_token: random_id().unwrap(),
        },
    }
}
fn write_pending(path: &Path, pending: &PendingWrite) {
    let session = FsSession::acquire(path, false).unwrap();
    session
        .create_only(&session.names().write_pending, &encode(pending).unwrap())
        .unwrap();
    session.sync_parent().unwrap();
}
fn apply_database_after(path: &Path, pending: &PendingWrite) {
    let raw = Connection::open(path).unwrap();
    raw.execute(
        "UPDATE ledger_metadata SET sequence=?1, security_token=?2 WHERE singleton=1",
        params![pending.after.seq as i64, &pending.after.security_token],
    )
    .unwrap();
}

#[test]
fn backup_binds_wal_safe_image_to_authority_and_checkpoint() {
    let (_dir, path) = fixture();
    let ledger = LedgerDb::initialize(&path).unwrap();
    let backup = ledger.backup().unwrap();
    assert_eq!(backup.sequence, 0);
    assert!(!backup.sqlite.is_empty());
    assert_eq!(
        ledger.restore_readiness(&backup).unwrap(),
        RestoreReadiness::Ready
    );
}

#[test]
fn old_backup_blocks_paid_restore_without_history_evidence() {
    let (_dir, path) = fixture();
    let ledger = LedgerDb::initialize(&path).unwrap();
    let backup = ledger.backup().unwrap();
    ledger.write(|_| Ok(())).unwrap();
    assert_eq!(
        ledger.restore_readiness(&backup).unwrap(),
        RestoreReadiness::HistoryEvidenceRequired
    );
}

#[test]
fn explicit_restore_only_recreates_a_missing_proven_generation() {
    let (_dir, path) = fixture();
    let ledger = LedgerDb::initialize(&path).unwrap();
    let backup = ledger.backup().unwrap();
    fs::remove_file(&path).unwrap();
    let restored = LedgerDb::restore_missing_database(&path, &backup).unwrap();
    assert_eq!(
        restored.restore_readiness(&backup).unwrap(),
        RestoreReadiness::Ready
    );
    assert!(!companion(&path, ".init.pending").exists());
    assert!(!companion(&path, ".restore.pending").exists());
}

#[test]
fn restore_requires_a_surviving_private_proof_before_any_publication() {
    let (_dir, path) = fixture();
    let ledger = LedgerDb::initialize(&path).unwrap();
    let backup = ledger.backup().unwrap();
    fs::remove_file(companion(&path, ".backup.proof.json")).unwrap();
    fs::remove_file(&path).unwrap();

    assert_eq!(
        error_code(LedgerDb::restore_missing_database(&path, &backup).unwrap_err()),
        "KIO-E-LEDGER-RESTORE-EVIDENCE-001"
    );
    assert!(!path.exists());
    assert!(!companion(&path, ".restore.pending").exists());
}

#[test]
fn restore_rejects_a_changed_backup_before_creating_database_or_journal() {
    let (_dir, path) = fixture();
    let ledger = LedgerDb::initialize(&path).unwrap();
    let mut backup = ledger.backup().unwrap();
    // This models a backup whose manifest was recomputed after its SQLite
    // payload changed. The surviving private proof is the independent trust
    // anchor and must reject it before restore writes anything.
    *backup.sqlite.last_mut().unwrap() ^= 1;
    fs::remove_file(&path).unwrap();

    assert_eq!(
        error_code(LedgerDb::restore_missing_database(&path, &backup).unwrap_err()),
        "KIO-E-LEDGER-RESTORE-EVIDENCE-001"
    );
    assert!(!path.exists());
    assert!(!companion(&path, ".restore.pending").exists());
}

#[test]
fn restore_resumes_only_the_matching_interrupted_restore_journal() {
    let (_dir, path) = fixture();
    let ledger = LedgerDb::initialize(&path).unwrap();
    let backup = ledger.backup().unwrap();
    let checkpoint = read_checkpoint(&path);
    let pending = RestorePending {
        version: 1,
        ledger_id: ledger.authority.ledger_id.clone(),
        era: ledger.authority.era.clone(),
        sequence: checkpoint.seq,
        backup_digest: backup.sqlite_digest(),
    };
    fs::remove_file(&path).unwrap();
    let session = FsSession::acquire(&path, false).unwrap();
    session
        .create_only(&session.names().restore_pending, &encode(&pending).unwrap())
        .unwrap();
    session
        .create_only(&session.names().db, &backup.sqlite)
        .unwrap();
    drop(session);

    let restored = LedgerDb::restore_missing_database(&path, &backup).unwrap();
    assert_eq!(
        restored.restore_readiness(&backup).unwrap(),
        RestoreReadiness::Ready
    );
    assert!(!companion(&path, ".restore.pending").exists());
}

#[test]
fn normal_open_is_noncreating_and_init_is_create_only() {
    let (_dir, path) = fixture();
    assert_eq!(
        error_code(LedgerDb::open_existing(&path).unwrap_err()),
        "KIO-E-LEDGER-UNINITIALIZED-001"
    );
    assert!(!path.exists());
    assert!(
        !path
            .parent()
            .unwrap()
            .join("ledger.lifecycle.lock")
            .exists()
    );
    let db = LedgerDb::initialize(&path).unwrap();
    assert_eq!(db.integrity_check().unwrap(), "ok");
    let before = fs::read(&path).unwrap();
    assert_eq!(
        error_code(LedgerDb::initialize(&path).unwrap_err()),
        "KIO-E-LEDGER-ALREADY-EXISTS-001"
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(valid_id(&db.authority.ledger_id));
    assert!(valid_id(&db.authority.era));
    assert_ne!(db.authority.ledger_id, db.authority.era);
}

#[test]
fn every_init_publication_can_resume_only_its_recorded_identity() {
    for stop in [
        InitPhase::Intent,
        InitPhase::Database,
        InitPhase::Configured,
        InitPhase::Checkpoint,
        InitPhase::Authority,
    ] {
        let (_dir, path) = fixture();
        LedgerDb::initialize_with_hook(&path, |phase| {
            if phase == stop {
                Err(contract(
                    "TEST-INTERRUPTED",
                    "simulated process interruption",
                ))
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        let pending: InitPending =
            decode(&fs::read(companion(&path, ".init.pending")).unwrap()).unwrap();
        assert!(LedgerDb::open_existing(&path).is_err());
        assert!(LedgerDb::initialize(&path).is_err());
        let resumed = LedgerDb::resume_initialization(&path).unwrap();
        assert_eq!(resumed.authority, pending.authority);
        assert_eq!(resumed.integrity_check().unwrap(), "ok");
        assert!(!companion(&path, ".init.pending").exists());
        assert_eq!(read_checkpoint(&path).seq, 0);
    }
}

#[test]
fn failed_operation_rolls_back_before_checkpoint_publication() {
    let (_dir, path) = fixture();
    let db = LedgerDb::initialize(&path).unwrap();
    let before = fs::read(companion(&path, ".checkpoint.json")).unwrap();
    let result: Result<()> = db.write(|tx| {
        tx.connection().execute(
            "INSERT INTO schema_migrations VALUES('must-rollback',1)",
            [],
        )?;
        Err(contract("TEST-ROLLBACK", "operation failed"))
    });
    assert!(result.is_err());
    assert_eq!(
        before,
        fs::read(companion(&path, ".checkpoint.json")).unwrap()
    );
    assert_eq!(
        db.read(|conn| Ok(conn
            .query_row("SELECT count(*) FROM schema_migrations", [], |r| r
                .get::<_, i64>(0))?))
            .unwrap(),
        0
    );
    assert!(!companion(&path, ".write.pending.json").exists());
    db.write(|_| Ok(())).unwrap();
}

#[test]
fn checkpoint_before_commit_interruption_is_explicitly_recoverable() {
    let (_dir, path) = fixture();
    let db = LedgerDb::initialize(&path).unwrap();
    let result = db.write_with_hook(
        |tx| {
            tx.connection().execute(
                "INSERT INTO schema_migrations VALUES('must-rollback',1)",
                [],
            )?;
            Ok(())
        },
        || {
            Err(contract(
                "TEST-INTERRUPTED",
                "stopped after checkpoint fsync",
            ))
        },
    );
    assert!(result.is_err());
    assert_eq!(read_checkpoint(&path).seq, 1);
    let raw = Connection::open(&path).unwrap();
    assert_eq!(
        raw.query_row("SELECT sequence FROM ledger_metadata", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        raw.query_row("SELECT count(*) FROM schema_migrations", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    drop(raw);
    assert!(companion(&path, ".write.pending.json").exists());
    assert!(LedgerDb::open_existing(&path).is_err());
    assert!(db.write(|_| Ok(())).is_err());
    assert_eq!(
        LedgerDb::recover_pending_write(&path).unwrap(),
        PendingWriteRecovery::RestoredBeforeCheckpoint
    );
    assert_eq!(read_checkpoint(&path).seq, 0);
    assert!(!companion(&path, ".write.pending.json").exists());
    assert_eq!(
        LedgerDb::open_existing(&path)
            .unwrap()
            .integrity_check()
            .unwrap(),
        "ok"
    );
}

#[test]
fn recovery_classifies_abort_and_committed_checkpoint_gaps_without_replaying_sql() {
    // A journal before any durable mutation is just an aborted operation.
    let (_dir, path) = fixture();
    let db = LedgerDb::initialize(&path).unwrap();
    let before = read_checkpoint(&path);
    let pending = pending_write(&db.authority, &before);
    write_pending(&path, &pending);
    assert_eq!(
        LedgerDb::recover_pending_write(&path).unwrap(),
        PendingWriteRecovery::Aborted
    );

    // A committed database with the old checkpoint publishes only the exact
    // journaled checkpoint; the accounting row is never replayed or erased.
    let (_dir, path) = fixture();
    let db = LedgerDb::initialize(&path).unwrap();
    let before = read_checkpoint(&path);
    let pending = pending_write(&db.authority, &before);
    apply_database_after(&path, &pending);
    write_pending(&path, &pending);
    assert_eq!(
        LedgerDb::recover_pending_write(&path).unwrap(),
        PendingWriteRecovery::PublishedAfterCheckpoint
    );
    assert_eq!(read_checkpoint(&path).seq, 1);

    // A committed checkpointed write only needs finalization of the exact
    // journal. A cost-like durable row remains present throughout recovery.
    let (_dir, path) = fixture();
    let db = LedgerDb::initialize(&path).unwrap();
    let before = read_checkpoint(&path);
    let pending = pending_write(&db.authority, &before);
    Connection::open(&path)
        .unwrap()
        .execute(
            "INSERT INTO schema_migrations(name, applied_at) VALUES('committed-cost-preserved', 1)",
            [],
        )
        .unwrap();
    apply_database_after(&path, &pending);
    let after = checkpoint_from_state(&db.authority, &pending.after);
    fs::write(
        companion(&path, ".checkpoint.json"),
        encode(&after).unwrap(),
    )
    .unwrap();
    write_pending(&path, &pending);
    assert_eq!(
        LedgerDb::recover_pending_write(&path).unwrap(),
        PendingWriteRecovery::FinalizedCommitted
    );
    assert_eq!(
        LedgerDb::open_existing(&path)
            .unwrap()
            .read(|conn| Ok(conn.query_row(
                "SELECT count(*) FROM schema_migrations WHERE name='committed-cost-preserved'",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .unwrap(),
        1
    );
    assert_eq!(
        LedgerDb::recover_pending_write(&path).unwrap(),
        PendingWriteRecovery::NoPending
    );
}

#[test]
fn recovery_refuses_tampered_or_foreign_journal_without_mutation() {
    let (_dir, path) = fixture();
    let db = LedgerDb::initialize(&path).unwrap();
    let before = read_checkpoint(&path);
    let mut pending = pending_write(&db.authority, &before);
    pending.after.security_token = pending.before.security_token.clone();
    write_pending(&path, &pending);
    let journal = companion(&path, ".write.pending.json");
    let original = fs::read(&journal).unwrap();
    assert_eq!(
        error_code(LedgerDb::recover_pending_write(&path).unwrap_err()),
        "KIO-E-LEDGER-WRITE-RECOVERY-001"
    );
    assert_eq!(fs::read(journal).unwrap(), original);
    assert_eq!(read_checkpoint(&path), before);
}

#[test]
fn read_snapshot_fails_closed_on_pending_write_without_resolving_it() {
    let (_dir, path) = fixture();
    let db = LedgerDb::initialize(&path).unwrap();
    let before = read_checkpoint(&path);
    let pending = pending_write(&db.authority, &before);
    write_pending(&path, &pending);
    let journal = companion(&path, ".write.pending.json");
    let original = fs::read(&journal).unwrap();
    assert!(crate::ledger::LedgerReadSnapshot::open(&path).is_err());
    assert_eq!(fs::read(journal).unwrap(), original);
    assert_eq!(read_checkpoint(&path), before);
}

#[test]
fn missing_or_malformed_checkpoint_never_becomes_a_new_baseline() {
    for replacement in [None, Some(b"invalid".as_slice())] {
        let (_dir, path) = fixture();
        let db = LedgerDb::initialize(&path).unwrap();
        let checkpoint = companion(&path, ".checkpoint.json");
        if let Some(bytes) = replacement {
            fs::write(&checkpoint, bytes).unwrap();
        } else {
            fs::remove_file(&checkpoint).unwrap();
        }
        assert!(db.write(|_| Ok(())).is_err());
        assert!(LedgerDb::open_existing(&path).is_err());
        match replacement {
            None => assert!(!checkpoint.exists()),
            Some(bytes) => assert_eq!(fs::read(checkpoint).unwrap(), bytes),
        }
    }
}

#[test]
fn stale_handle_refuses_another_era_and_database_ahead_is_rejected() {
    let (_dir, path) = fixture();
    let db = LedgerDb::initialize(&path).unwrap();
    let raw = Connection::open(&path).unwrap();
    raw.execute("UPDATE ledger_metadata SET sequence=1", [])
        .unwrap();
    drop(raw);
    assert_eq!(
        error_code(LedgerDb::open_existing(&path).unwrap_err()),
        "KIO-E-LEDGER-SEQUENCE-MISMATCH-001"
    );
    let mut authority = db.authority.clone();
    authority.era = random_id().unwrap();
    fs::write(
        companion(&path, ".authority.json"),
        encode(&authority).unwrap(),
    )
    .unwrap();
    assert!(db.read(|_| Ok(())).is_err());
}

#[test]
fn sequence_exhaustion_is_an_error_and_preserves_checkpoint() {
    let (_dir, path) = fixture();
    let db = LedgerDb::initialize(&path).unwrap();
    let raw = Connection::open(&path).unwrap();
    raw.execute("UPDATE ledger_metadata SET sequence=?1", [i64::MAX])
        .unwrap();
    drop(raw);
    let mut checkpoint = read_checkpoint(&path);
    checkpoint.seq = i64::MAX as u64;
    fs::write(
        companion(&path, ".checkpoint.json"),
        encode(&checkpoint).unwrap(),
    )
    .unwrap();
    assert_eq!(
        error_code(db.write(|_| Ok(())).unwrap_err()),
        "KIO-E-LEDGER-SEQUENCE-OVERFLOW-001"
    );
    assert_eq!(read_checkpoint(&path).seq, i64::MAX as u64);
}
