//! The §5.8/§5.4 state machine: phase 1-3, idempotent recording, outcome
//! validation, crash recovery, sync degenerate 2-phase, the query-embedding
//! device row, budget cap check-then-reserve, and abandon.
//!
//! Section markers below (`§A`.."§K") mirror `tasks/step4b-contract-tests-ledger.md`'s
//! own letter groups, so a CL number in a test name or comment can be traced back
//! here directly.
//!
//! No `07-adapter-spec.md` Batch trait exists in this codebase yet (`kio-adapter`
//! has zero references to upload/job/list_uploads — confirmed by grep before
//! writing this module), so the actual provider upload/job-create/list-jobs calls
//! this state machine drives are represented here only as the *data* a caller
//! would have obtained from them (a discovered job id, a classification of
//! found/confirmed-absent/unknown, a Retry-After value, …). Each function's doc
//! comment says which phase of the real protocol it corresponds to, so wiring in
//! a real Adapter Batch trait later is a matter of calling these in the right
//! order with real values, not redesigning the state machine.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use crate::ledger::model::{
    BatchRequestRow, BatchState, CostLedgerRow, Outcome, RequestKind, TaskKey,
};
use crate::ledger::schema::with_savepoint;
use crate::ledger::time::{now_millis, utc_month_of};
use crate::{PipelineError, Result};

// ---------------------------------------------------------------------------
// Shared primitives: row fetch, transaction helpers, UUIDv7, error classification
// ---------------------------------------------------------------------------

/// `BEGIN IMMEDIATE` wrapper (04 §5.4/§5.8: cap check + phase-1 reservation, or
/// any other check-then-act sequence, must share one immediate Tx so a
/// concurrent writer cannot interleave between the read and the write). Distinct
/// from [`with_savepoint`], which nests inside whatever transaction mode (or
/// autocommit) is already active — use this at the outermost call site.
pub(crate) fn get_batch_request(
    conn: &Connection,
    key: &TaskKey,
) -> Result<Option<BatchRequestRow>> {
    conn.query_row(
        "SELECT scope_id, adapter_kind, input_hash, tool_profile_hash, state, request_kind,
                intent_token, upload_id, batch_job_id, provider_scope_id, job_create_started_at,
                stale_after_at, submission_seq, attempts, contract_violation_count, estimated_usd,
                error, completed_at, created_at
         FROM batch_requests
         WHERE scope_id = ?1 AND adapter_kind = ?2 AND input_hash = ?3 AND tool_profile_hash = ?4",
        params![
            key.scope_id,
            key.adapter_kind,
            key.input_hash,
            key.tool_profile_hash
        ],
        row_to_batch_request,
    )
    .optional()
    .map_err(Into::into)
}

/// Rows for one scope whose provider-side residue still needs attribution or
/// cleanup.  This is intentionally a fixed query rather than an exposed
/// connection so purge/reconcile cannot broaden its ledger access.
pub(crate) fn requests_with_intent_for_scope(
    conn: &Connection,
    scope_id: &str,
) -> Result<Vec<BatchRequestRow>> {
    let mut stmt = conn.prepare(
        "SELECT scope_id, adapter_kind, input_hash, tool_profile_hash, state, request_kind,
                intent_token, upload_id, batch_job_id, provider_scope_id, job_create_started_at,
                stale_after_at, submission_seq, attempts, contract_violation_count, estimated_usd,
                error, completed_at, created_at
         FROM batch_requests WHERE scope_id = ?1 AND intent_token IS NOT NULL
         ORDER BY created_at, adapter_kind, input_hash, tool_profile_hash",
    )?;
    let rows = stmt.query_map(params![scope_id], row_to_batch_request)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Current terminal Batch contract violations remain authoritative after cleanup.
/// Historical outcomes and diagnostic error strings are not retry authority.
pub(crate) fn unreset_contract_violations_for_scope_adapter(
    conn: &Connection,
    scope_id: &str,
    adapter_kind: &str,
) -> Result<Vec<BatchRequestRow>> {
    let mut stmt = conn.prepare(
        "SELECT b.scope_id, b.adapter_kind, b.input_hash, b.tool_profile_hash, b.state, b.request_kind,
                b.intent_token, b.upload_id, b.batch_job_id, b.provider_scope_id, b.job_create_started_at,
                b.stale_after_at, b.submission_seq, b.attempts, b.contract_violation_count, b.estimated_usd,
                b.error, b.completed_at, b.created_at
         FROM batch_requests b JOIN cost_ledger c
           ON c.scope_id = b.scope_id AND c.adapter_kind = b.adapter_kind
          AND c.input_hash = b.input_hash AND c.tool_profile_hash = b.tool_profile_hash
          AND c.submission_seq = b.submission_seq
         WHERE b.scope_id = ?1 AND b.adapter_kind = ?2 AND b.request_kind = ?3
           AND b.state = ?4 AND b.contract_violation_count > 0 AND c.outcome = ?5
         ORDER BY b.created_at, b.input_hash, b.tool_profile_hash",
    )?;
    stmt.query_map(
        params![
            scope_id,
            adapter_kind,
            RequestKind::Batch.as_str(),
            BatchState::Terminal.as_i64(),
            Outcome::ContractViolation.as_str()
        ],
        row_to_batch_request,
    )?
    .collect::<rusqlite::Result<Vec<_>>>()
    .map_err(Into::into)
}

fn row_to_batch_request(row: &rusqlite::Row<'_>) -> rusqlite::Result<BatchRequestRow> {
    let state_value = row.get::<_, i64>(4)?;
    let state = BatchState::from_i64(state_value).ok_or_else(|| {
        invalid_ledger_enum(
            4,
            rusqlite::types::Type::Integer,
            "batch_requests.state",
            state_value,
        )
    })?;
    let request_kind_value = row.get::<_, String>(5)?;
    let request_kind = RequestKind::parse(&request_kind_value).ok_or_else(|| {
        invalid_ledger_enum(
            5,
            rusqlite::types::Type::Text,
            "batch_requests.request_kind",
            &request_kind_value,
        )
    })?;

    Ok(BatchRequestRow {
        key: TaskKey::new(
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ),
        state,
        request_kind,
        intent_token: row.get(6)?,
        upload_id: row.get(7)?,
        batch_job_id: row.get(8)?,
        provider_scope_id: row.get(9)?,
        job_create_started_at: row.get(10)?,
        stale_after_at: row.get(11)?,
        submission_seq: row.get(12)?,
        attempts: row.get(13)?,
        contract_violation_count: row.get(14)?,
        estimated_usd: row.get(15)?,
        error: row.get(16)?,
        completed_at: row.get(17)?,
        created_at: row.get(18)?,
    })
}

fn row_to_cost_ledger(row: &rusqlite::Row<'_>) -> rusqlite::Result<CostLedgerRow> {
    let outcome_value = row.get::<_, String>(8)?;
    let outcome = Outcome::parse(&outcome_value).ok_or_else(|| {
        invalid_ledger_enum(
            8,
            rusqlite::types::Type::Text,
            "cost_ledger.outcome",
            &outcome_value,
        )
    })?;

    Ok(CostLedgerRow {
        key: TaskKey::new(
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ),
        submission_seq: row.get(4)?,
        batch_job_id: row.get(5)?,
        usd: row.get(6)?,
        estimated: row.get::<_, i64>(7)? != 0,
        outcome,
        month: row.get(9)?,
        recorded_at: row.get(10)?,
    })
}

/// Persisted enum values are part of the ledger schema's closed set.  If a
/// damaged or bypass-written row contains an unknown value, surface it as a
/// SQLite conversion failure rather than silently inferring a safe-looking
/// state or outcome.
fn invalid_ledger_enum(
    column_index: usize,
    column_type: rusqlite::types::Type,
    column_name: &str,
    value: impl std::fmt::Display,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column_index,
        column_type,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid closed enum value {value} in {column_name}"),
        )),
    )
}

/// All `cost_ledger` rows for one task key, ordered by `submission_seq` — a test
/// / diagnostic convenience, not on any hot path.
pub(crate) fn cost_ledger_rows_for_key(
    conn: &Connection,
    key: &TaskKey,
) -> Result<Vec<CostLedgerRow>> {
    let mut stmt = conn.prepare(
        "SELECT scope_id, adapter_kind, input_hash, tool_profile_hash, submission_seq,
                batch_job_id, usd, estimated, outcome, month, recorded_at
         FROM cost_ledger
         WHERE scope_id = ?1 AND adapter_kind = ?2 AND input_hash = ?3 AND tool_profile_hash = ?4
         ORDER BY submission_seq",
    )?;
    let rows = stmt.query_map(
        params![
            key.scope_id,
            key.adapter_kind,
            key.input_hash,
            key.tool_profile_hash
        ],
        row_to_cost_ledger,
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// A `rusqlite::Error::SqliteFailure` whose extended code is
/// `SQLITE_CONSTRAINT_CHECK` is, by construction, unreachable through this
/// module's own pre-validated write paths — reaching one means a caller bypassed
/// validation (a test double, or an implementation bug). CL69: reclassify it as
/// the durable, non-retryable `KIO-E-STORE-CONSTRAINT-001` implementation error
/// rather than a generic transport error.
fn classify_check_violation(err: rusqlite::Error) -> PipelineError {
    if let rusqlite::Error::SqliteFailure(ref ffi_err, _) = err
        && ffi_err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_CHECK
    {
        return PipelineError::contract(
            "KIO-E-STORE-CONSTRAINT-001",
            format!(
                "cost_ledger/batch_requests CHECK constraint violated — this is an \
                 implementation error (pre-write validation should have prevented it): {err}"
            ),
        );
    }
    PipelineError::Sqlite(err)
}

/// A fresh UUIDv7 (RFC 9562): 48-bit big-endian UTC-ms timestamp, then a version
/// nibble (`0111`), then randomness, with the variant bits (`10`) set in the
/// right position. The random tail is `SHA-256(pid : nanos : thread_id)`, the
/// same no-extra-dependency technique `kio_core::scope::new_ulid` already uses
/// (that function is not reusable here — private, and its 16-byte layout is
/// ULID/Crockford-base32, not UUIDv7 — so the technique, not the code, is
/// shared).
#[must_use]
pub fn new_intent_token() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let millis = now.as_millis() as u64;
    let mut bytes = [0_u8; 16];
    bytes[0] = (millis >> 40) as u8;
    bytes[1] = (millis >> 32) as u8;
    bytes[2] = (millis >> 24) as u8;
    bytes[3] = (millis >> 16) as u8;
    bytes[4] = (millis >> 8) as u8;
    bytes[5] = millis as u8;
    let seed = format!(
        "{}:{}:{:?}",
        std::process::id(),
        now.as_nanos(),
        std::thread::current().id()
    );
    let digest = Sha256::digest(seed.as_bytes());
    bytes[6..16].copy_from_slice(&digest[0..10]);
    bytes[6] = (bytes[6] & 0x0F) | 0x70; // version 7
    bytes[8] = (bytes[8] & 0x3F) | 0x80; // variant 10xxxxxx
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15],
    )
}

/// Whether `token` is a syntactically valid UUIDv7 (version nibble `7`) — used
/// by CL13's "intent_token = valid UUIDv7" assertion.
#[must_use]
pub fn is_uuid_v7(token: &str) -> bool {
    uuid_bytes(token).is_some_and(|bytes| (bytes[6] >> 4) == 7)
}

/// Extract the 48-bit millisecond timestamp UUIDv7 embeds in its first 6 bytes
/// (CL13's "時刻成分を回復期限の起点に使う" — CL36's recovery-deadline basis).
#[must_use]
pub fn uuid_v7_timestamp_millis(token: &str) -> Option<i64> {
    let bytes = uuid_bytes(token)?;
    Some(
        (i64::from(bytes[0]) << 40)
            | (i64::from(bytes[1]) << 32)
            | (i64::from(bytes[2]) << 24)
            | (i64::from(bytes[3]) << 16)
            | (i64::from(bytes[4]) << 8)
            | i64::from(bytes[5]),
    )
}

fn uuid_bytes(token: &str) -> Option<[u8; 16]> {
    let hex: String = token.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return None;
    }
    let mut bytes = [0_u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(bytes)
}

/// CL15: `submission_seq`'s MAX+1 basis is "the greater of `cost_ledger`'s MAX
/// for this key and the row's own current value" (the row's stored value already
/// inherits from `cost_ledger`'s MAX at its own creation time — §5.4 DDL comment
/// — but `cost_ledger` may have grown since, e.g. via an estimate settlement that
/// bumped the row's seq without this basis ever being re-read).
fn next_submission_seq(conn: &Connection, key: &TaskKey, current_row_seq: i64) -> Result<i64> {
    let ledger_max: Option<i64> = conn.query_row(
        "SELECT MAX(submission_seq) FROM cost_ledger
         WHERE scope_id = ?1 AND adapter_kind = ?2 AND input_hash = ?3 AND tool_profile_hash = ?4",
        params![
            key.scope_id,
            key.adapter_kind,
            key.input_hash,
            key.tool_profile_hash
        ],
        |row| row.get(0),
    )?;
    Ok(ledger_max.unwrap_or(0).max(current_row_seq) + 1)
}

// ---------------------------------------------------------------------------
// §C — Phase 1/2a/2b (CL13-CL17, CL42-CL44)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Phase1Outcome {
    pub intent_token: String,
    pub submission_seq: i64,
}

