//! Step4b Mistral OCR Batch lane contract tests (04-pipeline.md §5.8 /
//! 07-adapter-spec.md §5.5): submit (phase 1 → 2a → 2b), poll/collect
//! (phase 3), the create_job crash window + `kio ledger reconcile` found
//! self-description, failed-job settlement, and the in-flight exit mapping.
//!
//! Harness conventions (the `kio()` runner with per-`Command` env — never
//! process-global mutation — the env denylist, tempdir XDG roots, `--json`)
//! are self-contained copies of `step4b_office_contract.rs` /
//! `step4b_ledger_reconcile_contract.rs`'s own (this crate's integration
//! test binaries do not share a `tests/common` module by convention).
//!
//! The hermetic seam is `KIO_TEST_MISTRAL_BATCH` (inline JSON script —
//! `kio-adapter/src/batch_client.rs`): `state_path` persists the poll
//! progression across separate CLI invocations, `capture_path` records every
//! client call as one JSON line. `KIO_TEST_MISTRAL_OCR=mock` is set alongside
//! it wherever the collect path resolves the online profile (network-free
//! model-pin resolution), mirroring the sync-lane tests.
//!
//! Scope config pins `[markdownize] bbox_annotation = false` before the
//! first index: (historical note — the batch input body now carries
//! `bbox_annotation_format`, so the Batch lane only engages for
//! bbox-disabled tasks (`markdownize_send_lane`'s documented gate).

use std::fs;
use std::path::PathBuf;

use assert_cmd::Command;
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
    "KIO_TEST_BATCH_INVENTORY",
    "KIO_TEST_MARKDOWNIZE_ADAPTER",
    "KIO_TEST_QUERY_EMBED_TRACE",
    "KIO_TEST_HOLD_LOCK_READY",
    "KIO_TEST_SCOPE_SEARCH_DELAY_SCOPE_ID",
    "KIO_TEST_SCOPE_SEARCH_DELAY_MS",
    "KIO_TEST_R13_2_AUTH",
    "KIO_TEST_R13_2_DECLARED",
    "KIO_TEST_R13_2_FALLBACK",
    "KIO_TEST_WINDOWS_PROFILE",
    "KIO_TEST_OFFICE_CONVERT",
    "KIO_OFFICE_CONVERTER",
    "KIO_TEST_CAPTURE_SENT_MEDIA",
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

fn init(dir: &TempDir) {
    kio(dir, &["init"]).assert().success();
    kio(dir, &["ledger", "init"]).assert().success();
}

/// A `%PDF` header followed by high-bit binary noise and no text layer (no
/// `BT`): local Prepare yields zero units, so the ONLY markdownize task is
/// the online OCR-from-scratch one (`step3_p0_contract.rs`'s ct4_bbox_006
/// fixture shape).
fn scanned_pdf_bytes() -> Vec<u8> {
    let mut scan = b"%PDF-1.4\n".to_vec();
    scan.extend((0u32..4000).map(|i| (i.wrapping_mul(97) & 0x7f) as u8 | 0x80));
    scan
}

fn ledger_path(dir: &TempDir) -> PathBuf {
    dir.path().join(".test-data/kio/cost-ledger.sqlite")
}

/// The single `batch_requests` row for `input_hash` (each test drives exactly
/// one online task): `(state, request_kind, intent_token, upload_id,
/// batch_job_id, provider_scope_id, job_create_started_at, error)`.
#[allow(clippy::type_complexity)]
fn batch_request_row(
    dir: &TempDir,
    input_hash: &str,
) -> (
    i64,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<String>,
) {
    let conn = Connection::open(ledger_path(dir)).unwrap();
    conn.query_row(
        "SELECT state, request_kind, intent_token, upload_id, batch_job_id, provider_scope_id,
                job_create_started_at, error
         FROM batch_requests WHERE input_hash = ?1",
        params![input_hash],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
            ))
        },
    )
    .unwrap()
}

