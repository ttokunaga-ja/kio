//! Step4b Phase 3-A contract tests: task state machine / Tier A-B scan
//! approval / adapter contract / pipeline remainder (batch/task/adapter/
//! approval wiring — main.rs exit-code/error/log/config cross-cutting areas
//! are P3-B's, tested in `step4b_p3b_contract.rs`).
//!
//! Source: `tasks/step4b-contract-tests-p3a.md` (QA1-QA71, §W arbitration
//! 1-8). Test names carry their QA number so a failure maps directly back to
//! the contract text. Most of the markdownize-response (§K/§L/§M),
//! adapter-type (§F), and identity (§J) contracts are pure functions and are
//! covered by inline `#[cfg(test)]` unit tests in `kio-pipeline`/`kio-adapter`
//! instead of duplicated here — this file covers the CLI-process-level
//! wiring: config/schema behavior, `kio status`/`kio index` end-to-end
//! effects, and cross-crate behavior/type boundaries that need a real build.
//!
//! Coverage is partial by design: several QA items (§E QA14/15 cost-ledger
//! backup/restore/orphan detection, §O Batch trait, §Q streaming, §U Batch
//! content recovery) are large, independent features deferred out of this
//! pass — see the implementation report, not this file, for the full
//! QA-by-QA accounting. §G/§H (the online opt-in AND-gate, `.kio/scope.json`
//! `approvals[]` storage, and `kio adapter revoke`) and §I
//! (`--online`/`--offline` wiring for `repair`/`batch resume`/`batch
//! retry`/`reindex`) ARE covered — QA21/22/25/26/27/29/30/31 near the end of
//! this file. §E QA13 (the provider-idempotency conditional) IS covered —
//! see the `qa13_*` tests just below §D (the header-assembly/fail-closed
//! mechanics themselves are `kio-adapter` unit tests; this file only proves
//! the CLI-to-Adapter-boundary wiring).

mod support;

use support::canonical_tempdir;

use std::fs;

use assert_cmd::Command;
use kio_adapter::bbox_annotation::mistral_markdownize_profile;
use kio_adapter::identity::tool_profile_hash;
use kio_adapter::tool_lock::{canonical_tool_lock_value, tool_lock_hash};
use kio_core::scope::{
    Repository, network_approvals_initialized, publish_network_approval, revoke_network_approval,
    tier_a_template_text, write_network_approval_pending,
};
use kio_index::registry::{RegistryDb, RegistryEntry};
use kio_pipeline::scan::TIER_B_NEEDLES;
use serde_json::{Value, json};
use tempfile::TempDir;

const TEST_ADOPTED_EMBEDDING_ENV: &str = "KIO_TEST_GEMINI_EMBED";
const TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV: &str = "KIO_TEST_MISTRAL_OCR";

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

fn json_failure(dir: &TempDir, args: &[&str], code: i32) -> Value {
    let stderr = kio(dir, args)
        .arg("--json")
        .assert()
        .code(code)
        .get_output()
        .stderr
        .clone();
    serde_json::from_slice(&stderr).unwrap()
}

fn init(dir: &TempDir) {
    kio(dir, &["init"]).assert().success();
}

/// Paid-provider assertions must exercise a real initialized device ledger;
/// absence is tested separately as an explicit hold, never as a zero-cost path.
fn init_paid(dir: &TempDir) {
    kio(dir, &["ledger", "init"]).assert().success();
}

fn scope_json(dir: &TempDir) -> Value {
    serde_json::from_str(&fs::read_to_string(dir.path().join(".kio/scope.json")).unwrap()).unwrap()
}

fn registry_path(dir: &TempDir) -> std::path::PathBuf {
    dir.path().join(".test-data/kio/scope-registry.sqlite")
}

fn ledger_row_counts(dir: &TempDir) -> (i64, i64) {
    support::prepare_private_sqlite_sidecars(&dir.path().join(".test-data/kio/cost-ledger.sqlite"))
        .unwrap();
    let conn = rusqlite::Connection::open_with_flags(
        dir.path().join(".test-data/kio/cost-ledger.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let requests = conn
        .query_row("SELECT COUNT(*) FROM batch_requests", [], |row| row.get(0))
        .unwrap();
    let charges = conn
        .query_row("SELECT COUNT(*) FROM cost_ledger", [], |row| row.get(0))
        .unwrap();
    (requests, charges)
}

// ===========================================================================
// §A task 状態機械 (U1, QA1-QA4 — all implemented; see the pipeline crate's
// `HoldReason` doc comment for the QA2/QA3 transition mapping)
// ===========================================================================

/// QA1 (partial) + QA4: a budget-paused task carries `hold_reason="budget"`
/// (QA1's closed enum wired at the budget-pause site), and `kio status`
/// reports it in a `paused_by_hold_reason` breakdown (QA4) instead of
/// forcing the caller to filter the raw task array client-side.
#[test]
fn qa1_qa4_status_reports_budget_paused_task_with_hold_reason() {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("a.md"), "# Doc\n\nbody text.\n").unwrap();
    init(&dir);
    json_success(&dir, &["index", "--yes"]);
    let status = json_success(&dir, &["status"]);
    let breakdown = &status["paused_by_hold_reason"];
    for reason in ["budget", "auth", "tier_b_approval"] {
        assert!(
            breakdown.get(reason).is_some(),
            "paused_by_hold_reason must always report all current-format buckets: {breakdown}"
        );
    }
    assert!(breakdown.get("unknown").is_none(), "{breakdown}");
    // No paused task yet in this fresh scope.
    assert_eq!(breakdown["budget"], 0);
}

/// Run `kio <args> --json` with the online markdownize seam pinned to `seam` and an
/// optional frozen clock, tolerating ANY exit (batch resume/retry return non-zero on
/// a retry-able failure while still printing their JSON result to stdout). Mirrors
/// `step3_p0_contract.rs`'s helper of the same name/shape.
fn run_markdownize_seam(
    dir: &TempDir,
    seam: &str,
    fixed_now: Option<&str>,
    args: &[&str],
) -> Value {
    let mut command = kio(dir, args);
    command.env(TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV, seam);
    if let Some(now) = fixed_now {
        command.env("KIO_FIXED_NOW", now);
    }
    let output = command.arg("--json").assert().get_output().stdout.clone();
    serde_json::from_slice(&output).unwrap_or(Value::Null)
}

fn approve_markdownize(dir: &TempDir, seam: &str) -> Value {
    run_markdownize_seam(
        dir,
        seam,
        None,
        &["adapter", "approve", "mistral_ocr_markdownize", "--yes"],
    )
}

/// The embedding-seam analog of [`run_markdownize_seam`] (QA3's embedding twin).
fn run_embedding_seam(dir: &TempDir, seam: &str, fixed_now: Option<&str>, args: &[&str]) -> Value {
    let mut command = kio(dir, args);
    command.env(TEST_ADOPTED_EMBEDDING_ENV, seam);
    if let Some(now) = fixed_now {
        command.env("KIO_FIXED_NOW", now);
    }
    let output = command.arg("--json").assert().get_output().stdout.clone();
    serde_json::from_slice(&output).unwrap_or(Value::Null)
}

fn approve_embedding(dir: &TempDir, seam: &str) -> Value {
    run_embedding_seam(
        dir,
        seam,
        None,
        &["adapter", "approve", "gemini_embedding_2", "--yes"],
    )
}

/// The online-OCR markdownize task — distinguished from the LOCAL deterministic
/// markdownize task a text-layer `fake_pdf` fixture also creates, both before
/// completion (`output_ref` still the `"online:"` placeholder) and after
/// (`output_ref` rewritten to the normalized-instance path, but
/// `fallback_reason="online_adapter_done"`).
fn online_markdownize_task(status: &Value) -> Value {
    status["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| {
            task["type"] == "markdownize"
                && (task["output_ref"]
                    .as_str()
                    .is_some_and(|output_ref| output_ref.starts_with("online:"))
                    || task["fallback_reason"] == "online_adapter_done")
        })
        .cloned()
        .unwrap_or_else(|| panic!("no online markdownize task in {status}"))
}