/// Raw phase-1 SQL, called only by a lifecycle-owned outer write transaction.
///
/// Enforces the CL21/CL39 ordering norm as a hard invariant rather than trusting
/// caller discipline: refuses to (re)issue phase 1 while the existing row's
/// residue cleanup is still pending (`intent_token IS NOT NULL`).
///
/// `sync_effective_timeout_seconds`: `Some` for `request_kind = sync` rows (the
/// resolved effective `timeout_seconds` used to compute `stale_after_at`, §5.4);
/// `None` for `batch` rows (which have no `stale_after_at` — §5.8's own
/// recovery-deadline mechanism covers them instead).
pub(crate) fn phase1_intent(
    conn: &Connection,
    key: &TaskKey,
    request_kind: RequestKind,
    estimated_usd: f64,
    sync_effective_timeout_seconds: Option<i64>,
) -> Result<Phase1Outcome> {
    guard_unauthorized_unknown_sync_resend(conn, key)?;
    if let Some(existing) = get_batch_request(conn, key)?
        && existing.cleanup_pending()
    {
        return Err(PipelineError::contract(
            "KIO-E-BATCH-CLEANUP-PENDING-001",
            "cannot start a new phase 1 before the previous attempt's residue cleanup \
             (upload/job reconciliation) has completed (04 §5.8 順序規範)",
        ));
    }
    // QA14 (10-operations.md §7.5.2): `phase1_intent` is the SOLE INSERT into
    // `batch_requests` (every other write is a CAS UPDATE/DELETE against an
    // already-existing row), so gating here — rather than at each of its
    // several callers (`check_then_reserve`'s two branches, `device_claim`,
    // `record_free_local_charge`, and the direct `bypass_cap_denial` call in
    // `reserve_or_reuse_task_charge`) — covers every path that could start a
    // brand-new online submission. An already-open row (the "Reused" path in
    // `reserve_or_reuse_task_charge`, which never reaches this function at
    // all) stays allowed — resending an EXISTING intent is not a 新規投入.
    if crate::ledger::schema::restore_reconcile_marker_present(conn)? {
        return Err(PipelineError::contract(
            "KIO-E-BATCH-RESTORE-RECONCILE-001",
            "cost-ledger was restored from a backup; run `kio ledger reconcile` before new \
             online submissions (10-operations.md §7.5.2)",
        ));
    }
    let now = now_millis();
    let existing_seq = get_batch_request(conn, key)?.map_or(0, |row| row.submission_seq);
    let submission_seq = next_submission_seq(conn, key, existing_seq)?;
    let intent_token = new_intent_token();
    let (job_create_started_at, stale_after_at) =
        match (request_kind, sync_effective_timeout_seconds) {
            (RequestKind::Sync, Some(timeout)) => {
                (Some(now), Some(compute_stale_after_at(now, timeout)))
            }
            (RequestKind::Sync, None) => (Some(now), None),
            (RequestKind::Batch, _) => (None, None),
        };
    // Nested savepoints preserve rollback boundaries within the lifecycle-owned
    // outer immediate transaction.
    with_savepoint(conn, "kio_ledger_phase1", || {
        conn.execute(
            "INSERT INTO batch_requests (
                scope_id, adapter_kind, input_hash, tool_profile_hash,
                state, request_kind, intent_token, upload_id, batch_job_id,
                provider_scope_id, job_create_started_at, stale_after_at,
                submission_seq, attempts, contract_violation_count, estimated_usd,
                error, completed_at, created_at
            ) VALUES (
                ?1, ?2, ?3, ?4,
                0, ?5, ?6, NULL, NULL,
                NULL, ?7, ?8,
                ?9, 0, 0, ?10,
                NULL, NULL, ?11
            )
            ON CONFLICT (scope_id, adapter_kind, input_hash, tool_profile_hash) DO UPDATE SET
                state = 0,
                request_kind = excluded.request_kind,
                intent_token = excluded.intent_token,
                upload_id = NULL,
                batch_job_id = NULL,
                provider_scope_id = NULL,
                job_create_started_at = excluded.job_create_started_at,
                stale_after_at = excluded.stale_after_at,
                submission_seq = excluded.submission_seq,
                estimated_usd = excluded.estimated_usd,
                error = NULL,
                completed_at = NULL",
            params![
                key.scope_id,
                key.adapter_kind,
                key.input_hash,
                key.tool_profile_hash,
                request_kind.as_str(),
                intent_token,
                job_create_started_at,
                stale_after_at,
                submission_seq,
                estimated_usd,
                now,
            ],
        )
        .map_err(classify_check_violation)?;
        Ok(())
    })?;
    Ok(Phase1Outcome {
        intent_token,
        submission_seq,
    })
}

/// Refuse a task-scoped retry after a sync result was settled as unknown until
/// an operator has explicitly authorized one resend. Device query rows are
/// intentionally excluded: their stale-claim recovery is a separate,
/// user-initiated query path.
pub(crate) fn guard_unauthorized_unknown_sync_resend(
    conn: &Connection,
    key: &TaskKey,
) -> Result<()> {
    if key.is_device() {
        return Ok(());
    }
    let Some(existing) = get_batch_request(conn, key)? else {
        return Ok(());
    };
    if existing.request_kind == RequestKind::Sync
        && existing.state == BatchState::Terminal
        && existing.error.as_deref() == Some("unknown_settled")
    {
        return Err(unknown_sync_resend_error(
            "the previous sync result is unknown and may already have been billed; run \
             `kio batch retry --resend-unknown ... --yes` before another submission",
        ));
    }
    Ok(())
}

/// Approve exactly one fresh task attempt after conservative unknown-result
/// settlement. The transition is intentionally limited to a fully cleaned
/// terminal sync row; the authorized next attempt may use the task's normal
/// sync or batch lane. It does not write `cost_ledger`, totals, or sequence.
/// A repeated approval observes the durable marker and is idempotent.
pub(crate) fn authorize_unknown_sync_resend(conn: &Connection, key: &TaskKey) -> Result<bool> {
    if key.is_device() {
        return Err(unknown_sync_resend_error(
            "device query rows do not support unknown-result resend authorization",
        ));
    }
    let Some(existing) = get_batch_request(conn, key)? else {
        return Err(unknown_sync_resend_error(
            "no settled-unknown sync attempt exists for this task",
        ));
    };
    if existing.request_kind != RequestKind::Sync
        || existing.state != BatchState::Terminal
        || existing.intent_token.is_some()
    {
        return Err(unknown_sync_resend_error(
            "only a cleaned terminal sync attempt can be authorized for resend",
        ));
    }
    match existing.error.as_deref() {
        Some("unknown_resend_authorized") => Ok(false),
        Some("unknown_settled") => {
            let changed = conn.execute(
                "UPDATE batch_requests SET error = 'unknown_resend_authorized'
                 WHERE scope_id = ?1 AND adapter_kind = ?2 AND input_hash = ?3 AND tool_profile_hash = ?4
                   AND request_kind = 'sync' AND state = 3 AND intent_token IS NULL
                   AND error = 'unknown_settled'",
                params![
                    key.scope_id,
                    key.adapter_kind,
                    key.input_hash,
                    key.tool_profile_hash
                ],
            )?;
            if changed == 1 {
                Ok(true)
            } else {
                Err(unknown_sync_resend_error(
                    "the settled-unknown sync attempt changed before authorization",
                ))
            }
        }
        _ => Err(unknown_sync_resend_error(
            "only a settled-unknown sync attempt can be authorized for resend",
        )),
    }
}

fn unknown_sync_resend_error(message: impl Into<String>) -> PipelineError {
    PipelineError::contract("KIO-E-SYNC-RESULT-UNKNOWN-001", message)
}

/// CL49's formula, exactly as 04 §5.4 states it: `max(effective_timeout_seconds
/// plus 60, 600)` seconds.
///
/// This deliberately deviates from CL49's own first worked example (see
/// `tasks/step4b-contract-tests-ledger.md` §H CL49). That example computes
/// `max(300, 120) + 60 = 360` as the final offset for `effective_timeout=300`,
/// never applying the 600s floor even though 360 is less than 600 — while its
/// own second example, in the same paragraph, applies the floor for the
/// analogous 160-less-than-600 case. The floor rule is unconditional in the
/// governing spec text (04 §5.4: "...plus a 60 second margin, floored at 600
/// seconds") and both examples share one paragraph, so the first example's
/// arithmetic is treated as a slip in the contract doc rather than a real
/// branch to implement, and is flagged as such in the implementation report.
#[must_use]
pub fn compute_stale_after_at(now_ms: i64, effective_timeout_seconds: i64) -> i64 {
    let margin_seconds = (effective_timeout_seconds + 60).max(600);
    now_ms + margin_seconds * 1000
}

/// CL16 — phase 2a, upload: `provider_scope_id` immediately before the upload
/// call (so a crash mid-upload still leaves the scope recorded), `upload_id`
/// immediately after a successful upload (so a subsequent job-create failure
/// does not lose the handle). Both CAS-guarded on `intent_token` so a superseded
/// attempt (a fresh phase 1 already issued a new token) cannot write into a
/// newer generation's row.
pub(crate) fn phase2a_record_provider_scope(
    conn: &Connection,
    key: &TaskKey,
    intent_token: &str,
    provider_scope_id: &str,
) -> Result<bool> {
    cas_update_one(
        conn,
        "UPDATE batch_requests SET provider_scope_id = ?1
         WHERE scope_id = ?2 AND adapter_kind = ?3 AND input_hash = ?4 AND tool_profile_hash = ?5
           AND intent_token = ?6",
        params![
            provider_scope_id,
            key.scope_id,
            key.adapter_kind,
            key.input_hash,
            key.tool_profile_hash,
            intent_token
        ],
    )
}

pub(crate) fn phase2a_record_upload_id(
    conn: &Connection,
    key: &TaskKey,
    intent_token: &str,
    upload_id: &str,
) -> Result<bool> {
    cas_update_one(
        conn,
        "UPDATE batch_requests SET upload_id = ?1
         WHERE scope_id = ?2 AND adapter_kind = ?3 AND input_hash = ?4 AND tool_profile_hash = ?5
           AND intent_token = ?6",
        params![
            upload_id,
            key.scope_id,
            key.adapter_kind,
            key.input_hash,
            key.tool_profile_hash,
            intent_token
        ],
    )
}

/// CL17(b) / R23-19 (04 §5.8 手順 2: "現 instance の scope が記録値と一致しない
/// 場合は呼び出さず、旧 upload を掃除して相 2a からやり直す"): when the current
/// client instance's own provider scope does not match the row's recorded
/// `provider_scope_id`, phase 2b must not call job creation — this clears the
/// stale upload handle so phase 2a restarts cleanly (a new `provider_scope_id`
/// gets recorded before a fresh upload), but ONLY once the caller has confirmed
/// the OLD upload (in the OLD, recorded scope — `row.upload_id`/
/// `row.provider_scope_id`, read before calling this) is actually gone
/// (deletion succeeded, or 404). Clearing the locator columns FIRST would
/// destroy the only discovery key a later cleanup pass has for that upload —
/// the same residue-tracking invariant [`recovery_finish_cleanup`]'s own doc
/// comment states ("once its upload(s) are confirmed deleted"). R23-19: prior
/// to this fix the clear was unconditional, with no way for a caller to even
/// express "not yet confirmed" — `old_upload_deletion_confirmed = false` is a
/// no-op (`Ok(false)`, locators left intact) so the caller must retry deletion
/// before calling this again.
pub(crate) fn phase2a_restart_after_scope_mismatch(
    conn: &Connection,
    key: &TaskKey,
    intent_token: &str,
    old_upload_deletion_confirmed: bool,
) -> Result<bool> {
    if !old_upload_deletion_confirmed {
        return Ok(false);
    }
    cas_update_one(
        conn,
        "UPDATE batch_requests SET upload_id = NULL, provider_scope_id = NULL
         WHERE scope_id = ?1 AND adapter_kind = ?2 AND input_hash = ?3 AND tool_profile_hash = ?4
           AND intent_token = ?5 AND request_kind = 'batch' AND state = 0
           AND upload_id IS NOT NULL AND job_create_started_at IS NULL AND batch_job_id IS NULL",
        params![
            key.scope_id,
            key.adapter_kind,
            key.input_hash,
            key.tool_profile_hash,
            intent_token
        ],
    )
}

#[must_use]
pub fn phase2b_scope_matches(row: &BatchRequestRow, current_provider_scope_id: &str) -> bool {
    row.provider_scope_id.as_deref() == Some(current_provider_scope_id)
}

/// CL17(a) — phase 2b, step 1: `job_create_started_at` recorded *immediately
/// before* the job-create call, as its own durable write. Must be called while
/// `conn` is in autocommit mode (no ambient `BEGIN`) so the `UPDATE` commits on
/// its own the instant this returns — bundling it into a larger Tx with the
/// job-create call itself would defeat the point (the record would not be
/// durable until the *call* also finished, exactly the crash window this
/// exists to close).
pub(crate) fn phase2b_record_job_create_started(
    conn: &Connection,
    key: &TaskKey,
    intent_token: &str,
) -> Result<bool> {
    let now = now_millis();
    cas_update_one(
        conn,
        "UPDATE batch_requests SET job_create_started_at = ?1
         WHERE scope_id = ?2 AND adapter_kind = ?3 AND input_hash = ?4 AND tool_profile_hash = ?5
           AND intent_token = ?6",
        params![
            now,
            key.scope_id,
            key.adapter_kind,
            key.input_hash,
            key.tool_profile_hash,
            intent_token
        ],
    )
}

/// CL17(a) — phase 2b, step 2: after the job-create call succeeds.
pub(crate) fn phase2b_record_job_created(
    conn: &Connection,
    key: &TaskKey,
    intent_token: &str,
    batch_job_id: &str,
) -> Result<bool> {
    cas_update_one(
        conn,
        "UPDATE batch_requests SET batch_job_id = ?1, state = 1
         WHERE scope_id = ?2 AND adapter_kind = ?3 AND input_hash = ?4 AND tool_profile_hash = ?5
           AND intent_token = ?6",
        params![
            batch_job_id,
            key.scope_id,
            key.adapter_kind,
            key.input_hash,
            key.tool_profile_hash,
            intent_token
        ],
    )
}

/// CL43 — the sync-row equivalent of phase 2b's job-id recording: the provider
/// request id is durably recorded immediately on response receipt, strictly
/// before the terminal Tx (so a crash between response and terminal recording
/// leaves a `batch_job_id` a later recovery pass can query against).
pub(crate) fn sync_record_provider_request_id(
    conn: &Connection,
    key: &TaskKey,
    intent_token: &str,
    provider_request_id: &str,
) -> Result<bool> {
    cas_update_one(
        conn,
        "UPDATE batch_requests SET batch_job_id = ?1
         WHERE scope_id = ?2 AND adapter_kind = ?3 AND input_hash = ?4 AND tool_profile_hash = ?5
           AND intent_token = ?6",
        params![
            provider_request_id,
            key.scope_id,
            key.adapter_kind,
            key.input_hash,
            key.tool_profile_hash,
            intent_token
        ],
    )
}

/// QA14's 3rd (and lowest-common-denominator) write-seq bump site: every
/// standalone CAS UPDATE/DELETE in this module (`phase2a_record_provider_scope`,
/// `phase2a_record_upload_id`, `phase2a_restart_after_scope_mismatch`,
/// `phase2b_record_job_create_started`, `phase2b_record_job_created`,
/// `sync_record_provider_request_id`, `recovery_mark_found`,
/// `recovery_finish_cleanup`, `reset_contract_violations`,
/// `device_extend_stale_after`'s UPDATE, and `execute_bounded_sweep`'s prune
/// DELETE — all refactored onto this one shared helper) funnels through here.
/// These are documented (`phase2b_record_job_create_started`'s doc comment,
/// among others) to commit the instant they return, with no ambient `BEGIN`
/// — a bare `conn.execute` and a THEN-issued bump would be two separate
/// autocommit statements (a crash between them loses the bump), so the
/// UPDATE/DELETE and the conditional bump are wrapped in one SAVEPOINT here,
/// preserving "commits the instant this returns" while making the two
/// atomic with each other. The bump is conditional on `changed` — a 0-row
/// CAS miss (a lost claim) is a common, expected, genuinely-no-op outcome
/// that must not be counted as a mutation.
fn cas_update_one(conn: &Connection, sql: &str, params: impl rusqlite::Params) -> Result<bool> {
    with_savepoint(conn, "kio_ledger_cas_update", || {
        Ok(conn.execute(sql, params)? > 0)
    })
}

// ---------------------------------------------------------------------------
// §E — outcome pre-validation (CL26-CL31)
// ---------------------------------------------------------------------------

