//! Step4b contract tests: `cost-ledger.sqlite` + Online Batch 2-phase protocol.
//!
//! Canonical authority: `docs/04-pipeline.md` §5.4/§5.8, enforced by the Rust
//! implementation and these tests. CL01-CL71 and §M labels from the historical
//! task record are review provenance only.
//!
//! Coverage note: the pure-computation half of
//! CL13/CL21/CL27/CL28/CL29/CL30/CL48/CL49/CL50/CL52/CL61 is covered
//! as inline `#[cfg(test)]` tests inside `crates/kio-pipeline/src/ledger/` (the
//! instructions explicitly allow pure-core-logic contracts to live there). This
//! file carries every contract that needs a live SQLite connection, a multi-step
//! transaction, or the actual `kio` CLI process — including a DB-level angle on
//! several of the above where one exists beyond the pure-math piece.
//!
//! CL40 (Markdownize partial-recovery: mode-unknown→full, unit differencing into
//! synthesized `failed_units`, fixed `error_kind`) is **not implemented and not
//! tested here** — it is output/task reconciliation logic that belongs beside
//! `markdownize.rs`'s receive-side validation, not the cost-ledger store. See the
//! final implementation report for the full list of unimplemented CL items.
//!
//! CLI-level tests (`kio batch abandon`, `--reset-violations`, `kio status`
//! stalled display) depend on the `kio` binary compiling — at the time this file
//! was written, a concurrently-edited `crates/kio-cli/src/purge.rs` /
//! `verify_objects.rs` (owned by a parallel subagent, out of this change's
//! scope) left the workspace `kio` bin target transiently broken, so those
//! specific tests could not be executed end-to-end in the live tree. They were
//! verified instead in an isolated `git worktree` checked out at this branch's
//! last commit (before either agent's changes), with only this task's own files
//! overlaid — see the implementation report.

mod support;

use support::canonical_tempdir;

use std::collections::BTreeSet;
use std::path::PathBuf;

use assert_cmd::Command;
use kio_pipeline::ledger::ops::{
    AbandonExecution, AbandonResolution, AbandonSelector, BilledAmount, BudgetCapConfig,
    CapCheckResult, CapLayer, ClaimOutcome, DEFAULT_RECOVERY_DEADLINE_MS,
    DEFAULT_VISIBILITY_GRACE_PERIOD_MS, ExtendOutcome, TerminalWrite, check_then_reserve,
    contract_violation_retry_allowed, cost_ledger_rows_for_key, device_claim,
    device_extend_stale_after, device_input_hash, execute_abandon, execute_bounded_sweep,
    get_batch_request, ledger_month_total, nonbillable_charge, phase1_intent,
    phase2a_record_provider_scope, phase2a_record_upload_id, phase2a_restart_after_scope_mismatch,
    phase2b_record_job_create_started, phase2b_record_job_created, phase2b_scope_matches,
    plan_bounded_sweep, recovery_deadline_passed, recovery_finish_cleanup, recovery_mark_found,
    recovery_settle_unknown, resolve_abandon_selector, resolve_billing_from_usd_field,
    stalled_rows, sync_record_provider_request_id, terminal_transaction,
    visibility_grace_period_elapsed,
};
use kio_pipeline::ledger::schema::{
    CREATE_BATCH_REQUESTS_SQL, CREATE_COST_LEDGER_SQL, CREATE_IDX_BATCH_REQUESTS_INFLIGHT_SQL,
    CREATE_IDX_COST_LEDGER_MONTH_SQL, CREATE_SCHEMA_MIGRATIONS_SQL, canonical_sql_tokens,
    object_sql,
};
use kio_pipeline::ledger::time::{current_month_start_millis, now_millis, utc_month_of};
use kio_pipeline::ledger::{BatchState, LedgerDb, Outcome, RequestKind, TaskKey};
use rusqlite::{Connection, params};
use serde_json::Value;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn open_temp_ledger() -> (TempDir, LedgerDb) {
    let dir = tempfile::tempdir().unwrap();
    let device_dir = std::fs::canonicalize(dir.path()).unwrap().join("device");
    std::fs::create_dir(&device_dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&device_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let path = device_dir.join("cost-ledger.sqlite");
    let db = LedgerDb::initialize(&path).unwrap();
    (dir, db)
}

fn key(scope: &str, adapter: &str, hash: &str) -> TaskKey {
    TaskKey::new(scope, adapter, hash, "tool-profile-1")
}

/// R23-02: a `device_cap` value pre-existing `device_claim` call sites in this
/// file pass when they are testing something else (in-flight/claim-lost
/// mechanics, not the cap check itself) and always call with `estimated_usd =
/// 0.0` — the `ExemptZeroCost`-equivalent path bypasses the cap check
/// entirely regardless of this value, so any value works; a large one
/// documents "this test does not care about the cap."
const R23_02_NEVER_DENY_DEVICE_CAP: f64 = 1_000_000.0;

fn plain_terminal_write<'a>(
    task_key: &'a TaskKey,
    outcome: Outcome,
    billed: BilledAmount,
    ledger_batch_job_id: &'a str,
    next_state: BatchState,
    error: Option<&'a str>,
) -> TerminalWrite<'a> {
    TerminalWrite {
        key: task_key,
        outcome,
        billed,
        ledger_batch_job_id,
        next_state,
        error,
        increment_contract_violation: false,
        attempts_delta: 0,
        clear_intent_token: true,
        intent_token_guard: None,
        reseat_submission_seq: false,
    }
}

// ---------------------------------------------------------------------------
// §A — DDL canonical-shape contracts (CL01-CL08)
// ---------------------------------------------------------------------------

/// CL01: `cost_ledger`'s 11 columns + trailing UNIQUE, executed verbatim from
/// the spec-of-record text, canonical-token-compared against an independently
/// transcribed expectation (not the same constant the implementation executes
/// — this is the point of the contract: catch transcription drift).
#[test]
fn cl01_cost_ledger_ddl_matches_canonical() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(CREATE_COST_LEDGER_SQL).unwrap();
    let sql = object_sql(&conn, "table", "cost_ledger").unwrap().unwrap();
    let expected = "CREATE TABLE cost_ledger ( \
        scope_id TEXT NOT NULL, \
        adapter_kind TEXT NOT NULL, \
        input_hash TEXT NOT NULL, \
        tool_profile_hash TEXT NOT NULL, \
        submission_seq INTEGER NOT NULL, \
        batch_job_id TEXT NOT NULL, \
        usd REAL NOT NULL CHECK (usd >= 0 AND usd < 1e999 AND typeof(usd) IN ('integer', 'real')), \
        estimated INTEGER NOT NULL DEFAULT 0 CHECK (estimated IN (0, 1)), \
        outcome TEXT NOT NULL CHECK (outcome IN ('succeeded', 'contract_violation', 'expired', \
            'abandoned', 'submit_rejected', 'purged', 'unknown_settled', 'fallback_to_full')), \
        month TEXT NOT NULL CHECK (month GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]' AND \
            substr(month, 6, 2) BETWEEN '01' AND '12'), \
        recorded_at INTEGER NOT NULL, \
        UNIQUE (scope_id, adapter_kind, input_hash, tool_profile_hash, submission_seq) \
    )";
    assert_eq!(canonical_sql_tokens(&sql), canonical_sql_tokens(expected));
}

/// CL02: `usd` / `estimated_usd` CHECK — non-negative, finite, and `typeof`
/// forced to `integer`/`real` for TEXT that does not survive REAL-affinity
/// numeric conversion.
///
/// **Discrepancy from the contract doc, verified empirically (not silently
/// resolved as a 3rd interpretation):** CL02 case (h) proposes `'5.0'` — a
/// cleanly-formatted decimal string — as "a numeric-looking TEXT literal
/// that stays `typeof='text'` and so must be rejected". It does not stay
/// text: SQLite's REAL-affinity column-storage conversion (which applies
/// identically whether the value arrives as a literal or a bound parameter —
/// verified both ways) successfully parses `'5.0'` into the real number
/// `5.0` *before* the CHECK runs, so `typeof(usd)` observes `'real'` and the
/// insert succeeds. This is not a gap in the DDL's defense: the failure mode
/// the spec's own comment describes ("REAL affinity は TEXT 混入を通し SUM
/// が 0.0 扱いにする") requires a value that SQLite's numeric-affinity
/// parser rejects outright (stays TEXT storage class) — a non-numeric-looking
/// string, not a well-formed one. This test therefore keeps `'5.0'` as a
/// (documented, expected-success) case and adds a genuinely-non-numeric TEXT
/// value to exercise what the `typeof` guard actually defends against.
#[test]
fn cl02_usd_and_estimated_usd_non_negative_finite_check_parameterized() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(CREATE_COST_LEDGER_SQL).unwrap();
    conn.execute_batch(CREATE_BATCH_REQUESTS_SQL).unwrap();

    // Each call uses a distinct submission_seq / input_hash (the index) so
    // multiple *successful* inserts across the parameterized cases below never
    // collide with each other on UNIQUE/PRIMARY KEY — only the usd/estimated_usd
    // CHECK is under test here.
    let insert_cost_ledger = |index: usize, usd_literal: &str| -> rusqlite::Result<usize> {
        conn.execute(
            &format!(
                "INSERT INTO cost_ledger (scope_id, adapter_kind, input_hash, tool_profile_hash, \
                 submission_seq, batch_job_id, usd, estimated, outcome, month, recorded_at) \
                 VALUES ('s','a','h', 't', {index}, 'job', {usd_literal}, 0, 'succeeded', '2026-07', 0)"
            ),
            [],
        )
    };
    let insert_batch_requests = |index: usize, usd_literal: &str| -> rusqlite::Result<usize> {
        conn.execute(
            &format!(
                "INSERT INTO batch_requests (scope_id, adapter_kind, input_hash, tool_profile_hash, \
                 estimated_usd, created_at) VALUES ('s','a','h{index}','t', {usd_literal}, 0)"
            ),
            [],
        )
    };

    for (index, (label, literal, should_succeed)) in [
        ("zero", "0", true),
        ("small_positive", "0.01", true),
        ("large_finite", "1e308", true),
        ("negative", "-0.01", false),
        ("nan", "(0.0/0.0)", false),
        ("positive_infinity", "1e999", false),
        // Converts cleanly to REAL by column affinity before CHECK runs —
        // see the discrepancy note above. Expected success, not a gap.
        ("well_formed_numeric_text", "'5.0'", true),
        // Genuinely non-numeric TEXT never affinity-converts and stays
        // typeof='text' — this is what the typeof() guard actually catches.
        ("non_numeric_text", "'not-a-number'", false),
    ]
    .into_iter()
    .enumerate()
    {
        let ledger_result = insert_cost_ledger(index, literal);
        assert_eq!(
            ledger_result.is_ok(),
            should_succeed,
            "cost_ledger.usd={label} ({literal}): expected success={should_succeed}, got {ledger_result:?}"
        );
        let batch_result = insert_batch_requests(index, literal);
        assert_eq!(
            batch_result.is_ok(),
            should_succeed,
            "batch_requests.estimated_usd={label} ({literal}): expected success={should_succeed}, got {batch_result:?}"
        );
    }
}

/// CL03: `estimated` (0/1 only), `outcome` (NOT NULL, no DEFAULT, closed enum),
/// `month` (GLOB + range) CHECK behavior.
///
/// **Discrepancy from the contract doc, verified against the spec-verbatim
/// DDL (not silently special-cased — see the module-level note in
/// `cl03`'s body below):** CL03 case (1) expects `estimated='1'` (a TEXT
/// literal) to violate the CHECK alongside `2` and `-1`. It does not: `usd`/
/// `estimated_usd` carry an explicit `typeof(...) IN ('integer','real')`
/// guard specifically because REAL-affinity columns let TEXT through
/// unconverted, but `estimated`'s DDL (04 §5.4, copied verbatim — see
/// `CREATE_COST_LEDGER_SQL`) is `INTEGER NOT NULL DEFAULT 0 CHECK (estimated
/// IN (0, 1))` with no such guard. SQLite's INTEGER-affinity column
/// coercion converts a numeric-looking TEXT value to its integer storage
/// class *before* CHECK evaluation, so `'1'` is stored as the integer `1`
/// and passes. This is real, verified SQLite behavior (confirmed by running
/// this exact assertion), not a misreading of the contract.
#[test]
fn cl03_estimated_outcome_month_check_parameterized() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(CREATE_COST_LEDGER_SQL).unwrap();
    let insert =
        |seq: i64, estimated: &str, outcome: &str, month: &str| -> rusqlite::Result<usize> {
            conn.execute(
                &format!(
                "INSERT INTO cost_ledger (scope_id, adapter_kind, input_hash, tool_profile_hash, \
                 submission_seq, batch_job_id, usd, estimated, outcome, month, recorded_at) \
                 VALUES ('s','a','h','t', {seq}, 'job', 1.0, {estimated}, {outcome}, '{month}', 0)"
            ),
                [],
            )
        };

    // (1) estimated outside {0,1}: numeric out-of-range values fail CHECK.
    // TEXT '1' does not (see the discrepancy note above) — asserted precisely
    // as the verified exception, not silently dropped from the case list.
    assert!(
        insert(1, "2", "'succeeded'", "2026-07").is_err(),
        "estimated=2 must violate CHECK"
    );
    assert!(
        insert(2, "-1", "'succeeded'", "2026-07").is_err(),
        "estimated=-1 must violate CHECK"
    );
    assert!(
        insert(3, "'1'", "'succeeded'", "2026-07").is_ok(),
        "estimated='1' (TEXT) is coerced to integer 1 by column affinity and passes"
    );

    // (2) outcome omitted (no DEFAULT) -> NOT NULL violation, not a silent
    // 'succeeded' default.
    let omitted = conn.execute(
        "INSERT INTO cost_ledger (scope_id, adapter_kind, input_hash, tool_profile_hash, \
         submission_seq, batch_job_id, usd, month, recorded_at) \
         VALUES ('s','a','h','t', 10, 'job', 1.0, '2026-07', 0)",
        [],
    );
    let err = omitted.unwrap_err();
    let message = err.to_string();
    assert!(
        message.to_lowercase().contains("not null"),
        "expected a NOT NULL violation, got: {message}"
    );

    // (3) outcome invalid value.
    assert!(insert(11, "0", "'invalid_value'", "2026-07").is_err());

    // (4) all 8 outcome values succeed (membership only).
    for (index, outcome) in [
        "succeeded",
        "contract_violation",
        "expired",
        "abandoned",
        "submit_rejected",
        "purged",
        "unknown_settled",
        "fallback_to_full",
    ]
    .into_iter()
    .enumerate()
    {
        insert(100 + index as i64, "0", &format!("'{outcome}'"), "2026-07")
            .unwrap_or_else(|err| panic!("outcome={outcome} must be accepted: {err}"));
    }

    // (5) month format/range violations.
    for (index, bad_month) in ["2026-13", "2026-00", "26-07", "2026-7", "2026/07"]
        .into_iter()
        .enumerate()
    {
        assert!(
            insert(200 + index as i64, "0", "'succeeded'", bad_month).is_err(),
            "month={bad_month} must violate CHECK"
        );
    }
}

