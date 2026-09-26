//! QA14/QA15 (step4b-contract-tests-p3a.md L307-338, 10-operations.md §7.5.2 /
//! 04-pipeline.md §5.8): `cost-ledger.sqlite` restore-from-backup detection +
//! the `kio ledger reconcile` integrity/recovery/orphan-attribution command.
//!
//! Harness conventions (helpers, env denylist, `fake_pdf`) are self-contained
//! copies of `step4b_ledger_contract.rs`/`step4b_selfheal_contract.rs`'s own
//! — this crate's integration test binaries do not share a `tests/common`
//! module (an established convention, not an oversight — see those files'
//! own header comments).

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use kio_pipeline::ledger::LedgerDb;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use tempfile::TempDir;

const KIO_CHILD_ENV_DENYLIST: &[&str] = &[
    "GEMINI_API_KEY",
    "MISTRAL_API_KEY",
    "KIO_FIXED_NOW",
    "KIO_TEST_GEMINI_EMBED",
    "KIO_TEST_MISTRAL_OCR",
    "KIO_TEST_MISTRAL_BATCH",
    "KIO_TEST_MARKDOWNIZE_ADAPTER",
    "KIO_TEST_QUERY_EMBED_TRACE",
    "KIO_TEST_HOLD_LOCK_READY",
    "KIO_TEST_SCOPE_SEARCH_DELAY_SCOPE_ID",
    "KIO_TEST_SCOPE_SEARCH_DELAY_MS",
    "KIO_TEST_R13_2_AUTH",
    "KIO_TEST_R13_2_DECLARED",
    "KIO_TEST_R13_2_FALLBACK",
    "KIO_TEST_WINDOWS_PROFILE",
    "KIO_TEST_BATCH_INVENTORY",
];