/// The closed `billable_units[].kind` enum (07-adapter-spec.md §4).
pub const BILLABLE_UNIT_KINDS: &[&str] = &["pages", "tokens_in", "tokens_out"];

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BilledAmount {
    pub usd: f64,
    pub estimated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReportedBillableUnit {
    pub kind: String,
    /// `f64` (not `u64`) deliberately — CL28(e)'s non-integer-count case must be
    /// representable as an invalid *input*, not be impossible to construct.
    pub count: f64,
}

/// CL29: the confirmed-nonbillable path (unit-priced-at-zero local LLM, or a
/// provider that declares `reject_billing = "nonbillable"`) — a real `usd=0`
/// charge, never the CL27/CL28 "invalid → estimated" degradation.
#[must_use]
pub fn nonbillable_charge() -> BilledAmount {
    BilledAmount {
        usd: 0.0,
        estimated: false,
    }
}

/// CL27: validate a directly-reported `usd` field (the `submit_rejected`
/// shape — 04 §5.4 DDL comment: "usd = 宣言請求額"). Finite and non-negative →
/// billed at that value; otherwise the conservative `fallback_estimated_usd`
/// (the row's `estimated_usd` reservation) is billed instead, `estimated=1`.
#[must_use]
pub fn resolve_billing_from_usd_field(
    reported_usd: f64,
    fallback_estimated_usd: f64,
) -> BilledAmount {
    if reported_usd.is_finite() && reported_usd >= 0.0 {
        BilledAmount {
            usd: reported_usd,
            estimated: false,
        }
    } else {
        BilledAmount {
            usd: fallback_estimated_usd,
            estimated: true,
        }
    }
}

/// CL28/CL30: validate a `billable_units[]` report. All of the following must
/// hold, or the whole report degrades to the conservative estimate: non-empty;
/// every `kind` in the closed enum AND in `declared_billable_kinds`; no
/// duplicate `kind`; every `count` a finite non-negative integer; every `kind`
/// has a resolvable unit price in `pricing`. Valid reports bill
/// `sum(count * pricing[kind])`.
#[must_use]
pub fn resolve_billing_from_billable_units(
    units: &[ReportedBillableUnit],
    declared_billable_kinds: &std::collections::BTreeSet<String>,
    pricing: &BTreeMap<String, f64>,
    fallback_estimated_usd: f64,
) -> BilledAmount {
    let degrade = || BilledAmount {
        usd: fallback_estimated_usd,
        estimated: true,
    };
    if units.is_empty() {
        return degrade();
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut total = 0.0_f64;
    for unit in units {
        if !BILLABLE_UNIT_KINDS.contains(&unit.kind.as_str()) {
            return degrade();
        }
        if !declared_billable_kinds.contains(&unit.kind) {
            return degrade();
        }
        if !seen.insert(unit.kind.clone()) {
            return degrade();
        }
        if !unit.count.is_finite() || unit.count < 0.0 || unit.count.fract() != 0.0 {
            return degrade();
        }
        let Some(price) = pricing.get(&unit.kind).copied() else {
            return degrade();
        };
        total += unit.count * price;
    }
    BilledAmount {
        usd: total,
        estimated: false,
    }
}

/// QA17/QA18/QA19 (step4b-contract-tests-p3a.md §F): resolve a terminal
/// task's billed amount from the Adapter's self-reported `usage` (07 §4
/// L291-307's one-of `usd`|`billable_units`), the Adapter's declared
/// `billable_kinds` (QA18), and `tools.toml`'s `[pricing]` table (QA19) — the
/// single call site that joins all three contracts. Wraps
/// [`resolve_billing_from_usd_field`]/[`resolve_billing_from_billable_units`]
/// (CL27/CL28's existing degrade rules, unchanged) so a missing or malformed
/// `usage` degrades exactly as it always has — `estimated=1` at
/// `fallback_estimated_usd` (the caller's reservation amount).
///
/// Returns the billed amount plus the distinct `billable_units[].kind`
/// values that were reported but had no `pricing` entry (QA19's "単価未被覆の
/// kind" — the caller logs ONE warning line naming them; this function stays
/// pure/IO-free so it is unit-testable without a logging seam. Note this list
/// can be non-empty even when `billed.estimated` is `false`: e.g. two
/// declared kinds are reported, one priced and one not — CL28's "every kind
/// must have a resolvable price" rule still degrades the WHOLE report, so in
/// practice a non-empty list and `estimated=true` always coincide for this
/// specific defect, but the two are computed independently on purpose so a
/// future partial-billing relaxation would not silently need this wired
/// again).
#[must_use]
pub fn resolve_billing_from_reported_usage(
    usage: Option<&kio_adapter::types::AdapterUsage>,
    declared_billable_kinds: &std::collections::BTreeSet<String>,
    pricing: &BTreeMap<String, f64>,
    fallback_estimated_usd: f64,
) -> (BilledAmount, Vec<String>) {
    use kio_adapter::types::AdapterUsage;
    match usage {
        None => (
            BilledAmount {
                usd: fallback_estimated_usd,
                estimated: true,
            },
            Vec::new(),
        ),
        Some(AdapterUsage::Usd { usd }) => (
            resolve_billing_from_usd_field(*usd, fallback_estimated_usd),
            Vec::new(),
        ),
        Some(AdapterUsage::BillableUnits { billable_units }) => {
            let uncovered: Vec<String> = billable_units
                .iter()
                .map(|unit| billable_unit_kind_name(unit.kind).to_owned())
                .filter(|kind| !pricing.contains_key(kind))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            let reported: Vec<ReportedBillableUnit> = billable_units
                .iter()
                .map(|unit| ReportedBillableUnit {
                    kind: billable_unit_kind_name(unit.kind).to_owned(),
                    count: unit.count as f64,
                })
                .collect();
            let billed = resolve_billing_from_billable_units(
                &reported,
                declared_billable_kinds,
                pricing,
                fallback_estimated_usd,
            );
            (billed, uncovered)
        }
    }
}

/// The [`BILLABLE_UNIT_KINDS`] string for an Adapter-reported
/// `BillableUnitKind` (07 §4's closed enum). `kio-pipeline` depends on
/// `kio-adapter`, so this is the single conversion point between the two
/// crates' representations (the typed enum vs. the ledger's string keys).
fn billable_unit_kind_name(kind: kio_adapter::types::BillableUnitKind) -> &'static str {
    use kio_adapter::types::BillableUnitKind;
    match kind {
        BillableUnitKind::Pages => "pages",
        BillableUnitKind::TokensIn => "tokens_in",
        BillableUnitKind::TokensOut => "tokens_out",
    }
}

// ---------------------------------------------------------------------------
// §D/§C — terminal transaction: idempotent recording + state 2/3 (CL18-CL25)
// ---------------------------------------------------------------------------

pub struct TerminalWrite<'a> {
    pub key: &'a TaskKey,
    pub outcome: Outcome,
    pub billed: BilledAmount,
    /// `cost_ledger.batch_job_id` value (04 §5.4 DDL comment's 3-way rule: real
    /// job id normally; the `intent_token` for a job-id-unknown settlement;
    /// provider request id for sync rows).
    pub ledger_batch_job_id: &'a str,
    pub next_state: BatchState,
    pub error: Option<&'a str>,
    pub increment_contract_violation: bool,
    pub attempts_delta: i64,
    pub clear_intent_token: bool,
    /// `Some(token)` CAS-guards the `batch_requests` update on `intent_token`
    /// (used by the device-row paths, §H, whose writers are not otherwise
    /// serialized by `.kio/.lock`). `None` performs an unconditional update by
    /// key alone — correct for the lock-serialized batch/sync command paths.
    pub intent_token_guard: Option<&'a str>,
    /// CL23/CL24/CL36(b)/CL45: the "job id unknown" recording pattern — bump
    /// `submission_seq` by 1 *before* recording, and use the bumped value both
    /// for the `cost_ledger` row and the `batch_requests` row it leaves behind
    /// (so a later-discovered real completion for the ORIGINAL seq cannot
    /// collide with this estimate).
    pub reseat_submission_seq: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TerminalReceipt {
    /// `false` when `cost_ledger`'s `ON CONFLICT DO NOTHING` absorbed a
    /// duplicate (CL22) or the row/CAS guard did not match (nothing recorded).
    pub recorded: bool,
    pub submission_seq: i64,
    /// Whether the `batch_requests` row was actually updated this call
    /// (`false` on a CAS miss, a missing row, or a no-op replay of an
    /// already-identical terminal state).
    pub row_updated: bool,
}

/// The single write path every §C/§D/§E/§F/§G/§J terminal outcome funnels
/// through: `cost_ledger` idempotent insert + `batch_requests` state 2/3 +
/// `completed_at`, all in one savepoint (04 §5.8 相 3 / §5.4 sync 終端).
pub(crate) fn terminal_transaction(
    conn: &Connection,
    write: &TerminalWrite<'_>,
) -> Result<TerminalReceipt> {
    let receipt = with_savepoint(conn, "kio_ledger_terminal", || {
        let Some(current) = get_batch_request(conn, write.key)? else {
            return Ok(TerminalReceipt {
                recorded: false,
                submission_seq: 0,
                row_updated: false,
            });
        };
        if let Some(guard) = write.intent_token_guard
            && current.intent_token.as_deref() != Some(guard)
        {
            // CL51: claim already lost to another writer — record nothing.
            return Ok(TerminalReceipt {
                recorded: false,
                submission_seq: current.submission_seq,
                row_updated: false,
            });
        }
        // Whether the charge/state transition itself has already landed (a
        // prior call already recorded this outcome at this state — the CL22
        // "crash right after commit, re-run" replay case). Controls whether we
        // bump submission_seq again and whether we re-apply the
        // violation-count/attempts deltas. Deliberately independent of
        // `clear_intent_token`: `execute_abandon`/`recovery_settle_unknown`
        // legitimately call this a second time on an already-terminal row
        // purely to *complete residue cleanup* once it was previously left
        // pending (CL37/CL38) — that call must still clear `intent_token` even
        // though it must not re-charge or re-bump `seq` (CL25: a charge, once
        // recorded, is the attempt's final record).
        let already_recorded = current.state == write.next_state && current.completed_at.is_some();
        let needs_intent_token_clear = write.clear_intent_token && current.intent_token.is_some();
        if already_recorded && !needs_intent_token_clear {
            return Ok(TerminalReceipt {
                recorded: false,
                submission_seq: current.submission_seq,
                row_updated: false,
            });
        }

        let mut seq = current.submission_seq;
        if write.reseat_submission_seq && !already_recorded {
            seq += 1;
            conn.execute(
                "UPDATE batch_requests SET submission_seq = ?1
                 WHERE scope_id = ?2 AND adapter_kind = ?3 AND input_hash = ?4 AND tool_profile_hash = ?5",
                params![
                    seq,
                    write.key.scope_id,
                    write.key.adapter_kind,
                    write.key.input_hash,
                    write.key.tool_profile_hash
                ],
            )?;
        }

        let now = now_millis();
        let month = utc_month_of(now);
        let inserted = conn
            .execute(
                "INSERT INTO cost_ledger (
                    scope_id, adapter_kind, input_hash, tool_profile_hash, submission_seq,
                    batch_job_id, usd, estimated, outcome, month, recorded_at
                ) VALUES (?1,?2,?3,?4,?5, ?6,?7,?8,?9,?10,?11)
                ON CONFLICT (scope_id, adapter_kind, input_hash, tool_profile_hash, submission_seq)
                DO NOTHING",
                params![
                    write.key.scope_id,
                    write.key.adapter_kind,
                    write.key.input_hash,
                    write.key.tool_profile_hash,
                    seq,
                    write.ledger_batch_job_id,
                    write.billed.usd,
                    i64::from(write.billed.estimated),
                    write.outcome.as_str(),
                    month,
                    now,
                ],
            )
            .map_err(classify_check_violation)?;

        // On a cleanup-only re-touch (already_recorded=true, only reachable
        // here because needs_intent_token_clear was also true), apply_deltas
        // is false: state/error/completed_at are re-written (harmless — same
        // values, except `error` may now reflect a newer action like abandon,
        // which is display-only per CL21) but contract_violation_count/
        // attempts are left untouched and intent_token still clears.
        let row_updated = apply_terminal_update(conn, write, now, !already_recorded)?;
        Ok(TerminalReceipt {
            recorded: inserted > 0,
            submission_seq: seq,
            row_updated,
        })
    })?;
    Ok(receipt)
}

fn apply_terminal_update(
    conn: &Connection,
    write: &TerminalWrite<'_>,
    now: i64,
    apply_deltas: bool,
) -> Result<bool> {
    let mut sql = String::from(
        "UPDATE batch_requests SET state = ?1, error = ?2, completed_at = ?3, \
         contract_violation_count = contract_violation_count + ?4, attempts = attempts + ?5",
    );
    if write.clear_intent_token {
        sql.push_str(", intent_token = NULL");
    }
    sql.push_str(
        " WHERE scope_id = ?6 AND adapter_kind = ?7 AND input_hash = ?8 AND tool_profile_hash = ?9",
    );
    let violation_delta = if apply_deltas {
        i64::from(write.increment_contract_violation)
    } else {
        0
    };
    let attempts_delta = if apply_deltas {
        write.attempts_delta
    } else {
        0
    };
    if let Some(guard) = write.intent_token_guard {
        sql.push_str(" AND intent_token = ?10");
        let changed = conn.execute(
            &sql,
            params![
                write.next_state.as_i64(),
                write.error,
                now,
                violation_delta,
                attempts_delta,
                write.key.scope_id,
                write.key.adapter_kind,
                write.key.input_hash,
                write.key.tool_profile_hash,
                guard,
            ],
        )?;
        Ok(changed > 0)
    } else {
        let changed = conn.execute(
            &sql,
            params![
                write.next_state.as_i64(),
                write.error,
                now,
                violation_delta,
                attempts_delta,
                write.key.scope_id,
                write.key.adapter_kind,
                write.key.input_hash,
                write.key.tool_profile_hash,
            ],
        )?;
        Ok(changed > 0)
    }
}

/// CL21: "1 回のみ再試行" — `contract_violation_count <= 1` allows a fresh
/// phase 1; `>= 2` is failed-permanent, independent of `mode`/`error`.
#[must_use]
pub fn contract_violation_retry_allowed(row: &BatchRequestRow) -> bool {
    row.contract_violation_count <= 1
}

// ---------------------------------------------------------------------------
// §F — crash recovery (CL32-CL40)
// ---------------------------------------------------------------------------