/// All `cost_ledger` rows for `input_hash`: `(batch_job_id, usd, estimated,
/// outcome)` ordered by submission_seq.
fn cost_ledger_rows(dir: &TempDir, input_hash: &str) -> Vec<(String, f64, i64, String)> {
    let conn = Connection::open(ledger_path(dir)).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT batch_job_id, usd, estimated, outcome FROM cost_ledger
             WHERE input_hash = ?1 ORDER BY submission_seq",
        )
        .unwrap();
    let rows = stmt
        .query_map(params![input_hash], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap();
    rows.map(Result::unwrap).collect()
}

/// Parsed `capture_path` JSONL (one JSON object per client call, in call
/// order — the mock's own format).
fn capture_calls(path: &PathBuf) -> Vec<Value> {
    let raw = fs::read_to_string(path).unwrap_or_default();
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn call_names(calls: &[Value]) -> Vec<String> {
    calls
        .iter()
        .map(|call| call["call"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// The one markdownize task of the scanned-PDF fixture from `status --json`.
fn scanned_markdownize_task(status: &Value) -> Value {
    let tasks: Vec<&Value> = status["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|task| task["type"] == "markdownize")
        .collect();
    assert_eq!(
        tasks.len(),
        1,
        "the scanned fixture must carry exactly one markdownize task: {status}"
    );
    tasks[0].clone()
}

/// Scope setup shared by every test: scanned PDF fixture, `[markdownize]
/// bbox_annotation = false` (the Batch lane's bbox gate) written BEFORE the
/// first index so the task enqueues bbox-disabled, then `index --approve`
/// (records the scan approval AND publishes the persistent per-adapter
/// network opt-in + `allow_network = true`, exactly like the sync-lane tests
/// office_03/qa2 rely on). Returns the task's `input_hash`.
///
/// NOTE: the SUBMISSION itself happens on `kio batch resume`, not on `kio
/// index` — R10-7 ("an online markdownize task can only ever be DRIVEN by
/// `batch`") is unchanged by the Batch lane; index only enqueues.
fn setup_scanned_scope(dir: &TempDir) -> String {
    fs::write(dir.path().join("scan.pdf"), scanned_pdf_bytes()).unwrap();
    init(dir);
    // Merge-safe order: the bbox key is written first; `index --approve`'s
    // network-approval publish merges `allow_network` into the same file.
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[markdownize]\nbbox_annotation = false\n",
    )
    .unwrap();
    kio(dir, &["index", "--yes"])
        .env("KIO_TEST_MISTRAL_OCR", "mock")
        .arg("--json")
        .assert()
        .success();
    kio(dir, &["adapter", "approve", "--all", "--yes"])
        .assert()
        .success();
    let status = json_success(dir, &["status"]);
    let task = scanned_markdownize_task(&status);
    assert_eq!(task["status"], "pending", "{status}");
    assert!(
        task["output_ref"]
            .as_str()
            .is_some_and(|output_ref| output_ref.starts_with("online:")),
        "{status}"
    );
    task["input_hash"].as_str().unwrap().to_owned()
}

/// One `kio <args> --json` run under the batch mock script + the OCR mock,
/// asserting `expected_code` and returning the stdout JSON.
fn run_with_batch_script(
    dir: &TempDir,
    script: &Value,
    args: &[&str],
    expected_code: i32,
) -> Value {
    let mut script = script.clone();
    script["attribution_path"] = json!(dir.path().join("mistral-provider-jobs.json"));
    let assert = kio(dir, args)
        .env("KIO_TEST_MISTRAL_OCR", "mock")
        .env("KIO_TEST_MISTRAL_BATCH", script.to_string())
        .arg("--json")
        .assert()
        .code(expected_code);
    let stdout = assert.get_output().stdout.clone();
    serde_json::from_slice(&stdout).unwrap_or(Value::Null)
}

fn success_body(marker: &str) -> Value {
    json!({
        "model": "mistral-ocr-2505",
        "pages": [{ "index": 0, "markdown": format!("{marker} scanned page text\n") }],
    })
}

// ===========================================================================
// b1 — submit: ledger order (upload → create_job), metadata 5 keys, filename
//      convention, task stays pending, batch_requests row reaches state=1
// ===========================================================================

#[test]
fn b1_submit_records_upload_then_create_job_and_leaves_task_pending() {
    let dir = tempfile::tempdir().unwrap();
    let input_hash = setup_scanned_scope(&dir);
    let capture = dir.path().join("batch-capture.jsonl");
    let state = dir.path().join("batch-poll-state");
    let script = json!({
        "status_sequence": ["QUEUED", "SUCCESS"],
        "state_path": state.display().to_string(),
        "capture_path": capture.display().to_string(),
        "output": [{
            "custom_id": input_hash,
            "response": { "status_code": 200, "body": success_body("b1marker") },
        }],
    });

    // Submission happens on `batch resume` (in-flight only afterward → exit 3).
    let resumed = run_with_batch_script(&dir, &script, &["batch", "resume"], 3);
    assert_eq!(resumed["tasks_inflight"], 1, "{resumed}");
    assert_eq!(resumed["tasks_failed"], 0, "{resumed}");

    // Call order: upload strictly before create_job, nothing else sent.
    let calls = capture_calls(&capture);
    assert_eq!(
        call_names(&calls),
        vec!["upload", "create_job"],
        "{calls:?}"
    );

    // Ledger row: phase 2b completed — state=1, request_kind='batch', both
    // handles + provider scope + the durable job_create_started_at recorded.
    let (state_value, kind, token, upload_id, job_id, provider_scope, started_at, error) =
        batch_request_row(&dir, &input_hash);
    assert_eq!(state_value, 1);
    assert_eq!(kind, "batch");
    assert_eq!(job_id.as_deref(), Some("batch-mock-job-1"));
    assert_eq!(upload_id.as_deref(), Some("file-mock-upload-1"));
    assert_eq!(provider_scope.as_deref(), Some("mock-workspace"));
    assert!(started_at.is_some());
    assert!(error.is_none());
    let token = token.expect("an in-flight row keeps its intent_token");

    // Upload filename convention embeds the intent_token (04 §5.8 発見キー),
    // and the JSONL line carries custom_id = the task's FULL input_hash.
    assert_eq!(calls[0]["filename"], format!("kio-{token}.jsonl"));
    let uploaded_line: Value = serde_json::from_str(
        calls[0]["jsonl"]
            .as_str()
            .unwrap()
            .lines()
            .next()
            .expect("one JSONL line"),
    )
    .unwrap();
    assert_eq!(uploaded_line["custom_id"], input_hash);
    assert!(uploaded_line["body"]["document"].is_object());
    assert_eq!(uploaded_line["body"]["include_image_base64"], true);

    // create_job: metadata is EXACTLY the 5-key contract, and the model rides
    // the job (resolved pin under the hermetic seam).
    assert_eq!(calls[1]["input_file_id"], "file-mock-upload-1");
    assert_eq!(calls[1]["model"], "mistral-ocr-2505");
    let metadata = calls[1]["metadata"].as_object().unwrap();
    assert_eq!(metadata.len(), 5, "{metadata:?}");
    assert_eq!(metadata["intent_token"], json!(token));
    assert_eq!(metadata["adapter_kind"], "markdownize");
    assert_eq!(metadata["input_hash"], json!(input_hash));
    assert!(metadata["scope_id"].is_string());
    assert!(metadata["tool_profile_hash"].is_string());

    // The task stays Pending on the poll cadence — no Running flip, no Done.
    let status = json_success(&dir, &["status"]);
    let task = scanned_markdownize_task(&status);
    assert_eq!(task["status"], "pending", "{status}");
    assert_eq!(task["fallback_reason"], "batch_submitted", "{status}");
    assert!(task["next_retry_at"].is_string(), "{status}");
}

// ===========================================================================
// b2 — collect: QUEUED poll (exit 3) then SUCCESS poll → task Done, output
//      searchable, delete_upload issued, ledger settled (state=2, token NULL)
// ===========================================================================

#[test]
fn b2_collect_completes_task_and_makes_output_searchable() {
    let dir = tempfile::tempdir().unwrap();
    let input_hash = setup_scanned_scope(&dir);
    let capture = dir.path().join("batch-capture.jsonl");
    let state = dir.path().join("batch-poll-state");
    let script = json!({
        "status_sequence": ["QUEUED", "SUCCESS"],
        "state_path": state.display().to_string(),
        "capture_path": capture.display().to_string(),
        "output": [{
            "custom_id": input_hash,
            "response": { "status_code": 200, "body": success_body("b2marker") },
        }],
    });

    // resume #1: submit (in-flight → exit 3).
    run_with_batch_script(&dir, &script, &["batch", "resume"], 3);
    // resume #2: poll observes QUEUED — still in flight (exit 3), task pending.
    let polled = run_with_batch_script(&dir, &script, &["batch", "resume"], 3);
    assert_eq!(polled["tasks_inflight"], 1, "{polled}");
    let status = json_success(&dir, &["status"]);
    assert_eq!(scanned_markdownize_task(&status)["status"], "pending");
    let (state_value, ..) = batch_request_row(&dir, &input_hash);
    assert_eq!(state_value, 1, "still in flight after a QUEUED poll");

    // resume #3: poll observes SUCCESS → collect + materialize + promote.
    let collected = run_with_batch_script(&dir, &script, &["batch", "resume"], 0);
    assert_eq!(collected["tasks_executed"], 1, "{collected}");
    let status = json_success(&dir, &["status"]);
    let task = scanned_markdownize_task(&status);
    assert_eq!(task["status"], "done", "{status}");
    assert_eq!(task["fallback_reason"], "online_adapter_done", "{status}");
    assert!(
        !task["output_ref"].as_str().unwrap().starts_with("online:"),
        "{status}"
    );

    // The collected markdown is searchable (promotion + index rebuild ran).
    let search = json_success(&dir, &["search", "b2marker", "--mode", "text"]);
    let results = search["results"].as_array().unwrap();
    assert!(!results.is_empty(), "{search}");
    assert!(
        results.iter().any(|result| result["title"] == "scan.pdf"),
        "{search}"
    );

    // Cleanup-first residue deletion happened via the client.
    let calls = call_names(&capture_calls(&capture));
    assert!(calls.contains(&"delete_upload".to_owned()), "{calls:?}");
    assert!(calls.contains(&"fetch_output".to_owned()), "{calls:?}");

    // Ledger: success terminal (state=2), token cleared AFTER cleanup, and
    // one succeeded cost_ledger row keyed by the REAL job id.
    let (state_value, kind, token, _upload, job_id, _scope, _started, error) =
        batch_request_row(&dir, &input_hash);
    assert_eq!(state_value, 2);
    assert_eq!(kind, "batch");
    assert!(token.is_none(), "intent_token clears only after cleanup");
    assert_eq!(job_id.as_deref(), Some("batch-mock-job-1"));
    assert!(error.is_none());
    let charges = cost_ledger_rows(&dir, &input_hash);
    assert_eq!(charges.len(), 1, "{charges:?}");
    let (ledger_job_id, _usd, _estimated, outcome) = &charges[0];
    assert_eq!(ledger_job_id, "batch-mock-job-1");
    assert_eq!(outcome, "succeeded");
}

// ===========================================================================
// b3 — create_job crash window: job_create_started recorded + job id NULL,
//      then `kio ledger reconcile` self-describes the found job, then resume
//      collects it to Done
// ===========================================================================

#[test]
fn b3_create_job_crash_window_reconcile_found_then_resume_collects() {
    let dir = tempfile::tempdir().unwrap();
    let input_hash = setup_scanned_scope(&dir);
    let capture = dir.path().join("batch-capture.jsonl");
    let state = dir.path().join("batch-poll-state");

    // Phase 1: the scripted create_job failure (the §5.8 crash window between
    // job_create_started_at and batch_job_id).
    let failing = json!({
        "fail_phase": "create_job",
        "capture_path": capture.display().to_string(),
    });
    let failed = run_with_batch_script(&dir, &failing, &["batch", "resume"], 3);
    assert_eq!(failed["tasks_failed"], 1, "{failed}");
    let (state_value, kind, token, upload_id, job_id, provider_scope, started_at, _error) =
        batch_request_row(&dir, &input_hash);
    assert_eq!(state_value, 0, "phase 2b never completed");
    assert_eq!(kind, "batch");
    assert!(job_id.is_none(), "the crash window: no job id recorded");
    assert!(started_at.is_some(), "job_create_started_at IS recorded");
    assert_eq!(upload_id.as_deref(), Some("file-mock-upload-1"));
    assert_eq!(provider_scope.as_deref(), Some("mock-workspace"));
    let token = token.expect("the failed attempt keeps its intent_token");
    let status = json_success(&dir, &["status"]);
    let task = scanned_markdownize_task(&status);
    assert_eq!(task["status"], "failed", "{status}");
    assert_eq!(task["fallback_reason"], "network_error", "{status}");

    // Phase 2: the provider-side job DOES exist (metadata carries the same
    // token + task key 4-tuple) — `kio ledger reconcile` matches it by
    // intent_token and self-describes batch_job_id onto the row (04 §5.8
    // found 自己記述化).
    let scope_id = {
        let scope: Value =
            serde_json::from_str(&fs::read_to_string(dir.path().join(".kio/scope.json")).unwrap())
                .unwrap();
        scope["scope_id"].as_str().unwrap().to_owned()
    };
    let conn = Connection::open(ledger_path(&dir)).unwrap();
    let tool_profile_hash: String = conn
        .query_row(
            "SELECT tool_profile_hash FROM batch_requests WHERE input_hash = ?1",
            params![input_hash],
            |row| row.get(0),
        )
        .unwrap();
    drop(conn);
    let listing_metadata = json!({
        "intent_token": token,
        "scope_id": scope_id,
        "adapter_kind": "markdownize",
        "input_hash": input_hash,
        "tool_profile_hash": tool_profile_hash,
    });
    let recovered = json!({
        "status_sequence": ["SUCCESS"],
        "state_path": state.display().to_string(),
        "capture_path": capture.display().to_string(),
        "job_metadata": listing_metadata,
        "jobs_listing": [{
            "job_id": "batch-mock-job-1",
            "status": "QUEUED",
            "metadata": listing_metadata,
        }],
        "uploads_listing": [{
            "upload_id": "file-mock-upload-1",
            "filename": format!("kio-{token}.jsonl"),
        }],
        "output": [{
            "custom_id": input_hash,
            "response": { "status_code": 200, "body": success_body("b3marker") },
        }],
    });
    let reconciled = run_with_batch_script(&dir, &recovered, &["ledger", "reconcile"], 0);
    assert_eq!(reconciled["batch_found"], 1, "{reconciled}");
    let (state_value, _kind, token_after, _upload, job_id, ..) =
        batch_request_row(&dir, &input_hash);
    assert_eq!(
        job_id.as_deref(),
        Some("batch-mock-job-1"),
        "found self-description recorded the discovered job id"
    );
    assert_eq!(
        state_value, 0,
        "self-description does not fabricate state=1"
    );
    assert_eq!(token_after.as_deref(), Some(token.as_str()));

    // Phase 3: resume polls the self-described row and collects it to Done.
    let collected = run_with_batch_script(&dir, &recovered, &["batch", "resume"], 0);
    assert_eq!(collected["tasks_executed"], 1, "{collected}");
    let status = json_success(&dir, &["status"]);
    assert_eq!(scanned_markdownize_task(&status)["status"], "done");
    let search = json_success(&dir, &["search", "b3marker", "--mode", "text"]);
    assert!(
        !search["results"].as_array().unwrap().is_empty(),
        "{search}"
    );
    let (state_value, _kind, token, ..) = batch_request_row(&dir, &input_hash);
    assert_eq!(state_value, 2);
    assert!(token.is_none());
}

// ===========================================================================
// b4 — failed job: reject terminal (state=3), zero confirmed charge, upload
//      cleaned, task permanently failed
// ===========================================================================

#[test]
fn b4_failed_job_settles_zero_charge_and_fails_task_permanently() {
    let dir = tempfile::tempdir().unwrap();
    let input_hash = setup_scanned_scope(&dir);
    let capture = dir.path().join("batch-capture.jsonl");
    let state = dir.path().join("batch-poll-state");
    let script = json!({
        "status_sequence": ["FAILED"],
        "state_path": state.display().to_string(),
        "capture_path": capture.display().to_string(),
    });

    run_with_batch_script(&dir, &script, &["batch", "resume"], 3);
    // The poll observes FAILED → permanent failure only → exit 4.
    let polled = run_with_batch_script(&dir, &script, &["batch", "resume"], 4);
    assert_eq!(polled["tasks_failed"], 1, "{polled}");
    assert_eq!(polled["tasks_inflight"], 0, "{polled}");

    let status = json_success(&dir, &["status"]);
    let task = scanned_markdownize_task(&status);
    assert_eq!(task["status"], "failed", "{status}");
    assert_eq!(
        task["fallback_reason"], "invalid_input",
        "provider job failure joins the permanent failure mapping: {status}"
    );
    assert!(task["next_retry_at"].is_null(), "{status}");

    // Ledger: reject terminal — state=3, error='failed', token cleared after
    // upload cleanup, and the confirmed charge is ZERO (usd=0, estimated=0 —
    // Mistral does not bill failed batch entries).
    let (state_value, _kind, token, _upload, job_id, _scope, _started, error) =
        batch_request_row(&dir, &input_hash);
    assert_eq!(state_value, 3);
    assert_eq!(error.as_deref(), Some("failed"));
    assert!(token.is_none(), "cleanup completed → token cleared");
    assert_eq!(job_id.as_deref(), Some("batch-mock-job-1"));
    let charges = cost_ledger_rows(&dir, &input_hash);
    assert_eq!(charges.len(), 1, "{charges:?}");
    let (ledger_job_id, usd, estimated, outcome) = &charges[0];
    assert_eq!(ledger_job_id, "batch-mock-job-1");
    assert_eq!(*usd, 0.0);
    assert_eq!(*estimated, 0, "a confirmed zero charge, not an estimate");
    assert_eq!(outcome, "submit_rejected");
    let calls = call_names(&capture_calls(&capture));
    assert!(calls.contains(&"delete_upload".to_owned()), "{calls:?}");
}

// ===========================================================================
// b5 — in-flight only → exit 3 on every poll until the provider progresses
// ===========================================================================

#[test]
fn b5_inflight_only_resume_exits_retryable() {
    let dir = tempfile::tempdir().unwrap();
    let input_hash = setup_scanned_scope(&dir);
    let state = dir.path().join("batch-poll-state");
    let script = json!({
        "status_sequence": ["QUEUED", "RUNNING", "SUCCESS"],
        "state_path": state.display().to_string(),
    });

    // Submit, then two consecutive in-flight polls: exit 3 every time, the
    // task never leaves pending, the row never leaves state=1.
    run_with_batch_script(&dir, &script, &["batch", "resume"], 3);
    for _ in 0..2 {
        let polled = run_with_batch_script(&dir, &script, &["batch", "resume"], 3);
        assert_eq!(polled["tasks_inflight"], 1, "{polled}");
        assert_eq!(polled["tasks_executed"], 0, "{polled}");
        assert_eq!(polled["tasks_failed"], 0, "{polled}");
        let status = json_success(&dir, &["status"]);
        assert_eq!(scanned_markdownize_task(&status)["status"], "pending");
        let (state_value, ..) = batch_request_row(&dir, &input_hash);
        assert_eq!(state_value, 1);
    }
}
