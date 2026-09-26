//! Contract tests for `tasks/step4b-contract-tests-p2b.md` (fsck expansion /
//! evidence pointer resolve-verify-retarget, P2-B). Test names embed the PB
//! number they lock down. §M/§L's display-field pieces (PB34-36) remain
//! outside this pass's scope (they need `path_at_commit`/`heading_path`
//! output fields `resolve_pointer_for_cli` does not produce yet). This file
//! does not fabricate coverage for those.

mod support;

use support::canonical_tempdir;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use assert_cmd::Command;
use base64::Engine;
use kio_core::cas::{ContentObjectKind, EmbeddingObject, ObjectKind, ObjectStore, hash_bytes};
use kio_core::dag::{CommitObject, CommitStats, CommitType, NormalizeRef, TreeEntry, build_tree};
use kio_core::gc::ShallowReceipt;
use kio_core::purge::{PurgeReason, PurgeState, TombstoneMode};
use kio_core::scope::Repository;
use kio_index::registry::{RegistryDb, RegistryEntry};
use kio_pipeline::markdownize::NormalizedUnitObject;
use rusqlite::{Connection, params};
use serde_json::Value;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Harness (mirrors crates/kio-cli/tests/step4_verify.rs / step3_p0_contract.rs).
// ---------------------------------------------------------------------------

fn kio(dir: &TempDir, args: &[&str]) -> Command {
    let mut command = Command::cargo_bin("kio").unwrap();
    command
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join(".test-config"))
        .env("XDG_DATA_HOME", dir.path().join(".test-data"))
        .env("XDG_CACHE_HOME", dir.path().join(".test-cache"))
        .env_remove("GEMINI_API_KEY")
        .env_remove("MISTRAL_API_KEY")
        .args(args);
    command
}

/// A fully private public-CLI process for diagnostics that must also bind
/// HOME, rather than relying on the broader contract harness's XDG-only
/// convenience setup.
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

fn private_success(dir: &TempDir, args: &[&str]) -> Value {
    let output = private_kio_process(dir, args)
        .arg("--json")
        .output()
        .unwrap();
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
/// stderr on failure.
fn run(dir: &TempDir, args: &[&str]) -> (i32, Value) {
    let output = kio(dir, args).arg("--json").output().unwrap();
    let code = output.status.code().unwrap();
    // A non-zero exit does NOT imply the JSON landed on stderr: several
    // commands (`repair verify-objects` findings, search partial failure,
    // this session's `evidence verify` status responses) succeed at the
    // `run()` level and request a non-zero process exit via a private
    // `__exit_code` marker while still printing their JSON body to STDOUT
    // (main.rs's `take_exit_override`) — only a genuine `Err(KioError)`
    // prints to stderr. Prefer whichever stream is non-empty.
    let stream: &[u8] = if !output.stdout.is_empty() {
        &output.stdout
    } else {
        &output.stderr
    };
    (code, serde_json::from_slice(stream).unwrap())
}

fn batch_file(dir: &TempDir, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = dir.path().join(name);
    fs::write(&path, bytes).unwrap();
    path
}

fn batch_output(dir: &TempDir, path: &Path, strict: bool) -> std::process::Output {
    let mut command = kio(
        dir,
        &["evidence", "verify", "--batch", path.to_str().unwrap()],
    );
    if strict {
        command.arg("--strict");
    }
    command.arg("--json").output().unwrap()
}

/// The batch row must be the single verify status object verbatim, including
/// statuses whose process exit is non-zero but whose JSON belongs on stdout.
fn assert_single_batch_parity(
    dir: &TempDir,
    pointer: &Value,
    strict: bool,
    expected_exit: i32,
) -> Value {
    let pointer_json = serde_json::to_string(pointer).unwrap();
    let mut single = kio(dir, &["evidence", "verify", &pointer_json]);
    if strict {
        single.arg("--strict");
    }
    let single = single.arg("--json").output().unwrap();
    assert_eq!(single.status.code(), Some(expected_exit), "{single:?}");
    let single_result: Value = serde_json::from_slice(&single.stdout).unwrap();

    let path = batch_file(
        dir,
        if strict {
            "single-parity-strict.jsonl"
        } else {
            "single-parity.jsonl"
        },
        format!("{pointer_json}\n").as_bytes(),
    );
    let batch = batch_output(dir, &path, strict);
    assert_eq!(batch.status.code(), Some(expected_exit), "{batch:?}");
    let batch_result: Value = serde_json::from_slice(&batch.stdout).unwrap();
    assert_eq!(batch_result["results"][0]["result"], single_result);
    batch_result["results"][0]["result"].clone()
}

fn registry_path(dir: &TempDir) -> std::path::PathBuf {
    dir.path().join(".test-data/kio/scope-registry.sqlite")
}

fn initialized_ledger_path(dir: &TempDir) -> PathBuf {
    let path = dir.path().join(".test-data/kio/cost-ledger.sqlite");
    if path.exists() {
        kio_pipeline::ledger::LedgerDb::open_existing(&path).unwrap();
    } else {
        kio_pipeline::ledger::LedgerDb::initialize(&path).unwrap();
    }
    path
}

/// A complete `Present` observation of one device-local cost-ledger leaf.
/// Every lifecycle artifact's identity matters: a read-only snapshot must not
/// create, replace, or repair any of them.
#[derive(Debug, PartialEq, Eq)]
struct PresentLedgerLeaf {
    bytes: Vec<u8>,
    sha256: String,
    size: u64,
    #[cfg(unix)]
    mode: u32,
    #[cfg(unix)]
    nlink: u64,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}

/// A raw source-leaf observation used only after this test deliberately makes
/// a leaf unsafe. Unlike normal observations it records, rather than rejects,
/// a hard-link count greater than one.
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
struct UnsafeLedgerLeafObservation {
    bytes: Vec<u8>,
    sha256: String,
    size: u64,
    mode: u32,
    nlink: u64,
    dev: u64,
    ino: u64,
}

/// The source ledger's SQLite leaves plus authority, checkpoint,
/// initialization-pending, and lifecycle-lock records. `Absent` is evidence
/// too: a text or cursor read must not turn it into an artifact merely by
/// observing budget status.
#[derive(Debug, PartialEq, Eq)]
enum LedgerLeafObservation {
    Absent,
    Present(PresentLedgerLeaf),
}

fn observe_ledger_leaf(path: &Path) -> LedgerLeafObservation {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return LedgerLeafObservation::Absent;
        }
        Err(error) => panic!("cannot observe ledger leaf {}: {error}", path.display()),
    };
    assert!(metadata.file_type().is_file(), "{}", path.display());
    #[cfg(unix)]
    assert_eq!(
        {
            use std::os::unix::fs::MetadataExt;
            metadata.nlink()
        },
        1,
        "ledger leaf must not be hard-linked: {}",
        path.display()
    );
    let bytes = fs::read(path).unwrap();
    LedgerLeafObservation::Present(PresentLedgerLeaf {
        sha256: hash_bytes(&bytes),
        size: metadata.len(),
        bytes,
        #[cfg(unix)]
        mode: {
            use std::os::unix::fs::MetadataExt;
            metadata.mode() & 0o7777
        },
        #[cfg(unix)]
        nlink: {
            use std::os::unix::fs::MetadataExt;
            metadata.nlink()
        },
        #[cfg(unix)]
        dev: {
            use std::os::unix::fs::MetadataExt;
            metadata.dev()
        },
        #[cfg(unix)]
        ino: {
            use std::os::unix::fs::MetadataExt;
            metadata.ino()
        },
    })
}

#[cfg(unix)]
fn observe_unsafe_ledger_leaf(path: &Path) -> UnsafeLedgerLeafObservation {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::symlink_metadata(path).unwrap();
    assert!(metadata.file_type().is_file(), "{}", path.display());
    let bytes = fs::read(path).unwrap();
    UnsafeLedgerLeafObservation {
        sha256: hash_bytes(&bytes),
        size: metadata.len(),
        mode: metadata.mode() & 0o7777,
        nlink: metadata.nlink(),
        dev: metadata.dev(),
        ino: metadata.ino(),
        bytes,
    }
}

fn ledger_leaf_paths(dir: &TempDir) -> [PathBuf; 7] {
    let main = dir.path().join(".test-data/kio/cost-ledger.sqlite");
    [
        main.clone(),
        PathBuf::from(format!("{}-wal", main.display())),
        PathBuf::from(format!("{}-shm", main.display())),
        PathBuf::from(format!("{}.authority.json", main.display())),
        PathBuf::from(format!("{}.checkpoint.json", main.display())),
        PathBuf::from(format!("{}.init.pending", main.display())),
        main.parent().unwrap().join("ledger.lifecycle.lock"),
    ]
}

fn observe_ledger_leaves(dir: &TempDir) -> [LedgerLeafObservation; 7] {
    ledger_leaf_paths(dir).map(|path| observe_ledger_leaf(&path))
}

fn assert_ledger_leaves_unchanged(
    dir: &TempDir,
    before: &[LedgerLeafObservation; 7],
    context: &str,
) {
    assert_eq!(
        observe_ledger_leaves(dir),
        *before,
        "{context}: main/WAL/SHM/authority/checkpoint/init-pending/lock must remain exact Present|Absent observations"
    );
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

/// Seed a genuine current-month settled charge before the observation.  The
/// search itself never owns this write: it proves that `budget_paused` must
/// inspect ledger truth, rather than infer exhaustion merely from a zero cap.
fn seed_current_scope_ledger_charge(dir: &TempDir, usd: f64) {
    let repo = Repository::open(dir.path()).unwrap();
    let scope_id = repo.scope_identity().unwrap().scope_id;
    let ledger_path = initialized_ledger_path(dir);
    Connection::open(&ledger_path)
        .unwrap()
        .execute(
            "INSERT INTO cost_ledger (
                scope_id, adapter_kind, input_hash, tool_profile_hash, submission_seq,
                batch_job_id, usd, estimated, outcome, month, recorded_at
             ) VALUES (?1, 'embedding', 'budget-search-seed-input',
                'budget-search-seed-profile', 1, 'budget-search-seed-job', ?2, 0,
                'succeeded', strftime('%Y-%m', 'now'), unixepoch('now') * 1000)",
            params![scope_id, usd],
        )
        .unwrap();
}

/// An unterminated batch intent/job is budget truth too:
/// `ledger_month_total` includes `state IN (0, 1)` reservations even before a
/// terminal `cost_ledger` row exists.
fn seed_current_scope_ledger_reservation(dir: &TempDir, state: i64, usd: f64) {
    let repo = Repository::open(dir.path()).unwrap();
    let scope_id = repo.scope_identity().unwrap().scope_id;
    let input_hash = format!("budget-search-reservation-input-state-{state}");
    let ledger_path = initialized_ledger_path(dir);
    Connection::open(&ledger_path)
        .unwrap()
        .execute(
            "INSERT INTO batch_requests (
                scope_id, adapter_kind, input_hash, tool_profile_hash, state,
                request_kind, estimated_usd, created_at
             ) VALUES (?1, 'embedding', ?2,
                'budget-search-reservation-profile', ?3, 'batch', ?4,
                unixepoch('now') * 1000)",
            params![scope_id, input_hash, state, usd],
        )
        .unwrap();
}

fn write_budget_caps(dir: &TempDir, device_cap: f64, folder_cap: Option<f64>) {
    let device_dir = dir.path().join(".test-config/kio");
    fs::create_dir_all(&device_dir).unwrap();
    fs::write(
        device_dir.join("config.toml"),
        format!("[budget]\nmonthly_usd_cap = {device_cap}\n"),
    )
    .unwrap();
    if let Some(folder_cap) = folder_cap {
        fs::write(
            dir.path().join(".kio/config.toml"),
            format!("[budget]\nmonthly_usd_cap = {folder_cap}\n"),
        )
        .unwrap();
    }
}

fn assert_text_budget_pause_preserves_ledger(dir: &TempDir, query: &str, cap_kind: &str) {
    let before = observe_ledger_leaves(dir);
    for (label, args) in [
        ("explicit text", vec!["search", query, "--mode", "text"]),
        ("auto resolved to text", vec!["search", query]),
    ] {
        let response = private_success(dir, &args);
        assert_eq!(
            response["requested_mode"],
            if label == "explicit text" {
                "text"
            } else {
                "auto"
            },
            "{response}"
        );
        assert_eq!(response["resolved_mode"], "text", "{response}");
        assert_eq!(
            response["index_status"]["budget_paused"], true,
            "{label} must preserve exhausted {cap_kind} budget truth: {response}"
        );
        assert_no_budget_paused_task(dir);
        assert_ledger_leaves_unchanged(dir, &before, label);
    }
}

fn wait_for_search_response_barrier(ready: &Path, child: &mut std::process::Child) {
    let deadline = Instant::now() + Duration::from_secs(6);
    while !ready.exists() && Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            let mut stderr = Vec::new();
            if let Some(mut pipe) = child.stderr.take() {
                use std::io::Read;
                pipe.read_to_end(&mut stderr).unwrap();
            }
            panic!(
                "search exited before response barrier: {status}; stderr={}",
                String::from_utf8_lossy(&stderr)
            );
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

/// Explicit text search is not a vector/hybrid page-one operation. It must
/// not mutate device-ledger SQLite or lifecycle artifacts.
#[test]
fn pb_text_search_preserves_cost_ledger_and_write_sequence_identity() {
    let dir = canonical_tempdir();
    fs::write(
        dir.path().join("ledger-text-search.md"),
        "# Ledger text search\n\ntextledgerneedle\n",
    )
    .unwrap();
    private_success(&dir, &["init"]);
    private_success(&dir, &["index", "--offline"]);
    initialized_ledger_path(&dir);

    let before = observe_ledger_leaves(&dir);
    assert!(
        matches!(before[0], LedgerLeafObservation::Present(_)),
        "index must have initialized the device ledger"
    );

    let ready = dir.path().join("text-search-response.ready");
    let release = ready.with_extension("release");
    let mut child = private_kio_process(&dir, &["search", "textledgerneedle", "--mode", "text"])
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
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["requested_mode"], "text", "{response}");
    assert_eq!(response["resolved_mode"], "text", "{response}");
    assert!(
        !response["results"].as_array().unwrap().is_empty(),
        "{response}"
    );

    assert_ledger_leaves_unchanged(&dir, &before, "explicit text search");

    let fallback = private_success(&dir, &["search", "textledgerneedle"]);
    assert_eq!(fallback["requested_mode"], "auto", "{fallback}");
    assert_eq!(fallback["resolved_mode"], "text", "{fallback}");
    assert!(
        !fallback["results"].as_array().unwrap().is_empty(),
        "{fallback}"
    );
    assert_ledger_leaves_unchanged(&dir, &before, "auto/default search resolved to text");
}