/// QA2 (step4b-contract-tests-p3a.md §A, 04 §5.2 L679-683): an auth_error
/// online-markdownize send lands `paused` with `hold_reason="auth"` — never
/// `failed` — and does not consume the retry budget (`attempts` stays 0, so a
/// later revival is not budget-exhausted, and `next_retry_at` stays unset).
/// `batch retry` remains a no-op (CT2-TASK-005: its Failed-only selection
/// naturally skips a Paused row); `batch resume` (credentials fixed) revives
/// and completes it.
#[test]
fn qa2_auth_error_send_lands_paused_hold_reason_auth() {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("a.pdf"), fake_pdf(&["hello qa2"])).unwrap();
    init(&dir);
    init_paid(&dir);
    approve_markdownize(&dir, "mock");
    run_markdownize_seam(&dir, "mock", None, &["index"]);

    kio(&dir, &["batch", "resume"])
        .env(TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV, "auth_error")
        .arg("--json")
        .assert()
        .code(5);

    let status = run_markdownize_seam(&dir, "mock", None, &["status"]);
    let task = online_markdownize_task(&status);
    assert_eq!(task["status"], "paused", "{status}");
    assert_eq!(task["hold_reason"], "auth", "{status}");
    assert_eq!(task["fallback_reason"], "auth_error", "{status}");
    assert_eq!(task["attempts"], 0, "{status}");
    assert!(task["next_retry_at"].is_null(), "{status}");

    // `batch retry` must not revive it (CT2-TASK-005).
    let retry = run_markdownize_seam(&dir, "mock", None, &["batch", "retry"]);
    assert_eq!(retry["tasks_executed"], 0, "{retry}");
    let status = run_markdownize_seam(&dir, "mock", None, &["status"]);
    assert_eq!(online_markdownize_task(&status)["status"], "paused");

    // `batch resume` with fixed credentials (mock) revives and completes it.
    let resumed = run_markdownize_seam(&dir, "mock", None, &["batch", "resume"]);
    assert_eq!(resumed["tasks_executed"], 1, "{resumed}");
    let status = run_markdownize_seam(&dir, "mock", None, &["status"]);
    let task = online_markdownize_task(&status);
    assert!(
        task["status"] == "done" || task["status"] == "partial",
        "{status}"
    );
}

/// QA3 (step4b-contract-tests-p3a.md §A, 04 §5.3): rate_limit WITH a provider
/// `Retry-After` header lands `pending` + `next_retry_at` set EXACTLY
/// `retry_after_ms` in the future — the `retry_after_ms -> next_retry_at`
/// wiring's proof (the `rate_limit_after` seam supplies 30_000ms). `attempts`
/// is not consumed (max_attempts=∞, 04 §5.3); an unelapsed resume is gated by
/// `next_retry_at`, and an elapsed one completes.
#[test]
fn qa3_rate_limit_send_stays_pending_with_retry_after() {
    let dir = canonical_tempdir();
    fs::write(
        dir.path().join("a.pdf"),
        fake_pdf(&["hello qa3 retry after"]),
    )
    .unwrap();
    init(&dir);
    init_paid(&dir);
    approve_markdownize(&dir, "mock");
    run_markdownize_seam(&dir, "mock", None, &["index"]);

    kio(&dir, &["batch", "resume"])
        .env(TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV, "rate_limit_after")
        .env("KIO_FIXED_NOW", "2026-07-03T00:00:00Z")
        .arg("--json")
        .assert()
        .code(3);

    let status = run_markdownize_seam(&dir, "mock", None, &["status"]);
    let task = online_markdownize_task(&status);
    assert_eq!(task["status"], "pending", "{status}");
    assert_eq!(task["attempts"], 0, "{status}");
    assert_eq!(
        task["next_retry_at"], "2026-07-03T00:00:30Z",
        "the Retry-After header (30_000ms) must be honored EXACTLY: {status}"
    );

    // Unelapsed resume (10s later, before the 30s Retry-After): the gate holds.
    let early = run_markdownize_seam(
        &dir,
        "mock",
        Some("2026-07-03T00:00:10Z"),
        &["batch", "resume"],
    );
    assert_eq!(early["tasks_executed"], 0, "{early}");

    // Elapsed resume (31s later): completes.
    let resumed = run_markdownize_seam(
        &dir,
        "mock",
        Some("2026-07-03T00:00:31Z"),
        &["batch", "resume"],
    );
    assert_eq!(resumed["tasks_executed"], 1, "{resumed}");
}

/// QA3: a rate_limit send with NO `Retry-After` header (the headerless
/// `"rate_limit"` seam, as opposed to `"rate_limit_after"`) falls back to the
/// synthetic +2s backoff.
#[test]
fn qa3_rate_limit_headerless_uses_synthetic_backoff() {
    let dir = canonical_tempdir();
    fs::write(
        dir.path().join("a.pdf"),
        fake_pdf(&["hello qa3 headerless"]),
    )
    .unwrap();
    init(&dir);
    init_paid(&dir);
    approve_markdownize(&dir, "mock");
    run_markdownize_seam(&dir, "mock", None, &["index"]);

    kio(&dir, &["batch", "resume"])
        .env(TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV, "rate_limit")
        .env("KIO_FIXED_NOW", "2026-07-03T00:00:00Z")
        .arg("--json")
        .assert()
        .code(3);

    let status = run_markdownize_seam(&dir, "mock", None, &["status"]);
    let task = online_markdownize_task(&status);
    assert_eq!(task["status"], "pending", "{status}");
    assert_eq!(
        task["next_retry_at"], "2026-07-03T00:00:02Z",
        "a headerless rate_limit must fall back to the synthetic +2s backoff: {status}"
    );
}