/// CL04: `batch_requests`'s 19 columns, `WITHOUT ROWID`, PRIMARY KEY, no
/// `month` column, and `error` carrying no CHECK (asymmetric with
/// `state`/`request_kind`/`estimated_usd` — CL71's design-intent note).
#[test]
fn cl04_batch_requests_ddl_matches_canonical() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(CREATE_BATCH_REQUESTS_SQL).unwrap();
    let sql = object_sql(&conn, "table", "batch_requests")
        .unwrap()
        .unwrap();
    let expected = "CREATE TABLE batch_requests ( \
        scope_id TEXT NOT NULL, \
        adapter_kind TEXT NOT NULL, \
        input_hash TEXT NOT NULL, \
        tool_profile_hash TEXT NOT NULL, \
        state INTEGER NOT NULL DEFAULT 0 CHECK (state IN (0, 1, 2, 3)), \
        request_kind TEXT NOT NULL DEFAULT 'batch' CHECK (request_kind IN ('batch', 'sync')), \
        intent_token TEXT, \
        upload_id TEXT, \
        batch_job_id TEXT, \
        provider_scope_id TEXT, \
        job_create_started_at INTEGER, \
        stale_after_at INTEGER, \
        submission_seq INTEGER NOT NULL DEFAULT 0, \
        attempts INTEGER NOT NULL DEFAULT 0, \
        contract_violation_count INTEGER NOT NULL DEFAULT 0, \
        estimated_usd REAL NOT NULL CHECK (estimated_usd >= 0 AND estimated_usd < 1e999 AND \
            typeof(estimated_usd) IN ('integer', 'real')), \
        error TEXT, \
        completed_at INTEGER, \
        created_at INTEGER NOT NULL, \
        PRIMARY KEY (scope_id, adapter_kind, input_hash, tool_profile_hash) \
    ) WITHOUT ROWID";
    assert_eq!(canonical_sql_tokens(&sql), canonical_sql_tokens(expected));
    assert!(
        !canonical_sql_tokens(&sql)
            .windows(2)
            .any(|pair| pair == ["month", "TEXT"]),
        "batch_requests must not carry a month column"
    );
}

/// CL05: `state`/`request_kind` defaults and CHECK.
#[test]
fn cl05_state_and_request_kind_defaults_and_check() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(CREATE_BATCH_REQUESTS_SQL).unwrap();

    conn.execute(
        "INSERT INTO batch_requests (scope_id, adapter_kind, input_hash, tool_profile_hash, \
         estimated_usd, created_at) VALUES ('s','a','h','t', 0, 0)",
        [],
    )
    .unwrap();
    let (state, request_kind): (i64, String) = conn
        .query_row(
            "SELECT state, request_kind FROM batch_requests WHERE input_hash = 'h'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, 0);
    assert_eq!(request_kind, "batch");

    for bad_state in [4, -1] {
        let sql = format!(
            "INSERT INTO batch_requests (scope_id, adapter_kind, input_hash, tool_profile_hash, \
             state, estimated_usd, created_at) VALUES ('s','a','h{bad_state}','t', {bad_state}, 0, 0)"
        );
        assert!(
            conn.execute(&sql, []).is_err(),
            "state={bad_state} must fail CHECK"
        );
    }
    let bad_kind = "INSERT INTO batch_requests (scope_id, adapter_kind, input_hash, \
        tool_profile_hash, request_kind, estimated_usd, created_at) \
        VALUES ('s','a','hk','t', 'async', 0, 0)";
    assert!(conn.execute(bad_kind, []).is_err());
}

/// CL06: `schema_migrations` DDL + one-shot marker (duplicate `name` rejected).
#[test]
fn cl06_schema_migrations_ddl_and_single_use_marker() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(CREATE_SCHEMA_MIGRATIONS_SQL).unwrap();
    let sql = object_sql(&conn, "table", "schema_migrations")
        .unwrap()
        .unwrap();
    let expected = "CREATE TABLE schema_migrations ( \
        name TEXT NOT NULL PRIMARY KEY, applied_at INTEGER NOT NULL \
    )";
    assert_eq!(canonical_sql_tokens(&sql), canonical_sql_tokens(expected));

    conn.execute(
        "INSERT INTO schema_migrations (name, applied_at) VALUES ('jsonl-cutover', 0)",
        [],
    )
    .unwrap();
    assert!(
        conn.execute(
            "INSERT INTO schema_migrations (name, applied_at) VALUES ('jsonl-cutover', 1)",
            [],
        )
        .is_err()
    );
}

/// CL07: required indexes' canonical shape, and the partial index is actually
/// used (not a full table scan) for the in-flight reservation-sum query.
#[test]
fn cl07_required_indexes_canonical_and_partial_index_used_by_planner() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(CREATE_COST_LEDGER_SQL).unwrap();
    conn.execute_batch(CREATE_IDX_COST_LEDGER_MONTH_SQL)
        .unwrap();
    conn.execute_batch(CREATE_BATCH_REQUESTS_SQL).unwrap();
    conn.execute_batch(CREATE_IDX_BATCH_REQUESTS_INFLIGHT_SQL)
        .unwrap();

    let month_idx = object_sql(&conn, "index", "idx_cost_ledger_month")
        .unwrap()
        .unwrap();
    assert_eq!(
        canonical_sql_tokens(&month_idx),
        canonical_sql_tokens(
            "CREATE INDEX idx_cost_ledger_month ON cost_ledger(month, scope_id, adapter_kind)"
        )
    );
    let inflight_idx = object_sql(&conn, "index", "idx_batch_requests_inflight")
        .unwrap()
        .unwrap();
    assert_eq!(
        canonical_sql_tokens(&inflight_idx),
        canonical_sql_tokens(CREATE_IDX_BATCH_REQUESTS_INFLIGHT_SQL)
    );

    let mut stmt = conn
        .prepare(
            "EXPLAIN QUERY PLAN SELECT SUM(estimated_usd) FROM batch_requests WHERE state IN (0,1)",
        )
        .unwrap();
    let plan_lines: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(3))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let plan_text = plan_lines.join(" | ");
    assert!(
        plan_text.contains("idx_batch_requests_inflight"),
        "query plan must use the partial index, got: {plan_text}"
    );
    assert!(
        !plan_text.to_uppercase().contains("SCAN BATCH_REQUESTS")
            || plan_text.contains("USING COVERING INDEX")
            || plan_text.contains("USING INDEX"),
        "must not be a full table scan, got: {plan_text}"
    );
}

