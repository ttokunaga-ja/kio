//! Embedding Batch lane contract (07 §5.3 の 2026-07-24 訂正 / §5.7, 04 §5.8).
//!
//! The Gemini Developer API bills an embedding batch at half the sync rate, so
//! Batch is the default lane. Unlike the OCR lane its input is INLINE, so there
//! is no upload phase and no provider residue — which is exactly why terminal
//! here IS cleanup-complete and the `intent_token` must be cleared.
//!
//! **1 job = 1 task** is preserved by making the JOB the task: the batch row's
//! `input_hash` digests the job's member set. These tests pin the observable
//! consequences of that choice.

use std::path::Path;

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

fn fixture_dir() -> TempDir {
    // macOS's per-user TMPDIR has an inherited ACL.  v1 correctly declines to
    // create durable private state below that unchecked path; the system
    // sticky temporary root is an explicitly permitted ancestor instead.
    #[cfg(target_os = "macos")]
    {
        tempfile::tempdir_in("/private/tmp").unwrap()
    }
    #[cfg(not(target_os = "macos"))]
    {
        tempfile::tempdir().unwrap()
    }
}

fn root(dir: &TempDir) -> std::path::PathBuf {
    dir.path().join("repo")
}

fn kio(dir: &TempDir) -> Command {
    let mut command = Command::cargo_bin("kio").unwrap();
    command.env_clear();
    // Keep the device-scoped ledger and approvals beneath an existing,
    // owner-private fixture home.  The v1 private-state lifecycle deliberately
    // refuses to create this state below an unchecked temporary ancestor.
    let home = dir.path().join("home");
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    command
        .current_dir(root(dir))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("TMPDIR", home.join("tmp"))
        .env("TEMP", home.join("tmp"))
        .env("TMP", home.join("tmp"))
        .env("KIO_TEST_MARKDOWNIZE_ADAPTER", "deterministic")
        .env("KIO_TEST_GEMINI_EMBED", "mock")
        .env_remove("GEMINI_API_KEY")
        .env_remove("MISTRAL_API_KEY");
    command
}

fn script_with_attribution(dir: &TempDir, raw: &str) -> String {
    let mut script: Value = serde_json::from_str(raw).unwrap();
    script["attribution_path"] = serde_json::json!(dir.path().join("gemini-provider-jobs.json"));
    script.to_string()
}

fn json(dir: &TempDir, batch_script: &str, args: &[&str]) -> Value {
    let assert = kio(dir)
        .env(
            "KIO_TEST_GEMINI_BATCH",
            script_with_attribution(dir, batch_script),
        )
        .arg("--json")
        .args(args)
        .assert()
        .success();
    serde_json::from_slice(&assert.get_output().stdout).unwrap()
}

/// Like [`json`] but tolerates the documented non-zero exits: a pass that
/// leaves batch work in flight or terminally failed reports
/// `KIO-E-BATCH-PARTIAL-001` (exit 3) by design.
fn json_any(dir: &TempDir, batch_script: &str, args: &[&str]) -> Value {
    let output = kio(dir)
        .env(
            "KIO_TEST_GEMINI_BATCH",
            script_with_attribution(dir, batch_script),
        )
        .arg("--json")
        .args(args)
        .output()
        .unwrap();
    serde_json::from_slice(&output.stdout).unwrap()
}

/// Run and discard the result. For the failure paths whose report goes to
/// stderr, where the caller only cares about the state left behind.
fn run_ignoring_output(dir: &TempDir, batch_script: &str, args: &[&str]) {
    let _ = kio(dir)
        .env(
            "KIO_TEST_GEMINI_BATCH",
            script_with_attribution(dir, batch_script),
        )
        .arg("--json")
        .args(args)
        .output()
        .unwrap();
}

fn ledger_query(dir: &TempDir, sql: &str) -> String {
    let db = dir.path().join("home/data/kio/cost-ledger.sqlite");
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row(sql, [], |row| row.get::<_, String>(0))
        .unwrap_or_default()
}

fn approve_online_profiles(dir: &TempDir) {
    // `index --online` is the explicit external-send mode for the whole
    // pipeline.  Grant the deterministic Markdown fixture's exact active
    // profile as well as the Gemini embedding profile; neither grant is
    // inferred from the other.
    kio(dir)
        .args(["adapter", "approve", "mistral_ocr_markdownize", "--yes"])
        .assert()
        .success();
    kio(dir)
        .args(["adapter", "approve", "gemini_embedding_2", "--yes"])
        .assert()
        .success();
}