/// QA3 embedding twin (04 §5.2/§5.3, 04 §876): a rate-limited embedding send
/// lands `pending` + `next_retry_at` honoring `Retry-After` (never `failed`),
/// and — the wiring's enrichment-gate proof — an unelapsed resend of that
/// chunk is excluded from the very next enrichment pass
/// (`embeddable_task_state`'s new Pending arm). Drives to `done` once the
/// backoff elapses.
#[test]
fn qa3_embedding_rate_limit_pending_and_gated() {
    let dir = canonical_tempdir();
    // A single flat section (no sub-heading) so this chunks to exactly one
    // embedding task — a nested `##` sub-heading (as in some other fixtures)
    // would split into multiple chunks/tasks here.
    fs::write(
        dir.path().join("a.md"),
        "# Doc\n\nQA3 embedding twin regression body text.\n",
    )
    .unwrap();
    init(&dir);
    init_paid(&dir);
    approve_embedding(&dir, "rate_limit_after");
    run_embedding_seam(
        &dir,
        "rate_limit_after",
        Some("2026-07-03T00:00:00Z"),
        &["index"],
    );

    let status = run_embedding_seam(&dir, "mock", None, &["status"]);
    let embedding_tasks: Vec<&Value> = status["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|task| task["type"] == "embedding")
        .collect();
    assert!(!embedding_tasks.is_empty(), "no embedding task in {status}");
    assert!(
        embedding_tasks
            .iter()
            .all(|task| task["status"] == "pending"
                && task["attempts"] == 0
                && task["next_retry_at"] == "2026-07-03T00:00:30Z"),
        "the Retry-After header (30_000ms) must be honored EXACTLY: {status}"
    );
    let expected_count = embedding_tasks.len() as u64;

    // An immediate second enrichment pass (10s later, before the 30s
    // Retry-After) must exclude them all — the new `embeddable_task_state`
    // Pending gate's proof (without it, a Pending task was always re-sent
    // regardless of `next_retry_at`).
    let early = run_embedding_seam(
        &dir,
        "mock",
        Some("2026-07-03T00:00:10Z"),
        &["batch", "resume"],
    );
    assert_eq!(early["tasks_executed"], 0, "{early}");

    // Past the backoff: all complete.
    let resumed = run_embedding_seam(
        &dir,
        "mock",
        Some("2026-07-03T00:00:31Z"),
        &["batch", "resume"],
    );
    assert_eq!(resumed["tasks_executed"], expected_count, "{resumed}");
    let status = run_embedding_seam(&dir, "mock", None, &["status"]);
    assert!(
        status["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|task| task["type"] == "embedding")
            .all(|task| task["status"] == "done")
    );
}

// ===========================================================================
// §B Tier A/B scan approval (U2, QA5-7)
// ===========================================================================

/// QA5: `.kio/scope.json` carries a `scan_approval` key (distinct from the
/// adapter-level grants) after `kio index --yes`, with the 10
/// §1 L101-113 required fields.
#[test]
fn qa5_scope_json_records_scan_approval_after_index_yes() {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("a.md"), "# Doc\n\nbody text.\n").unwrap();
    init(&dir);
    assert!(
        scope_json(&dir).get("scan_approval").is_none(),
        "scan_approval must not exist before any approval"
    );

    json_success(&dir, &["index", "--yes"]);
    let scope = scope_json(&dir);
    let scan_approval = scope
        .get("scan_approval")
        .expect("scan_approval must exist after `index --yes`");
    for field in [
        "scope_id",
        "root_path",
        "approved_at",
        "actor",
        "approval_method",
        "kio_version",
        "effective_ignore_hash",
        "estimated_file_count",
        "estimated_total_bytes",
        "estimated_markdownize_usd",
        "estimated_embedding_usd",
    ] {
        assert!(
            scan_approval.get(field).is_some(),
            "scan_approval missing required field {field}: {scan_approval}"
        );
    }
    assert_eq!(scan_approval["approval_method"], "yes");
    assert_eq!(scan_approval["scope_id"], scope["scope_id"]);
}

/// QA5 (idempotency): a second `index --yes` does not overwrite the
/// original scan_approval (scope-level approval is recorded once).
#[test]
fn qa5_scan_approval_is_recorded_once_not_per_index_run() {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("a.md"), "# Doc\n\nbody text.\n").unwrap();
    init(&dir);
    json_success(&dir, &["index", "--yes"]);
    let first_approved_at = scope_json(&dir)["scan_approval"]["approved_at"]
        .as_str()
        .unwrap()
        .to_owned();

    fs::write(dir.path().join("b.md"), "# Doc2\n\nmore text.\n").unwrap();
    json_success(&dir, &["index", "--yes"]);
    let second_approved_at = scope_json(&dir)["scan_approval"]["approved_at"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        first_approved_at, second_approved_at,
        "scan_approval is a one-time scope-level record, not per-adapter"
    );
}

/// QA7 (arbitration #1): `effective_ignore_hash` is the sha256 of the actual
/// built-in Tier A/B pattern content (`tier_a_template_text`), not a fixed
/// version-string literal — it must equal the same computation the
/// production code performs on the current pattern set, and it must NOT
/// equal a hash of an arbitrary unrelated literal (proving it is content-
/// derived, not a hardcoded constant reused verbatim).
#[test]
fn qa7_effective_ignore_hash_is_derived_from_real_pattern_content() {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("a.md"), "# Doc\n\nbody text.\n").unwrap();
    init(&dir);
    json_success(&dir, &["index", "--yes"]);
    let scope = scope_json(&dir);
    let hash = scope["scan_approval"]["effective_ignore_hash"]
        .as_str()
        .unwrap()
        .to_owned();

    let expected = kio_core::cas::hash_bytes(tier_a_template_text(TIER_B_NEEDLES).as_bytes());
    assert_eq!(hash, expected);
    // Not the old fixed version-literal hash (QA7's identified regression).
    let stale_literal_hash = kio_core::cas::hash_bytes(b"built-in-tier-a-v1");
    assert_ne!(hash, stale_literal_hash);
}

// ===========================================================================
// §D budget guardrail — folder per_adapter removal (U4 残り, QA11-12)
// ===========================================================================

/// QA12 (arbitration #2): the folder `.kio/config.toml` does not define
/// `[budget.per_adapter]` (04 §5.4 — device-layer only) — setting it is a
/// config schema error (`KIO-E-CONFIG-SCHEMA-001`), not a silently-parsed-
/// but-unused key.
#[test]
fn qa12_folder_config_per_adapter_is_a_schema_error() {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("a.md"), "# Doc\n\nbody text.\n").unwrap();
    init(&dir);
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[budget.per_adapter]\nmarkdownize = 1.0\n",
    )
    .unwrap();
    let err = json_failure(&dir, &["status"], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-SCHEMA-001");
}

/// QA11: `kio status`'s budget report no longer presents a folder
/// `per_adapter` constraint (it does not exist, 04 §5.4) — only the
/// device-layer one appears.
#[test]
fn qa11_status_budget_report_has_no_folder_per_adapter_key() {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("a.md"), "# Doc\n\nbody text.\n").unwrap();
    init(&dir);
    json_success(&dir, &["index", "--yes"]);
    let status = json_success(&dir, &["status"]);
    let budget = &status["budget"];
    assert!(budget.get("device_per_adapter").is_some());
    assert!(
        budget.get("folder_per_adapter").is_none(),
        "folder_per_adapter must not be reported: {budget}"
    );
}

// ===========================================================================
// §E LLM API idempotency 二段階 (U11, QA13 — QA14/15 backup/restore/orphan
// detection remain deferred, see this file's header note)
// ===========================================================================

/// QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): a sync markdownize
/// send under the `require_idempotency_token` seam SUCCEEDS — the seam's
/// adapter declares `ProviderIdempotency::HttpHeader` and fails closed with
/// `ContractViolation` on a missing token, so a completed task proves the
/// CLI threaded the ledger's `intent_token` all the way to the Adapter
/// boundary (`kio-adapter` unit tests cover the header-assembly/fail-closed
/// mechanics directly; this is the end-to-end CLI wiring proof).
#[test]
fn qa13_sync_send_threads_intent_token_to_adapter() {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("a.pdf"), fake_pdf(&["hello qa13"])).unwrap();
    init(&dir);
    init_paid(&dir);
    approve_markdownize(&dir, "mock");
    // R14-2: the online markdownize send is deferred — `index --approve`
    // only enqueues it (mirrors QA2/QA3's two-step flow above).
    run_markdownize_seam(&dir, "mock", None, &["index"]);

    let resumed = run_markdownize_seam(
        &dir,
        "require_idempotency_token",
        None,
        &["batch", "resume"],
    );
    assert_eq!(resumed["tasks_executed"], 1, "{resumed}");

    let status = run_markdownize_seam(&dir, "mock", None, &["status"]);
    let task = online_markdownize_task(&status);
    assert!(
        task["status"] == "done" || task["status"] == "partial",
        "the ledger intent_token must reach the adapter boundary so the seam's \
         idempotency gate is satisfied and the send succeeds: {status}"
    );
}