fn kio(dir: &TempDir, args: &[&str]) -> Command {
    let mut command = Command::cargo_bin("kio").unwrap();
    for name in KIO_CHILD_ENV_DENYLIST {
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

/// Like [`json_success`] but for a `batch resume` under the Mistral OCR mock
/// send seam (`step4_promotion.rs`'s established pattern: `Some("mock")`
/// triggers a synthetic online markdownize response instead of a real
/// network call — `kio-adapter/src/catalog.rs`'s `TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV`).
fn json_success_mock_send(dir: &TempDir, args: &[&str]) -> Value {
    let stdout = kio(dir, args)
        .env("KIO_TEST_MISTRAL_OCR", "mock")
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&stdout).unwrap()
}

/// Like [`json_success_mock_send`] but asserts process FAILURE and returns
/// the parsed stderr JSON error envelope.
fn json_failure_mock_send(dir: &TempDir, args: &[&str]) -> (i32, Value) {
    let output = kio(dir, args)
        .env("KIO_TEST_MISTRAL_OCR", "mock")
        .arg("--json")
        .assert()
        .failure()
        .get_output()
        .clone();
    let code = output.status.code().unwrap();
    let stderr: Value = serde_json::from_slice(&output.stderr).unwrap();
    (code, stderr)
}

fn init_scope(dir: &TempDir) {
    json_success(dir, &["init"]);
}

/// Paid-ledger tests initialize a ledger explicitly.  Scope initialization
/// never creates a device-global billing authority as a side effect.
fn init(dir: &TempDir) {
    init_scope(dir);
    LedgerDb::initialize(ledger_path(dir)).unwrap();
}

fn scope_json(dir: &TempDir) -> Value {
    serde_json::from_str(&fs::read_to_string(dir.path().join(".kio/scope.json")).unwrap()).unwrap()
}

fn fake_pdf(pages: &[&str]) -> String {
    let kids = (0..pages.len())
        .map(|index| format!("{} 0 R", index + 2))
        .collect::<Vec<_>>()
        .join(" ");
    let mut out = format!(
        "%PDF-1.4\n1 0 obj << /Type /Pages /Kids [{kids}] /Count {} >> endobj\n",
        pages.len()
    );
    for (index, page) in pages.iter().enumerate() {
        out.push_str(&format!(
            "{} 0 obj << /Type /Page /Parent 1 0 R >> stream\nBT ({page}) Tj ET\nendstream endobj\n",
            index + 2
        ));
    }
    out.push_str("%%EOF\n");
    out
}

fn ledger_path(dir: &TempDir) -> PathBuf {
    fs::canonicalize(dir.path())
        .unwrap()
        .join(".test-data/kio/cost-ledger.sqlite")
}

fn wal_sidecar_paths(ledger_path: &Path) -> (PathBuf, PathBuf) {
    let mut wal = ledger_path.as_os_str().to_owned();
    wal.push("-wal");
    let mut shm = ledger_path.as_os_str().to_owned();
    shm.push("-shm");
    (PathBuf::from(wal), PathBuf::from(shm))
}

/// Fold the current WAL fully into the main DB file and clear it, so a plain
/// `fs::copy` of the main file alone (no `-wal`/`-shm` sidecars) captures a
/// complete, self-consistent snapshot — the same guarantee a real
/// `sqlite3 ... .backup` (10-operations.md §7.5.2) provides.
fn checkpoint_wal(ledger_path: &Path) {
    let conn = Connection::open(ledger_path).unwrap();
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    drop(conn);
}

fn snapshot_ledger(ledger_path: &Path, snapshot_path: &Path) {
    checkpoint_wal(ledger_path);
    fs::copy(ledger_path, snapshot_path).unwrap();
}

/// Overwrite the live ledger file with an earlier snapshot — emulating the
/// documented `sqlite3 ... .backup`/restore procedure (10 §7.5.2): the
/// restored file replaces the DB wholesale, with no WAL/SHM sidecars of its
/// own (a `.backup` output is a plain, complete single file).
fn restore_ledger(ledger_path: &Path, snapshot_path: &Path) {
    checkpoint_wal(ledger_path);
    let (wal, shm) = wal_sidecar_paths(ledger_path);
    let _ = fs::remove_file(&wal);
    let _ = fs::remove_file(&shm);
    fs::copy(snapshot_path, ledger_path).unwrap();
}

/// A synthetic UUIDv7 string with an ARBITRARY embedded millisecond
/// timestamp (mirrors `kio_pipeline::ledger::ops::new_intent_token`'s exact
/// bit layout — version nibble `7`, variant `10xxxxxx` — but for a
/// caller-chosen instant rather than "now", so a test can hand-craft a
/// `batch_requests` row whose `intent_token` reads as issued far in the
/// past without waiting real wall-clock time for `recovery_deadline_passed`
/// to become true). `discriminator` varies the trailing (non-timestamp)
/// bytes so two calls with the SAME `millis` (as every row in
/// `qa15_batch_recovery_walk_first_wiring` deliberately shares, to keep
/// every row equally past the recovery deadline) still produce DISTINCT
/// tokens — required both for `batch_requests`' own uniqueness and so a
/// fixture job's `intent_token` can unambiguously match exactly one row.
fn synthetic_uuid_v7(millis: u64, discriminator: u8) -> String {
    let mut bytes = [0_u8; 16];
    bytes[0] = (millis >> 40) as u8;
    bytes[1] = (millis >> 32) as u8;
    bytes[2] = (millis >> 24) as u8;
    bytes[3] = (millis >> 16) as u8;
    bytes[4] = (millis >> 8) as u8;
    bytes[5] = millis as u8;
    bytes[6] = 0x71;
    bytes[7] = discriminator;
    bytes[8] = 0x80;
    bytes[9] = 0x45;
    for (index, byte) in bytes.iter_mut().enumerate().take(16).skip(10) {
        *byte = (index as u8).wrapping_mul(7).wrapping_add(discriminator);
    }
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

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Directly craft a `batch_requests` row (bypassing the CLI/ledger API on
/// purpose — QA15's batch-row recovery walk contract targets rows however
/// they came to exist, e.g. from a device that crashed mid-protocol before
/// this session's own `phase1_intent`/`terminal_transaction` bump
/// instrumentation ever ran).
#[allow(clippy::too_many_arguments)]
fn insert_batch_request_row(
    conn: &Connection,
    scope_id: &str,
    adapter_kind: &str,
    input_hash: &str,
    tool_profile_hash: &str,
    intent_token: &str,
    provider_scope_id: &str,
    job_create_started_at: i64,
) {
    conn.execute(
        "INSERT INTO batch_requests (
            scope_id, adapter_kind, input_hash, tool_profile_hash,
            state, request_kind, intent_token, upload_id, batch_job_id,
            provider_scope_id, job_create_started_at, stale_after_at,
            submission_seq, attempts, contract_violation_count, estimated_usd,
            error, completed_at, created_at
        ) VALUES (
            ?1, ?2, ?3, ?4,
            1, 'batch', ?5, NULL, NULL,
            ?6, ?7, NULL,
            1, 0, 0, 1.5,
            NULL, NULL, ?8
        )",
        params![
            scope_id,
            adapter_kind,
            input_hash,
            tool_profile_hash,
            intent_token,
            provider_scope_id,
            job_create_started_at,
            job_create_started_at,
        ],
    )
    .unwrap();
}

fn batch_request_row_exists(
    conn: &Connection,
    scope_id: &str,
    adapter_kind: &str,
    input_hash: &str,
    tool_profile_hash: &str,
) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM batch_requests
         WHERE scope_id = ?1 AND adapter_kind = ?2 AND input_hash = ?3 AND tool_profile_hash = ?4",
        params![scope_id, adapter_kind, input_hash, tool_profile_hash],
        |row| row.get::<_, i64>(0),
    )
    .unwrap()
        > 0
}

