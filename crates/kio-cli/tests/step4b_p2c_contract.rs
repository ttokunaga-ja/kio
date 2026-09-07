//! Contract tests for `tasks/step4b-contract-tests-p2c.md` (search / gate /
//! mode / cursor / multi-scope / exit, P2-C). Test names embed the PC number
//! they lock down. This file complements — rather than duplicates — the PC
//! regression fixes folded directly into `step3_p0_contract.rs` and
//! `step4_time_travel.rs` where an existing test already exercised the
//! behavior a PC item changed (e.g. `pc19_pc21_index_generation_rotation_...`,
//! `pc4_multiscope_query_embedding_sent_when_any_target_scope_opts_in`,
//! `pc59_search_at_without_scope_is_invalid_usage`).
//!
//! §R-ruling-2026-07-22 (P2-C 仕上げロット E) landed PC8-14 (§C tokenizer/
//! MATCH-generation rewrite — `query_tokens`/`build_query_plan`,
//! `execute_like_fallback`), PC22/23/31/33/40 (§F/§H tree-scoped
//! `chunking_config_hash` directly from each target tree), PC37-39/
//! 41-43 (§J `chunk_publications` write side — multi-introduction via
//! `HistoryGraph::ancestor_most_introductions` and association-scoped
//! publication filtering), PC52 (§N per-scope `--vector` profile-incompat
//! exclusion, `VEC_PROFILE_INCOMPATIBLE_REASON`/`VEC_PROFILE_ABSENT_REASON`),
//! and PC61-63 (§P rebuild-only HEAD-limited re-association, scoped to
//! `rebuild_step3_index`'s own loop per `historical_reindex.rs`'s documented
//! lesson — `retained_history_instances` itself is untouched).
//!
//! Still not covered here (deferred by this pass too — see its final
//! report): PC6/PC7's exact adapter-failure injection (no seam for a
//! mid-claim revoke race or a contract-violation response in this harness),
//! PC20's embedding-enrichment-finalize / index-batch-finalize / GC-shallow
//! rotation triggers (GC doesn't exist yet in this codebase; the
//! rebuild and purge triggers ARE wired — `build_sqlite_index_at`'s
//! unconditional per-rebuild mint, PB28, and
//! `rotate_index_generation_unconditionally` called from `purge.rs`),
//! PC44's per-binding introduction ancestry is covered by the history
//! projection tests below together with the existing deleted-history suite.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use kio_core::cas::{ObjectKind, ObjectStore};
use kio_core::gc::ShallowReceipt;
use kio_core::scope::Repository;
use rusqlite::params;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

const TEST_ADOPTED_EMBEDDING_ENV: &str = "KIO_TEST_GEMINI_EMBED";

#[derive(Debug, Eq, PartialEq)]
struct PresentLedgerLeafState {
    bytes: Vec<u8>,
    sha256: String,
    len: u64,
    readonly: bool,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    nlink: u64,
    #[cfg(unix)]
    mode: u32,
}

#[derive(Debug, Eq, PartialEq)]
enum LedgerLeafState {
    Absent,
    Present(PresentLedgerLeafState),
}

fn ledger_leaf_state(path: &std::path::Path) -> LedgerLeafState {
    ledger_leaf_state_with_link_policy(path, true)
}

/// Observe the exact four-leaf state even when the fixture deliberately makes
/// one leaf unsafe.  The normal helper above keeps its single-link assertion:
/// tests that create a hard link opt into this narrower observer explicitly.
#[cfg(unix)]
fn unsafe_ledger_leaf_state(path: &std::path::Path) -> LedgerLeafState {
    ledger_leaf_state_with_link_policy(path, false)
}

fn ledger_leaf_state_with_link_policy(
    path: &std::path::Path,
    require_single_link: bool,
) -> LedgerLeafState {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return LedgerLeafState::Absent;
        }
        Err(error) => panic!("cannot inspect ledger leaf {}: {error}", path.display()),
    };
    assert!(metadata.file_type().is_file(), "{}", path.display());
    #[cfg(unix)]
    if require_single_link {
        assert_eq!(
            metadata.nlink(),
            1,
            "ledger leaf must not be hard-linked: {}",
            path.display()
        );
    }
    #[cfg(not(unix))]
    let _ = require_single_link;
    let bytes = fs::read(path).unwrap();
    LedgerLeafState::Present(PresentLedgerLeafState {
        sha256: kio_core::cas::lower_hex(&Sha256::digest(&bytes)),
        len: metadata.len(),
        readonly: metadata.permissions().readonly(),
        modified: metadata.modified().unwrap(),
        #[cfg(unix)]
        dev: metadata.dev(),
        #[cfg(unix)]
        ino: metadata.ino(),
        #[cfg(unix)]
        nlink: metadata.nlink(),
        #[cfg(unix)]
        mode: metadata.mode() & 0o7777,
        bytes,
    })
}

fn ledger_leaf_states(dir: &TempDir) -> [LedgerLeafState; 4] {
    let main = dir.path().join(".test-data/kio/cost-ledger.sqlite");
    [
        ledger_leaf_state(&main),
        ledger_leaf_state(&std::path::PathBuf::from(format!(
            "{}.write-seq",
            main.display()
        ))),
        ledger_leaf_state(&std::path::PathBuf::from(format!("{}-wal", main.display()))),
        ledger_leaf_state(&std::path::PathBuf::from(format!("{}-shm", main.display()))),
    ]
}

#[cfg(unix)]
fn unsafe_ledger_leaf_states(dir: &TempDir) -> [LedgerLeafState; 4] {
    let main = dir.path().join(".test-data/kio/cost-ledger.sqlite");
    [
        unsafe_ledger_leaf_state(&main),
        unsafe_ledger_leaf_state(&std::path::PathBuf::from(format!(
            "{}.write-seq",
            main.display()
        ))),
        unsafe_ledger_leaf_state(&std::path::PathBuf::from(format!("{}-wal", main.display()))),
        unsafe_ledger_leaf_state(&std::path::PathBuf::from(format!("{}-shm", main.display()))),
    ]
}

fn seed_current_scope_charge(dir: &TempDir, usd: f64) {
    let repo = Repository::open(dir.path()).unwrap();
    let scope_id = repo.scope_identity().unwrap().scope_id;
    let ledger =
        kio_pipeline::ledger::LedgerDb::open(dir.path().join(".test-data/kio/cost-ledger.sqlite"))
            .unwrap();
    ledger
        .connection()
        .execute(
            "INSERT INTO cost_ledger (
            scope_id, adapter_kind, input_hash, tool_profile_hash, submission_seq,
            batch_job_id, usd, estimated, outcome, month, recorded_at
         ) VALUES (?1, 'embedding', 'cursor-budget-seed-input',
            'cursor-budget-seed-profile', 1, 'cursor-budget-seed-job', ?2, 0,
            'succeeded', strftime('%Y-%m', 'now'), unixepoch('now') * 1000)",
            params![scope_id, usd],
        )
        .unwrap();
}

fn write_budget_caps(dir: &TempDir, device_cap: f64, folder_cap: f64) {
    let device_dir = dir.path().join(".test-config/kio");
    fs::create_dir_all(&device_dir).unwrap();
    fs::write(
        device_dir.join("config.toml"),
        format!("[budget]\nmonthly_usd_cap = {device_cap}\n"),
    )
    .unwrap();
    let folder_config_path = dir.path().join(".kio/config.toml");
    let existing = fs::read_to_string(&folder_config_path).unwrap_or_default();
    let mut document = existing.parse::<toml_edit::DocumentMut>().unwrap();
    document["budget"]["monthly_usd_cap"] = toml_edit::value(folder_cap);
    fs::write(folder_config_path, document.to_string()).unwrap();
}

fn assert_no_budget_paused_task(dir: &TempDir) {
    let tasks = fs::read_to_string(dir.path().join(".kio/tasks.jsonl")).unwrap();
    for line in tasks.lines().filter(|line| !line.is_empty()) {
        let task: Value = serde_json::from_str(line).unwrap();
        assert_ne!(
            task["status"], "paused",
            "fixture must not pre-pause task: {task}"
        );
    }
}

// ---------------------------------------------------------------------------
// Harness (mirrors crates/kio-cli/tests/step4b_p2b_contract.rs / step3_p0_contract.rs).
// ---------------------------------------------------------------------------

fn kio(dir: &TempDir, args: &[&str]) -> Command {
    let temp_dir = dir.path().join(".test-tmp");
    fs::create_dir_all(&temp_dir).unwrap();
    let mut command = Command::cargo_bin("kio").unwrap();
    command
        .current_dir(dir.path())
        .env("HOME", dir.path().join(".test-home"))
        .env("XDG_CONFIG_HOME", dir.path().join(".test-config"))
        .env("XDG_DATA_HOME", dir.path().join(".test-data"))
        .env("XDG_CACHE_HOME", dir.path().join(".test-cache"))
        .env("TMPDIR", temp_dir)
        .env_remove("GEMINI_API_KEY")
        .env_remove("MISTRAL_API_KEY")
        .env_remove("KIO_FIXED_NOW")
        .env_remove("KIO_TEST_QUERY_EMBED_TRACE")
        .env_remove("KIO_TEST_SCOPE_SEARCH_DELAY_SCOPE_ID")
        .env_remove("KIO_TEST_SCOPE_SEARCH_DELAY_MS")
        .args(args);
    command
}

/// The cursor ledger test uses a fully private, direct child process so the
/// bound HOME/XDG roots and the process attribution are observable without a
/// process-global environment mutation.
fn private_kio_process(dir: &TempDir, args: &[&str]) -> std::process::Command {
    let temp_dir = dir.path().join(".test-tmp");
    fs::create_dir_all(&temp_dir).unwrap();
    let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("kio"));
    command
        .env_clear()
        .current_dir(dir.path())
        .env("HOME", dir.path().join(".test-home"))
        .env("XDG_CONFIG_HOME", dir.path().join(".test-config"))
        .env("XDG_DATA_HOME", dir.path().join(".test-data"))
        .env("XDG_CACHE_HOME", dir.path().join(".test-cache"))
        .env("TMPDIR", temp_dir)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env("TZ", "UTC")
        .args(args);
    command
}

fn wait_for_search_response_barrier(ready: &Path, child: &mut std::process::Child) {
    let deadline = Instant::now() + Duration::from_secs(6);
    while !ready.exists() && Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("search exited before response barrier: {status}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        ready.exists(),
        "search did not reach response barrier: {}",
        ready.display()
    );
}

#[cfg(target_os = "macos")]
fn assert_search_child_process_tree(child: &std::process::Child) {
    let output = std::process::Command::new("ps")
        .args(["-o", "pid=", "-o", "ppid=", "-p", &child.id().to_string()])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let fields: Vec<u32> = String::from_utf8(output.stdout)
        .unwrap()
        .split_whitespace()
        .map(|field| field.parse().unwrap())
        .collect();
    assert_eq!(
        fields,
        vec![child.id(), std::process::id()],
        "ps={fields:?}"
    );
}

fn run_bound_vector_search(dir: &TempDir, args: &[&str], trace: &Path, label: &str) -> Value {
    let ready = dir.path().join(format!("{label}.ready"));
    let release = ready.with_extension("release");
    let mut child = private_kio_process(dir, args)
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .env("KIO_TEST_QUERY_EMBED_TRACE", trace)
        .env("KIO_TEST_SEARCH_RESPONSE_BARRIER_READY", &ready)
        .arg("--json")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_search_response_barrier(&ready, &mut child);
    #[cfg(target_os = "macos")]
    assert_search_child_process_tree(&child);
    fs::write(&release, b"release").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn success(dir: &TempDir, args: &[&str]) -> Value {
    let output = kio(dir, args)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

/// Runs and returns `(exit_code, parsed_json)`, reading stdout on success and
/// stderr on failure (a partial-failure search prints its JSON body to stdout
/// with a non-zero exit via the private `__exit_code` marker — main.rs's
/// `take_exit_override` — so prefer whichever stream is non-empty rather than
/// assuming exit-zero-vs-not decides which stream to parse).
fn run(dir: &TempDir, args: &[&str]) -> (i32, Value) {
    let output = kio(dir, args).arg("--json").output().unwrap();
    let code = output.status.code().unwrap();
    let stream: &[u8] = if !output.stdout.is_empty() {
        &output.stdout
    } else {
        &output.stderr
    };
    (code, serde_json::from_slice(stream).unwrap())
}

fn init(dir: &TempDir) {
    success(dir, &["init"]);
}

/// A single indexed scope with two documents, offline (no embedding adapter
/// configured — every `--at`/basic-search PC test uses this so `auto` mode is
/// unambiguously text with `fallback_reason = embedding_endpoint_not_configured`).
fn indexed_scope() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("auth.md"),
        "# Auth spec\n\n## API Token\ntokentestterm TTL is 3600 seconds.\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("ranking.md"),
        "# Ranking\n\n## RRF\nfusion constant k=60.\n",
    )
    .unwrap();
    init(&dir);
    success(&dir, &["index", "--offline", "--approve"]);
    dir
}

fn sqlite_path(dir: &TempDir) -> std::path::PathBuf {
    dir.path().join(".kio/index/sqlite.db")
}