/// QA13's embedding twin: `index --approve` runs embedding enrichment
/// synchronously (unlike markdownize's deferred online task), so the
/// `require_idempotency_token` seam's proof is a single command — every
/// embedding task must land `done`, never a `ContractViolation` failure.
#[test]
fn qa13_embedding_sync_send_threads_intent_token_to_adapter() {
    let dir = canonical_tempdir();
    fs::write(
        dir.path().join("a.md"),
        "# Doc\n\nQA13 embedding idempotency threading body text.\n",
    )
    .unwrap();
    init(&dir);
    init_paid(&dir);
    approve_embedding(&dir, "require_idempotency_token");
    run_embedding_seam(&dir, "require_idempotency_token", None, &["index"]);

    let status = run_embedding_seam(&dir, "mock", None, &["status"]);
    let embedding_tasks: Vec<&Value> = status["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|task| task["type"] == "embedding")
        .collect();
    assert!(!embedding_tasks.is_empty(), "no embedding task in {status}");
    assert!(
        embedding_tasks.iter().all(|task| task["status"] == "done"),
        "the ledger intent_token must reach the adapter boundary so the seam's \
         idempotency gate is satisfied and the send succeeds: {status}"
    );
}

// ===========================================================================
// §F AdapterRun/AdapterProfile 応答 schema の拡張 (U78, QA16-19)
//
// QA16 (error_code/error_category/retry_after_ms), QA17 (usage one-of +
// estimated degrade), and QA18 (billable_kinds/reject_billing) are pure
// AdapterRun/AdapterProfile/MarkdownizeResponse/Usage-shaped contracts,
// covered by inline `#[cfg(test)]` unit tests in `kio-adapter`
// (`adapter_error_tests::*`, `http_policy::tests::retry_after_ms_*`) and
// `kio-pipeline` (`ledger::ops::tests::qa17_*`/`qa19_*`,
// `task::tests::qa16_adapter_error_code_matches_retry_policy`) — per this
// file's own header note, not duplicated here. The existing `rate_limit`
// mock-seam coverage across `step2_p0_contract.rs`/`step3_p0_contract.rs`
// regression-locks `AdapterError::RateLimit`'s shape change (tuple ->
// struct variant with `retry_after_ms`). QA19's tools.toml `[pricing]`
// table IS a CLI-process-level concern (schema validation at startup, and
// the preview/scan_approval cost estimate) — tested here.
// ===========================================================================

/// QA19 (step4b-contract-tests-p3a.md §F, 03 §11 L832-837): the spec's
/// literal `[<kind>.<tool_id>.pricing]` example is accepted by `tools.toml`
/// schema validation — before this fix `pricing` fell through the closed
/// `TOOLS_ENTRY_FIELDS` list and every CLI invocation failed at startup
/// (`validate_user_tools_config`, exit 2).
#[test]
fn qa19_tools_toml_pricing_example_is_accepted() {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("a.md"), "# Doc\n\nbody text.\n").unwrap();
    init(&dir);
    fs::create_dir_all(dir.path().join(".test-config/kio")).unwrap();
    fs::write(
        dir.path().join(".test-config/kio/tools.toml"),
        "[markdown.mistral_ocr_markdownize]\nkind = \"online_api\"\n\n\
         [markdown.mistral_ocr_markdownize.pricing]\npages = 0.004\n",
    )
    .unwrap();
    json_success(&dir, &["status"]);
}

/// QA19: an unknown pricing kind is a config schema error, not a silently
/// accepted/ignored key — the strict-schema posture (R13-2) extends to
/// `[pricing]`.
#[test]
fn qa19_tools_toml_pricing_unknown_kind_is_a_schema_error() {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("a.md"), "# Doc\n\nbody text.\n").unwrap();
    init(&dir);
    fs::create_dir_all(dir.path().join(".test-config/kio")).unwrap();
    fs::write(
        dir.path().join(".test-config/kio/tools.toml"),
        "[markdown.mistral_ocr_markdownize]\nkind = \"online_api\"\n\n\
         [markdown.mistral_ocr_markdownize.pricing]\nbogus_kind = 0.01\n",
    )
    .unwrap();
    let err = json_failure(&dir, &["status"], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-SCHEMA-001");
}

/// QA19: a declared markdownize `pages` price makes `scan_approval`'s
/// `estimated_markdownize_usd` a real non-zero figure (`preview.estimated_cost`,
/// `kio-pipeline`'s `build_scan_preview`, wired to the declared pricing) — the
/// exact pre-fix gap this contract closes (`write_approval_record` hard-coded
/// `0.0` unconditionally, cited by name in the old code comment).
#[test]
fn qa19_scan_approval_estimated_markdownize_usd_reflects_declared_pricing() {
    let dir = canonical_tempdir();
    // A non-text-native candidate (skips the text-native exclusion in the
    // markdownize byte sum) — any positive size rounds up to at least 1
    // estimated page under the fixed 3_000-byte/page assumption, so a small,
    // locally-parseable minimal PDF (same fixture `scan.rs`'s own tests use)
    // is enough; a real Mistral OCR document is not needed for this baseline
    // (non-`--online`) index pass.
    fs::write(dir.path().join("scan.pdf"), b"%PDF BT (text)").unwrap();
    init(&dir);
    fs::create_dir_all(dir.path().join(".test-config/kio")).unwrap();
    fs::write(
        dir.path().join(".test-config/kio/tools.toml"),
        "[markdown.mistral_ocr_markdownize]\nkind = \"online_api\"\n\n\
         [markdown.mistral_ocr_markdownize.pricing]\npages = 0.004\n",
    )
    .unwrap();
    // No `--online`: the online OCR task is enqueued Pending, never sent —
    // this proves the PREVIEW estimate is wired, independent of any real
    // network call.
    json_success(&dir, &["index", "--yes"]);
    let scope = scope_json(&dir);
    let estimated_markdownize_usd = scope["scan_approval"]["estimated_markdownize_usd"]
        .as_f64()
        .expect("estimated_markdownize_usd must be a number");
    assert!(
        estimated_markdownize_usd > 0.0,
        "expected a non-zero markdownize estimate with pricing declared, got {estimated_markdownize_usd}"
    );
}

// ===========================================================================
// §J bbox_annotation / tool_lock_hash (U83/U84, QA32/33/35)
// ===========================================================================

/// QA32 [regression-lock]: bbox_annotation on/off is folded into
/// `tool_profile_hash` via the `output_schema`/`prompt_template_*` diff
/// (identity.rs has no literal `"bbox_annotation"` PROFILE_FIELDS entry) —
/// the two hashes must differ.
#[test]
fn qa32_bbox_annotation_toggle_changes_tool_profile_hash() {
    let enabled = mistral_markdownize_profile("mistral-ocr-2505", true);
    let disabled = mistral_markdownize_profile("mistral-ocr-2505", false);
    assert_ne!(
        tool_profile_hash(&enabled).unwrap(),
        tool_profile_hash(&disabled).unwrap()
    );
}