fn batch_request_row(
    conn: &Connection,
    scope_id: &str,
    adapter_kind: &str,
    input_hash: &str,
    tool_profile_hash: &str,
) -> (i64, Option<String>, Option<String>) {
    conn.query_row(
        "SELECT state, intent_token, batch_job_id FROM batch_requests
         WHERE scope_id = ?1 AND adapter_kind = ?2 AND input_hash = ?3 AND tool_profile_hash = ?4",
        params![scope_id, adapter_kind, input_hash, tool_profile_hash],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        },
    )
    .unwrap()
}

fn write_inventory_fixture(dir: &TempDir, inventories: &Value) -> PathBuf {
    let path = dir.path().join("batch-inventory.json");
    fs::write(&path, serde_json::to_string_pretty(inventories).unwrap()).unwrap();
    path
}

// ---------------------------------------------------------------------------
// QA14 — restore-from-backup detection gates new submissions
// ---------------------------------------------------------------------------

/// QA14: restoring an older SQLite main file leaves the current checkpoint in
/// place. The resulting sequence gap blocks both a new paid operation and an
/// ordinary reconcile; neither command may silently repair the ledger.
#[test]
fn qa14_restore_detection_gates_new_submissions() {
    let dir = tempfile::tempdir().unwrap();
    init(&dir);

    // Send #1: the ledger gets its first row and the authority-bound checkpoint
    // advances for the first time.
    fs::write(dir.path().join("a.pdf"), fake_pdf(&["alpha"])).unwrap();
    json_success(&dir, &["index", "--yes"]);
    json_success(&dir, &["adapter", "approve", "--all", "--yes"]);
    json_success_mock_send(&dir, &["batch", "resume"]);

    let ledger = ledger_path(&dir);
    let snapshot = dir.path().join("cost-ledger.sqlite.snapshot");
    snapshot_ledger(&ledger, &snapshot);

    // Send #2: the ledger advances further — the checkpoint is now PAST the
    // point the snapshot captured.
    fs::write(dir.path().join("b.pdf"), fake_pdf(&["beta"])).unwrap();
    json_success(&dir, &["index", "--yes"]);
    json_success(&dir, &["adapter", "approve", "--all", "--yes"]);
    json_success_mock_send(&dir, &["batch", "resume"]);

    // A third file's task is enqueued BEFORE the restore, so the restore
    // itself cannot be blamed for whether `index --approve`'s own local
    // processing does or does not touch the ledger — only the SEND
    // (`batch resume`, below) is under test.
    fs::write(dir.path().join("c.pdf"), fake_pdf(&["gamma"])).unwrap();
    json_success(&dir, &["index", "--yes"]);
    json_success(&dir, &["adapter", "approve", "--all", "--yes"]);

    let checkpoint_path = ledger.with_file_name("cost-ledger.sqlite.checkpoint.json");
    let checkpoint_before_restore = fs::read(&checkpoint_path).unwrap();

    // "Stop" (nothing running) — restore the snapshot over the live DB,
    // emulating 10 §7.5.2's documented backup/restore procedure.
    restore_ledger(&ledger, &snapshot);

    // The NEW submission (file c's send) must be refused before the mock
    // provider seam can perform a service call.
    let (code, error) = json_failure_mock_send(&dir, &["batch", "resume"]);
    assert_eq!(code, 3, "got {error}");
    assert_eq!(
        error["error_code"], "KIO-E-LEDGER-SEQUENCE-MISMATCH-001",
        "got {error}"
    );
    assert!(
        error["error_code"]
            .as_str()
            .unwrap_or_default()
            .contains("LEDGER"),
        "a restored sequence gap must fail at lifecycle validation: {error}"
    );
    assert_eq!(
        fs::read(&checkpoint_path).unwrap(),
        checkpoint_before_restore
    );

    // Ordinary reconciliation has no authority to heal this gap. A future
    // explicit recovery workflow needs its own test; it must prove no provider
    // service calls occur before recovery authority is established.
    let output = kio(&dir, &["ledger", "reconcile"])
        .arg("--json")
        .assert()
        .failure()
        .get_output()
        .clone();
    let reconcile_error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(
        reconcile_error["error_code"]
            .as_str()
            .unwrap_or_default()
            .contains("LEDGER"),
        "ordinary reconcile must not report a successful repair: {reconcile_error}"
    );
    assert_eq!(
        fs::read(&checkpoint_path).unwrap(),
        checkpoint_before_restore
    );
}