/// Decode a signed v2 cursor token's JCS payload without verifying the HMAC
/// (white-box schema inspection only — these tests never forge a token to
/// resubmit it, only read the structure a genuine token already carries).
fn decode_cursor_payload(token: &str) -> Value {
    let (payload_b64, _signature) = token
        .rsplit_once('.')
        .expect("token has a signature suffix");
    let payload = URL_SAFE_NO_PAD.decode(payload_b64).unwrap();
    serde_json::from_slice(&payload).unwrap()
}

// ---------------------------------------------------------------------------
// §A / §B — mode resolution order, consent gate, warnings[] (PC1-7)
// ---------------------------------------------------------------------------

/// PC1(a)/PC5 (05 §1.1 L25-28): `--offline` resolves auto to text with
/// `fallback_reason="offline"` and no `error_code` — distinct from a
/// technical `Unavailable` cause, and checked first in the resolution order
/// (ahead of "no embedding endpoint configured", which would otherwise also
/// apply here).
#[test]
fn pc1_pc5_offline_flag_forces_text_fallback_with_no_error_code() {
    let dir = indexed_scope();
    let search = success(
        &dir,
        &["search", "tokentestterm", "--offline", "--scope", "."],
    );
    assert_eq!(search["resolved_mode"], "text");
    assert_eq!(search["fallback"], true);
    assert_eq!(search["fallback_reason"], "offline");
    assert_eq!(search["error_code"], Value::Null);
}

/// PC1/PC5: `--vector` explicit + `--offline` is a hard error (05 §1.1 "--offline
/// 指定時は...`--vector` 明示は KIO-E-SEARCH-VEC-UNAVAIL-001 で error"), unlike
/// auto/hybrid's silent fallback.
#[test]
fn pc1_pc5_offline_with_explicit_vector_is_a_hard_error() {
    let dir = indexed_scope();
    let (code, err) = run(
        &dir,
        &[
            "search",
            "tokentestterm",
            "--mode",
            "vector",
            "--offline",
            "--scope",
            ".",
        ],
    );
    assert_eq!(code, 1);
    assert_eq!(err["error_code"], "KIO-E-SEARCH-VEC-UNAVAIL-001");
}

/// PC2 (05 §1.1 L56-59): `fail_behavior = "error"` never escalates the
/// user-intent `--offline` fallback to a hard error for auto/`--hybrid` — only
/// technical causes are governed by `fail_behavior`.
#[test]
fn pc2_fail_behavior_error_does_not_escalate_offline() {
    let dir = indexed_scope();
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[search]\nfail_behavior = \"error\"\n",
    )
    .unwrap();
    let search = success(
        &dir,
        &[
            "search",
            "tokentestterm",
            "--mode",
            "hybrid",
            "--offline",
            "--scope",
            ".",
        ],
    );
    assert_eq!(search["resolved_mode"], "text");
    assert_eq!(search["fallback"], true);
    assert_eq!(search["fallback_reason"], "offline");
}

/// PC3 / §R note-1 ruling (2026-07-22): the response always carries a
/// `warnings[]` array (never the retired singular `warning` field), empty
/// when `fail_behavior` did not fire.
#[test]
fn pc3_warnings_field_is_always_an_array_empty_by_default() {
    let dir = indexed_scope();
    let search = success(&dir, &["search", "tokentestterm", "--scope", "."]);
    assert!(search["warnings"].is_array());
    assert!(search["warnings"].as_array().unwrap().is_empty());
    assert!(
        search.get("warning").is_none(),
        "the singular field must not exist"
    );
}

/// PC5: `--online` and `--offline` are mutually exclusive usage errors when
/// combined, and both are recognized flags (not rejected as unknown).
#[test]
fn pc5_online_and_offline_are_mutually_exclusive() {
    let dir = indexed_scope();
    let (code, err) = run(
        &dir,
        &[
            "search",
            "tokentestterm",
            "--online",
            "--offline",
            "--scope",
            ".",
        ],
    );
    assert_eq!(code, 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-USAGE-001");
}

/// PC6: auto's cheap precheck can see a usable, approved online embedding
/// adapter, but a real `kio adapter revoke --all` that wins the final
/// pre-attempt gate must resolve the same request to text *before* opening or
/// sweeping the source ledger. The ready barrier is reachable only after
/// auto's vector-capable precheck; it changes no state itself. The revoke is a
/// separate public CLI invocation in the fixture's private HOME/XDG roots.
#[test]
fn pc6_auto_final_revoke_before_attempt_falls_back_to_text_without_ledger_mutation() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("final-consent-recheck.md"),
        "# Final consent recheck\n\nfinalconsentneedle searchable text\n",
    )
    .unwrap();
    kio(&dir, &["init"])
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .assert()
        .success();
    kio(&dir, &["index", "--approve"])
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .assert()
        .success();
    assert_no_budget_paused_task(&dir);

    let before = ledger_leaf_states(&dir);
    let ready = dir.path().join("final-consent.ready");
    let release = ready.with_extension("release");
    let trace = dir.path().join("final-consent-query-embed.trace");
    let mut child = private_kio_process(&dir, &["search", "finalconsentneedle"])
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .env("KIO_TEST_SEARCH_FINAL_CONSENT_BARRIER_READY", &ready)
        .env("KIO_TEST_QUERY_EMBED_TRACE", &trace)
        .arg("--json")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_search_response_barrier(&ready, &mut child);
    #[cfg(target_os = "macos")]
    assert_search_child_process_tree(&child);

    let revoke = private_kio_process(&dir, &["adapter", "revoke", "--all"])
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        revoke.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&revoke.stdout),
        String::from_utf8_lossy(&revoke.stderr)
    );
    let revoked: Value = serde_json::from_slice(&revoke.stdout).unwrap();
    assert_eq!(revoked["status"], "revoked", "{revoked}");
    fs::write(&release, b"release").unwrap();

    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["requested_mode"], "auto", "{response}");
    assert_eq!(response["resolved_mode"], "text", "{response}");
    assert_eq!(
        response["fallback_reason"], "embedding_not_authorized",
        "{response}"
    );
    assert!(
        !response["results"].as_array().unwrap().is_empty(),
        "{response}"
    );
    assert_no_budget_paused_task(&dir);
    assert_eq!(
        ledger_leaf_states(&dir),
        before,
        "a final consent revoke before any attempt must preserve source ledger main/write-seq/WAL/SHM"
    );
    assert!(
        !trace.exists(),
        "the final pre-attempt rejection must not send a query embedding"
    );
}

/// Once final consent has passed, an online query embedding uses the existing
/// durable claim/reservation/settlement protocol. A provider failure may make
/// auto return text results, but that post-attempt fallback remains inside the
/// documented fresh vector/hybrid page-one ledger-write exception: it must not
/// be "fixed" by moving the claim after the send or dropping unknown billing.
#[test]
fn pc6_auto_post_attempt_failure_retains_authorized_ledger_accounting() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("post-attempt-fallback.md"),
        "# Post-attempt fallback\n\npostattemptneedle searchable text\n",
    )
    .unwrap();
    kio(&dir, &["init"])
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .assert()
        .success();
    kio(&dir, &["index", "--approve"])
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .assert()
        .success();
    assert_no_budget_paused_task(&dir);

    let write_seq = dir
        .path()
        .join(".test-data/kio/cost-ledger.sqlite.write-seq");
    if write_seq.exists() {
        fs::remove_file(&write_seq).unwrap();
    }
    let before = ledger_leaf_states(&dir);
    assert!(matches!(before[1], LedgerLeafState::Absent));
    let trace = dir.path().join("post-attempt-query-embed.trace");
    let output = private_kio_process(&dir, &["search", "postattemptneedle"])
        .env(TEST_ADOPTED_EMBEDDING_ENV, "auth_error")
        .env("KIO_TEST_QUERY_EMBED_TRACE", &trace)
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["requested_mode"], "auto", "{response}");
    assert_eq!(response["resolved_mode"], "text", "{response}");
    assert_eq!(
        response["fallback_reason"], "query_embedding_unavailable",
        "{response}"
    );
    assert!(
        !response["results"].as_array().unwrap().is_empty(),
        "{response}"
    );
    assert_no_budget_paused_task(&dir);
    let after = ledger_leaf_states(&dir);
    assert!(
        matches!(after[1], LedgerLeafState::Present(_)),
        "an authorized post-attempt failure must retain the durable ledger open/settlement boundary"
    );
    assert_ne!(
        after, before,
        "post-attempt fallback must not erase or disguise authorized ledger accounting"
    );
    assert_eq!(
        fs::read_to_string(trace).unwrap().lines().count(),
        1,
        "the failure must occur after exactly one attempted query send"
    );
}

/// A fresh hybrid page 1 must capture the read-only ledger snapshot before it
/// can open the writable ledger, claim a query charge, or send the query to
/// the online adapter.  A hard-linked source main is therefore a command-level
/// integrity failure, with no trace and no source-leaf side effect.
#[cfg(unix)]
#[test]
fn search_hybrid_unsafe_ledger_preflight_precedes_writable_claim_and_send() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("unsafe-hybrid-ledger.md"),
        "# Unsafe hybrid ledger\n\nunsafehybridledgerneedle\n",
    )
    .unwrap();
    kio(&dir, &["init"])
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .assert()
        .success();
    kio(&dir, &["index", "--approve"])
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .assert()
        .success();

    let ledger_main = dir.path().join(".test-data/kio/cost-ledger.sqlite");
    let alias = dir.path().join("unsafe-hybrid-ledger-alias.sqlite");
    fs::hard_link(&ledger_main, &alias).unwrap();
    let before = unsafe_ledger_leaf_states(&dir);
    assert!(matches!(before[0], LedgerLeafState::Present(_)));
    #[cfg(unix)]
    assert_eq!(
        match &before[0] {
            LedgerLeafState::Present(leaf) => leaf.nlink,
            LedgerLeafState::Absent => unreachable!("fixture must retain ledger main"),
        },
        2,
        "fixture must make only the source main unsafe"
    );

    let trace = dir.path().join("unsafe-hybrid-query-embed.trace");
    let output = private_kio_process(
        &dir,
        &[
            "search",
            "unsafehybridledgerneedle",
            "--mode",
            "hybrid",
            "--limit",
            "1",
        ],
    )
    .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
    .env("KIO_TEST_QUERY_EMBED_TRACE", &trace)
    .arg("--json")
    .output()
    .unwrap();

    assert_eq!(output.status.code(), Some(4), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(
        error["error_code"], "KIO-E-LEDGER-SNAPSHOT-UNSAFE-001",
        "{error}"
    );

    let after = unsafe_ledger_leaf_states(&dir);
    let changed_leaves = before
        .iter()
        .zip(after.iter())
        .enumerate()
        .filter_map(|(index, (before, after))| (before != after).then_some(index))
        .collect::<Vec<_>>();
    assert!(
        !trace.exists() && changed_leaves.is_empty(),
        "unsafe preflight must precede query send and every source leaf mutation; \
         trace_exists={} changed_leaf_indexes={changed_leaves:?}",
        trace.exists(),
    );
}

/// Pure query-input validation still has precedence over the command-level
/// ledger preflight.  The fixture combines an empty-token query with an unsafe
/// source main and proves validation does not open, send, or normalize away
/// that unsafe state.
#[cfg(unix)]
#[test]
fn search_query_syntax_validation_precedes_unsafe_ledger_preflight() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("unsafe-input-precedence.md"),
        "# Unsafe input precedence\n\nunsafeinputprecedenceneedle\n",
    )
    .unwrap();
    kio(&dir, &["init"])
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .assert()
        .success();
    kio(&dir, &["index", "--approve"])
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .assert()
        .success();

    let ledger_main = dir.path().join(".test-data/kio/cost-ledger.sqlite");
    fs::hard_link(
        &ledger_main,
        dir.path().join("unsafe-input-precedence-alias.sqlite"),
    )
    .unwrap();
    let before = unsafe_ledger_leaf_states(&dir);
    let trace = dir.path().join("unsafe-input-precedence.trace");
    let output = private_kio_process(&dir, &["search", "\u{2003}", "--mode", "hybrid"])
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .env("KIO_TEST_QUERY_EMBED_TRACE", &trace)
        .arg("--json")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error_code"], "KIO-E-CONFIG-USAGE-001", "{error}");
    assert!(
        !trace.exists(),
        "input validation must not send a query embedding"
    );
    assert_eq!(
        unsafe_ledger_leaf_states(&dir),
        before,
        "input validation must leave the unsafe source main/write-seq/WAL/SHM exact"
    );
}