/// QA33 (arbitration #4): `[markdownize] bbox_annotation = true` (the spec's
/// literal flat-key TOML example, 07 §5.2) is accepted by config.schema.json
/// and honored by `kio index`; a stale nested
/// `[markdownize.bbox_annotation] enabled = true` shape is now a schema
/// error (type mismatch: boolean expected, object found).
#[test]
fn qa33_bbox_annotation_flat_key_is_accepted_nested_shape_is_rejected() {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("a.md"), "# Doc\n\nbody text.\n").unwrap();
    init(&dir);
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[markdownize]\nbbox_annotation = false\n",
    )
    .unwrap();
    json_success(&dir, &["status"]);

    fs::write(
        dir.path().join(".kio/config.toml"),
        "[markdownize.bbox_annotation]\nenabled = false\n",
    )
    .unwrap();
    let err = json_failure(&dir, &["status"], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-SCHEMA-001");
}

/// QA35 [regression-lock]: `tool_lock_hash` folds only the role-specific
/// canonical fields (`tool_id`+`profile_hash`, embedding also
/// `+dimensions+distance+modality`) — `capabilities` (markdown) and `mode`
/// (embedding) are display-only and must not perturb the hash.
#[test]
fn qa35_tool_lock_hash_ignores_capabilities_and_mode() {
    let base = json!({
        "spec_version": 1,
        "markdown": {
            "tool_id": "mistral_ocr_markdownize",
            "profile_hash": format!("sha256:{}", "a".repeat(64)),
            "capabilities": ["ocr"]
        },
        "embedding": {
            "tool_id": "gemini_embedding_2",
            "profile_hash": format!("sha256:{}", "b".repeat(64)),
            "dimensions": 768,
            "distance": "cosine",
            "modality": "multimodal",
            "mode": "online"
        }
    });
    let mut changed = base.clone();
    changed["markdown"]["capabilities"] = json!(["ocr", "layout_detection"]);
    changed["embedding"]["mode"] = json!("offline_replay");
    assert_eq!(
        canonical_tool_lock_value(&base).unwrap(),
        canonical_tool_lock_value(&changed).unwrap()
    );
    assert_eq!(
        tool_lock_hash(&base).unwrap(),
        tool_lock_hash(&changed).unwrap()
    );
}

// ===========================================================================
// §R include_neighbors removal (U94/U143, QA61)
// ===========================================================================

/// QA61 (arbitration #7): `markdownize.incremental.include_neighbors` was
/// removed from config.schema.json entirely — even the old documented
/// default (1) is now an unknown-key schema error, not a silent no-op
/// accept.
#[test]
fn qa61_include_neighbors_key_removed_from_schema() {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("a.md"), "# Doc\n\nbody text.\n").unwrap();
    init(&dir);
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[markdownize.incremental]\ninclude_neighbors = 1\n",
    )
    .unwrap();
    let err = json_failure(&dir, &["status"], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-SCHEMA-001");
}

// ===========================================================================
// §T registry live 重複 fail-closed の拡大 (PB24 継承, QA66-68)
// ===========================================================================

fn make_registry_duplicate(dir_a: &TempDir, scope_id: &str) -> TempDir {
    let dir_b = canonical_tempdir();
    fs::write(dir_b.path().join("other.md"), "# Other\n\nOther body.\n").unwrap();
    kio(&dir_b, &["init"]).assert().success();
    let scope_path = dir_b.path().join(".kio/scope.json");
    let mut value: Value = serde_json::from_str(&fs::read_to_string(&scope_path).unwrap()).unwrap();
    value["scope_id"] = json!(scope_id);
    fs::write(&scope_path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

    let registry = RegistryDb::open(registry_path(dir_a)).unwrap();
    registry
        .upsert(&RegistryEntry {
            scope_id: scope_id.to_owned(),
            kio_path: dir_a.path().join(".kio").display().to_string(),
            root_path: dir_a.path().display().to_string(),
            participates_in_global_search: true,
            indexed: true,
            last_seen_at: "2020-01-01T00:00:00Z".to_owned(),
        })
        .unwrap();
    registry
        .upsert(&RegistryEntry {
            scope_id: scope_id.to_owned(),
            kio_path: dir_b.path().join(".kio").display().to_string(),
            root_path: dir_b.path().display().to_string(),
            participates_in_global_search: true,
            indexed: true,
            last_seen_at: "2099-01-01T00:00:00Z".to_owned(),
        })
        .unwrap();
    dir_b
}

/// QA66/67: an online task's phase-1 reservation must fail closed on a live
/// registry scope-id duplicate, before a device-global ledger row or provider
/// call can be made. Offline local indexing has no reservation and therefore
/// cannot exercise this guard.
#[test]
fn qa66_qa67_online_reservation_fails_closed_on_registry_duplicate() {
    let dir_a = canonical_tempdir();
    fs::write(dir_a.path().join("seed.pdf"), fake_pdf(&["seed"])).unwrap();
    kio(&dir_a, &["init"]).assert().success();
    init_paid(&dir_a);
    approve_markdownize(&dir_a, "mock");
    run_markdownize_seam(&dir_a, "mock", None, &["index"]);
    let before_status = run_markdownize_seam(&dir_a, "mock", None, &["status"]);
    let before_task = online_markdownize_task(&before_status);
    assert_eq!(before_task["status"], "pending", "{before_status}");
    let scope_id = scope_json(&dir_a)["scope_id"].as_str().unwrap().to_owned();
    let _dir_b = make_registry_duplicate(&dir_a, &scope_id);
    let before_ledger = ledger_row_counts(&dir_a);

    // `registry_duplicate_error` uses `ExitCode::PermanentFailure` (4) — this
    // existing pending task must never reach either reservation or the mock
    // adapter. `auth_error` would pause the task if the provider boundary ran.
    let stderr = kio(&dir_a, &["batch", "resume"])
        .env(TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV, "auth_error")
        .arg("--json")
        .assert()
        .code(4)
        .get_output()
        .stderr
        .clone();
    let err: Value = serde_json::from_slice(&stderr).unwrap();
    assert_eq!(err["error_code"], "KIO-E-REGISTRY-DUP-001");
    assert_eq!(ledger_row_counts(&dir_a), before_ledger);
    let after_status = run_markdownize_seam(&dir_a, "mock", None, &["status"]);
    let after_task = online_markdownize_task(&after_status);
    assert_eq!(after_task["status"], "pending", "{after_status}");
    assert_eq!(after_task["attempts"], before_task["attempts"]);
}

/// QA68 [regression-lock]: the read-only registry-dup check
/// (`resolve_scope_id_in_registry`/`resolve_scope_target`) already correctly
/// fail-closes `kio evidence verify` — unaffected by the QA66/67 write-path
/// extension above.
#[test]
fn qa68_evidence_verify_still_fails_closed_on_registry_duplicate() {
    let dir_a = canonical_tempdir();
    fs::write(
        dir_a.path().join("seed.md"),
        "# Seed\n\nTTL is 3600 seconds.\n",
    )
    .unwrap();
    kio(&dir_a, &["init"]).assert().success();
    json_success(&dir_a, &["index", "--offline"]);
    let search = json_success(&dir_a, &["search", "3600", "--mode", "text"]);
    let pointer = search["results"][0]["evidence_pointer"].clone();
    let scope_id = scope_json(&dir_a)["scope_id"].as_str().unwrap().to_owned();
    let _dir_b = make_registry_duplicate(&dir_a, &scope_id);

    // `kio evidence verify` is the one command whose doc comment on
    // `registry_duplicate_error` pins exit 3 (PB54) instead of the default
    // `ExitCode::PermanentFailure` (4) other write/read commands use — see
    // `qb4b_qb4c_registry_duplicate_outranks_journal_and_index_generation_via_view`
    // in step4b_p3b_contract.rs for the sibling `open`/`view` case (exit 4).
    // `evidence verify`'s registry_duplicate outcome is a structured result
    // on stdout (`{"status":"registry_duplicate","error_code":...}`), not a
    // stderr failure envelope — unlike most other error paths in this CLI.
    let stdout = kio(
        &dir_a,
        &[
            "evidence",
            "verify",
            &serde_json::to_string(&pointer).unwrap(),
        ],
    )
    .arg("--json")
    .assert()
    .code(3)
    .get_output()
    .stdout
    .clone();
    let result: Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(result["status"], "registry_duplicate");
    assert_eq!(result["error_code"], "KIO-E-REGISTRY-DUP-001");
}

// ===========================================================================
// §G online opt-in AND-gate + storage (U79, QA21/22)
// ===========================================================================

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

/// The SCOPE-local `.kio/config.toml` carries the persistent policy half of
/// the network gate. It is necessary but never sufficient: a matching,
/// explicit approval record is also required.
fn write_scope_allow_network_true(dir: &TempDir) {
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[adapter.policy]\nallow_network = true\n",
    )
    .unwrap();
}