/// QA14: a restored database behind its authority-bound checkpoint also blocks
/// status. The read must not silently accept the old database or rewrite the
/// checkpoint to manufacture a new baseline.
#[test]
fn qa14_sequence_gap_blocks_status_without_mutating_lifecycle_artifacts() {
    let dir = tempfile::tempdir().unwrap();
    init(&dir);
    fs::write(dir.path().join("a.pdf"), fake_pdf(&["alpha"])).unwrap();
    json_success(&dir, &["index", "--yes"]);
    json_success(&dir, &["adapter", "approve", "--all", "--yes"]);
    json_success_mock_send(&dir, &["batch", "resume"]);

    let ledger = ledger_path(&dir);
    let snapshot = dir.path().join("cost-ledger.sqlite.snapshot");
    snapshot_ledger(&ledger, &snapshot);
    fs::write(dir.path().join("b.pdf"), fake_pdf(&["beta"])).unwrap();
    json_success(&dir, &["index", "--yes"]);
    json_success(&dir, &["adapter", "approve", "--all", "--yes"]);
    json_success_mock_send(&dir, &["batch", "resume"]);
    restore_ledger(&ledger, &snapshot);
    let checkpoint_path = ledger.with_file_name("cost-ledger.sqlite.checkpoint.json");
    let checkpoint_before_status = fs::read(&checkpoint_path).unwrap();
    let authority_path = ledger.with_file_name("cost-ledger.sqlite.authority.json");
    let authority_before_status = fs::read(&authority_path).unwrap();

    // Status reads an owned snapshot rather than reopening the mutable source.
    // The stable lifecycle mismatch therefore reaches its public read path as
    // an unsafe snapshot, while the paid submission path remains the direct
    // sequence-mismatch exit checked above.
    let output = kio(&dir, &["status"])
        .arg("--json")
        .assert()
        .code(4)
        .get_output()
        .clone();
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(
        error["error_code"], "KIO-E-LEDGER-SNAPSHOT-UNSAFE-001",
        "{error}"
    );
    assert_eq!(
        fs::read(&checkpoint_path).unwrap(),
        checkpoint_before_status
    );
    assert_eq!(fs::read(&authority_path).unwrap(), authority_before_status);
}

// ---------------------------------------------------------------------------
// QA15 — orphan/unknown provider-side job attribution walk
// ---------------------------------------------------------------------------