/// A valid text query must not downgrade an unsafe device-ledger leaf to a
/// no-ledger/zero-spend observation or a scope-level partial response. The
/// fixture owns both aliases; no user cache is ever modified.
#[cfg(unix)]
#[test]
fn pb_text_search_unsafe_ledger_hardlink_fails_closed_without_search_fallback() {
    let dir = canonical_tempdir();
    fs::write(
        dir.path().join("unsafe-ledger-search.md"),
        "# Unsafe ledger\n\nunsafeledgerneedle\n",
    )
    .unwrap();
    private_success(&dir, &["init"]);
    private_success(&dir, &["index", "--offline"]);
    initialized_ledger_path(&dir);

    let leaves = ledger_leaf_paths(&dir);
    let main = &leaves[0];
    let before_other_leaves = leaves[1..]
        .iter()
        .map(|leaf| observe_ledger_leaf(leaf))
        .collect::<Vec<_>>();
    let alias = dir.path().join("unsafe-ledger-alias.sqlite");
    fs::hard_link(main, &alias).unwrap();
    let before_main = observe_unsafe_ledger_leaf(main);
    assert_eq!(before_main.nlink, 2, "fixture must create a main hardlink");

    let output = private_kio_process(&dir, &["search", "unsafeledgerneedle", "--mode", "text"])
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
    assert_eq!(error["context"]["reason"], "unsafe_integrity", "{error}");
    assert!(error.get("index_status").is_none(), "{error}");
    assert!(error.get("excluded_scopes").is_none(), "{error}");

    assert_eq!(
        observe_unsafe_ledger_leaf(main),
        before_main,
        "failed text search must not change the unsafe source main leaf"
    );
    for (leaf, before) in leaves[1..].iter().zip(before_other_leaves) {
        assert_eq!(observe_ledger_leaf(leaf), before);
    }
}

/// Budget status is supplementary to a text search result. A syntactically
/// valid but policy-invalid cap must not turn an otherwise usable text search
/// into a command failure (and must not reopen the source ledger merely while
/// discovering that policy error).
#[test]
fn pb_text_search_budget_policy_error_remains_best_effort_and_non_mutating() {
    let dir = canonical_tempdir();
    fs::write(
        dir.path().join("invalid-budget-policy.md"),
        "# Invalid budget policy\n\ninvalidbudgetpolicyneedle\n",
    )
    .unwrap();
    private_success(&dir, &["init"]);
    private_success(&dir, &["index", "--offline"]);
    initialized_ledger_path(&dir);
    // Valid TOML and global config-schema shape, but `read_budget_policy`
    // rejects a folder-layer `per_adapter` policy (that constraint is
    // device-only). There is deliberately no `[search]` key here, so normal
    // text mode selection remains usable.
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[budget.per_adapter]\nembedding = 1\n",
    )
    .unwrap();
    let before = observe_ledger_leaves(&dir);

    let response = private_success(
        &dir,
        &["search", "invalidbudgetpolicyneedle", "--mode", "text"],
    );
    assert_eq!(response["requested_mode"], "text", "{response}");
    assert_eq!(response["resolved_mode"], "text", "{response}");
    assert!(
        !response["results"].as_array().unwrap().is_empty(),
        "best-effort budget observation must not hide text results: {response}"
    );
    assert_ledger_leaves_unchanged(&dir, &before, "invalid budget policy text search");
}

/// An entirely absent source main/WAL/SHM triad is the normal zero-spend
/// observation. Text search must not recreate the ledger, lifecycle records,
/// or SQLite sidecars merely to render `index_status`.
#[test]
fn pb_text_search_missing_ledger_is_zero_spend_and_no_create() {
    let dir = canonical_tempdir();
    fs::write(
        dir.path().join("missing-ledger-search.md"),
        "# Missing ledger\n\nmissingledgerneedle\n",
    )
    .unwrap();
    private_success(&dir, &["init"]);
    private_success(&dir, &["index", "--offline"]);
    for path in ledger_leaf_paths(&dir) {
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }
    let absent = observe_ledger_leaves(&dir);
    assert!(
        absent
            .iter()
            .all(|leaf| matches!(leaf, LedgerLeafObservation::Absent)),
        "fixture must start with no source ledger or lifecycle artifacts"
    );

    let response = private_success(&dir, &["search", "missingledgerneedle", "--mode", "text"]);
    assert_eq!(response["requested_mode"], "text", "{response}");
    assert_eq!(response["resolved_mode"], "text", "{response}");
    assert_eq!(
        response["index_status"]["budget_paused"], false,
        "{response}"
    );
    assert!(
        !response["results"].as_array().unwrap().is_empty(),
        "missing ledger must not hide ordinary text results: {response}"
    );
    assert_ledger_leaves_unchanged(&dir, &absent, "missing ledger text search");
}

/// A no-ledger result needs the same stable-or-fail observation as a present
/// snapshot. If another process creates an already-exhausted ledger between
/// the first and confirming all-absent triad reads, search must retry and use
/// that real total rather than publish a false `budget_paused=false`.
#[cfg(unix)]
#[test]
fn pb_text_search_retries_absent_to_present_ledger_before_budget_status() {
    let dir = canonical_tempdir();
    fs::write(
        dir.path().join("appearing-ledger-search.md"),
        "# Appearing ledger\n\nappearingledgerneedle\n",
    )
    .unwrap();
    private_success(&dir, &["init"]);
    private_success(&dir, &["index", "--offline"]);
    write_budget_caps(&dir, 1.0, None);
    assert_no_budget_paused_task(&dir);
    for path in ledger_leaf_paths(&dir) {
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }
    assert!(
        observe_ledger_leaves(&dir)
            .iter()
            .all(|leaf| matches!(leaf, LedgerLeafObservation::Absent))
    );

    let barrier_dir = dir.path().join(".ledger-absent-barrier");
    fs::create_dir(&barrier_dir).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&barrier_dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let ready = barrier_dir.join("ledger-absent-confirm.ready");
    let release = ready.with_extension("release");
    let mut child =
        private_kio_process(&dir, &["search", "appearingledgerneedle", "--mode", "text"])
            .env("KIO_TEST_LEDGER_SNAPSHOT_ABSENT_BARRIER_READY", &ready)
            .arg("--json")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
    wait_for_search_response_barrier(&ready, &mut child);
    #[cfg(target_os = "macos")]
    assert_search_child_process_tree(&child);

    seed_current_scope_ledger_charge(&dir, 1.0);
    let after_creator = observe_ledger_leaves(&dir);
    fs::write(&release, b"release").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["requested_mode"], "text", "{response}");
    assert_eq!(response["resolved_mode"], "text", "{response}");
    assert_eq!(
        response["index_status"]["budget_paused"], true,
        "the appearing exhausted ledger must never be reported as zero spend: {response}"
    );
    assert!(
        !response["results"].as_array().unwrap().is_empty(),
        "{response}"
    );
    assert_no_budget_paused_task(&dir);
    assert_ledger_leaves_unchanged(
        &dir,
        &after_creator,
        "absent-to-present retry must not mutate the creator's ledger",
    );
}

/// Exhaustion is ledger-derived state, not a property of the cap alone. Both
/// text entry paths must report an exhausted device cap without reopening the
/// source SQLite database or creating WAL/SHM sidecars.
#[test]
fn pb_text_and_auto_preserve_device_cap_exhaustion_without_ledger_mutation() {
    let dir = canonical_tempdir();
    fs::write(
        dir.path().join("device-budget.md"),
        "# Device budget\n\ndevicebudgetneedle\n",
    )
    .unwrap();
    private_success(&dir, &["init"]);
    private_success(&dir, &["index", "--offline"]);
    assert_no_budget_paused_task(&dir);
    write_budget_caps(&dir, 1.0, None);
    seed_current_scope_ledger_charge(&dir, 1.0);
    assert_text_budget_pause_preserves_ledger(&dir, "devicebudgetneedle", "device");
}

/// The folder cap has an independent current-scope total. It must remain
/// visible to text/auto status while the source device ledger stays untouched.
#[test]
fn pb_text_and_auto_preserve_folder_cap_exhaustion_without_ledger_mutation() {
    let dir = canonical_tempdir();
    fs::write(
        dir.path().join("folder-budget.md"),
        "# Folder budget\n\nfolderbudgetneedle\n",
    )
    .unwrap();
    private_success(&dir, &["init"]);
    private_success(&dir, &["index", "--offline"]);
    assert_no_budget_paused_task(&dir);
    write_budget_caps(&dir, 10.0, Some(1.0));
    seed_current_scope_ledger_charge(&dir, 1.0);
    assert_text_budget_pause_preserves_ledger(&dir, "folderbudgetneedle", "folder");
}

/// Reservation-only exhaustion must not be mistaken for zero spend. The
/// device total depends on both active states: neither the `state=0` intent
/// nor the `state=1` job-created reservation alone reaches the cap.
#[test]
fn pb_text_and_auto_preserve_device_reservation_exhaustion_without_ledger_mutation() {
    let dir = canonical_tempdir();
    fs::write(
        dir.path().join("device-reservation.md"),
        "# Device reservation\n\ndevicereservationneedle\n",
    )
    .unwrap();
    private_success(&dir, &["init"]);
    private_success(&dir, &["index", "--offline"]);
    assert_no_budget_paused_task(&dir);
    write_budget_caps(&dir, 1.0, None);
    seed_current_scope_ledger_reservation(&dir, 0, 0.4);
    seed_current_scope_ledger_reservation(&dir, 1, 0.6);
    assert_text_budget_pause_preserves_ledger(&dir, "devicereservationneedle", "device");
}

/// The folder total likewise depends on both active states, so an
/// implementation that omits either `state=0` or `state=1` reports the wrong
/// non-paused status. No settled charge is present here.
#[test]
fn pb_text_and_auto_preserve_folder_reservation_exhaustion_without_ledger_mutation() {
    let dir = canonical_tempdir();
    fs::write(
        dir.path().join("folder-reservation.md"),
        "# Folder reservation\n\nfolderreservationneedle\n",
    )
    .unwrap();
    private_success(&dir, &["init"]);
    private_success(&dir, &["index", "--offline"]);
    assert_no_budget_paused_task(&dir);
    write_budget_caps(&dir, 10.0, Some(1.0));
    seed_current_scope_ledger_reservation(&dir, 0, 0.4);
    seed_current_scope_ledger_reservation(&dir, 1, 0.6);
    assert_text_budget_pause_preserves_ledger(&dir, "folderreservationneedle", "folder");
}

/// init + index + search, returning (dir, evidence_pointer, evidence_uri).
fn fixture() -> (TempDir, Value, String) {
    let dir = canonical_tempdir();
    fs::write(
        dir.path().join("evidence.md"),
        "# Evidence\n\nTTL is 3600 seconds.\n",
    )
    .unwrap();
    success(&dir, &["init"]);
    success(&dir, &["index", "--offline"]);
    let search = success(&dir, &["search", "3600", "--mode", "text"]);
    let pointer = search["results"][0]["evidence_pointer"].clone();
    let uri = search["results"][0]["evidence_uri"]
        .as_str()
        .unwrap()
        .to_owned();
    (dir, pointer, uri)
}

fn make_pointer_commit_final_shallow(dir: &TempDir, pointer: &Value) {
    let commit_hash = pointer["commit"].as_str().unwrap();
    let repo = Repository::open(dir.path()).unwrap();
    let commit = repo.read_commit(commit_hash).unwrap();

    // A completed shallow state is valid only for a non-tip Auto/Repaired
    // commit with an exact canonical receipt. Advance HEAD before removing the
    // pointer commit's tree so this fixture models a real completed GC sweep.
    fs::write(
        dir.path().join("advance.md"),
        "# Advance\n\nadvance the shallow fixture head\n",
    )
    .unwrap();
    success(dir, &["index", "--offline"]);

    let receipt_path = kio_dir(dir)
        .join("gc/shallowed")
        .join(commit_hash.strip_prefix("sha256:").unwrap());
    fs::create_dir_all(receipt_path.parent().unwrap()).unwrap();
    let receipt = ShallowReceipt::new(
        commit_hash.to_owned(),
        commit.tree.clone(),
        "2026-08-14T00:00:00Z".into(),
    )
    .unwrap();
    fs::write(receipt_path, receipt.canonical_bytes().unwrap()).unwrap();

    let store = ObjectStore::new(kio_dir(dir));
    fs::remove_file(store.object_path(ObjectKind::Tree, &commit.tree).unwrap()).unwrap();
}

fn kio_dir(dir: &TempDir) -> std::path::PathBuf {
    dir.path().join(".kio")
}

/// Fixtures that append a lifecycle event directly via `PurgeState` (bypassing
/// the `kio purge` CLI, which would otherwise also resync `index_metadata.
/// last_lifecycle_epoch`) must resync it explicitly afterward, or the LC45
/// read-command-entry check (`check_index_generation_current`) correctly —
/// if inconveniently for a hand-built fixture — refuses every subsequent read
/// command with `KIO-E-INDEX-REBUILDING-001` until it does. `repair
/// --rebuild-db` is the standard resync path (PB28).
fn resync_index_metadata(dir: &TempDir) {
    success(dir, &["repair", "rebuild-db"]);
}

/// Fan-out CAS path for a raw content-addressed object under
/// `objects/<kind_dir>/`, independent of `hash`'s real correspondence to the
/// bytes at that path (used to write deliberately-corrupt fixtures).
fn content_path(kio_dir: &Path, kind_dir: &str, hash: &str) -> std::path::PathBuf {
    let digest = hash.strip_prefix("sha256:").unwrap();
    kio_dir
        .join("objects")
        .join(kind_dir)
        .join(&digest[0..2])
        .join(&digest[2..4])
        .join(digest)
}

fn write_content_bytes(kio_dir: &Path, kind_dir: &str, bytes: &[u8]) -> String {
    let hash = hash_bytes(bytes);
    let path = content_path(kio_dir, kind_dir, &hash);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, bytes).unwrap();
    hash
}