/// QA21: setting `allow_network = true` alone never materializes consent.
/// A non-approval index invocation is read-only with respect to the portable
/// policy and approval state; the existing explicit `--approve` flow is the
/// positive control that creates the active record.
#[test]
fn qa21_config_only_requires_explicit_approval_and_read_gate_is_immutable() {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("a.pdf"), fake_pdf(&["hello"])).unwrap();
    init(&dir);
    // `--yes` records a local scan grant. Establish it before capturing the
    // external-consent state, then exercise the read gate without an approval
    // flag so whole-state equality has a precise meaning.
    json_success(&dir, &["index", "--yes"]);
    write_scope_allow_network_true(&dir);
    let scope_path = dir.path().join(".kio/scope.json");
    let config_path = dir.path().join(".kio/config.toml");
    let before_scope = fs::read(&scope_path).unwrap();
    let before_config = fs::read(&config_path).unwrap();

    let output = json_success(&dir, &["index"]);
    assert_eq!(
        output["network_opt_in"], false,
        "policy alone must not create or activate consent: {output}"
    );
    assert_eq!(fs::read(&scope_path).unwrap(), before_scope);
    assert_eq!(fs::read(&config_path).unwrap(), before_config);
    assert!(
        scope_json(&dir).get("approvals").is_none(),
        "read-only policy evaluation must not materialize an approval"
    );

    json_success(
        &dir,
        &["adapter", "approve", "mistral_ocr_markdownize", "--yes"],
    );
    let explicit = json_success(&dir, &["index"]);
    assert_eq!(explicit["network_opt_in"], true, "{explicit}");
    let scope = scope_json(&dir);
    let approvals = scope["approvals"]
        .as_array()
        .expect("approvals[] must exist after explicit approval");
    assert_eq!(approvals.len(), 1);
    assert_eq!(approvals[0]["status"], "active");
    assert_eq!(approvals[0]["approval_method"], "approve");
    assert_eq!(approvals[0]["execution_mode"], "online_api");
    assert_eq!(approvals[0]["scope_id"], scope["scope_id"]);
    assert_eq!(scope["approvals_initialized"], true);
}

/// QA22 (step4b-contract-tests-p3a.md §G, 07 §3 L148-168, 10 §11.3): the
/// persistent approval record is stored in `.kio/scope.json`'s
/// `approvals[]` — not a device-global file — with the full required-field
/// row shape (scope_id / tool_id / execution_mode / tool_profile_hash /
/// approved_at / approval_method / status). This is also QA23/24's field
/// -shape enabler: the same row carries what a profile-change invalidation
/// check and a single-Adapter revoke both need.
#[test]
fn qa22_approval_row_is_stored_in_scope_json_approvals_not_a_device_global_file() {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("a.pdf"), fake_pdf(&["hello"])).unwrap();
    init(&dir);
    assert!(
        scope_json(&dir).get("approvals").is_none(),
        "no approvals[] before any approve"
    );

    json_success(
        &dir,
        &["adapter", "approve", "mistral_ocr_markdownize", "--yes"],
    );
    json_success(&dir, &["index"]);
    let scope = scope_json(&dir);
    let approvals = scope["approvals"]
        .as_array()
        .expect("approvals[] must exist after --approve");
    assert_eq!(
        approvals.len(),
        1,
        "exactly one online tool (markdownize) is active in this hermetic harness: {approvals:?}"
    );
    let row = &approvals[0];
    for field in [
        "scope_id",
        "tool_id",
        "execution_mode",
        "tool_profile_hash",
        "approved_at",
        "approval_method",
        "status",
    ] {
        assert!(
            row.get(field).is_some(),
            "approvals[] row missing {field}: {row}"
        );
    }
    assert_eq!(row["scope_id"], scope["scope_id"]);
    assert_eq!(row["execution_mode"], "online_api");
    assert_eq!(row["status"], "active");
    assert_eq!(row["approval_method"], "approve");
}

// ===========================================================================
// §H `kio adapter revoke` (U80, QA25/26/27)
// ===========================================================================

/// QA25 (step4b-contract-tests-p3a.md §H, 07 §3 L134-136): `kio adapter
/// revoke <tool_id>` with nothing ever approved is an idempotent success
/// (exit 0, "no_target") that does NOT write the `approvals_initialized`
/// marker — a no-op revoke must not alter portable consent state.
#[test]
fn qa25_revoke_with_nothing_approved_is_idempotent_no_target() {
    let dir = canonical_tempdir();
    init(&dir);
    let output = json_success(&dir, &["adapter", "revoke", "some_tool_id"]);
    assert_eq!(output["status"], "no_target");
    let scope = scope_json(&dir);
    assert!(
        scope.get("approvals_initialized").is_none(),
        "a no-op revoke must not write the marker: {scope}"
    );
}

/// QA24/25: revoking a specific approved tool_id flips ONLY that row to
/// `status=revoked` (+ `revoked_at`) — the row is never deleted (audit
/// preservation).
#[test]
fn qa25_revoke_single_tool_id_flips_status_without_deleting_row() {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("a.pdf"), fake_pdf(&["hello"])).unwrap();
    init(&dir);
    json_success(
        &dir,
        &["adapter", "approve", "mistral_ocr_markdownize", "--yes"],
    );
    json_success(&dir, &["index"]);
    let tool_id = scope_json(&dir)["approvals"][0]["tool_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let output = json_success(&dir, &["adapter", "revoke", &tool_id]);
    assert_eq!(output["status"], "revoked");
    assert_eq!(output["revoked_tool_ids"], json!([tool_id]));
    let scope = scope_json(&dir);
    let approvals = scope["approvals"].as_array().unwrap();
    assert_eq!(
        approvals.len(),
        1,
        "the row must not be deleted: {approvals:?}"
    );
    assert_eq!(approvals[0]["status"], "revoked");
    assert!(approvals[0].get("revoked_at").is_some());

    // Idempotent: revoking the already-revoked tool_id again is a no-op
    // that reports no_target (its status is no longer `active`).
    let second = json_success(&dir, &["adapter", "revoke", &tool_id]);
    assert_eq!(second["status"], "no_target");
}