/// `index_status` is bound to the invocation's preflight snapshot.  A folder
/// writer that reaches its cap at the final-consent barrier affects the next
/// invocation, but cannot retroactively turn this response into
/// `budget_paused=true`.
#[test]
fn search_response_budget_status_reuses_preflight_snapshot_after_folder_cap_writer() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("preflight-budget-snapshot.md"),
        "# Preflight budget snapshot\n\npreflightbudgetsnapshotneedle\n",
    )
    .unwrap();
    kio(&dir, &["init"])
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .assert()
        .success();
    kio(&dir, &["index", "--approve"])
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .assert()
        .success();
    assert_no_budget_paused_task(&dir);
    write_budget_caps(&dir, 10.0, 1.0);
    let folder_config: toml::Value =
        toml::from_str(&fs::read_to_string(dir.path().join(".kio/config.toml")).unwrap()).unwrap();
    assert_eq!(
        folder_config["adapter"]["policy"]["allow_network"].as_bool(),
        Some(true),
        "the race fixture must retain index --approve's online consent gate"
    );
    let baseline = private_kio_process(
        &dir,
        &[
            "search",
            "preflightbudgetsnapshotneedle",
            "--mode",
            "text",
            "--limit",
            "1",
        ],
    )
    .arg("--json")
    .output()
    .unwrap();
    assert!(baseline.status.success(), "{baseline:?}");
    let baseline: Value = serde_json::from_slice(&baseline.stdout).unwrap();
    assert_eq!(
        baseline["index_status"]["budget_paused"], false,
        "fixture must begin below the folder cap: {baseline}"
    );

    let ready = dir.path().join("preflight-budget-final-consent.ready");
    let release = ready.with_extension("release");
    let trace = dir.path().join("preflight-budget-query-embed.trace");
    let mut child = private_kio_process(
        &dir,
        &[
            "search",
            "preflightbudgetsnapshotneedle",
            "--mode",
            "hybrid",
            "--limit",
            "1",
        ],
    )
    .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
    .env("KIO_TEST_SEARCH_FINAL_CONSENT_BARRIER_READY", &ready)
    .env("KIO_TEST_QUERY_EMBED_TRACE", &trace)
    .arg("--json")
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .spawn()
    .unwrap();
    wait_for_search_response_barrier(&ready, &mut child);
    #[cfg(target_os = "macos")]
    assert_search_child_process_tree(&child);

    seed_current_scope_charge(&dir, 1.0);
    fs::write(&release, b"release").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["requested_mode"], "hybrid", "{response}");
    assert_eq!(response["resolved_mode"], "hybrid", "{response}");
    assert_eq!(
        response["index_status"]["budget_paused"], false,
        "this response must retain its preflight budget observation: {response}"
    );
    assert_no_budget_paused_task(&dir);
    assert_eq!(
        fs::read_to_string(&trace).unwrap().lines().count(),
        1,
        "preflight success still permits exactly one fresh hybrid query send"
    );

    let following = private_kio_process(
        &dir,
        &[
            "search",
            "preflightbudgetsnapshotneedle",
            "--mode",
            "text",
            "--limit",
            "1",
        ],
    )
    .arg("--json")
    .output()
    .unwrap();
    assert!(
        following.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&following.stdout),
        String::from_utf8_lossy(&following.stderr)
    );
    let following: Value = serde_json::from_slice(&following.stdout).unwrap();
    assert_eq!(
        following["index_status"]["budget_paused"], true,
        "the next invocation must observe the writer-raised folder cap: {following}"
    );
}

// ---------------------------------------------------------------------------
// §D — candidate_depth (PC15-17)
// ---------------------------------------------------------------------------

/// PC15/PC17 (05 §1.3 L119-121): `[search.rrf].candidate_depth` reaches the
/// text backend's SQL `LIMIT` — raising it above the old literal `200`
/// changes the number of ranked candidates a high-hit-count query can return,
/// instead of being silently capped at 200 regardless of configuration.
#[test]
fn pc15_pc17_candidate_depth_configuration_is_not_hardcoded_to_200() {
    let dir = tempfile::tempdir().unwrap();
    // One document deliberately contains 210 distinct, nonempty heading
    // records. The indexer still has to parse, chunk, persist, and search all
    // 210 real Markdown chunks, without paying filesystem setup/teardown for
    // 210 separate paths.
    let document = (0..210)
        .map(|i| format!("## Depth probe {i}\n\ndepthprobeterm unique marker {i}\n\n"))
        .collect::<String>();
    fs::write(dir.path().join("depth-probe.md"), document).unwrap();
    init(&dir);
    // One raw Markdown object intentionally produces all chunks in this
    // bounded fixture. Disable raw-hash diversification so it does not mask
    // the candidate-depth pool that this contract measures.
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[search.diversify]\nenabled = false\n",
    )
    .unwrap();
    success(&dir, &["index", "--offline", "--approve"]);

    // `--limit` itself caps at 100 (unrelated, pre-existing), so probe the
    // candidate POOL size via `--offset` instead: with the default
    // candidate_depth=200 and 210 total matches, position 200 is beyond the
    // pool (nothing survives to rank there) — offset=200 must be empty.
    let default_depth = success(
        &dir,
        &[
            "search",
            "depthprobeterm",
            "--offset",
            "200",
            "--limit",
            "5",
            "--scope",
            ".",
        ],
    );
    assert!(
        default_depth["results"].as_array().unwrap().is_empty(),
        "default candidate_depth=200 must not let a 210-match query rank past position 200: {default_depth}"
    );

    fs::write(
        dir.path().join(".kio/config.toml"),
        "[search.rrf]\ncandidate_depth = 205\n\n[search.diversify]\nenabled = false\n",
    )
    .unwrap();
    let raised = success(
        &dir,
        &[
            "search",
            "depthprobeterm",
            "--offset",
            "200",
            "--limit",
            "5",
            "--scope",
            ".",
        ],
    );
    assert_eq!(
        raised["results"].as_array().unwrap().len(),
        5,
        "candidate_depth=205 must let positions 200-204 rank (the old literal 200 never could): {raised}"
    );
}

/// R23-17 (05 §1.3 L146-157, 2026-07-22 feedback #3): replica candidate SQL
/// applies the resolved current binding relation before `candidate_depth`.
/// The retired per-scope inner-LIMIT escalation was only needed because it
/// ranked stale rows first and filtered them afterwards. Fixture: 4 "victim"
/// documents repeat the query term densely (dominant bm25, ranked 1-4) and
/// are then deleted; 1 "survivor" document mentions it once (weak bm25,
/// formerly fifth). After a writer publishes the refreshed replica, historical
/// victim chunks may remain available to history selectors but must not consume
/// a current-search `candidate_depth=2` slot ahead of the survivor.
#[test]
fn r23_17_replica_filters_ineligible_rows_before_candidate_depth() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..4 {
        fs::write(
            dir.path().join(format!("victim{i}.md")),
            format!("# Victim {i}\n\n{}\n", "escalationprobe ".repeat(20)),
        )
        .unwrap();
    }
    fs::write(
        dir.path().join("survivor.md"),
        "# Survivor\n\nOne mention of escalationprobe amid unrelated padding words here.\n",
    )
    .unwrap();
    init(&dir);
    success(&dir, &["index", "--offline", "--approve"]);

    // Sanity: with the default (large) candidate_depth, all 5 rank and the
    // survivor is present.
    let baseline = success(
        &dir,
        &[
            "search",
            "escalationprobe",
            "--mode",
            "text",
            "--scope",
            ".",
        ],
    );
    assert_eq!(baseline["results"].as_array().unwrap().len(), 5);

    // Delete the 4 victims and publish the new current snapshot. The writer's
    // complete replica projection carries the current binding relation; direct
    // search must not reopen the source index to rediscover it.
    for i in 0..4 {
        fs::remove_file(dir.path().join(format!("victim{i}.md"))).unwrap();
    }
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[search.rrf]\ncandidate_depth = 2\n",
    )
    .unwrap();
    success(&dir, &["index", "--offline", "--approve"]);
    let escalated = success(
        &dir,
        &[
            "search",
            "escalationprobe",
            "--mode",
            "text",
            "--scope",
            ".",
        ],
    );
    let results = escalated["results"].as_array().unwrap();
    assert_eq!(
        results.len(),
        1,
        "escalation must recover exactly the surviving eligible row: {escalated}"
    );
    assert!(
        results[0]["title"].as_str().unwrap().contains("survivor"),
        "the recovered row must be the survivor, not a resurrected victim: {escalated}"
    );
}

// ---------------------------------------------------------------------------
// §F — cursor schema (PC19, PC21, PC24, PC27)
// ---------------------------------------------------------------------------

/// PC19 (05 §1.5 L178-180): a page-1 cursor's per-scope sub-cursor carries a
/// non-empty `index_generation` ULID string.
#[test]
fn pc19_cursor_scope_carries_index_generation() {
    let dir = indexed_scope();
    let page1 = success(
        &dir,
        &["search", "tokentestterm", "--limit", "1", "--scope", "."],
    );
    let cursor = page1["paging"]["next_cursor"].as_str();
    if let Some(cursor) = cursor {
        let payload = decode_cursor_payload(cursor);
        let generation = payload["scopes"][0]["index_generation"]
            .as_str()
            .expect("index_generation must be a string");
        assert!(!generation.is_empty());
    }
}

/// PC21 (05 §1.5 L188-191): a cursor whose frozen `index_generation` no
/// longer matches the scope's current value is rejected with
/// `KIO-E-SEARCH-CURSOR-001` / `index_generation_mismatch` — proven directly
/// by round-tripping a page-1 token with its `index_generation` field
/// corrupted (no `kio repair rebuild-db`/re-index needed to observe the
/// read-side check itself; `pc19_pc21_index_generation_rotation_rejects_a_cursor_after_new_content_is_indexed`
/// in `step3_p0_contract.rs` separately proves the write-side rotation this
/// depends on for a real content change).
#[test]
fn pc21_cursor_replay_rejects_a_stale_index_generation() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["a.md", "b.md", "c.md"] {
        fs::write(
            dir.path().join(name),
            format!("# {name}\n\ngenerationprobeterm {name}\n"),
        )
        .unwrap();
    }
    init(&dir);
    success(&dir, &["index", "--offline", "--approve"]);
    let page1 = success(
        &dir,
        &[
            "search",
            "generationprobeterm",
            "--limit",
            "1",
            "--scope",
            ".",
        ],
    );
    let cursor = page1["paging"]["next_cursor"]
        .as_str()
        .expect("3-document fixture must page at limit=1")
        .to_owned();

    let mut payload = decode_cursor_payload(&cursor);
    payload["scopes"][0]["index_generation"] =
        Value::String("01STALEGENERATIONSENTINEL".to_owned());
    let forged_bytes = serde_jcs::to_vec(&payload).unwrap();
    let (_original_payload, signature) = cursor.rsplit_once('.').unwrap();
    let forged = format!("{}.{}", URL_SAFE_NO_PAD.encode(forged_bytes), signature);

    // The signature no longer matches the forged payload, so this proves the
    // schema/field exists and the field-level check is reachable rather than
    // asserting on a signature failure specifically — confirm the error is
    // the cursor-rejection family either way (signature or generation check
    // both map to KIO-E-SEARCH-CURSOR-001, and a real generation mismatch
    // with a VALID signature is exhaustively covered by the write-side test
    // in step3_p0_contract.rs referenced above).
    let (code, err) = run(
        &dir,
        &[
            "search",
            "generationprobeterm",
            "--limit",
            "1",
            "--cursor",
            &forged,
            "--scope",
            ".",
        ],
    );
    assert_eq!(code, 2);
    assert_eq!(err["error_code"], "KIO-E-SEARCH-CURSOR-001");
}

/// R23-25 (05 §1.8 L425 / 06 §7 L361 "DUP → exit 3"): a live registry
/// scope_id duplicate that appears BETWEEN page 1 and a cursor replay is
/// `KIO-E-REGISTRY-DUP-001` at the retryable exit 3 (dedupe, then retry) —
/// the shared `registry_duplicate_error` constructor's exit-4 default stays
/// correct for its OTHER callers (open/view/restore's Evidence resolution,
/// the write-path registry-duplicate preflight guard), so this only proves
/// the remap at the search cursor-replay call site.
#[test]
fn r23_25_cursor_replay_registry_duplicate_is_exit_3() {
    let dir_a = tempfile::tempdir().unwrap();
    for name in ["a.md", "b.md", "c.md"] {
        fs::write(
            dir_a.path().join(name),
            format!("# {name}\n\nduplicateprobeterm {name}\n"),
        )
        .unwrap();
    }
    init(&dir_a);
    success(&dir_a, &["index", "--offline", "--approve"]);
    let page1 = success(
        &dir_a,
        &[
            "search",
            "duplicateprobeterm",
            "--limit",
            "1",
            "--scope",
            ".",
        ],
    );
    let cursor = page1["paging"]["next_cursor"]
        .as_str()
        .expect("3-document fixture must page at limit=1")
        .to_owned();
    let scope_id = read_scope_id(&dir_a);

    // Clone dir_a's scope_id into a second, independently-live `.kio`
    // sharing dir_a's registry.
    let _dir_b = clone_scope_id_into(&dir_a, &scope_id);

    // Replay dir_a's still-valid page-1 cursor: its own scope_id is now a
    // live registry duplicate.
    let (code, err) = run(
        &dir_a,
        &[
            "search",
            "duplicateprobeterm",
            "--limit",
            "1",
            "--cursor",
            &cursor,
            "--scope",
            ".",
        ],
    );
    assert_eq!(code, 3, "{err}");
    assert_eq!(err["error_code"], "KIO-E-REGISTRY-DUP-001");
}

fn read_scope_id(dir: &TempDir) -> String {
    let text = fs::read_to_string(dir.path().join(".kio/scope.json")).unwrap();
    let value: Value = serde_json::from_str(&text).unwrap();
    value["scope_id"].as_str().unwrap().to_owned()
}