/// CL32: the set of `batch` rows a write-command's recovery pass must
/// reconcile — unterminated (`state IN (0,1)`) or terminal-but-uncleaned
/// (`state IN (2,3)` with residual `intent_token`). `request_kind = 'sync'` rows
/// are excluded (handled by §G's own crash-recovery pass, not job/upload
/// matching).
pub(crate) fn recovery_candidates(conn: &Connection) -> Result<Vec<BatchRequestRow>> {
    let mut stmt = conn.prepare(
        "SELECT scope_id, adapter_kind, input_hash, tool_profile_hash, state, request_kind,
                intent_token, upload_id, batch_job_id, provider_scope_id, job_create_started_at,
                stale_after_at, submission_seq, attempts, contract_violation_count, estimated_usd,
                error, completed_at, created_at
         FROM batch_requests
         WHERE request_kind = 'batch'
           AND (state IN (0, 1) OR (state IN (2, 3) AND intent_token IS NOT NULL))
         ORDER BY scope_id, adapter_kind, input_hash, tool_profile_hash",
    )?;
    let rows = stmt.query_map([], row_to_batch_request)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Batch-lane POLL candidates (04 §5.8 相 3 collect; the send-lane wiring's
/// counterpart of [`recovery_candidates`]): one scope + adapter's
/// `request_kind='batch'` rows whose provider job id is already durably known
/// (`batch_job_id IS NOT NULL`) and whose charge is still open
/// (`state IN (0, 1)`). `state = 0` rows are included deliberately — a
/// crash between phase 2b's successful job-create call and its own
/// `phase2b_record_job_created` write is recovered by the reconcile walk's
/// `recovery_mark_found` self-description (04 §5.8 "found ... batch_job_id
/// 未記録なら発見値を行へ書く"), which records the id WITHOUT flipping
/// `state` to 1; excluding those rows would strand a discovered job forever.
/// Rows with `batch_job_id IS NULL` stay owned by the recovery walk's
/// token-matching (found / confirmed-absent / unknown) and are never polled
/// here.
pub(crate) fn batch_poll_candidates(
    conn: &Connection,
    scope_id: &str,
    adapter_kind: &str,
) -> Result<Vec<BatchRequestRow>> {
    let mut stmt = conn.prepare(
        "SELECT scope_id, adapter_kind, input_hash, tool_profile_hash, state, request_kind,
                intent_token, upload_id, batch_job_id, provider_scope_id, job_create_started_at,
                stale_after_at, submission_seq, attempts, contract_violation_count, estimated_usd,
                error, completed_at, created_at
         FROM batch_requests
         WHERE request_kind = 'batch'
           AND state IN (0, 1)
           AND batch_job_id IS NOT NULL
           AND scope_id = ?1
           AND adapter_kind = ?2
         ORDER BY input_hash, tool_profile_hash",
    )?;
    let rows = stmt.query_map(params![scope_id, adapter_kind], row_to_batch_request)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// CL45 (04 §5.4): the `request_kind = 'sync'` counterpart of
/// [`recovery_candidates`] — rows `recovery_candidates` itself explicitly
/// excludes (its own doc comment: "handled by §G's own crash-recovery pass,
/// not job/upload matching"). Scoped to one `scope_id` (a sync row belongs to
/// exactly one scope; only that scope's `.kio/.lock` holder has authority to
/// reconcile it — unlike the device-global bounded sweep of §H, which is a
/// separate mechanism for `scope_id='device'` rows only) and gated on
/// `stale_after_at` (NULL — a row from before `sync_effective_timeout_seconds`
/// was threaded through, or a caller that never supplied one — is always
/// eligible, matching `phase1_intent`'s own lenient default): a row whose
/// `stale_after_at` has not yet elapsed may still be a live, in-flight sync
/// call from a concurrent process and must not be raced.
pub(crate) fn sync_recovery_candidates(
    conn: &Connection,
    scope_id: &str,
    now_ms: i64,
) -> Result<Vec<BatchRequestRow>> {
    let mut stmt = conn.prepare(
        "SELECT scope_id, adapter_kind, input_hash, tool_profile_hash, state, request_kind,
                intent_token, upload_id, batch_job_id, provider_scope_id, job_create_started_at,
                stale_after_at, submission_seq, attempts, contract_violation_count, estimated_usd,
                error, completed_at, created_at
         FROM batch_requests
         WHERE request_kind = 'sync'
           AND state IN (0, 1)
           AND scope_id = ?1
           AND (stale_after_at IS NULL OR stale_after_at <= ?2)
         ORDER BY adapter_kind, input_hash, tool_profile_hash",
    )?;
    let rows = stmt.query_map(params![scope_id, now_ms], row_to_batch_request)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// QA14/QA15 (`kio ledger reconcile`, 10 §7.5.2): every distinct `scope_id`
/// carrying at least one `request_kind='sync'` row — including the reserved
/// `TaskKey::DEVICE_SCOPE_ID` pseudo-scope (query-embedding device rows are
/// `request_kind='sync'` too, §H). [`sync_recovery_candidates`] itself is
/// scoped to one `scope_id` at a time (normally only that scope's
/// `.kio/.lock` holder has authority to reconcile it — that function's own
/// doc comment); `kio ledger reconcile` is a device-global maintenance
/// command with no single owning scope, so its caller sweeps every scope_id
/// this returns in turn.
pub(crate) fn distinct_scope_ids_with_sync_rows(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT scope_id FROM batch_requests WHERE request_kind = 'sync' ORDER BY scope_id",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// CL34 — `found`: self-describe `batch_job_id` if a prior crash left it
/// unrecorded (relevant only when the row was already at `state=1` with
/// `batch_job_id IS NULL` — a crash right after phase 2b's job-create call
/// succeeded but before its own record landed). A no-op (but harmless) call
/// when `batch_job_id` is already set.
pub(crate) fn recovery_mark_found(
    conn: &Connection,
    key: &TaskKey,
    discovered_job_id: &str,
) -> Result<bool> {
    cas_update_one(
        conn,
        "UPDATE batch_requests SET batch_job_id = COALESCE(batch_job_id, ?1)
         WHERE scope_id = ?2 AND adapter_kind = ?3 AND input_hash = ?4 AND tool_profile_hash = ?5",
        params![
            discovered_job_id,
            key.scope_id,
            key.adapter_kind,
            key.input_hash,
            key.tool_profile_hash
        ],
    )
}

/// Default recovery deadline (04 §5.8: "既定 48h", config-overridable — the
/// override itself is a CLI/config concern outside this module).
pub const DEFAULT_RECOVERY_DEADLINE_MS: i64 = 48 * 3_600 * 1_000;

/// CL36: `max(intent_token 時刻, job_create_started_at) + deadline_ms` has
/// elapsed. `false` for a row with no `intent_token` (nothing to recover).
#[must_use]
pub fn recovery_deadline_passed(row: &BatchRequestRow, now_ms: i64, deadline_ms: i64) -> bool {
    let Some(token) = row.intent_token.as_deref() else {
        return false;
    };
    let Some(token_ts) = uuid_v7_timestamp_millis(token) else {
        return false;
    };
    let basis = token_ts.max(row.job_create_started_at.unwrap_or(token_ts));
    now_ms.saturating_sub(basis) >= deadline_ms
}

/// CL35: the default visibility grace period (10 minutes) a `confirmed-absent`
/// classification must additionally satisfy on top of a full-page provider scan.
pub const DEFAULT_VISIBILITY_GRACE_PERIOD_MS: i64 = 10 * 60 * 1_000;

/// R23-05 (04 §5.4 DDL: "`job_create_started_at` INTEGER ... batch 行 = 可視化
/// 猶予・回復期限の起点"): the grace period is measured from the row's own
/// durable `job_create_started_at`, not from the `intent_token`'s embedded
/// UUIDv7 issue time. The two diverge whenever phase 2a (upload) takes long
/// enough that job creation starts well after the token was minted (04 §5.8
/// AUD-13's failure scenario: a 20-minute upload followed by a crash 1 minute
/// into job creation would already read as 21 minutes token-age-stale even
/// though the job itself has been visible-or-not for only 1 minute). A row
/// whose `job_create_started_at` is still `NULL` (phase 2b never started) has
/// no basis to measure from and must never be treated as grace-elapsed — 04
/// §5.8's own confirmed-absent rule already excludes these rows from job-list
/// matching ("相 2b 未着手 (`job_create_started_at` IS NULL) の行は job 一覧
/// 照合の対象にしない — job 不存在は記録から確定している"), and this
/// function's `false` keeps a caller that ignores that gate from mistakenly
/// concluding grace has elapsed on a NULL basis.
#[must_use]
pub fn visibility_grace_period_elapsed(
    job_create_started_at: Option<i64>,
    now_ms: i64,
    grace_ms: i64,
) -> bool {
    job_create_started_at.is_some_and(|started_at| now_ms.saturating_sub(started_at) >= grace_ms)
}

/// CL23/CL24/CL36(b)/CL45(b)(c)/§H's inline sweep/abandon: the "job id unknown"
/// settlement — `submission_seq += 1`, record an estimate keyed by
/// `intent_token`, terminal the row. `cleanup_already_complete` controls whether
/// `intent_token` is cleared in the same Tx (CL36(c)/CL39/note-5: only once the
/// residue — if any could exist — has been confirmed cleaned up; sync/device
/// rows and phase-1-only batch rows are always "already complete" since no
/// upload could exist).
///
/// R23-03 (04 §5.4: "device 行の全ての状態遷移 UPDATE ... は `WHERE
/// intent_token = <自 token>` の条件付き (CAS) で行う — 0 行更新 = 他プロセス
/// に回収済みであり、自プロセスは応答・課金のどちらも記帳しない"): CAS-guarded
/// on the exact `intent_token` this call believes it is settling. Every
/// existing caller already reads that token from the row immediately before
/// calling this (either within the same `BEGIN IMMEDIATE`/savepoint as the
/// read, or — for the device sync callers this fix targets — across a real
/// network call with no such atomicity) — so the guard is a no-op for the
/// former (the token cannot have changed) and closes a real double-settlement
/// race for the latter (a stale-sweep recoverer already reclaimed the row
/// under a NEW token; this call must not settle the reclaimer's newer
/// generation under the OLD token).
pub(crate) fn recovery_settle_unknown(
    conn: &Connection,
    key: &TaskKey,
    intent_token: &str,
    estimated_usd: f64,
    cleanup_already_complete: bool,
) -> Result<TerminalReceipt> {
    terminal_transaction(
        conn,
        &TerminalWrite {
            key,
            outcome: Outcome::UnknownSettled,
            billed: BilledAmount {
                usd: estimated_usd,
                estimated: true,
            },
            ledger_batch_job_id: intent_token,
            next_state: BatchState::Terminal,
            error: Some("unknown_settled"),
            increment_contract_violation: false,
            attempts_delta: 0,
            clear_intent_token: cleanup_already_complete,
            intent_token_guard: Some(intent_token),
            reseat_submission_seq: true,
        },
    )
}

/// CL38: residual-cleanup completion — clears `intent_token` for a terminal row
/// once its upload(s) are confirmed deleted (or never existed). Distinct call
/// from the terminal Tx itself because provider-side deletion cannot
/// participate in the SQLite Tx (04 §5.8: "provider 側削除は SQLite Tx に原子
/// 参加できない").
pub(crate) fn recovery_finish_cleanup(
    conn: &Connection,
    key: &TaskKey,
    intent_token: &str,
) -> Result<bool> {
    cas_update_one(
        conn,
        "UPDATE batch_requests SET intent_token = NULL
         WHERE scope_id = ?1 AND adapter_kind = ?2 AND input_hash = ?3 AND tool_profile_hash = ?4
           AND intent_token = ?5 AND state IN (2, 3)",
        params![
            key.scope_id,
            key.adapter_kind,
            key.input_hash,
            key.tool_profile_hash,
            intent_token
        ],
    )
}

/// CL37/CL68: `kio status`'s "stalled" set — settled (state=3) but residue
/// cleanup still outstanding. Display must include `intent_token` (the only
/// selector that still resolves it — CL62/CL63).
pub(crate) fn stalled_rows(conn: &Connection) -> Result<Vec<BatchRequestRow>> {
    let mut stmt = conn.prepare(
        "SELECT scope_id, adapter_kind, input_hash, tool_profile_hash, state, request_kind,
                intent_token, upload_id, batch_job_id, provider_scope_id, job_create_started_at,
                stale_after_at, submission_seq, attempts, contract_violation_count, estimated_usd,
                error, completed_at, created_at
         FROM batch_requests
         WHERE state = 3 AND intent_token IS NOT NULL
         ORDER BY scope_id, adapter_kind, input_hash, tool_profile_hash",
    )?;
    let rows = stmt.query_map([], row_to_batch_request)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// §H — the query-embedding device row (CL48-CL55)
// ---------------------------------------------------------------------------

/// CL48: `input_hash = sha256(NFC(query))` — the query text itself is never
/// persisted.
#[must_use]
pub fn device_input_hash(query: &str) -> String {
    let normalized: String = query.nfc().collect();
    crate::prepare::hash_bytes(normalized.as_bytes())
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClaimOutcome {
    Claimed(Phase1Outcome),
    /// CL54: a live (non-stale) in-flight claim already exists for this key —
    /// fall back to text search (`fallback_reason = "embedding_in_flight"`),
    /// never issue a second phase 1 for the same key.
    InFlight,
    /// R23-02 (04 §5.4 / AUD-11 / A-14): the device or per_adapter(embedding)
    /// cap would be exceeded — no reservation is made; the caller must fall
    /// back to text search rather than send.
    Denied(CapLayer),
}

/// CL48-CL54 / R23-02: claim a device row for a query-embedding sync request.
/// Must run inside the caller's `BEGIN IMMEDIATE` Tx together with the
/// budget-cap check, same as any other phase 1 (`§I`) — as of R23-02 the cap
/// check is performed INSIDE this function rather than left to the caller, so
/// it cannot be silently skipped: device rows are NOT exempt from the device /
/// per_adapter (embedding) cap, only from folder cap (04 §5.4: "folder cap
/// 判定 (scope 別集計) には現れず、device cap / per_adapter (embedding) の
/// 合算には通常どおり含まれる — 判定式は不変"). If an existing in-flight claim
/// for this exact key is stale, it is swept first (`§53`'s inline rule: never
/// queries the provider, always settles `unknown`) so the fresh claim can
/// proceed immediately rather than being blocked by [`phase1_intent`]'s
/// cleanup guard.
pub(crate) fn device_claim(
    conn: &Connection,
    key: &TaskKey,
    estimated_usd: f64,
    effective_timeout_seconds: i64,
    device_cap: f64,
    device_per_adapter_cap: Option<f64>,
) -> Result<ClaimOutcome> {
    let now = now_millis();
    if let Some(existing) = get_batch_request(conn, key)?
        && existing.state.is_inflight()
    {
        let stale = existing
            .stale_after_at
            .is_some_and(|deadline| deadline <= now);
        if !stale {
            return Ok(ClaimOutcome::InFlight);
        }
        if let Some(token) = existing.intent_token.clone() {
            recovery_settle_unknown(conn, key, &token, existing.estimated_usd, true)?;
        }
    }
    // R23-02: `estimated_usd == 0.0` (a zero-priced local adapter) is exempt
    // from the cap check entirely, mirroring `check_then_reserve`'s own
    // `ExemptZeroCost` rule (04 §5.4: "candidate = 0 のタスク ... は cap 判定
    // の対象外"). Device rows never participate in folder cap (excluded above
    // by construction — this function never reads a folder_cap parameter).
    if estimated_usd != 0.0 {
        let month = utc_month_of(now);
        let device_total = ledger_month_total(conn, None, None, &month)?;
        if device_total + estimated_usd >= device_cap {
            return Ok(ClaimOutcome::Denied(CapLayer::Device));
        }
        if let Some(per_adapter_cap) = device_per_adapter_cap {
            let adapter_total = ledger_month_total(conn, None, Some(&key.adapter_kind), &month)?;
            if adapter_total + estimated_usd >= per_adapter_cap {
                return Ok(ClaimOutcome::Denied(CapLayer::PerAdapter));
            }
        }
    }
    let outcome = phase1_intent(
        conn,
        key,
        RequestKind::Sync,
        estimated_usd,
        Some(effective_timeout_seconds),
    )?;
    Ok(ClaimOutcome::Claimed(outcome))
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExtendOutcome {
    Extended(i64),
    /// CL51: the CAS `UPDATE` matched 0 rows — another process already
    /// recovered this row. The caller must stop waiting/retrying/billing.
    ClaimLost,
}

/// CL50: `stale_after_at := max(current, now + safe(Retry-After) + timeout + 60s)`
/// computed and applied atomically in one `UPDATE` (avoiding a read-then-write
/// race), CAS-guarded on `intent_token`. An invalid `retry_after_seconds`
/// (non-finite or negative) substitutes 3600s; a valid value is never clamped
/// — R23-30 (04 §5.4: "有効な実値は clamp しない"): a fractional
/// `retry_after_seconds` (e.g. `600.5`) rounds UP (`ceil`), never truncates.
/// Truncating would compute a protection deadline strictly SHORTER than the
/// provider's actual requested wait, reopening exactly the double-invocation
/// window this deadline exists to close (a stale-sweep recoverer could then
/// reclaim and re-call the provider while the original holder is still
/// legitimately waiting out its Retry-After).
pub(crate) fn device_extend_stale_after(
    conn: &Connection,
    key: &TaskKey,
    intent_token: &str,
    retry_after_seconds: f64,
    effective_timeout_seconds: i64,
) -> Result<ExtendOutcome> {
    let now = now_millis();
    let safe_retry_after = if retry_after_seconds.is_finite() && retry_after_seconds >= 0.0 {
        retry_after_seconds.ceil() as i64
    } else {
        3600
    };
    let candidate = now + (safe_retry_after + effective_timeout_seconds + 60) * 1000;
    // QA14: routed through `cas_update_one` (rather than a bare `conn.execute`)
    // so this mutation participates in the write-seq bump — the read-back
    // SELECT just below is unaffected (a plain autocommit read of what this
    // call just durably committed).
    let changed = cas_update_one(
        conn,
        "UPDATE batch_requests SET stale_after_at = MAX(COALESCE(stale_after_at, 0), ?1)
         WHERE scope_id = ?2 AND adapter_kind = ?3 AND input_hash = ?4 AND tool_profile_hash = ?5
           AND intent_token = ?6",
        params![
            candidate,
            key.scope_id,
            key.adapter_kind,
            key.input_hash,
            key.tool_profile_hash,
            intent_token
        ],
    )?;
    if !changed {
        return Ok(ExtendOutcome::ClaimLost);
    }
    let new_value: i64 = conn.query_row(
        "SELECT stale_after_at FROM batch_requests
         WHERE scope_id = ?1 AND adapter_kind = ?2 AND input_hash = ?3 AND tool_profile_hash = ?4",
        params![
            key.scope_id,
            key.adapter_kind,
            key.input_hash,
            key.tool_profile_hash
        ],
        |row| row.get(0),
    )?;
    Ok(ExtendOutcome::Extended(new_value))
}

/// CL52's bounded-sweep allocation: `total_cap` total, `prune_min` reserved for
/// pruning when candidates exist, symmetric reallocation of either side's
/// unused reservation to the other. Pure arithmetic — split out so it is
/// independently testable from the SQL candidate queries.
#[must_use]
pub fn allocate_sweep_capacity(
    prune_available: usize,
    general_available: usize,
    total_cap: usize,
    prune_min: usize,
) -> (usize, usize) {
    let prune_reserved = prune_min.min(prune_available);
    let mut prune_take = prune_reserved;
    let mut general_take = general_available.min(total_cap - prune_take);
    let mut remaining = total_cap - prune_take - general_take;
    if remaining > 0 {
        let extra_prune = remaining.min(prune_available - prune_take);
        prune_take += extra_prune;
        remaining -= extra_prune;
    }
    if remaining > 0 {
        let extra_general = remaining.min(general_available - general_take);
        general_take += extra_general;
        remaining -= extra_general;
    }
    let _ = remaining; // any leftover means both pools are exhausted; carries to next run
    (prune_take, general_take)
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SweepPlan {
    /// CL52(1): stale rows sharing `(adapter_kind, input_hash)` with the key
    /// about to be claimed — outside the 256 cap, always fully included.
    pub own_key_stale: Vec<TaskKey>,
    pub general_stale: Vec<TaskKey>,
    pub prune: Vec<TaskKey>,
}

const SWEEP_TOTAL_CAP: usize = 256;
const SWEEP_PRUNE_MIN: usize = 128;
/// Defensive upper bound on how many candidate rows are pulled into memory
/// before the 256-row allocation is applied — far above the cap itself, purely
/// to avoid unbounded allocation on a pathological store (archival/compaction
/// of this table is an explicit Phase 4+ item per 04 §5.4).
const SWEEP_CANDIDATE_FETCH_LIMIT: i64 = 10_000;

/// CL52 / R23-04: plan (without applying) the next bounded sweep. `own_key` is
/// the exact 4-tuple about to be claimed, if any (device rows only; `None` for
/// a standalone sweep pass with no specific claim in progress). The own-key /
/// general-pool split below matches on the FULL 4-tuple
/// (`adapter_kind`/`input_hash`/`tool_profile_hash`) — 04 §5.4's own-key rule
/// ("自 key (今回 claim する 4 組 key) の stale 行は上限枠外で常に最優先に回収
/// する") is scoped to the exact key about to be claimed, not merely the same
/// `(adapter_kind, input_hash)` pair with a DIFFERENT `tool_profile_hash` (a
/// distinct task identity per §5.5 — matching on only 2 of the 4 identity
/// columns would let an unrelated profile's stale rows ride the unbounded
/// own-key exemption instead of the capped general pool).
pub(crate) fn plan_bounded_sweep(
    conn: &Connection,
    own_key: Option<&TaskKey>,
    now_ms: i64,
) -> Result<SweepPlan> {
    let month_start = crate::ledger::time::current_month_start_millis(now_ms);

    let own_key_stale = if let Some(key) = own_key {
        query_device_keys(
            conn,
            "SELECT scope_id, adapter_kind, input_hash, tool_profile_hash FROM batch_requests
             WHERE scope_id = 'device' AND state IN (0, 1) AND stale_after_at IS NOT NULL
               AND stale_after_at <= ?1 AND adapter_kind = ?2 AND input_hash = ?3
               AND tool_profile_hash = ?4
             ORDER BY job_create_started_at ASC, scope_id, adapter_kind, input_hash, tool_profile_hash
             LIMIT ?5",
            params![
                now_ms,
                key.adapter_kind,
                key.input_hash,
                key.tool_profile_hash,
                SWEEP_CANDIDATE_FETCH_LIMIT
            ],
        )?
    } else {
        Vec::new()
    };

    let general_candidates = if let Some(key) = own_key {
        query_device_keys(
            conn,
            "SELECT scope_id, adapter_kind, input_hash, tool_profile_hash FROM batch_requests
             WHERE scope_id = 'device' AND state IN (0, 1) AND stale_after_at IS NOT NULL
               AND stale_after_at <= ?1
               AND NOT (adapter_kind = ?2 AND input_hash = ?3 AND tool_profile_hash = ?4)
             ORDER BY job_create_started_at ASC, scope_id, adapter_kind, input_hash, tool_profile_hash
             LIMIT ?5",
            params![
                now_ms,
                key.adapter_kind,
                key.input_hash,
                key.tool_profile_hash,
                SWEEP_CANDIDATE_FETCH_LIMIT
            ],
        )?
    } else {
        query_device_keys(
            conn,
            "SELECT scope_id, adapter_kind, input_hash, tool_profile_hash FROM batch_requests
             WHERE scope_id = 'device' AND state IN (0, 1) AND stale_after_at IS NOT NULL
               AND stale_after_at <= ?1
             ORDER BY job_create_started_at ASC, scope_id, adapter_kind, input_hash, tool_profile_hash
             LIMIT ?2",
            params![now_ms, SWEEP_CANDIDATE_FETCH_LIMIT],
        )?
    };

    // Planning is intentionally device-only, so task-scoped unknown-result
    // authorization rows never enter this candidate list. The execution-side
    // predicate below repeats that preservation in case a caller supplies a
    // manually constructed `SweepPlan` containing a task key.
    let prune_candidates = query_device_keys(
        conn,
        "SELECT scope_id, adapter_kind, input_hash, tool_profile_hash FROM batch_requests
         WHERE scope_id = 'device' AND state IN (2, 3) AND intent_token IS NULL
           AND contract_violation_count = 0 AND completed_at IS NOT NULL AND completed_at < ?1
         ORDER BY completed_at ASC, scope_id, adapter_kind, input_hash, tool_profile_hash
         LIMIT ?2",
        params![month_start, SWEEP_CANDIDATE_FETCH_LIMIT],
    )?;

    let (prune_take, general_take) = allocate_sweep_capacity(
        prune_candidates.len(),
        general_candidates.len(),
        SWEEP_TOTAL_CAP,
        SWEEP_PRUNE_MIN,
    );

    Ok(SweepPlan {
        own_key_stale,
        general_stale: general_candidates.into_iter().take(general_take).collect(),
        prune: prune_candidates.into_iter().take(prune_take).collect(),
    })
}

fn query_device_keys(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<TaskKey>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params, |row| {
        Ok(TaskKey::new(
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SweepReport {
    pub settled: Vec<TaskKey>,
    pub pruned: Vec<TaskKey>,
}

/// CL52-CL55: apply a [`SweepPlan`] — settle stale rows as `unknown_settled`
/// (never queried — CL53), delete terminal rows that still meet the pruning
/// predicate at execution time (re-checked, defense in depth against a
/// concurrent writer). All in one savepoint.
pub(crate) fn execute_bounded_sweep(
    conn: &Connection,
    plan: &SweepPlan,
    now_ms: i64,
) -> Result<SweepReport> {
    let report = with_savepoint(conn, "kio_ledger_device_sweep", || {
        let mut settled = Vec::new();
        for key in plan.own_key_stale.iter().chain(plan.general_stale.iter()) {
            if let Some(row) = get_batch_request(conn, key)?
                && let Some(token) = row.intent_token.clone()
            {
                recovery_settle_unknown(conn, key, &token, row.estimated_usd, true)?;
                settled.push(key.clone());
            }
        }
        let month_start = crate::ledger::time::current_month_start_millis(now_ms);
        let mut pruned = Vec::new();
        for key in &plan.prune {
            // QA14: routed through `cas_update_one` (rather than a bare
            // `conn.execute`) so a pruning DELETE also participates in the
            // write-seq bump.
            let changed = cas_update_one(
                conn,
                "DELETE FROM batch_requests
                 WHERE scope_id = ?1 AND adapter_kind = ?2 AND input_hash = ?3 AND tool_profile_hash = ?4
                   AND state IN (2, 3) AND intent_token IS NULL AND contract_violation_count = 0
                   AND completed_at < ?5
                   AND NOT (scope_id <> 'device' AND request_kind = 'sync'
                            AND error IN ('unknown_settled', 'unknown_resend_authorized'))",
                params![key.scope_id, key.adapter_kind, key.input_hash, key.tool_profile_hash, month_start],
            )?;
            if changed {
                pruned.push(key.clone());
            }
        }
        Ok(SweepReport { settled, pruned })
    })?;
    Ok(report)
}

// ---------------------------------------------------------------------------
// §I — budget cap check-then-reserve (CL56-CL61)
// ---------------------------------------------------------------------------

pub const PER_ADAPTER_KIND_ENUM: &[&str] = &["markdownize", "embedding"];

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BudgetCapConfig {
    pub device_cap: f64,
    pub folder_cap: Option<f64>,
    /// `ledger(device, adapter_kind, month)`'s cap — device-layer only per 04
    /// §5.4 ("folder cap は total のみ"); `None` when unset for this
    /// `adapter_kind`.
    pub device_per_adapter_cap: Option<f64>,
}

/// CL59: `cost_ledger`'s current-month sum (estimate rows count at face value)
/// plus `batch_requests`'s unterminated (`state IN (0,1)`) `estimated_usd`
/// reservation sum. `scope_id`/`adapter_kind` filters are optional (`None` =
/// unfiltered, i.e. the device-wide total).
pub(crate) fn ledger_month_total(
    conn: &Connection,
    scope_id: Option<&str>,
    adapter_kind: Option<&str>,
    month: &str,
) -> Result<f64> {
    let ledger_sum: f64 = conn.query_row(
        "SELECT COALESCE(SUM(usd), 0) FROM cost_ledger
         WHERE month = ?1 AND (?2 IS NULL OR scope_id = ?2) AND (?3 IS NULL OR adapter_kind = ?3)",
        params![month, scope_id, adapter_kind],
        |row| row.get(0),
    )?;
    let reserved_sum: f64 = conn.query_row(
        "SELECT COALESCE(SUM(estimated_usd), 0) FROM batch_requests
         WHERE state IN (0, 1) AND (?1 IS NULL OR scope_id = ?1) AND (?2 IS NULL OR adapter_kind = ?2)",
        params![scope_id, adapter_kind],
        |row| row.get(0),
    )?;
    Ok(ledger_sum + reserved_sum)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapLayer {
    Device,
    Folder,
    PerAdapter,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CapCheckResult {
    /// CL58: `candidate_usd == 0.0` bypasses the cap check entirely.
    ExemptZeroCost(Phase1Outcome),
    Allowed(Phase1Outcome),
    Denied(CapLayer),
}

/// CL56-CL61: the full check-then-reserve — evaluate device / folder /
/// per-adapter caps (in that order; `folder_cap`/`device_per_adapter_cap`
/// absent = that layer does not apply), and only on a pass, run phase 1 in the
/// SAME outer transaction so the check and reservation cannot race.
pub(crate) fn check_then_reserve(
    conn: &Connection,
    key: &TaskKey,
    candidate_usd: f64,
    caps: &BudgetCapConfig,
    request_kind: RequestKind,
    sync_effective_timeout_seconds: Option<i64>,
) -> Result<CapCheckResult> {
    if candidate_usd == 0.0 {
        let outcome = phase1_intent(conn, key, request_kind, 0.0, sync_effective_timeout_seconds)?;
        return Ok(CapCheckResult::ExemptZeroCost(outcome));
    }
    let month = utc_month_of(now_millis());
    let device_total = ledger_month_total(conn, None, None, &month)?;
    if device_total + candidate_usd >= caps.device_cap {
        return Ok(CapCheckResult::Denied(CapLayer::Device));
    }
    if let Some(folder_cap) = caps.folder_cap {
        let folder_total = ledger_month_total(conn, Some(&key.scope_id), None, &month)?;
        if folder_total + candidate_usd >= folder_cap {
            return Ok(CapCheckResult::Denied(CapLayer::Folder));
        }
    }
    if let Some(per_adapter_cap) = caps.device_per_adapter_cap {
        let adapter_total = ledger_month_total(conn, None, Some(&key.adapter_kind), &month)?;
        if adapter_total + candidate_usd >= per_adapter_cap {
            return Ok(CapCheckResult::Denied(CapLayer::PerAdapter));
        }
    }
    let outcome = phase1_intent(
        conn,
        key,
        request_kind,
        candidate_usd,
        sync_effective_timeout_seconds,
    )?;
    Ok(CapCheckResult::Allowed(outcome))
}

/// CL61: `[budget.per_adapter]` keys are the same closed enum as
/// `adapter_kind` (`markdownize` | `embedding`); anything else is a config
/// schema error.
#[must_use]
pub fn is_valid_per_adapter_key(key: &str) -> bool {
    PER_ADAPTER_KIND_ENUM.contains(&key)
}

// ---------------------------------------------------------------------------
// §J — `kio batch abandon` (CL62-CL68)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum AbandonSelector {
    IntentToken(String),
    TaskKey(TaskKey),
    /// 3-tuple selector missing `tool_profile_hash` — valid only when it
    /// resolves unambiguously (CL62(c)).
    ThreeTuple {
        scope_id: String,
        adapter_kind: String,
        input_hash: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum AbandonResolution {
    /// CL66: no matching row — the caller returns exit-0 idempotent success.
    NotFound,
    /// CL62(c): a 3-tuple matched more than one `tool_profile_hash` — reject
    /// and require the caller to disambiguate with a token or the full 4-tuple.
    Ambiguous,
    Found(TaskKey),
}

pub(crate) fn resolve_abandon_selector(
    conn: &Connection,
    selector: &AbandonSelector,
) -> Result<AbandonResolution> {
    match selector {
        AbandonSelector::IntentToken(token) => {
            let key = conn
                .query_row(
                    "SELECT scope_id, adapter_kind, input_hash, tool_profile_hash
                     FROM batch_requests WHERE intent_token = ?1",
                    params![token],
                    |row| {
                        Ok(TaskKey::new(
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    },
                )
                .optional()?;
            Ok(key.map_or(AbandonResolution::NotFound, AbandonResolution::Found))
        }
        AbandonSelector::TaskKey(key) => {
            let exists: i64 = conn.query_row(
                "SELECT COUNT(*) FROM batch_requests
                 WHERE scope_id = ?1 AND adapter_kind = ?2 AND input_hash = ?3 AND tool_profile_hash = ?4",
                params![key.scope_id, key.adapter_kind, key.input_hash, key.tool_profile_hash],
                |row| row.get(0),
            )?;
            Ok(if exists > 0 {
                AbandonResolution::Found(key.clone())
            } else {
                AbandonResolution::NotFound
            })
        }
        AbandonSelector::ThreeTuple {
            scope_id,
            adapter_kind,
            input_hash,
        } => {
            let mut stmt = conn.prepare(
                "SELECT tool_profile_hash FROM batch_requests
                 WHERE scope_id = ?1 AND adapter_kind = ?2 AND input_hash = ?3",
            )?;
            let hashes: Vec<String> = stmt
                .query_map(params![scope_id, adapter_kind, input_hash], |row| {
                    row.get(0)
                })?
                .collect::<rusqlite::Result<_>>()?;
            match hashes.len() {
                0 => Ok(AbandonResolution::NotFound),
                1 => Ok(AbandonResolution::Found(TaskKey::new(
                    scope_id.clone(),
                    adapter_kind.clone(),
                    input_hash.clone(),
                    hashes.into_iter().next().expect("len == 1"),
                ))),
                _ => Ok(AbandonResolution::Ambiguous),
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AbandonExecution {
    /// CL66: exit-0 idempotent success — nothing to do.
    NoTarget,
    Abandoned,
}

/// CL64-CL67: abandon, after the caller has already obtained user confirmation
/// (CL65 — confirmation itself is a CLI-layer concern, not this module's).
/// note-5: a phase-1-only row (`provider_scope_id IS NULL`, so no upload could
/// possibly exist) clears `intent_token` immediately; any row that reached
/// phase 2a keeps it set until the caller separately confirms cleanup via
/// [`recovery_finish_cleanup`] (sync rows always take the immediate-clear path,
/// since `provider_scope_id` is never set for `request_kind = 'sync'` — CL47).
///
/// R23-18 (04 §5.4: "剪定・確定済みの 4 組 key への `kio batch abandon` は対象
/// なしの冪等成功"): a `state=2` (Completed) row already has its terminal
/// charge durably recorded under its own `submission_seq` — abandon must not
/// re-settle it (no new `submission_seq`, no additional `cost_ledger` row).
/// This differs from the `intent_token IS NULL` short-circuit above only in
/// that residue cleanup (upload deletion → `provider_scope_id IS NULL`) may
/// still be outstanding for a successful row; when it is, only that cleanup
/// proceeds — the same immediate-vs-deferred split note-5 (above) describes
/// for the non-Completed path — never a re-charge.
pub(crate) fn execute_abandon(conn: &Connection, key: &TaskKey) -> Result<AbandonExecution> {
    let outcome = with_savepoint(conn, "kio_ledger_abandon", || {
        let Some(row) = get_batch_request(conn, key)? else {
            return Ok(AbandonExecution::NoTarget);
        };
        if row.state.is_terminal() && row.intent_token.is_none() {
            return Ok(AbandonExecution::NoTarget);
        }
        let Some(token) = row.intent_token.clone() else {
            // An in-flight (non-terminal) row always carries a token by
            // construction (phase 1 always sets one) — unreachable in
            // practice, but treated as an idempotent no-op rather than panicking.
            return Ok(AbandonExecution::NoTarget);
        };
        if row.state == BatchState::Completed {
            // R23-18: the success charge is already final — only clear the
            // residual token once no upload could possibly remain (the same
            // `provider_scope_id IS NULL` test the re-settle path below uses).
            // A non-NULL `provider_scope_id` means cleanup is still pending;
            // that is resolved out-of-band via `recovery_finish_cleanup` once
            // deletion is confirmed, never by this call re-charging.
            if row.provider_scope_id.is_none() {
                recovery_finish_cleanup(conn, key, &token)?;
            }
            return Ok(AbandonExecution::Abandoned);
        }
        let cleanup_already_complete = row.provider_scope_id.is_none();
        terminal_transaction(
            conn,
            &TerminalWrite {
                key,
                outcome: Outcome::Abandoned,
                billed: BilledAmount {
                    usd: row.estimated_usd,
                    estimated: true,
                },
                ledger_batch_job_id: &token,
                next_state: BatchState::Terminal,
                error: Some("abandoned"),
                increment_contract_violation: false,
                attempts_delta: 0,
                clear_intent_token: cleanup_already_complete,
                intent_token_guard: None,
                reseat_submission_seq: true,
            },
        )?;
        Ok(AbandonExecution::Abandoned)
    })?;
    Ok(outcome)
}

/// CL55: `contract_violation_count` reset for `--reset-violations` (§M note-6):
/// a `count == 0` row is a no-op success; an in-flight (`state IN (0,1)`) row is
/// skipped (only terminal rows are reset). Returns whether the row was reset.
///
/// QA14: routed through `cas_update_one` (rather than a bare `conn.execute`,
/// as before) so this mutation also participates in the write-seq bump.
pub(crate) fn reset_contract_violations(conn: &Connection, key: &TaskKey) -> Result<bool> {
    let changed = cas_update_one(
        conn,
        "UPDATE batch_requests SET contract_violation_count = 0
         WHERE scope_id = ?1 AND adapter_kind = ?2 AND input_hash = ?3 AND tool_profile_hash = ?4
           AND state IN (2, 3) AND contract_violation_count != 0",
        params![
            key.scope_id,
            key.adapter_kind,
            key.input_hash,
            key.tool_profile_hash
        ],
    )?;
    Ok(changed)
}

// ---------------------------------------------------------------------------
// Inline tests for pure (non-DB) logic. The larger DB-integration contracts
// (phase transitions, idempotent recording, crash recovery, budget cap,
// abandon) live in `crates/kio-cli/tests/step4b_ledger_contract.rs` per the
// implementation instructions; the CL-numbered contracts covered here are
// specifically the ones whose "期待" is pure computation.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn unreset_batch_contract_violations_require_exact_current_attempt() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::ledger::schema::CREATE_BATCH_REQUESTS_SQL)
            .unwrap();
        conn.execute_batch(crate::ledger::schema::CREATE_COST_LEDGER_SQL)
            .unwrap();
        let cases = [
            (
                "current",
                "scope",
                "embedding",
                "batch",
                3,
                1,
                2,
                "contract_violation",
            ),
            (
                "prior",
                "scope",
                "embedding",
                "batch",
                3,
                1,
                1,
                "contract_violation",
            ),
            ("expired", "scope", "embedding", "batch", 3, 1, 2, "expired"),
            (
                "reset",
                "scope",
                "embedding",
                "batch",
                3,
                0,
                2,
                "contract_violation",
            ),
            (
                "other-scope",
                "elsewhere",
                "embedding",
                "batch",
                3,
                1,
                2,
                "contract_violation",
            ),
            (
                "other-adapter",
                "scope",
                "markdownize",
                "batch",
                3,
                1,
                2,
                "contract_violation",
            ),
            (
                "sync",
                "scope",
                "embedding",
                "sync",
                3,
                1,
                2,
                "contract_violation",
            ),
            (
                "completed",
                "scope",
                "embedding",
                "batch",
                2,
                1,
                2,
                "contract_violation",
            ),
            (
                "inflight",
                "scope",
                "embedding",
                "batch",
                1,
                1,
                2,
                "contract_violation",
            ),
        ];
        for (input, scope, adapter, kind, state, count, cost_seq, outcome) in cases {
            conn.execute("INSERT INTO batch_requests(scope_id,adapter_kind,input_hash,tool_profile_hash,state,request_kind,submission_seq,contract_violation_count,estimated_usd,created_at,completed_at,error)
                VALUES (?1,?2,?3,'profile',?4,?5,2,?6,0.0,0,1,'contract_violation diagnostic is not authority')",
                params![scope, adapter, input, state, kind, count]).unwrap();
            conn.execute("INSERT INTO cost_ledger(scope_id,adapter_kind,input_hash,tool_profile_hash,submission_seq,batch_job_id,usd,outcome,month,recorded_at)
                VALUES (?1,?2,?3,'profile',?4,'job',0.0,?5,'2026-09',1)",
                params![scope, adapter, input, cost_seq, outcome]).unwrap();
        }
        // An old violation plus a current expired attempt must not block new work.
        conn.execute("INSERT INTO cost_ledger(scope_id,adapter_kind,input_hash,tool_profile_hash,submission_seq,batch_job_id,usd,outcome,month,recorded_at)
            VALUES ('scope','embedding','expired','profile',1,'old-job',0.0,'contract_violation','2026-09',0)", []).unwrap();
        // Each full-key component must match; a same-input current outcome from a
        // different profile, scope, or adapter cannot supply missing authority.
        for (scope, adapter, profile) in [
            ("scope", "embedding", "other-profile"),
            ("other-scope", "embedding", "profile"),
            ("scope", "markdownize", "profile"),
        ] {
            conn.execute("INSERT INTO cost_ledger(scope_id,adapter_kind,input_hash,tool_profile_hash,submission_seq,batch_job_id,usd,outcome,month,recorded_at)
                VALUES (?1,?2,'prior',?3,2,'unrelated-job',0.0,'contract_violation','2026-09',0)",
                params![scope, adapter, profile]).unwrap();
        }
        let rows =
            unreset_contract_violations_for_scope_adapter(&conn, "scope", "embedding").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key.input_hash, "current");
        assert_eq!(rows[0].submission_seq, 2);
        assert!(
            rows[0].intent_token.is_none(),
            "cleaned terminal attempts remain visible"
        );
        assert!(rows[0].upload_id.is_none());
        assert_eq!(
            rows[0].completed_at,
            Some(1),
            "old terminal timestamps do not expire the breaker"
        );
        assert!(
            unreset_contract_violations_for_scope_adapter(&conn, "scope' OR 1=1 --", "embedding")
                .unwrap()
                .is_empty()
        );
        reset_contract_violations(&conn, &rows[0].key).unwrap();
        assert!(
            unreset_contract_violations_for_scope_adapter(&conn, "scope", "embedding")
                .unwrap()
                .is_empty()
        );
    }

    // CL13 (partial — the pure "is this a valid UUIDv7" / "does it embed the
    // right timestamp" checks; the DB-side INSERT contents are covered by the
    // dedicated contract test file).
    #[test]
    fn cl13_new_intent_token_is_a_valid_uuid_v7_embedding_the_current_millisecond() {
        let before = now_millis();
        let token = new_intent_token();
        let after = now_millis();
        assert_eq!(token.len(), 36, "canonical hyphenated UUID length");
        assert!(is_uuid_v7(&token), "version nibble must be 7: {token}");
        let embedded = uuid_v7_timestamp_millis(&token).expect("timestamp decodes");
        assert!(
            (before..=after).contains(&embedded),
            "embedded timestamp {embedded} must fall within [{before}, {after}]"
        );
        // Two tokens minted back-to-back must never collide.
        assert_ne!(token, new_intent_token());
    }

    #[test]
    fn non_uuid_v7_strings_are_rejected() {
        assert!(!is_uuid_v7("not-a-uuid"));
        // A v4 UUID (version nibble 4) must not pass.
        assert!(!is_uuid_v7("00000000-0000-4000-8000-000000000000"));
        assert!(uuid_v7_timestamp_millis("garbage").is_none());
    }

    // CL49: the governing spec formula (`max(effective_timeout + 60, 600)`),
    // including the case CL49's own first worked example gets arithmetically
    // wrong (see compute_stale_after_at's doc comment) — implemented per the
    // spec text and this function's second, self-consistent example.
    #[test]
    fn cl49_stale_after_at_applies_the_600s_floor_unconditionally() {
        let now = 1_000_000_000_i64;
        // max(300, 120) = 300 effective timeout -> 300+60=360, floored to 600.
        assert_eq!(compute_stale_after_at(now, 300), now + 600_000);
        // Both scopes at timeout=100 -> 100+60=160, floored to 600 (CL49's own
        // second example, arithmetically consistent with the spec text).
        assert_eq!(compute_stale_after_at(now, 100), now + 600_000);
        // A large enough timeout is not floored: 1000+60=1060 > 600.
        assert_eq!(compute_stale_after_at(now, 1000), now + 1_060_000);
    }

    // CL50: Retry-After extension math (the pure "safe substitution + additive
    // formula" half; the CAS UPDATE / claim-loss half is a DB contract in the
    // dedicated test file).
    #[test]
    fn cl50_retry_after_substitutes_3600_only_for_invalid_values() {
        let now = 1_000_000_000_i64;
        let timeout = 300_i64;
        // Valid short Retry-After: 30 + 300 + 60 = 390s.
        let short = now + (30 + timeout + 60) * 1000;
        assert_eq!(short, now + 390_000);
        // Valid long Retry-After is never clamped: 7200 + 300 + 60 = 7560s.
        let long = now + (7200 + timeout + 60) * 1000;
        assert_eq!(long, now + 7_560_000);
        // Invalid (negative) substitutes 3600: 3600 + 300 + 60 = 3960s.
        let substituted = now + (3600 + timeout + 60) * 1000;
        assert_eq!(substituted, now + 3_960_000);
    }

    // CL35 / R23-05: the grace-period basis is `job_create_started_at`, not
    // the (now-removed) `intent_token` parameter — a pure `Option<i64>` in,
    // `bool` out predicate. A `None` basis (phase 2b never started) can never
    // report elapsed, matching 04 §5.8's own rule excluding such rows from
    // confirmed-absent job-list matching entirely.
    #[test]
    fn cl35_r23_05_visibility_grace_period_is_pure_and_null_basis_never_elapses() {
        let started_at = 5_000_000_i64;
        assert!(!visibility_grace_period_elapsed(
            Some(started_at),
            started_at + DEFAULT_VISIBILITY_GRACE_PERIOD_MS - 1,
            DEFAULT_VISIBILITY_GRACE_PERIOD_MS
        ));
        assert!(visibility_grace_period_elapsed(
            Some(started_at),
            started_at + DEFAULT_VISIBILITY_GRACE_PERIOD_MS,
            DEFAULT_VISIBILITY_GRACE_PERIOD_MS
        ));
        assert!(!visibility_grace_period_elapsed(
            None,
            i64::MAX,
            DEFAULT_VISIBILITY_GRACE_PERIOD_MS
        ));
    }

    // CL27: usd-field pre-validation.
    #[test]
    fn cl27_usd_field_validation_degrades_non_finite_or_negative_to_estimated() {
        let fallback = 4.0;
        assert_eq!(
            resolve_billing_from_usd_field(2.50, fallback),
            BilledAmount {
                usd: 2.50,
                estimated: false
            }
        );
        for bad in [-1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(
                resolve_billing_from_usd_field(bad, fallback),
                BilledAmount {
                    usd: fallback,
                    estimated: true
                },
                "usd={bad} must degrade to the estimated fallback"
            );
        }
    }

    // CL28: billable_units pre-validation (empty / duplicate kind / unknown
    // kind / non-integer count / negative count / valid single / valid
    // multi-element sum).
    #[test]
    fn cl28_billable_units_validation_paramterized() {
        let declared: BTreeSet<String> = ["pages", "tokens_in", "tokens_out"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let mut pricing = BTreeMap::new();
        pricing.insert("pages".to_owned(), 0.01);
        pricing.insert("tokens_in".to_owned(), 0.000_002);
        pricing.insert("tokens_out".to_owned(), 0.000_006);
        let fallback = 9.0;

        // (a) valid single element.
        let valid_single = [ReportedBillableUnit {
            kind: "pages".to_owned(),
            count: 10.0,
        }];
        assert_eq!(
            resolve_billing_from_billable_units(&valid_single, &declared, &pricing, fallback),
            BilledAmount {
                usd: 0.10,
                estimated: false
            }
        );

        // (b) empty array.
        assert_eq!(
            resolve_billing_from_billable_units(&[], &declared, &pricing, fallback),
            BilledAmount {
                usd: fallback,
                estimated: true
            }
        );

        // (c) duplicate kind.
        let dup = [
            ReportedBillableUnit {
                kind: "pages".to_owned(),
                count: 5.0,
            },
            ReportedBillableUnit {
                kind: "pages".to_owned(),
                count: 3.0,
            },
        ];
        assert!(resolve_billing_from_billable_units(&dup, &declared, &pricing, fallback).estimated);

        // (d) kind outside the declared set.
        let undeclared = [ReportedBillableUnit {
            kind: "images".to_owned(),
            count: 2.0,
        }];
        assert!(
            resolve_billing_from_billable_units(&undeclared, &declared, &pricing, fallback)
                .estimated
        );

        // (e) non-integer count.
        let fractional = [ReportedBillableUnit {
            kind: "pages".to_owned(),
            count: 2.5,
        }];
        assert!(
            resolve_billing_from_billable_units(&fractional, &declared, &pricing, fallback)
                .estimated
        );

        // (f) negative count.
        let negative = [ReportedBillableUnit {
            kind: "pages".to_owned(),
            count: -1.0,
        }];
        assert!(
            resolve_billing_from_billable_units(&negative, &declared, &pricing, fallback).estimated
        );

        // (g) valid multi-element sum: 1000*0.000002 + 200*0.000006 = 0.0032.
        let multi = [
            ReportedBillableUnit {
                kind: "tokens_in".to_owned(),
                count: 1000.0,
            },
            ReportedBillableUnit {
                kind: "tokens_out".to_owned(),
                count: 200.0,
            },
        ];
        let billed = resolve_billing_from_billable_units(&multi, &declared, &pricing, fallback);
        assert!(!billed.estimated);
        assert!((billed.usd - 0.0032).abs() < 1e-12, "got {}", billed.usd);
    }

    // CL30: a declared, well-formed kind whose price is unresolvable in the
    // pricing table degrades to estimated (never a $0 confirmed charge).
    #[test]
    fn cl30_unresolvable_price_degrades_to_estimated_not_zero() {
        let declared: BTreeSet<String> = ["tokens_in"].into_iter().map(str::to_owned).collect();
        let pricing = BTreeMap::new(); // tokens_in absent from [pricing]
        let units = [ReportedBillableUnit {
            kind: "tokens_in".to_owned(),
            count: 500.0,
        }];
        let billed = resolve_billing_from_billable_units(&units, &declared, &pricing, 3.0);
        assert!(billed.estimated);
        assert_eq!(billed.usd, 3.0, "must use the fallback, not usd=0");
    }

    // CL29: the nonbillable path is a real $0 confirmed charge, never estimated.
    #[test]
    fn cl29_nonbillable_charge_is_confirmed_zero_not_estimated() {
        assert_eq!(
            nonbillable_charge(),
            BilledAmount {
                usd: 0.0,
                estimated: false
            }
        );
    }

    // ------------------------------------------------------------------
    // QA17/QA18/QA19 (step4b-contract-tests-p3a.md §F): the AdapterRun-usage
    // -> BilledAmount join point, `resolve_billing_from_reported_usage`.
    // ------------------------------------------------------------------

    /// QA17: `usage: None` (an Adapter with no real per-call signal to
    /// report, e.g. this codebase's Gemini embedding integration) degrades
    /// to the reservation estimate — exactly the behavior every settle call
    /// site had before this field existed. No uncovered-kind warning either
    /// (there is no billable_units report to inspect).
    #[test]
    fn qa17_no_usage_degrades_to_estimated_reservation() {
        let declared: BTreeSet<String> = BTreeSet::new();
        let pricing = BTreeMap::new();
        let (billed, uncovered) =
            resolve_billing_from_reported_usage(None, &declared, &pricing, 7.5);
        assert_eq!(
            billed,
            BilledAmount {
                usd: 7.5,
                estimated: true
            }
        );
        assert!(uncovered.is_empty());
    }

    /// QA17: a `usd`-shaped usage report resolves through
    /// `resolve_billing_from_usd_field` unchanged (CL27's rules) — a valid,
    /// finite non-negative value bills at that exact figure.
    #[test]
    fn qa17_usd_usage_resolves_via_existing_cl27_rule() {
        let declared: BTreeSet<String> = BTreeSet::new();
        let pricing = BTreeMap::new();
        let usage = kio_adapter::types::AdapterUsage::Usd { usd: 1.23 };
        let (billed, uncovered) =
            resolve_billing_from_reported_usage(Some(&usage), &declared, &pricing, 9.0);
        assert_eq!(
            billed,
            BilledAmount {
                usd: 1.23,
                estimated: false
            }
        );
        assert!(uncovered.is_empty());
    }

    /// QA17/QA19: a `billable_units` report whose kind IS declared and IS
    /// priced bills the real amount (`count * pricing[kind]`) — the Mistral
    /// OCR "processed N pages" self-report shape.
    #[test]
    fn qa17_qa19_billable_units_with_full_coverage_bills_real_amount() {
        let declared: BTreeSet<String> = ["pages"].into_iter().map(str::to_owned).collect();
        let mut pricing = BTreeMap::new();
        pricing.insert("pages".to_owned(), 0.004);
        let usage = kio_adapter::types::AdapterUsage::BillableUnits {
            billable_units: vec![kio_adapter::types::BillableUnit {
                kind: kio_adapter::types::BillableUnitKind::Pages,
                count: 12,
            }],
        };
        let (billed, uncovered) =
            resolve_billing_from_reported_usage(Some(&usage), &declared, &pricing, 5.0);
        assert!(!billed.estimated);
        assert!((billed.usd - 0.048).abs() < 1e-12, "got {}", billed.usd);
        assert!(uncovered.is_empty());
    }

    /// QA19: a `billable_units` kind with NO `tools.toml` price ("単価未被覆の
    /// kind") degrades to the reservation estimate (CL30, unchanged) AND is
    /// named in the returned uncovered-kind list so the caller can log the
    /// one warning line — this is the field this function adds over calling
    /// `resolve_billing_from_billable_units` directly.
    #[test]
    fn qa19_uncovered_pricing_kind_degrades_and_is_reported() {
        let declared: BTreeSet<String> = ["tokens_in"].into_iter().map(str::to_owned).collect();
        let pricing = BTreeMap::new(); // tokens_in declared but NOT priced
        let usage = kio_adapter::types::AdapterUsage::BillableUnits {
            billable_units: vec![kio_adapter::types::BillableUnit {
                kind: kio_adapter::types::BillableUnitKind::TokensIn,
                count: 500,
            }],
        };
        let (billed, uncovered) =
            resolve_billing_from_reported_usage(Some(&usage), &declared, &pricing, 3.0);
        assert_eq!(
            billed,
            BilledAmount {
                usd: 3.0,
                estimated: true
            }
        );
        assert_eq!(uncovered, vec!["tokens_in".to_owned()]);
    }

    /// QA19: an undeclared/unknown kind (not a pricing-coverage defect) still
    /// degrades billing (CL28, unchanged) but is NOT reported as an
    /// "uncovered pricing kind" — the uncovered-kind list is scoped
    /// specifically to "declared and reportable, but tools.toml has no
    /// price for it", not every reason a report can degrade.
    #[test]
    fn qa19_undeclared_kind_degrades_without_a_pricing_warning() {
        let declared: BTreeSet<String> = BTreeSet::new(); // pages NOT declared
        let mut pricing = BTreeMap::new();
        pricing.insert("pages".to_owned(), 0.004); // priced, but not declared
        let usage = kio_adapter::types::AdapterUsage::BillableUnits {
            billable_units: vec![kio_adapter::types::BillableUnit {
                kind: kio_adapter::types::BillableUnitKind::Pages,
                count: 3,
            }],
        };
        let (billed, uncovered) =
            resolve_billing_from_reported_usage(Some(&usage), &declared, &pricing, 2.0);
        assert!(billed.estimated);
        assert!(
            uncovered.is_empty(),
            "a priced-but-undeclared kind is not a pricing-coverage defect"
        );
    }

    // CL52: the bounded-sweep capacity allocator's three worked scenarios.
    #[test]
    fn cl52_allocate_sweep_capacity_examples() {
        // Exactly the contract doc's own numbers: prune=400, general=500.
        assert_eq!(allocate_sweep_capacity(400, 500, 256, 128), (128, 128));
        // General short (50 < 128): prune absorbs the leftover.
        assert_eq!(allocate_sweep_capacity(400, 50, 256, 128), (206, 50));
        // Prune short (50 < 128): general absorbs the leftover.
        assert_eq!(allocate_sweep_capacity(50, 500, 256, 128), (50, 206));
        // Both short: nothing left over to reallocate, both fully drained.
        assert_eq!(allocate_sweep_capacity(10, 20, 256, 128), (10, 20));
        // Both empty.
        assert_eq!(allocate_sweep_capacity(0, 0, 256, 128), (0, 0));
    }

    // CL61: the per_adapter config key enum.
    #[test]
    fn cl61_per_adapter_key_enum_is_closed() {
        assert!(is_valid_per_adapter_key("markdownize"));
        assert!(is_valid_per_adapter_key("embedding"));
        assert!(!is_valid_per_adapter_key("summary"));
        assert!(!is_valid_per_adapter_key("markdown"));
        assert!(!is_valid_per_adapter_key("unknown_kind"));
    }

    // CL21: the durable retry-allowed predicate (count<=1 allowed, >=2 blocked).
    #[test]
    fn cl21_contract_violation_retry_allowed_threshold() {
        let row = |count| BatchRequestRow {
            key: TaskKey::new("s", "markdownize", "h", "t"),
            state: BatchState::Terminal,
            request_kind: RequestKind::Batch,
            intent_token: None,
            upload_id: None,
            batch_job_id: None,
            provider_scope_id: None,
            job_create_started_at: None,
            stale_after_at: None,
            submission_seq: 1,
            attempts: 0,
            contract_violation_count: count,
            estimated_usd: 0.0,
            error: None,
            completed_at: None,
            created_at: 0,
        };
        assert!(contract_violation_retry_allowed(&row(0)));
        assert!(contract_violation_retry_allowed(&row(1)));
        assert!(!contract_violation_retry_allowed(&row(2)));
        assert!(!contract_violation_retry_allowed(&row(3)));
    }

    // CL48: device input_hash never stores the query text and is stable under
    // NFC-equivalent inputs.
    #[test]
    fn cl48_device_input_hash_is_nfc_stable_and_looks_nothing_like_the_query() {
        // "é" as a single codepoint vs. "e" + combining acute accent — NFC
        // normalizes both to the same string before hashing.
        let precomposed = "café";
        let decomposed = "cafe\u{0301}";
        assert_ne!(precomposed, decomposed, "test fixture sanity");
        assert_eq!(
            device_input_hash(precomposed),
            device_input_hash(decomposed)
        );
        assert!(device_input_hash(precomposed).starts_with("sha256:"));
        assert!(!device_input_hash(precomposed).contains("café"));
    }

    // -----------------------------------------------------------------
    // QA14 — the restore-reconcile gate inside `phase1_intent`/`device_claim`
    // (step4b-contract-tests-p3a.md L307-321, 10-operations.md §7.5.2)
    // -----------------------------------------------------------------

    /// Raw fixture access is deliberately test-only. Production callers use
    /// the typed `LedgerDb` facade and cannot obtain this connection.
    fn open_temp_ledger_for_gate_tests() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .canonicalize()
            .unwrap()
            .join("device/cost-ledger.sqlite");
        crate::ledger::LedgerDb::initialize(&path).unwrap();
        (dir, Connection::open(path).unwrap())
    }

    fn assert_closed_enum_decode_failure(err: PipelineError, column_name: &str) {
        assert!(
            matches!(
                err,
                PipelineError::Sqlite(rusqlite::Error::FromSqlConversionFailure(_, _, _))
            ),
            "unknown persisted enum must be a SQL conversion failure: {err}"
        );
        assert!(
            err.to_string().contains(column_name),
            "failure must identify the corrupt column: {err}"
        );
    }

    #[test]
    fn corrupt_batch_request_enums_fail_public_read_without_defaulting() {
        let (_dir, raw_conn) = open_temp_ledger_for_gate_tests();
        let conn = &raw_conn;
        conn.execute_batch("PRAGMA ignore_check_constraints = ON;")
            .unwrap();

        for (suffix, column, value) in [
            ("state", "state", "99"),
            ("kind", "request_kind", "'unknown-kind'"),
        ] {
            let key = TaskKey::new(
                format!("scope-{suffix}"),
                "markdownize",
                "hash-a",
                "profile-a",
            );
            phase1_intent(conn, &key, RequestKind::Batch, 1.0, None).unwrap();
            conn.execute_batch(&format!(
                "UPDATE batch_requests SET {column} = {value} WHERE scope_id = 'scope-{suffix}'"
            ))
            .unwrap();

            let err = get_batch_request(conn, &key).unwrap_err();
            assert_closed_enum_decode_failure(err, &format!("batch_requests.{column}"));
        }
    }

    #[test]
    fn recovery_finish_cleanup_refuses_inflight_rows_and_preserves_terminal_accounting() {
        let (_dir, raw_conn) = open_temp_ledger_for_gate_tests();
        let conn = &raw_conn;
        let key = TaskKey::new("cleanup-scope", "embedding", "input", "profile");
        let phase1 = phase1_intent(conn, &key, RequestKind::Batch, 1.25, None).unwrap();
        let token = phase1.intent_token;

        // State=0 (intent recorded) must retain its only recovery selector.
        assert!(!recovery_finish_cleanup(conn, &key, &token).unwrap());
        assert_eq!(
            get_batch_request(conn, &key)
                .unwrap()
                .unwrap()
                .intent_token
                .as_deref(),
            Some(token.as_str())
        );

        // State=1 (provider job known) is equally live and must retain it.
        assert!(phase2b_record_job_created(conn, &key, &token, "batches/live").unwrap());
        assert!(!recovery_finish_cleanup(conn, &key, &token).unwrap());
        assert_eq!(
            get_batch_request(conn, &key)
                .unwrap()
                .unwrap()
                .intent_token
                .as_deref(),
            Some(token.as_str())
        );

        // A terminal row with residue may clear once, without reseating or
        // adding a cost row; repeat cleanup is idempotent.
        recovery_settle_unknown(conn, &key, &token, 1.25, false).unwrap();
        let charges_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM cost_ledger", [], |row| row.get(0))
            .unwrap();
        assert!(recovery_finish_cleanup(conn, &key, &token).unwrap());
        assert!(!recovery_finish_cleanup(conn, &key, &token).unwrap());
        assert!(
            get_batch_request(conn, &key)
                .unwrap()
                .unwrap()
                .intent_token
                .is_none()
        );
        let charges_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM cost_ledger", [], |row| row.get(0))
            .unwrap();
        assert_eq!(charges_after, charges_before);
    }

    #[test]
    fn corrupt_cost_ledger_outcome_fails_public_read_without_succeeding() {
        let (_dir, raw_conn) = open_temp_ledger_for_gate_tests();
        let conn = &raw_conn;
        let key = TaskKey::new("scope-a", "markdownize", "hash-a", "profile-a");
        conn.execute_batch("PRAGMA ignore_check_constraints = ON;")
            .unwrap();
        conn.execute(
            "INSERT INTO cost_ledger (
                scope_id, adapter_kind, input_hash, tool_profile_hash, submission_seq,
                batch_job_id, usd, estimated, outcome, month, recorded_at
             ) VALUES (?1, ?2, ?3, ?4, 1, 'job-a', 1.0, 0, 'unknown-outcome', '2026-08', 0)",
            params![
                key.scope_id,
                key.adapter_kind,
                key.input_hash,
                key.tool_profile_hash
            ],
        )
        .unwrap();

        let err = cost_ledger_rows_for_key(conn, &key).unwrap_err();
        assert_closed_enum_decode_failure(err, "cost_ledger.outcome");
    }

    /// `phase1_intent` is the SOLE `batch_requests` INSERT — gating it here
    /// covers every caller (`check_then_reserve`'s two branches,
    /// `device_claim`, and `kio-cli`'s `record_free_local_charge`/
    /// `reserve_or_reuse_task_charge` bypass branch). No row is created —
    /// the gate refuses BEFORE the INSERT runs.
    #[test]
    fn qa14_phase1_intent_refuses_a_new_submission_while_restore_marker_present() {
        let (_dir, raw_conn) = open_temp_ledger_for_gate_tests();
        let conn = &raw_conn;
        crate::ledger::schema::record_marker(
            conn,
            crate::ledger::schema::RESTORE_RECONCILE_PENDING_MARKER,
        )
        .unwrap();
        let key = TaskKey::new("scope-a", "markdownize", "hash-a", "profile-a");
        let err = match phase1_intent(conn, &key, RequestKind::Batch, 1.0, None) {
            Ok(outcome) => panic!("expected the restore gate to refuse, got {outcome:?}"),
            Err(err) => err,
        };
        assert!(
            err.to_string()
                .contains("KIO-E-BATCH-RESTORE-RECONCILE-001"),
            "got {err}"
        );
        assert!(
            get_batch_request(conn, &key).unwrap().is_none(),
            "the gate must refuse BEFORE any row is inserted"
        );
    }

    /// `device_claim` (the query-embedding device row) funnels its own
    /// reservation through `phase1_intent` too, so it inherits the same
    /// gate without any separate check of its own.
    #[test]
    fn qa14_device_claim_refuses_a_new_submission_while_restore_marker_present() {
        let (_dir, raw_conn) = open_temp_ledger_for_gate_tests();
        let conn = &raw_conn;
        crate::ledger::schema::record_marker(
            conn,
            crate::ledger::schema::RESTORE_RECONCILE_PENDING_MARKER,
        )
        .unwrap();
        let key = TaskKey::new(TaskKey::DEVICE_SCOPE_ID, "embedding", "hash-a", "profile-a");
        let err = match device_claim(conn, &key, 1.0, 300, 1_000_000.0, None) {
            Ok(outcome) => panic!("expected the restore gate to refuse, got {outcome:?}"),
            Err(err) => err,
        };
        assert!(
            err.to_string()
                .contains("KIO-E-BATCH-RESTORE-RECONCILE-001"),
            "got {err}"
        );
    }

    /// The "Reused arm stays allowed" contract (`kio-cli`'s
    /// `reserve_or_reuse_task_charge`: "an existing intent is not a
    /// 新規投入"): an already-open reservation made BEFORE a restore was
    /// detected remains fully readable via `get_batch_request` afterward —
    /// `get_batch_request` never consults the marker at all. This is
    /// exactly the mechanism that lets `reserve_or_reuse_task_charge` bypass
    /// `phase1_intent` (and therefore this gate) entirely when a live row
    /// already exists: it checks `get_batch_request` FIRST and only calls
    /// `phase1_intent` when no such row is found. A GENUINELY fresh
    /// `phase1_intent` call for the SAME key, by contrast, is still refused
    /// — by the pre-existing CLEANUP-PENDING precedent this time (the row's
    /// residue cleanup has not completed), confirming this key is not
    /// somehow exempt from gating in general.
    #[test]
    fn qa14_existing_open_row_stays_reachable_without_a_new_phase1_intent_call() {
        let (_dir, raw_conn) = open_temp_ledger_for_gate_tests();
        let conn = &raw_conn;
        let key = TaskKey::new("scope-a", "markdownize", "hash-a", "profile-a");
        let outcome = phase1_intent(conn, &key, RequestKind::Batch, 1.0, None).unwrap();

        crate::ledger::schema::record_marker(
            conn,
            crate::ledger::schema::RESTORE_RECONCILE_PENDING_MARKER,
        )
        .unwrap();

        let existing = get_batch_request(conn, &key).unwrap().unwrap();
        assert_eq!(
            existing.intent_token.as_deref(),
            Some(outcome.intent_token.as_str())
        );
        assert!(existing.state.is_inflight());

        let err = match phase1_intent(conn, &key, RequestKind::Batch, 1.0, None) {
            Ok(_) => panic!("a fresh phase1_intent call for this key must still be refused"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("KIO-E-BATCH-CLEANUP-PENDING-001"),
            "got {err}"
        );
    }

    #[test]
    fn sync_unknown_resend_requires_authorization_and_preserves_settled_accounting() {
        let (_dir, raw_conn) = open_temp_ledger_for_gate_tests();
        let conn = &raw_conn;
        let key = TaskKey::new("scope-a", "markdownize", "hash-a", "profile-a");
        let first = phase1_intent(conn, &key, RequestKind::Sync, 1.25, Some(300)).unwrap();
        recovery_settle_unknown(conn, &key, &first.intent_token, 1.25, true).unwrap();

        let settled = get_batch_request(conn, &key).unwrap().unwrap();
        assert_eq!(settled.state, BatchState::Terminal);
        assert_eq!(settled.error.as_deref(), Some("unknown_settled"));
        assert!(settled.intent_token.is_none());
        let settled_seq = settled.submission_seq;
        let charges_before = cost_ledger_rows_for_key(conn, &key).unwrap();
        assert_eq!(charges_before.len(), 1);
        assert_eq!(charges_before[0].usd, 1.25);
        assert!(charges_before[0].estimated);

        let denied = phase1_intent(conn, &key, RequestKind::Sync, 1.25, Some(300))
            .expect_err("unknown settlement must fail closed before authorization");
        assert!(
            denied.to_string().contains("KIO-E-SYNC-RESULT-UNKNOWN-001"),
            "got {denied}"
        );
        let cross_lane_denied = phase1_intent(conn, &key, RequestKind::Batch, 1.25, None)
            .expect_err("switching to batch must not evade unknown-result authorization");
        assert!(
            cross_lane_denied
                .to_string()
                .contains("KIO-E-SYNC-RESULT-UNKNOWN-001"),
            "got {cross_lane_denied}"
        );

        assert!(authorize_unknown_sync_resend(conn, &key).unwrap());
        let authorized = get_batch_request(conn, &key).unwrap().unwrap();
        assert_eq!(authorized.submission_seq, settled_seq);
        assert_eq!(
            authorized.error.as_deref(),
            Some("unknown_resend_authorized")
        );
        assert!(authorized.intent_token.is_none());
        assert_eq!(
            cost_ledger_rows_for_key(conn, &key).unwrap(),
            charges_before
        );

        assert!(
            !authorize_unknown_sync_resend(conn, &key).unwrap(),
            "the durable approval marker is idempotent"
        );
        assert_eq!(
            cost_ledger_rows_for_key(conn, &key).unwrap(),
            charges_before
        );

        let retry = phase1_intent(conn, &key, RequestKind::Batch, 1.25, None).unwrap();
        let fresh = get_batch_request(conn, &key).unwrap().unwrap();
        assert!(fresh.state.is_inflight());
        assert_eq!(fresh.request_kind, RequestKind::Batch);
        assert_eq!(
            fresh.intent_token.as_deref(),
            Some(retry.intent_token.as_str())
        );
        assert!(
            fresh.error.is_none(),
            "phase 1 consumes the approval marker"
        );
        assert_eq!(fresh.submission_seq, settled_seq + 1);
        assert_eq!(
            cost_ledger_rows_for_key(conn, &key).unwrap(),
            charges_before,
            "authorization and fresh reservation must not refund or duplicate the settled charge"
        );
    }

    #[test]
    fn sync_resend_authorization_refuses_missing_or_inflight_attempts() {
        let (_dir, raw_conn) = open_temp_ledger_for_gate_tests();
        let conn = &raw_conn;
        let missing = TaskKey::new("scope-missing", "markdownize", "hash-a", "profile-a");
        let missing_err = authorize_unknown_sync_resend(conn, &missing)
            .expect_err("missing attempts must not create an authorization marker");
        assert!(
            missing_err
                .to_string()
                .contains("KIO-E-SYNC-RESULT-UNKNOWN-001"),
            "got {missing_err}"
        );

        let key = TaskKey::new("scope-a", "markdownize", "hash-a", "profile-a");
        let phase1 = phase1_intent(conn, &key, RequestKind::Sync, 1.25, Some(300)).unwrap();
        let inflight_err = authorize_unknown_sync_resend(conn, &key)
            .expect_err("an in-flight attempt must be recovered, not authorized for resend");
        assert!(
            inflight_err
                .to_string()
                .contains("KIO-E-SYNC-RESULT-UNKNOWN-001"),
            "got {inflight_err}"
        );
        assert_eq!(
            get_batch_request(conn, &key)
                .unwrap()
                .unwrap()
                .intent_token
                .as_deref(),
            Some(phase1.intent_token.as_str()),
            "failed authorization must not clear the recovery token"
        );
        assert!(cost_ledger_rows_for_key(conn, &key).unwrap().is_empty());
    }

    #[test]
    fn sweep_plan_cannot_prune_old_task_unknown_result_authority() {
        let (_dir, raw_conn) = open_temp_ledger_for_gate_tests();
        let conn = &raw_conn;
        let key = TaskKey::new("scope-a", "markdownize", "hash-a", "profile-a");
        let phase1 = phase1_intent(conn, &key, RequestKind::Sync, 1.25, Some(300)).unwrap();
        recovery_settle_unknown(conn, &key, &phase1.intent_token, 1.25, true).unwrap();
        conn.execute(
            "UPDATE batch_requests SET completed_at = 0
             WHERE scope_id = ?1 AND adapter_kind = ?2 AND input_hash = ?3 AND tool_profile_hash = ?4",
            params![
                key.scope_id,
                key.adapter_kind,
                key.input_hash,
                key.tool_profile_hash
            ],
        )
        .unwrap();

        let report = execute_bounded_sweep(
            conn,
            &SweepPlan {
                own_key_stale: Vec::new(),
                general_stale: Vec::new(),
                prune: vec![key.clone()],
            },
            now_millis(),
        )
        .unwrap();
        assert!(report.pruned.is_empty());
        let row = get_batch_request(conn, &key).unwrap().unwrap();
        assert_eq!(row.error.as_deref(), Some("unknown_settled"));
        assert!(row.intent_token.is_none());
        let err = phase1_intent(conn, &key, RequestKind::Batch, 1.25, None)
            .expect_err("the preserved authority marker must still block lane switching");
        assert!(
            err.to_string().contains("KIO-E-SYNC-RESULT-UNKNOWN-001"),
            "got {err}"
        );
    }
}