/// Prepare the exact owner-private device home required by the v1 ledger and
/// grant stores before any CLI command tries to initialize them.
fn prepare_private_home(dir: &TempDir) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let home = dir.path().join("home");
    let root = root(dir);
    std::fs::create_dir(&root).unwrap();
    for path in [
        &root,
        &home,
        &home.join("data"),
        &home.join("config"),
        &home.join("cache"),
        &home.join("tmp"),
    ] {
        std::fs::create_dir_all(path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
}

/// A scope with one indexable document, not yet indexed.
fn scope() -> TempDir {
    let dir = fixture_dir();
    prepare_private_home(&dir);
    std::fs::write(
        root(&dir).join("auth.md"),
        "# 認証仕様\n\nトークン TTL は 3600 秒です。\n",
    )
    .unwrap();
    kio(&dir).arg("init").assert().success();
    kio(&dir).args(["ledger", "init"]).assert().success();
    approve_online_profiles(&dir);
    dir
}

/// The chunk id the submit carried, read back from the mock's capture log.
fn submitted_key(capture: &Path) -> String {
    let line = std::fs::read_to_string(capture).unwrap();
    let first = line.lines().next().unwrap();
    let value: Value = serde_json::from_str(first).unwrap();
    value["keys"][0].as_str().unwrap().to_owned()
}

/// Every chunk id the first submit carried.
fn submitted_keys(capture: &Path) -> Vec<String> {
    let line = std::fs::read_to_string(capture).unwrap();
    let first = line.lines().next().unwrap();
    let value: Value = serde_json::from_str(first).unwrap();
    value["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|key| key.as_str().unwrap().to_owned())
        .collect()
}

/// Every chunk id any submit carried, deduplicated — for fixtures with more
/// than one job, where a per-job key list would fail the bijection check.
fn all_submitted_keys(capture: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(capture).unwrap();
    let mut keys = Vec::new();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value["call"] != "create_embedding_job" {
            continue;
        }
        for key in value["keys"].as_array().into_iter().flatten() {
            let key = key.as_str().unwrap().to_owned();
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }
    keys
}

fn success_script(job: &str, key: &str) -> String {
    // A unit vector: 768 identical components of 1/sqrt(768).
    let component = 1.0f64 / (768.0f64).sqrt();
    let values = vec![component; 768];
    serde_json::json!({
        "state_sequence": ["BATCH_STATE_SUCCEEDED"],
        "job_name": job,
        "inlined_responses": [{
            "metadata": { "key": key },
            "output": { "response": {
                "embedding": { "values": values },
                // The live endpoint reports this per line; the fixture must
                // too, or the settlement path it drives is never exercised.
                "usageMetadata": { "promptTokenCount": 40 },
            } },
        }],
    })
    .to_string()
}

/// Same job, but the provider omitted the usage report.
fn success_script_without_usage(job: &str, key: &str) -> String {
    let component = 1.0f64 / (768.0f64).sqrt();
    serde_json::json!({
        "state_sequence": ["BATCH_STATE_SUCCEEDED"],
        "job_name": job,
        "inlined_responses": [{
            "metadata": { "key": key },
            "output": { "response": { "embedding": { "values": vec![component; 768] } } },
        }],
    })
    .to_string()
}

/// I4's degrade path. A provider that reports no token count must still settle
/// — at the conservative reservation, flagged `estimated`. Trusting a missing
/// report as zero would settle the charge at $0 and let the budget cap release
/// spending the provider has already billed.
#[test]
fn a_result_without_a_usage_report_settles_at_the_reservation_estimate() {
    let dir = scope();
    let capture = root(&dir).join("capture.jsonl");
    json(
        &dir,
        &serde_json::json!({
            "state_sequence": ["BATCH_STATE_PENDING"],
            "job_name": "batches/no-usage",
            "capture_path": capture.to_string_lossy(),
        })
        .to_string(),
        &["index", "--online"],
    );
    let key = submitted_key(&capture);

    let resumed = json(
        &dir,
        &success_script_without_usage("batches/no-usage", &key),
        &["batch", "resume"],
    );
    assert_eq!(resumed["tasks_executed"], 1, "{resumed}");
    assert_eq!(
        ledger_query(
            &dir,
            "SELECT outcome || '|' || estimated FROM cost_ledger WHERE adapter_kind = 'embedding'"
        ),
        "succeeded|1",
        "a missing usage report must fall back to the estimate, not to zero"
    );
    assert!(
        ledger_query(
            &dir,
            "SELECT CAST(usd AS TEXT) FROM cost_ledger WHERE adapter_kind = 'embedding'"
        )
        .parse::<f64>()
        .unwrap()
            > 0.0,
        "the fallback charge must be the reservation, never 0"
    );
}

/// `index --online` on the default (Batch) lane SUBMITS a job and returns; the
/// vectors arrive later, from `batch resume`. The ledger row it leaves behind
/// is a `request_kind='batch'` row in state 1 (JobCreated) carrying the
/// provider's job name — the key the §5.8 recovery walk needs.
#[test]
fn index_submits_a_batch_job_instead_of_embedding_inline() {
    let dir = scope();
    let capture = root(&dir).join("capture.jsonl");
    let submit_script = serde_json::json!({
        "state_sequence": ["BATCH_STATE_PENDING"],
        "job_name": "batches/contract-1",
        "capture_path": capture.to_string_lossy(),
    })
    .to_string();

    let indexed = json(&dir, &submit_script, &["index", "--online"]);
    assert_eq!(indexed["status"], "indexed", "{indexed}");
    assert_eq!(
        indexed["embedding_tasks_executed"], 0,
        "the Batch lane must not embed inline: {indexed}"
    );

    // The adapter call carried the model, the truncated dimensionality, and the
    // intent_token as the job's displayName (the §5.8 発見キー).
    let captured: Value = serde_json::from_str(
        std::fs::read_to_string(&capture)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(captured["call"], "create_embedding_job", "{captured}");
    assert_eq!(captured["model"], "gemini-embedding-2", "{captured}");
    assert_eq!(captured["dimensions"], 768, "{captured}");
    assert!(
        captured["display_name"]
            .as_str()
            .unwrap()
            .starts_with("kio-"),
        "the intent_token rides in displayName: {captured}"
    );

    assert_eq!(
        ledger_query(
            &dir,
            "SELECT request_kind || '|' || state || '|' || COALESCE(batch_job_id, '')
             FROM batch_requests WHERE adapter_kind = 'embedding'"
        ),
        "batch|1|batches/contract-1"
    );
}

/// `batch resume` polls, collects the inline results, writes the vectors, and
/// settles the row — including clearing the `intent_token`, which an inline
/// lane MUST do (no upload residue exists, so terminal is cleanup-complete;
/// leaving it set would permanently block re-submission of that task key).
#[test]
fn batch_resume_collects_the_vectors_and_clears_the_intent_token() {
    let dir = scope();
    let capture = root(&dir).join("capture.jsonl");
    let submit_script = serde_json::json!({
        "state_sequence": ["BATCH_STATE_PENDING"],
        "job_name": "batches/contract-2",
        "capture_path": capture.to_string_lossy(),
    })
    .to_string();
    json(&dir, &submit_script, &["index", "--online"]);
    let key = submitted_key(&capture);

    let resumed = json(
        &dir,
        &success_script("batches/contract-2", &key),
        &["batch", "resume"],
    );
    assert_eq!(resumed["tasks_executed"], 1, "{resumed}");
    assert_eq!(resumed["tasks_failed"], 0, "{resumed}");

    assert_eq!(
        ledger_query(
            &dir,
            "SELECT state || '|' || (intent_token IS NULL)
             FROM batch_requests WHERE adapter_kind = 'embedding'"
        ),
        "2|1",
        "terminal state with the token cleared"
    );
    // I4: settled on the token count the provider reported, at the BATCH rate
    // — `estimated=0`. This used to be `1` on the belief that the endpoint
    // reports no usage, which the wire contradicts.
    assert_eq!(
        ledger_query(
            &dir,
            "SELECT outcome || '|' || estimated FROM cost_ledger WHERE adapter_kind = 'embedding'"
        ),
        "succeeded|0"
    );

    // The collected vector is real: an explicit vector search resolves with it.
    let searched = json(
        &dir,
        &success_script("batches/contract-2", &key),
        &["search", "認証", "--mode", "vector"],
    );
    assert_eq!(searched["resolved_mode"], "vector", "{searched}");
    assert_eq!(
        searched["results"].as_array().unwrap().len(),
        1,
        "{searched}"
    );
}

/// I12: the SYNC query embed settles on the tokens the endpoint reported, not
/// on its reservation.
///
/// The Batch lane learned to read `usage` while this path went on booking the
/// estimate, guarded by four doc comments asserting the endpoint reports no
/// token count — an assertion the live wire had already contradicted. Nothing
/// caught it because `run_adopted_embedding` dropped `response.usage` at the
/// crate boundary, so no call site could have used it even had it wanted to.
/// `kio search` is the highest-frequency send in the product; it should be the
/// last one billing on a guess.
#[test]
fn a_sync_query_embed_settles_on_the_reported_tokens() {
    let dir = scope();
    let capture = root(&dir).join("capture.jsonl");
    let submit = serde_json::json!({
        "state_sequence": ["BATCH_STATE_PENDING"],
        "job_name": "batches/query-usage",
        "capture_path": capture.to_string_lossy(),
    })
    .to_string();
    json(&dir, &submit, &["index", "--online"]);
    let key = submitted_key(&capture);
    json(
        &dir,
        &success_script("batches/query-usage", &key),
        &["batch", "resume"],
    );

    // 8 ASCII scalars in, and the mock reports one token per scalar the way the
    // live endpoint reports `usageMetadata.promptTokenCount` for the call.
    json(
        &dir,
        &success_script("batches/query-usage", &key),
        &["search", "rollback", "--mode", "vector"],
    );

    assert_eq!(
        ledger_query(
            &dir,
            "SELECT outcome || '|' || estimated || '|' || printf('%.10f', usd)
             FROM cost_ledger WHERE scope_id = 'device' AND adapter_kind = 'embedding'"
        ),
        // 8 tokens at the SYNC rate ($0.20 / 1M) — a query cannot wait out a
        // batch turnaround, so it is never eligible for the half rate.
        "succeeded|0|0.0000016000",
        "the query embed must settle on reported tokens at the sync rate"
    );
}

/// I12's fallback: a response with no usage report keeps the reservation and
/// stays `estimated=1`. Reading a missing count as zero would settle a real
/// send at $0.00 — the one direction a budget cap cannot defend against.
#[test]
fn a_sync_query_embed_without_a_usage_report_keeps_the_reservation() {
    let dir = scope();
    let capture = root(&dir).join("capture.jsonl");
    let submit = serde_json::json!({
        "state_sequence": ["BATCH_STATE_PENDING"],
        "job_name": "batches/query-no-usage",
        "capture_path": capture.to_string_lossy(),
    })
    .to_string();
    json(&dir, &submit, &["index", "--online"]);
    let key = submitted_key(&capture);
    json(
        &dir,
        &success_script("batches/query-no-usage", &key),
        &["batch", "resume"],
    );

    let _ = kio(&dir)
        .env("KIO_TEST_GEMINI_EMBED", "no_usage_report")
        .env(
            "KIO_TEST_GEMINI_BATCH",
            script_with_attribution(&dir, &success_script("batches/query-no-usage", &key)),
        )
        .args(["--json", "search", "rollback", "--mode", "vector"])
        .output()
        .unwrap();

    assert_eq!(
        ledger_query(
            &dir,
            "SELECT outcome || '|' || estimated
             FROM cost_ledger WHERE scope_id = 'device' AND adapter_kind = 'embedding'"
        ),
        "succeeded|1",
        "no usage report must degrade to the reservation, not to zero"
    );
}

/// A job that ends in a non-success terminal state settles the row rather than
/// leaving it in flight forever, and records the reservation (the provider may
/// still have billed part of the work).
#[test]
fn a_failed_batch_job_terminates_the_row_instead_of_hanging() {
    let dir = scope();
    let capture = root(&dir).join("capture.jsonl");
    json(
        &dir,
        &serde_json::json!({
            "state_sequence": ["BATCH_STATE_PENDING"],
            "job_name": "batches/contract-3",
            "capture_path": capture.to_string_lossy(),
        })
        .to_string(),
        &["index", "--online"],
    );

    let failed_script = serde_json::json!({
        "state_sequence": ["BATCH_STATE_FAILED"],
        "job_name": "batches/contract-3",
    })
    .to_string();
    let resumed = json_any(&dir, &failed_script, &["batch", "resume"]);
    assert_eq!(resumed["tasks_executed"], 0, "{resumed}");

    // The failed job WAS settled: a terminal ledger entry exists for it, and it
    // is not `succeeded`. (The row itself is not asserted here because the same
    // `batch resume` pass then re-drives enrichment and submits a fresh job for
    // the same member set — see the KNOWN GAP below.)
    assert_eq!(
        ledger_query(
            &dir,
            "SELECT outcome FROM cost_ledger
             WHERE adapter_kind = 'embedding' AND outcome <> 'succeeded'
             ORDER BY rowid LIMIT 1"
        ),
        "expired",
        "a non-success terminal must be recorded, not left in flight"
    );
}

/// A failed job IS retried — but a bounded number of times.
///
/// R24 (6 系統全会一致) closed what this test previously pinned as a KNOWN GAP:
/// a job that fails settles the row and NULLs its `intent_token`, and
/// `phase1_intent`'s ON CONFLICT clears `batch_job_id`, so the same pass
/// re-reserved the same task key and created another job — forever. A failed
/// send carries no usage to settle on (measured settlement covers the success
/// path only), so each pass recorded a full estimate for work that produced
/// nothing, until the budget cap hard-stopped the scope.
///
/// The bound rides on `batch_requests.attempts`, which that ON CONFLICT clause
/// does NOT reset. Here one failure is retried (attempts 1 < 3), which is the
/// behavior we want; `a_permanently_failing_job_stops_being_resubmitted` pins
/// the other end.
#[test]
fn a_failed_job_is_retried_within_the_same_pass() {
    let dir = scope();
    let capture = root(&dir).join("capture.jsonl");
    json(
        &dir,
        &serde_json::json!({
            "state_sequence": ["BATCH_STATE_PENDING"],
            "job_name": "batches/gap-1",
            "capture_path": capture.to_string_lossy(),
        })
        .to_string(),
        &["index", "--online"],
    );
    json_any(
        &dir,
        &serde_json::json!({
            "state_sequence": ["BATCH_STATE_FAILED"],
            "job_name": "batches/gap-1",
            "capture_path": capture.to_string_lossy(),
        })
        .to_string(),
        &["batch", "resume"],
    );
    let creates = std::fs::read_to_string(&capture)
        .unwrap()
        .lines()
        .filter(|line| line.contains("create_embedding_job"))
        .count();
    assert_eq!(
        creates, 2,
        "a first failure is still retried — the bound is on repetition, not on retrying at all"
    );
}

/// R24 (6 系統全会一致): the retry budget actually runs out. Repeated
/// `batch resume` passes against a job that always fails must stop creating
/// jobs — and stop charging for them — rather than re-reserving forever.
#[test]
fn a_permanently_failing_job_stops_being_resubmitted() {
    let dir = scope();
    let capture = root(&dir).join("capture.jsonl");
    json(
        &dir,
        &serde_json::json!({
            "state_sequence": ["BATCH_STATE_PENDING"],
            "job_name": "batches/gap-2",
            "capture_path": capture.to_string_lossy(),
        })
        .to_string(),
        &["index", "--online"],
    );
    let failing = serde_json::json!({
        "state_sequence": ["BATCH_STATE_FAILED"],
        "job_name": "batches/gap-2",
        "capture_path": capture.to_string_lossy(),
    })
    .to_string();
    // Far more passes than the retry budget allows.
    for _ in 0..8 {
        json_any(&dir, &failing, &["batch", "resume"]);
    }
    let creates = std::fs::read_to_string(&capture)
        .unwrap()
        .lines()
        .filter(|line| line.contains("create_embedding_job"))
        .count();
    assert!(
        creates <= 3,
        "the retry budget must cap job creation; created {creates} jobs across 8 passes"
    );

    // And the charging stops with it: no more cost_ledger rows than jobs.
    let db = dir.path().join("home/data/kio/cost-ledger.sqlite");
    let conn = rusqlite::Connection::open(db).unwrap();
    let charges: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM cost_ledger WHERE adapter_kind = 'embedding'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        charges <= 3,
        "a permanently failing member set must not accrue an unbounded number of \
         settled charges; found {charges}"
    );
}

/// A still-running job is reported as in-flight and left alone — polling is
/// idempotent and must not settle or re-charge anything.
#[test]
fn a_running_batch_job_stays_in_flight_across_repeated_polls() {
    let dir = scope();
    let capture = root(&dir).join("capture.jsonl");
    json(
        &dir,
        &serde_json::json!({
            "state_sequence": ["BATCH_STATE_PENDING"],
            "job_name": "batches/contract-4",
            "capture_path": capture.to_string_lossy(),
        })
        .to_string(),
        &["index", "--online"],
    );
    let running = serde_json::json!({
        "state_sequence": ["BATCH_STATE_RUNNING"],
        "job_name": "batches/contract-4",
    })
    .to_string();

    for _ in 0..2 {
        let resumed = json_any(&dir, &running, &["batch", "resume"]);
        assert_eq!(resumed["tasks_executed"], 0, "{resumed}");
        assert_eq!(
            ledger_query(
                &dir,
                "SELECT state || '|' || (intent_token IS NULL)
                 FROM batch_requests WHERE adapter_kind = 'embedding'"
            ),
            "1|0",
            "still JobCreated with its token held"
        );
    }
    // Exactly one ledger row for the job — repeated polls never fan out.
    assert_eq!(
        ledger_query(
            &dir,
            "SELECT CAST(COUNT(*) AS TEXT) FROM batch_requests WHERE adapter_kind = 'embedding'"
        ),
        "1"
    );
}

/// `--realtime` moves the send to the synchronous lane, which completes inside
/// the invocation. The ledger records a `sync` row, never a `batch` one — the
/// two lanes must not both fire for one logical embedding.
#[test]
fn realtime_uses_the_synchronous_lane_and_never_creates_a_batch_row() {
    let dir = scope();
    let capture = root(&dir).join("capture.jsonl");
    let script = serde_json::json!({
        "state_sequence": ["BATCH_STATE_PENDING"],
        "job_name": "batches/contract-5",
        "capture_path": capture.to_string_lossy(),
    })
    .to_string();

    let indexed = json(&dir, &script, &["index", "--online", "--realtime"]);
    assert_eq!(indexed["status"], "indexed", "{indexed}");
    assert_eq!(
        indexed["embedding_tasks_executed"], 1,
        "realtime completes in-invocation: {indexed}"
    );
    assert!(
        !capture.exists(),
        "the batch client must not be called at all on the realtime lane"
    );
    assert_eq!(
        ledger_query(
            &dir,
            "SELECT COALESCE(MIN(request_kind), 'none')
             FROM batch_requests WHERE adapter_kind = 'embedding'"
        ),
        "sync"
    );
}

/// A realtime pass over several groups sends one call per group, so EVERY row
/// settles on a token count the provider reported for that row.
///
/// `:batchEmbedContents` reports one count per CALL. Batching groups into one
/// call — which `EMBEDDING_BATCH_SIZE` used to do, up to 32 at a time — leaves
/// nothing that can honestly settle a single row, and the dogfood run showed
/// what that cost: 1 of 30 sync settlements carried a measured figure, and
/// every realtime chunk embed fell back to a reservation that I10 measured at
/// -32%..+52%. One call per row removes the question instead of answering it.
///
/// The inputs are pure ASCII, where `estimate_embedding_tokens` counts one
/// token per 4 scalars and the mock reports one per scalar — so a settled row
/// is exactly 4x its own reservation, and "did it settle on the measurement"
/// has an arithmetic answer rather than an approximate one.
#[test]
fn every_row_of_a_realtime_pass_settles_on_its_own_reported_tokens() {
    let dir = fixture_dir();
    prepare_private_home(&dir);
    for (name, body) in [
        (
            "alpha.md",
            "# Rollback drill\n\nThe checkout gateway retries twice.\n",
        ),
        (
            "beta.md",
            "# Capacity plan\n\nQueue depth stays under four hundred.\n",
        ),
        (
            "gamma.md",
            "# Handoff notes\n\nThe operator owns the bridge decision.\n",
        ),
    ] {
        std::fs::write(root(&dir).join(name), body).unwrap();
    }
    kio(&dir).arg("init").assert().success();
    kio(&dir).args(["ledger", "init"]).assert().success();
    approve_online_profiles(&dir);

    let indexed = json(&dir, "{}", &["index", "--online", "--realtime"]);
    let executed = indexed["embedding_tasks_executed"].as_u64().unwrap();
    assert!(
        executed > 1,
        "this contract needs a pass covering several rows: {indexed}"
    );

    // Every row is its own call, so every row is measured — `estimated=0`
    // across the board, with no apportioned share left anywhere.
    assert_eq!(
        ledger_query(
            &dir,
            "SELECT COUNT(*) || '|' || SUM(estimated)
             FROM cost_ledger WHERE adapter_kind = 'embedding' AND scope_id <> 'device'"
        ),
        format!("{executed}|0"),
        "a per-call row is measured, not estimated"
    );

    // Row by row, not just in aggregate: an apportioned split could match the
    // total while getting every individual row wrong.
    // `ledger_query` reads column 0 as TEXT, so a bare COUNT(*) comes back
    // empty and would pass every comparison it is given.
    let mismatched = ledger_query(
        &dir,
        "SELECT printf('%d', COUNT(*)) FROM cost_ledger l
         JOIN batch_requests b USING (scope_id, adapter_kind, input_hash, tool_profile_hash)
         WHERE l.adapter_kind = 'embedding'
           AND abs(l.usd - b.estimated_usd * 4.0) > 1e-12",
    );
    assert_eq!(
        mismatched, "0",
        "every row must settle at 4x its OWN reservation"
    );
}

/// A second submission pass over the SAME member set lands on the same task key
/// and must not create a second provider job — "1 job = 1 task" holds across
/// re-runs, which is what keeps the §5.8 recovery walk unambiguous.
#[test]
fn resubmitting_the_same_member_set_reuses_the_row_and_creates_no_second_job() {
    let dir = scope();
    let capture = root(&dir).join("capture.jsonl");
    let script = serde_json::json!({
        "state_sequence": ["BATCH_STATE_PENDING"],
        "job_name": "batches/contract-6",
        "capture_path": capture.to_string_lossy(),
    })
    .to_string();
    json(&dir, &script, &["index", "--online"]);
    json_any(&dir, &script, &["index", "--online"]);

    let creates = std::fs::read_to_string(&capture)
        .unwrap()
        .lines()
        .filter(|line| line.contains("create_embedding_job"))
        .count();
    assert_eq!(creates, 1, "the in-flight job must not be duplicated");
    assert_eq!(
        ledger_query(
            &dir,
            "SELECT CAST(COUNT(*) AS TEXT) FROM batch_requests WHERE adapter_kind = 'embedding'"
        ),
        "1"
    );
}

/// R24 (4 系統一致): 07 §5.3 (1) requires the returned ids to be a BIJECTION
/// onto the submitted ids. A provider that answers 1 of 2 requests must NOT
/// leave the row Completed: the missing chunk would have no vector, no failed
/// task, and — because terminal clears the `intent_token` — no way back. That
/// is a silent, permanent hole in the index.
///
/// Design A makes this checkable with no schema change: the task key IS the
/// digest of the member set, so the collect side re-derives the digest from
/// what actually came back and compares it against the row's `input_hash`.
#[test]
fn a_short_result_set_does_not_settle_the_row_as_succeeded() {
    let dir = fixture_dir();
    prepare_private_home(&dir);
    // Two documents with distinct content → two distinct embedding groups in
    // one job, so the provider can answer one and drop the other.
    std::fs::write(
        root(&dir).join("auth.md"),
        "# 認証仕様\n\nトークン TTL は 3600 秒です。\n",
    )
    .unwrap();
    std::fs::write(
        root(&dir).join("billing.md"),
        "# 請求仕様\n\n締め日は毎月 15 日です。\n",
    )
    .unwrap();
    kio(&dir).arg("init").assert().success();
    kio(&dir).args(["ledger", "init"]).assert().success();
    approve_online_profiles(&dir);

    let capture = root(&dir).join("capture.jsonl");
    let submit = serde_json::json!({
        "state_sequence": ["BATCH_STATE_PENDING"],
        "job_name": "batches/short-1",
        "capture_path": capture.to_string_lossy(),
    })
    .to_string();
    json(&dir, &submit, &["index", "--online"]);

    let keys = submitted_keys(&capture);
    assert_eq!(keys.len(), 2, "fixture must submit two members: {keys:?}");

    // The provider answers only the FIRST key.
    let component = 1.0f64 / (768.0f64).sqrt();
    let short = serde_json::json!({
        "state_sequence": ["BATCH_STATE_SUCCEEDED"],
        "job_name": "batches/short-1",
        "capture_path": capture.to_string_lossy(),
        "inlined_responses": [{
            "metadata": { "key": keys[0] },
            "output": { "response": { "embedding": { "values": vec![component; 768] } } },
        }],
    })
    .to_string();
    use kio_pipeline::ledger::model::{BatchState, Outcome};

    // The malformed result is settled, then the durable breaker holds any
    // replacement submission. Repeating resume must retain that hold.
    for _ in 0..2 {
        let assert = kio(&dir)
            .env(
                "KIO_TEST_GEMINI_BATCH",
                script_with_attribution(&dir, &short),
            )
            .args(["--json", "batch", "resume"])
            .assert()
            .code(4);
        assert!(assert.get_output().stdout.is_empty());
        let error: Value = serde_json::from_slice(&assert.get_output().stderr).unwrap();
        assert_eq!(error["error_code"], "KIO-E-EMBED-BATCH-CONTRACT-HOLD-001");
        let blocked = error["context"]["blocked_attempts"].as_array().unwrap();
        assert_eq!(blocked.len(), 1, "{error}");

        let ledger =
            rusqlite::Connection::open(dir.path().join("home/data/kio/cost-ledger.sqlite"))
                .unwrap();
        let (selector, sequence): (String, i64) = ledger.query_row(
                "SELECT b.state, b.contract_violation_count, b.intent_token IS NULL,
                        b.estimated_usd, l.outcome, l.estimated, l.usd,
                        b.scope_id || '/' || b.adapter_kind || '/' || b.input_hash || '/' || b.tool_profile_hash,
                        b.submission_seq
                 FROM batch_requests b JOIN cost_ledger l
                   USING (scope_id, adapter_kind, input_hash, tool_profile_hash, submission_seq)
                 WHERE b.adapter_kind = 'embedding'",
                [],
                |row| {
                    let state: i64 = row.get(0)?;
                    let violations: i64 = row.get(1)?;
                    let cleared: bool = row.get(2)?;
                    let estimate: f64 = row.get(3)?;
                    let outcome: String = row.get(4)?;
                    let estimated: bool = row.get(5)?;
                    let usd: f64 = row.get(6)?;
                    assert_eq!(BatchState::from_i64(state), Some(BatchState::Terminal));
                    assert_eq!(Outcome::parse(&outcome), Some(Outcome::ContractViolation));
                    assert_eq!(violations, 1);
                    assert!(cleared, "terminal inline job must clear its intent");
                    assert!(estimated && estimate > 0.0);
                    assert_eq!(usd, estimate, "malformed results settle at the reservation");
                    Ok((row.get(7)?, row.get(8)?))
                },
            ).unwrap();
        assert_eq!(blocked[0]["selector"], selector);
        assert_eq!(blocked[0]["submission_seq"], sequence);
        let counts: (i64, i64) = ledger
            .query_row(
                "SELECT (SELECT COUNT(*) FROM batch_requests WHERE adapter_kind = 'embedding'),
                    (SELECT COUNT(*) FROM cost_ledger WHERE adapter_kind = 'embedding')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            counts,
            (1, 1),
            "resume must not reserve or settle another job"
        );

        let index = rusqlite::Connection::open(root(&dir).join(".kio/index/sqlite.db")).unwrap();
        let published: i64 = index
            .query_row("SELECT COUNT(*) FROM embeddings", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            published, 0,
            "even the valid first member must not be published"
        );
    }
    let creates = std::fs::read_to_string(&capture)
        .unwrap()
        .lines()
        .filter(|line| line.contains("create_embedding_job"))
        .count();
    assert_eq!(
        creates, 1,
        "contract hold must prevent a second provider job"
    );
}

/// G1 (2026-07-25): a row stranded in §5.8's job-creation window is recoverable.
///
/// `create_embedding_job` failing AFTER 相 2b was recorded leaves state 0 with a
/// live reservation and no `batch_job_id`. `poll_batch_embedding_jobs` skips
/// such a row by design (it has no job id), so recovery is `ledger reconcile`'s
/// job — and that walk could not see it, because `configured_inventories` only
/// enumerated the Mistral client. The reservation held the device budget cap
/// forever; the only escape was a manual `kio batch abandon`.
#[test]
fn reconcile_recovers_a_row_stranded_in_the_job_creation_window() {
    let dir = scope();
    run_ignoring_output(
        &dir,
        r#"{"fail_phase":"create_job","job_name":"batches/never","state_sequence":["BATCH_STATE_PENDING"]}"#,
        &["index", "--online"],
    );
    // Stranded exactly as described: reserved, 相 2b started, no job id.
    assert_eq!(
        ledger_query(
            &dir,
            "SELECT state || '|' || (batch_job_id IS NULL) || '|' || (intent_token IS NOT NULL)
             FROM batch_requests WHERE adapter_kind = 'embedding'"
        ),
        "0|1|1"
    );
    let token = ledger_query(
        &dir,
        "SELECT intent_token FROM batch_requests WHERE adapter_kind = 'embedding'",
    );
    assert!(!token.is_empty());

    // The provider DID create the job — it is discoverable by the intent_token
    // Kio embedded in its displayName.
    let listing = serde_json::json!({
        "job_name": "batches/recovered",
        "state_sequence": ["BATCH_STATE_PENDING"],
        "jobs_listing": [{
            "name": "batches/recovered",
            "state": "BATCH_STATE_PENDING",
            "displayName": format!("kio-{token}"),
        }],
    })
    .to_string();
    let report = json(&dir, &listing, &["ledger", "reconcile"]);
    assert_eq!(report["batch_found"], 1, "{report}");
    assert_eq!(report["unlistable"], 0, "{report}");

    // The row now carries the job id, so the poll lane can collect it.
    assert_eq!(
        ledger_query(
            &dir,
            "SELECT COALESCE(batch_job_id, 'NONE') FROM batch_requests WHERE adapter_kind = 'embedding'"
        ),
        "batches/recovered"
    );
}

/// G1: a provider job that is NOT Kio's must never be claimed. A Gemini job
/// carries no task-key metadata — only the `displayName` token — so a job whose
/// display name does not parse is reported, not attributed (the same
/// report-only posture 10 §7.5.2 gives an upload).
#[test]
fn reconcile_does_not_claim_a_foreign_provider_job() {
    let dir = scope();
    run_ignoring_output(
        &dir,
        r#"{"fail_phase":"create_job","job_name":"batches/never","state_sequence":["BATCH_STATE_PENDING"]}"#,
        &["index", "--online"],
    );
    let listing = r#"{
        "job_name": "batches/other",
        "state_sequence": ["BATCH_STATE_PENDING"],
        "jobs_listing": [
            {"name": "batches/someone-else", "state": "BATCH_STATE_PENDING",
             "displayName": "not-a-kio-job"}
        ]
    }"#;
    let report = json(&dir, listing, &["ledger", "reconcile"]);
    assert_eq!(report["batch_found"], 0, "{report}");
    // Still stranded — correctly, since nothing matched.
    assert_eq!(
        ledger_query(
            &dir,
            "SELECT COALESCE(batch_job_id, 'NONE') FROM batch_requests WHERE adapter_kind = 'embedding'"
        ),
        "NONE"
    );
    // And the foreign job is surfaced rather than silently ignored.
    assert!(
        !report["unknown"].as_array().unwrap().is_empty(),
        "a job that cannot be attributed must be reported: {report}"
    );
}

/// G2 (2026-07-25): a provider failure at submit fails that JOB, not the
/// invocation.
///
/// These calls used to `?` straight out of the function, so one transient 429
/// aborted `kio index` for the whole scope with `KIO-E-CONFIG-SCHEMA-001` — a
/// permanent-looking config error for a transient network condition. The
/// synchronous lane has always classified and recorded per chunk instead.
#[test]
fn a_submit_failure_does_not_abort_the_invocation() {
    let dir = scope();
    let output = kio(&dir)
        .env(
            "KIO_TEST_GEMINI_BATCH",
            script_with_attribution(&dir, r#"{"fail_phase":"create_job","job_name":"batches/nope","state_sequence":["BATCH_STATE_PENDING"]}"#),
        )
        .arg("--json")
        .args(["index", "--online"])
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "index must still report on stdout; stderr was: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert_eq!(report["status"], "indexed", "{report}");
    // The chunk is accounted for as failed, not silently dropped.
    assert!(
        report["embedding_tasks_failed"].as_u64().unwrap_or(0) > 0,
        "the affected chunks must be recorded as failed: {report}"
    );
}

/// G2 flagship: one unreachable row must not block the others.
///
/// The poll loop used to `?` on `get_job`, so a single row the provider could
/// not answer for aborted collection for every remaining row in the scope —
/// head-of-line blocking on a pass that is supposed to be per-row idempotent.
#[test]
fn an_unreachable_row_does_not_block_collection_of_the_others() {
    let dir = scope();
    let capture = root(&dir).join("capture.jsonl");

    // Row A.
    json(
        &dir,
        &serde_json::json!({
            "state_sequence": ["BATCH_STATE_PENDING"],
            "job_name": "batches/blocked",
            "capture_path": capture.to_string_lossy(),
        })
        .to_string(),
        &["index", "--online"],
    );
    // A second document makes a different member set, hence a second row.
    std::fs::write(
        root(&dir).join("billing.md"),
        "# 請求仕様\n\n締め日は毎月 15 日です。\n",
    )
    .unwrap();
    json_any(
        &dir,
        &serde_json::json!({
            "state_sequence": ["BATCH_STATE_PENDING"],
            "job_name": "batches/ok",
            "capture_path": capture.to_string_lossy(),
        })
        .to_string(),
        &["index", "--online"],
    );
    assert_eq!(
        ledger_query(
            &dir,
            "SELECT CAST(COUNT(*) AS TEXT) FROM batch_requests WHERE adapter_kind = 'embedding'"
        ),
        "2",
        "fixture must produce two batch rows"
    );

    // `batches/blocked` is unreachable; `batches/ok` answers normally.
    let keys = all_submitted_keys(&capture);
    let component = 1.0f64 / (768.0f64).sqrt();
    let script = serde_json::json!({
        "state_sequence": ["BATCH_STATE_SUCCEEDED"],
        "job_name": "batches/ok",
        "fail_job_names": ["batches/blocked"],
        "inlined_responses": keys.iter().map(|key| serde_json::json!({
            "metadata": { "key": key },
            "output": { "response": { "embedding": { "values": vec![component; 768] } } },
        })).collect::<Vec<_>>(),
    })
    .to_string();
    let resumed = json_any(&dir, &script, &["batch", "resume"]);

    // A hold must SAY it is a hold. `tasks_inflight` alone reads exactly like a
    // healthy queued job, and twice that silence let a permanent fault (a result
    // parser that could not read the live shape, then a size bound that rejected
    // any job past ~20 members) look like "still waiting" for as long as anyone
    // cared to poll.
    assert_eq!(
        resumed["tasks_inflight_unreadable"], 1,
        "an unreadable row must be distinguishable from one that is merely queued"
    );

    // The reachable row completed...
    assert_eq!(
        ledger_query(
            &dir,
            "SELECT CAST(state AS TEXT) FROM batch_requests
             WHERE adapter_kind = 'embedding' AND batch_job_id = 'batches/ok'"
        ),
        "2",
        "the reachable row must still be collected"
    );
    // ...and the unreachable one is held untouched for the next pass (§5.8
    // unknown), not settled and not charged.
    assert_eq!(
        ledger_query(
            &dir,
            "SELECT state || '|' || (intent_token IS NOT NULL) FROM batch_requests
             WHERE adapter_kind = 'embedding' AND batch_job_id = 'batches/blocked'"
        ),
        "1|1",
        "the unreachable row must be held, not settled"
    );
}