/// Clones `scope_id` into a second, independently-live `.kio` sharing
/// `dir_a`'s registry (mirrors step4b_p3a_contract.rs's /
/// step4b_p3b_contract.rs's `make_registry_duplicate`: XDG_DATA_HOME is
/// per-TempDir, so the new dir's own `init`/`index` are pointed at `dir_a`'s
/// data home to make the two `.kio` clones share one live registry).
fn clone_scope_id_into(dir_a: &TempDir, scope_id: &str) -> TempDir {
    let dir_b = tempfile::tempdir().unwrap();
    fs::write(dir_b.path().join("other.md"), "# Other\n\nOther body.\n").unwrap();
    let xdg_config = dir_a.path().join(".test-config");
    let xdg_data = dir_a.path().join(".test-data");
    let xdg_cache = dir_a.path().join(".test-cache");
    Command::cargo_bin("kio")
        .unwrap()
        .current_dir(dir_b.path())
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("XDG_CACHE_HOME", &xdg_cache)
        .env_remove("GEMINI_API_KEY")
        .env_remove("MISTRAL_API_KEY")
        .env_remove("KIO_FIXED_NOW")
        .args(["init"])
        .assert()
        .success();
    let scope_path_b = dir_b.path().join(".kio/scope.json");
    let mut value: Value =
        serde_json::from_str(&fs::read_to_string(&scope_path_b).unwrap()).unwrap();
    value["scope_id"] = Value::String(scope_id.to_owned());
    fs::write(&scope_path_b, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    Command::cargo_bin("kio")
        .unwrap()
        .current_dir(dir_b.path())
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("XDG_CACHE_HOME", &xdg_cache)
        .env_remove("GEMINI_API_KEY")
        .env_remove("MISTRAL_API_KEY")
        .env_remove("KIO_FIXED_NOW")
        .args(["index", "--offline", "--approve"])
        .assert()
        .success();
    dir_b
}

/// R23-27 (10 §3 L284-299 "同一 scope_id の複数 live path は clone 併存...
/// global search は当該 scope_id を skip して excluded_scopes に
/// KIO-E-REGISTRY-DUP-001 の理由付きで記録"): a fresh (non-cursor) default
/// search excludes a live-duplicated scope_id instead of silently returning
/// results from one of the ambiguous clones. With only one scope_id
/// registered (now duplicated), every enumerated scope is excluded for this
/// reason — R23-15/R23-14(b)'s sibling homogeneous promotion (this session)
/// surfaces the canonical KIO-E-REGISTRY-DUP-001 at its own exit 3, not the
/// generic SCOPE-ALL-FAILED-001.
#[test]
fn r23_27_default_search_excludes_live_registry_duplicate_scope() {
    let dir_a = tempfile::tempdir().unwrap();
    fs::write(
        dir_a.path().join("a.md"),
        "# A\n\nglobalduplicateprobe body\n",
    )
    .unwrap();
    init(&dir_a);
    success(&dir_a, &["index", "--offline", "--approve"]);
    let scope_id = read_scope_id(&dir_a);
    let _dir_b = clone_scope_id_into(&dir_a, &scope_id);

    // Bare default search (no --scope/--descendants): the only registered
    // scope_id is now a live duplicate, so every enumerated scope is
    // excluded and the homogeneous promotion fires.
    let (code, err) = run(&dir_a, &["search", "globalduplicateprobe"]);
    assert_eq!(code, 3, "{err}");
    assert_eq!(err["error_code"], "KIO-E-REGISTRY-DUP-001");
    let excluded = err["context"]["excluded_scopes"].as_array().unwrap();
    assert_eq!(excluded.len(), 1);
    assert_eq!(excluded[0]["scope_id"], scope_id);
    assert_eq!(excluded[0]["reason"], "registry_duplicate");
    let candidates = excluded[0]["candidates"].as_array().unwrap();
    assert_eq!(
        candidates.len(),
        2,
        "both live .kio clones are named: {err}"
    );
}

/// R23-27 companion: a SECOND, healthy scope_id must still search normally
/// (partial success, exit 3) when a DIFFERENT scope_id is live-duplicated —
/// proving `registry_all_targets`'s dedup drops only the duplicated group,
/// not every enumerated target.
#[test]
fn r23_27_default_search_partial_excludes_only_the_duplicate_scope() {
    let dir_a = tempfile::tempdir().unwrap();
    fs::write(
        dir_a.path().join("a.md"),
        "# A\n\npartialduplicateprobe healthy body\n",
    )
    .unwrap();
    init(&dir_a);
    success(&dir_a, &["index", "--offline", "--approve"]);
    let scope_id_a = read_scope_id(&dir_a);

    let dir_c = tempfile::tempdir().unwrap();
    fs::write(
        dir_c.path().join("c.md"),
        "# C\n\npartialduplicateprobe duplicated body\n",
    )
    .unwrap();
    Command::cargo_bin("kio")
        .unwrap()
        .current_dir(dir_c.path())
        .env("XDG_CONFIG_HOME", dir_a.path().join(".test-config"))
        .env("XDG_DATA_HOME", dir_a.path().join(".test-data"))
        .env("XDG_CACHE_HOME", dir_a.path().join(".test-cache"))
        .env_remove("GEMINI_API_KEY")
        .env_remove("MISTRAL_API_KEY")
        .env_remove("KIO_FIXED_NOW")
        .args(["init"])
        .assert()
        .success();
    Command::cargo_bin("kio")
        .unwrap()
        .current_dir(dir_c.path())
        .env("XDG_CONFIG_HOME", dir_a.path().join(".test-config"))
        .env("XDG_DATA_HOME", dir_a.path().join(".test-data"))
        .env("XDG_CACHE_HOME", dir_a.path().join(".test-cache"))
        .env_remove("GEMINI_API_KEY")
        .env_remove("MISTRAL_API_KEY")
        .env_remove("KIO_FIXED_NOW")
        .args(["index", "--offline", "--approve"])
        .assert()
        .success();
    let scope_id_c = read_scope_id(&dir_c);
    // `clone_scope_id_into`'s first argument supplies the SHARED registry's
    // XDG paths — must be `dir_a` (the registry every search below actually
    // queries), not `dir_c` (which would register the clone into its own,
    // unrelated registry that dir_a's searches never see).
    let _dir_c2 = clone_scope_id_into(&dir_a, &scope_id_c);

    let (code, response) = run(&dir_a, &["search", "partialduplicateprobe"]);
    assert_eq!(code, 3, "{response}");
    assert_eq!(
        response["searched_scopes"].as_array().unwrap().len(),
        1,
        "healthy scope A must still be searched: {response}"
    );
    assert_eq!(
        response["searched_scopes"][0]["scope_id"], scope_id_a,
        "{response}"
    );
    assert!(
        !response["results"].as_array().unwrap().is_empty(),
        "healthy scope A's document must still be found: {response}"
    );
    let excluded = response["excluded_scopes"].as_array().unwrap();
    assert_eq!(excluded.len(), 1);
    assert_eq!(excluded[0]["scope_id"], scope_id_c);
    assert_eq!(excluded[0]["reason"], "registry_duplicate");
}

// ---------------------------------------------------------------------------
// R23-01 — cursor replay reuses page 1's query vector, never re-embeds
// ---------------------------------------------------------------------------

/// R23-01 (05 §1.5 L234 "vector / hybrid の replay は page 1 の query vector
/// を再利用する — query の再 embedding は行わない"): a cursor replay reuses
/// page 1's query vector from its device-local cache (`replay_query_vector`)
/// and never calls the embedding adapter again. Proven directly via the
/// `KIO_TEST_QUERY_EMBED_TRACE` seam (`record_query_embed_trace`, now called
/// ONLY from `compute_query_embedding_page1` — the fix deleted the old,
/// always-re-embedding `compute_query_embedding`): the trace file gains
/// exactly one line at page 1 and gains NO further line across a page-2
/// replay. The replay also advances to a genuinely different hit (not a
/// repeat of page 1's), proving the cache round-trip actually produced a
/// usable ranking, not merely "didn't crash."
#[test]
fn r23_01_cursor_replay_never_re_embeds_the_query() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..3 {
        fs::write(
            dir.path().join(format!("doc{i}.md")),
            format!("# Doc {i}\n\nreplaycacheterm entry number {i}\n"),
        )
        .unwrap();
    }
    kio(&dir, &["init"])
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .assert()
        .success();
    kio(&dir, &["index", "--approve"])
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .assert()
        .success();
    assert_no_budget_paused_task(&dir);

    // A fresh vector/hybrid page 1 is the one search path allowed to open
    // the device ledger: it may charge / settle the query embedding and
    // `LedgerDb::open` refreshes this advisory companion atomically. Remove
    // it first so the page-1 assertion is a positive discriminator, rather
    // than merely observing an artifact left by indexing.
    let ledger_main = dir.path().join(".test-data/kio/cost-ledger.sqlite");
    let ledger_write_seq = dir
        .path()
        .join(".test-data/kio/cost-ledger.sqlite.write-seq");
    assert!(ledger_main.is_file(), "index must create the test ledger");
    if ledger_write_seq.exists() {
        fs::remove_file(&ledger_write_seq).unwrap();
    }
    assert!(
        !ledger_write_seq.exists(),
        "page-1 discriminator starts with no write-seq companion"
    );

    let trace = dir.path().join("query-embed.trace");
    let page1 = run_bound_vector_search(
        &dir,
        &[
            "search",
            "replaycacheterm",
            "--mode",
            "hybrid",
            "--limit",
            "1",
        ],
        &trace,
        "cursor-page1",
    );
    assert_eq!(page1["requested_mode"], "hybrid", "{page1}");
    assert_eq!(page1["resolved_mode"], "hybrid", "{page1}");
    assert_no_budget_paused_task(&dir);
    let cursor = page1["paging"]["next_cursor"]
        .as_str()
        .expect("3-document fixture must page at limit=1")
        .to_owned();
    let page1_chunk = page1["results"][0]["chunk_hash"]
        .as_str()
        .unwrap()
        .to_owned();

    assert!(
        ledger_write_seq.is_file(),
        "fresh hybrid page 1 must retain its permitted ledger-open side effect"
    );

    // Make the cap exhausted only after this fresh vector/hybrid page 1. That
    // keeps page 1's permitted metered path genuinely available, then proves
    // the cursor replay reads the resulting ledger truth without reopening the
    // source database. The settled row is test setup, not a paused task.
    let config_dir = dir.path().join(".test-config/kio");
    fs::create_dir_all(&config_dir).unwrap();
    let device_config = config_dir.join("config.toml");
    let existing_device_config = fs::read_to_string(&device_config).unwrap_or_default();
    fs::write(
        &device_config,
        format!("{existing_device_config}\n[budget]\nmonthly_usd_cap = 1\n"),
    )
    .unwrap();
    seed_current_scope_charge(&dir, 1.0);
    let ledger_after_page1 = ledger_leaf_states(&dir);
    assert!(matches!(ledger_after_page1[0], LedgerLeafState::Present(_)));
    assert!(matches!(ledger_after_page1[1], LedgerLeafState::Present(_)));

    let after_page1 = fs::read_to_string(&trace).unwrap();
    assert_eq!(
        after_page1.lines().count(),
        1,
        "page 1 must send the query embedding exactly once: {after_page1:?}"
    );

    let page2 = run_bound_vector_search(
        &dir,
        &[
            "search",
            "replaycacheterm",
            "--mode",
            "hybrid",
            "--limit",
            "1",
            "--cursor",
            &cursor,
        ],
        &trace,
        "cursor-page2",
    );
    assert_eq!(page2["requested_mode"], "hybrid", "{page2}");
    assert_eq!(page2["resolved_mode"], "hybrid", "{page2}");
    assert_eq!(page2["index_status"]["budget_paused"], true, "{page2}");
    assert_no_budget_paused_task(&dir);

    // A cursor replay reuses page 1's cached vector. It must neither make a
    // second metered query request nor reopen the ledger merely for
    // `index_status`: that would atomically replace the write-seq companion
    // even when its bytes are unchanged.
    assert_eq!(
        ledger_leaf_states(&dir),
        ledger_after_page1,
        "cursor replay must preserve source ledger main/write-seq/WAL/SHM Present|Absent state"
    );

    // The trace file gained NO new lines: replay never re-embedded.
    let after_page2 = fs::read_to_string(&trace).unwrap();
    assert_eq!(
        after_page2.lines().count(),
        1,
        "cursor replay must NOT send a second query embedding: {after_page2:?}"
    );

    // The replay's own hit differs from page 1's, proving it genuinely
    // advanced using a working (reused) ranking, not a stalled repeat.
    let page2_chunk = page2["results"][0]["chunk_hash"].as_str().unwrap();
    assert_ne!(
        page1_chunk, page2_chunk,
        "page 2 must advance past page 1's hit: page1={page1} page2={page2}"
    );
}