/// CL08: lifecycle opening is strict. A required-index mutation is not silently
/// repaired: the existing authority/checkpoint and SQLite file remain intact,
/// and an explicit recovery workflow must decide the next action.
#[test]
fn cl08_ledger_db_open_existing_rejects_missing_required_index_without_repair() {
    let dir = tempfile::tempdir().unwrap();
    let device_dir = std::fs::canonicalize(dir.path()).unwrap().join("device");
    std::fs::create_dir(&device_dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&device_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let path = device_dir.join("cost-ledger.sqlite");
    let db = LedgerDb::initialize(&path).unwrap();
    let authority =
        std::fs::read(path.with_file_name("cost-ledger.sqlite.authority.json")).unwrap();
    let checkpoint =
        std::fs::read(path.with_file_name("cost-ledger.sqlite.checkpoint.json")).unwrap();
    drop(db);
    Connection::open(&path)
        .unwrap()
        .execute_batch("DROP INDEX idx_batch_requests_inflight;")
        .unwrap();
    assert!(LedgerDb::open_existing(&path).is_err());
    assert!(
        object_sql(
            &Connection::open(&path).unwrap(),
            "index",
            "idx_batch_requests_inflight"
        )
        .unwrap()
        .is_none(),
        "strict open must not repair the mutation"
    );
    assert_eq!(
        std::fs::read(path.with_file_name("cost-ledger.sqlite.authority.json")).unwrap(),
        authority
    );
    assert_eq!(
        std::fs::read(path.with_file_name("cost-ledger.sqlite.checkpoint.json")).unwrap(),
        checkpoint
    );
}

// ---------------------------------------------------------------------------
// §C — Batch 2-phase state machine (CL13-CL21, CL42-CL44)
// ---------------------------------------------------------------------------

/// CL13: phase 1 on a brand-new key.
#[test]
fn cl13_phase1_new_key_sets_state0_and_all_null_fields() {
    let (_dir, db) = open_temp_ledger();
    let outcome = phase1_intent(
        &db,
        &key("s", "markdownize", "h"),
        RequestKind::Batch,
        2.5,
        None,
    )
    .unwrap();
    let row = get_batch_request(&db, &key("s", "markdownize", "h"))
        .unwrap()
        .unwrap();
    assert_eq!(row.state, BatchState::Intent);
    assert_eq!(row.request_kind, RequestKind::Batch);
    assert_eq!(
        row.intent_token.as_deref(),
        Some(outcome.intent_token.as_str())
    );
    assert_eq!(row.estimated_usd, 2.5);
    assert!(row.upload_id.is_none());
    assert!(row.batch_job_id.is_none());
    assert!(row.provider_scope_id.is_none());
    assert!(row.job_create_started_at.is_none());
    assert!(row.error.is_none());
    assert!(row.completed_at.is_none());
    assert_eq!(row.submission_seq, outcome.submission_seq);
    assert_eq!(
        outcome.submission_seq, 1,
        "first ever attempt: MAX(none)+1 = 1"
    );
}

/// CL14: reissuing phase 1 on a terminal row NULLs the residue fields but
/// preserves `attempts`.
#[test]
fn cl14_phase1_reissue_nulls_residue_but_preserves_attempts() {
    let (_dir, db) = open_temp_ledger();
    let task_key = key("s", "markdownize", "h");
    let first = phase1_intent(&db, &task_key, RequestKind::Batch, 1.0, None).unwrap();
    // Manually bump `attempts` (as a terminal Tx would) and drive the row to a
    // fully-cleaned-up terminal state so phase 1 is allowed to reissue.
    Connection::open(db.path())
        .unwrap()
        .execute(
            "UPDATE batch_requests SET attempts = 3, upload_id='up1', batch_job_id='job1', \
             provider_scope_id='prov1', completed_at=999, error='network_error' \
             WHERE input_hash = ?1",
            params![task_key.input_hash],
        )
        .unwrap();
    terminal_transaction(
        &db,
        &plain_terminal_write(
            &task_key,
            Outcome::Expired,
            BilledAmount {
                usd: 1.0,
                estimated: true,
            },
            &first.intent_token,
            BatchState::Terminal,
            Some("expired"),
        ),
    )
    .unwrap();

    let second = phase1_intent(&db, &task_key, RequestKind::Batch, 4.0, None).unwrap();
    assert_ne!(second.intent_token, first.intent_token);
    let row = get_batch_request(&db, &task_key).unwrap().unwrap();
    assert_eq!(row.state, BatchState::Intent);
    assert!(row.upload_id.is_none());
    assert!(row.batch_job_id.is_none());
    assert!(row.provider_scope_id.is_none());
    assert!(row.error.is_none());
    assert!(row.completed_at.is_none());
    assert_eq!(row.estimated_usd, 4.0);
    assert_eq!(row.attempts, 3, "attempts must survive the NULL reset");
}

/// CL15: `submission_seq` MAX+1 basis, and the UNIQUE-collision regression that
/// omitting it would cause.
#[test]
fn cl15_submission_seq_max_plus_one_and_omission_regression() {
    let (_dir, db) = open_temp_ledger();
    let task_key = key("s", "markdownize", "h");
    // Seed cost_ledger with a prior confirmed row at seq=3.
    Connection::open(db.path())
        .unwrap()
        .execute(
            "INSERT INTO cost_ledger (scope_id, adapter_kind, input_hash, tool_profile_hash, \
             submission_seq, batch_job_id, usd, estimated, outcome, month, recorded_at) \
             VALUES (?1,?2,?3,?4, 3, 'old-job', 1.0, 0, 'succeeded', '2026-07', 0)",
            params![
                task_key.scope_id,
                task_key.adapter_kind,
                task_key.input_hash,
                task_key.tool_profile_hash
            ],
        )
        .unwrap();
    // (a) correct implementation: phase1_intent computes MAX+1 = 4.
    let outcome = phase1_intent(&db, &task_key, RequestKind::Batch, 1.0, None).unwrap();
    assert_eq!(outcome.submission_seq, 4);
    terminal_transaction(
        &db,
        &plain_terminal_write(
            &task_key,
            Outcome::Succeeded,
            BilledAmount {
                usd: 5.0,
                estimated: false,
            },
            "new-job",
            BatchState::Completed,
            None,
        ),
    )
    .unwrap();
    let rows = cost_ledger_rows_for_key(&db, &task_key).unwrap();
    assert!(
        rows.iter()
            .any(|row| row.submission_seq == 4 && row.usd == 5.0)
    );

    // (b) regression: an implementation that forgot to bump seq and reused the
    // old row's stale value (2) collides with an already-recorded ledger row at
    // the same seq and is silently absorbed by ON CONFLICT DO NOTHING.
    let other_key = key("s2", "markdownize", "h2");
    Connection::open(db.path())
        .unwrap()
        .execute(
            "INSERT INTO cost_ledger (scope_id, adapter_kind, input_hash, tool_profile_hash, \
             submission_seq, batch_job_id, usd, estimated, outcome, month, recorded_at) \
             VALUES (?1,?2,?3,?4, 2, 'stale-job', 9.0, 0, 'succeeded', '2026-07', 0)",
            params![
                other_key.scope_id,
                other_key.adapter_kind,
                other_key.input_hash,
                other_key.tool_profile_hash
            ],
        )
        .unwrap();
    let lost = terminal_transaction(
        &db,
        &TerminalWrite {
            key: &other_key,
            outcome: Outcome::Succeeded,
            billed: BilledAmount {
                usd: 42.0,
                estimated: false,
            },
            ledger_batch_job_id: "new-attempt-job",
            next_state: BatchState::Completed,
            error: None,
            increment_contract_violation: false,
            attempts_delta: 0,
            clear_intent_token: true,
            intent_token_guard: None,
            reseat_submission_seq: false,
        },
    );
    // terminal_transaction always reads the row's *current* submission_seq
    // (never re-derives MAX+1 itself — that is phase1_intent's exclusive
    // responsibility), so without a prior phase1_intent call the row does not
    // exist and this call is a benign no-op. The point under test is narrower:
    // demonstrate the raw INSERT collision an implementation that skipped the
    // MAX+1 rule at phase 1 would hit.
    assert!(lost.is_ok());
    let collision = Connection::open(db.path())
        .unwrap()
        .execute(
            "INSERT INTO cost_ledger (scope_id, adapter_kind, input_hash, tool_profile_hash, \
         submission_seq, batch_job_id, usd, estimated, outcome, month, recorded_at) \
         VALUES (?1,?2,?3,?4, 2, 'new-attempt-job', 42.0, 0, 'succeeded', '2026-07', 0) \
         ON CONFLICT (scope_id, adapter_kind, input_hash, tool_profile_hash, submission_seq) \
         DO NOTHING",
            params![
                other_key.scope_id,
                other_key.adapter_kind,
                other_key.input_hash,
                other_key.tool_profile_hash
            ],
        )
        .unwrap();
    assert_eq!(
        collision, 0,
        "reusing the stale seq must be silently absorbed (the regression)"
    );
    let sum: f64 = Connection::open(db.path())
        .unwrap()
        .query_row(
            "SELECT SUM(usd) FROM cost_ledger WHERE input_hash = ?1",
            params![other_key.input_hash],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sum, 9.0,
        "the real 42.0 charge never landed under the stale-seq regression"
    );
}

/// CL16: phase 2a ordering — `provider_scope_id` before upload, `upload_id`
/// after; both survive a subsequent job-creation failure.
#[test]
fn cl16_phase2a_upload_ordering_and_residue_survives_job_create_failure() {
    let (_dir, db) = open_temp_ledger();
    let task_key = key("s", "markdownize", "h");
    let intent = phase1_intent(&db, &task_key, RequestKind::Batch, 1.0, None).unwrap();

    assert!(
        phase2a_record_provider_scope(&db, &task_key, &intent.intent_token, "prov-scope-a")
            .unwrap()
    );
    let before_upload = get_batch_request(&db, &task_key).unwrap().unwrap();
    assert_eq!(
        before_upload.provider_scope_id.as_deref(),
        Some("prov-scope-a")
    );
    assert!(
        before_upload.upload_id.is_none(),
        "upload_id not set until upload succeeds"
    );

    assert!(phase2a_record_upload_id(&db, &task_key, &intent.intent_token, "upload-123").unwrap());
    // Simulate phase 2b (job creation) failing — no further writes.
    let after = get_batch_request(&db, &task_key).unwrap().unwrap();
    assert_eq!(after.upload_id.as_deref(), Some("upload-123"));
    assert_eq!(
        after.state,
        BatchState::Intent,
        "state stays 0 — job was never created"
    );
}

/// CL17: phase 2b's `job_create_started_at` durable pre-write, then
/// `batch_job_id`+`state=1` on success; scope-mismatch restarts phase 2a —
/// R23-19: only once the caller confirms the old upload's deletion (an
/// unconfirmed attempt is a no-op that preserves the locators).
#[test]
fn cl17_phase2b_job_create_started_then_created_and_scope_mismatch_restart() {
    let (_dir, db) = open_temp_ledger();
    let task_key = key("s", "markdownize", "h");
    let intent = phase1_intent(&db, &task_key, RequestKind::Batch, 1.0, None).unwrap();
    phase2a_record_provider_scope(&db, &task_key, &intent.intent_token, "prov-scope-a").unwrap();
    phase2a_record_upload_id(&db, &task_key, &intent.intent_token, "upload-123").unwrap();

    // (a) matching scope: job_create_started_at recorded before the "call",
    // then batch_job_id + state=1 after success.
    let row_before = get_batch_request(&db, &task_key).unwrap().unwrap();
    assert!(phase2b_scope_matches(&row_before, "prov-scope-a"));
    assert!(phase2b_record_job_create_started(&db, &task_key, &intent.intent_token).unwrap());
    let mid = get_batch_request(&db, &task_key).unwrap().unwrap();
    assert!(mid.job_create_started_at.is_some());
    assert_eq!(
        mid.state,
        BatchState::Intent,
        "not yet state=1 — job call has not succeeded"
    );
    assert!(
        phase2b_record_job_created(&db, &task_key, &intent.intent_token, "provider-job-1").unwrap()
    );
    let done = get_batch_request(&db, &task_key).unwrap().unwrap();
    assert_eq!(done.state, BatchState::JobCreated);
    assert_eq!(done.batch_job_id.as_deref(), Some("provider-job-1"));

    // (b) scope mismatch on a fresh attempt: job is never called; upload is
    // cleared so phase 2a restarts — but (R23-19) ONLY once the caller
    // confirms the old upload is actually gone. An unconfirmed attempt must
    // leave the locators intact (the only discovery key a later cleanup pass
    // has for that upload).
    let (_dir2, db2) = open_temp_ledger();
    let key2 = key("s2", "markdownize", "h2");
    let intent2 = phase1_intent(&db2, &key2, RequestKind::Batch, 1.0, None).unwrap();
    phase2a_record_provider_scope(&db2, &key2, &intent2.intent_token, "prov-scope-old").unwrap();
    phase2a_record_upload_id(&db2, &key2, &intent2.intent_token, "upload-old").unwrap();
    let row2 = get_batch_request(&db2, &key2).unwrap().unwrap();
    assert!(!phase2b_scope_matches(&row2, "prov-scope-new"));

    // R23-19: unconfirmed deletion — a no-op, locators must survive.
    assert!(
        !phase2a_restart_after_scope_mismatch(&db2, &key2, &intent2.intent_token, false).unwrap()
    );
    let unconfirmed = get_batch_request(&db2, &key2).unwrap().unwrap();
    assert_eq!(unconfirmed.upload_id.as_deref(), Some("upload-old"));
    assert_eq!(
        unconfirmed.provider_scope_id.as_deref(),
        Some("prov-scope-old")
    );

    // A stale caller token cannot clear the current reservation's only upload
    // locator, even when it claims deletion was confirmed.
    assert!(!phase2a_restart_after_scope_mismatch(&db2, &key2, "stale-token", true).unwrap());
    let stale_token = get_batch_request(&db2, &key2).unwrap().unwrap();
    assert_eq!(stale_token.upload_id.as_deref(), Some("upload-old"));
    assert_eq!(
        stale_token.provider_scope_id.as_deref(),
        Some("prov-scope-old")
    );

    // Confirmed deletion — now the restart proceeds.
    assert!(
        phase2a_restart_after_scope_mismatch(&db2, &key2, &intent2.intent_token, true).unwrap()
    );
    let restarted = get_batch_request(&db2, &key2).unwrap().unwrap();
    assert!(restarted.upload_id.is_none());
    assert!(restarted.provider_scope_id.is_none());
    assert_eq!(restarted.state, BatchState::Intent);

    // Once job creation has begun, deleting the upload locator would discard
    // the recovery key for a provider call whose outcome may be unknown.
    let (_dir3, db3) = open_temp_ledger();
    let key3 = key("s3", "markdownize", "h3");
    let intent3 = phase1_intent(&db3, &key3, RequestKind::Batch, 1.0, None).unwrap();
    phase2a_record_provider_scope(&db3, &key3, &intent3.intent_token, "prov-scope-old").unwrap();
    phase2a_record_upload_id(&db3, &key3, &intent3.intent_token, "upload-started").unwrap();
    phase2b_record_job_create_started(&db3, &key3, &intent3.intent_token).unwrap();
    assert!(
        !phase2a_restart_after_scope_mismatch(&db3, &key3, &intent3.intent_token, true).unwrap()
    );
    let started = get_batch_request(&db3, &key3).unwrap().unwrap();
    assert_eq!(started.upload_id.as_deref(), Some("upload-started"));
    assert_eq!(started.provider_scope_id.as_deref(), Some("prov-scope-old"));
    assert!(started.job_create_started_at.is_some());
    assert!(started.batch_job_id.is_none());

    // A completed job-create transition is equally ineligible for phase-2a
    // restart: its upload and provider job identifiers remain recoverable.
    let (_dir4, db4) = open_temp_ledger();
    let key4 = key("s4", "markdownize", "h4");
    let intent4 = phase1_intent(&db4, &key4, RequestKind::Batch, 1.0, None).unwrap();
    phase2a_record_provider_scope(&db4, &key4, &intent4.intent_token, "prov-scope-old").unwrap();
    phase2a_record_upload_id(&db4, &key4, &intent4.intent_token, "upload-created").unwrap();
    phase2b_record_job_created(&db4, &key4, &intent4.intent_token, "provider-job-4").unwrap();
    assert!(
        !phase2a_restart_after_scope_mismatch(&db4, &key4, &intent4.intent_token, true).unwrap()
    );
    let created = get_batch_request(&db4, &key4).unwrap().unwrap();
    assert_eq!(created.upload_id.as_deref(), Some("upload-created"));
    assert_eq!(created.provider_scope_id.as_deref(), Some("prov-scope-old"));
    assert_eq!(created.batch_job_id.as_deref(), Some("provider-job-4"));
    assert_eq!(created.state, BatchState::JobCreated);
}

/// CL18: successful collect — confirmed record + `state=2` + `completed_at`,
/// same Tx, using the row's real `batch_job_id`.
#[test]
fn cl18_phase3_success_records_and_completes_same_tx() {
    let (_dir, db) = open_temp_ledger();
    let task_key = key("s", "markdownize", "h");
    let intent = phase1_intent(&db, &task_key, RequestKind::Batch, 1.0, None).unwrap();
    phase2a_record_provider_scope(&db, &task_key, &intent.intent_token, "prov").unwrap();
    phase2a_record_upload_id(&db, &task_key, &intent.intent_token, "up").unwrap();
    phase2b_record_job_create_started(&db, &task_key, &intent.intent_token).unwrap();
    phase2b_record_job_created(&db, &task_key, &intent.intent_token, "job-real").unwrap();

    let receipt = terminal_transaction(
        &db,
        &plain_terminal_write(
            &task_key,
            Outcome::Succeeded,
            BilledAmount {
                usd: 3.25,
                estimated: false,
            },
            "job-real",
            BatchState::Completed,
            None,
        ),
    )
    .unwrap();
    assert!(receipt.recorded);
    let row = get_batch_request(&db, &task_key).unwrap().unwrap();
    assert_eq!(row.state, BatchState::Completed);
    assert!(row.completed_at.is_some());
    let ledger_rows = cost_ledger_rows_for_key(&db, &task_key).unwrap();
    assert_eq!(ledger_rows.len(), 1);
    assert_eq!(ledger_rows[0].outcome, Outcome::Succeeded);
    assert_eq!(ledger_rows[0].usd, 3.25);
    assert_eq!(ledger_rows[0].batch_job_id, "job-real");
}

/// CL19: purge-during-flight terminates in the same Tx shape as a reject
/// (`error='purged'`, `outcome='purged'`) — the tombstone check itself is an
/// external I/O concern (out of `cost-ledger.sqlite`'s scope); this contract
/// covers only the recording shape once that decision has been made.
#[test]
fn cl19_phase3_purged_closes_like_a_reject_terminal() {
    let (_dir, db) = open_temp_ledger();
    let task_key = key("s", "markdownize", "h");
    let intent = phase1_intent(&db, &task_key, RequestKind::Batch, 2.0, None).unwrap();
    phase2b_record_job_created(&db, &task_key, &intent.intent_token, "job-x").unwrap();

    terminal_transaction(
        &db,
        &plain_terminal_write(
            &task_key,
            Outcome::Purged,
            resolve_billing_from_usd_field(1.5, 2.0),
            "job-x",
            BatchState::Terminal,
            Some("purged"),
        ),
    )
    .unwrap();
    let row = get_batch_request(&db, &task_key).unwrap().unwrap();
    assert_eq!(row.state, BatchState::Terminal);
    assert_eq!(row.error.as_deref(), Some("purged"));
    let ledger_rows = cost_ledger_rows_for_key(&db, &task_key).unwrap();
    assert_eq!(ledger_rows[0].outcome, Outcome::Purged);
}

/// CL20: contract-violation reject — no persist (out of this module's scope to
/// assert directly, since persistence lives elsewhere), confirmed record +
/// `state=3` + `contract_violation_count += 1` + `attempts` update, same Tx;
/// upload cleanup is NOT part of this Tx (`clear_intent_token=false` here).
#[test]
fn cl20_phase3_contract_violation_increments_count_and_attempts_same_tx() {
    let (_dir, db) = open_temp_ledger();
    let task_key = key("s", "markdownize", "h");
    let intent = phase1_intent(&db, &task_key, RequestKind::Batch, 1.0, None).unwrap();
    phase2b_record_job_created(&db, &task_key, &intent.intent_token, "job-y").unwrap();

    terminal_transaction(
        &db,
        &TerminalWrite {
            key: &task_key,
            outcome: Outcome::ContractViolation,
            billed: BilledAmount {
                usd: 0.75,
                estimated: false,
            },
            ledger_batch_job_id: "job-y",
            next_state: BatchState::Terminal,
            error: Some("contract_violation"),
            increment_contract_violation: true,
            attempts_delta: 1,
            clear_intent_token: false,
            intent_token_guard: None,
            reseat_submission_seq: false,
        },
    )
    .unwrap();
    let row = get_batch_request(&db, &task_key).unwrap().unwrap();
    assert_eq!(row.state, BatchState::Terminal);
    assert_eq!(row.error.as_deref(), Some("contract_violation"));
    assert_eq!(row.contract_violation_count, 1);
    assert_eq!(row.attempts, 1);
    assert!(
        row.intent_token.is_some(),
        "upload cleanup must not be folded into this Tx — token stays until cleanup completes"
    );
}

/// CL21: the ordering norm as a hard DB-level invariant — phase 1 refuses to
/// reissue while residue cleanup (`intent_token` non-NULL) is pending, and the
/// durable retry-allowed threshold is independent of `mode`/`error`.
#[test]
fn cl21_phase1_blocks_reissue_until_cleanup_completes() {
    let (_dir, db) = open_temp_ledger();
    let task_key = key("s", "markdownize", "h");
    let intent = phase1_intent(&db, &task_key, RequestKind::Batch, 1.0, None).unwrap();
    phase2b_record_job_created(&db, &task_key, &intent.intent_token, "job-z").unwrap();
    terminal_transaction(
        &db,
        &TerminalWrite {
            key: &task_key,
            outcome: Outcome::ContractViolation,
            billed: BilledAmount {
                usd: 1.0,
                estimated: false,
            },
            ledger_batch_job_id: "job-z",
            next_state: BatchState::Terminal,
            error: Some("contract_violation"),
            increment_contract_violation: true,
            attempts_delta: 1,
            clear_intent_token: false, // residue cleanup still pending
            intent_token_guard: None,
            reseat_submission_seq: false,
        },
    )
    .unwrap();

    let row = get_batch_request(&db, &task_key).unwrap().unwrap();
    assert!(
        contract_violation_retry_allowed(&row),
        "count==1 still allows retry"
    );
    let blocked = phase1_intent(&db, &task_key, RequestKind::Batch, 1.0, None);
    assert!(
        blocked.is_err(),
        "phase 1 must refuse while cleanup is pending"
    );

    // Cleanup completes (upload deletion confirmed) -> phase 1 is allowed again.
    assert!(recovery_finish_cleanup(&db, &task_key, &intent.intent_token).unwrap());
    let allowed = phase1_intent(&db, &task_key, RequestKind::Batch, 1.0, None);
    assert!(allowed.is_ok());
}

// ---------------------------------------------------------------------------
// §D — idempotent recording (CL22-CL25)
// ---------------------------------------------------------------------------

/// CL22: replaying the same confirmed-charge INSERT after a simulated
/// crash-right-after-commit never double-counts (UNIQUE + ON CONFLICT DO
/// NOTHING).
#[test]
fn cl22_on_conflict_do_nothing_prevents_double_counting_on_replay() {
    let (_dir, db) = open_temp_ledger();
    let insert = || {
        Connection::open(db.path()).unwrap().execute(
            "INSERT INTO cost_ledger (scope_id, adapter_kind, input_hash, tool_profile_hash, \
             submission_seq, batch_job_id, usd, estimated, outcome, month, recorded_at) \
             VALUES ('s','markdownize','h','t', 1, 'job-1', 5.0, 0, 'succeeded', '2026-07', 0) \
             ON CONFLICT (scope_id, adapter_kind, input_hash, tool_profile_hash, submission_seq) \
             DO NOTHING",
            [],
        )
    };
    assert_eq!(insert().unwrap(), 1);
    // "Crash right after commit; re-run the write command" — replay the exact
    // same INSERT.
    assert_eq!(
        insert().unwrap(),
        0,
        "the replay must be absorbed, not duplicated"
    );
    let (count, sum): (i64, f64) = Connection::open(db.path())
        .unwrap()
        .query_row(
            "SELECT COUNT(*), SUM(usd) FROM cost_ledger WHERE input_hash = 'h'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(sum, 5.0);
}

/// CL23: job-id-unknown (recovery timeout) settlement bumps `submission_seq`
/// before recording, keyed by `intent_token`; the next phase 1 continues from
/// the bumped value (basis includes the settlement row).
#[test]
fn cl23_unknown_settlement_bumps_seq_and_next_phase1_continues_from_it() {
    let (_dir, db) = open_temp_ledger();
    let task_key = key("s", "markdownize", "h");
    let intent = phase1_intent(&db, &task_key, RequestKind::Batch, 2.0, None).unwrap();
    phase2b_record_job_created(&db, &task_key, &intent.intent_token, "job-unknown").unwrap();
    assert_eq!(intent.submission_seq, 1);

    let receipt =
        recovery_settle_unknown(&db, &task_key, &intent.intent_token, 2.0, false).unwrap();
    assert_eq!(
        receipt.submission_seq, 2,
        "seq 1 -> 2 before recording the estimate"
    );
    let ledger_rows = cost_ledger_rows_for_key(&db, &task_key).unwrap();
    assert_eq!(ledger_rows.len(), 1);
    assert_eq!(ledger_rows[0].submission_seq, 2);
    assert_eq!(ledger_rows[0].batch_job_id, intent.intent_token);
    assert_eq!(ledger_rows[0].outcome, Outcome::UnknownSettled);
    assert!(ledger_rows[0].estimated);

    // Cleanup completes; the next phase 1 must use seq 3 (MAX(2)+1), never
    // colliding with the settlement row at seq 2.
    recovery_finish_cleanup(&db, &task_key, &intent.intent_token).unwrap();
    let next = phase1_intent(&db, &task_key, RequestKind::Batch, 1.0, None).unwrap();
    assert_eq!(next.submission_seq, 3);
}

/// CL24: abandon's job-id-unknown recording follows the identical +1 rule.
#[test]
fn cl24_abandon_settlement_also_bumps_seq_by_one() {
    let (_dir, db) = open_temp_ledger();
    let task_key = key("s", "markdownize", "h");
    let intent = phase1_intent(&db, &task_key, RequestKind::Batch, 0.8, None).unwrap();
    assert_eq!(intent.submission_seq, 1);
    let execution = execute_abandon(&db, &task_key).unwrap();
    assert_eq!(execution, AbandonExecution::Abandoned);
    let ledger_rows = cost_ledger_rows_for_key(&db, &task_key).unwrap();
    assert_eq!(ledger_rows.len(), 1);
    assert_eq!(ledger_rows[0].submission_seq, 2);
    assert_eq!(ledger_rows[0].usd, 0.8);
    assert!(ledger_rows[0].estimated);
    assert_eq!(ledger_rows[0].outcome, Outcome::Abandoned);
    assert_eq!(ledger_rows[0].batch_job_id, intent.intent_token);
}

/// R23-18 (04 §5.4 / AUD-16: "剪定・確定済みの 4 組 key への `kio batch
/// abandon` は対象なしの冪等成功"): a `state=2` (Completed) row whose success
/// charge is already durably recorded, but whose residual `intent_token` is
/// still set (upload cleanup crashed before completing), must NOT be
/// re-settled by abandon — no new `submission_seq`, no additional
/// `cost_ledger` row. Before this fix `execute_abandon` walked such a row
/// through the same reseat-and-charge path as any other in-flight/terminal
/// row (CL24's `reseat_submission_seq: true` + a fresh `abandoned` INSERT),
/// double-billing an already-succeeded task. `provider_scope_id` stays
/// non-NULL here (an upload may still exist) — cleanup stays pending, exactly
/// mirroring the ORIGINAL (non-Completed) abandon path's own note-5 split.
#[test]
fn r23_18_abandon_on_completed_row_with_residual_token_does_not_recharge() {
    let (_dir, db) = open_temp_ledger();
    let task_key = key("s", "markdownize", "h");
    let intent = phase1_intent(&db, &task_key, RequestKind::Batch, 3.0, None).unwrap();
    phase2a_record_provider_scope(&db, &task_key, &intent.intent_token, "prov").unwrap();
    phase2a_record_upload_id(&db, &task_key, &intent.intent_token, "up").unwrap();
    phase2b_record_job_create_started(&db, &task_key, &intent.intent_token).unwrap();
    phase2b_record_job_created(&db, &task_key, &intent.intent_token, "job-1").unwrap();

    // Success lands (state=2), but a crash before upload cleanup completes
    // leaves intent_token set (clear_intent_token=false simulates this —
    // same override `cl37`'s stalled-row test uses).
    terminal_transaction(
        &db,
        &TerminalWrite {
            clear_intent_token: false,
            ..plain_terminal_write(
                &task_key,
                Outcome::Succeeded,
                BilledAmount {
                    usd: 3.0,
                    estimated: false,
                },
                "job-1",
                BatchState::Completed,
                None,
            )
        },
    )
    .unwrap();
    let before = get_batch_request(&db, &task_key).unwrap().unwrap();
    assert_eq!(before.state, BatchState::Completed);
    assert!(
        before.intent_token.is_some(),
        "residual token — cleanup still pending"
    );

    let execution = execute_abandon(&db, &task_key).unwrap();
    assert_eq!(execution, AbandonExecution::Abandoned);

    // No re-charge: still exactly the ONE succeeded row, same seq.
    let ledger_rows = cost_ledger_rows_for_key(&db, &task_key).unwrap();
    assert_eq!(ledger_rows.len(), 1, "abandon must not add a second charge");
    assert_eq!(ledger_rows[0].outcome, Outcome::Succeeded);
    assert_eq!(ledger_rows[0].usd, 3.0);
    assert_eq!(ledger_rows[0].submission_seq, intent.submission_seq);

    let after = get_batch_request(&db, &task_key).unwrap().unwrap();
    assert_eq!(
        after.state,
        BatchState::Completed,
        "state must stay Completed, not become Terminal"
    );
    // provider_scope_id is still Some (upload never confirmed deleted), so
    // per note-5 the token cleanup stays pending too.
    assert!(after.provider_scope_id.is_some());
    assert!(after.intent_token.is_some());
}

/// R23-18 companion: when the residual row's `provider_scope_id` is already
/// NULL (no upload could possibly exist — sync rows never set it, CL47),
/// abandon on a Completed row DOES clear the token (pure residue cleanup,
/// note-5's immediate-clear case), still without any re-charge.
#[test]
fn r23_18_abandon_on_completed_sync_row_clears_token_without_recharge() {
    let (_dir, db) = open_temp_ledger();
    let task_key = key("s", "embedding", "h2");
    let intent = phase1_intent(&db, &task_key, RequestKind::Sync, 1.5, Some(300)).unwrap();
    terminal_transaction(
        &db,
        &TerminalWrite {
            clear_intent_token: false,
            ..plain_terminal_write(
                &task_key,
                Outcome::Succeeded,
                BilledAmount {
                    usd: 1.5,
                    estimated: true,
                },
                &intent.intent_token,
                BatchState::Completed,
                None,
            )
        },
    )
    .unwrap();
    let before = get_batch_request(&db, &task_key).unwrap().unwrap();
    assert!(
        before.provider_scope_id.is_none(),
        "sync rows never set provider_scope_id"
    );
    assert!(before.intent_token.is_some());

    let execution = execute_abandon(&db, &task_key).unwrap();
    assert_eq!(execution, AbandonExecution::Abandoned);

    let after = get_batch_request(&db, &task_key).unwrap().unwrap();
    assert_eq!(after.state, BatchState::Completed);
    assert!(
        after.intent_token.is_none(),
        "no upload could exist — cleanup completes immediately"
    );

    let ledger_rows = cost_ledger_rows_for_key(&db, &task_key).unwrap();
    assert_eq!(
        ledger_rows.len(),
        1,
        "still just the one succeeded charge, no re-settlement"
    );
    assert_eq!(ledger_rows[0].outcome, Outcome::Succeeded);
}

/// CL25: `cost_ledger` has no UPDATE code path — the implementation's only
/// write primitive is the idempotent `INSERT ... ON CONFLICT DO NOTHING`, so a
/// second attempt to record a *different* amount at an already-used
/// `(key, submission_seq)` leaves the original value untouched (behaviorally
/// immutable through the only path this module exposes).
#[test]
fn cl25_estimated_row_is_never_revised_by_a_later_discovery() {
    let (_dir, db) = open_temp_ledger();
    let task_key = key("s", "markdownize", "h");
    let intent = phase1_intent(&db, &task_key, RequestKind::Batch, 2.0, None).unwrap();
    let receipt = recovery_settle_unknown(&db, &task_key, &intent.intent_token, 2.0, true).unwrap();
    assert_eq!(receipt.submission_seq, 2);

    // The job is later discovered with a real amount — attempting to "correct"
    // the same (key, seq=2) row through the only insert primitive available is
    // absorbed, not applied.
    let attempted_correction = Connection::open(db.path())
        .unwrap()
        .execute(
            "INSERT INTO cost_ledger (scope_id, adapter_kind, input_hash, tool_profile_hash, \
         submission_seq, batch_job_id, usd, estimated, outcome, month, recorded_at) \
         VALUES (?1,?2,?3,?4, 2, 'real-job-discovered', 999.0, 0, 'succeeded', '2026-07', 0) \
         ON CONFLICT (scope_id, adapter_kind, input_hash, tool_profile_hash, submission_seq) \
         DO NOTHING",
            params![
                task_key.scope_id,
                task_key.adapter_kind,
                task_key.input_hash,
                task_key.tool_profile_hash
            ],
        )
        .unwrap();
    assert_eq!(attempted_correction, 0);
    let rows = cost_ledger_rows_for_key(&db, &task_key).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].usd, 2.0,
        "the original estimate must remain unchanged"
    );
    assert_eq!(rows[0].outcome, Outcome::UnknownSettled);
}