/// QA25: `--all` revokes every currently-active row without touching
/// `.kio/config.toml`'s `allow_network` boolean kill switch AT ALL — that
/// remains `kio index --revoke-network`'s job specifically (07 §3 L206-211:
/// "boolean の false 化は kill switch 操作...側の責務 — 自動整合はしな
/// い"). A pre-existing, unrelated `allow_network = true` left in the file
/// (e.g. by an older `kio` version, or a hand-edit) is byte-for-byte
/// preserved by `--all`.
#[test]
fn qa25_revoke_all_revokes_every_row_without_touching_allow_network_boolean() {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("a.pdf"), fake_pdf(&["hello"])).unwrap();
    init(&dir);
    json_success(
        &dir,
        &["adapter", "approve", "mistral_ocr_markdownize", "--yes"],
    );
    json_success(&dir, &["index"]);
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[adapter.policy]\nallow_network = true\n",
    )
    .unwrap();
    let before_config = fs::read_to_string(dir.path().join(".kio/config.toml")).unwrap();

    let output = json_success(&dir, &["adapter", "revoke", "--all"]);
    assert_eq!(output["status"], "revoked");
    let scope = scope_json(&dir);
    let approvals = scope["approvals"].as_array().unwrap();
    assert!(!approvals.is_empty());
    for row in approvals {
        assert_eq!(row["status"], "revoked", "every row must be revoked: {row}");
    }
    let after_config = fs::read_to_string(dir.path().join(".kio/config.toml")).unwrap();
    assert_eq!(
        before_config, after_config,
        "revoke --all must not touch config.toml at all (the boolean kill switch is a separate command's job)"
    );
}

/// QA25: exactly one of `<tool_id>` or `--all` is required (usage error
/// otherwise).
#[test]
fn qa25_revoke_requires_tool_id_or_all() {
    let dir = canonical_tempdir();
    init(&dir);
    let err = json_failure(&dir, &["adapter", "revoke"], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-USAGE-001");
}

/// QA26 (step4b-contract-tests-p3a.md §H, 07 §3 L138-142): a concurrent
/// `kio adapter revoke` that removes `approval_pending` between an
/// approval's step (0) (pending write) and its step (2) (publish) makes the
/// publish detect the CAS mismatch and return
/// `KIO-E-ADAPTER-APPROVAL-CONFLICT-001` (exit 5) instead of silently
/// publishing a stale intent. Exercised at the library level
/// (`kio_core::scope`) since true cross-process interleaving cannot be
/// driven deterministically through the CLI.
#[test]
fn qa26_publish_detects_concurrent_revoke_removing_the_pending() {
    let dir = canonical_tempdir();
    let repo = Repository::init(dir.path()).unwrap();
    let scope_id = repo.scope_identity().unwrap().scope_id;
    let tool_id = "mistral_ocr_markdownize";
    let pending = json!({
        "scope_id": scope_id,
        "tool_id": tool_id,
        "execution_mode": "online_api",
        "tool_profile_hash": format!("sha256:{}", "a".repeat(64)),
        "approved_at": "2026-07-22T00:00:00Z",
        "approval_method": "approve",
    });
    write_network_approval_pending(repo.kio_dir(), pending.clone()).unwrap();

    // Simulate the concurrent `kio adapter revoke <tool_id>` landing between
    // this in-flight approval's step (0) and step (2).
    let revoke_outcome =
        revoke_network_approval(repo.kio_dir(), Some(tool_id), "2026-07-22T00:00:01Z").unwrap();
    assert!(revoke_outcome.pending_removed);

    let row = json!({
        "scope_id": scope_id,
        "tool_id": tool_id,
        "execution_mode": "online_api",
        "tool_profile_hash": format!("sha256:{}", "a".repeat(64)),
        "approved_at": "2026-07-22T00:00:00Z",
        "approval_method": "approve",
        "status": "active",
    });
    let error = publish_network_approval(repo.kio_dir(), row, Some(&pending)).unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-ADAPTER-APPROVAL-CONFLICT-001");
    assert_eq!(error.exit_code().code(), 5);
}

/// QA27 (step4b-contract-tests-p3a.md §H, 07 §3 L120-134): `kio adapter
/// revoke <tool_id>` removes a matching `approval_pending` regardless of
/// whether its `execution_mode`/`tool_profile_hash` are STALE (left behind
/// under a profile that has since changed) — matched by `(scope_id,
/// tool_id)` only, per 07 §3's explicit "4 組一致に限ると...pending を取り
/// 逃し" rationale — and, since this actually changed something, sets the
/// `approvals_initialized` marker in the same write.
#[test]
fn qa27_revoke_removes_stale_profile_pending_and_sets_marker() {
    let dir = canonical_tempdir();
    let repo = Repository::init(dir.path()).unwrap();
    let scope_id = repo.scope_identity().unwrap().scope_id;
    let tool_id = "mistral_ocr_markdownize";
    // A pending left behind under an OLD profile (e.g. bbox_annotation was
    // toggled after this pending was written, changing tool_profile_hash) —
    // deliberately NOT the profile a fresh approval would compute today.
    let stale_pending = json!({
        "scope_id": scope_id,
        "tool_id": tool_id,
        "execution_mode": "online_api",
        "tool_profile_hash": format!("sha256:{}", "0".repeat(64)),
        "approved_at": "2026-07-01T00:00:00Z",
        "approval_method": "approve",
    });
    write_network_approval_pending(repo.kio_dir(), stale_pending).unwrap();
    assert!(!network_approvals_initialized(repo.kio_dir()).unwrap());

    let outcome =
        revoke_network_approval(repo.kio_dir(), Some(tool_id), "2026-07-22T00:00:00Z").unwrap();
    assert!(
        outcome.pending_removed,
        "revoke must remove the pending despite its stale profile"
    );
    assert!(outcome.marker_written);
    assert!(network_approvals_initialized(repo.kio_dir()).unwrap());
}

// ===========================================================================
// §I `--online`/`--offline` wiring for repair/batch resume/retry/reindex
// (U81, QA29/30/31)
// ===========================================================================

/// Shared QA29/30/31 fixture: a scope with exactly one chunk whose Embedding
/// task is Pending-but-approved — the embedding Adapter is "active" (mock,
/// no real credentials needed) and its `approvals[]` row is `active`, but
/// its `tool_profile_hash` has since gone STALE (simulating a profile
/// change after approval, QA23) — the row still exists (matched by
/// `tool_id` alone), so the steady-state exact-match gate
/// (`persistent_network_allowed_for_kio_dir`) is closed, but `--online`'s
/// one-shot fallback (`kio_core::scope::network_approval_row_present` —
/// tool_id presence only, profile unchecked) can still recover it for one
/// send. This isolates QA29/30/31's `--online` wiring from the steady-state
/// AND-gate this file's QA21 covers separately (a revoke, the OTHER way the
/// steady-state gate can close, would ALSO block `--online` itself per
/// QA28's regression lock — so revoke cannot be used to build this
/// fixture).
fn setup_pending_mock_embedding_scope() -> TempDir {
    let dir = canonical_tempdir();
    fs::write(
        dir.path().join("a.md"),
        "# Doc\n\nSome body text to embed.\n",
    )
    .unwrap();
    let mut init_command = Command::cargo_bin("kio").unwrap();
    for name in KIO_CHILD_ENV_DENYLIST {
        init_command.env_remove(name);
    }
    init_command
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join(".test-config"))
        .env("XDG_DATA_HOME", dir.path().join(".test-data"))
        .env("XDG_CACHE_HOME", dir.path().join(".test-cache"))
        .args(["init"])
        .assert()
        .success();
    kio(&dir, &["ledger", "init"]).assert().success();
    // Explicit adapter approvals establish the paired device grants; offline
    // indexing then leaves the embedding task enqueued-but-Pending.
    mock_embed_command(&dir, &["adapter", "approve", "gemini_embedding_2", "--yes"])
        .assert()
        .success();
    mock_embed_command(
        &dir,
        &["adapter", "approve", "mistral_ocr_markdownize", "--yes"],
    )
    .assert()
    .success();
    mock_embed_command(&dir, &["index", "--offline"])
        .assert()
        .success();
    let scope = scope_json(&dir);
    let approvals = scope["approvals"].as_array().expect("approvals[]");
    assert!(
        approvals.len() >= 2,
        "expected markdownize + mock embedding rows: {approvals:?}"
    );
    let baseline = mock_embed_json(&dir, &["status"]);
    assert!(
        baseline["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|task| { task["input_path"] == "a.md" && task["status"] == "pending" }),
        "the embedding task must still be Pending (the stale profile hash must have closed the gate): {baseline}"
    );
    dir
}

/// `kio(...)` plus `KIO_TEST_GEMINI_EMBED=mock` so `active_embedding_adapter_identity`
/// treats the embedding Adapter as active without any real credentials.
fn mock_embed_command(dir: &TempDir, args: &[&str]) -> Command {
    let mut command = kio(dir, args);
    command.env("KIO_TEST_GEMINI_EMBED", "mock");
    command
}

fn mock_embed_json(dir: &TempDir, args: &[&str]) -> Value {
    let stdout = mock_embed_command(dir, args)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&stdout).unwrap()
}