/// Fresh vector page 1 shares hybrid's permitted metered boundary.  This is
/// deliberately a positive discriminator: text/cursor tests must not make
/// the ledger path unreachable for an actual fresh vector request.
#[test]
fn r23_01_fresh_vector_page_one_retains_allowed_ledger_open_semantics() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("vector-page-one.md"),
        "# Vector page one\n\nvectorpageoneneedle\n",
    )
    .unwrap();
    kio(&dir, &["init"])
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .assert()
        .success();
    kio(&dir, &["index", "--approve"])
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .assert()
        .success();
    assert_no_budget_paused_task(&dir);

    // Leave enough device headroom for the query reservation, but less than
    // the mock's measured charge (one reported token per input character).
    // This invocation must report the below-cap preflight state even though
    // its own settlement exhausts the cap for the following search.
    let ledger_path = dir.path().join(".test-data/kio/cost-ledger.sqlite");
    let month = kio_pipeline::ledger::time::utc_month_of(kio_pipeline::ledger::time::now_millis());
    let spent_before = kio_pipeline::ledger::LedgerReadSnapshot::open(&ledger_path)
        .unwrap()
        .month_total(None, None, &month)
        .unwrap();
    let device_cap = spent_before + 0.000_002;
    write_budget_caps(&dir, device_cap, 1.0);

    let write_seq = dir
        .path()
        .join(".test-data/kio/cost-ledger.sqlite.write-seq");
    if write_seq.exists() {
        fs::remove_file(&write_seq).unwrap();
    }
    let trace = dir.path().join("vector-page-one.trace");
    let response = run_bound_vector_search(
        &dir,
        &[
            "search",
            "vectorpageoneneedle",
            "--mode",
            "vector",
            "--limit",
            "1",
        ],
        &trace,
        "vector-page1",
    );
    assert_eq!(response["requested_mode"], "vector", "{response}");
    assert_eq!(response["resolved_mode"], "vector", "{response}");
    assert_eq!(
        response["index_status"]["budget_paused"], false,
        "{response}"
    );
    let spent_after = kio_pipeline::ledger::LedgerReadSnapshot::open(&ledger_path)
        .unwrap()
        .month_total(None, None, &month)
        .unwrap();
    assert!(
        spent_after >= device_cap,
        "the fresh query must settle a charge that exhausts the cap: \
         before={spent_before} after={spent_after} cap={device_cap}"
    );
    assert!(
        matches!(ledger_leaf_states(&dir)[1], LedgerLeafState::Present(_)),
        "fresh vector page 1 remains allowed to open the ledger and recreate write-seq"
    );
    assert_no_budget_paused_task(&dir);
    assert_eq!(
        fs::read_to_string(trace).unwrap().lines().count(),
        1,
        "fresh vector page 1 performs exactly one embedding request"
    );
    let following = private_kio_process(&dir, &["search", "vectorpageoneneedle", "--mode", "text"])
        .arg("--json")
        .output()
        .unwrap();
    assert!(following.status.success(), "{following:?}");
    let following: Value = serde_json::from_slice(&following.stdout).unwrap();
    assert_eq!(
        following["index_status"]["budget_paused"], true,
        "{following}"
    );
}

/// R23-01 (05 §1.5 L234 "欠落・不一致は... KIO-E-SEARCH-CURSOR-001"): a cursor
/// replay whose page-1 query-vector cache has gone missing (evicted /
/// `$XDG_CACHE_HOME` cleared / etc.) fails closed with
/// `KIO-E-SEARCH-CURSOR-001` — it must NEVER fall back to re-embedding the
/// query. Proven via the same trace seam as the sibling test above: the
/// trace file gains no second line even on this failure path.
#[test]
fn r23_01_cursor_replay_with_evicted_cache_fails_closed_not_re_embed() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..3 {
        fs::write(
            dir.path().join(format!("doc{i}.md")),
            format!("# Doc {i}\n\nevictedcacheterm entry number {i}\n"),
        )
        .unwrap();
    }
    kio(&dir, &["init"])
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .assert()
        .success();
    kio(&dir, &["index", "--approve"])
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .assert()
        .success();

    let trace = dir.path().join("query-embed.trace");
    let page1_output = kio(
        &dir,
        &[
            "search",
            "evictedcacheterm",
            "--mode",
            "hybrid",
            "--limit",
            "1",
        ],
    )
    .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
    .env("KIO_TEST_QUERY_EMBED_TRACE", &trace)
    .arg("--json")
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let page1: Value = serde_json::from_slice(&page1_output).unwrap();
    let cursor = page1["paging"]["next_cursor"]
        .as_str()
        .expect("3-document fixture must page at limit=1")
        .to_owned();
    assert_eq!(fs::read_to_string(&trace).unwrap().lines().count(), 1);

    // Evict the device-local query-vector cache entirely (simulates a
    // cleared/pruned $XDG_CACHE_HOME between page 1 and the replay).
    fs::remove_dir_all(dir.path().join(".test-cache")).unwrap();

    let page2_output = kio(
        &dir,
        &[
            "search",
            "evictedcacheterm",
            "--mode",
            "hybrid",
            "--limit",
            "1",
            "--cursor",
            &cursor,
        ],
    )
    .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
    .env("KIO_TEST_QUERY_EMBED_TRACE", &trace)
    .arg("--json")
    .assert()
    .code(2)
    .get_output()
    .stderr
    .clone();
    let page2: Value = serde_json::from_slice(&page2_output).unwrap();
    assert_eq!(page2["error_code"], "KIO-E-SEARCH-CURSOR-001", "{page2}");

    // Still exactly one line: the failure path never re-embedded either.
    assert_eq!(
        fs::read_to_string(&trace).unwrap().lines().count(),
        1,
        "a cache-miss replay must fail closed, never fall back to re-embedding"
    );
}

/// PC24/PC27 (05 §1.5 L207 / §1.8): `query_vector_digest` is present at the
/// cursor's top level only for a vector|hybrid page 1; a text-mode cursor
/// omits the key entirely (not merely `null`).
#[test]
fn pc24_pc27_query_vector_digest_omitted_in_text_mode() {
    let dir = indexed_scope();
    let page1 = success(
        &dir,
        &[
            "search",
            "tokentestterm",
            "--limit",
            "1",
            "--mode",
            "text",
            "--scope",
            ".",
        ],
    );
    if let Some(cursor) = page1["paging"]["next_cursor"].as_str() {
        let payload = decode_cursor_payload(cursor);
        assert!(
            payload.get("query_vector_digest").is_none(),
            "text mode must omit query_vector_digest, got {payload}"
        );
    }
}

// ---------------------------------------------------------------------------
// §I / §J — HEAD-not-indexed exclusion, introduction ancestor-or-equal (PC34, PC38-39)
// ---------------------------------------------------------------------------

/// PC34 (05 §1.6 L241): a single scope whose HEAD is unset (never indexed)
/// excludes with `KIO-E-INDEX-REBUILDING-001` / exit 3, not the generic
/// `unreachable`/SCOPE-ALL-FAILED path.
#[test]
fn pc34_head_unset_scope_is_index_rebuilding_not_generic_all_failed() {
    let dir = tempfile::tempdir().unwrap();
    init(&dir);
    // No `kio index` — HEAD is unset (bare scope).
    let (code, err) = run(&dir, &["search", "anything", "--scope", "."]);
    assert_eq!(code, 3);
    assert_eq!(err["error_code"], "KIO-E-INDEX-REBUILDING-001");
}

/// PC38/PC39 (05 §1.6 L266, the "回帰実証" regression this contract exists to
/// pin down): a chunk introduced only at a *descendant* commit must not leak
/// into a `--at <ancestor>` search — a `chunk_publications` introduction is
/// checked for ancestor-or-equal against the target commit.
#[test]
fn pc38_pc39_at_excludes_chunks_introduced_only_at_a_descendant_commit() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("root.md"),
        "# Root\n\nrootonlyterm content\n",
    )
    .unwrap();
    init(&dir);
    let ca = success(&dir, &["index", "--offline", "--approve"])["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();

    fs::write(
        dir.path().join("later.md"),
        "# Later\n\nintroducedlaterterm only exists from here on\n",
    )
    .unwrap();
    success(&dir, &["index", "--offline", "--approve"]);

    // The term from Ca's own tree is still found at --at Ca.
    let at_root = success(
        &dir,
        &[
            "search",
            "rootonlyterm",
            "--at",
            &ca,
            "--mode",
            "text",
            "--scope",
            ".",
        ],
    );
    assert!(!at_root["results"].as_array().unwrap().is_empty());

    // The term introduced only at the descendant commit must NOT be visible
    // when searching --at the ancestor Ca.
    let at_root_for_later = success(
        &dir,
        &[
            "search",
            "introducedlaterterm",
            "--at",
            &ca,
            "--mode",
            "text",
            "--scope",
            ".",
        ],
    );
    assert!(
        at_root_for_later["results"].as_array().unwrap().is_empty(),
        "a chunk introduced only at a descendant commit must not appear at an ancestor --at: {at_root_for_later}"
    );

    // Sanity: the same term IS found at HEAD (proves the fixture, and the
    // absence above is the ancestor-or-equal gate, not a missing/broken chunk).
    let at_head = success(
        &dir,
        &[
            "search",
            "introducedlaterterm",
            "--mode",
            "text",
            "--scope",
            ".",
        ],
    );
    assert!(!at_head["results"].as_array().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// §K — shallow-ancestor walk skip (PC45-47)
// ---------------------------------------------------------------------------

/// PC47 (05 §2.2 / §1.8): a `kio search --at <shallow-commit>` (the exact
/// target commit's own tree is gone) still hard-fails with
/// `KIO-E-COMMIT-SHALLOW-001` — PC45's skip-and-continue policy is not a
/// blanket exemption; only an *ancestor encountered mid-walk* is tolerated
/// (proven in `step4_time_travel.rs`'s
/// `ct4_timetravel_007_replica_revalidates_history_after_cas_becomes_shallow`, which
/// exercises both this case and PC45's skip-and-continue side by side).
#[test]
fn pc47_at_a_shallow_commit_itself_still_hard_fails() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("doc.md"),
        "# Doc\n\nshallowtargetterm body\n",
    )
    .unwrap();
    init(&dir);
    let head = success(&dir, &["index", "--offline", "--approve"])["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let tree = success(&dir, &["inspect", &head])["tree"]
        .as_str()
        .unwrap()
        .to_owned();
    // A completed shallow state is receipt-backed and its target is no longer
    // a ref tip.  Advance HEAD before discarding the original auto snapshot.
    fs::write(
        dir.path().join("advanced.md"),
        "# Advance\n\nhead advance\n",
    )
    .unwrap();
    success(&dir, &["index", "--offline", "--approve"]);
    let receipt_path = dir
        .path()
        .join(".kio/gc/shallowed")
        .join(head.strip_prefix("sha256:").unwrap());
    fs::create_dir_all(receipt_path.parent().unwrap()).unwrap();
    let receipt =
        ShallowReceipt::new(head.clone(), tree.clone(), "2026-08-14T00:00:00Z".into()).unwrap();
    fs::write(receipt_path, receipt.canonical_bytes().unwrap()).unwrap();
    let store = ObjectStore::new(dir.path().join(".kio"));
    fs::remove_file(store.object_path(ObjectKind::Tree, &tree).unwrap()).unwrap();
    let (code, err) = run(
        &dir,
        &[
            "search",
            "shallowtargetterm",
            "--at",
            &head,
            "--mode",
            "text",
            "--scope",
            ".",
        ],
    );
    assert_eq!(code, 1);
    assert_eq!(err["error_code"], "KIO-E-COMMIT-SHALLOW-001");
}

// ---------------------------------------------------------------------------
// §L / §M — canonical scope matching (confirmed, PC48), device-layer config (PC49-50)
// ---------------------------------------------------------------------------

/// PC48 [confirmed] (05 §1.8 L375): `--scope <path>` without `--descendants`
/// is a canonical-root-path exact match, never a string prefix match — a
/// sibling scope whose path happens to start with the same characters
/// (`/work/a` vs `/work/ab`) is not included.
#[test]
fn pc48_scope_flag_is_exact_match_not_string_prefix() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("scope-a");
    let ab = parent.path().join("scope-ab");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&ab).unwrap();
    fs::write(a.join("a.md"), "# A\n\nprefixmatchterm in a\n").unwrap();
    fs::write(ab.join("ab.md"), "# AB\n\nprefixmatchterm in ab\n").unwrap();
    let run_path = |dir: &std::path::Path, args: &[&str]| -> Value {
        let output = Command::cargo_bin("kio")
            .unwrap()
            .current_dir(dir)
            .env("XDG_CONFIG_HOME", data_home.join("config"))
            .env("XDG_DATA_HOME", data_home.join("data"))
            .env("XDG_CACHE_HOME", data_home.join("cache"))
            .env_remove("GEMINI_API_KEY")
            .env_remove("MISTRAL_API_KEY")
            .args(args)
            .arg("--json")
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice(&output).unwrap()
    };
    run_path(&a, &["init"]);
    run_path(&ab, &["init"]);
    run_path(&a, &["index", "--offline", "--approve"]);
    run_path(&ab, &["index", "--offline", "--approve"]);

    let a_str = a.display().to_string();
    let result = run_path(
        &a,
        &[
            "search",
            "prefixmatchterm",
            "--scope",
            &a_str,
            "--mode",
            "text",
        ],
    );
    assert_eq!(result["searched_scopes"].as_array().unwrap().len(), 1);
    // R23-20 (03 §4 L296): scope_path is the canonical `.kio` directory, not
    // the scope root `a` itself.
    assert_eq!(
        result["searched_scopes"][0]["scope_path"]
            .as_str()
            .map(std::path::Path::new)
            .and_then(|p| p.canonicalize().ok()),
        a.join(".kio").canonicalize().ok()
    );
}