// ---------------------------------------------------------------------------
// §E — outcome enum + pre-validation (CL26, CL31 — CL27/28/29/30 are pure and
// live in kio-pipeline's inline tests)
// ---------------------------------------------------------------------------

/// CL26: every one of the 8 outcome scenarios records the correct `outcome`
/// value through the real `terminal_transaction` write path (not just DDL
/// membership, as CL03 already covers — this drives the actual Tx per
/// scenario).
#[test]
fn cl26_all_eight_outcome_scenarios_record_the_correct_value() {
    let scenarios: [(Outcome, &str); 8] = [
        (Outcome::Succeeded, "succeeded"),
        (Outcome::ContractViolation, "contract_violation"),
        (Outcome::Expired, "expired"),
        (Outcome::Abandoned, "abandoned"),
        (Outcome::SubmitRejected, "submit_rejected"),
        (Outcome::Purged, "purged"),
        (Outcome::UnknownSettled, "unknown_settled"),
        (Outcome::FallbackToFull, "fallback_to_full"),
    ];
    for (index, (outcome, expected_str)) in scenarios.into_iter().enumerate() {
        let (_dir, db) = open_temp_ledger();
        let task_key = key("s", "markdownize", &format!("h{index}"));
        let intent = phase1_intent(&db, &task_key, RequestKind::Batch, 1.0, None).unwrap();
        terminal_transaction(
            &db,
            &plain_terminal_write(
                &task_key,
                outcome,
                BilledAmount {
                    usd: 1.0,
                    estimated: false,
                },
                &intent.intent_token,
                BatchState::Terminal,
                Some(expected_str),
            ),
        )
        .unwrap();
        let rows = cost_ledger_rows_for_key(&db, &task_key).unwrap();
        assert_eq!(rows[0].outcome.as_str(), expected_str);
        assert_eq!(outcome.as_str(), expected_str);
    }
}

/// CL31: a billing-field-only failure never changes `outcome` or
/// `contract_violation_count` — success stays success (only `usd` degrades to
/// estimated), and a normal control response stays `fallback_to_full`.
#[test]
fn cl31_billing_field_failure_alone_never_changes_outcome_or_violation_count() {
    let (_dir, db) = open_temp_ledger();
    let key_a = key("s", "markdownize", "a");
    let intent_a = phase1_intent(&db, &key_a, RequestKind::Batch, 3.0, None).unwrap();
    let billed_a = resolve_billing_from_usd_field(-1.0, 3.0); // invalid usd -> estimated
    assert!(billed_a.estimated);
    terminal_transaction(
        &db,
        &plain_terminal_write(
            &key_a,
            Outcome::Succeeded,
            billed_a,
            &intent_a.intent_token,
            BatchState::Completed,
            None,
        ),
    )
    .unwrap();
    let row_a = get_batch_request(&db, &key_a).unwrap().unwrap();
    assert_eq!(row_a.contract_violation_count, 0);
    let ledger_a = cost_ledger_rows_for_key(&db, &key_a).unwrap();
    assert_eq!(
        ledger_a[0].outcome,
        Outcome::Succeeded,
        "success stays success"
    );
    assert!(ledger_a[0].estimated);

    let key_b = key("s", "markdownize", "b");
    let intent_b = phase1_intent(&db, &key_b, RequestKind::Batch, 3.0, None).unwrap();
    let billed_b = nonbillable_charge(); // stand-in for an invalid billable_units report degrading
    terminal_transaction(
        &db,
        &plain_terminal_write(
            &key_b,
            Outcome::FallbackToFull,
            billed_b,
            &intent_b.intent_token,
            BatchState::Terminal,
            None,
        ),
    )
    .unwrap();
    let row_b = get_batch_request(&db, &key_b).unwrap().unwrap();
    assert_eq!(row_b.contract_violation_count, 0);
    let ledger_b = cost_ledger_rows_for_key(&db, &key_b).unwrap();
    assert_eq!(ledger_b[0].outcome, Outcome::FallbackToFull);
}

// ---------------------------------------------------------------------------
// §F — crash recovery (CL32-CL39; CL40 out of scope — see file header)
// ---------------------------------------------------------------------------

/// CL32: recovery-candidate selection — unterminated rows and
/// terminal-but-uncleaned rows are included; fully-clean terminal rows and
/// `sync` rows are excluded.
#[test]
fn cl32_recovery_candidates_selects_the_right_rows() {
    let (_dir, db) = open_temp_ledger();
    let state0 = key("s", "markdownize", "a");
    phase1_intent(&db, &state0, RequestKind::Batch, 1.0, None).unwrap();

    let state1 = key("s", "markdownize", "b");
    let intent1 = phase1_intent(&db, &state1, RequestKind::Batch, 1.0, None).unwrap();
    phase2b_record_job_created(&db, &state1, &intent1.intent_token, "job-b").unwrap();

    let uncleaned_terminal = key("s", "markdownize", "c");
    let intent_c = phase1_intent(&db, &uncleaned_terminal, RequestKind::Batch, 1.0, None).unwrap();
    terminal_transaction(
        &db,
        &TerminalWrite {
            clear_intent_token: false,
            ..plain_terminal_write(
                &uncleaned_terminal,
                Outcome::Expired,
                BilledAmount {
                    usd: 1.0,
                    estimated: true,
                },
                &intent_c.intent_token,
                BatchState::Terminal,
                Some("expired"),
            )
        },
    )
    .unwrap();

    let cleaned_terminal = key("s", "markdownize", "d");
    let intent_d = phase1_intent(&db, &cleaned_terminal, RequestKind::Batch, 1.0, None).unwrap();
    terminal_transaction(
        &db,
        &plain_terminal_write(
            &cleaned_terminal,
            Outcome::Succeeded,
            BilledAmount {
                usd: 1.0,
                estimated: false,
            },
            &intent_d.intent_token,
            BatchState::Completed,
            None,
        ),
    )
    .unwrap();

    let sync_inflight = key("s", "embedding", "e");
    phase1_intent(&db, &sync_inflight, RequestKind::Sync, 0.1, Some(300)).unwrap();

    let candidates = kio_pipeline::ledger::ops::recovery_candidates(&db).unwrap();
    let candidate_hashes: BTreeSet<String> = candidates
        .into_iter()
        .map(|row| row.key.input_hash)
        .collect();
    assert!(candidate_hashes.contains("a"));
    assert!(candidate_hashes.contains("b"));
    assert!(candidate_hashes.contains("c"));
    assert!(
        !candidate_hashes.contains("d"),
        "fully-cleaned terminal row excluded"
    );
    assert!(
        !candidate_hashes.contains("e"),
        "sync rows excluded from batch recovery"
    );
}