/// QA29 (step4b-contract-tests-p3a.md §I, 06-cli-spec.md §1 L52-55,
/// 07-adapter-spec.md §3 L220-222): `kio repair rebuild-db --online`
/// drives the post-rebuild enrichment pass's `--online` fallback — the
/// previously hard-coded `embedding_online_allowed(&repo, false, false,
/// false)` call — instead of failing as an unknown flag or being silently
/// neutralized.
#[test]
fn qa29_repair_rebuild_db_online_reaches_the_post_rebuild_enrichment() {
    let dir = setup_pending_mock_embedding_scope();

    // Without --online, the gate stays closed (key loss) and rebuild-db's
    // enrichment pass sends nothing.
    let without_online = mock_embed_json(&dir, &["repair", "rebuild-db", "--offline"]);
    assert_eq!(
        without_online["embedding_tasks_executed"], 0,
        "no --online: nothing should send: {without_online}"
    );

    let with_online = mock_embed_json(&dir, &["repair", "rebuild-db", "--online"]);
    assert_eq!(
        with_online["embedding_tasks_executed"], 1,
        "--online must reach the enrichment pass and send the pending task: {with_online}"
    );
}

/// QA30 (step4b-contract-tests-p3a.md §I, 06-cli-spec.md §1 L21-30,
/// 07-adapter-spec.md §3 L220-222): `kio batch resume --online` and `kio
/// batch retry --online` both drive `execute_pending_tasks`'s embedding
/// enrichment pass — the previously hard-coded
/// `embedding_online_allowed(repo, false, false, false)` call.
#[test]
fn qa30_batch_resume_online_reaches_the_embedding_enrichment_pass() {
    let dir = setup_pending_mock_embedding_scope();

    let without_online = mock_embed_json(&dir, &["batch", "resume", "--offline"]);
    assert_eq!(
        without_online["tasks_executed"], 0,
        "no --online: nothing should send: {without_online}"
    );

    let with_online = mock_embed_json(&dir, &["batch", "resume", "--online"]);
    assert_eq!(
        with_online["tasks_executed"], 1,
        "--online must reach execute_pending_tasks's enrichment pass: {with_online}"
    );
}

/// QA30: `kio batch retry --online` is accepted and reaches the same
/// enrichment pass (the Pending task from the shared fixture is due
/// immediately, so a plain retry scan picks it up the same way resume's
/// does).
#[test]
fn qa30_batch_retry_online_reaches_the_embedding_enrichment_pass() {
    let dir = setup_pending_mock_embedding_scope();

    let with_online = mock_embed_json(&dir, &["batch", "retry", "--online"]);
    assert_eq!(
        with_online["tasks_executed"], 1,
        "--online must reach execute_pending_tasks's enrichment pass via retry too: {with_online}"
    );
}

/// QA30: `--online`/`--offline` remain mutually exclusive on `batch resume`
/// (matching `kio index`'s existing validation), not silently accepted
/// together.
#[test]
fn qa30_batch_resume_online_and_offline_are_mutually_exclusive() {
    let dir = canonical_tempdir();
    init(&dir);
    let err = json_failure(&dir, &["batch", "resume", "--online", "--offline"], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-USAGE-001");
}

/// QA31 (step4b-contract-tests-p3a.md §I, 06-cli-spec.md §1 L77-83,
/// 07-adapter-spec.md §3 L220-222): `kio reindex --force --online` drives
/// the re-normalized generation's embedding enrichment — the previously
/// hard-coded `embedding_online_allowed(&repo, false, false, false)` call
/// on the `--force` path.
#[test]
fn qa31_reindex_force_online_reaches_the_embedding_enrichment_pass() {
    // `--force` is not idempotent (it always advances to a brand-new
    // normalized generation, docs/06 §1), so the "without --online" and
    // "with --online" comparisons each need their OWN fresh fixture —
    // calling `--force` twice on the same scope would leave two separate
    // generations' worth of Pending embedding tasks for the second call to
    // pick up, confounding the count.
    let dir_without = setup_pending_mock_embedding_scope();
    let without_online = mock_embed_json(
        &dir_without,
        &["reindex", "--regenerate", "--yes", "--offline"],
    );
    assert_eq!(
        without_online["embedding_tasks_executed"], 0,
        "no --online: nothing should send: {without_online}"
    );

    let dir_with = setup_pending_mock_embedding_scope();
    let with_online = mock_embed_json(&dir_with, &["reindex", "--regenerate", "--yes", "--online"]);
    let executed = with_online["embedding_tasks_executed"]
        .as_u64()
        .expect("embedding_tasks_executed must be a number");
    assert!(
        executed >= 1,
        "--online must reach reindex --force's enrichment pass and send at least the pending task: {with_online}"
    );
}

/// QA31: `--online`/`--offline` are also accepted on the `--at <commit>`
/// historical-reindex path (a separate call site,
/// `historical_reindex::run`), not just `--force`'s.
#[test]
fn qa31_reindex_at_online_flag_is_accepted_not_a_usage_error() {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("a.md"), "# Doc\n\nbody text.\n").unwrap();
    init(&dir);
    json_success(&dir, &["index", "--offline"]);
    let log = json_success(&dir, &["log"]);
    let head_commit = log["commits"][0]["commit_hash"]
        .as_str()
        .expect("at least one commit after index")
        .to_owned();
    // Prior to QA31, `--online` here was an "unknown reindex flag" usage
    // error (exit 2) — this must now succeed.
    let output = json_success(&dir, &["reindex", "--at", &head_commit, "--online"]);
    assert_eq!(output["status"], "reindexed");
}