/// Publish a real successor snapshot whose immutable manifest marks every
/// normalized unit Failed. This models a point-in-time retry state without
/// corrupting bytes underneath an existing CAS hash.
fn successor_with_failed_pinned_manifest(dir: &TempDir, pointer: &Value) -> String {
    let repo = Repository::open(dir.path()).unwrap();
    let parent = pointer["commit"].as_str().unwrap().to_owned();
    let parent_commit = repo.read_commit(&parent).unwrap();
    let mut tree = repo.read_tree(&parent_commit.tree).unwrap();
    let entry = tree
        .entries
        .iter_mut()
        .find(|entry| entry.raw_hash == pointer["raw_hash"].as_str().unwrap())
        .unwrap();
    let normalize = entry.normalize.as_mut().unwrap();
    let store = ObjectStore::new(kio_dir(dir));
    let manifest_bytes = store
        .read_content_object_bytes(
            ContentObjectKind::Manifest,
            &normalize.manifest_hash,
            8 * 1024 * 1024,
        )
        .unwrap();
    let mut manifest: Value = serde_json::from_slice(&manifest_bytes).unwrap();
    for unit in manifest["units"].as_array_mut().unwrap() {
        unit["status"] = Value::String("failed".to_owned());
        unit["unit_object_hash"] = Value::Null;
        unit["error_kind"] = Value::String("contract_violation".to_owned());
    }
    let manifest_bytes = serde_jcs::to_vec(&manifest).unwrap();
    normalize.manifest_hash = store
        .write_content_object(ContentObjectKind::Manifest, &manifest_bytes)
        .unwrap();

    let tree = build_tree(tree.entries).unwrap();
    let (tree_hash, _) = store
        .write_json(ObjectKind::Tree, &serde_json::to_value(&tree).unwrap())
        .unwrap();
    let commit = CommitObject::new(
        tree_hash,
        Some(parent),
        "2026-07-20T00:00:00Z".to_owned(),
        "fixture: pin failed normalized manifest".to_owned(),
        parent_commit.tool_lock_hash,
        CommitStats {
            files_added: 0,
            files_modified: 1,
            files_deleted: 0,
        },
        CommitType::Manual,
    )
    .unwrap();
    let (commit_hash, _) = store
        .write_json(ObjectKind::Commit, &serde_json::to_value(&commit).unwrap())
        .unwrap();
    fs::write(kio_dir(dir).join("HEAD"), format!("{commit_hash}\n")).unwrap();
    commit_hash
}

/// PB46: two immutable normalized bodies for the same
/// `(raw_hash, tool_profile_hash, gen, unit_key)` are distinguished by the
/// manifest pinned by each commit. Rebuilding the derived index must preserve
/// that point-in-time binding rather than letting C2's same-generation retry
/// overwrite C1's searchable body.
#[test]
fn pb46_same_generation_immutable_bodies_remain_commit_pinned_after_rebuild() {
    const BODY_A: &str = "samegenbodyalphaonly";
    const BODY_B: &str = "samegenbodybetaonly";

    let dir = canonical_tempdir();
    fs::write(
        dir.path().join("evidence.md"),
        format!("# Evidence\n\n{BODY_A}\n"),
    )
    .unwrap();
    success(&dir, &["init"]);
    let indexed = success(&dir, &["index", "--offline"]);
    let c1 = indexed["commit_hash"].as_str().unwrap().to_owned();

    let repo = Repository::open(dir.path()).unwrap();
    let parent_commit = repo.read_commit(&c1).unwrap();
    let mut tree = repo.read_tree(&parent_commit.tree).unwrap();
    let entry = tree
        .entries
        .iter_mut()
        .find(|entry| entry.path == "evidence.md")
        .unwrap();
    let normalize = entry.normalize.as_mut().unwrap();
    let store = ObjectStore::new(kio_dir(&dir));
    let manifest_bytes = store
        .read_content_object_bytes(
            ContentObjectKind::Manifest,
            &normalize.manifest_hash,
            8 * 1024 * 1024,
        )
        .unwrap();
    let mut manifest: Value = serde_json::from_slice(&manifest_bytes).unwrap();
    let unit_hash = manifest["units"].as_array().unwrap().first().unwrap()["unit_object_hash"]
        .as_str()
        .unwrap();
    let unit_bytes = store
        .read_content_object_bytes(
            ContentObjectKind::NormalizedUnit,
            unit_hash,
            8 * 1024 * 1024,
        )
        .unwrap();
    let mut unit: NormalizedUnitObject = serde_json::from_slice(&unit_bytes).unwrap();
    unit.markdown = BODY_B.to_owned();
    let replacement_unit_bytes = serde_jcs::to_vec(&unit).unwrap();
    let replacement_unit_hash = store
        .write_content_object(ContentObjectKind::NormalizedUnit, &replacement_unit_bytes)
        .unwrap();
    assert_ne!(replacement_unit_hash, unit_hash);

    manifest["units"].as_array_mut().unwrap()[0]["unit_object_hash"] =
        Value::String(replacement_unit_hash.clone());
    let replacement_manifest_bytes = serde_jcs::to_vec(&manifest).unwrap();
    normalize.manifest_hash = store
        .write_content_object(ContentObjectKind::Manifest, &replacement_manifest_bytes)
        .unwrap();
    let tree = build_tree(tree.entries).unwrap();
    let (tree_hash, _) = store
        .write_json(ObjectKind::Tree, &serde_json::to_value(&tree).unwrap())
        .unwrap();
    let c2_commit = CommitObject::new(
        tree_hash,
        Some(c1.clone()),
        "2026-08-12T00:00:00Z".to_owned(),
        "fixture: same-generation immutable normalized body B".to_owned(),
        parent_commit.tool_lock_hash,
        CommitStats {
            files_added: 0,
            files_modified: 1,
            files_deleted: 0,
        },
        CommitType::Manual,
    )
    .unwrap();
    let (c2, _) = store
        .write_json(
            ObjectKind::Commit,
            &serde_json::to_value(&c2_commit).unwrap(),
        )
        .unwrap();
    fs::write(kio_dir(&dir).join("HEAD"), format!("{c2}\n")).unwrap();

    success(&dir, &["repair", "rebuild-db"]);

    let at_c1_a = success(
        &dir,
        &[
            "search", BODY_A, "--mode", "text", "--at", &c1, "--scope", ".",
        ],
    );
    assert_eq!(at_c1_a["results"].as_array().unwrap().len(), 1, "{at_c1_a}");
    let c1_pointer = &at_c1_a["results"][0]["evidence_pointer"];
    assert_eq!(c1_pointer["commit"], c1, "{at_c1_a}");
    assert!(
        at_c1_a["results"][0].to_string().contains(BODY_A),
        "{at_c1_a}"
    );
    let at_c1_b = success(
        &dir,
        &[
            "search", BODY_B, "--mode", "text", "--at", &c1, "--scope", ".",
        ],
    );
    assert!(
        at_c1_b["results"].as_array().unwrap().is_empty(),
        "{at_c1_b}"
    );

    let at_c2_b = success(
        &dir,
        &[
            "search", BODY_B, "--mode", "text", "--at", &c2, "--scope", ".",
        ],
    );
    assert_eq!(at_c2_b["results"].as_array().unwrap().len(), 1, "{at_c2_b}");
    let c2_pointer = &at_c2_b["results"][0]["evidence_pointer"];
    assert_eq!(c2_pointer["commit"], c2, "{at_c2_b}");
    assert!(
        at_c2_b["results"][0].to_string().contains(BODY_B),
        "{at_c2_b}"
    );
    let at_c2_a = success(
        &dir,
        &[
            "search", BODY_A, "--mode", "text", "--at", &c2, "--scope", ".",
        ],
    );
    assert!(
        at_c2_a["results"].as_array().unwrap().is_empty(),
        "{at_c2_a}"
    );
}

// ===========================================================================
// §A/§B — fsck verification-target expansion (U39/U40).
// ===========================================================================

/// An embedding object in the canonical 03 §8.1 form, filed under its own
/// identity hash. `spec_version`/`target_type`/… are the identity fields the
/// storage key is computed from; the vector rides in the body.
fn write_embedding_fixture(kio_dir: &Path, vector: &[f32], declared_dimensions: u64) -> String {
    let object = EmbeddingObject {
        spec_version: 1,
        target_type: "chunk".to_owned(),
        target_hash: "sha256:fixture-text".to_owned(),
        profile_hash: "sha256:fixture-profile".to_owned(),
        modality: "multimodal".to_owned(),
        dimensions: declared_dimensions,
        distance: "cosine".to_owned(),
        context: None,
        vector: vector.to_vec(),
    };
    // Written by hand rather than through `write_embedding`, because several of
    // these fixtures are deliberately invalid and the store would refuse them.
    // Several of these fixtures are invalid on purpose (wrong length, NaN) and
    // cannot compute their own identity, so the key comes from a sanitized
    // sibling. The identity fields are unaffected by the vector either way —
    // the key names what the vector is OF — so this is the same key the fixture
    // would have had if it were well-formed, which is what lets fsck find it.
    let mut keyed = object.clone();
    keyed.vector = vec![0.0; keyed.dimensions as usize];
    let hash = keyed.identity_hash().unwrap();
    let bytes = embedding_fixture_bytes(&object);
    let path = content_path(kio_dir, ContentObjectKind::Embedding.directory(), &hash);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, bytes).unwrap();
    hash
}

/// The 03 §8.1 byte layout, assembled without the validation `to_bytes` does,
/// so a fixture can be invalid on purpose.
fn embedding_fixture_bytes(object: &EmbeddingObject) -> Vec<u8> {
    let header = serde_jcs::to_vec(&serde_json::json!({
        "dimensions": object.dimensions,
        "distance": object.distance,
        "modality": object.modality,
        "profile_hash": object.profile_hash,
        "spec_version": object.spec_version,
        "target_hash": object.target_hash,
        "target_type": object.target_type,
    }))
    .unwrap();
    let mut vector_bytes = Vec::new();
    for component in &object.vector {
        vector_bytes.extend_from_slice(&component.to_le_bytes());
    }
    let mut out = header;
    out.push(b'\n');
    out.extend_from_slice(
        base64::engine::general_purpose::STANDARD
            .encode(&vector_bytes)
            .as_bytes(),
    );
    out.push(b'\n');
    out.extend_from_slice(
        kio_core::cas::lower_hex(&<sha2::Sha256 as sha2::Digest>::digest(&vector_bytes)).as_bytes(),
    );
    out
}

/// PB01 (a)(c)(f): a well-formed embedding object (declared dimensions ==
/// vector length, all finite, digest matches, identity matches its key) passes
/// verification cleanly.
#[test]
fn pb01_embedding_valid_vector_passes_verification() {
    let (dir, ..) = fixture();
    write_embedding_fixture(&kio_dir(&dir), &[0.1, 0.2, 0.3], 3);
    let output = success(&dir, &["repair", "verify-objects"]);
    assert_eq!(output["status"], "ok", "{output}");
    assert_eq!(output["checked"]["embeddings"], 1, "{output}");
}