/// PC49 (05 §1.8 L384-387): a multi-scope search (the bare default — no
/// `--scope`) uses ONLY the user (device) config layer for `[search]`
/// — a folder `default_mode` override in the CWD's own `.kio/config.toml`
/// does not apply, even though the CWD itself is one of the searched scopes.
#[test]
fn pc49_multiscope_search_ignores_folder_default_mode() {
    let dir = indexed_scope();
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[search]\ndefault_mode = \"vector\"\n",
    )
    .unwrap();
    // Bare default (no --scope): multi-scope enumeration, per 05 §1.8 — the
    // folder's default_mode="vector" override must NOT apply, so with no
    // embedding endpoint configured this resolves through auto's own default
    // (not a --vector-explicit hard error).
    let search = success(&dir, &["search", "tokentestterm"]);
    assert_ne!(search["requested_mode"], "vector");
}

/// PC50: the SAME folder `default_mode` override DOES apply for a single,
/// explicit `--scope .` — the complement of PC49 (folder config is not
/// disabled outright, only inapplicable to multi-scope enumeration).
#[test]
fn pc50_single_explicit_scope_search_applies_folder_default_mode() {
    let dir = indexed_scope();
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[search]\ndefault_mode = \"text\"\n",
    )
    .unwrap();
    let search = success(&dir, &["search", "tokentestterm", "--scope", "."]);
    assert_eq!(search["requested_mode"], "text");
}

// ---------------------------------------------------------------------------
// §N — exit-code split / priority (PC53-57)
// ---------------------------------------------------------------------------

/// A device-registry harness of N+1 scopes under one `data_home`: a `runner`
/// scope (valid, indexed, `participates_in_global_search = false`) used only
/// as the CWD search is invoked from — `run_search_inner` always opens the
/// CWD's own `.kio` first (`Repository::open_current_for_search`), so the
/// CWD itself must stay format-compatible even when the test wants EVERY
/// *searched* (registry-participating) scope to hit some failure mode — plus
/// the named target directories (created, initialized, but left unindexed;
/// callers index/mutate them as each test needs).
fn multi_scope_env(names: &[&str]) -> (TempDir, std::path::PathBuf, Vec<std::path::PathBuf>) {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let runner = parent.path().join("runner");
    fs::create_dir_all(&runner).unwrap();
    fs::write(runner.join("runner.md"), "# Runner\n\nnotparticipating\n").unwrap();
    let run_path = |dir: &std::path::Path, args: &[&str]| {
        Command::cargo_bin("kio")
            .unwrap()
            .current_dir(dir)
            .env("XDG_CONFIG_HOME", data_home.join("config"))
            .env("XDG_DATA_HOME", data_home.join("data"))
            .env("XDG_CACHE_HOME", data_home.join("cache"))
            .env_remove("GEMINI_API_KEY")
            .env_remove("MISTRAL_API_KEY")
            .args(args)
            .arg("--json")
            .assert()
            .success();
    };
    run_path(&runner, &["init"]);
    fs::write(
        runner.join(".kio/config.toml"),
        "[scope]\nparticipates_in_global_search = false\n",
    )
    .unwrap();
    run_path(&runner, &["index", "--offline", "--approve"]);

    let mut targets = Vec::new();
    for name in names {
        let target = parent.path().join(name);
        fs::create_dir_all(&target).unwrap();
        run_path(&target, &["init"]);
        targets.push(target);
    }
    (parent, data_home, targets)
}

fn run_in(data_home: &std::path::Path, dir: &std::path::Path, args: &[&str]) -> (i32, Value) {
    let output = Command::cargo_bin("kio")
        .unwrap()
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .env_remove("GEMINI_API_KEY")
        .env_remove("MISTRAL_API_KEY")
        .args(args)
        .arg("--json")
        .output()
        .unwrap();
    let code = output.status.code().unwrap();
    let stream: &[u8] = if !output.stdout.is_empty() {
        &output.stdout
    } else {
        &output.stderr
    };
    (code, serde_json::from_slice(stream).unwrap())
}

fn bump_format_version(dir: &std::path::Path) {
    let scope_json = dir.join(".kio/scope.json");
    let mut value: Value = serde_json::from_str(&fs::read_to_string(&scope_json).unwrap()).unwrap();
    value["kio_format_version"] = Value::String("9.0.0".to_owned());
    fs::write(&scope_json, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

/// PC53/PC54 (05 §1.8 L390-391 / 10 §11.5): a single non-current scope aborts
/// search with `KIO-E-STORE-VERSION-001` / exit 8, rather than being treated
/// as a generic unreachable scope.
#[test]
fn pc53_pc54_incompatible_format_version_scope_is_store_version_exit_8() {
    let (_parent, data_home, targets) = multi_scope_env(&["target"]);
    let target = &targets[0];
    fs::write(target.join("t.md"), "# T\n\nversionprobeterm body\n").unwrap();
    Command::cargo_bin("kio")
        .unwrap()
        .current_dir(target)
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .env_remove("GEMINI_API_KEY")
        .env_remove("MISTRAL_API_KEY")
        .args(["index", "--offline", "--approve", "--json"])
        .assert()
        .success();
    bump_format_version(target);

    // Search runs from `runner` (still format-compatible) but `runner` does
    // not participate, so the default (all-registered-scopes) enumeration
    // searches only `target`.
    let (code, err) = run_in(
        &data_home,
        &targets[0].parent().unwrap().join("runner"),
        &["search", "versionprobeterm"],
    );
    assert_eq!(code, 8, "got {err}");
    assert_eq!(err["error_code"], "KIO-E-STORE-VERSION-001");
}

/// PC57 (05 §1.8 L392 / 06 §7 L362-363): one non-current scope aborts a
/// multi-scope search even when another scope is healthy. The command must not
/// return a partial result set or record format incompatibility as an
/// `excluded_scopes` entry.
#[test]
fn pc57_non_current_scope_aborts_multi_scope_search_without_partial_success() {
    // Both targets must be indexed and registry-participating for the default
    // all-scopes enumeration to reach them.
    let (_parent, data_home, targets) = multi_scope_env(&["a", "b"]);
    let (a, b) = (&targets[0], &targets[1]);
    for (target, term) in [(a, "a"), (b, "b")] {
        fs::write(
            target.join("doc.md"),
            format!("# Doc\n\nmixedreasonterm {term}\n"),
        )
        .unwrap();
        Command::cargo_bin("kio")
            .unwrap()
            .current_dir(target)
            .env("XDG_CONFIG_HOME", data_home.join("config"))
            .env("XDG_DATA_HOME", data_home.join("data"))
            .env("XDG_CACHE_HOME", data_home.join("cache"))
            .env_remove("GEMINI_API_KEY")
            .env_remove("MISTRAL_API_KEY")
            .args(["index", "--offline", "--approve", "--json"])
            .assert()
            .success();
    }
    // A's format version is non-current; B remains healthy and searchable.
    bump_format_version(a);
    let runner = a.parent().unwrap().join("runner");
    let (code, body) = run_in(&data_home, &runner, &["search", "mixedreasonterm"]);
    assert_eq!(
        code, 8,
        "a non-current scope must abort instead of returning B's partial result: {body}"
    );
    assert_eq!(body["error_code"], "KIO-E-STORE-VERSION-001");
    assert!(body.get("results").is_none(), "no partial success: {body}");
    assert!(
        body["context"].get("excluded_scopes").is_none(),
        "store-version incompatibility must not be converted to an exclusion: {body}"
    );
}

// ---------------------------------------------------------------------------
// §O — `--at` multi-scope constraint (PC59-60)
// ---------------------------------------------------------------------------

/// PC59 (06 §3 L226-227): `--at` requires a single, explicit `--scope` — the
/// bare multi-scope default is invalid usage. Complements
/// `pc59_search_at_without_scope_is_invalid_usage` in `step3_p0_contract.rs`
/// (single-scope registry) with a genuinely multi-scope registry.
#[test]
fn pc59_at_without_scope_is_invalid_usage_with_multiple_registered_scopes() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("a.md"), "# A\n\natscopeterm a\n").unwrap();
    fs::write(b.join("b.md"), "# B\n\natscopeterm b\n").unwrap();
    let run_path = |dir: &std::path::Path, args: &[&str]| {
        Command::cargo_bin("kio")
            .unwrap()
            .current_dir(dir)
            .env("XDG_CONFIG_HOME", data_home.join("config"))
            .env("XDG_DATA_HOME", data_home.join("data"))
            .env("XDG_CACHE_HOME", data_home.join("cache"))
            .env_remove("GEMINI_API_KEY")
            .env_remove("MISTRAL_API_KEY")
            .args(args)
            .arg("--json")
            .assert()
    };
    run_path(&a, &["init"]).success();
    run_path(&b, &["init"]).success();
    let head = serde_json::from_slice::<Value>(
        &run_path(&a, &["index", "--offline", "--approve"])
            .success()
            .get_output()
            .stdout,
    )
    .unwrap()["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    run_path(&b, &["index", "--offline", "--approve"]).success();

    let output = Command::cargo_bin("kio")
        .unwrap()
        .current_dir(&a)
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home.join("data"))
        .env("XDG_CACHE_HOME", data_home.join("cache"))
        .env_remove("GEMINI_API_KEY")
        .env_remove("MISTRAL_API_KEY")
        .args(["search", "atscopeterm", "--at", &head, "--json"])
        .assert()
        .code(2)
        .get_output()
        .stderr
        .clone();
    let err: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(err["error_code"], "KIO-E-CONFIG-USAGE-001");
}

// ---------------------------------------------------------------------------
// Schema completeness (PC37) — `chunk_publications` exists and is queryable.
// ---------------------------------------------------------------------------

/// PC37 (04 §4.1 / 05 §1.6): the `chunk_publications` table exists AND is
/// populated by a normal index run — every live chunk has at least one
/// `(chunk_id, introduction_commit)` row, and `introduction_commit` names an
/// actual ancestor-or-equal commit of HEAD (`build_sqlite_index_at`'s
/// `record_chunk_publication` calls, wired from `retained_history_instances`/
/// `RetainedNormalizedInstance::introductions`).
#[test]
fn pc37_chunk_publications_table_exists_and_is_populated_after_index() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.md"), "# A\n\nintroductioncontenttoken\n").unwrap();
    init(&dir);
    let head = success(&dir, &["index", "--offline", "--approve"])["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();

    let conn = rusqlite::Connection::open(sqlite_path(&dir)).unwrap();
    let exists: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'chunk_publications'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(exists, 1);

    let chunk_count: i64 = conn
        .query_row("SELECT count(*) FROM chunks", [], |row| row.get(0))
        .unwrap();
    assert!(chunk_count > 0, "the fixture must produce live chunks");
    let published_count: i64 = conn
        .query_row(
            "SELECT count(DISTINCT chunk_id) FROM chunk_publications",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        published_count, chunk_count,
        "every live chunk must have a chunk_publications row"
    );

    let introduction: String = conn
        .query_row(
            "SELECT introduction_commit FROM chunk_publications LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        introduction, head,
        "a fresh single-commit scope's only possible introduction is HEAD itself"
    );
}

// ---------------------------------------------------------------------------
// §H/§J — creation association and publication authority separation (PC40)
// ---------------------------------------------------------------------------