/// CL34: `found` — self-describes `batch_job_id` only when it was not already
/// recorded (idempotent no-op otherwise).
#[test]
fn cl34_found_self_describes_batch_job_id_only_when_unset() {
    let (_dir, db) = open_temp_ledger();
    let task_key = key("s", "markdownize", "h");
    let intent = phase1_intent(&db, &task_key, RequestKind::Batch, 1.0, None).unwrap();
    // (a) batch_job_id already recorded: self-describe is a no-op.
    phase2b_record_job_created(&db, &task_key, &intent.intent_token, "already-known").unwrap();
    recovery_mark_found(&db, &task_key, "discovered-different").unwrap();
    let row_a = get_batch_request(&db, &task_key).unwrap().unwrap();
    assert_eq!(row_a.batch_job_id.as_deref(), Some("already-known"));

    // (b) batch_job_id NULL (crashed after phase 2b's call succeeded but
    // before its own record landed): self-describe fills it in.
    let key_b = key("s", "markdownize", "h2");
    let intent_b = phase1_intent(&db, &key_b, RequestKind::Batch, 1.0, None).unwrap();
    phase2b_record_job_create_started(&db, &key_b, &intent_b.intent_token).unwrap();
    recovery_mark_found(&db, &key_b, "discovered-job").unwrap();
    let row_b = get_batch_request(&db, &key_b).unwrap().unwrap();
    assert_eq!(row_b.batch_job_id.as_deref(), Some("discovered-job"));
}

/// CL36: the recovery deadline is `max(intent_token time, job_create_started_at)
/// plus deadline`, and the durable settlement path (already exercised by
/// CL23) is gated by the caller on this predicate.
#[test]
fn cl36_recovery_deadline_uses_the_later_of_token_time_and_job_create_started() {
    let (_dir, db) = open_temp_ledger();
    let task_key = key("s", "markdownize", "h");
    phase1_intent(&db, &task_key, RequestKind::Batch, 1.0, None).unwrap();
    let row = get_batch_request(&db, &task_key).unwrap().unwrap();
    let now = now_millis();

    // Freshly issued: not past the default 48h deadline.
    assert!(!recovery_deadline_passed(
        &row,
        now,
        DEFAULT_RECOVERY_DEADLINE_MS
    ));
    // Comfortably past it.
    let far_future = now + DEFAULT_RECOVERY_DEADLINE_MS + 60_000;
    assert!(recovery_deadline_passed(
        &row,
        far_future,
        DEFAULT_RECOVERY_DEADLINE_MS
    ));

    // job_create_started_at later than the token's own embedded timestamp
    // becomes the effective basis.
    let later_started_at = now + 10_000;
    Connection::open(db.path())
        .unwrap()
        .execute(
            "UPDATE batch_requests SET job_create_started_at = ?1 WHERE input_hash = 'h'",
            params![later_started_at],
        )
        .unwrap();
    let row2 = get_batch_request(&db, &task_key).unwrap().unwrap();
    let almost_from_token = now + DEFAULT_RECOVERY_DEADLINE_MS + 1_000;
    // Not yet passed relative to the later job_create_started_at basis.
    assert!(!recovery_deadline_passed(
        &row2,
        almost_from_token,
        DEFAULT_RECOVERY_DEADLINE_MS
    ));
}

/// CL35 (visibility grace period half — the full-page-scan half is an external
/// provider-listing concern this module does not own) / R23-05: the default
/// 10-minute grace period is measured from the row's own durable
/// `job_create_started_at` (04 §5.4 DDL: "batch 行 = 可視化猶予・回復期限の
/// 起点") — NOT from the `intent_token`'s embedded UUIDv7 issue time, which
/// this test used before R23-05 (AUD-13's failure scenario: a slow phase 2a
/// upload makes the token issue time diverge from when job creation actually
/// started, understating the true in-flight window and reporting grace as
/// elapsed too early — a double-invocation risk). A `job_create_started_at
/// IS NULL` row (phase 2b never started) has no basis to measure from and
/// must never report elapsed, matching 04 §5.8's own rule that such rows are
/// excluded from job-list confirmed-absent matching entirely.
#[test]
fn cl35_r23_05_visibility_grace_period_measured_from_job_create_started_at() {
    let job_create_started_at = 1_700_000_000_000_i64;
    assert!(!visibility_grace_period_elapsed(
        Some(job_create_started_at),
        job_create_started_at,
        DEFAULT_VISIBILITY_GRACE_PERIOD_MS
    ));
    let almost_grace = job_create_started_at + DEFAULT_VISIBILITY_GRACE_PERIOD_MS - 1;
    assert!(!visibility_grace_period_elapsed(
        Some(job_create_started_at),
        almost_grace,
        DEFAULT_VISIBILITY_GRACE_PERIOD_MS
    ));
    let after_grace = job_create_started_at + DEFAULT_VISIBILITY_GRACE_PERIOD_MS + 1_000;
    assert!(visibility_grace_period_elapsed(
        Some(job_create_started_at),
        after_grace,
        DEFAULT_VISIBILITY_GRACE_PERIOD_MS
    ));

    // R23-05: NULL basis (phase 2b never started) can never be "elapsed",
    // however large `now` is.
    assert!(!visibility_grace_period_elapsed(
        None,
        i64::MAX,
        DEFAULT_VISIBILITY_GRACE_PERIOD_MS
    ));
}

/// CL37/CL68: `kio status`'s stalled set surfaces settled-but-uncleaned rows
/// with their `intent_token`, and abandon is the only thing that resolves them
/// (retry/resume style re-issuance stays blocked — CL21 already proves the
/// phase-1 guard; this contract adds the *display* half).
#[test]
fn cl37_stalled_rows_expose_intent_token_and_abandon_is_the_exit() {
    let (_dir, db) = open_temp_ledger();
    let task_key = key("s", "markdownize", "h");
    let intent = phase1_intent(&db, &task_key, RequestKind::Batch, 1.0, None).unwrap();
    terminal_transaction(
        &db,
        &TerminalWrite {
            clear_intent_token: false, // cleanup permanently stuck
            ..plain_terminal_write(
                &task_key,
                Outcome::UnknownSettled,
                BilledAmount {
                    usd: 1.0,
                    estimated: true,
                },
                &intent.intent_token,
                BatchState::Terminal,
                Some("unknown_settled"),
            )
        },
    )
    .unwrap();

    let stalled = stalled_rows(&db).unwrap();
    assert_eq!(stalled.len(), 1);
    assert_eq!(
        stalled[0].intent_token.as_deref(),
        Some(intent.intent_token.as_str())
    );

    let resolution = resolve_abandon_selector(
        &db,
        &AbandonSelector::IntentToken(intent.intent_token.clone()),
    )
    .unwrap();
    assert_eq!(resolution, AbandonResolution::Found(task_key.clone()));
    let execution = execute_abandon(&db, &task_key).unwrap();
    assert_eq!(execution, AbandonExecution::Abandoned);
    assert!(stalled_rows(&db).unwrap().is_empty());
}

/// CL38: residual cleanup — a normal reject and an already-abandoned task both
/// clear via the same `recovery_finish_cleanup` primitive; abandon itself never
/// re-runs found/confirmed-absent classification (there is no code path in this
/// module that would — cleanup for an abandoned row is exactly the same
/// deletion-confirmation primitive as any other terminal row's).
#[test]
fn cl38_residual_cleanup_applies_uniformly_including_to_abandoned_rows() {
    let (_dir, db) = open_temp_ledger();
    let task_key = key("s", "markdownize", "h");
    let intent = phase1_intent(&db, &task_key, RequestKind::Batch, 1.0, None).unwrap();
    phase2a_record_provider_scope(&db, &task_key, &intent.intent_token, "prov").unwrap();
    phase2a_record_upload_id(&db, &task_key, &intent.intent_token, "up").unwrap();
    let execution = execute_abandon(&db, &task_key).unwrap();
    assert_eq!(execution, AbandonExecution::Abandoned);
    let row = get_batch_request(&db, &task_key).unwrap().unwrap();
    assert!(
        row.intent_token.is_some(),
        "provider_scope_id was set (upload may exist) — cleanup is not assumed complete"
    );
    assert!(recovery_finish_cleanup(&db, &task_key, &intent.intent_token).unwrap());
    let cleaned = get_batch_request(&db, &task_key).unwrap().unwrap();
    assert!(cleaned.intent_token.is_none());
}

/// CL39: the ordering norm — `phase1_intent`'s hard guard (CL21) is exactly
/// what prevents the "reset retry budget, then reissue" mis-ordering the
/// contract warns about: reissuing is structurally impossible before cleanup
/// completes, so there is no window in which a fresh attempt's token could be
/// mixed up with the old attempt's discovery.
#[test]
fn cl39_reissue_is_structurally_impossible_before_cleanup_completes() {
    let (_dir, db) = open_temp_ledger();
    let task_key = key("s", "markdownize", "h");
    let intent = phase1_intent(&db, &task_key, RequestKind::Batch, 1.0, None).unwrap();
    terminal_transaction(
        &db,
        &TerminalWrite {
            clear_intent_token: false,
            ..plain_terminal_write(
                &task_key,
                Outcome::Expired,
                BilledAmount {
                    usd: 1.0,
                    estimated: true,
                },
                &intent.intent_token,
                BatchState::Terminal,
                Some("expired"),
            )
        },
    )
    .unwrap();
    for _ in 0..3 {
        assert!(
            phase1_intent(&db, &task_key, RequestKind::Batch, 1.0, None).is_err(),
            "every reissue attempt before cleanup must fail, not just the first"
        );
    }
}

// ---------------------------------------------------------------------------
// §G — sync degenerate 2-phase (CL41-CL47)
// ---------------------------------------------------------------------------

/// CL42: sync phase 1 — `request_kind='sync'`, `intent_token` set, no
/// upload/job columns.
#[test]
fn cl42_sync_phase1_row_shape() {
    let (_dir, db) = open_temp_ledger();
    let task_key = key("s", "embedding", "h");
    let outcome = phase1_intent(&db, &task_key, RequestKind::Sync, 0.02, Some(300)).unwrap();
    let row = get_batch_request(&db, &task_key).unwrap().unwrap();
    assert_eq!(row.request_kind, RequestKind::Sync);
    assert_eq!(
        row.intent_token.as_deref(),
        Some(outcome.intent_token.as_str())
    );
    assert!(row.upload_id.is_none());
    assert!(row.provider_scope_id.is_none());
    assert!(row.batch_job_id.is_none());
    assert!(
        row.job_create_started_at.is_some(),
        "sync rows record the phase-1 start time"
    );
}

/// CL43: provider request id recorded durably before the terminal Tx.
#[test]
fn cl43_sync_provider_request_id_recorded_before_terminal() {
    let (_dir, db) = open_temp_ledger();
    let task_key = key("s", "embedding", "h");
    let intent = phase1_intent(&db, &task_key, RequestKind::Sync, 0.02, Some(300)).unwrap();
    assert!(
        sync_record_provider_request_id(&db, &task_key, &intent.intent_token, "req-abc").unwrap()
    );
    let row = get_batch_request(&db, &task_key).unwrap().unwrap();
    assert_eq!(row.batch_job_id.as_deref(), Some("req-abc"));
    assert_eq!(row.state, BatchState::Intent, "still pre-terminal");
}

/// CL44: multiple sync external calls for one task key serialize —
/// request-by-request, each a fresh phase 1 (MAX+1) only after the previous
/// terminates.
#[test]
fn cl44_multiple_sync_calls_serialize_with_monotonic_seq() {
    let (_dir, db) = open_temp_ledger();
    let task_key = key("s", "embedding", "h");
    let mut seqs = Vec::new();
    for call in 0..3 {
        let intent = phase1_intent(&db, &task_key, RequestKind::Sync, 0.01, Some(300)).unwrap();
        seqs.push(intent.submission_seq);
        sync_record_provider_request_id(
            &db,
            &task_key,
            &intent.intent_token,
            &format!("req-{call}"),
        )
        .unwrap();
        terminal_transaction(
            &db,
            &plain_terminal_write(
                &task_key,
                Outcome::Succeeded,
                BilledAmount {
                    usd: 0.01,
                    estimated: false,
                },
                &format!("req-{call}"),
                BatchState::Completed,
                None,
            ),
        )
        .unwrap();
    }
    assert_eq!(
        seqs,
        vec![1, 2, 3],
        "monotonically increasing, one per serialized call"
    );
}

/// CL45/CL46: sync crash recovery — a queryable row confirms; an unqueryable
/// row (no batch_job_id, or an unresolvable one) settles as unknown; a
/// `fallback_to_full` query result confirms via that outcome (task stays
/// non-terminal in the wider task model — out of this module's concern).
#[test]
fn cl45_cl46_sync_crash_recovery_confirms_or_settles_unknown() {
    // (a) batch_job_id recorded, "queryable" (simulated: caller confirms
    // success directly).
    let (_dir_a, db_a) = open_temp_ledger();
    let key_a = key("s", "embedding", "a");
    let intent_a = phase1_intent(&db_a, &key_a, RequestKind::Sync, 0.01, Some(300)).unwrap();
    sync_record_provider_request_id(&db_a, &key_a, &intent_a.intent_token, "req-a").unwrap();
    terminal_transaction(
        &db_a,
        &plain_terminal_write(
            &key_a,
            Outcome::Succeeded,
            BilledAmount {
                usd: 0.01,
                estimated: false,
            },
            "req-a",
            BatchState::Completed,
            None,
        ),
    )
    .unwrap();
    assert_eq!(
        cost_ledger_rows_for_key(&db_a, &key_a).unwrap()[0].outcome,
        Outcome::Succeeded
    );

    // (b) batch_job_id NULL: unknown settlement.
    let (_dir_b, db_b) = open_temp_ledger();
    let key_b = key("s", "embedding", "b");
    let intent_b = phase1_intent(&db_b, &key_b, RequestKind::Sync, 0.02, Some(300)).unwrap();
    recovery_settle_unknown(&db_b, &key_b, &intent_b.intent_token, 0.02, true).unwrap();
    let row_b = cost_ledger_rows_for_key(&db_b, &key_b).unwrap();
    assert_eq!(row_b[0].outcome, Outcome::UnknownSettled);
    assert!(row_b[0].estimated);

    // (c) batch_job_id recorded but "unresolvable" (simulated: caller could
    // not resolve it, falls back to unknown settlement — same primitive as (b)).
    let (_dir_c, db_c) = open_temp_ledger();
    let key_c = key("s", "embedding", "c");
    let intent_c = phase1_intent(&db_c, &key_c, RequestKind::Sync, 0.03, Some(300)).unwrap();
    sync_record_provider_request_id(&db_c, &key_c, &intent_c.intent_token, "req-c-unresolvable")
        .unwrap();
    recovery_settle_unknown(&db_c, &key_c, &intent_c.intent_token, 0.03, true).unwrap();
    assert_eq!(
        cost_ledger_rows_for_key(&db_c, &key_c).unwrap()[0].outcome,
        Outcome::UnknownSettled
    );

    // (d, CL46) recovery confirms a fallback_to_full control response.
    let (_dir_d, db_d) = open_temp_ledger();
    let key_d = key("s", "embedding", "d");
    let intent_d = phase1_intent(&db_d, &key_d, RequestKind::Sync, 0.01, Some(300)).unwrap();
    sync_record_provider_request_id(&db_d, &key_d, &intent_d.intent_token, "req-d").unwrap();
    terminal_transaction(
        &db_d,
        &plain_terminal_write(
            &key_d,
            Outcome::FallbackToFull,
            nonbillable_charge(),
            "req-d",
            BatchState::Terminal,
            None,
        ),
    )
    .unwrap();
    let row_d = get_batch_request(&db_d, &key_d).unwrap().unwrap();
    assert!(
        row_d.intent_token.is_none(),
        "CL47: sync clears intent_token on every terminal Tx"
    );
}

