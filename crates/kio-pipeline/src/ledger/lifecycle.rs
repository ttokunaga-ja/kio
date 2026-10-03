//! Explicit, authority-bound lifecycle for non-rebuildable billing state.
//!
//! Public operations hold an immutable locator, never a writable connection.
//! Each durable domain operation validates the same connection it uses, under
//! the device filesystem lease. Writes publish an exact private recovery
//! journal before their checkpoint, then require explicit recovery after a
//! process interruption.

use super::{
    backup::{LedgerBackup, RestoreReadiness},
    lifecycle_fs::{ArtifactSetState, FsSession},
    schema,
};
use crate::{PipelineError, Result};
use getrandom::fill;
use kio_core::durability::{DurabilityPoint, checkpoint};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

const MAX_RECORD: u64 = 16 * 1024;
const READ_WRITE: OpenFlags = OpenFlags::SQLITE_OPEN_READ_WRITE;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Authority {
    version: u8,
    ledger_id: String,
    era: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    version: u8,
    ledger_id: String,
    era: String,
    seq: u64,
    security_token: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct WriteState {
    seq: u64,
    security_token: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct PendingWrite {
    version: u8,
    ledger_id: String,
    era: String,
    schema_version: u8,
    transaction_nonce: String,
    before: WriteState,
    after: WriteState,
}

/// Result of resolving the one exact private write journal for a ledger.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PendingWriteRecovery {
    NoPending,
    Aborted,
    RestoredBeforeCheckpoint,
    FinalizedCommitted,
    PublishedAfterCheckpoint,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InitPending {
    version: u8,
    init_id: String,
    authority: Authority,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RestorePending {
    version: u8,
    ledger_id: String,
    era: String,
    sequence: u64,
    backup_digest: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct BackupProof {
    version: u8,
    ledger_id: String,
    era: String,
    sequence: u64,
    sqlite_sha256: String,
}

/// Expected authority of a device ledger. Every operation revalidates it.
#[derive(Clone, Debug)]
pub struct LedgerDb {
    path: PathBuf,
    authority: Authority,
}

/// Only the SQL operation module can access a live write transaction.
/// The callback ends before checkpoint publication; this value never exists
/// while the transaction is prepared for commit.
pub(crate) struct LedgerWriteTxn<'a> {
    conn: &'a Connection,
}
impl LedgerWriteTxn<'_> {
    pub(super) fn connection(&self) -> &Connection {
        self.conn
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InitPhase {
    Intent,
    Database,
    Configured,
    Checkpoint,
    Authority,
}

impl LedgerDb {
    /// Create a fresh ledger. Existing or partially initialized state is never
    /// replaced; resuming an interrupted init requires the explicit resume API.
    pub fn initialize(path: impl AsRef<Path>) -> Result<Self> {
        Self::initialize_with_hook(path.as_ref(), |_| Ok(()))
    }
    fn initialize_with_hook(path: &Path, hook: impl Fn(InitPhase) -> Result<()>) -> Result<Self> {
        let fs = FsSession::acquire(path, true)?;
        fs.recover_atomic_storage()?;
        if fs.artifact_state()? != ArtifactSetState::Missing {
            return Err(contract(
                "KIO-E-LEDGER-ALREADY-EXISTS-001",
                "ledger artifacts already exist; use explicit recovery for interrupted initialization",
            ));
        }
        let pending = InitPending {
            version: 1,
            init_id: random_id()?,
            authority: Authority {
                version: 1,
                ledger_id: random_id()?,
                era: random_id()?,
            },
        };
        let bytes = encode(&pending)?;
        fs.create_only(&fs.names().init_pending, &bytes)?;
        hook(InitPhase::Intent)?;
        complete_initialization(&fs, &pending, &bytes, hook)?;
        Ok(Self {
            path: path.to_path_buf(),
            authority: pending.authority,
        })
    }

    /// Resume only the exact recorded initialization. This cannot reset billing
    /// history, generate a replacement era, or adopt an arbitrary database.
    pub fn resume_initialization(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let fs = FsSession::acquire(path, false)?;
        let bytes = fs.read_private(&fs.names().init_pending, MAX_RECORD)?;
        let pending: InitPending = decode(&bytes)?;
        if pending.version != 1 || !valid_id(&pending.init_id) {
            return Err(contract(
                "KIO-E-LEDGER-INIT-PARTIAL-001",
                "initialization intent is malformed",
            ));
        }
        validate_authority(&pending.authority)?;
        complete_initialization(&fs, &pending, &bytes, |_| Ok(()))?;
        Ok(Self {
            path: path.to_path_buf(),
            authority: pending.authority,
        })
    }

    /// Open existing operational state without creating or repairing authority,
    /// schema or history. SQLite may use its normal private WAL coordination
    /// files; read-only status uses LedgerReadSnapshot instead.
    pub fn open_existing(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let fs = FsSession::acquire(path, false)?;
        let (authority, checkpoint) = read_authority(&fs)?;
        let mut bound = fs.open_existing_sqlite(READ_WRITE)?;
        let result = (|| {
            validate_bound_connection(bound.connection(), &authority, &checkpoint)?;
            bound.revalidate_open()
        })();
        let close = bound.finish();
        result.and(close)?;
        Ok(Self {
            path: path.to_path_buf(),
            authority,
        })
    }
    pub fn open_default_existing() -> Result<Self> {
        Self::open_existing(schema::default_ledger_path()?)
    }
    /// Resolve only a complete, exact private pending-write journal. Ordinary
    /// open and read APIs deliberately never invoke this mutating operation.
    pub fn recover_pending_write(path: impl AsRef<Path>) -> Result<PendingWriteRecovery> {
        let path = path.as_ref();
        let fs = FsSession::acquire(path, false)?;
        fs.recover_atomic_storage()?;
        let Some(bytes) = fs.read_optional_private(&fs.names().write_pending, MAX_RECORD)? else {
            // Preserve the normal strict validation even for an empty recovery
            // request; it must never bless a malformed ledger as recoverable.
            let (authority, checkpoint) = read_authority(&fs)?;
            let mut bound = fs.open_existing_sqlite(READ_WRITE)?;
            let result = (|| {
                validate_bound_connection(bound.connection(), &authority, &checkpoint)?;
                bound.revalidate_open()
            })();
            let close = bound.finish();
            result.and(close)?;
            return Ok(PendingWriteRecovery::NoPending);
        };
        let pending: PendingWrite = decode(&bytes).map_err(|_| recovery_invalid())?;
        let (authority, checkpoint, checkpoint_bytes) = read_authority_for_recovery(&fs)?;
        validate_pending(&pending, &authority)?;
        let before = checkpoint_from_state(&authority, &pending.before);
        let after = checkpoint_from_state(&authority, &pending.after);
        // Checkpoint publication always uses this canonical encoding. Refuse
        // equivalent-looking hand edits too: recovery acts only on the exact
        // bytes generated by the lifecycle writer.
        if checkpoint_bytes != encode(&checkpoint)? || (checkpoint != before && checkpoint != after)
        {
            return Err(recovery_invalid());
        }
        let mut bound = fs.open_existing_sqlite(READ_WRITE)?;
        let result = (|| {
            let actual = read_database_state(bound.connection())?;
            validate_database_authority(bound.connection(), &authority)?;
            let db_before = actual == pending.before;
            let db_after = actual == pending.after;
            let checkpoint_before = checkpoint == before;
            let checkpoint_after = checkpoint == after;
            let outcome = match (db_before, checkpoint_before, db_after, checkpoint_after) {
                (true, true, false, false) => PendingWriteRecovery::Aborted,
                (true, false, false, true) => {
                    fs.replace_existing(&fs.names().checkpoint, &encode(&before)?)?;
                    fs.sync_parent()?;
                    PendingWriteRecovery::RestoredBeforeCheckpoint
                }
                (false, false, true, true) => PendingWriteRecovery::FinalizedCommitted,
                (false, true, true, false) => {
                    fs.replace_existing(&fs.names().checkpoint, &encode(&after)?)?;
                    fs.sync_parent()?;
                    PendingWriteRecovery::PublishedAfterCheckpoint
                }
                _ => return Err(recovery_invalid()),
            };
            let expected = match outcome {
                PendingWriteRecovery::Aborted | PendingWriteRecovery::RestoredBeforeCheckpoint => {
                    &before
                }
                PendingWriteRecovery::FinalizedCommitted
                | PendingWriteRecovery::PublishedAfterCheckpoint => &after,
                PendingWriteRecovery::NoPending => unreachable!(),
            };
            validate_bound_connection(bound.connection(), &authority, expected)?;
            bound.revalidate_open()?;
            fs.remove_exact(&fs.names().write_pending, &bytes, MAX_RECORD)?;
            fs.sync_parent()?;
            Ok(outcome)
        })();
        let close = bound.finish();
        result.and_then(|outcome| close.map(|()| outcome))
    }
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn integrity_check(&self) -> Result<String> {
        self.read(|conn| {
            conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))
                .map_err(Into::into)
        })
    }
    /// Capture a coherent, authority-bound backup while holding the lifecycle
    /// lock. The caller must publish these bytes create-only as one manifest
    /// set; this API never overwrites a backup destination.
    pub fn backup(&self) -> Result<LedgerBackup> {
        let fs = FsSession::acquire(&self.path, false)?;
        let (authority, checkpoint) = self.expected_authority(&fs)?;
        let mut bound = fs.open_existing_sqlite(READ_WRITE)?;
        let result = (|| {
            let sqlite = {
                let conn = bound.connection();
                validate_bound_connection(conn, &authority, &checkpoint)?;
                let busy: i64 =
                    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
                if busy != 0 {
                    return Err(PipelineError::locked("ledger WAL checkpoint"));
                }
                conn.serialize("main")?.to_vec()
            };
            bound.revalidate_open()?;
            let backup = LedgerBackup::new(
                authority.ledger_id.clone(),
                authority.era.clone(),
                checkpoint.seq,
                sqlite,
                fs.read_private(&fs.names().authority, MAX_RECORD)?,
                fs.read_private(&fs.names().checkpoint, MAX_RECORD)?,
            )?;
            let proof = BackupProof {
                version: 1,
                ledger_id: authority.ledger_id.clone(),
                era: authority.era.clone(),
                sequence: checkpoint.seq,
                sqlite_sha256: backup.sqlite_digest(),
            };
            let bytes = encode(&proof)?;
            if fs.contains_leaf(&fs.names().backup_proof)? {
                fs.replace_existing(&fs.names().backup_proof, &bytes)?;
            } else {
                fs.create_only(&fs.names().backup_proof, &bytes)?;
            }
            fs.sync_parent()?;
            durability_checkpoint(DurabilityPoint::LedgerBackupProof)?;
            Ok(backup)
        })();
        let close = bound.finish();
        result.and_then(|backup| close.map(|()| backup))
    }

    /// Inspect a proposed explicit restore. A backup behind the current
    /// checkpoint is never treated as proof that no later paid request ran.
    pub fn restore_readiness(&self, backup: &LedgerBackup) -> Result<RestoreReadiness> {
        let fs = FsSession::acquire(&self.path, false)?;
        let (authority, checkpoint) = self.expected_authority(&fs)?;
        let backup_authority: Authority = decode(&backup.authority)?;
        let backup_checkpoint: Checkpoint = decode(&backup.checkpoint)?;
        let proof = read_backup_proof(&fs)?;
        if backup_authority.ledger_id != backup.ledger_id
            || backup_authority.era != backup.era
            || backup_checkpoint.ledger_id != backup.ledger_id
            || backup_checkpoint.era != backup.era
            || backup_checkpoint.seq != backup.sequence
        {
            return Err(contract(
                "KIO-E-LEDGER-BACKUP-IDENTITY-001",
                "backup manifest and authority records disagree",
            ));
        }
        if backup.ledger_id != authority.ledger_id || backup.era != authority.era {
            return Err(contract(
                "KIO-E-LEDGER-BACKUP-IDENTITY-001",
                "backup authority does not match the live ledger",
            ));
        }
        if backup.sequence != checkpoint.seq
            || proof.version != 1
            || proof.ledger_id != authority.ledger_id
            || proof.era != authority.era
            || proof.sequence != checkpoint.seq
            || proof.sqlite_sha256 != backup.sqlite_digest()
        {
            return Ok(RestoreReadiness::HistoryEvidenceRequired);
        }
        Ok(RestoreReadiness::Ready)
    }
    /// Explicitly restore only a missing database when the surviving live
    /// authority/checkpoint prove this exact backup generation.
    pub fn restore_missing_database(path: impl AsRef<Path>, backup: &LedgerBackup) -> Result<Self> {
        let path = path.as_ref();
        let fs = FsSession::acquire(path, false)?;
        if fs.contains_leaf(&fs.names().write_pending)? {
            return Err(recovery_invalid());
        }
        let authority: Authority = decode(&fs.read_private(&fs.names().authority, MAX_RECORD)?)?;
        let checkpoint: Checkpoint = decode(&fs.read_private(&fs.names().checkpoint, MAX_RECORD)?)?;
        let backup_authority: Authority = decode(&backup.authority)?;
        let backup_checkpoint: Checkpoint = decode(&backup.checkpoint)?;
        let proof = read_backup_proof(&fs)?;
        if authority != backup_authority
            || checkpoint != backup_checkpoint
            || backup.identity()
                != (
                    authority.ledger_id.as_str(),
                    authority.era.as_str(),
                    checkpoint.seq,
                )
            || proof.version != 1
            || proof.ledger_id != authority.ledger_id
            || proof.era != authority.era
            || proof.sequence != checkpoint.seq
            || proof.sqlite_sha256 != backup.sqlite_digest()
        {
            return Err(contract(
                "KIO-E-LEDGER-RESTORE-EVIDENCE-001",
                "live authority does not prove this backup generation",
            ));
        }
        let pending = RestorePending {
            version: 1,
            ledger_id: authority.ledger_id.clone(),
            era: authority.era.clone(),
            sequence: checkpoint.seq,
            backup_digest: backup.sqlite_digest(),
        };
        let pending_bytes = encode(&pending)?;
        let db_exists = fs.contains_leaf(&fs.names().db)?;
        if db_exists
            && fs
                .read_optional_private(&fs.names().restore_pending, MAX_RECORD)?
                .is_none()
        {
            return Err(contract(
                "KIO-E-LEDGER-RESTORE-TARGET-001",
                "database exists without restore journal",
            ));
        }
        create_record_or_match(&fs, &fs.names().restore_pending, &pending)?;
        fs.sync_parent()?;
        durability_checkpoint(DurabilityPoint::LedgerRestoreJournal)?;
        if !db_exists {
            fs.create_only(&fs.names().db, &backup.sqlite)?;
        }
        fs.sync_parent()?;
        let mut bound = fs.open_existing_sqlite(READ_WRITE)?;
        let result = (|| {
            validate_bound_connection(bound.connection(), &authority, &checkpoint)?;
            bound.revalidate_open()
        })();
        let close = bound.finish();
        result.and(close)?;
        durability_checkpoint(DurabilityPoint::LedgerRestoreDatabase)?;
        fs.remove_exact(&fs.names().restore_pending, &pending_bytes, MAX_RECORD)?;
        fs.sync_parent()?;
        Ok(Self {
            path: path.to_path_buf(),
            authority,
        })
    }
    pub(crate) fn read<T>(&self, operation: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let fs = FsSession::acquire(&self.path, false)?;
        let (authority, checkpoint) = self.expected_authority(&fs)?;
        let mut bound = fs.open_existing_sqlite(READ_WRITE)?;
        let result = (|| {
            validate_bound_connection(bound.connection(), &authority, &checkpoint)?;
            let value = operation(bound.connection())?;
            bound.revalidate_open()?;
            Ok(value)
        })();
        let close = bound.finish();
        result.and_then(|value| close.map(|()| value))
    }
    pub(crate) fn write<T>(
        &self,
        operation: impl FnOnce(&mut LedgerWriteTxn<'_>) -> Result<T>,
    ) -> Result<T> {
        self.write_with_hook(operation, || Ok(()))
    }
    fn write_with_hook<T>(
        &self,
        operation: impl FnOnce(&mut LedgerWriteTxn<'_>) -> Result<T>,
        prepared: impl FnOnce() -> Result<()>,
    ) -> Result<T> {
        let fs = FsSession::acquire(&self.path, false)?;
        let (authority, checkpoint) = self.expected_authority(&fs)?;
        let next = checkpoint
            .seq
            .checked_add(1)
            .filter(|v| *v <= i64::MAX as u64)
            .ok_or_else(|| {
                contract(
                    "KIO-E-LEDGER-SEQUENCE-OVERFLOW-001",
                    "ledger sequence exhausted; it cannot wrap or saturate",
                )
            })?;
        let after_token = random_id()?;
        let journal = PendingWrite {
            version: 1,
            ledger_id: authority.ledger_id.clone(),
            era: authority.era.clone(),
            schema_version: 2,
            transaction_nonce: random_id()?,
            before: WriteState {
                seq: checkpoint.seq,
                security_token: checkpoint.security_token.clone(),
            },
            after: WriteState {
                seq: next,
                security_token: after_token.clone(),
            },
        };
        let journal_bytes = encode(&journal)?;
        let mut bound = fs.open_existing_sqlite(READ_WRITE)?;
        let result = (|| {
            let conn = bound.connection();
            conn.busy_timeout(Duration::from_secs(5))?;
            conn.execute_batch("PRAGMA synchronous=FULL; BEGIN IMMEDIATE;")?;
            validate_bound_connection(conn, &authority, &checkpoint)?;
            let value = {
                let mut tx = LedgerWriteTxn { conn };
                operation(&mut tx)?
            };
            let changed = conn.execute(
                "UPDATE ledger_metadata SET sequence=?1, security_token=?2 WHERE singleton=1 AND sequence=?3 AND security_token=?4",
                params![next as i64, &after_token, checkpoint.seq as i64, &checkpoint.security_token],
            )?;
            if changed != 1 {
                return Err(contract(
                    "KIO-E-LEDGER-SEQUENCE-MISMATCH-001",
                    "ledger sequence changed during the operation",
                ));
            }
            bound.revalidate_open()?;
            // This record contains only authority and state identifiers. It is
            // deliberately not a SQL replay log and carries no paid inputs.
            // It follows every ordinary transactional failure, so a rolled
            // back validation/budget error never leaves the ledger blocked.
            fs.create_only(&fs.names().write_pending, &journal_bytes)?;
            fs.sync_parent()?;
            // SQL mutation is now frozen. A failed COMMIT leaves an ahead
            // checkpoint, which ordinary operations must refuse to acknowledge.
            let checkpoint = Checkpoint {
                seq: next,
                security_token: after_token,
                ..checkpoint
            };
            fs.replace_existing(&fs.names().checkpoint, &encode(&checkpoint)?)?;
            fs.sync_parent()?;
            durability_checkpoint(DurabilityPoint::LedgerCheckpoint)?;
            prepared()?;
            bound.connection().execute_batch("COMMIT;")?;
            validate_bound_connection(bound.connection(), &authority, &checkpoint)?;
            bound.revalidate_open()?;
            durability_checkpoint(DurabilityPoint::LedgerWriteCommitted)?;
            fs.remove_exact(&fs.names().write_pending, &journal_bytes, MAX_RECORD)?;
            fs.sync_parent()?;
            Ok(value)
        })();
        if result.is_err() && !bound.connection().is_autocommit() {
            let _ = bound.connection().execute_batch("ROLLBACK;");
        }
        let close = bound.finish();
        result.and_then(|value| close.map(|()| value))
    }
    fn expected_authority(&self, fs: &FsSession) -> Result<(Authority, Checkpoint)> {
        let (actual, checkpoint) = read_authority(fs)?;
        if actual != self.authority {
            return Err(contract(
                "KIO-E-LEDGER-AUTHORITY-CHANGED-001",
                "ledger authority changed since this handle was opened",
            ));
        }
        Ok((actual, checkpoint))
    }
}

fn complete_initialization(
    fs: &FsSession,
    pending: &InitPending,
    bytes: &[u8],
    hook: impl Fn(InitPhase) -> Result<()>,
) -> Result<()> {
    validate_authority(&pending.authority)?;
    // Construct a complete empty SQLite image privately in memory. Publishing
    // it create-only avoids any crash state with half a public schema or an
    // empty database whose initialization identity cannot be authenticated.
    if !fs.contains_leaf(&fs.names().db)? {
        if fs.contains_leaf(&fs.names().wal)?
            || fs.contains_leaf(&fs.names().shm)?
            || fs.contains_leaf(&fs.names().authority)?
            || fs.contains_leaf(&fs.names().checkpoint)?
            || fs.contains_leaf(&fs.names().write_pending)?
        {
            return Err(contract(
                "KIO-E-LEDGER-INIT-PARTIAL-001",
                "initialization artifacts exist without their database",
            ));
        }
        let conn = Connection::open_in_memory()?;
        schema::create_fresh_schema(&conn)?;
        conn.execute(
            "INSERT INTO ledger_metadata(singleton,ledger_id,era,sequence,security_token) VALUES(1,?1,?2,0,?3)",
            params![pending.authority.ledger_id, pending.authority.era, random_id()?],
        )?;
        let image = conn.serialize("main")?;
        fs.create_only(&fs.names().db, &image)?;
    }
    hook(InitPhase::Database)?;
    let mut bound = fs.open_existing_sqlite(READ_WRITE)?;
    let initial_state = read_database_state(bound.connection())?;
    let checkpoint = Checkpoint {
        version: 2,
        ledger_id: pending.authority.ledger_id.clone(),
        era: pending.authority.era.clone(),
        seq: 0,
        security_token: initial_state.security_token,
    };
    let result = (|| {
        validate_bound_connection(bound.connection(), &pending.authority, &checkpoint)?;
        let rows: i64 = bound.connection().query_row(
            "SELECT (SELECT count(*) FROM batch_requests)+(SELECT count(*) FROM cost_ledger)",
            [],
            |r| r.get(0),
        )?;
        if rows != 0 {
            return Err(contract(
                "KIO-E-LEDGER-INIT-PARTIAL-001",
                "initialization cannot adopt a ledger with billing history",
            ));
        }
        bound
            .connection()
            .execute_batch("PRAGMA synchronous=FULL;")?;
        let mode: String = bound
            .connection()
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        if mode != "wal" {
            return Err(contract(
                "KIO-E-LEDGER-JOURNAL-001",
                "ledger requires WAL journaling",
            ));
        }
        let busy: i64 =
            bound
                .connection()
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
        if busy != 0 {
            return Err(PipelineError::locked("initial ledger WAL checkpoint"));
        }
        bound.revalidate_open()
    })();
    let close = bound.finish();
    result.and(close)?;
    fs.sync_parent()?;
    hook(InitPhase::Configured)?;
    create_record_or_match(fs, &fs.names().checkpoint, &checkpoint)?;
    hook(InitPhase::Checkpoint)?;
    create_record_or_match(fs, &fs.names().authority, &pending.authority)?;
    hook(InitPhase::Authority)?;
    fs.remove_exact(&fs.names().init_pending, bytes, MAX_RECORD)?;
    fs.sync_parent()
}

fn create_record_or_match<T: Serialize + DeserializeOwned + PartialEq>(
    fs: &FsSession,
    leaf: &Path,
    expected: &T,
) -> Result<()> {
    if let Some(bytes) = fs.read_optional_private(leaf, MAX_RECORD)? {
        if decode::<T>(&bytes)? != *expected {
            return Err(contract(
                "KIO-E-LEDGER-INIT-PARTIAL-001",
                "existing initialization record belongs to different state",
            ));
        }
        Ok(())
    } else {
        fs.create_only(leaf, &encode(expected)?)
    }
}
fn read_backup_proof(fs: &FsSession) -> Result<BackupProof> {
    let bytes = fs
        .read_private(&fs.names().backup_proof, MAX_RECORD)
        .map_err(|_| {
            contract(
                "KIO-E-LEDGER-RESTORE-EVIDENCE-001",
                "private backup proof is missing or invalid",
            )
        })?;
    decode(&bytes).map_err(|_| {
        contract(
            "KIO-E-LEDGER-RESTORE-EVIDENCE-001",
            "private backup proof is missing or invalid",
        )
    })
}
fn read_authority(fs: &FsSession) -> Result<(Authority, Checkpoint)> {
    match fs.artifact_state()? {
        ArtifactSetState::Complete => {}
        ArtifactSetState::Missing => {
            return Err(contract(
                "KIO-E-LEDGER-UNINITIALIZED-001",
                "ledger is not initialized",
            ));
        }
        ArtifactSetState::Partial { .. } => {
            return Err(contract(
                "KIO-E-LEDGER-INIT-PARTIAL-001",
                "ledger artifacts are incomplete; explicit recovery is required",
            ));
        }
    }
    let authority: Authority = decode(&fs.read_private(&fs.names().authority, MAX_RECORD)?)?;
    let checkpoint: Checkpoint = decode(&fs.read_private(&fs.names().checkpoint, MAX_RECORD)?)?;
    validate_authority(&authority)?;
    if checkpoint.version != 2
        || checkpoint.ledger_id != authority.ledger_id
        || checkpoint.era != authority.era
        || checkpoint.seq > i64::MAX as u64
        || !valid_id(&checkpoint.security_token)
    {
        return Err(contract(
            "KIO-E-LEDGER-CHECKPOINT-IDENTITY-001",
            "checkpoint does not bind this authority or has invalid sequence",
        ));
    }
    Ok((authority, checkpoint))
}

/// Read the stable authority records while deliberately admitting the one
/// declared write journal. All other partial lifecycle state remains blocked.
fn read_authority_for_recovery(fs: &FsSession) -> Result<(Authority, Checkpoint, Vec<u8>)> {
    if !fs.contains_leaf(&fs.names().db)?
        || !fs.contains_leaf(&fs.names().authority)?
        || !fs.contains_leaf(&fs.names().checkpoint)?
        || fs.contains_leaf(&fs.names().init_pending)?
        || fs.contains_leaf(&fs.names().restore_pending)?
    {
        return Err(recovery_invalid());
    }
    let authority: Authority = decode(&fs.read_private(&fs.names().authority, MAX_RECORD)?)
        .map_err(|_| recovery_invalid())?;
    let checkpoint_bytes = fs.read_private(&fs.names().checkpoint, MAX_RECORD)?;
    let checkpoint: Checkpoint = decode(&checkpoint_bytes).map_err(|_| recovery_invalid())?;
    validate_authority(&authority).map_err(|_| recovery_invalid())?;
    if checkpoint.version != 2
        || checkpoint.ledger_id != authority.ledger_id
        || checkpoint.era != authority.era
        || checkpoint.seq > i64::MAX as u64
        || !valid_id(&checkpoint.security_token)
    {
        return Err(recovery_invalid());
    }
    Ok((authority, checkpoint, checkpoint_bytes))
}

fn checkpoint_from_state(authority: &Authority, state: &WriteState) -> Checkpoint {
    Checkpoint {
        version: 2,
        ledger_id: authority.ledger_id.clone(),
        era: authority.era.clone(),
        seq: state.seq,
        security_token: state.security_token.clone(),
    }
}

fn read_database_state(conn: &Connection) -> Result<WriteState> {
    schema::validate_schema(conn)?;
    conn.query_row(
        "SELECT sequence, security_token FROM ledger_metadata WHERE singleton=1",
        [],
        |row| {
            let sequence: i64 = row.get(0)?;
            let security_token: String = row.get(1)?;
            Ok(WriteState {
                seq: u64::try_from(sequence).unwrap_or(u64::MAX),
                security_token,
            })
        },
    )
    .map_err(Into::into)
}

fn validate_database_authority(conn: &Connection, authority: &Authority) -> Result<()> {
    let (ledger_id, era): (String, String) = conn.query_row(
        "SELECT ledger_id, era FROM ledger_metadata WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if ledger_id != authority.ledger_id || era != authority.era {
        return Err(recovery_invalid());
    }
    Ok(())
}

fn validate_pending(pending: &PendingWrite, authority: &Authority) -> Result<()> {
    if pending.version != 1
        || pending.schema_version != 2
        || pending.ledger_id != authority.ledger_id
        || pending.era != authority.era
        || !valid_id(&pending.transaction_nonce)
        || pending.before.seq > i64::MAX as u64
        || pending.after.seq != pending.before.seq.saturating_add(1)
        || pending.after.seq > i64::MAX as u64
        || !valid_id(&pending.before.security_token)
        || !valid_id(&pending.after.security_token)
        || pending.before.security_token == pending.after.security_token
    {
        return Err(recovery_invalid());
    }
    Ok(())
}

fn recovery_invalid() -> PipelineError {
    contract(
        "KIO-E-LEDGER-WRITE-RECOVERY-001",
        "pending ledger write is missing, malformed, foreign, or does not name an exact recoverable state",
    )
}
/// Validate a private snapshot against lifecycle artifacts through the parent
/// capability the snapshot reader already bound. The supplied path is
/// diagnostic only and is never reopened.
pub(crate) fn validate_read_snapshot_from_retained_parent(
    parent: &std::fs::File,
    parent_path: &Path,
    main: &str,
    conn: &Connection,
) -> Result<()> {
    let fs = FsSession::acquire_retained_snapshot_parent(parent, parent_path, main)?;
    let (authority, checkpoint) = read_authority(&fs)?;
    validate_bound_connection(conn, &authority, &checkpoint)
}

/// Inspect lifecycle artifacts through a parent already retained by the
/// snapshot reader. This is the all-absent counterpart to retained snapshot
/// validation and cannot re-import a mutable `/dev/fd/N` diagnostic spelling.
pub(crate) fn snapshot_artifact_state_from_retained_parent(
    parent: &std::fs::File,
    parent_path: &Path,
    main: &str,
) -> Result<ArtifactSetState> {
    match FsSession::acquire_retained_snapshot_parent(parent, parent_path, main) {
        Ok(fs) => fs.artifact_state(),
        Err(PipelineError::Contract {
            code: "KIO-E-LEDGER-UNINITIALIZED-001",
            ..
        }) => Ok(ArtifactSetState::Missing),
        Err(error) => Err(error),
    }
}
fn validate_bound_connection(
    conn: &Connection,
    authority: &Authority,
    checkpoint: &Checkpoint,
) -> Result<()> {
    schema::validate_schema(conn)?;
    let metadata: (String, String, i64, String) = conn.query_row(
        "SELECT ledger_id, era, sequence, security_token FROM ledger_metadata WHERE singleton=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    if metadata.0 != authority.ledger_id || metadata.1 != authority.era {
        return Err(contract(
            "KIO-E-LEDGER-AUTHORITY-MISMATCH-001",
            "authority and database metadata differ",
        ));
    }
    if u64::try_from(metadata.2).ok() != Some(checkpoint.seq) {
        return Err(contract(
            "KIO-E-LEDGER-SEQUENCE-MISMATCH-001",
            "database and checkpoint sequence differ; paid work is blocked",
        ));
    }
    if metadata.3 != checkpoint.security_token {
        return Err(contract(
            "KIO-E-LEDGER-SECURITY-STATE-MISMATCH-001",
            "database and checkpoint security state differ; paid work is blocked",
        ));
    }
    Ok(())
}
fn validate_authority(value: &Authority) -> Result<()> {
    if value.version != 1 || !valid_id(&value.ledger_id) || !valid_id(&value.era) {
        return Err(contract(
            "KIO-E-LEDGER-AUTHORITY-INVALID-001",
            "ledger authority id, era or version is invalid",
        ));
    }
    Ok(())
}
fn valid_id(value: &str) -> bool {
    value.len() == 32
        && value.bytes().any(|b| b != b'0')
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn random_id() -> Result<String> {
    let mut bytes = [0_u8; 16];
    loop {
        fill(&mut bytes).map_err(|e| contract("KIO-E-LEDGER-RANDOM-001", e.to_string()))?;
        if bytes.iter().any(|v| *v != 0) {
            break;
        }
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn encode(value: &impl Serialize) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|e| contract("KIO-E-LEDGER-JSON-001", e.to_string()))
}
fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|e| contract("KIO-E-LEDGER-JSON-001", e.to_string()))
}
fn contract(code: &'static str, message: impl Into<String>) -> PipelineError {
    PipelineError::contract(code, message)
}

fn durability_checkpoint(point: DurabilityPoint) -> Result<()> {
    checkpoint(point).map_err(|error| {
        contract(
            "KIO-E-LEDGER-DURABILITY-001",
            format!("durability checkpoint failed: {error}"),
        )
    })
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;

#[cfg(all(test, windows))]
mod windows_sidecar_tests {
    use super::*;
    use kio_core::store_dir::{Publication, StoreDirectory};
    use std::fs;

    #[test]
    fn interrupted_first_sidecar_publication_requires_exact_init_resume() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp
            .path()
            .canonicalize()
            .unwrap()
            .join("device/cost-ledger.sqlite");
        let parent_path = path.parent().unwrap();
        let wal_leaf = Path::new("cost-ledger.sqlite-wal");
        let shm_path = parent_path.join("cost-ledger.sqlite-shm");
        let error = LedgerDb::initialize_with_hook(&path, |phase| {
            if phase == InitPhase::Database {
                // Simulate the crash state after the first create-only sidecar
                // publication, before SQLite opens or SHM is published. The
                // initialization session still owns its lifecycle lock here.
                let parent = StoreDirectory::open(parent_path).unwrap();
                parent
                    .write_atomic(wal_leaf, b"", Publication::CreateOnly)
                    .unwrap();
                return Err(contract("TEST-INTERRUPTED", "first sidecar published"));
            }
            Ok(())
        })
        .unwrap_err();
        assert!(matches!(
            error,
            PipelineError::Contract {
                code: "TEST-INTERRUPTED",
                ..
            }
        ));
        let pending_path = parent_path.join("cost-ledger.sqlite.init.pending");
        let pending_bytes = fs::read(&pending_path).unwrap();
        let pending: InitPending = decode(&pending_bytes).unwrap();
        let wal_path = parent_path.join(wal_leaf);
        assert_eq!(fs::read(&wal_path).unwrap(), b"");
        assert!(!shm_path.exists());
        assert!(LedgerDb::open_existing(&path).is_err());
        assert!(LedgerDb::initialize(&path).is_err());
        assert_eq!(fs::read(&pending_path).unwrap(), pending_bytes);
        assert_eq!(fs::read(&wal_path).unwrap(), b"");
        assert!(!shm_path.exists());

        let resumed = LedgerDb::resume_initialization(&path).unwrap();
        assert_eq!(resumed.authority, pending.authority);
        assert_eq!(resumed.integrity_check().unwrap(), "ok");
        let rows: i64 = resumed
            .read(|connection| {
                Ok(connection.query_row(
                    "SELECT (SELECT count(*) FROM batch_requests)+(SELECT count(*) FROM cost_ledger)",
                    [],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(rows, 0);
        assert!(!pending_path.exists());
    }
}