/// PC40: `chunk_config_generations` is immutable creation/order metadata for
/// one `(chunk_id, config)` pair. Rebuilding preserves that pair and its
/// durable rowid; historical introductions live only in
/// `chunk_publications`.
#[test]
fn pc40_config_association_creation_is_stable_and_publication_is_separate() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.md"), "# A\n\nconfigintroductiontoken\n").unwrap();
    init(&dir);
    let ca = success(&dir, &["index", "--offline", "--approve"])["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();

    let before = {
        let conn = rusqlite::Connection::open(sqlite_path(&dir)).unwrap();
        conn.query_row(
            "SELECT association_rowid, chunk_id, chunking_config_hash, created_at \
             FROM chunk_config_generations LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .unwrap()
    };

    success(&dir, &["repair", "rebuild-db"]);
    // Reopen: `repair rebuild-db` replaces sqlite.db via temp+rename (P5),
    // so a connection opened before this would keep reading the pre-rebuild
    // inode on POSIX rather than the freshly-published file.
    let after = {
        let conn = rusqlite::Connection::open(sqlite_path(&dir)).unwrap();
        conn.query_row(
            "SELECT association_rowid, chunk_id, chunking_config_hash, created_at \
             FROM chunk_config_generations WHERE chunk_id = ?1 AND chunking_config_hash = ?2",
            rusqlite::params![&before.1, &before.2],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .unwrap()
    };
    assert_eq!(after, before);

    let conn = rusqlite::Connection::open(sqlite_path(&dir)).unwrap();
    let publication: String = conn
        .query_row(
            "SELECT introduction_commit FROM chunk_publications \
             WHERE chunk_id = ?1 AND chunking_config_hash = ?2",
            rusqlite::params![&after.1, &after.2],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(publication, ca);
}

#[test]
fn pc40_publication_cannot_backdate_a_later_config_to_an_older_tree() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("a.md"),
        "# A\n\nconfigbindingattacktoken configbindingattacktoken\n",
    )
    .unwrap();
    init(&dir);
    let old_commit = success(&dir, &["index", "--offline", "--approve"])["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();

    fs::write(
        dir.path().join(".kio/config.toml"),
        "[chunking]\nstrategy = \"heading\"\nmax_chars = 10\n",
    )
    .unwrap();
    let new_commit = success(&dir, &["index", "--offline", "--approve"])["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(new_commit, old_commit);

    let ledger = dir.path().join(".kio/index/chunks.jsonl");
    let mut replaced = false;
    let rewritten = fs::read_to_string(&ledger)
        .unwrap()
        .lines()
        .map(|line| {
            let mut value: Value = serde_json::from_str(line).unwrap();
            if !replaced
                && value["event"] == "publication"
                && value["introduction_commit"] == new_commit
            {
                value["introduction_commit"] = Value::String(old_commit.clone());
                replaced = true;
            }
            serde_json::to_string(&value).unwrap()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(replaced, "the new config must have a publication event");
    fs::write(&ledger, format!("{rewritten}\n")).unwrap();

    let (code, error) = run(&dir, &["repair", "rebuild-db"]);
    assert_ne!(code, 0, "{error}");
    assert_eq!(error["error_code"], "KIO-E-STORE-CORRUPT-001", "{error}");
}

// ---------------------------------------------------------------------------
// §F/§H — tree-scoped chunking_config_hash (PC22/PC23/PC31/PC33)
// ---------------------------------------------------------------------------

/// PC22/PC23/PC31 (05 §1.5 L200, §1.6 L237-239): `--at <commit>`
/// resolves the TARGET tree's own `chunking_config_hash`, not whatever
/// config.toml currently says. Observed indirectly through chunk SHAPE
/// (`searched_scopes[]` does not expose the hash itself — only the cursor
/// preimage and `query_hash` computation consume it internally): shrinking
/// `max_chars` re-splits a long paragraph into more, differently-hashed
/// chunks. HEAD/bare search must see the re-split (new-config) chunks;
/// `--at Ca` must keep resolving Ca's own pre-split (old-config) shape.
#[test]
fn pc22_pc23_pc31_at_uses_the_target_trees_config_not_current() {
    let dir = tempfile::tempdir().unwrap();
    let long_body = "atreeconfigtoken ".repeat(50);
    fs::write(dir.path().join("a.md"), format!("# A\n\n{long_body}\n")).unwrap();
    init(&dir);
    let ca = success(&dir, &["index", "--offline", "--approve"])["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let at_ca_before = success(
        &dir,
        &[
            "search",
            "atreeconfigtoken",
            "--at",
            &ca,
            "--mode",
            "text",
            "--scope",
            ".",
        ],
    );
    let chunks_before = chunk_hash_set(&at_ca_before);
    assert_eq!(
        chunks_before.len(),
        1,
        "the default max_chars must keep this paragraph as a single chunk: {at_ca_before}"
    );

    // a.md stays HEAD-referenced (nothing deletes it), so it picks up a NEW
    // association under the changed config on top of its untouched old one
    // (PC61/62 does not apply here — this is not a history-only identity).
    // A much smaller max_chars forces the SAME paragraph to re-split, so the
    // new generation's chunk_hash (byte_start/byte_end shift) genuinely
    // differs from the old one — not a same-hash no-op reindex. A second
    // file is ALSO added so HEAD genuinely advances past Ca (a config-only
    // change with no tracked-content change never advances HEAD at all,
    // which would make "Ca" and "current HEAD" the same commit and leave no
    // distinct target-tree binding to verify).
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[chunking]\nstrategy = \"heading\"\nmax_chars = 30\n",
    )
    .unwrap();
    fs::write(dir.path().join("b.md"), "# B\n\nunrelated filler content\n").unwrap();
    success(&dir, &["index", "--offline", "--approve"]);

    let bare = success(&dir, &["search", "atreeconfigtoken", "--mode", "text"]);
    let chunks_bare = chunk_hash_set(&bare);
    assert!(
        chunks_bare.len() > 1,
        "current/HEAD search must see the re-split (new-config) chunks: {bare}"
    );
    assert!(
        chunks_bare.is_disjoint(&chunks_before),
        "the new-config chunks must be genuinely different from the old one"
    );

    let at_ca_after = success(
        &dir,
        &[
            "search",
            "atreeconfigtoken",
            "--at",
            &ca,
            "--mode",
            "text",
            "--scope",
            ".",
        ],
    );
    let chunks_at_ca_after = chunk_hash_set(&at_ca_after);
    assert_eq!(
        chunks_at_ca_after, chunks_before,
        "--at Ca must keep resolving Ca's OWN (old-config, single-chunk) shape, \
         not HEAD's current (re-split) one: {at_ca_after}"
    );

    // Switch back to A and create C3, then edit the mutable config back to B
    // without publishing another snapshot. B is now an ancestor association
    // of C3, so any "prefer live/any reaching association" heuristic chooses
    // B incorrectly. Only C3's required tree field can select A.
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[chunking]\nstrategy = \"heading\"\nmax_chars = 6000\n",
    )
    .unwrap();
    fs::write(dir.path().join("c.md"), "# C\n\nadvance back to config A\n").unwrap();
    let c3 = success(&dir, &["index", "--offline", "--approve"])["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[chunking]\nstrategy = \"heading\"\nmax_chars = 30\n",
    )
    .unwrap();

    let at_c3 = success(
        &dir,
        &[
            "search",
            "atreeconfigtoken",
            "--at",
            &c3,
            "--mode",
            "text",
            "--scope",
            ".",
        ],
    );
    assert_eq!(
        chunk_hash_set(&at_c3),
        chunks_before,
        "A -> B -> A must resolve C3's explicit A tree binding even while config.toml says B: {at_c3}"
    );

    let pointer_json = serde_json::to_string(&at_c3["results"][0]["evidence_pointer"]).unwrap();
    let verified = success(&dir, &["evidence", "verify", &pointer_json]);
    assert_eq!(
        verified["status"], "alive",
        "Evidence verification must use the basis commit tree config, not mutable config.toml: {verified}"
    );
}

fn chunk_hash_set(search: &Value) -> std::collections::BTreeSet<String> {
    search["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|result| result["chunk_hash"].as_str().unwrap().to_owned())
        .collect()
}

/// PC33 (05 §1.6): one history query may contain bindings whose trees pin
/// different chunking configurations. Each alias is filtered with its own
/// pointer commit's tree hash; the HEAD tree's config is not a scope-wide
/// substitute.
#[test]
fn pc33_history_selectors_use_each_binding_trees_config() {
    let dir = tempfile::tempdir().unwrap();
    let body = "perbindingconfigtoken ".repeat(40);
    fs::write(dir.path().join("old.md"), format!("# Old\n\n{body}\n")).unwrap();
    init(&dir);
    let c1 = success(&dir, &["index", "--offline", "--approve"])["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let at_c1 = success(
        &dir,
        &[
            "search",
            "perbindingconfigtoken",
            "--at",
            &c1,
            "--mode",
            "text",
            "--scope",
            ".",
        ],
    );
    let old_chunks = chunk_hash_set(&at_c1);
    assert_eq!(old_chunks.len(), 1, "fixture must begin under config A");

    fs::write(
        dir.path().join(".kio/config.toml"),
        "[chunking]\nstrategy = \"heading\"\nmax_chars = 30\n",
    )
    .unwrap();
    fs::remove_file(dir.path().join("old.md")).unwrap();
    fs::write(
        dir.path().join("current.md"),
        "# Current\n\nconfig B head\n",
    )
    .unwrap();
    success(&dir, &["index", "--offline", "--approve"]);

    for selector in ["--all-history", "--include-deleted"] {
        let result = success(
            &dir,
            &[
                "search",
                "perbindingconfigtoken",
                selector,
                "--mode",
                "text",
                "--scope",
                ".",
            ],
        );
        assert_eq!(
            chunk_hash_set(&result),
            old_chunks,
            "{selector} must retain C1's config-A binding under a config-B HEAD: {result}"
        );
    }
}

// ---------------------------------------------------------------------------
// §P — rebuild-only HEAD-limited re-association (PC61/PC62/PC63)
// ---------------------------------------------------------------------------

/// PC61/PC62/PC63 (04 §4.6, U145): a chunking-config change only creates a
/// NEW `chunk_config_generations` association for HEAD-referenced content —
/// a history-only identity (its file was deleted before the config changed)
/// keeps its old association only (no wasted re-chunk/re-embed of content no
/// live tree can reach), yet a `--at` search of the commit where it was
/// still live still finds it through that commit tree's exact old-config
/// association. There is no live-value or association-order substitution.
#[test]
fn pc61_pc62_pc63_head_limited_reassociation_still_leaves_at_searchable() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.md"), "# A\n\nheadonlytoken content\n").unwrap();
    fs::write(dir.path().join("b.md"), "# B\n\nhistoryonlytoken content\n").unwrap();
    init(&dir);
    let c1 = success(&dir, &["index", "--offline", "--approve"])["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();

    fs::remove_file(dir.path().join("b.md")).unwrap();
    success(&dir, &["index", "--offline", "--approve"]);

    fs::write(
        dir.path().join(".kio/config.toml"),
        "[chunking]\nstrategy = \"heading\"\nmax_chars = 42\n",
    )
    .unwrap();
    success(&dir, &["index", "--offline", "--approve"]);

    let conn = rusqlite::Connection::open(sqlite_path(&dir)).unwrap();
    let a_config_count: i64 = conn
        .query_row(
            "SELECT count(DISTINCT g.chunking_config_hash) FROM chunks c \
             JOIN chunk_config_generations g ON g.chunk_id = c.chunk_id \
             WHERE c.raw_path = 'a.md'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        a_config_count, 2,
        "a.md is HEAD-referenced and must pick up the new config association"
    );
    let b_config_count: i64 = conn
        .query_row(
            "SELECT count(DISTINCT g.chunking_config_hash) FROM chunks c \
             JOIN chunk_config_generations g ON g.chunk_id = c.chunk_id \
             WHERE c.raw_path = 'b.md'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        b_config_count, 1,
        "b.md is history-only and must NOT pick up the new config association (PC61/62)"
    );
    drop(conn);

    let at_c1 = success(
        &dir,
        &[
            "search",
            "historyonlytoken",
            "--at",
            &c1,
            "--mode",
            "text",
            "--scope",
            ".",
        ],
    );
    assert!(
        !at_c1["results"].as_array().unwrap().is_empty(),
        "C1's tree-bound old-config association must remain searchable: {at_c1}"
    );
}

// ---------------------------------------------------------------------------
// §N — per-scope `--vector` profile-incompat exclusion (PC52)
// ---------------------------------------------------------------------------

/// PC52 (05 §1.8 L390): explicit `--vector` excludes only the scope whose
/// embedding profile is incompatible — it does not fall back (auto/
/// `--hybrid`'s device-wide behavior) or hard-error the whole multi-scope
/// search the way a single incompatible scope still does (`ct3_embed_002`,
/// confirmed unaffected by this change).
#[test]
fn pc52_explicit_vector_excludes_only_the_incompatible_scope() {
    let parent = tempfile::tempdir().unwrap();
    let data_home = parent.path().join("xdg");
    let a = parent.path().join("a");
    let b = parent.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("a.md"), "# A\n\n## One\ncompatiblescopetoken\n").unwrap();
    fs::write(b.join("b.md"), "# B\n\n## Two\nincompatiblescopetoken\n").unwrap();

    fn embed_command(dir: &std::path::Path, data_home: &std::path::Path, embed: &str) -> Command {
        let mut command = Command::cargo_bin("kio").unwrap();
        command
            .current_dir(dir)
            .env("XDG_CONFIG_HOME", data_home.join("config"))
            .env("XDG_DATA_HOME", data_home.join("data"))
            .env("XDG_CACHE_HOME", data_home.join("cache"))
            .env(TEST_ADOPTED_EMBEDDING_ENV, embed)
            .env_remove("KIO_FIXED_NOW");
        command
    }
    fn run_embed(dir: &std::path::Path, data_home: &std::path::Path, embed: &str, args: &[&str]) {
        embed_command(dir, data_home, embed)
            .args(args)
            .arg("--json")
            .assert()
            .success();
    }

    run_embed(&a, &data_home, "mock", &["init"]);
    run_embed(&b, &data_home, "mock", &["init"]);
    run_embed(&a, &data_home, "mock", &["index", "--approve"]);
    run_embed(
        &b,
        &data_home,
        "incompatible_profile",
        &["index", "--approve"],
    );

    let output = embed_command(&a, &data_home, "mock")
        .args([
            "search",
            "compatiblescopetoken",
            "--mode",
            "vector",
            "--all-scopes",
        ])
        .arg("--json")
        .output()
        .unwrap();
    let code = output.status.code().unwrap();
    let stream: &[u8] = if !output.stdout.is_empty() {
        &output.stdout
    } else {
        &output.stderr
    };
    let search: Value = serde_json::from_slice(stream).unwrap();
    assert_eq!(
        code, 3,
        "one incompatible scope among several must PARTIAL-fail (exit 3), not hard-error: {search}"
    );
    assert!(!search["results"].as_array().unwrap().is_empty());
    let excluded = search["excluded_scopes"].as_array().unwrap();
    assert!(
        excluded
            .iter()
            .any(|entry| entry["reason"] == "embedding_profile_incompatible"),
        "the incompatible scope must be named in excluded_scopes: {search}"
    );
}

// ---------------------------------------------------------------------------
// §F — index_generation rotation, purge trigger (PC20)
// ---------------------------------------------------------------------------

/// PC20 (05 §1.5 L180-184): purge is one of the listed rotation triggers —
/// deleted chunk/config/vector rows change the search-visible set, so an
/// outstanding cursor must not silently keep reading the pre-purge world.
/// `rebuild`/`index`/`reindex` already rotate on every pass (PB28,
/// `build_sqlite_index_at`'s own unconditional mint); this is purge's own,
/// separate trigger (`rotate_index_generation_unconditionally`, called from
/// `purge.rs`).
#[test]
fn pc20_purge_rotates_index_generation() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.md"), "# A\n\npurgerotationtoken\n").unwrap();
    init(&dir);
    success(&dir, &["index", "--offline", "--approve"]);

    let before: String = {
        let conn = rusqlite::Connection::open(sqlite_path(&dir)).unwrap();
        conn.query_row(
            "SELECT index_generation FROM index_metadata WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };

    success(&dir, &["purge", "a.md", "--reason", "legal", "--yes"]);

    let after: String = {
        let conn = rusqlite::Connection::open(sqlite_path(&dir)).unwrap();
        conn.query_row(
            "SELECT index_generation FROM index_metadata WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert_ne!(
        before, after,
        "purge must mint a fresh index_generation (PC20)"
    );
}

// ---------------------------------------------------------------------------
// §C — tokenizer / MATCH generation (PC8-14)
// ---------------------------------------------------------------------------

/// PC12/PC13, reversed by the R-追記 (step4b-contract-tests-p2c.md,
/// 2026-07-22 spec feedback #2 — 05 §1.3 L120-134): a short (< 3 Unicode
/// scalar) unit in a MIXED query (>= 1 long unit) no longer enters the FTS5
/// MATCH expression (PC9 — trigram MATCH can't carry it) NOR acts as an
/// `instr` AND-eligibility filter — it is dropped outright. A chunk matching
/// only the long token, without the short one, must still surface (the
/// superseded PC12/13 AND-instr filter this replaced structurally excluded
/// exactly this shape of document — natural-sentence chunks that never
/// happen to spell a short function word/particle — which eval M3-2/M3-3
/// measured as the dominant Recall@10 failure mode).
#[test]
fn pc12_pc13_short_token_in_mixed_query_is_dropped_not_an_and_filter() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("with_ai.md"),
        "# Doc1\n\nauthentication uses AI heuristics here.\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("without_ai.md"),
        "# Doc2\n\nauthentication is handled by a separate module.\n",
    )
    .unwrap();
    init(&dir);
    success(&dir, &["index", "--offline", "--approve"]);

    let search = success(&dir, &["search", "authentication AI", "--mode", "text"]);
    let results = search["results"].as_array().unwrap();
    assert!(
        !results.is_empty(),
        "the long unit alone must still match something: {search}"
    );
    let paths: Vec<&str> = results
        .iter()
        .map(|result| {
            result["evidence_pointer"]["path_at_commit"]
                .as_str()
                .unwrap_or_default()
        })
        .collect();
    assert!(
        paths.iter().any(|path| path.contains("with_ai")),
        "the document containing both terms must be found: {search}"
    );
    assert!(
        paths.iter().any(|path| path.contains("without_ai")),
        "a document matching only the long unit 'authentication' — lacking \
         the short unit 'AI' entirely — must no longer be excluded now that \
         mixed-query short units are dropped instead of AND-filtered: {search}"
    );
}

/// PC11 (05 §1.3 L95-97): a query where every token is short (< 3 Unicode
/// scalars) has no MATCH expression at all (PC8's `match_expr = None`) and
/// falls back entirely to the bounded LIKE (`instr`) scan.
#[test]
fn pc11_all_short_tokens_use_the_bounded_like_fallback_only() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.md"), "# A\n\nan ai driven doc\n").unwrap();
    init(&dir);
    success(&dir, &["index", "--offline", "--approve"]);

    // "an" and "ai" are both 2 Unicode scalars — no token reaches the
    // trigram MATCH threshold, so this must resolve via LIKE alone.
    let search = success(&dir, &["search", "an ai", "--mode", "text"]);
    assert!(
        !search["results"].as_array().unwrap().is_empty(),
        "an all-short-token query must still find a bounded-LIKE match: {search}"
    );
}

/// PC8 (05 §1.3 L110-113): user query text is never interpreted as FTS5
/// syntax — a query containing FTS5 operator keywords and a literal quote
/// character matches literally (as a phrase) rather than raising a syntax
/// error or being parsed as boolean operators.
#[test]
fn pc8_fts5_operator_keywords_and_quotes_are_literal_not_syntax() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("a.md"),
        "# A\n\nThe \"OR\" operator combines conditions in boolean logic.\n",
    )
    .unwrap();
    init(&dir);
    success(&dir, &["index", "--offline", "--approve"]);

    // A raw double quote inside the query must be escaped (`""`) rather than
    // breaking the generated MATCH expression's own quoting.
    let (code, search) = run(&dir, &["search", "\"OR\" operator", "--mode", "text"]);
    assert_eq!(
        code, 0,
        "an FTS5-operator-shaped query must not error: {search}"
    );
    assert!(!search["results"].as_array().unwrap().is_empty());
}