/// QA15: `kio ledger reconcile`'s orphan-attribution walk classifies
/// provider-side jobs/uploads with no matching local `batch_requests` row —
/// a fully-attributable job in a locally-verified scope is an orphan
/// (fetch/delete guidance included); a job whose scope is not locally
/// verified, or whose metadata is absent, is `unknown` (report-only); an
/// upload with an unmatched filename token is `unknown_uploads`. Nothing is
/// mutated, and rerunning produces an identical report.
#[test]
fn qa15_orphan_attribution_walk() {
    let dir = tempfile::tempdir().unwrap();
    init(&dir);
    let local_scope_id = scope_json(&dir)["scope_id"].as_str().unwrap().to_owned();

    let fixture = json!([
        {
            "provider_scope_id": "provider-scope-1",
            "jobs": [
                {
                    "job_id": "job-orphan",
                    "intent_token": "01H0000000000000000000ORPH",
                    "task_key": {
                        "scope_id": local_scope_id,
                        "adapter_kind": "markdownize",
                        "input_hash": "sha256:orphan-input",
                        "tool_profile_hash": "sha256:orphan-profile"
                    }
                },
                {
                    "job_id": "job-unknown-scope",
                    "intent_token": "01H0000000000000000UNKNOWN",
                    "task_key": {
                        "scope_id": "unrelated-scope-not-registered",
                        "adapter_kind": "markdownize",
                        "input_hash": "sha256:unknown-input",
                        "tool_profile_hash": "sha256:unknown-profile"
                    }
                },
                {
                    "job_id": "job-no-metadata"
                }
            ],
            "uploads": [
                {
                    "upload_id": "upload-unmatched",
                    "filename_token": "01H0000000000000000NOMATCH"
                }
            ]
        }
    ]);
    let fixture_path = write_inventory_fixture(&dir, &fixture);

    let run_reconcile = || {
        kio(&dir, &["ledger", "reconcile"])
            .env("KIO_TEST_BATCH_INVENTORY", &fixture_path)
            .arg("--json")
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    let first: Value = serde_json::from_slice(&run_reconcile()).unwrap();

    let orphans = first["orphans"].as_array().unwrap();
    assert_eq!(orphans.len(), 1, "got {first}");
    assert_eq!(orphans[0]["job_id"], "job-orphan");
    assert_eq!(orphans[0]["task_key"]["scope_id"], local_scope_id);
    assert!(
        orphans[0].get("guidance").is_some(),
        "orphan entries must carry fetch/delete guidance: {first}"
    );

    let unknown = first["unknown"].as_array().unwrap();
    assert_eq!(unknown.len(), 2, "got {first}");
    let unknown_job_ids: Vec<&str> = unknown
        .iter()
        .map(|entry| entry["job_id"].as_str().unwrap())
        .collect();
    assert!(unknown_job_ids.contains(&"job-unknown-scope"));
    assert!(unknown_job_ids.contains(&"job-no-metadata"));
    assert!(
        unknown.iter().all(|entry| entry.get("guidance").is_none()),
        "unknown entries must NOT carry fetch/delete guidance: {first}"
    );

    let unknown_uploads = first["unknown_uploads"].as_array().unwrap();
    assert_eq!(unknown_uploads.len(), 1, "got {first}");
    assert_eq!(unknown_uploads[0]["upload_id"], "upload-unmatched");

    // Nothing was mutated: no batch_requests row exists for any of these
    // task keys (they were never created).
    let ledger = LedgerDb::open_existing(ledger_path(&dir)).unwrap();
    let conn = Connection::open(ledger.path()).unwrap();
    assert!(!batch_request_row_exists(
        &conn,
        &local_scope_id,
        "markdownize",
        "sha256:orphan-input",
        "sha256:orphan-profile",
    ));
    drop(ledger);

    // Idempotent: an identical rerun (same fixture, nothing changed) reports
    // byte-for-byte the same classification.
    let second: Value = serde_json::from_slice(&run_reconcile()).unwrap();
    assert_eq!(first["orphans"], second["orphans"], "not idempotent");
    assert_eq!(first["unknown"], second["unknown"], "not idempotent");
    assert_eq!(
        first["unknown_uploads"], second["unknown_uploads"],
        "not idempotent"
    );
}

/// QA15: `kio ledger reconcile`'s batch-row recovery walk — the FIRST
/// production wiring of `recovery_candidates`/`recovery_mark_found`/
/// `recovery_settle_unknown` (previously test-only). A row whose provider
/// scope IS covered by the inventory and whose job IS listed becomes
/// `found` (self-describes `batch_job_id`); a row past both the recovery
/// deadline and visibility grace period whose job is NOT listed settles
/// `unknown` (a `cost_ledger` estimate row is recorded, `state` becomes
/// terminal); a row whose provider scope has no inventory at all is
/// `unlistable` and untouched.
#[test]
fn qa15_batch_recovery_walk_first_wiring() {
    let dir = tempfile::tempdir().unwrap();
    init_scope(&dir);
    let scope_id = scope_json(&dir)["scope_id"].as_str().unwrap().to_owned();

    let old_millis = (now_millis() - 50 * 3_600 * 1_000) as u64; // 50h ago
    let token_found = synthetic_uuid_v7(old_millis, 0x01);
    let token_settle = synthetic_uuid_v7(old_millis, 0x02);
    let token_unlistable = synthetic_uuid_v7(old_millis, 0x03);

    {
        // The paid-ledger fixture creates a fresh authority-bound ledger
        // explicitly; `init` itself does not implicitly create one.
        let ledger = LedgerDb::initialize(ledger_path(&dir)).unwrap();
        let conn = Connection::open(ledger.path()).unwrap();
        // Row A: provider scope is covered by the inventory, and the
        // inventory lists a job with this exact intent_token -> found.
        insert_batch_request_row(
            &conn,
            &scope_id,
            "markdownize",
            "sha256:row-a-input",
            "sha256:row-a-profile",
            &token_found,
            "provider-listed",
            old_millis as i64,
        );
        // Row B: same provider scope (covered), but the inventory does NOT
        // list a job with this token, and the row is past both the recovery
        // deadline (48h default) and the visibility grace period (10min
        // default) -> confirmed-absent -> settled unknown.
        insert_batch_request_row(
            &conn,
            &scope_id,
            "markdownize",
            "sha256:row-b-input",
            "sha256:row-b-profile",
            &token_settle,
            "provider-listed",
            old_millis as i64,
        );
        // Row C: a provider scope with NO configured inventory at all ->
        // unlistable, must stay completely untouched.
        insert_batch_request_row(
            &conn,
            &scope_id,
            "markdownize",
            "sha256:row-c-input",
            "sha256:row-c-profile",
            &token_unlistable,
            "provider-unlisted",
            old_millis as i64,
        );
    }

    let fixture = json!([
        {
            "provider_scope_id": "provider-listed",
            "jobs": [
                { "job_id": "provider-job-a", "intent_token": token_found }
            ],
            "uploads": []
        }
    ]);
    let fixture_path = write_inventory_fixture(&dir, &fixture);

    let stdout = kio(&dir, &["ledger", "reconcile"])
        .env("KIO_TEST_BATCH_INVENTORY", &fixture_path)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let report: Value = serde_json::from_slice(&stdout).unwrap();

    assert_eq!(report["batch_found"], 1, "got {report}");
    assert_eq!(report["batch_settled_unknown"], 1, "got {report}");
    assert_eq!(report["unlistable"], 1, "got {report}");

    let conn = Connection::open(ledger_path(&dir)).unwrap();

    // Row A: found — self-described batch_job_id, state unchanged (still 1
    // — `recovery_mark_found` only records the discovered job id).
    let (state_a, intent_a, job_id_a) = batch_request_row(
        &conn,
        &scope_id,
        "markdownize",
        "sha256:row-a-input",
        "sha256:row-a-profile",
    );
    assert_eq!(state_a, 1);
    assert_eq!(intent_a.as_deref(), Some(token_found.as_str()));
    assert_eq!(job_id_a.as_deref(), Some("provider-job-a"));

    // Row B: settled unknown — terminal state (3), a cost_ledger estimate
    // row recorded.
    let (state_b, _intent_b, _job_id_b) = batch_request_row(
        &conn,
        &scope_id,
        "markdownize",
        "sha256:row-b-input",
        "sha256:row-b-profile",
    );
    assert_eq!(state_b, 3, "row B must be settled terminal");
    let settled_charge: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM cost_ledger
             WHERE scope_id = ?1 AND adapter_kind = 'markdownize' AND input_hash = ?2
               AND tool_profile_hash = ?3 AND outcome = 'unknown_settled' AND estimated = 1",
            params![scope_id, "sha256:row-b-input", "sha256:row-b-profile"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(settled_charge, 1, "row B must record an estimated charge");

    // Row C: unlistable — completely untouched (still state 1, original
    // intent_token, batch_job_id still NULL).
    let (state_c, intent_c, job_id_c) = batch_request_row(
        &conn,
        &scope_id,
        "markdownize",
        "sha256:row-c-input",
        "sha256:row-c-profile",
    );
    assert_eq!(state_c, 1, "unlistable row must not change state");
    assert_eq!(intent_c.as_deref(), Some(token_unlistable.as_str()));
    assert_eq!(
        job_id_c, None,
        "unlistable row must not gain a batch_job_id"
    );
}