/// PB01 (b): declared `dimensions` does not match the vector length — a finding.
#[test]
fn pb01_embedding_length_mismatch_is_a_finding() {
    let (dir, ..) = fixture();
    write_embedding_fixture(&kio_dir(&dir), &[0.1, 0.2], 3);
    let (code, output) = run(&dir, &["repair", "verify-objects"]);
    assert_eq!(code, 3, "{output}");
    assert!(
        output["remaining_findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["kind"] == "embedding_corrupt")
    );
}

/// PB01 (d)/(e): a non-finite (`NaN`/`Infinity`) vector element is a finding.
///
/// Unlike the old JSON body, the binary layout can CARRY a NaN — it is just a
/// bit pattern — so this now exercises the explicit finiteness check rather
/// than a JSON parse error.
#[test]
fn pb01_embedding_non_finite_vector_element_is_a_finding() {
    let (dir, ..) = fixture();
    write_embedding_fixture(&kio_dir(&dir), &[f32::NAN], 1);
    let (code, output) = run(&dir, &["repair", "verify-objects"]);
    assert_eq!(code, 3, "{output}");
    assert!(
        output["remaining_findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["kind"] == "embedding_corrupt")
    );
}

/// PB01 (g): the vector's bytes do not match the digest recorded beside them —
/// a finding.
///
/// This is the check that has no substitute. An embedding's storage key hashes
/// its IDENTITY (03 §8.1), so unlike every other content-addressed kind, the
/// key says nothing about the body: a bit flip inside the vector is invisible
/// to it and visible only to the trailing digest.
#[test]
fn pb01_embedding_digest_mismatch_is_a_finding() {
    let (dir, ..) = fixture();
    let hash = write_embedding_fixture(&kio_dir(&dir), &[0.5], 1);
    let path = content_path(
        &kio_dir(&dir),
        ContentObjectKind::Embedding.directory(),
        &hash,
    );
    let text = fs::read_to_string(&path).unwrap();
    let mut lines = text.split('\n').collect::<Vec<_>>();
    let flipped = if lines[1].starts_with('A') {
        format!("B{}", &lines[1][1..])
    } else {
        format!("A{}", &lines[1][1..])
    };
    lines[1] = &flipped;
    fs::write(&path, lines.join("\n")).unwrap();
    let (code, output) = run(&dir, &["repair", "verify-objects"]);
    assert_eq!(code, 3, "{output}");
    assert!(
        output["remaining_findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["kind"] == "embedding_corrupt")
    );
}

/// PB01 (2026-07-26, R25-6): an object filed under someone else's identity is a
/// finding — the check that replaces the generic content-hash comparison every
/// other CAS kind gets.
#[test]
fn pb01_embedding_under_a_foreign_identity_is_a_finding() {
    let (dir, ..) = fixture();
    let hash = write_embedding_fixture(&kio_dir(&dir), &[0.5], 1);
    let bytes = fs::read(content_path(
        &kio_dir(&dir),
        ContentObjectKind::Embedding.directory(),
        &hash,
    ))
    .unwrap();
    let foreign = "sha256:0000000000000000000000000000000000000000000000000000000000000001";
    let path = content_path(
        &kio_dir(&dir),
        ContentObjectKind::Embedding.directory(),
        foreign,
    );
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, bytes).unwrap();
    let (code, output) = run(&dir, &["repair", "verify-objects"]);
    assert_eq!(code, 3, "{output}");
    assert!(
        output["remaining_findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["kind"] == "embedding_corrupt")
    );
}

/// PB02: manifest and toollock CAS objects join the fsck verification
/// closure — a valid one is counted, a digest-corrupt one is a finding.
#[test]
fn pb02_manifest_and_toollock_join_the_verification_closure() {
    let (dir, ..) = fixture();
    write_content_bytes(
        &kio_dir(&dir),
        ContentObjectKind::Manifest.directory(),
        br#"{"kind":"manifest-fixture"}"#,
    );
    let toollock_hash = write_content_bytes(
        &kio_dir(&dir),
        ContentObjectKind::Toollock.directory(),
        br#"{"kind":"toollock-fixture"}"#,
    );
    let toollock_path = content_path(
        &kio_dir(&dir),
        ContentObjectKind::Toollock.directory(),
        &toollock_hash,
    );
    fs::write(&toollock_path, br#"{"kind":"corrupted-in-place"}"#).unwrap();

    let (code, output) = run(&dir, &["repair", "verify-objects"]);
    assert_eq!(code, 3, "{output}");
    // PB04: `fixture()`'s own `kio index` now durably CAS-writes ITS
    // normalized instance's genuine manifest object too (NormalizeRef's new
    // `manifest_hash` field, computed at index time) — one real manifest
    // from the fixture's indexing plus this test's own synthetic
    // `manifest-fixture` object above, so the closure now counts 2, not 1.
    assert_eq!(output["checked"]["manifests"], 2, "{output}");
    assert!(
        output["remaining_findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["kind"] == "toollock_corrupt")
    );
}

/// PB03 [regression-lock]: chunk exact-span/text_hash mismatch is still
/// detected exactly as before this session's changes.
#[test]
fn pb03_regression_chunk_span_mismatch_still_a_finding() {
    let (dir, pointer, _) = fixture();
    let chunk_hash = pointer["chunk_hash"].as_str().unwrap();
    let store = ObjectStore::new(kio_dir(&dir));
    let chunk_path = store.chunk_path(chunk_hash).unwrap();
    let mut chunk: Value = serde_json::from_slice(&fs::read(&chunk_path).unwrap()).unwrap();
    // Keep `text`/`text_hash` mutually consistent (so `ChunkObject::validate`'s
    // own CAS-level identity check still passes and this reaches the span
    // comparison as `chunk_span_mismatch`, not an earlier `chunk_corrupt`),
    // but inconsistent with the normalized unit's actual [byte_start,
    // byte_end) span.
    let tampered = "tampered text that does not match the span (byte length differs)";
    chunk["text"] = serde_json::json!(tampered);
    chunk["text_hash"] = serde_json::json!(hash_bytes(tampered.as_bytes()));
    // Rewrite the object directly (bypassing CAS identity re-derivation) so
    // the exact on-disk bytes disagree with the normalized span.
    fs::write(&chunk_path, serde_json::to_vec(&chunk).unwrap()).unwrap();
    let (code, output) = run(&dir, &["repair", "verify-objects"]);
    assert_eq!(code, 3, "{output}");
    assert!(
        output["remaining_findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["kind"] == "chunk_span_mismatch")
    );
}

// ===========================================================================
// §C — names.jsonl (U41).
// ===========================================================================

fn names_jsonl_path(dir: &TempDir) -> std::path::PathBuf {
    kio_dir(dir).join("refs/tags-v1/names.jsonl")
}

/// PB07: `kio tag` appends a names.jsonl row whose digest64 matches the
/// recomputed digest of the logical name; a corrupted digest64 is a finding.
#[test]
fn pb07_names_jsonl_schema_and_digest_recompute() {
    let (dir, ..) = fixture();
    success(&dir, &["tag", "release-1"]);
    let text = fs::read_to_string(names_jsonl_path(&dir)).unwrap();
    let line = text.lines().next().unwrap();
    let record: Value = serde_json::from_str(line).unwrap();
    assert_eq!(record["logical_name"], "release-1");
    assert!(record["digest64"].as_str().unwrap().len() == 64);

    // Baseline: a freshly-tagged scope has no findings.
    let clean = success(&dir, &["repair", "verify-objects"]);
    assert_eq!(clean["status"], "ok", "{clean}");

    // Corrupt digest64 -> finding.
    let mut corrupted = record.clone();
    corrupted["digest64"] = serde_json::json!("0".repeat(64));
    fs::write(
        names_jsonl_path(&dir),
        format!("{}\n", serde_json::to_string(&corrupted).unwrap()),
    )
    .unwrap();
    let (code, output) = run(&dir, &["repair", "verify-objects"]);
    assert_eq!(code, 3, "{output}");
    assert!(
        output["remaining_findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["kind"] == "names_jsonl_corrupt")
    );
}

/// PB08: only the FINAL line may be a torn (truncated) tail — tolerated. A
/// malformed line that is NOT the final one is corruption.
#[test]
fn pb08_names_jsonl_torn_tail_tolerated_mid_malformed_rejected() {
    let (dir, ..) = fixture();
    success(&dir, &["tag", "release-1"]);
    let good_line = fs::read_to_string(names_jsonl_path(&dir)).unwrap();
    let good_line = good_line.trim_end();

    // (a) torn tail: append a final line with no trailing newline that is
    // itself truncated mid-object.
    fs::write(
        names_jsonl_path(&dir),
        format!("{good_line}\n{{\"digest64\":\"ab"),
    )
    .unwrap();
    let torn = success(&dir, &["repair", "verify-objects"]);
    assert_eq!(
        torn["status"], "ok",
        "torn tail must not be a finding: {torn}"
    );

    // (b) malformed line NOT last: a torn line followed by a valid line.
    fs::write(
        names_jsonl_path(&dir),
        format!("{{\"digest64\":\"ab\n{good_line}\n"),
    )
    .unwrap();
    let (code, output) = run(&dir, &["repair", "verify-objects"]);
    assert_eq!(code, 3, "{output}");
    assert!(
        output["remaining_findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["kind"] == "names_jsonl_corrupt")
    );
}

/// PB09: a canonical tag ref with no corresponding names.jsonl row is
/// corruption; the reverse (a names row with no ref, tag-delete residue) is
/// normal.
#[test]
fn pb09_canonical_ref_names_correspondence_is_asymmetric() {
    let (dir, ..) = fixture();
    success(&dir, &["tag", "release-1"]);

    // (b) ref-less names row (simulated tag deletion: remove only the ref)
    // must NOT be a finding.
    let canonical_dir = kio_dir(&dir).join("refs/tags-v1");
    for entry in fs::read_dir(&canonical_dir).unwrap().flatten() {
        if entry.file_name() != "names.jsonl" {
            fs::remove_file(entry.path()).unwrap();
        }
    }
    let ref_less = success(&dir, &["repair", "verify-objects"]);
    assert_eq!(
        ref_less["status"], "ok",
        "a names row with no ref must be normal: {ref_less}"
    );

    // (a) a ref with no names row IS corruption: recreate a canonical ref
    // pointing at HEAD that names.jsonl never recorded.
    let head = fs::read_to_string(kio_dir(&dir).join("HEAD")).unwrap();
    let orphan_leaf = format!("tag-{}", "1".repeat(64));
    fs::write(canonical_dir.join(&orphan_leaf), head.trim()).unwrap();
    let (code, output) = run(&dir, &["repair", "verify-objects"]);
    assert_eq!(code, 3, "{output}");
    assert!(
        output["remaining_findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["kind"] == "names_jsonl_corrupt")
    );
}

// ===========================================================================
// §E — `--prune-orphans` (U43). PB14/16/17 and two of PB15's four blockers
// are NOT implemented this session (see `prune_orphans`'s doc comment in
// src/verify_objects.rs) — not tested here.
// ===========================================================================

/// PB12: `repair` accepts exactly one of `--rebuild-db`/`--verify-objects`/
/// `--registry-prune`; `--prune-orphans` is a `--verify-objects`-only
/// modifier.
#[test]
fn pb12_prune_orphans_flag_parsing() {
    let (dir, ..) = fixture();
    let accepted = success(
        &dir,
        &["repair", "verify-objects", "--prune-orphans", "--yes"],
    );
    assert_eq!(accepted["status"], "ok", "{accepted}");

    kio(&dir, &["repair", "--prune-orphans"])
        .arg("--json")
        .assert()
        .code(2);
    kio(&dir, &["repair", "rebuild-db", "--prune-orphans"])
        .arg("--json")
        .assert()
        .code(2);
}

/// 06 §1 requires a confirmation prompt before either destructive `repair`
/// operation removes anything. Implemented 2026-07-25 — until then `--yes` was
/// accepted and inert, so the prompt it was meant to skip did not exist.
///
/// Non-interactive (no TTY, as every contract run is) without `--yes` must
/// refuse, and must refuse WITHOUT deleting: the second call proves the same
/// orphans are still there to be pruned.
#[test]
fn repair_prune_requires_confirmation_and_refuses_without_deleting() {
    let (dir, ..) = fixture();
    let prepared_hash = write_content_bytes(
        &kio_dir(&dir),
        ContentObjectKind::Prepared.directory(),
        b"orphan prepared bytes for the confirmation contract",
    );
    let store = ObjectStore::new(kio_dir(&dir));

    let (code, output) = run(&dir, &["repair", "verify-objects", "--prune-orphans"]);
    assert_eq!(code, 9, "confirmation-rejected exit: {output}");
    assert_eq!(
        output["error_code"], "KIO-E-CONFIRM-REJECTED-001",
        "{output}"
    );
    assert!(
        output["context"]["target_count"].as_u64().unwrap() >= 1,
        "the refusal reports what it would have removed: {output}"
    );
    // The refusal removed nothing: the orphan is still accounted for.
    assert!(
        store
            .inspect_content_accounted(ContentObjectKind::Prepared, &prepared_hash)
            .is_ok(),
        "a rejected confirmation must not delete anything"
    );
    let accepted = success(
        &dir,
        &["repair", "verify-objects", "--prune-orphans", "--yes"],
    );
    assert_eq!(accepted["prune_orphans"]["status"], "pruned", "{accepted}");
    assert!(
        accepted["prune_orphans"]["pruned_prepared_count"]
            .as_u64()
            .unwrap()
            >= 1,
        "the refused run must not have consumed the orphans: {accepted}"
    );
}

/// PB13: an orphan prepared/image object (referenced by no live manifest) is
/// deleted by `--prune-orphans`.
#[test]
fn pb13_prune_orphans_deletes_unreferenced_prepared_and_image() {
    let (dir, ..) = fixture();
    let prepared_hash = write_content_bytes(
        &kio_dir(&dir),
        ContentObjectKind::Prepared.directory(),
        b"orphan prepared bytes never referenced by any manifest",
    );
    let image_hash = write_content_bytes(
        &kio_dir(&dir),
        ContentObjectKind::Image.directory(),
        b"orphan image bytes never referenced by any manifest",
    );
    let store = ObjectStore::new(kio_dir(&dir));
    assert!(
        store
            .inspect_content_accounted(ContentObjectKind::Prepared, &prepared_hash)
            .is_ok()
    );

    let output = success(
        &dir,
        &["repair", "verify-objects", "--prune-orphans", "--yes"],
    );
    assert_eq!(output["status"], "ok", "{output}");
    assert_eq!(output["prune_orphans"]["status"], "pruned", "{output}");
    assert_eq!(
        output["prune_orphans"]["pruned_prepared_count"], 1,
        "{output}"
    );
    assert_eq!(output["prune_orphans"]["pruned_image_count"], 1, "{output}");
    assert!(
        store
            .inspect_content_accounted(ContentObjectKind::Prepared, &prepared_hash)
            .is_err()
    );
    assert!(
        store
            .inspect_content_accounted(ContentObjectKind::Image, &image_hash)
            .is_err()
    );
}

/// PB15 (partial — active purge journal only, one of four blocker
/// conditions): `--prune-orphans` refuses to run while a purge journal is
/// active, even though an orphan exists that would otherwise be pruned.
#[test]
fn pb15_prune_orphans_blocked_by_active_purge_journal() {
    let (dir, pointer, _) = fixture();
    let raw_hash = pointer["raw_hash"].as_str().unwrap().to_owned();
    write_content_bytes(
        &kio_dir(&dir),
        ContentObjectKind::Prepared.directory(),
        b"orphan that must survive because the journal blocks pruning",
    );
    let purge = PurgeState::open(kio_dir(&dir)).unwrap();
    purge
        .begin(
            vec![raw_hash],
            PurgeReason::Legal,
            TombstoneMode::Default,
            "user",
            "2026-07-13T00:00:00Z",
            1,
            hash_bytes(b"planned purge commit placeholder"),
            hash_bytes(b"planned purge closure placeholder"),
            kio_core::scope::new_ulid(dir.path()),
        )
        .unwrap();

    let (code, output) = run(
        &dir,
        &["repair", "verify-objects", "--prune-orphans", "--yes"],
    );
    // The underlying verify pass itself reports the active journal as a
    // `purge_incomplete` finding (exit 3) before prune-orphans would even run.
    assert_eq!(code, 3, "{output}");
    assert!(
        output["remaining_findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["kind"] == "purge_incomplete")
    );
    assert!(output.get("prune_orphans").is_none(), "{output}");
}

// ===========================================================================
// §G — SQLite schema-change regression locks (U45).
// ===========================================================================

/// PB20: a non-current derived schema is rejected before Kio can create, alter, or
/// rebuild any SQLite object. The bytes are the contract: the operator must
/// choose `kio repair rebuild-db`, not receive a silent in-place migration.
#[test]
fn pb20_incompatible_sqlite_schema_is_fail_closed_without_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sqlite.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE chunks (chunk_id TEXT PRIMARY KEY, chunking_config_hash TEXT NOT NULL);",
    )
    .unwrap();
    drop(conn);
    let before = fs::read(&path).unwrap();

    let error = kio_index::fts::SqliteFtsIndex::open(
        &path,
        kio_index::fts::FtsSchemaConfig {
            tokenizer: kio_index::fts::FtsTokenizer::Trigram,
        },
    )
    .err()
    .expect("non-current schema must fail closed");
    assert!(error.to_string().contains("kio repair rebuild-db"));
    assert_eq!(fs::read(&path).unwrap(), before);
}

// ===========================================================================
// §H — registry live-duplicate fail-closed + `--registry-prune` (U46).
// ===========================================================================

/// PB21/22: a scope_id registered against two distinct LIVE `.kio` with
/// DIFFERENT `last_seen_at` (not a tie) is fail-closed with the new
/// `KIO-E-REGISTRY-DUP-001` — the old implementation only caught a
/// `last_seen_at` tie and silently auto-selected the newest otherwise.
#[test]
fn pb21_pb22_live_duplicate_fails_closed_even_without_a_last_seen_tie() {
    let (dir_a, pointer, _) = fixture();
    let scope_id = pointer["scope_id"].as_str().unwrap().to_owned();

    let dir_b = canonical_tempdir();
    kio(&dir_b, &["init"]).arg("--json").assert().success();
    let scope_path = dir_b.path().join(".kio/scope.json");
    let mut scope: Value = serde_json::from_slice(&fs::read(&scope_path).unwrap()).unwrap();
    scope["scope_id"] = serde_json::json!(scope_id);
    fs::write(&scope_path, serde_json::to_vec_pretty(&scope).unwrap()).unwrap();

    let registry = RegistryDb::open(registry_path(&dir_a)).unwrap();
    registry
        .upsert(&RegistryEntry {
            scope_id: scope_id.clone(),
            kio_path: kio_dir(&dir_a).display().to_string(),
            root_path: dir_a.path().display().to_string(),
            participates_in_global_search: true,
            indexed: true,
            last_seen_at: "2020-01-01T00:00:00Z".to_owned(),
        })
        .unwrap();
    registry
        .upsert(&RegistryEntry {
            scope_id: scope_id.clone(),
            kio_path: dir_b.path().join(".kio").display().to_string(),
            root_path: dir_b.path().display().to_string(),
            participates_in_global_search: true,
            indexed: true,
            // Deliberately NEWER, not a tie — the old code silently picked
            // this one; PB21 requires fail-closed regardless.
            last_seen_at: "2099-01-01T00:00:00Z".to_owned(),
        })
        .unwrap();

    let mut orphan_pointer = pointer.clone();
    orphan_pointer["scope_path"] =
        serde_json::json!(dir_a.path().join("gone/.kio").display().to_string());
    let pointer_json = serde_json::to_string(&orphan_pointer).unwrap();
    let (code, output) = run(&dir_a, &["evidence", "verify", &pointer_json]);
    // PB54: registry_duplicate is exit 3 regardless of --strict — a
    // structured status (body still prints), but always retryable-flagged,
    // unlike scope_unreachable's exit-0-without-strict (PB53).
    assert_eq!(
        code, 3,
        "registry_duplicate is exit 3 unconditionally: {output}"
    );
    assert_eq!(output["status"], "registry_duplicate", "{output}");
    assert_eq!(output["error_code"], "KIO-E-REGISTRY-DUP-001", "{output}");

    let batch = batch_file(
        &dir_a,
        "registry-duplicate.jsonl",
        format!("{pointer_json}\n").as_bytes(),
    );
    let batch_output = batch_output(&dir_a, &batch, false);
    assert_eq!(batch_output.status.code(), Some(3));
    let batch: Value = serde_json::from_slice(&batch_output.stdout).unwrap();
    assert_eq!(
        batch["results"][0]["result"]["status"],
        "registry_duplicate"
    );
}

/// PB25: `kio repair registry-prune` deletes only registry rows whose
/// `.kio` is unreachable, never a live (even if duplicated) row.
#[test]
fn pb25_registry_prune_removes_only_unreachable_rows() {
    let (dir, ..) = fixture();
    let registry = RegistryDb::open(registry_path(&dir)).unwrap();
    registry
        .upsert(&RegistryEntry {
            scope_id: "scope_deadbeef".to_owned(),
            kio_path: dir.path().join("nonexistent/.kio").display().to_string(),
            root_path: dir.path().join("nonexistent").display().to_string(),
            participates_in_global_search: true,
            indexed: true,
            last_seen_at: "2020-01-01T00:00:00Z".to_owned(),
        })
        .unwrap();
    assert_eq!(
        registry.all_entries().unwrap().len(),
        2,
        "fixture's own scope + the stale one"
    );

    let output = success(&dir, &["repair", "registry-prune", "--yes"]);
    assert_eq!(output["pruned_count"], 1, "{output}");
    let remaining = registry.all_entries().unwrap();
    assert_eq!(
        remaining.len(),
        1,
        "only the unreachable row must be pruned"
    );
    assert_ne!(
        remaining[0].scope_id, "scope_deadbeef",
        "the live fixture scope must survive; the unreachable stale row must not"
    );
}

// ===========================================================================
// §J — rebuild-db index_metadata (U144, §Z ruling 4).
// ===========================================================================

/// PB28: `kio repair rebuild-db` initializes `index_metadata.
/// last_lifecycle_epoch` to the CURRENT lifecycle-epoch counter value (not
/// the column's `DEFAULT 0`), atomically with the rebuild.
#[test]
fn pb28_rebuild_db_initializes_last_lifecycle_epoch_to_current_value() {
    let (dir, pointer, _) = fixture();
    let raw_hash = pointer["raw_hash"].as_str().unwrap();

    // Bump the lifecycle-epoch counter past 0 via a real, COMPLETE purge, so
    // `last_lifecycle_epoch` would be wrong if left at DEFAULT 0. The
    // working-tree source file must be gone first — otherwise purge reports
    // `purge_incomplete` (its working-tree-alias guard, 09 §5.2) rather than
    // completing, and never advances the tombstone all the way.
    fs::remove_file(dir.path().join("evidence.md")).unwrap();
    success(
        &dir,
        &[
            "purge",
            "--raw-hash",
            raw_hash,
            "--reason",
            "legal",
            "--yes",
        ],
    );
    let before = fs::read_to_string(kio_dir(&dir).join("tombstones/lifecycle-epoch")).unwrap();
    let before: u64 = before.trim().parse().unwrap();
    assert!(
        before > 0,
        "purge must have advanced the lifecycle-epoch counter"
    );

    success(&dir, &["repair", "rebuild-db"]);

    let db_path = kio_dir(&dir).join("index/sqlite.db");
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let metadata = kio_index::fts::read_index_metadata(&conn).unwrap().unwrap();
    assert_eq!(
        metadata.last_lifecycle_epoch, before,
        "must be the current counter value, not DEFAULT 0"
    );
}

// ===========================================================================
// §N — shallow-commit strict downgrade (U51). tree_v1/manifest_missing
// reasons are unreachable this session (deferred prerequisites) and are not
// asserted here.
// ===========================================================================

/// PB39 [regression-lock]: non-strict shallow-commit resolution stays
/// `alive` with `commit_shallow: true` (tree-dependent steps skipped).
#[test]
fn pb39_regression_shallow_commit_resolves_alive_non_strict() {
    let (dir, pointer, _) = fixture();
    make_pointer_commit_final_shallow(&dir, &pointer);
    let pointer_json = serde_json::to_string(&pointer).unwrap();
    let output = success(&dir, &["evidence", "verify", &pointer_json]);
    assert_eq!(output["status"], "alive");
    assert_eq!(output["details"]["commit_shallow"], true);
}

/// PB40/41: `--strict` downgrades a shallow-commit resolution from `alive`
/// to `unverifiable` with `reason: "commit_shallow"`, exit 3 (retryable —
/// distinct from the exit-4 reasons, which are unreachable this session).
#[test]
fn pb40_pb41_strict_shallow_commit_downgrades_to_unverifiable_exit_three() {
    let (dir, pointer, _) = fixture();
    make_pointer_commit_final_shallow(&dir, &pointer);
    let pointer_json = serde_json::to_string(&pointer).unwrap();

    let (code, output) = run(&dir, &["evidence", "verify", &pointer_json, "--strict"]);
    assert_eq!(code, 3, "{output}");
    assert_eq!(output["status"], "unverifiable", "{output}");
    assert_eq!(output["details"]["reason"], "commit_shallow", "{output}");
}

// ===========================================================================
// §O — decisive entry selection (U52), verify side only (open/view side is
// P2-A's `resolve_pointer_for_cli`, out of this session's scope).
// ===========================================================================

/// PB44 (verify side): zero tree entries name the pointer's raw_hash at this
/// commit — short-circuits directly to `KIO-E-STORE-CORRUPT-001`, never
/// consulting the tombstone/erase-receipt markers.
#[test]
fn pb44_verify_side_no_matching_entry_short_circuits_to_store_corrupt() {
    let (dir, mut pointer, _) = fixture();
    pointer["raw_hash"] = serde_json::json!(hash_bytes(
        b"content that was never part of any commit's tree"
    ));
    let pointer_json = serde_json::to_string(&pointer).unwrap();
    let (code, output) = run(&dir, &["evidence", "verify", &pointer_json]);
    assert_eq!(code, 4, "{output}");
    assert_eq!(output["error_code"], "KIO-E-STORE-CORRUPT-001", "{output}");
}

// ===========================================================================
// §P — procedure 6a's unit-status check (U54), the part reachable without
// `normalize.manifest_hash` (§B's deferred prerequisite).
// ===========================================================================

/// PB45 (b): a chunk whose backing unit is not `status: done` in the tree
/// entry's own normalized-instance manifest resolves `not_found` — it did
/// not exist at this commit's point in time.
#[test]
fn pb45_chunk_backed_by_non_done_unit_resolves_not_found() {
    let (dir, mut pointer, _) = fixture();
    pointer["commit"] = Value::String(successor_with_failed_pinned_manifest(&dir, &pointer));

    let pointer_json = serde_json::to_string(&pointer).unwrap();
    let output = success(&dir, &["evidence", "verify", &pointer_json]);
    assert_eq!(output["status"], "not_found", "{output}");
}

/// The manifest path named by a tree is not trustworthy merely because it is
/// present. Point-in-time attribution must reject bytes forged underneath the
/// pinned digest instead of treating its JSON shape as sufficient evidence.
#[test]
fn point_in_time_rejects_forged_pinned_manifest_bytes() {
    let (dir, pointer, _) = fixture();
    let repo = Repository::open(dir.path()).unwrap();
    let commit = repo
        .read_commit(pointer["commit"].as_str().unwrap())
        .unwrap();
    let tree = repo.read_tree(&commit.tree).unwrap();
    let normalize = tree
        .entries
        .iter()
        .find(|entry| entry.raw_hash == pointer["raw_hash"].as_str().unwrap())
        .and_then(|entry| entry.normalize.as_ref())
        .unwrap();
    let store = ObjectStore::new(kio_dir(&dir));
    let manifest_path = store
        .content_path(ContentObjectKind::Manifest, &normalize.manifest_hash)
        .unwrap();
    fs::write(&manifest_path, br#"{"forged":true}"#).unwrap();

    let pointer_json = serde_json::to_string(&pointer).unwrap();
    let (code, output) = run(&dir, &["evidence", "verify", &pointer_json]);
    assert_eq!(code, 4, "{output}");
    assert_eq!(output["error_code"], "KIO-E-STORE-CORRUPT-001", "{output}");
}

/// A Done entry authorizes a pointer only if its pinned immutable normalized
/// unit object remains present and valid. The mutable normalized-unit cache is
/// deliberately irrelevant to this closure check.
#[test]
fn point_in_time_rejects_missing_pinned_normalized_unit() {
    let (dir, pointer, _) = fixture();
    let repo = Repository::open(dir.path()).unwrap();
    let commit = repo
        .read_commit(pointer["commit"].as_str().unwrap())
        .unwrap();
    let tree = repo.read_tree(&commit.tree).unwrap();
    let normalize = tree
        .entries
        .iter()
        .find(|entry| entry.raw_hash == pointer["raw_hash"].as_str().unwrap())
        .and_then(|entry| entry.normalize.as_ref())
        .unwrap();
    let store = ObjectStore::new(kio_dir(&dir));
    let manifest_bytes = store
        .read_content_object_bytes(
            ContentObjectKind::Manifest,
            &normalize.manifest_hash,
            8 * 1024 * 1024,
        )
        .unwrap();
    let manifest: Value = serde_json::from_slice(&manifest_bytes).unwrap();
    let unit_hash = manifest["units"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|unit| unit["unit_object_hash"].as_str())
        .unwrap();
    fs::remove_file(
        store
            .content_path(ContentObjectKind::NormalizedUnit, unit_hash)
            .unwrap(),
    )
    .unwrap();

    let pointer_json = serde_json::to_string(&pointer).unwrap();
    let (code, output) = run(&dir, &["evidence", "verify", &pointer_json]);
    assert_eq!(code, 4, "{output}");
    assert_eq!(output["error_code"], "KIO-E-STORE-CORRUPT-001", "{output}");
}

/// Presence alone is likewise insufficient: byte substitution below a Done
/// entry's immutable unit digest must fail the CAS closure before attribution.
#[test]
fn point_in_time_rejects_forged_pinned_normalized_unit_bytes() {
    let (dir, pointer, _) = fixture();
    let repo = Repository::open(dir.path()).unwrap();
    let commit = repo
        .read_commit(pointer["commit"].as_str().unwrap())
        .unwrap();
    let tree = repo.read_tree(&commit.tree).unwrap();
    let normalize = tree
        .entries
        .iter()
        .find(|entry| entry.raw_hash == pointer["raw_hash"].as_str().unwrap())
        .and_then(|entry| entry.normalize.as_ref())
        .unwrap();
    let store = ObjectStore::new(kio_dir(&dir));
    let manifest_bytes = store
        .read_content_object_bytes(
            ContentObjectKind::Manifest,
            &normalize.manifest_hash,
            8 * 1024 * 1024,
        )
        .unwrap();
    let manifest: Value = serde_json::from_slice(&manifest_bytes).unwrap();
    let unit_hash = manifest["units"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|unit| unit["unit_object_hash"].as_str())
        .unwrap();
    fs::write(
        store
            .content_path(ContentObjectKind::NormalizedUnit, unit_hash)
            .unwrap(),
        br#"{"forged":true}"#,
    )
    .unwrap();

    let pointer_json = serde_json::to_string(&pointer).unwrap();
    let (code, output) = run(&dir, &["evidence", "verify", &pointer_json]);
    assert_eq!(code, 4, "{output}");
    assert_eq!(output["error_code"], "KIO-E-STORE-CORRUPT-001", "{output}");
}

/// R25-10: `reindex --at` reads the PINNED manifest, not the working copy.
///
/// `manifest.json` is defined as the latest working copy (03 §2.1) — a same-gen
/// partial retry rewrites it in place. Historical reindex loaded it and chunked
/// every unit it called `done`, then recorded those chunks' publication under
/// the commit being reindexed. So a unit that only completed AFTER commit `C`
/// became, retroactively, text that existed at `C`, and `search --at C` would
/// return it. The tree entry pinned the right manifest object all along
/// (`normalize.manifest_hash`, tree schema v2); nothing consulted it.
///
/// This is PB45's fixture applied to the enrichment path: freeze a manifest
/// object whose units are NOT done, and require the reindex to believe it.
#[test]
fn pb45_historical_reindex_reads_the_pinned_manifest_not_the_working_copy() {
    let (dir, pointer, _) = fixture();
    // The successor's immutable pinned manifest says the unit was unfinished;
    // the path-named current cache remains Done, exactly as a later same-gen
    // retry can leave it.
    let commit = successor_with_failed_pinned_manifest(&dir, &pointer);

    let chunks_path = kio_dir(&dir).join("index/chunks.jsonl");
    let publication_for_commit = || {
        fs::read_to_string(&chunks_path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .any(|row| row["event"] == "publication" && row["introduction_commit"] == commit)
    };
    assert!(!publication_for_commit());

    let reindex = success(&dir, &["reindex", "--at", &commit]);
    assert_eq!(
        reindex["rebuilt_chunks"], 0,
        "a unit the pinned manifest calls unfinished must not be chunked into \
         this commit: {reindex}"
    );
    assert!(
        !publication_for_commit(),
        "working-copy Done units must not mint publication authority for the pinned Failed manifest"
    );
}

// ===========================================================================
// §S — evidence verify 6-value status union (U57/U53).
// ===========================================================================

/// PB53: `scope_unreachable` is a structured `status`, not a raw command
/// error — exit 0 without `--strict`.
#[test]
fn pb53_scope_unreachable_is_a_structured_status_exit_zero() {
    let dir = canonical_tempdir();
    let pointer = serde_json::json!({
        "schema_version": 1,
        "commit": format!("sha256:{}", "a".repeat(64)),
        "raw_hash": format!("sha256:{}", "b".repeat(64)),
        "tool_profile_hash": format!("sha256:{}", "c".repeat(64)),
        "chunk_hash": format!("sha256:{}", "d".repeat(64)),
        "scope_id": "scope_totally_unregistered_and_unreachable",
    });
    let pointer_json = serde_json::to_string(&pointer).unwrap();
    let temp = TempDir::new_in(dir.path()).unwrap();
    let output = success(&temp, &["evidence", "verify", &pointer_json]);
    assert_eq!(output["status"], "scope_unreachable", "{output}");
}

/// PB57: sqlite.db missing/unavailable is a command-level retryable error
/// (`KIO-E-INDEX-REBUILDING-001`), not a status — regardless of `--strict`.
#[test]
fn pb57_sqlite_unavailable_is_command_level_retryable_error() {
    let (dir, pointer, _) = fixture();
    fs::remove_file(kio_dir(&dir).join("index/sqlite.db")).unwrap();
    let pointer_json = serde_json::to_string(&pointer).unwrap();
    for args in [
        vec!["evidence", "verify", pointer_json.as_str()],
        vec!["evidence", "verify", pointer_json.as_str(), "--strict"],
    ] {
        let (code, output) = run(&dir, &args);
        assert_eq!(code, 3, "{output}");
        assert_eq!(
            output["error_code"], "KIO-E-INDEX-REBUILDING-001",
            "{output}"
        );
    }
    let batch = batch_file(
        &dir,
        "index-rebuilding.jsonl",
        format!("{pointer_json}\n").as_bytes(),
    );
    let output = batch_output(&dir, &batch, false);
    assert_eq!(output.status.code(), Some(3));
    assert!(output.stdout.is_empty(), "no partial batch may publish");
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error_code"], "KIO-E-INDEX-REBUILDING-001");
}

/// PB56: the reason-based exit table — `alive` is 0, `not_found` is 4 (a
/// permanent, not merely retryable, outcome).
#[test]
fn pb56_exit_table_alive_zero_not_found_four() {
    // `alive` -> 0 is already exercised by every other `success(&dir,
    // &["evidence", "verify", ...])` call in this file. This test locks the
    // `not_found` -> 4 half: canonical `erased` + raw absent (LC12) is a
    // genuine `not_found` status (unlike an unmarked absence, which is
    // corruption — pb65_lc14_unmarked_missing_raw_is_store_corrupt).
    let (dir, pointer, _) = fixture();
    let raw_hash = pointer["raw_hash"].as_str().unwrap().to_owned();
    let store = ObjectStore::new(kio_dir(&dir));
    fs::remove_file(store.object_path(ObjectKind::Raw, &raw_hash).unwrap()).unwrap();
    let purge = PurgeState::open(kio_dir(&dir)).unwrap();
    let repo = Repository::open(dir.path()).unwrap();
    let commit_hash = repo.head_commit_hash().unwrap().unwrap();
    purge
        .append_erase_receipt_event(
            &raw_hash,
            kio_core::purge::LifecycleEvent::erased(
                "2026-07-13T00:00:00Z",
                &commit_hash,
                PurgeReason::Legal,
                "user",
                1,
            ),
        )
        .unwrap();
    resync_index_metadata(&dir);
    let pointer_json = serde_json::to_string(&pointer).unwrap();
    let (code, output) = run(&dir, &["evidence", "verify", &pointer_json, "--strict"]);
    assert_eq!(code, 4, "{output}");
    assert_eq!(output["status"], "not_found", "{output}");
}

// ===========================================================================
// §T/§V — regression locks (U58/U61).
// ===========================================================================

/// PB58 [regression-lock]: `kio evidence verify` refuses to evaluate while a
/// purge journal is active for the scope, regardless of whether the pointer's
/// own raw_hash is the journal's target.
#[test]
fn pb58_regression_active_journal_blocks_verify_for_unrelated_raw_hash() {
    let (dir, pointer, _) = fixture();
    let unrelated_raw_hash = hash_bytes(b"a raw_hash the journal does not target");
    let purge = PurgeState::open(kio_dir(&dir)).unwrap();
    purge
        .begin(
            vec![unrelated_raw_hash],
            PurgeReason::Legal,
            TombstoneMode::Default,
            "user",
            "2026-07-13T00:00:00Z",
            1,
            hash_bytes(b"planned purge commit placeholder"),
            hash_bytes(b"planned purge closure placeholder"),
            kio_core::scope::new_ulid(dir.path()),
        )
        .unwrap();
    let pointer_json = serde_json::to_string(&pointer).unwrap();
    let (code, output) = run(&dir, &["evidence", "verify", &pointer_json]);
    assert_eq!(code, 3, "{output}");
    assert_eq!(
        output["error_code"], "KIO-E-PURGE-JOURNAL-ACTIVE-001",
        "{output}"
    );
    let batch = batch_file(
        &dir,
        "active-purge.jsonl",
        format!("{pointer_json}\n").as_bytes(),
    );
    let batch_output = batch_output(&dir, &batch, false);
    assert_eq!(batch_output.status.code(), Some(3));
    assert!(
        batch_output.stdout.is_empty(),
        "no partial batch may publish"
    );
    let error: Value = serde_json::from_slice(&batch_output.stderr).unwrap();
    assert_eq!(error["error_code"], "KIO-E-PURGE-JOURNAL-ACTIVE-001");
}

/// PB62: the typed `evidence verify` leaf has exactly one input form.
#[test]
fn pb62_batch_and_single_are_clap_exactly_one() {
    let (dir, pointer, _) = fixture();
    kio(&dir, &["evidence", "verify", "--json"])
        .assert()
        .code(2);

    let path = batch_file(&dir, "one.jsonl", format!("{pointer}\n").as_bytes());
    kio(
        &dir,
        &[
            "evidence",
            "verify",
            pointer.to_string().as_str(),
            "--batch",
            path.to_str().unwrap(),
            "--json",
        ],
    )
    .assert()
    .code(2);
}

/// PB62: malformed JSONL is rejected before any pointer is evaluated, hence
/// a failing batch never leaks a partial result array to stdout.
#[test]
fn pb62_batch_jsonl_validation_and_limits_are_fail_closed() {
    let (dir, pointer, _) = fixture();
    let valid = serde_json::to_string(&pointer).unwrap();
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty", vec![]),
        ("blank", b"\n".to_vec()),
        ("whitespace", b"  \t\n".to_vec()),
        ("malformed", b"{not-json}\n".to_vec()),
        ("nonobject", b"[]\n".to_vec()),
        (
            "unknown",
            format!(
                "{}\n",
                valid.trim_end_matches('}').to_owned() + ",\"unknown\":true}"
            )
            .into_bytes(),
        ),
        ("invalid-utf8", vec![b'{', 0xff, b'}', b'\n']),
        ("line-limit", vec![b'x'; 64 * 1024 + 1]),
    ];
    for (name, bytes) in cases {
        let path = batch_file(&dir, &format!("{name}.jsonl"), &bytes);
        let output = batch_output(&dir, &path, false);
        assert_eq!(output.status.code(), Some(2), "{name}: {output:?}");
        assert!(output.stdout.is_empty(), "{name} published partial stdout");
    }

    let too_many = format!("{valid}\n").repeat(4097);
    let path = batch_file(&dir, "count-limit.jsonl", too_many.as_bytes());
    let output = batch_output(&dir, &path, false);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());

    // The exact-file cap includes all delimiters. A syntactically invalid body
    // above the cap must still be reported as an input limit, before parsing.
    let path = batch_file(&dir, "file-limit.jsonl", &vec![b'x'; 16 * 1024 * 1024 + 1]);
    let output = batch_output(&dir, &path, false);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());

    let mut many_scopes = String::new();
    for i in 0..257 {
        let mut row = pointer.clone();
        row["scope_id"] = serde_json::json!(format!("scope_batch_limit_{i:03}"));
        many_scopes.push_str(&serde_json::to_string(&row).unwrap());
        many_scopes.push('\n');
    }
    let path = batch_file(&dir, "scope-limit.jsonl", many_scopes.as_bytes());
    let output = batch_output(&dir, &path, false);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
}

#[cfg(unix)]
#[test]
fn pb62_batch_input_must_be_a_single_link_regular_file() {
    use std::os::unix::fs::symlink;

    let (dir, pointer, _) = fixture();
    let original = batch_file(&dir, "real.jsonl", format!("{pointer}\n").as_bytes());
    let symlink_path = dir.path().join("link.jsonl");
    symlink(&original, &symlink_path).unwrap();
    let symlink_output = batch_output(&dir, &symlink_path, false);
    assert_eq!(symlink_output.status.code(), Some(4));
    assert!(symlink_output.stdout.is_empty());

    let hardlink_path = dir.path().join("hardlink.jsonl");
    fs::hard_link(&original, &hardlink_path).unwrap();
    let hardlink_output = batch_output(&dir, &original, false);
    assert_eq!(hardlink_output.status.code(), Some(4));
    assert!(hardlink_output.stdout.is_empty());
}

/// PB62: batch results retain order and duplicates, embed the exact single
/// wire object, and have deterministic bytes for identical input bytes.
#[test]
fn pb62_batch_success_is_versioned_deterministic_and_single_parity() {
    let (dir, alive, _) = fixture();
    let alive_json = serde_json::to_string(&alive).unwrap();
    let single_alive = success(&dir, &["evidence", "verify", &alive_json]);
    let single_alive_bytes = kio(&dir, &["evidence", "verify", &alive_json])
        .arg("--json")
        .output()
        .unwrap()
        .stdout;
    let mut unreachable = alive.clone();
    unreachable["scope_id"] = serde_json::json!("scope_batch_unreachable");
    unreachable["scope_path"] = serde_json::json!("/definitely/not/a/kio");
    let unreachable_json = serde_json::to_string(&unreachable).unwrap();
    let (_, single_unreachable) = run(&dir, &["evidence", "verify", &unreachable_json]);

    let bytes = format!("{alive_json}\n{unreachable_json}\n{alive_json}\n").into_bytes();
    let path = batch_file(&dir, "ordered.jsonl", &bytes);
    let first = batch_output(&dir, &path, false);
    assert_eq!(first.status.code(), Some(0), "{first:?}");
    let second = batch_output(&dir, &path, false);
    assert_eq!(second.status.code(), Some(0));
    assert_eq!(
        first.stdout, second.stdout,
        "batch JSON must be deterministic"
    );
    let output: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(output["schema"], "kio.evidence.batch-verify");
    assert_eq!(output["schema_version"], 1);
    assert_eq!(output["input_sha256"], hash_bytes(&bytes));
    assert_eq!(output["strict"], false);
    assert_eq!(output["results"].as_array().unwrap().len(), 3);
    assert_eq!(output["results"][0]["line"], 1);
    assert_eq!(output["results"][1]["line"], 2);
    assert_eq!(output["results"][2]["line"], 3);
    assert_eq!(output["results"][0]["result"], single_alive);
    assert_eq!(output["results"][1]["result"], single_unreachable);
    assert_eq!(output["results"][2]["result"], single_alive);
    assert_eq!(output["summary"]["total"], 3);
    for status in [
        "alive",
        "tombstoned",
        "not_found",
        "scope_unreachable",
        "unverifiable",
        "registry_duplicate",
    ] {
        assert!(output["summary"]["status_counts"].get(status).is_some());
    }
    assert_eq!(output["summary"]["status_counts"]["alive"], 2);
    assert_eq!(output["summary"]["status_counts"]["scope_unreachable"], 1);
    assert_eq!(output["verified_count"], 3);
    assert_eq!(
        single_alive_bytes
            .strip_suffix(b"\n")
            .unwrap_or(&single_alive_bytes),
        serde_json::to_vec(&output["results"][0]["result"]).unwrap(),
        "the nested batch result is the exact single JSON wire object"
    );
}

/// Distinct, directly validated scope paths remain separate bindings while a
/// single batch preserves the original cross-scope row order.
#[test]
fn pb62_batch_verifies_multiple_scopes_in_input_order() {
    let (dir_a, pointer_a, _) = fixture();
    let (_dir_b, pointer_b, _) = fixture();
    let bytes = format!(
        "{}\n{}\n",
        serde_json::to_string(&pointer_a).unwrap(),
        serde_json::to_string(&pointer_b).unwrap()
    );
    let path = batch_file(&dir_a, "multi-scope.jsonl", bytes.as_bytes());
    let output = batch_output(&dir_a, &path, false);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let body: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(body["results"][0]["result"]["status"], "alive");
    assert_eq!(body["results"][1]["result"]["status"], "alive");
    assert_eq!(
        body["results"][0]["result"]["details"]["scope_id"],
        pointer_a["scope_id"]
    );
    assert_eq!(
        body["results"][1]["result"]["details"]["scope_id"],
        pointer_b["scope_id"]
    );
}

/// Strict aggregate status chooses permanent failures over retryable ones.
#[test]
fn pb62_batch_strict_exit_priority_is_permanent_then_retryable() {
    let (dir, shallow, _) = fixture();
    make_pointer_commit_final_shallow(&dir, &shallow);
    let shallow_json = serde_json::to_string(&shallow).unwrap();
    let retryable = batch_file(
        &dir,
        "retryable.jsonl",
        format!("{shallow_json}\n").as_bytes(),
    );
    let output = batch_output(&dir, &retryable, true);
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let retryable_body: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        retryable_body["results"][0]["result"]["details"]["reason"],
        "commit_shallow"
    );

    let (permanent_dir, permanent, _) = fixture();
    let mut not_found = permanent.clone();
    not_found["commit"] = Value::String(successor_with_failed_pinned_manifest(
        &permanent_dir,
        &permanent,
    ));
    let mut unreachable = permanent;
    unreachable["scope_id"] = serde_json::json!("scope_batch_priority_unreachable");
    unreachable["scope_path"] = serde_json::json!("/definitely/not/a/kio");
    let bytes = format!(
        "{}\n{}\n",
        serde_json::to_string(&unreachable).unwrap(),
        serde_json::to_string(&not_found).unwrap()
    );
    let mixed = batch_file(&permanent_dir, "mixed.jsonl", bytes.as_bytes());
    let output = batch_output(&permanent_dir, &mixed, true);
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let mixed_body: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(mixed_body["results"][1]["result"]["status"], "not_found");
}

/// Each member of the closed six-status union has exactly the same wire result
/// and process-exit semantics when carried as a one-row batch.
#[test]
fn pb62_batch_complete_single_status_and_exit_parity() {
    let (alive_dir, alive, _) = fixture();
    assert_eq!(
        assert_single_batch_parity(&alive_dir, &alive, false, 0)["status"],
        "alive"
    );

    let mut unreachable = alive.clone();
    unreachable["scope_id"] = serde_json::json!("scope_batch_parity_unreachable");
    unreachable["scope_path"] = serde_json::json!("/definitely/not/a/kio");
    assert_eq!(
        assert_single_batch_parity(&alive_dir, &unreachable, false, 0)["status"],
        "scope_unreachable"
    );
    assert_eq!(
        assert_single_batch_parity(&alive_dir, &unreachable, true, 3)["status"],
        "scope_unreachable"
    );

    let (tombstone_dir, tombstone_pointer, _) = fixture();
    let raw_hash = tombstone_pointer["raw_hash"].as_str().unwrap().to_owned();
    success(
        &tombstone_dir,
        &[
            "purge",
            "--raw-hash",
            &raw_hash,
            "--reason",
            "legal",
            "--yes",
        ],
    );
    assert_eq!(
        assert_single_batch_parity(&tombstone_dir, &tombstone_pointer, false, 0)["status"],
        "tombstoned"
    );
    assert_eq!(
        assert_single_batch_parity(&tombstone_dir, &tombstone_pointer, true, 4)["status"],
        "tombstoned"
    );

    let (not_found_dir, mut not_found, _) = fixture();
    not_found["commit"] = Value::String(successor_with_failed_pinned_manifest(
        &not_found_dir,
        &not_found,
    ));
    assert_eq!(
        assert_single_batch_parity(&not_found_dir, &not_found, false, 0)["status"],
        "not_found"
    );
    assert_eq!(
        assert_single_batch_parity(&not_found_dir, &not_found, true, 4)["status"],
        "not_found"
    );

    let (shallow_dir, shallow, _) = fixture();
    make_pointer_commit_final_shallow(&shallow_dir, &shallow);
    let shallow_result = assert_single_batch_parity(&shallow_dir, &shallow, true, 3);
    assert_eq!(shallow_result["status"], "unverifiable");
    assert_eq!(shallow_result["details"]["reason"], "commit_shallow");

    let (registry_dir, mut duplicate, _) = fixture();
    let duplicate_scope = duplicate["scope_id"].as_str().unwrap().to_owned();
    let clone = canonical_tempdir();
    success(&clone, &["init"]);
    let scope_path = clone.path().join(".kio/scope.json");
    let mut scope: Value = serde_json::from_slice(&fs::read(&scope_path).unwrap()).unwrap();
    scope["scope_id"] = serde_json::json!(duplicate_scope);
    fs::write(&scope_path, serde_json::to_vec_pretty(&scope).unwrap()).unwrap();
    let registry = RegistryDb::open(registry_path(&registry_dir)).unwrap();
    registry
        .upsert(&RegistryEntry {
            scope_id: duplicate_scope.clone(),
            kio_path: kio_dir(&registry_dir).display().to_string(),
            root_path: registry_dir.path().display().to_string(),
            participates_in_global_search: true,
            indexed: true,
            last_seen_at: "2020-01-01T00:00:00Z".to_owned(),
        })
        .unwrap();
    registry
        .upsert(&RegistryEntry {
            scope_id: duplicate_scope,
            kio_path: clone.path().join(".kio").display().to_string(),
            root_path: clone.path().display().to_string(),
            participates_in_global_search: true,
            indexed: true,
            last_seen_at: "2099-01-01T00:00:00Z".to_owned(),
        })
        .unwrap();
    duplicate["scope_path"] =
        serde_json::json!(registry_dir.path().join("gone/.kio").display().to_string());
    assert_eq!(
        assert_single_batch_parity(&registry_dir, &duplicate, false, 3)["status"],
        "registry_duplicate"
    );
    assert_eq!(
        assert_single_batch_parity(&registry_dir, &duplicate, true, 3)["status"],
        "registry_duplicate"
    );
}

// ===========================================================================
// §X — canonical validator unification (LC21 principle extended to
// canonical-event aggregation; Phase 1 hand-off).
// ===========================================================================

/// A tombstone with a `retired` tail (via erase-receipt resurrection) that
/// out-epochs a `purged` tombstone tail. Returns the raw_hash and pointer.
fn build_lc10_fixture() -> (TempDir, Value) {
    let (dir, pointer, _) = fixture();
    let raw_hash = pointer["raw_hash"].as_str().unwrap().to_owned();
    let purge = PurgeState::open(kio_dir(&dir)).unwrap();
    let repo = Repository::open(dir.path()).unwrap();
    let commit_hash = repo.head_commit_hash().unwrap().unwrap();
    // Tombstone tail = purged (lifecycle_epoch stamped 1st -> lower).
    purge
        .append_tombstone_event(
            &raw_hash,
            kio_core::purge::LifecycleEvent::purged(
                "2026-07-13T00:00:00Z",
                &commit_hash,
                PurgeReason::Legal,
                "user",
                1,
            ),
        )
        .unwrap();
    // Erase receipt: erased then retired (tail = retired, stamped later ->
    // higher lifecycle_epoch, so canonical_final_event picks this marker).
    purge
        .append_erase_receipt_event(
            &raw_hash,
            kio_core::purge::LifecycleEvent::erased(
                "2026-07-13T00:00:01Z",
                &commit_hash,
                PurgeReason::Legal,
                "user",
                1,
            ),
        )
        .unwrap();
    purge
        .append_erase_receipt_event(
            &raw_hash,
            kio_core::purge::LifecycleEvent::retired("2026-07-13T00:00:02Z", &commit_hash, "user"),
        )
        .unwrap();
    resync_index_metadata(&dir);
    (dir, pointer)
}

/// PB64: the LC10 worked example reproduced via `kio evidence verify` —
/// tombstone tail `purged` + erase-receipt tail `retired` (higher
/// lifecycle_epoch) is canonical `retired`, which resolves `alive` — the
/// resolver never short-circuits on the tombstone's own tail alone.
#[test]
fn pb64_lc10_worked_example_tombstone_purged_receipt_retired_is_alive() {
    let (dir, pointer) = build_lc10_fixture();
    let pointer_json = serde_json::to_string(&pointer).unwrap();
    let output = success(&dir, &["evidence", "verify", &pointer_json]);
    assert_eq!(output["status"], "alive", "{output}");
}

/// PB65 (a) / LC12: canonical `erased`, raw absent — `not_found`,
/// `KIO-E-PURGE-NOT-FOUND-001` (erase receipts are never disclosed as
/// "tombstoned").
#[test]
fn pb65_lc12_canonical_erased_raw_absent_is_not_found() {
    let (dir, pointer, _) = fixture();
    let raw_hash = pointer["raw_hash"].as_str().unwrap().to_owned();
    let store = ObjectStore::new(kio_dir(&dir));
    fs::remove_file(store.object_path(ObjectKind::Raw, &raw_hash).unwrap()).unwrap();
    let purge = PurgeState::open(kio_dir(&dir)).unwrap();
    let repo = Repository::open(dir.path()).unwrap();
    let commit_hash = repo.head_commit_hash().unwrap().unwrap();
    purge
        .append_erase_receipt_event(
            &raw_hash,
            kio_core::purge::LifecycleEvent::erased(
                "2026-07-13T00:00:00Z",
                &commit_hash,
                PurgeReason::Legal,
                "user",
                1,
            ),
        )
        .unwrap();
    resync_index_metadata(&dir);
    let pointer_json = serde_json::to_string(&pointer).unwrap();
    let (code, output) = run(&dir, &["evidence", "verify", &pointer_json]);
    assert_eq!(code, 0, "{output}");
    assert_eq!(output["status"], "not_found", "{output}");
    assert_eq!(
        output["error_code"], "KIO-E-PURGE-NOT-FOUND-001",
        "{output}"
    );
}

/// PB65 (c) / LC14(a): no marker at all, raw absent — genuine corruption
/// (`KIO-E-STORE-CORRUPT-001`), distinct from the expected-absence LC12 code.
#[test]
fn pb65_lc14_unmarked_missing_raw_is_store_corrupt() {
    let (dir, pointer, _) = fixture();
    let raw_hash = pointer["raw_hash"].as_str().unwrap();
    let store = ObjectStore::new(kio_dir(&dir));
    fs::remove_file(store.object_path(ObjectKind::Raw, raw_hash).unwrap()).unwrap();
    let pointer_json = serde_json::to_string(&pointer).unwrap();
    let (code, output) = run(&dir, &["evidence", "verify", &pointer_json]);
    assert_eq!(code, 4, "{output}");
    assert_eq!(output["error_code"], "KIO-E-STORE-CORRUPT-001", "{output}");

    let batch = batch_file(
        &dir,
        "corrupt.jsonl",
        format!("{pointer_json}\n").as_bytes(),
    );
    let batch_output = batch_output(&dir, &batch, false);
    assert_eq!(batch_output.status.code(), Some(4));
    assert!(
        batch_output.stdout.is_empty(),
        "no partial batch may publish"
    );
    let error: Value = serde_json::from_slice(&batch_output.stderr).unwrap();
    assert_eq!(error["error_code"], "KIO-E-STORE-CORRUPT-001");
}

/// PB67 [regression-lock]: a structurally malformed tombstone record is
/// `KIO-E-STORE-CORRUPT-001` identically via `kio repair verify-objects`
/// (a finding) and `kio evidence verify` (a raw command error) — both read
/// through the same low-level `PurgeState::read_tombstone` parse/validate
/// path.
#[test]
fn pb67_regression_malformed_tombstone_is_store_corrupt_in_both_commands() {
    let (dir, pointer, _) = fixture();
    let raw_hash = pointer["raw_hash"].as_str().unwrap();
    let digest = raw_hash.trim_start_matches("sha256:");
    let tombstone_path = kio_dir(&dir)
        .join("tombstones")
        .join(&digest[0..2])
        .join(&digest[2..4])
        .join(digest);
    fs::create_dir_all(tombstone_path.parent().unwrap()).unwrap();
    fs::write(&tombstone_path, b"not valid json").unwrap();

    let (fsck_code, fsck_output) = run(&dir, &["repair", "verify-objects"]);
    assert_eq!(fsck_code, 3, "{fsck_output}");
    assert_eq!(
        fsck_output["error_code"], "KIO-E-STORE-CORRUPT-001",
        "{fsck_output}"
    );

    let pointer_json = serde_json::to_string(&pointer).unwrap();
    let (verify_code, verify_output) = run(&dir, &["evidence", "verify", &pointer_json]);
    assert_ne!(verify_code, 0, "{verify_output}");
    assert_eq!(
        verify_output["error_code"], "KIO-E-STORE-CORRUPT-001",
        "{verify_output}"
    );
}

/// PB68: `kio evidence verify` and `kio open` agree on `status`/`error_code`
/// for the same fixture (structural result of PB66's shared-implementation
/// requirement — `enforce_canonical_marker_barrier` backs both).
#[test]
fn pb68_verify_and_open_agree_on_canonical_erased_raw_absent() {
    let (dir, pointer, _) = fixture();
    let raw_hash = pointer["raw_hash"].as_str().unwrap().to_owned();
    let store = ObjectStore::new(kio_dir(&dir));
    fs::remove_file(store.object_path(ObjectKind::Raw, &raw_hash).unwrap()).unwrap();
    // `open` checks the working tree before CAS (05 §4.2) — remove it too so
    // this genuinely probes the CAS+purge-marker cross-check the pointer
    // resolution path is supposed to take, not the working-tree fast path.
    fs::remove_file(dir.path().join("evidence.md")).unwrap();
    let purge = PurgeState::open(kio_dir(&dir)).unwrap();
    let repo = Repository::open(dir.path()).unwrap();
    let commit_hash = repo.head_commit_hash().unwrap().unwrap();
    purge
        .append_erase_receipt_event(
            &raw_hash,
            kio_core::purge::LifecycleEvent::erased(
                "2026-07-13T00:00:00Z",
                &commit_hash,
                PurgeReason::Legal,
                "user",
                1,
            ),
        )
        .unwrap();
    resync_index_metadata(&dir);

    let pointer_json = serde_json::to_string(&pointer).unwrap();
    let (verify_code, verify_output) = run(&dir, &["evidence", "verify", &pointer_json]);
    assert_eq!(verify_code, 0, "{verify_output}");
    assert_eq!(verify_output["status"], "not_found", "{verify_output}");

    let (open_code, open_output) = run(&dir, &["open", &pointer_json]);
    assert_ne!(open_code, 0, "{open_output}");
    assert_eq!(
        open_output["error_code"], "KIO-E-PURGE-NOT-FOUND-001",
        "verify's not_found <-> open's error_code must agree: {open_output}"
    );
    assert_eq!(verify_output["error_code"], open_output["error_code"]);
}

// ===========================================================================
// §Q — procedure 6b: manifest-missing direct-resolution downgrade and
// resurrection-link fallback (U55).
// ===========================================================================

/// PB48/PB49: an old pointer whose commit precedes a purge (`in_commit`)
/// cannot resolve directly once the purge deletes its manifest object AND
/// `chunk_publications`/`chunk_config_generations` rows (05 §3.5) — but once
/// the raw is resurrected (re-ingested with byte-identical content, same
/// deterministic `--offline` markdownize reproducing the same chunk_hash at
/// gen 0), canonical final event becomes `retired` with a
/// `resurrection_commit`, and 6b's link path re-runs the SAME
/// publication/association ancestry check against that later commit. The old
/// pointer resolves alive again (`manifest_missing: true`) through `open`,
/// `view`, and `evidence verify` alike (PB68's cross-command agreement).
#[test]
fn pb48_pb49_manifest_missing_resolves_via_resurrection_link_after_reingest() {
    let dir = canonical_tempdir();
    fs::write(
        dir.path().join("evidence.md"),
        "# Evidence\n\nTTL is 3600 seconds.\n",
    )
    .unwrap();
    success(&dir, &["init"]);
    success(&dir, &["index", "--offline"]);
    let search = success(&dir, &["search", "3600", "--mode", "text"]);
    let old_pointer = search["results"][0]["evidence_pointer"].clone();
    let old_pointer_json = serde_json::to_string(&old_pointer).unwrap();
    let raw_hash = old_pointer["raw_hash"].as_str().unwrap().to_owned();

    // Confirm the pre-purge pointer resolves normally (sanity baseline).
    let baseline = success(&dir, &["evidence", "verify", &old_pointer_json]);
    assert_eq!(baseline["status"], "alive", "{baseline}");

    success(
        &dir,
        &[
            "purge",
            "--raw-hash",
            &raw_hash,
            "--reason",
            "legal",
            "--yes",
        ],
    );

    // Purged: canonical final event is (still active) `purged` at the
    // raw_hash level, which procedure 5 short-circuits to `tombstoned`
    // regardless of which commit the pointer names -- 6b (commit-scoped)
    // never even runs yet. It starts mattering once resurrection makes the
    // canonical event `retired` below.
    let purged = success(&dir, &["evidence", "verify", &old_pointer_json]);
    assert_eq!(purged["status"], "tombstoned", "{purged}");
    let tombstoned_batch = batch_file(
        &dir,
        "tombstoned.jsonl",
        format!("{old_pointer_json}\n").as_bytes(),
    );
    let tombstoned_output = batch_output(&dir, &tombstoned_batch, false);
    assert_eq!(tombstoned_output.status.code(), Some(0));
    let tombstoned: Value = serde_json::from_slice(&tombstoned_output.stdout).unwrap();
    assert_eq!(tombstoned["results"][0]["result"]["status"], "tombstoned");

    // Kio never deletes the working-tree original (05 §3.5), so the exact
    // same bytes are still sitting in evidence.md -- the next `kio index`
    // re-ingests and resurrects (retires the tombstone) in the same locked
    // mutation, reproducing the identical chunk_hash at gen 0.
    success(&dir, &["index", "--offline"]);

    for args in [
        vec![
            "evidence".to_owned(),
            "verify".to_owned(),
            old_pointer_json.clone(),
        ],
        vec!["open".to_owned(), old_pointer_json.clone()],
        vec!["view".to_owned(), old_pointer_json.clone()],
    ] {
        let args_ref = args.iter().map(String::as_str).collect::<Vec<_>>();
        let output = success(&dir, &args_ref);
        if args_ref[0] == "evidence" {
            assert_eq!(output["status"], "alive", "{output}");
            assert_eq!(
                output["details"]["manifest_missing"], true,
                "resurrection-link resolution must still surface manifest_missing: {output}"
            );
        } else {
            assert_eq!(output["manifest_missing"], true, "{output}");
            assert_eq!(output["commit_shallow"], false, "{output}");
        }
    }

    let batch = batch_file(
        &dir,
        "resurrection-manifest-missing.jsonl",
        format!("{old_pointer_json}\n").as_bytes(),
    );
    let strict_single = kio(&dir, &["evidence", "verify", &old_pointer_json])
        .arg("--strict")
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(strict_single.status.code(), Some(4));
    let strict_single: Value = serde_json::from_slice(&strict_single.stdout).unwrap();
    let batch_output = batch_output(&dir, &batch, true);
    assert_eq!(batch_output.status.code(), Some(4));
    let batch: Value = serde_json::from_slice(&batch_output.stdout).unwrap();
    assert_eq!(batch["results"][0]["result"], strict_single);
    assert_eq!(batch["results"][0]["result"]["status"], "unverifiable");
    assert_eq!(
        batch["results"][0]["result"]["details"]["reason"],
        "manifest_missing"
    );
}

// ===========================================================================
// §O — decisive entry selection (U52), open/view side
// (`resolve_pointer_for_cli`, main.rs).
// ===========================================================================

/// PB42 (open/view side): when the same raw_hash is duplicate-placed in a
/// commit's tree under two different `tool_profile_hash` bindings, selection
/// must bind to the pointer's own `tool_profile_hash` — not to whichever
/// entry a naive `tree.entries.iter().find(...)` happens to reach first.
/// The synthetic alias entry here is a UTF-8-byte-order-earlier path
/// (`a-alias.md` < `evidence.md`) with a DIFFERENT `tool_profile_hash`/`gen`
/// than the pointer's own: before PB42's fix, `resolve_pointer_for_cli`
/// picked the first raw_hash match regardless of binding, mismatched the
/// pointer's `tool_profile_hash` against the alias's, and rejected a
/// perfectly valid pointer as `invalid_pointer_identity` (misreported
/// corruption). `kio open`/`kio view` must succeed, resolving through the
/// correctly-bound `evidence.md` entry instead.
#[test]
fn pb42_open_view_side_binds_to_pointer_tool_profile_hash_not_first_raw_hash_match() {
    let (dir, pointer, _) = fixture();
    let repo = Repository::open(dir.path()).unwrap();
    let head = repo.head_commit_hash().unwrap().unwrap();
    let commit = repo.read_commit(&head).unwrap();
    let tree = repo.read_tree(&commit.tree).unwrap();
    let real_entry = tree
        .entries
        .iter()
        .find(|entry| entry.raw_hash == pointer["raw_hash"].as_str().unwrap())
        .unwrap()
        .clone();
    let real_normalize = real_entry.normalize.clone().unwrap();

    // A byte-order-earlier alias path, same raw_hash, a DIFFERENT (fake)
    // tool_profile_hash/gen binding -- exactly the "duplicate placement"
    // shape PB42 describes, engineered so a naive first-match `.find()`
    // reaches the wrong (alias) entry first.
    let alias_normalize = NormalizeRef {
        tool_profile_hash: hash_bytes(b"a different tool profile entirely"),
        r#gen: real_normalize.r#gen + 7,
        manifest_hash: hash_bytes(b"an unrelated manifest"),
    };
    let mut alias_entry = TreeEntry::raw_file("a-alias.md", real_entry.raw_hash.clone()).unwrap();
    alias_entry.normalize = Some(alias_normalize);
    assert!(alias_entry.path.as_bytes() < real_entry.path.as_bytes());

    let mut entries = tree.entries.clone();
    entries.push(alias_entry);
    let new_tree = build_tree(entries).unwrap();
    let store = ObjectStore::new(kio_dir(&dir));
    let (new_tree_hash, _) = store
        .write_json(ObjectKind::Tree, &serde_json::to_value(&new_tree).unwrap())
        .unwrap();
    let new_commit = CommitObject::new(
        new_tree_hash,
        Some(head),
        "2026-07-21T00:00:00Z".to_owned(),
        "pb42 fixture: duplicate raw_hash placement".to_owned(),
        commit.tool_lock_hash.clone(),
        CommitStats {
            files_added: 1,
            files_modified: 0,
            files_deleted: 0,
        },
        CommitType::Manual,
    )
    .unwrap();
    let (new_commit_hash, _) = store
        .write_json(
            ObjectKind::Commit,
            &serde_json::to_value(&new_commit).unwrap(),
        )
        .unwrap();
    fs::write(kio_dir(&dir).join("HEAD"), format!("{new_commit_hash}\n")).unwrap();

    let mut new_pointer = pointer.clone();
    new_pointer["commit"] = Value::String(new_commit_hash);
    let pointer_json = serde_json::to_string(&new_pointer).unwrap();

    let open_output = success(&dir, &["open", &pointer_json]);
    assert_eq!(open_output["status"], "opened", "{open_output}");
    let view_output = success(&dir, &["view", &pointer_json]);
    assert_eq!(view_output["status"], "viewed", "{view_output}");

    // Cross-command agreement (PB66/68's principle applied to §O): evidence
    // verify resolves the same pointer through the same binding.
    let verify_output = success(&dir, &["evidence", "verify", &pointer_json]);
    assert_eq!(verify_output["status"], "alive", "{verify_output}");
}

/// Evidence's registry is only a cache miss when the registry is genuinely
/// absent.  A valid pointer hint remains usable, and the read-only resolver
/// must not recreate the device cache merely to perform that lookup.
#[test]
fn registry_snapshot_missing_is_a_no_create_cache_miss_with_valid_hint() {
    let (dir, pointer, _) = fixture();
    let registry_dir = registry_path(&dir).parent().unwrap().to_path_buf();
    fs::remove_dir_all(&registry_dir).unwrap();
    assert!(!registry_dir.exists());

    let pointer_text = serde_json::to_string(&pointer).unwrap();
    let output = kio(&dir, &["evidence", "verify", &pointer_text])
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let output: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(output["status"], "alive", "{output}");
    assert!(
        !registry_dir.exists(),
        "EvidenceReadOnly recreated a missing registry cache"
    );
}

/// A valid scope_path must not bypass an unsafe registry leaf.  This is a
/// command-level integrity failure, so neither single nor batch verify emits
/// a result envelope on stdout.
#[cfg(unix)]
#[test]
fn registry_snapshot_unsafe_leaf_fails_closed_without_hint_fallback_or_stdout() {
    let (dir, pointer, _) = fixture();
    let registry = registry_path(&dir);
    let victim = dir.path().join("registry-snapshot-victim");
    let victim_bytes = b"do not follow this registry link";
    fs::write(&victim, victim_bytes).unwrap();
    fs::remove_file(&registry).unwrap();
    std::os::unix::fs::symlink(&victim, &registry).unwrap();
    let pointer_text = serde_json::to_string(&pointer).unwrap();

    let single = kio(&dir, &["evidence", "verify", &pointer_text])
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(single.status.code(), Some(4), "{single:?}");
    assert!(single.stdout.is_empty(), "{single:?}");
    let single_error: Value = serde_json::from_slice(&single.stderr).unwrap();
    assert_eq!(
        single_error["error_code"], "KIO-E-REGISTRY-SNAPSHOT-UNSAFE-001",
        "{single_error}"
    );

    let batch_path = batch_file(
        &dir,
        "unsafe-registry.jsonl",
        format!("{pointer_text}\n").as_bytes(),
    );
    let batch = batch_output(&dir, &batch_path, false);
    assert_eq!(batch.status.code(), Some(4), "{batch:?}");
    assert!(batch.stdout.is_empty(), "{batch:?}");
    let batch_error: Value = serde_json::from_slice(&batch.stderr).unwrap();
    assert_eq!(
        batch_error["error_code"], "KIO-E-REGISTRY-SNAPSHOT-UNSAFE-001",
        "{batch_error}"
    );
    assert_eq!(fs::read(&victim).unwrap(), victim_bytes);
}

/// A snapshot may open and satisfy its pre-query yet still fail the typed
/// candidate query.  That malformed schema is unsafe integrity, never a
/// cache miss that a valid pointer hint can bypass.
#[test]
fn registry_snapshot_malformed_lookup_fails_closed_for_single_and_batch() {
    let (dir, pointer, _) = fixture();
    let registry = registry_path(&dir);
    fs::remove_file(&registry).unwrap();
    let malformed = rusqlite::Connection::open(&registry).unwrap();
    malformed
        .execute_batch("CREATE TABLE scopes(scope_id TEXT)")
        .unwrap();
    drop(malformed);
    let registry_before = fs::read(&registry).unwrap();
    let scope_before = fs::read(kio_dir(&dir).join("scope.json")).unwrap();
    let pointer_text = serde_json::to_string(&pointer).unwrap();

    let single = kio(&dir, &["evidence", "verify", &pointer_text])
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(single.status.code(), Some(4), "{single:?}");
    assert!(single.stdout.is_empty(), "{single:?}");
    let single_error: Value = serde_json::from_slice(&single.stderr).unwrap();
    assert_eq!(
        single_error["error_code"],
        "KIO-E-REGISTRY-SNAPSHOT-UNSAFE-001"
    );
    assert_eq!(single_error["context"]["reason"], "unsafe_integrity");

    let batch_path = batch_file(
        &dir,
        "malformed-registry.jsonl",
        format!("{pointer_text}\n").as_bytes(),
    );
    let batch = batch_output(&dir, &batch_path, false);
    assert_eq!(batch.status.code(), Some(4), "{batch:?}");
    assert!(batch.stdout.is_empty(), "{batch:?}");
    let batch_error: Value = serde_json::from_slice(&batch.stderr).unwrap();
    assert_eq!(
        batch_error["error_code"],
        "KIO-E-REGISTRY-SNAPSHOT-UNSAFE-001"
    );
    assert_eq!(batch_error["context"]["reason"], "unsafe_integrity");
    assert_eq!(fs::read(&registry).unwrap(), registry_before);
    assert_eq!(
        fs::read(kio_dir(&dir).join("scope.json")).unwrap(),
        scope_before
    );
}

#[cfg(unix)]
fn geteuid_is_root() -> bool {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: libc's geteuid has no preconditions and returns the effective
    // uid of this test process.
    unsafe { geteuid() == 0 }
}

#[cfg(unix)]
struct PermissionRestore {
    path: std::path::PathBuf,
    original: std::fs::Permissions,
}

#[cfg(unix)]
impl Drop for PermissionRestore {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.path, self.original.clone());
    }
}

/// A source leaf that cannot be opened is retryable, and a valid hint cannot
/// turn it into an alive result.  Root bypasses mode bits, so that environment
/// cannot exercise this Unix permission boundary.
#[cfg(unix)]
#[test]
fn registry_snapshot_unreadable_leaf_is_retryable_without_hint_fallback() {
    use std::os::unix::fs::PermissionsExt;

    if geteuid_is_root() {
        return;
    }
    let (dir, pointer, _) = fixture();
    let registry = registry_path(&dir);
    let registry_before = fs::read(&registry).unwrap();
    let scope_before = fs::read(kio_dir(&dir).join("scope.json")).unwrap();
    let original = fs::metadata(&registry).unwrap().permissions();
    let _restore = PermissionRestore {
        path: registry.clone(),
        original,
    };
    fs::set_permissions(&registry, fs::Permissions::from_mode(0o000)).unwrap();
    let pointer_text = serde_json::to_string(&pointer).unwrap();

    let single = kio(&dir, &["evidence", "verify", &pointer_text])
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(single.status.code(), Some(3), "{single:?}");
    assert!(single.stdout.is_empty(), "{single:?}");
    let single_error: Value = serde_json::from_slice(&single.stderr).unwrap();
    assert_eq!(single_error["error_code"], "KIO-E-REGISTRY-SNAPSHOT-001");
    assert_eq!(single_error["context"]["reason"], "unstable_or_busy");

    let batch_path = batch_file(
        &dir,
        "unreadable-registry.jsonl",
        format!("{pointer_text}\n").as_bytes(),
    );
    let batch = batch_output(&dir, &batch_path, false);
    assert_eq!(batch.status.code(), Some(3), "{batch:?}");
    assert!(batch.stdout.is_empty(), "{batch:?}");
    let batch_error: Value = serde_json::from_slice(&batch.stderr).unwrap();
    assert_eq!(batch_error["error_code"], "KIO-E-REGISTRY-SNAPSHOT-001");
    assert_eq!(batch_error["context"]["reason"], "unstable_or_busy");

    // Read through the original open descriptor is not possible after mode
    // removal; restore before asserting source bytes are unchanged.
    drop(_restore);
    assert_eq!(fs::read(&registry).unwrap(), registry_before);
    assert_eq!(
        fs::read(kio_dir(&dir).join("scope.json")).unwrap(),
        scope_before
    );
}