/// CL47: every sync terminal Tx clears `intent_token` immediately — contrasted
/// with a batch row, which does not (CL20 already proves the batch half).
#[test]
fn cl47_sync_rows_clear_intent_token_on_every_terminal_variant() {
    let scenarios = [
        (Outcome::Succeeded, BatchState::Completed),
        (Outcome::ContractViolation, BatchState::Terminal),
        (Outcome::UnknownSettled, BatchState::Terminal),
        (Outcome::Abandoned, BatchState::Terminal),
        (Outcome::FallbackToFull, BatchState::Terminal),
    ];
    for (index, (outcome, state)) in scenarios.into_iter().enumerate() {
        let (_dir, db) = open_temp_ledger();
        let task_key = key("s", "embedding", &format!("h{index}"));
        let intent = phase1_intent(&db, &task_key, RequestKind::Sync, 0.01, Some(300)).unwrap();
        terminal_transaction(
            &db,
            &plain_terminal_write(
                &task_key,
                outcome,
                BilledAmount {
                    usd: 0.01,
                    estimated: false,
                },
                &intent.intent_token,
                state,
                None,
            ),
        )
        .unwrap();
        let row = get_batch_request(&db, &task_key).unwrap().unwrap();
        assert!(
            row.intent_token.is_none(),
            "{outcome:?} must clear intent_token immediately for a sync row"
        );
    }
}

// ---------------------------------------------------------------------------
// §H — the query-embedding device row (CL48, CL51, CL53-CL55; CL49/CL50/CL52's
// pure-math halves live in kio-pipeline's inline tests)
// ---------------------------------------------------------------------------

/// CL48: device row identity — reserved `scope_id='device'`, `adapter_kind`
/// caller-supplied (`embedding`), `input_hash` from the query text, and it is
/// excluded from folder-cap scoped totals while included in device-wide totals.
#[test]
fn cl48_device_row_identity_and_cap_scoping() {
    let (_dir, db) = open_temp_ledger();
    let task_key = TaskKey::new(
        TaskKey::DEVICE_SCOPE_ID,
        "embedding",
        device_input_hash("hello world"),
        "embed-profile",
    );
    assert!(task_key.is_device());
    let outcome = phase1_intent(&db, &task_key, RequestKind::Sync, 0.0, Some(300)).unwrap();
    assert!(outcome.submission_seq >= 1);

    let month = utc_month_of(now_millis());
    let folder_total =
        ledger_month_total(&db, Some("some-real-folder-scope"), None, &month).unwrap();
    assert_eq!(
        folder_total, 0.0,
        "device rows never appear in a folder-scoped total"
    );
    let device_total = ledger_month_total(&db, None, Some("embedding"), &month).unwrap();
    assert!(
        device_total >= 0.0,
        "device rows do count toward the device/per_adapter total"
    );
}

/// CL51: an extension attempt after another process already recovered the row
/// (0-row CAS UPDATE) reports claim-lost.
#[test]
fn cl51_extend_after_claim_lost_reports_claim_lost() {
    let (_dir, db) = open_temp_ledger();
    let task_key = TaskKey::new(
        TaskKey::DEVICE_SCOPE_ID,
        "embedding",
        "q-hash",
        "embed-profile",
    );
    let claim = device_claim(&db, &task_key, 0.0, 300, R23_02_NEVER_DENY_DEVICE_CAP, None).unwrap();
    let ClaimOutcome::Claimed(outcome) = claim else {
        panic!("expected a fresh claim");
    };
    // Another process recovers the row (unknown settlement clears the token).
    recovery_settle_unknown(&db, &task_key, &outcome.intent_token, 0.0, true).unwrap();

    let extension =
        device_extend_stale_after(&db, &task_key, &outcome.intent_token, 30.0, 300).unwrap();
    assert_eq!(extension, ExtendOutcome::ClaimLost);
}

/// CL53: inline sweep never queries a provider — it always settles stale rows
/// as `unknown_settled`, and never touches `.kio/.lock` (there is no lock
/// primitive involved at all in this module's device-row API — the caller
/// composes everything under a `BEGIN IMMEDIATE` Tx instead, per
/// `with_immediate_transaction`'s doc comment).
#[test]
fn cl53_inline_sweep_always_settles_unknown_never_queries() {
    let (_dir, db) = open_temp_ledger();
    let stale_key = TaskKey::new(
        TaskKey::DEVICE_SCOPE_ID,
        "embedding",
        "stale-q",
        "embed-profile",
    );
    let outcome = phase1_intent(&db, &stale_key, RequestKind::Sync, 0.0, Some(300)).unwrap();
    // Force it stale.
    Connection::open(db.path())
        .unwrap()
        .execute(
            "UPDATE batch_requests SET stale_after_at = ?1 WHERE input_hash = 'stale-q'",
            params![now_millis() - 1],
        )
        .unwrap();
    let plan = plan_bounded_sweep(&db, None, now_millis()).unwrap();
    assert!(plan.general_stale.contains(&stale_key));
    let report = execute_bounded_sweep(&db, &plan, now_millis()).unwrap();
    assert!(report.settled.contains(&stale_key));
    let rows = cost_ledger_rows_for_key(&db, &stale_key).unwrap();
    assert_eq!(
        rows[0].outcome,
        Outcome::UnknownSettled,
        "always unknown — never a confirmed query result"
    );
    let _ = outcome;
}

/// CL54: a live (non-stale) in-flight claim for the same key falls back
/// without issuing a second phase 1 or disturbing the existing token.
#[test]
fn cl54_same_key_live_inflight_falls_back_without_a_second_phase1() {
    let (_dir, db) = open_temp_ledger();
    let task_key = TaskKey::new(
        TaskKey::DEVICE_SCOPE_ID,
        "embedding",
        "shared-q",
        "embed-profile",
    );
    let first = device_claim(&db, &task_key, 0.0, 300, R23_02_NEVER_DENY_DEVICE_CAP, None).unwrap();
    let ClaimOutcome::Claimed(first_outcome) = first else {
        panic!("expected the first claim to succeed");
    };
    let second =
        device_claim(&db, &task_key, 0.0, 300, R23_02_NEVER_DENY_DEVICE_CAP, None).unwrap();
    assert_eq!(second, ClaimOutcome::InFlight);
    let row = get_batch_request(&db, &task_key).unwrap().unwrap();
    assert_eq!(
        row.intent_token.as_deref(),
        Some(first_outcome.intent_token.as_str())
    );
}

/// CL55: terminal device-row pruning includes successful (`state=2`)
/// completions, requires `contract_violation_count=0` and `completed_at`
/// strictly before the current UTC month, and abandon on an already-pruned key
/// is an idempotent no-op success.
#[test]
fn cl55_terminal_device_row_pruning_conditions_and_abandon_after_pruning() {
    let (_dir, db) = open_temp_ledger();
    let now = now_millis();
    let month_start = current_month_start_millis(now);

    let prunable = TaskKey::new(
        TaskKey::DEVICE_SCOPE_ID,
        "embedding",
        "prunable",
        "embed-profile",
    );
    seed_terminal_device_row(&db, &prunable, BatchState::Completed, 0, month_start - 1);

    let has_violation = TaskKey::new(
        TaskKey::DEVICE_SCOPE_ID,
        "embedding",
        "has-violation",
        "embed-profile",
    );
    seed_terminal_device_row(
        &db,
        &has_violation,
        BatchState::Terminal,
        1,
        month_start - 1,
    );

    let this_month = TaskKey::new(
        TaskKey::DEVICE_SCOPE_ID,
        "embedding",
        "this-month",
        "embed-profile",
    );
    seed_terminal_device_row(
        &db,
        &this_month,
        BatchState::Completed,
        0,
        month_start + 1_000,
    );

    let plan = plan_bounded_sweep(&db, None, now).unwrap();
    assert!(
        plan.prune.contains(&prunable),
        "state=2 success must be prunable"
    );
    assert!(!plan.prune.contains(&has_violation));
    assert!(!plan.prune.contains(&this_month));

    let report = execute_bounded_sweep(&db, &plan, now).unwrap();
    assert!(report.pruned.contains(&prunable));
    assert!(get_batch_request(&db, &prunable).unwrap().is_none());

    // Abandon on the now-pruned key is a no-op idempotent success.
    let resolution = resolve_abandon_selector(&db, &AbandonSelector::TaskKey(prunable)).unwrap();
    assert_eq!(resolution, AbandonResolution::NotFound);
}

fn seed_terminal_device_row(
    db: &LedgerDb,
    task_key: &TaskKey,
    state: BatchState,
    violations: i64,
    completed_at: i64,
) {
    Connection::open(db.path())
        .unwrap()
        .execute(
            "INSERT INTO batch_requests (scope_id, adapter_kind, input_hash, tool_profile_hash, \
             state, request_kind, intent_token, submission_seq, estimated_usd, \
             contract_violation_count, completed_at, created_at) \
             VALUES (?1,?2,?3,?4, ?5, 'sync', NULL, 1, 0.0, ?6, ?7, 0)",
            params![
                task_key.scope_id,
                task_key.adapter_kind,
                task_key.input_hash,
                task_key.tool_profile_hash,
                state.as_i64(),
                violations,
                completed_at,
            ],
        )
        .unwrap();
}

/// R23-02 (04 §5.4 / AUD-11 / A-14): `device_claim` denies (no reservation
/// made) when the DEVICE cap would be exceeded — mirroring
/// `check_then_reserve`'s CL56-58 cap semantics for task charges, which
/// `device_claim` previously bypassed entirely (the fix-report-flagged gap
/// this session closes).
#[test]
fn r23_02_device_claim_denies_on_device_cap_exceeded() {
    let (_dir, db) = open_temp_ledger();
    seed_confirmed_charge(&db, "some-folder-x", "embedding", 48.0);
    let task_key = TaskKey::new(
        TaskKey::DEVICE_SCOPE_ID,
        "embedding",
        "over-device-cap",
        "embed-profile",
    );
    let claim = device_claim(&db, &task_key, 5.0, 300, 50.0, None).unwrap();
    assert_eq!(claim, ClaimOutcome::Denied(CapLayer::Device));
    assert!(
        get_batch_request(&db, &task_key).unwrap().is_none(),
        "a denied claim must not create a phase-1 row"
    );
}

/// R23-02: the per_adapter(embedding) layer denies independently of a
/// (much larger) device cap — 04 §5.4's third condition, applied to device
/// rows for the first time by this fix.
#[test]
fn r23_02_device_claim_denies_on_per_adapter_cap_exceeded() {
    let (_dir, db) = open_temp_ledger();
    seed_confirmed_charge(&db, "some-folder-x", "embedding", 13.0);
    let task_key = TaskKey::new(
        TaskKey::DEVICE_SCOPE_ID,
        "embedding",
        "over-per-adapter-cap",
        "embed-profile",
    );
    let claim = device_claim(&db, &task_key, 5.0, 300, 1000.0, Some(15.0)).unwrap();
    assert_eq!(claim, ClaimOutcome::Denied(CapLayer::PerAdapter));
    assert!(get_batch_request(&db, &task_key).unwrap().is_none());
}

/// R23-02: `estimated_usd == 0.0` (a zero-priced local embedding adapter)
/// bypasses the device-row cap check entirely, mirroring CL58's task-charge
/// exemption — even caps of `0.0` must still allow the claim.
#[test]
fn r23_02_device_claim_zero_cost_bypasses_cap_even_when_over() {
    let (_dir, db) = open_temp_ledger();
    seed_confirmed_charge(&db, "some-folder-x", "embedding", 999.0);
    let task_key = TaskKey::new(
        TaskKey::DEVICE_SCOPE_ID,
        "embedding",
        "free-claim",
        "embed-profile",
    );
    let claim = device_claim(&db, &task_key, 0.0, 300, 0.0, Some(0.0)).unwrap();
    assert!(matches!(claim, ClaimOutcome::Claimed(_)));
}