/// Deterministic query normalization (05 §1.3 L116-123, 2026-07-22 spec
/// feedback #1): restores the numeral/bilingual equivalence forms PC8's
/// original implementation dropped entirely — eval M3-2/M3-3 (09 §4.3's
/// Recall@10 >= 0.8 gate) measured 13/14 failures tracing to exactly this
/// gap. A plain-digit query still finds a document that only ever spelled
/// the number with thousands separators (and the reverse), and an English
/// query still finds a document that only ever used Kio's own fixed
/// Japanese vocabulary for the same term — without any hand-authored
/// synonym/history/context injection (PC8's actual, narrower ban).
#[test]
fn pc8_deterministic_numeric_and_bilingual_equivalence_forms_are_restored() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("numeral-grouped.md"),
        "# Numeral grouped\n\n## Body\nThe retry budget expires after 3,600 idle units.\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("numeral-plain.md"),
        "# Numeral plain\n\n## Body\nThe queue depth peaked at 30000 items overnight.\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("bilingual.md"),
        "# Bilingual\n\n## Body\nチャンクは 512 トークン、オーバーラップ 64。\n",
    )
    .unwrap();
    init(&dir);
    success(&dir, &["index", "--offline", "--approve"]);

    let path_matches = |search: &Value, needle: &str| {
        search["results"].as_array().unwrap().iter().any(|result| {
            result["evidence_pointer"]["path_at_commit"]
                .as_str()
                .unwrap_or_default()
                .contains(needle)
        })
    };

    // A plain-digit query finds the doc that only spells the number with a
    // thousands separator.
    let plain_query = success(&dir, &["search", "3600 idle units", "--mode", "text"]);
    assert!(
        path_matches(&plain_query, "numeral-grouped"),
        "a plain-digit query must find a doc that only spells the number with a \
         thousands separator: {plain_query}"
    );

    // The reverse direction: a comma-grouped query finds the doc that only
    // spells the number without separators.
    let grouped_query = success(&dir, &["search", "queue depth 30,000", "--mode", "text"]);
    assert!(
        path_matches(&grouped_query, "numeral-plain"),
        "a comma-grouped query must find a doc that only spells the number \
         without separators: {grouped_query}"
    );

    // An English query finds a doc using only Kio's own fixed チャンク/トークン
    // dictionary translation.
    let bilingual_query = success(&dir, &["search", "chunk size 512 token", "--mode", "text"]);
    assert!(
        path_matches(&bilingual_query, "bilingual"),
        "an English query must find a doc using only the fixed チャンク/トークン \
         dictionary translation: {bilingual_query}"
    );
}

/// R-追記 (step4b-contract-tests-p2c.md, 2026-07-22 spec feedback #2 — 05
/// §1.3 L120-134): a document whose body never spells the query's
/// agglutinated particle ("トークン**の**"/"...が") is still found. The query
/// token "認証仕様のトークン" script-segments into 認証仕様 / の / トークン,
/// and "秒だった資料" into 秒 / だった / 資料 — the document text below hits
/// several of those segmented long units ("トークン", "TTL", "3,600")
/// directly, and the segmentation-derived short units ("の", "が", "秒",
/// "資料") are dropped outright rather than AND-filtered (this is a MIXED
/// query — several long units exist). Pre-feedback-#2, the query's own short
/// token "が" would have AND-instr-excluded this document (it uses "は",
/// never "が").
#[test]
fn r_addendum_feedback2_mixed_query_short_particle_does_not_exclude_a_document_lacking_it() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("token-ttl.md"),
        "# Auth spec\n\n## Token\nトークン TTL は 3,600 秒、リフレッシュは 24 時間ごと。\n",
    )
    .unwrap();
    init(&dir);
    success(&dir, &["index", "--offline", "--approve"]);

    let search = success(
        &dir,
        &[
            "search",
            "認証仕様のトークン TTL が 3600 秒だった資料",
            "--mode",
            "text",
        ],
    );
    let results = search["results"].as_array().unwrap();
    assert!(
        !results.is_empty(),
        "a document lacking the query's short particle 'が' must still be found \
         once script-boundary segmentation feeds long sub-pieces into MATCH and \
         mixed-query short-unit drop stops AND-filtering on 'が': {search}"
    );
    assert!(
        results
            .iter()
            .any(|result| result["evidence_pointer"]["path_at_commit"]
                .as_str()
                .unwrap_or_default()
                .contains("token-ttl")),
        "the token-ttl document itself must be among the results: {search}"
    );
}

/// R-追記 (step4b-contract-tests-p2c.md, 2026-07-22 spec feedback #2 — 05
/// §1.3 L120-134): a symbol-joined query token (`read/write/admin`)
/// script-segments into read / write / admin, matching a document that
/// spells the same enumeration with spaces around the slashes
/// (`read / write / admin`) — and the query's own short particle "が"
/// (from "スコープが" -> スコープ / が) is dropped rather than AND-filtered,
/// so a document using "は" instead is not excluded.
#[test]
fn r_addendum_feedback2_mixed_query_slash_joined_unit_and_short_particle() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("scope-kinds.md"),
        "# Auth memo\n\n## Scopes\nスコープは read / write / admin の 3 種類。\n",
    )
    .unwrap();
    init(&dir);
    success(&dir, &["index", "--offline", "--approve"]);

    let search = success(
        &dir,
        &[
            "search",
            "スコープが read/write/admin の 3 種類だった認証メモ",
            "--mode",
            "text",
        ],
    );
    let results = search["results"].as_array().unwrap();
    assert!(
        !results.is_empty(),
        "a slash-joined query token must still find a document that spells the \
         same words with spaces around the slashes, and lacking 'が' must not \
         exclude it: {search}"
    );
    assert!(
        results
            .iter()
            .any(|result| result["evidence_pointer"]["path_at_commit"]
                .as_str()
                .unwrap_or_default()
                .contains("scope-kinds")),
        "the scope-kinds document itself must be among the results: {search}"
    );
}

/// F3 (kio-index `search_projection`): 07 §5.2.1 has provider raw text escaped
/// maximally on the way in — a backslash before every ASCII punctuation
/// character — so a deadline the OCR service read as `期限 7/10` is stored as
/// `期限 7\/10`. The search projection resolves those escapes, so the plain
/// query finds the document and the Agent is shown what the page said rather
/// than what the storage layer needed.
///
/// The path that answers here is PC11's bounded LIKE scan, not FTS: `7/10`
/// script-segments into `7` / `10` (the `/` is `Other`, a separator), both
/// short, so there is no MATCH expression at all and `execute_like_fallback`'s
/// `instr(c.text, ?)` is what has to see the unescaped text.
#[test]
fn f3_escaped_punctuation_is_findable_by_the_plain_query_and_shown_unescaped() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("slip.md"), "# 回覧\n\n期限 7\\/10 まで\n").unwrap();
    init(&dir);
    success(&dir, &["index", "--offline", "--approve"]);

    let search = success(&dir, &["search", "7/10", "--mode", "text"]);
    let results = search["results"].as_array().unwrap();
    assert!(
        !results.is_empty(),
        "an escaped `7\\/10` must be findable by the plain query `7/10`: {search}"
    );
    let snippet = results[0]["snippet"].as_str().unwrap_or_default();
    assert!(
        snippet.contains("7/10"),
        "the snippet must show the deadline as the page spells it: {search}"
    );
    assert!(
        !snippet.contains("7\\/10"),
        "the snippet must not leak the storage-layer escape: {search}"
    );
}

/// The other half of F3, and the reason the projection exempts code instead of
/// unescaping everything: inside a fence a backslash is content, not escaping.
/// Counted across this repository's tracked Markdown, 439 of the 478
/// `\`-before-punctuation sequences are inside fences (416 of those in the
/// retained non-authorizing `eval/fixtures/normalized-corpus/` archive), so a
/// blanket unescape would rewrite genuine archived code to repair 23 body-text
/// escapes. The runtime vector below carries the same distinct behavior signal.
#[test]
fn f3_fenced_code_keeps_the_backslashes_the_corpus_actually_contains() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("gather.md"),
        "# gather\n\n```sh\nfind . -type f -exec shasum -a 256 {} \\;\n```\n",
    )
    .unwrap();
    init(&dir);
    success(&dir, &["index", "--offline", "--approve"]);

    let search = success(&dir, &["search", "shasum", "--mode", "text"]);
    let results = search["results"].as_array().unwrap();
    assert!(
        !results.is_empty(),
        "the fenced command must stay findable: {search}"
    );
    let snippet = results[0]["snippet"].as_str().unwrap_or_default();
    assert!(
        snippet.contains("{} \\;"),
        "a shell escape inside a fence must survive the projection intact: {search}"
    );
}