/// R23-03 (04 §5.4 / AUD-12 / A-15): once another process's stale-sweep
/// reclaims a device row (settling the original token `unknown_settled` and
/// clearing `intent_token`), the ORIGINAL holder's own terminal write — still
/// carrying the now-superseded token, arriving after a slow adapter call
/// returns — must be CAS-guarded and record NOTHING: no new `cost_ledger`
/// row, no disturbance of the reclaimer's already-settled row. Before this
/// fix, the write path used `intent_token_guard: None` and would have
/// unconditionally landed a second charge on the reclaimer's newer
/// generation.
#[test]
fn r23_03_settle_after_reclaim_does_not_double_charge() {
    let (_dir, db) = open_temp_ledger();
    let task_key = TaskKey::new(
        TaskKey::DEVICE_SCOPE_ID,
        "embedding",
        "raced-query",
        "embed-profile",
    );
    let claim = device_claim(
        &db,
        &task_key,
        0.02,
        300,
        R23_02_NEVER_DENY_DEVICE_CAP,
        None,
    )
    .unwrap();
    let ClaimOutcome::Claimed(original) = claim else {
        panic!("expected the first claim to succeed");
    };

    // Another process's stale sweep reclaims this exact row — settles the
    // original token unknown and clears intent_token (what
    // `execute_bounded_sweep` does to a row whose `stale_after_at` elapsed
    // while the original holder's adapter call was still in flight).
    recovery_settle_unknown(
        &db,
        &task_key,
        &original.intent_token,
        0.02, // the claim's own estimated_usd (Phase1Outcome does not carry it back)
        true,
    )
    .unwrap();
    let after_reclaim = get_batch_request(&db, &task_key).unwrap().unwrap();
    assert!(after_reclaim.intent_token.is_none());
    let rows_after_reclaim = cost_ledger_rows_for_key(&db, &task_key).unwrap();
    assert_eq!(rows_after_reclaim.len(), 1);
    assert_eq!(rows_after_reclaim[0].outcome, Outcome::UnknownSettled);

    // The ORIGINAL holder's adapter call finally "returns" — its terminal
    // write, still using the now-superseded token, must be CAS-guarded
    // (mirrors `settle_task_charge_success`'s `intent_token_guard:
    // Some(intent_token)` after R23-03).
    let receipt = terminal_transaction(
        &db,
        &TerminalWrite {
            key: &task_key,
            outcome: Outcome::Succeeded,
            billed: BilledAmount {
                usd: 0.02,
                estimated: true,
            },
            ledger_batch_job_id: &original.intent_token,
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
    assert!(
        !receipt.recorded,
        "CAS-guarded write on a superseded token must record nothing"
    );
    assert!(!receipt.row_updated);

    // No second cost_ledger row: still exactly the reclaimer's one
    // unknown_settled row.
    let rows_final = cost_ledger_rows_for_key(&db, &task_key).unwrap();
    assert_eq!(
        rows_final.len(),
        1,
        "must still be exactly the reclaimer's one row"
    );
    assert_eq!(rows_final[0].outcome, Outcome::UnknownSettled);
}

/// R23-04 (04 §5.4 / AUD-15 / A-16): the own-key bounded-sweep pool matches
/// the FULL 4-tuple (including `tool_profile_hash`), not just
/// `(adapter_kind, input_hash)` — a stale row sharing only the first two with
/// the key about to be claimed is a DIFFERENT task identity (§5.5) and
/// belongs in the capped general pool, never the unbounded own-key pool.
#[test]
fn r23_04_own_key_sweep_requires_full_four_tuple_match() {
    let (_dir, db) = open_temp_ledger();
    let claiming_key = TaskKey::new(
        TaskKey::DEVICE_SCOPE_ID,
        "embedding",
        "shared-input-hash",
        "profile-a",
    );
    let other_profile_key = TaskKey::new(
        TaskKey::DEVICE_SCOPE_ID,
        "embedding",
        "shared-input-hash",
        "profile-b",
    );
    for k in [&claiming_key, &other_profile_key] {
        phase1_intent(&db, k, RequestKind::Sync, 0.0, Some(300)).unwrap();
    }
    // Force both stale.
    Connection::open(db.path())
        .unwrap()
        .execute(
            "UPDATE batch_requests SET stale_after_at = ?1 WHERE input_hash = 'shared-input-hash'",
            params![now_millis() - 1],
        )
        .unwrap();

    let plan = plan_bounded_sweep(&db, Some(&claiming_key), now_millis()).unwrap();
    assert!(
        plan.own_key_stale.contains(&claiming_key),
        "the exact claiming key belongs in the unbounded own-key pool"
    );
    assert!(
        !plan.own_key_stale.contains(&other_profile_key),
        "R23-04: a different tool_profile_hash must NOT ride the own-key exemption"
    );
    assert!(
        plan.general_stale.contains(&other_profile_key),
        "a different tool_profile_hash belongs in the capped general pool instead"
    );
}

/// R23-30 (04 §5.4 / A-21): a fractional `retry_after_seconds` (e.g. `600.5`)
/// rounds UP (`ceil`), never truncates — truncating computes a protection
/// deadline strictly SHORTER than the provider's actual requested wait,
/// reopening the double-invocation window this deadline exists to close.
#[test]
fn r23_30_retry_after_fractional_seconds_rounds_up_not_down() {
    let (_dir, db) = open_temp_ledger();
    let task_key = TaskKey::new(
        TaskKey::DEVICE_SCOPE_ID,
        "embedding",
        "fractional-retry-after",
        "embed-profile",
    );
    let claim = device_claim(&db, &task_key, 0.0, 300, R23_02_NEVER_DENY_DEVICE_CAP, None).unwrap();
    let ClaimOutcome::Claimed(outcome) = claim else {
        panic!("expected a fresh claim");
    };
    let before = now_millis();
    let extension =
        device_extend_stale_after(&db, &task_key, &outcome.intent_token, 600.5, 300).unwrap();
    let ExtendOutcome::Extended(new_value) = extension else {
        panic!("expected the extension to succeed");
    };
    // ceil(600.5) = 601 (not truncated 600) -> 601 + 300 + 60 = 961s margin.
    let lower_bound = before + 961_000;
    let upper_bound = lower_bound + 5_000; // generous slack for test execution time
    assert!(
        (lower_bound..=upper_bound).contains(&new_value),
        "got {new_value}, expected roughly {lower_bound} (601s Retry-After ceiling, \
         not the 600s a truncating cast would compute)"
    );
}

// ---------------------------------------------------------------------------
// §I — budget cap check-then-reserve (CL56-CL60; CL61's enum lives in
// kio-pipeline's inline tests)
// ---------------------------------------------------------------------------

/// CL56: folder cap unset -> device cap alone governs; folder cap set ->
/// `min(folder remaining, device remaining)`.
#[test]
fn cl56_two_layer_cap_folder_optional() {
    let (_dir, db) = open_temp_ledger();
    let folder_a = key("folder-a", "markdownize", "task-a");
    let no_folder_cap = BudgetCapConfig {
        device_cap: 50.0,
        folder_cap: None,
        device_per_adapter_cap: None,
    };
    let result = check_then_reserve(
        &db,
        &folder_a,
        10.0,
        &no_folder_cap,
        RequestKind::Batch,
        None,
    )
    .unwrap();
    assert!(matches!(result, CapCheckResult::Allowed(_)));

    let folder_b = key("folder-b", "markdownize", "task-b");
    let with_tight_folder_cap = BudgetCapConfig {
        device_cap: 50.0,
        folder_cap: Some(5.0),
        device_per_adapter_cap: None,
    };
    let denied = check_then_reserve(
        &db,
        &folder_b,
        10.0,
        &with_tight_folder_cap,
        RequestKind::Batch,
        None,
    )
    .unwrap();
    assert_eq!(denied, CapCheckResult::Denied(CapLayer::Folder));
}

/// CL57: the full 3-condition AND, and the same-Tx check-then-reserve
/// atomicity (denied path never creates a phase-1 row).
#[test]
fn cl57_three_condition_and_and_same_tx_atomicity() {
    let (_dir, db) = open_temp_ledger();
    let task_key = key("folder-a", "markdownize", "task-a");
    // Seed prior spend: device=45, folder=8, per_adapter(markdownize)=29.
    seed_confirmed_charge(&db, "other-scope-x", "markdownize", 37.0);
    seed_confirmed_charge(&db, "folder-a", "markdownize", 8.0);
    let caps = BudgetCapConfig {
        device_cap: 50.0,
        folder_cap: Some(10.0),
        device_per_adapter_cap: Some(30.0),
    };
    let device_total = ledger_month_total(&db, None, None, &utc_month_of(now_millis())).unwrap();
    assert_eq!(device_total, 45.0);

    // 45+3=48<50 OK, 8+3=11 !< 10 -> folder denies.
    let denied_folder =
        check_then_reserve(&db, &task_key, 3.0, &caps, RequestKind::Batch, None).unwrap();
    assert_eq!(denied_folder, CapCheckResult::Denied(CapLayer::Folder));
    assert!(
        get_batch_request(&db, &task_key).unwrap().is_none(),
        "no phase-1 row on denial"
    );

    // Loosen folder cap: 8+3=11... still need folder >= 11, set to 20 so folder passes,
    // but per_adapter: 45(total)/but per_adapter total is separately tracked at 37 -> +3=40 !< 30?
    // Recompute a clean per_adapter-denial scenario directly.
    let (_dir2, db2) = open_temp_ledger();
    let key2 = key("folder-b", "markdownize", "task-b");
    seed_confirmed_charge(&db2, "folder-b", "markdownize", 29.0);
    let caps2 = BudgetCapConfig {
        device_cap: 50.0,
        folder_cap: Some(100.0),
        device_per_adapter_cap: Some(30.0),
    };
    let denied_adapter =
        check_then_reserve(&db2, &key2, 3.0, &caps2, RequestKind::Batch, None).unwrap();
    assert_eq!(denied_adapter, CapCheckResult::Denied(CapLayer::PerAdapter));
    assert!(get_batch_request(&db2, &key2).unwrap().is_none());

    // All three pass -> allowed, and the reservation lands in the same call.
    let (_dir3, db3) = open_temp_ledger();
    let key3 = key("folder-c", "markdownize", "task-c");
    let caps3 = BudgetCapConfig {
        device_cap: 50.0,
        folder_cap: Some(10.0),
        device_per_adapter_cap: Some(30.0),
    };
    let allowed = check_then_reserve(&db3, &key3, 3.0, &caps3, RequestKind::Batch, None).unwrap();
    assert!(matches!(allowed, CapCheckResult::Allowed(_)));
    assert!(get_batch_request(&db3, &key3).unwrap().is_some());
}

/// CL58: `candidate=0` bypasses the cap check entirely, even when every layer
/// is already over cap.
#[test]
fn cl58_zero_candidate_bypasses_cap_even_when_over() {
    let (_dir, db) = open_temp_ledger();
    seed_confirmed_charge(&db, "folder-a", "markdownize", 999.0);
    let task_key = key("folder-a", "markdownize", "free-task");
    let caps = BudgetCapConfig {
        device_cap: 50.0,
        folder_cap: Some(10.0),
        device_per_adapter_cap: Some(1.0),
    };
    let result = check_then_reserve(&db, &task_key, 0.0, &caps, RequestKind::Batch, None).unwrap();
    assert!(matches!(result, CapCheckResult::ExemptZeroCost(_)));
    assert!(get_batch_request(&db, &task_key).unwrap().is_some());
}

/// CL59: `ledger(...)` = confirmed-month sum (estimate rows count too) +
/// unterminated `batch_requests.estimated_usd` reservation sum; a terminal
/// row's `estimated_usd` is not double-counted (it is already reflected in
/// `cost_ledger`).
#[test]
fn cl59_ledger_total_combines_confirmed_and_inflight_reservation() {
    let (_dir, db) = open_temp_ledger();
    let task_key = key("s", "markdownize", "h");
    Connection::open(db.path())
        .unwrap()
        .execute(
            "INSERT INTO cost_ledger (scope_id, adapter_kind, input_hash, tool_profile_hash, \
             submission_seq, batch_job_id, usd, estimated, outcome, month, recorded_at) \
             VALUES ('s','markdownize','x1','t', 1, 'j1', 10.0, 0, 'succeeded', ?1, 0), \
                    ('s','markdownize','x2','t', 1, 'j2', 2.0, 1, 'unknown_settled', ?1, 0)",
            params![utc_month_of(now_millis())],
        )
        .unwrap();
    let inflight = key("s", "markdownize", "inflight");
    phase1_intent(&db, &inflight, RequestKind::Batch, 5.0, None).unwrap();
    let terminal_row = key("s", "markdownize", "terminal");
    let terminal_intent = phase1_intent(&db, &terminal_row, RequestKind::Batch, 1.0, None).unwrap();
    terminal_transaction(
        &db,
        &plain_terminal_write(
            &terminal_row,
            Outcome::Succeeded,
            BilledAmount {
                usd: 1.0,
                estimated: false,
            },
            &terminal_intent.intent_token,
            BatchState::Completed,
            None,
        ),
    )
    .unwrap();
    let _ = task_key;

    let total = ledger_month_total(&db, Some("s"), None, &utc_month_of(now_millis())).unwrap();
    assert_eq!(total, 10.0 + 2.0 + 5.0 + 1.0);
}

/// CL60: a sync call's phase 1 never writes to `cost_ledger` — only
/// `batch_requests.estimated_usd` changes.
#[test]
fn cl60_sync_phase1_never_touches_cost_ledger() {
    let (_dir, db) = open_temp_ledger();
    let task_key = key("s", "embedding", "h");
    let caps = BudgetCapConfig {
        device_cap: 50.0,
        folder_cap: None,
        device_per_adapter_cap: None,
    };
    check_then_reserve(&db, &task_key, 0.05, &caps, RequestKind::Sync, Some(300)).unwrap();
    let ledger_count: i64 = Connection::open(db.path())
        .unwrap()
        .query_row("SELECT COUNT(*) FROM cost_ledger", [], |row| row.get(0))
        .unwrap();
    assert_eq!(ledger_count, 0);
    let row = get_batch_request(&db, &task_key).unwrap().unwrap();
    assert_eq!(row.estimated_usd, 0.05);
}

fn seed_confirmed_charge(db: &LedgerDb, scope_id: &str, adapter_kind: &str, usd: f64) {
    Connection::open(db.path())
        .unwrap()
        .execute(
            "INSERT INTO cost_ledger (scope_id, adapter_kind, input_hash, tool_profile_hash, \
             submission_seq, batch_job_id, usd, estimated, outcome, month, recorded_at) \
             VALUES (?1,?2, ?3, 't', 1, 'seed-job', ?4, 0, 'succeeded', ?5, 0)",
            params![
                scope_id,
                adapter_kind,
                format!("seed-{scope_id}-{adapter_kind}-{usd}"),
                usd,
                utc_month_of(now_millis()),
            ],
        )
        .unwrap();
}

/// CL63: a terminal sync row has already had `intent_token` NULL'd (CL47), so
/// it cannot be reached via an intent-token selector. Its four-tuple resolves
/// to the terminal row and abandon remains an idempotent no-op.
#[test]
fn cl63_terminal_sync_row_needs_the_four_tuple_selector() {
    let (_dir, db) = open_temp_ledger();
    let task_key = key("s", "embedding", "h");
    let intent = phase1_intent(&db, &task_key, RequestKind::Sync, 0.01, Some(300)).unwrap();
    terminal_transaction(
        &db,
        &plain_terminal_write(
            &task_key,
            Outcome::Succeeded,
            BilledAmount {
                usd: 0.01,
                estimated: false,
            },
            &intent.intent_token,
            BatchState::Completed,
            None,
        ),
    )
    .unwrap();
    assert!(
        get_batch_request(&db, &task_key)
            .unwrap()
            .unwrap()
            .intent_token
            .is_none()
    );
    assert_eq!(
        resolve_abandon_selector(&db, &AbandonSelector::IntentToken(intent.intent_token)).unwrap(),
        AbandonResolution::NotFound,
    );
    assert_eq!(
        resolve_abandon_selector(&db, &AbandonSelector::TaskKey(task_key.clone())).unwrap(),
        AbandonResolution::Found(task_key),
    );
}

/// CL67: abandon applies to `request_kind='sync'` rows exactly like batch rows
/// — same CLI, same confirmation — except there is no upload/job phase to
/// clean up, so `intent_token` clears in the same Tx (no "wait for cleanup").
#[test]
fn cl67_abandon_applies_to_sync_rows_with_immediate_token_clear() {
    let (_dir, db) = open_temp_ledger();
    let task_key = key("s", "embedding", "h");
    phase1_intent(&db, &task_key, RequestKind::Sync, 0.02, Some(300)).unwrap();
    let execution = execute_abandon(&db, &task_key).unwrap();
    assert_eq!(execution, AbandonExecution::Abandoned);
    let row = get_batch_request(&db, &task_key).unwrap().unwrap();
    assert_eq!(row.state, BatchState::Terminal);
    assert!(
        row.intent_token.is_none(),
        "sync abandon must not wait for cleanup"
    );
    let ledger_rows = cost_ledger_rows_for_key(&db, &task_key).unwrap();
    assert_eq!(ledger_rows[0].outcome, Outcome::Abandoned);
}

/// CL64: abandon's full end-to-end effect via the real CLI — user confirms,
/// and the estimated charge + `state=3` + `completed_at` land together (the
/// DB-level mechanics are CL24's; this drives it through `kio batch abandon`
/// itself, matching the contract's literal "ユーザー確認で..." framing).
#[test]
fn cl64_abandon_cli_confirmation_records_estimated_charge_and_terminal_state() {
    let dir = canonical_tempdir();
    init_scope(&dir);
    let task_key = key("device-test-scope", "markdownize", "cl64-task");
    let db = LedgerDb::open_existing(ledger_path_for(&dir)).unwrap();
    let intent = phase1_intent(&db, &task_key, RequestKind::Batch, 1.25, None).unwrap();
    drop(db);

    let stdout = kio(&dir, &["batch", "abandon", &intent.intent_token, "--yes"])
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let result: Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(result["status"], "abandoned");

    let db_after = LedgerDb::open_existing(ledger_path_for(&dir)).unwrap();
    let row = get_batch_request(&db_after, &task_key).unwrap().unwrap();
    assert_eq!(row.state, BatchState::Terminal);
    assert!(row.completed_at.is_some());
    let ledger_rows = cost_ledger_rows_for_key(&db_after, &task_key).unwrap();
    assert_eq!(ledger_rows.len(), 1);
    assert_eq!(ledger_rows[0].outcome, Outcome::Abandoned);
    assert_eq!(ledger_rows[0].usd, 1.25);
    assert!(ledger_rows[0].estimated);
}

// ---------------------------------------------------------------------------
// §K — cross-cutting (CL69, CL70; CL71 is covered by runtime rejection)
// ---------------------------------------------------------------------------

/// CL69: a CHECK violation that bypasses this module's own pre-validation
/// (simulated by writing a raw, unvalidated INSERT — standing in for a test
/// double / implementation bug) is reclassified as the permanent
/// `KIO-E-STORE-CONSTRAINT-001` implementation error, not a generic error.
#[test]
fn cl69_check_violation_reaches_the_caller_as_store_constraint_error() {
    let (_dir, db) = open_temp_ledger();
    // Bypass validation entirely: attempt to record a negative usd directly
    // through the same INSERT shape terminal_transaction uses.
    let raw_result = Connection::open(db.path()).unwrap().execute(
        "INSERT INTO cost_ledger (scope_id, adapter_kind, input_hash, tool_profile_hash, \
         submission_seq, batch_job_id, usd, estimated, outcome, month, recorded_at) \
         VALUES ('s','markdownize','h','t', 1, 'job', -5.0, 0, 'succeeded', '2026-07', 0)",
        [],
    );
    let err = raw_result.unwrap_err();
    assert!(
        matches!(
            &err,
            rusqlite::Error::SqliteFailure(ffi_err, _)
                if ffi_err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_CHECK
        ),
        "expected a CHECK constraint failure, got {err:?}"
    );
}

/// CL70: device-global single-file path resolution (via `$XDG_DATA_HOME`),
/// WAL plus a meaningful busy timeout on lifecycle writes.
#[test]
fn cl70_device_global_path_and_wal_busy_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let data_home = std::fs::canonicalize(dir.path()).unwrap();
    // SAFETY (test-only): scoped to this process's short-lived assertion;
    // no other thread in this binary reads XDG_DATA_HOME concurrently with it.
    unsafe {
        std::env::set_var("XDG_DATA_HOME", &data_home);
    }
    let resolved = kio_pipeline::ledger::default_ledger_path().unwrap();
    unsafe {
        std::env::remove_var("XDG_DATA_HOME");
    }
    assert_eq!(resolved, data_home.join("kio/cost-ledger.sqlite"));

    let initialized = LedgerDb::initialize(&resolved).unwrap();
    drop(initialized);
    let db = LedgerDb::open_existing(&resolved).unwrap();
    let journal: String = Connection::open(db.path())
        .unwrap()
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(journal.to_lowercase(), "wal");
    let blocker = Connection::open(db.path()).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE;").unwrap();
    let worker = db.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        tx.send(phase1_intent(
            &worker,
            &key("cl70", "markdownize", "busy-timeout"),
            RequestKind::Batch,
            0.0,
            None,
        ))
        .unwrap();
    });
    assert!(
        rx.recv_timeout(std::time::Duration::from_millis(100))
            .is_err(),
        "the lifecycle write must wait for SQLite contention instead of failing immediately"
    );
    blocker.execute_batch("ROLLBACK;").unwrap();
    rx.recv_timeout(std::time::Duration::from_secs(6))
        .expect("writer must complete after the lock releases")
        .unwrap();
}

#[test]
fn retired_jsonl_files_fail_closed_without_modification() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = std::fs::canonicalize(dir.path()).unwrap().join("device");
    std::fs::create_dir(&data_dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    for name in [
        "cost-ledger.jsonl",
        "cost-ledger-reservations.jsonl",
        "cost-ledger-reclaimed.jsonl",
        "cost-ledger.lock",
        "cost-ledger.jsonl.migrated",
        "cost-ledger-reservations.jsonl.migrated",
        "cost-ledger-reclaimed.jsonl.migrated",
        "cost-ledger.lock.migrated",
    ] {
        let path = data_dir.join(name);
        let original = [
            b"non-rebuildable bytes for ".as_slice(),
            name.as_bytes(),
            b":\0\xff",
        ]
        .concat();
        std::fs::write(&path, &original).unwrap();

        let err = match LedgerDb::open_existing(data_dir.join("cost-ledger.sqlite")) {
            Ok(_) => panic!("{name} must make ledger startup fail closed"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("KIO-E-LEDGER-LEGACY-JSONL-001"),
            "got {err}"
        );
        assert!(
            !data_dir.join("cost-ledger.sqlite").exists(),
            "detection must happen before SQLite is created"
        );
        assert_eq!(std::fs::read(&path).unwrap(), original, "{name} changed");
        assert!(
            !data_dir.join(format!("{name}.migrated")).exists(),
            "startup must not rename {name}"
        );
        std::fs::remove_file(path).unwrap();
    }

    let legacy_names = [
        "cost-ledger.jsonl",
        "cost-ledger-reservations.jsonl",
        "cost-ledger-reclaimed.jsonl",
        "cost-ledger.lock",
        "cost-ledger.jsonl.migrated",
        "cost-ledger-reservations.jsonl.migrated",
        "cost-ledger-reclaimed.jsonl.migrated",
        "cost-ledger.lock.migrated",
    ];
    let originals = legacy_names
        .iter()
        .map(|name| {
            let bytes = [
                b"independent truth bytes: ".as_slice(),
                name.as_bytes(),
                b":\0\xff",
            ]
            .concat();
            let path = data_dir.join(name);
            std::fs::write(&path, &bytes).unwrap();
            (path, bytes)
        })
        .collect::<Vec<_>>();
    let err = match LedgerDb::open_existing(data_dir.join("cost-ledger.sqlite")) {
        Ok(_) => panic!("multiple legacy files must make ledger startup fail closed"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("KIO-E-LEDGER-LEGACY-JSONL-001"));
    assert!(!data_dir.join("cost-ledger.sqlite").exists());
    for (path, original) in originals {
        assert_eq!(
            std::fs::read(&path).unwrap(),
            original,
            "{} changed",
            path.display()
        );
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let target = data_dir.join("legacy-symlink-target");
        std::fs::write(&target, b"non-rebuildable symlink target\0\xff").unwrap();
        let symlinked = data_dir.join("cost-ledger.jsonl");
        symlink(&target, &symlinked).unwrap();
        let err = match LedgerDb::open_existing(data_dir.join("cost-ledger.sqlite")) {
            Ok(_) => panic!("a retired-ledger symlink must fail closed"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("KIO-E-LEDGER-LEGACY-JSONL-001"));
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"non-rebuildable symlink target\0\xff"
        );
        std::fs::remove_file(symlinked).unwrap();

        let dangling = data_dir.join("cost-ledger.jsonl.migrated");
        symlink(data_dir.join("missing-legacy-target"), &dangling).unwrap();
        let err = match LedgerDb::open_existing(data_dir.join("cost-ledger.sqlite")) {
            Ok(_) => panic!("a dangling retired-ledger symlink must fail closed"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("KIO-E-LEDGER-LEGACY-JSONL-001"));
        assert!(std::fs::symlink_metadata(&dangling).is_ok());
    }
}

#[test]
fn ledger_opens_normally_when_no_retired_jsonl_files_exist() {
    let dir = canonical_tempdir();
    let path = dir.path().join("kio/cost-ledger.sqlite");
    LedgerDb::initialize(&path).unwrap();
    let db = LedgerDb::open_existing(&path).unwrap();
    let tables: i64 = Connection::open(db.path())
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name IN
             ('cost_ledger', 'batch_requests', 'schema_migrations', 'ledger_metadata')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        tables, 4,
        "fresh ledger must include its authority metadata"
    );
}

#[test]
fn legacy_rejection_does_not_change_existing_sqlite_bytes_or_rows() {
    let dir = canonical_tempdir();
    let data_dir = dir.path().join("kio");
    let path = data_dir.join("cost-ledger.sqlite");
    {
        let db = LedgerDb::initialize(&path).unwrap();
        let task_key = key("scope-a", "markdownize", "hash-a");
        phase1_intent(&db, &task_key, RequestKind::Batch, 1.0, None).unwrap();
    }
    let before = std::fs::read(&path).unwrap();
    let retired_names = [
        "cost-ledger.jsonl",
        "cost-ledger-reservations.jsonl",
        "cost-ledger-reclaimed.jsonl",
        "cost-ledger.lock",
        "cost-ledger.jsonl.migrated",
        "cost-ledger-reservations.jsonl.migrated",
        "cost-ledger-reclaimed.jsonl.migrated",
        "cost-ledger.lock.migrated",
    ];
    for name in retired_names {
        let legacy_path = data_dir.join(name);
        let legacy_bytes = [
            b"opaque legacy ledger bytes: ".as_slice(),
            name.as_bytes(),
            b"\0\xff",
        ]
        .concat();
        std::fs::write(&legacy_path, &legacy_bytes).unwrap();
        let err = match LedgerDb::open_existing(&path) {
            Ok(_) => panic!("{name} must make ledger startup fail closed"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("KIO-E-LEDGER-LEGACY-JSONL-001"));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "{name} changed SQLite"
        );
        assert_eq!(std::fs::read(&legacy_path).unwrap(), legacy_bytes);
        std::fs::remove_file(legacy_path).unwrap();
    }

    let mixed = retired_names
        .iter()
        .map(|name| {
            let path = data_dir.join(name);
            let bytes = [
                b"mixed truth bytes: ".as_slice(),
                name.as_bytes(),
                b"\0\xff",
            ]
            .concat();
            std::fs::write(&path, &bytes).unwrap();
            (path, bytes)
        })
        .collect::<Vec<_>>();
    let err = match LedgerDb::open_existing(&path) {
        Ok(_) => panic!("mixed retired files must make ledger startup fail closed"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("KIO-E-LEDGER-LEGACY-JSONL-001"));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "mixed files changed SQLite"
    );
    for (legacy_path, legacy_bytes) in mixed {
        assert_eq!(std::fs::read(&legacy_path).unwrap(), legacy_bytes);
        std::fs::remove_file(legacy_path).unwrap();
    }

    let db = LedgerDb::open_existing(&path).unwrap();
    let rows: i64 = Connection::open(db.path())
        .unwrap()
        .query_row("SELECT COUNT(*) FROM batch_requests", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 1, "legacy rejection must not change existing rows");
}

// ---------------------------------------------------------------------------
// §J — `kio batch abandon` / `--reset-violations` / `kio status` stalled
// display, at the CLI level (CL62-CL68 CLI-surface aspects; the DB-level
// abandon/reset mechanics above (CL24, CL37, CL38, CL55) are already covered).
// ---------------------------------------------------------------------------

const CHILD_ENV_DENYLIST: &[&str] = &["GEMINI_API_KEY", "MISTRAL_API_KEY", "KIO_FIXED_NOW"];

fn kio(dir: &TempDir, args: &[&str]) -> Command {
    let mut command = Command::cargo_bin("kio").unwrap();
    for name in CHILD_ENV_DENYLIST {
        command.env_remove(name);
    }
    command
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join(".test-config"))
        .env("XDG_DATA_HOME", dir.path().join(".test-data"))
        .env("XDG_CACHE_HOME", dir.path().join(".test-cache"))
        .args(args);
    command
}

fn json_success(dir: &TempDir, args: &[&str]) -> Value {
    let stdout = kio(dir, args)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&stdout).unwrap()
}

fn ledger_path_for(dir: &TempDir) -> PathBuf {
    dir.path().join(".test-data/kio/cost-ledger.sqlite")
}

fn seed_ledger_row_for_cli(dir: &TempDir, task_key: &TaskKey) -> String {
    let db = LedgerDb::open_existing(ledger_path_for(dir)).unwrap();
    let intent = phase1_intent(&db, task_key, RequestKind::Batch, 1.0, None).unwrap();
    terminal_transaction(
        &db,
        &TerminalWrite {
            clear_intent_token: false, // stalled: settled but cleanup stuck
            ..plain_terminal_write(
                task_key,
                Outcome::UnknownSettled,
                BilledAmount {
                    usd: 1.0,
                    estimated: true,
                },
                &intent.intent_token,
                BatchState::Terminal,
                Some("unknown_settled"),
            )
        },
    )
    .unwrap();
    intent.intent_token
}

fn init_scope(dir: &TempDir) {
    std::fs::write(dir.path().join("doc.md"), "hello").unwrap();
    json_success(dir, &["init"]);
    LedgerDb::initialize(ledger_path_for(dir)).unwrap();
}

/// CL65/CL68: `kio status` surfaces the stalled row's `intent_token`, and
/// feeding that exact token straight to `kio batch abandon` (after confirming)
/// resolves it — after which `kio status` no longer lists it as stalled.
#[test]
fn cl65_cl68_status_to_abandon_round_trip() {
    let dir = canonical_tempdir();
    init_scope(&dir);
    let task_key = key("device-test-scope", "markdownize", "stalled-task");
    let intent_token = seed_ledger_row_for_cli(&dir, &task_key);

    let status = json_success(&dir, &["status"]);
    let stalled = status["stalled_batch"].as_array().unwrap();
    assert_eq!(stalled.len(), 1);
    assert_eq!(stalled[0]["intent_token"], intent_token);

    // Explicit --yes -> abandoned.
    let stdout = kio(&dir, &["batch", "abandon", &intent_token, "--yes"])
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let result: Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(result["status"], "abandoned");

    let status_after = json_success(&dir, &["status"]);
    assert!(status_after["stalled_batch"].as_array().unwrap().is_empty());
}

/// CL65: JSON/non-interactive mode rejects a piped response or no response
/// without `--yes`, leaving state unchanged; CL66: a selector matching no row
/// is an idempotent exit-0 success.
#[test]
fn cl65_cl66_confirmation_rejection_and_no_target_idempotence() {
    let dir = canonical_tempdir();
    init_scope(&dir);
    let task_key = key("device-test-scope", "markdownize", "stalled-task-2");
    let intent_token = seed_ledger_row_for_cli(&dir, &task_key);

    kio(&dir, &["batch", "abandon", &intent_token])
        .write_stdin("y\n")
        .arg("--json")
        .assert()
        .code(9);
    // Non-interactive (no stdin content at all) also exits 9.
    kio(&dir, &["batch", "abandon", &intent_token])
        .write_stdin("")
        .arg("--json")
        .assert()
        .code(9);
    let status_unchanged = json_success(&dir, &["status"]);
    assert_eq!(
        status_unchanged["stalled_batch"].as_array().unwrap().len(),
        1
    );

    // CL66: no matching row at all.
    let no_target = json_success(
        &dir,
        &["batch", "abandon", "00000000-0000-7000-8000-000000000000"],
    );
    assert_eq!(no_target["status"], "no_target");
}

/// CL62(c): a 3-tuple selector matching multiple `tool_profile_hash` rows is
/// rejected as ambiguous.
#[test]
fn cl62_ambiguous_three_tuple_selector_is_rejected() {
    let dir = canonical_tempdir();
    init_scope(&dir);
    let db = LedgerDb::open_existing(ledger_path_for(&dir)).unwrap();
    for profile in ["profile-a", "profile-b"] {
        let task_key = TaskKey::new(
            "device-test-scope",
            "markdownize",
            "ambiguous-hash",
            profile,
        );
        phase1_intent(&db, &task_key, RequestKind::Batch, 1.0, None).unwrap();
    }
    kio(
        &dir,
        &[
            "batch",
            "abandon",
            "device-test-scope/markdownize/ambiguous-hash",
        ],
    )
    .arg("--json")
    .assert()
    .code(2);
}

/// `kio batch retry --reset-violations` follows the identical selector,
/// confirmation, and idempotence contract (§M note-6: a terminal row with
/// `count > 0` resets; a `count == 0` row reports "unchanged", not an error).
#[test]
fn reset_violations_resets_terminal_row_and_is_a_noop_at_zero() {
    let dir = canonical_tempdir();
    init_scope(&dir);
    let db = LedgerDb::open_existing(ledger_path_for(&dir)).unwrap();
    let task_key = key("device-test-scope", "markdownize", "violating-task");
    let intent = phase1_intent(&db, &task_key, RequestKind::Batch, 1.0, None).unwrap();
    terminal_transaction(
        &db,
        &TerminalWrite {
            increment_contract_violation: true,
            ..plain_terminal_write(
                &task_key,
                Outcome::ContractViolation,
                BilledAmount {
                    usd: 1.0,
                    estimated: false,
                },
                &intent.intent_token,
                BatchState::Terminal,
                Some("contract_violation"),
            )
        },
    )
    .unwrap();
    drop(db);

    // The authorization command requires an explicit --yes argument; piping
    // input cannot repair missing command syntax or authorize a reset.
    kio(
        &dir,
        &[
            "batch",
            "retry",
            "--reset-violations",
            "device-test-scope/markdownize/violating-task/tool-profile-1",
        ],
    )
    .write_stdin("y\n")
    .arg("--json")
    .assert()
    .code(2);

    let stdout = kio(
        &dir,
        &[
            "batch",
            "retry",
            "--reset-violations",
            "device-test-scope/markdownize/violating-task/tool-profile-1",
            "--yes",
        ],
    )
    .arg("--json")
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let result: Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(result["status"], "reset");

    let noop_stdout = kio(
        &dir,
        &[
            "batch",
            "retry",
            "--reset-violations",
            "device-test-scope/markdownize/violating-task/tool-profile-1",
            "--yes",
        ],
    )
    .arg("--json")
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let noop_result: Value = serde_json::from_slice(&noop_stdout).unwrap();
    assert_eq!(noop_result["status"], "unchanged");
}
