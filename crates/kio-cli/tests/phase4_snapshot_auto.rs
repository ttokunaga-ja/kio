//! Phase 4 scheduled snapshot contract tests.
//!
//! These are deliberately process-level tests: the scheduler must make its
//! authorization decision from the same descriptor-bound state it later
//! publishes through.

mod support;

use support::canonical_tempdir;

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use kio_core::cas::{ObjectKind, ObjectStore, hash_bytes};
use kio_core::dag::CommitType;
use kio_core::scope::Repository;
use serde_json::Value;
use tempfile::TempDir;

const T0: &str = "2026-08-14T00:00:00Z";

fn kio(dir: &TempDir, args: &[&str], now: &str) -> Command {
    let mut command = Command::cargo_bin("kio").unwrap();
    for name in [
        "GEMINI_API_KEY",
        "MISTRAL_API_KEY",
        "KIO_TEST_GEMINI_EMBED",
        "KIO_TEST_MISTRAL_OCR",
        "KIO_TEST_GC_RUNTIME_CHECKPOINTS",
        "KIO_TEST_GC_POST_PUBLICATION_READY",
        "KIO_TEST_GC_FAULT",
        "KIO_TEST_SNAPSHOT_AUTO_AFTER_STATE_WRITE_READY",
        "KIO_TEST_SNAPSHOT_AUTO_AUTHORITY_CAPTURE_READY",
        "KIO_TEST_SNAPSHOT_AUTO_BEFORE_STATE_WRITE_READY",
        "KIO_TEST_SNAPSHOT_AUTO_PRE_GC_PREFLIGHT_READY",
        "KIO_TEST_SNAPSHOT_AUTO_PRELOCK_READY",
        "KIO_TEST_SNAPSHOT_AUTO_LOCKED_READY",
        "KIO_TEST_SNAPSHOT_AUTO_BEFORE_PUBLICATION_READY",
        "KIO_TEST_SNAPSHOT_AUTO_BOUND_LAYOUT_READY",
        "KIO_TEST_SNAPSHOT_AUTO_PRE_CHECKPOINT_READY",
        "KIO_TEST_SNAPSHOT_AUTO_WRITER_BOUNDARY_READY",
        "KIO_TEST_HOLD_LOCK_READY",
        "KIO_TEST_PURGE_FAIL_AFTER_PHASE",
    ] {
        command.env_remove(name);
    }
    command
        .current_dir(dir.path())
        .env("HOME", dir.path().join("home"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env("XDG_CACHE_HOME", dir.path().join("cache"))
        .env("KIO_FIXED_NOW", now)
        .args(args);
    command
}

fn kio_process(dir: &TempDir, args: &[&str], now: &str) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_kio"));
    for name in [
        "GEMINI_API_KEY",
        "MISTRAL_API_KEY",
        "KIO_TEST_GEMINI_EMBED",
        "KIO_TEST_MISTRAL_OCR",
        "KIO_TEST_GC_RUNTIME_CHECKPOINTS",
        "KIO_TEST_GC_POST_PUBLICATION_READY",
        "KIO_TEST_GC_FAULT",
        "KIO_TEST_SNAPSHOT_AUTO_AFTER_STATE_WRITE_READY",
        "KIO_TEST_SNAPSHOT_AUTO_AUTHORITY_CAPTURE_READY",
        "KIO_TEST_SNAPSHOT_AUTO_BEFORE_STATE_WRITE_READY",
        "KIO_TEST_SNAPSHOT_AUTO_PRE_GC_PREFLIGHT_READY",
        "KIO_TEST_SNAPSHOT_AUTO_PRELOCK_READY",
        "KIO_TEST_SNAPSHOT_AUTO_LOCKED_READY",
        "KIO_TEST_SNAPSHOT_AUTO_BEFORE_PUBLICATION_READY",
        "KIO_TEST_SNAPSHOT_AUTO_BOUND_LAYOUT_READY",
        "KIO_TEST_SNAPSHOT_AUTO_PRE_CHECKPOINT_READY",
        "KIO_TEST_SNAPSHOT_AUTO_WRITER_BOUNDARY_READY",
        "KIO_TEST_HOLD_LOCK_READY",
        "KIO_TEST_PURGE_FAIL_AFTER_PHASE",
    ] {
        command.env_remove(name);
    }
    command
        .current_dir(dir.path())
        .env("HOME", dir.path().join("home"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env("XDG_CACHE_HOME", dir.path().join("cache"))
        .env("KIO_FIXED_NOW", now)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(args);
    command
}

fn wait_for_path(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        path.exists(),
        "test barrier was not reached: {}",
        path.display()
    );
}

#[cfg(debug_assertions)]
fn wait_for_path_or_child_exit(path: &Path, mut child: std::process::Child) -> std::process::Child {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if path.exists() {
            return child;
        }
        match child.try_wait() {
            Ok(Some(_)) => {
                let output = child.wait_with_output().unwrap();
                panic!(
                    "child exited before reaching {} (status {}): stdout={} stderr={}",
                    path.display(),
                    output.status,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Ok(None) => {}
            Err(error) => panic!(
                "failed to poll child while waiting for {}: {error}",
                path.display()
            ),
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let output = child.wait_with_output().unwrap();
    panic!(
        "timed out waiting for child to reach {} (child status {}): stdout={} stderr={}",
        path.display(),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(debug_assertions)]
struct HeldLockChild {
    child: Option<std::process::Child>,
    release: PathBuf,
}

#[cfg(debug_assertions)]
impl HeldLockChild {
    fn new(child: std::process::Child, ready: &Path) -> Self {
        Self {
            child: Some(child),
            release: ready.with_extension("release"),
        }
    }

    fn release_and_wait(&mut self) -> std::process::Output {
        fs::write(&self.release, b"release").expect("test lock release marker must be writable");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match self
                .child
                .as_mut()
                .expect("held lock child must be present")
                .try_wait()
            {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => {
                    let mut child = self.child.take().expect("held lock child must be present");
                    let _ = child.kill();
                    let output = child
                        .wait_with_output()
                        .expect("held lock child must be waitable");
                    panic!(
                        "timed out waiting for released lock holder (status {}): stdout={} stderr={}",
                        output.status,
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                Err(error) => panic!("failed to poll released lock holder: {error}"),
            }
        }
        self.child
            .take()
            .expect("held lock child must be present")
            .wait_with_output()
            .expect("held lock child must be waitable")
    }
}

#[cfg(debug_assertions)]
impl Drop for HeldLockChild {
    fn drop(&mut self) {
        let _ = fs::write(&self.release, b"release");
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// Windows retained handles may prevent the attack before Kio observes it.
// Only access-denied/sharing-violation count as that successful protection.
fn attack_succeeded(result: std::io::Result<()>) -> bool {
    match result {
        Ok(()) => true,
        #[cfg(windows)]
        Err(error) if matches!(error.raw_os_error(), Some(5 | 32)) => false,
        Err(error) => panic!("unexpected attack setup failure: {error}"),
    }
}

fn barrier_path(dir: &TempDir, name: &str) -> PathBuf {
    let barriers = dir.path().join("barriers");
    fs::create_dir_all(&barriers).unwrap();
    barriers.join(name)
}

fn output_json(output: &std::process::Output) -> Value {
    serde_json::from_slice(if output.stderr.is_empty() {
        &output.stdout
    } else {
        &output.stderr
    })
    .unwrap()
}

fn kio_bytes(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, at: &Path, result: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(at).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            assert!(!metadata.file_type().is_symlink());
            if metadata.is_dir() {
                walk(root, &path, result);
            } else {
                result.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(&path).unwrap(),
                );
            }
        }
    }
    let mut result = BTreeMap::new();
    walk(root, root, &mut result);
    result
}

fn json(dir: &TempDir, args: &[&str], now: &str) -> Value {
    let output = kio(dir, args, now)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

fn configure(dir: &TempDir, enabled: bool, interval: u64, threshold: u64) {
    fs::write(
        dir.path().join(".kio/config.toml"),
        format!(
            "[snapshot.auto]\nenabled = {enabled}\ninterval_seconds = {interval}\non_change_threshold = {threshold}\n"
        ),
    )
    .unwrap();
}

fn configure_gc(dir: &TempDir, mode: &str, interval: u64, threshold: u64) {
    let gc = match mode {
        "manual_only" => "[gc]\nmode = \"manual_only\"\n".to_owned(),
        "on_idle" => format!(
            "[gc]\nmode = \"on_idle\"\nidle_threshold_seconds = {interval}\nmax_runtime_seconds = 60\n"
        ),
        _ => format!("[gc]\nmode = \"{mode}\"\nmax_runtime_seconds = 60\n"),
    };
    fs::write(
        dir.path().join(".kio/config.toml"),
        format!(
            "{gc}\n[snapshot.auto]\nenabled = true\ninterval_seconds = {interval}\n\
             on_change_threshold = {threshold}\n"
        ),
    )
    .unwrap();
}

fn indexed_fixture(interval: u64, threshold: u64) -> TempDir {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("note.md"), "baseline scheduled snapshot\n").unwrap();
    json(&dir, &["init"], T0);
    json(&dir, &["index", "--offline", "--yes"], T0);
    configure(&dir, true, interval, threshold);
    dir
}

fn auto(dir: &TempDir, now: &str) -> Value {
    json(dir, &["snapshot", "auto"], now)
}

fn head(dir: &TempDir) -> Option<String> {
    Repository::open(dir.path())
        .unwrap()
        .head_commit_hash()
        .unwrap()
}

fn tree_paths(dir: &TempDir) -> Vec<String> {
    let repo = Repository::open(dir.path()).unwrap();
    let hash = head(dir).unwrap();
    let commit = repo.read_commit(&hash).unwrap();
    repo.read_tree(&commit.tree)
        .unwrap()
        .entries
        .into_iter()
        .map(|entry| entry.path)
        .collect()
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn stale_gc_candidate(mode: &str) -> (TempDir, String, String) {
    let old = "2025-01-01T00:00:00Z";
    let dir = canonical_tempdir();
    fs::write(dir.path().join("note.md"), "old scheduled candidate\n").unwrap();
    json(&dir, &["init"], old);
    let first = json(&dir, &["index", "--offline", "--yes"], old);
    let commit = first["commit_hash"].as_str().unwrap().to_owned();
    fs::write(dir.path().join("note.md"), "current ref tip\n").unwrap();
    json(&dir, &["index", "--offline", "--yes"], T0);
    configure_gc(&dir, mode, 60, 99);
    let repo = Repository::open(dir.path()).unwrap();
    let tree = repo.read_commit(&commit).unwrap().tree;
    let plan = json(&dir, &["gc", "--dry-run"], T0);
    assert!(
        plan["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|candidate| candidate["commit_hash"] == commit)
    );
    (dir, commit, tree)
}

#[test]
fn skips_disabled_missing_and_not_indexed_without_mutating_kio() {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("note.md"), "unindexed\n").unwrap();
    json(&dir, &["init"], T0);
    let before = kio_bytes(&dir.path().join(".kio"));
    let missing = auto(&dir, T0);
    assert_eq!(missing["reason"], "disabled");
    assert_eq!(kio_bytes(&dir.path().join(".kio")), before);
    configure(&dir, false, 60, 1);
    let disabled_before = kio_bytes(&dir.path().join(".kio"));
    assert_eq!(auto(&dir, T0)["reason"], "disabled");
    assert_eq!(kio_bytes(&dir.path().join(".kio")), disabled_before);
    configure(&dir, true, 60, 1);
    let unindexed_before = kio_bytes(&dir.path().join(".kio"));
    assert_eq!(auto(&dir, T0)["reason"], "not_indexed");
    assert_eq!(kio_bytes(&dir.path().join(".kio")), unindexed_before);
    assert!(!dir.path().join(".kio/snapshot-auto.json").exists());
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
#[test]
fn eligible_snapshot_fails_before_lock_or_state_on_unsupported_platform() {
    let dir = indexed_fixture(60, 1);
    fs::write(dir.path().join("new.md"), "eligible but unsupported\n").unwrap();
    let before = kio_bytes(&dir.path().join(".kio"));
    let before_head = fs::read(dir.path().join(".kio/HEAD")).unwrap();

    let output = kio(&dir, &["snapshot", "auto", "--json"], T0)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(
        output_json(&output)["error_code"],
        "KIO-E-SNAPSHOT-PLATFORM-UNSUPPORTED-001"
    );
    assert_eq!(fs::read(dir.path().join(".kio/HEAD")).unwrap(), before_head);
    assert_eq!(kio_bytes(&dir.path().join(".kio")), before);
    assert!(!dir.path().join(".kio/.lock").exists());
    assert!(!dir.path().join(".kio/snapshot-auto.json").exists());
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn first_run_noop_advances_canonical_state_and_json_and_human_agree() {
    let dir = indexed_fixture(60, 99);
    let before_head = head(&dir);
    let report = auto(&dir, T0);
    assert_eq!(report["status"], "noop");
    assert_eq!(report["eligibility_reason"], "first_run");
    assert_eq!(report["change_count"], 0);
    assert_eq!(head(&dir), before_head);
    let state = fs::read(dir.path().join(".kio/snapshot-auto.json")).unwrap();
    let state: Value = serde_json::from_slice(&state).unwrap();
    assert_eq!(state["version"], 2);
    assert_eq!(state["idle_observed_since"], T0);
    assert_eq!(state["last_successful_eligible_attempt_at"], T0);
    assert!(
        state["working_set_digest"]
            .as_str()
            .is_some_and(|digest| digest.starts_with("sha256:") && digest.len() == 71)
    );
    let human = kio(&dir, &["snapshot", "auto"], "2026-08-14T00:01:00Z")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let human = String::from_utf8(human).unwrap();
    assert!(human.contains("status: noop"));
    assert!(human.contains("reason: tree_and_tool_lock_unchanged"));
    assert!(human.contains("eligibility_reason: interval_elapsed"));
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn interval_boundaries_and_threshold_boundaries_are_deterministic() {
    let dir = indexed_fixture(60, 2);
    auto(&dir, T0); // establishes the first-run checkpoint
    fs::write(dir.path().join("note.md"), "one change\n").unwrap();
    let skipped_bytes = kio_bytes(&dir.path().join(".kio"));
    let before = auto(&dir, "2026-08-14T00:00:59Z");
    assert_eq!(before["status"], "skipped");
    assert_eq!(before["change_count"], 1);
    assert_eq!(before["next_eligible_at"], "2026-08-14T00:01:00Z");
    assert_eq!(kio_bytes(&dir.path().join(".kio")), skipped_bytes);
    fs::write(dir.path().join("second.md"), "second change\n").unwrap();
    let threshold = auto(&dir, "2026-08-14T00:00:59Z");
    assert_eq!(threshold["status"], "completed");
    assert_eq!(threshold["eligibility_reason"], "change_threshold");

    let equal = auto(&dir, "2026-08-14T00:01:59Z");
    assert_eq!(equal["status"], "noop"); // exactly 60s since threshold attempt
    assert_eq!(equal["eligibility_reason"], "interval_elapsed");
    let after = auto(&dir, "2026-08-14T00:03:00Z");
    assert_eq!(after["status"], "noop");
    assert_eq!(after["eligibility_reason"], "interval_elapsed");
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn scheduled_counts_add_edit_delete_and_rename() {
    let dir = indexed_fixture(3600, 1);
    auto(&dir, T0);
    fs::write(dir.path().join("added.md"), "added\n").unwrap();
    let add = auto(&dir, "2026-08-14T00:00:01Z");
    assert_eq!(add["change_count"], 1);
    assert_eq!(add["stats"]["files_added"], 1);
    fs::write(dir.path().join("added.md"), "edited\n").unwrap();
    assert_eq!(
        auto(&dir, "2026-08-14T00:00:02Z")["stats"]["files_modified"],
        1
    );
    fs::remove_file(dir.path().join("added.md")).unwrap();
    assert_eq!(
        auto(&dir, "2026-08-14T00:00:03Z")["stats"]["files_deleted"],
        1
    );
    fs::rename(dir.path().join("note.md"), dir.path().join("renamed.md")).unwrap();
    let rename = auto(&dir, "2026-08-14T00:00:04Z");
    assert_eq!(rename["change_count"], 2);
    assert_eq!(rename["stats"]["files_added"], 1);
    assert_eq!(rename["stats"]["files_deleted"], 1);
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn ignored_and_tier_a_inputs_do_not_change_snapshot() {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("note.md"), "baseline scheduled snapshot\n").unwrap();
    json(&dir, &["init"], T0);
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[scope]\nignore = [\"ignored.md\"]\n\n[snapshot.auto]\nenabled = true\ninterval_seconds = 3600\non_change_threshold = 1\n",
    )
    .unwrap();
    fs::write(dir.path().join(".kioignore"), "ignored-local.md\n").unwrap();
    json(&dir, &["index", "--offline", "--yes"], T0);
    auto(&dir, T0);
    fs::write(dir.path().join("ignored.md"), "ignore this\n").unwrap();
    fs::write(dir.path().join("ignored-local.md"), "ignore this locally\n").unwrap();
    // This is a built-in Tier A credential-shaped filename and must never be
    // included merely because the scheduled path is non-interactive.
    fs::write(dir.path().join(".env"), "TOKEN=private\n").unwrap();
    let report = auto(&dir, "2026-08-14T00:00:01Z");
    assert_eq!(report["status"], "skipped");
    assert_eq!(report["change_count"], 0);
    let paths = tree_paths(&dir);
    assert!(
        !paths
            .iter()
            .any(|path| { path == "ignored.md" || path == "ignored-local.md" || path == ".env" })
    );
}

#[cfg(unix)]
#[test]
fn symlink_is_rejected_before_snapshot_mutation() {
    use std::os::unix::fs::symlink;
    let dir = indexed_fixture(60, 1);
    auto(&dir, T0);
    let before = head(&dir);
    symlink("note.md", dir.path().join("linked.md")).unwrap();
    let output = kio(
        &dir,
        &["snapshot", "auto", "--json"],
        "2026-08-14T00:00:01Z",
    )
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(4));
    let error: Value = serde_json::from_slice(if output.stderr.is_empty() {
        &output.stdout
    } else {
        &output.stderr
    })
    .unwrap();
    assert_eq!(error["error_code"], "KIO-E-SNAPSHOT-UNSAFE-ENTRY-001");
    assert_eq!(head(&dir), before);
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn hardlinked_working_file_is_rejected_before_snapshot_mutation() {
    let dir = indexed_fixture(60, 1);
    auto(&dir, T0);
    let before_head = head(&dir);
    let before_state = fs::read(dir.path().join(".kio/snapshot-auto.json")).unwrap();
    fs::hard_link(dir.path().join("note.md"), dir.path().join("linked.md")).unwrap();
    let output = kio(
        &dir,
        &["snapshot", "auto", "--json"],
        "2026-08-14T00:01:00Z",
    )
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(
        output_json(&output)["error_code"],
        "KIO-E-SNAPSHOT-UNSAFE-ENTRY-001"
    );
    assert_eq!(head(&dir), before_head);
    assert_eq!(
        fs::read(dir.path().join(".kio/snapshot-auto.json")).unwrap(),
        before_state
    );
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn auto_preserves_existing_normalize_refs_and_never_runs_after_index_gc() {
    let dir = indexed_fixture(60, 1);
    let repo = Repository::open(dir.path()).unwrap();
    let initial = head(&dir).unwrap();
    let initial_commit = repo.read_commit(&initial).unwrap();
    let initial_entry = repo
        .read_tree(&initial_commit.tree)
        .unwrap()
        .entries
        .remove(0);
    assert!(initial_entry.normalize.is_some());
    configure(&dir, true, 60, 1);
    fs::write(dir.path().join("new.md"), "new raw has no normalized CAS\n").unwrap();
    let preserved = auto(&dir, T0);
    assert_eq!(preserved["status"], "completed");
    let preserved_tree = repo
        .read_tree(&repo.read_commit(&head(&dir).unwrap()).unwrap().tree)
        .unwrap();
    let unchanged = preserved_tree
        .entries
        .iter()
        .find(|entry| entry.path == "note.md")
        .unwrap();
    let new = preserved_tree
        .entries
        .iter()
        .find(|entry| entry.path == "new.md")
        .unwrap();
    assert_eq!(unchanged.normalize, initial_entry.normalize);
    assert!(
        new.normalize.is_none(),
        "new raw cannot inherit a normalize ref"
    );

    fs::write(
        dir.path().join("note.md"),
        "changed raw but scheduled preserves no stale normalize\n",
    )
    .unwrap();
    let report = auto(&dir, "2026-08-14T00:00:01Z");
    assert_eq!(report["status"], "completed");
    let current = head(&dir).unwrap();
    let entry = repo
        .read_tree(&repo.read_commit(&current).unwrap().tree)
        .unwrap()
        .entries
        .remove(0);
    assert!(
        entry.normalize.is_none(),
        "changed raw cannot retain normalize ref"
    );
    // The descriptor-bound writer lock may retain its own crash-recovery
    // sentinel under internal/locks. Scheduled publication must not start
    // an after_index sweep: no active marker, receipts, or tree retirement.
    assert!(!dir.path().join(".kio/gc/in_progress").exists());
    assert!(!dir.path().join(".kio/gc/shallowed").exists());
    assert!(!dir.path().join(".kio/gc/internal/trees").exists());
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn clock_rollback_is_retryable_and_does_not_move_state_or_head() {
    let dir = indexed_fixture(60, 99);
    auto(&dir, T0);
    let before_head = head(&dir);
    let state_path = dir.path().join(".kio/snapshot-auto.json");
    let before_state = fs::read(&state_path).unwrap();
    let output = kio(
        &dir,
        &["snapshot", "auto", "--json"],
        "2026-08-13T23:59:59Z",
    )
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        output_json(&output)["error_code"],
        "KIO-E-SNAPSHOT-CLOCK-001"
    );
    assert_eq!(head(&dir), before_head);
    assert_eq!(fs::read(state_path).unwrap(), before_state);
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn tool_lock_only_change_waits_for_interval_then_creates_auto_commit() {
    let dir = indexed_fixture(60, 99);
    auto(&dir, T0);
    let repo = Repository::open(dir.path()).unwrap();
    let before_hash = head(&dir).unwrap();
    let before_commit = repo.read_commit(&before_hash).unwrap();
    let lock_path = dir.path().join(".kio/tool-lock.json");
    let mut lock: Value = serde_json::from_slice(&fs::read(&lock_path).unwrap()).unwrap();
    lock.as_object_mut().unwrap().insert(
        "prepare".to_owned(),
        serde_json::json!({
            "tool_id": "scheduled-test-prepare",
            "profile_hash": format!("sha256:{}", "a".repeat(64)),
        }),
    );
    let mut bytes = serde_json::to_vec(&lock).unwrap();
    bytes.push(b'\n');
    fs::write(&lock_path, bytes).unwrap();

    let early = auto(&dir, "2026-08-14T00:00:59Z");
    assert_eq!(early["status"], "skipped");
    assert_eq!(early["change_count"], 0);
    assert_eq!(head(&dir).as_deref(), Some(before_hash.as_str()));

    let due = auto(&dir, "2026-08-14T00:01:00Z");
    assert_eq!(due["status"], "completed");
    assert_eq!(due["change_count"], 0);
    assert_eq!(due["stats"]["files_added"], 0);
    assert_eq!(due["stats"]["files_modified"], 0);
    assert_eq!(due["stats"]["files_deleted"], 0);
    let after = repo.read_commit(&head(&dir).unwrap()).unwrap();
    assert_eq!(after.tree, before_commit.tree);
    assert_ne!(after.tool_lock_hash, before_commit.tool_lock_hash);
    assert_eq!(after.commit_type, CommitType::Auto);
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn tool_lock_change_at_checkpoint_boundary_cannot_be_reported_as_noop() {
    let dir = indexed_fixture(60, 99);
    let before_head = head(&dir);
    let ready = barrier_path(&dir, "snapshot-tool-lock-checkpoint.ready");
    let mut child = kio_process(&dir, &["snapshot", "auto", "--json"], T0);
    child.env("KIO_TEST_SNAPSHOT_AUTO_PRE_CHECKPOINT_READY", &ready);
    let child = child.spawn().unwrap();
    wait_for_path(&ready);

    let lock_path = dir.path().join(".kio/tool-lock.json");
    let mut lock: Value = serde_json::from_slice(&fs::read(&lock_path).unwrap()).unwrap();
    lock.as_object_mut().unwrap().insert(
        "prepare".to_owned(),
        serde_json::json!({
            "tool_id": "scheduled-race-prepare",
            "profile_hash": format!("sha256:{}", "b".repeat(64)),
        }),
    );
    let mut bytes = serde_json::to_vec(&lock).unwrap();
    bytes.push(b'\n');
    let attacked = attack_succeeded(fs::write(&lock_path, bytes));
    fs::write(ready.with_extension("release"), b"release").unwrap();

    let output = child.wait_with_output().unwrap();
    if !attacked {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(dir.path().join(".kio/snapshot-auto.json").is_file());
        return;
    }
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        output_json(&output)["error_code"],
        "KIO-E-SNAPSHOT-AUTHORITY-CHANGED-001"
    );
    assert_eq!(head(&dir), before_head);
    assert!(!dir.path().join(".kio/snapshot-auto.json").exists());
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn ref_advance_at_checkpoint_boundary_is_preserved_and_rejected() {
    let dir = indexed_fixture(60, 1);
    let original_head = head(&dir).unwrap();
    let original_manifest = fs::read(dir.path().join(".kio/manifest.json")).unwrap();
    fs::write(dir.path().join("alternate.md"), "alternate branch\n").unwrap();
    let alternate = json(
        &dir,
        &["snapshot", "create", "-m", "alternate"],
        "2026-08-14T00:00:01Z",
    )["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::remove_file(dir.path().join("alternate.md")).unwrap();
    fs::write(dir.path().join(".kio/HEAD"), format!("{original_head}\n")).unwrap();
    fs::write(dir.path().join(".kio/manifest.json"), original_manifest).unwrap();
    fs::write(dir.path().join("new.md"), "scheduled candidate\n").unwrap();

    let ready = barrier_path(&dir, "snapshot-ref-checkpoint.ready");
    let mut child = kio_process(&dir, &["snapshot", "auto", "--json"], T0);
    child.env("KIO_TEST_SNAPSHOT_AUTO_PRE_CHECKPOINT_READY", &ready);
    let child = child.spawn().unwrap();
    wait_for_path(&ready);

    let attacked = attack_succeeded(fs::write(
        dir.path().join(".kio/HEAD"),
        format!("{alternate}\n"),
    ));
    fs::write(ready.with_extension("release"), b"release").unwrap();

    let output = child.wait_with_output().unwrap();
    if !attacked {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(dir.path().join(".kio/snapshot-auto.json").is_file());
        return;
    }
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        output_json(&output)["error_code"],
        "KIO-E-SNAPSHOT-AUTHORITY-CHANGED-001"
    );
    assert_eq!(head(&dir).as_deref(), Some(alternate.as_str()));
    assert!(!dir.path().join(".kio/snapshot-auto.json").exists());
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn torn_head_capture_is_rejected_as_authority_change() {
    let dir = indexed_fixture(60, 1);
    fs::write(dir.path().join("new.md"), "eligible torn ref race\n").unwrap();
    let ready = barrier_path(&dir, "snapshot-authority-capture.ready");
    let mut child = kio_process(&dir, &["snapshot", "auto", "--json"], T0);
    child.env("KIO_TEST_SNAPSHOT_AUTO_AUTHORITY_CAPTURE_READY", &ready);
    let child = child.spawn().unwrap();
    wait_for_path(&ready);

    let competing = format!("sha256:{}", "c".repeat(64));
    let attacked = attack_succeeded(fs::write(
        dir.path().join(".kio/HEAD"),
        format!("{competing}\n"),
    ));
    fs::write(ready.with_extension("release"), b"release").unwrap();

    let output = child.wait_with_output().unwrap();
    if !attacked {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(dir.path().join(".kio/snapshot-auto.json").is_file());
        return;
    }
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        output_json(&output)["error_code"],
        "KIO-E-SNAPSHOT-AUTHORITY-CHANGED-001"
    );
    assert_eq!(head(&dir).as_deref(), Some(competing.as_str()));
    assert_eq!(
        fs::read_to_string(dir.path().join(".kio/HEAD")).unwrap(),
        format!("{competing}\n")
    );
    assert!(!dir.path().join(".kio/snapshot-auto.json").exists());
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn malformed_checkpoint_temp_fails_before_snapshot_publication() {
    let dir = indexed_fixture(60, 1);
    let new_bytes = b"must not reach raw CAS\n";
    fs::write(dir.path().join("new.md"), new_bytes).unwrap();
    let before_head = head(&dir);
    let malformed = dir.path().join(".kio/.snapshot-auto-state-123-456");
    fs::write(&malformed, b"not canonical scheduler state\n").unwrap();

    let output = kio(&dir, &["snapshot", "auto", "--json"], T0)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(
        output_json(&output)["error_code"],
        "KIO-E-STORE-CORRUPT-001"
    );
    assert_eq!(head(&dir), before_head);
    assert!(malformed.is_file());
    assert!(!dir.path().join(".kio/snapshot-auto.json").exists());
    assert!(
        !ObjectStore::new(dir.path().join(".kio"))
            .object_path(ObjectKind::Raw, &hash_bytes(new_bytes))
            .unwrap()
            .exists()
    );
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn concurrent_checkpoint_replacement_is_preserved_and_fails_closed() {
    let dir = indexed_fixture(60, 99);
    auto(&dir, T0);
    fs::write(dir.path().join("new.md"), "eligible state race\n").unwrap();
    let before_head = head(&dir);
    let ready = barrier_path(&dir, "snapshot-state-write.ready");
    let mut child = kio_process(
        &dir,
        &["snapshot", "auto", "--json"],
        "2026-08-14T00:01:00Z",
    );
    child.env("KIO_TEST_SNAPSHOT_AUTO_BEFORE_STATE_WRITE_READY", &ready);
    let child = child.spawn().unwrap();
    wait_for_path(&ready);

    let concurrent =
        b"{\"last_successful_eligible_attempt_at\":\"2026-08-14T00:00:30Z\",\"version\":1}\n";
    let replacement = dir.path().join(".kio/snapshot-auto.concurrent");
    let mut file = fs::File::create(&replacement).unwrap();
    file.write_all(concurrent).unwrap();
    file.sync_all().unwrap();
    let attacked = attack_succeeded(fs::rename(
        &replacement,
        dir.path().join(".kio/snapshot-auto.json"),
    ));
    fs::write(ready.with_extension("release"), b"release").unwrap();

    let output = child.wait_with_output().unwrap();
    if !attacked {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(dir.path().join(".kio/snapshot-auto.json").is_file());
        return;
    }
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        output_json(&output)["error_code"],
        "KIO-E-SNAPSHOT-STATE-CHANGED-001"
    );
    assert_eq!(
        fs::read(dir.path().join(".kio/snapshot-auto.json")).unwrap(),
        concurrent
    );
    assert!(fs::read_dir(dir.path().join(".kio")).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".snapshot-auto-state-")
    }));
    assert_eq!(head(&dir), before_head);
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn concurrent_first_checkpoint_publication_is_no_clobber() {
    let dir = indexed_fixture(60, 99);
    fs::write(
        dir.path().join("new.md"),
        "eligible first checkpoint race\n",
    )
    .unwrap();
    let before_head = head(&dir);
    let ready = barrier_path(&dir, "snapshot-first-state-write.ready");
    let mut child = kio_process(&dir, &["snapshot", "auto", "--json"], T0);
    child.env("KIO_TEST_SNAPSHOT_AUTO_BEFORE_STATE_WRITE_READY", &ready);
    let child = child.spawn().unwrap();
    wait_for_path(&ready);

    let concurrent =
        b"{\"last_successful_eligible_attempt_at\":\"2026-08-13T23:59:30Z\",\"version\":1}\n";
    let state = dir.path().join(".kio/snapshot-auto.json");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&state)
        .unwrap();
    file.write_all(concurrent).unwrap();
    file.sync_all().unwrap();
    fs::write(ready.with_extension("release"), b"release").unwrap();

    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        output_json(&output)["error_code"],
        "KIO-E-SNAPSHOT-STATE-CHANGED-001"
    );
    assert_eq!(fs::read(&state).unwrap(), concurrent);
    assert_eq!(head(&dir), before_head);
    assert!(fs::read_dir(dir.path().join(".kio")).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".snapshot-auto-state-")
    }));
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn crash_after_checkpoint_temp_fsync_is_recovered_without_residue() {
    let dir = indexed_fixture(60, 99);
    let ready = barrier_path(&dir, "snapshot-state-crash.ready");
    let mut command = kio_process(&dir, &["snapshot", "auto", "--json"], T0);
    command.env("KIO_TEST_SNAPSHOT_AUTO_BEFORE_STATE_WRITE_READY", &ready);
    let mut child = command.spawn().unwrap();
    wait_for_path(&ready);

    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success());
    assert!(!dir.path().join(".kio/snapshot-auto.json").exists());
    assert!(fs::read_dir(dir.path().join(".kio")).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".snapshot-auto-state-")
    }));

    let recovered = auto(&dir, T0);
    assert_eq!(recovered["status"], "noop");
    assert_eq!(recovered["eligibility_reason"], "first_run");
    assert!(dir.path().join(".kio/snapshot-auto.json").is_file());
    assert!(fs::read_dir(dir.path().join(".kio")).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".snapshot-auto-state-")
    }));
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn crash_after_checkpoint_before_ref_keeps_cooldown_and_head_boundary() {
    let dir = indexed_fixture(60, 99);
    fs::write(dir.path().join("new.md"), "prepared but unreachable\n").unwrap();
    let before_head = head(&dir);
    let ready = barrier_path(&dir, "snapshot-after-state.ready");
    let mut command = kio_process(&dir, &["snapshot", "auto", "--json"], T0);
    command.env("KIO_TEST_SNAPSHOT_AUTO_AFTER_STATE_WRITE_READY", &ready);
    let mut child = command.spawn().unwrap();
    wait_for_path(&ready);

    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    assert_eq!(head(&dir), before_head);
    let state: Value =
        serde_json::from_slice(&fs::read(dir.path().join(".kio/snapshot-auto.json")).unwrap())
            .unwrap();
    assert_eq!(state["version"], 2);
    assert_eq!(state["idle_observed_since"], T0);

    let early = auto(&dir, "2026-08-14T00:00:01Z");
    assert_eq!(early["status"], "skipped");
    assert_eq!(early["reason"], "not_eligible");
    assert_eq!(early["next_eligible_at"], "2026-08-14T00:01:00Z");
    assert_eq!(head(&dir), before_head);

    let due = auto(&dir, "2026-08-14T00:01:00Z");
    assert_eq!(due["status"], "completed");
    assert_ne!(head(&dir), before_head);
}

#[cfg(all(
    debug_assertions,
    any(target_os = "macos", target_os = "linux", windows)
))]
#[test]
fn live_writer_lock_rejects_scheduled_snapshot_without_state_publication() {
    let dir = indexed_fixture(60, 1);
    fs::write(dir.path().join("new.md"), "pending writer race\n").unwrap();
    let mut first = kio_process(
        &dir,
        &["snapshot", "create", "-m", "hold lock", "--json"],
        "2026-08-14T00:00:01Z",
    );
    let ready = barrier_path(&dir, "live-writer-lock.ready");
    first.env("KIO_TEST_HOLD_LOCK_READY", &ready);
    let first = first.spawn().unwrap();
    let mut first = HeldLockChild::new(wait_for_path_or_child_exit(&ready, first), &ready);

    let rejected = kio(
        &dir,
        &["snapshot", "auto", "--json"],
        "2026-08-14T00:00:01Z",
    )
    .output()
    .unwrap();
    let first_output = first.release_and_wait();
    assert_eq!(rejected.status.code(), Some(3));
    assert_eq!(
        output_json(&rejected)["error_code"],
        "KIO-E-STORE-LOCKED-001"
    );
    assert!(!dir.path().join(".kio/snapshot-auto.json").exists());
    assert!(first_output.status.success());
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn config_inode_replacement_at_publication_is_retryable_and_nonpublishing() {
    let dir = indexed_fixture(60, 1);
    fs::write(dir.path().join("new.md"), "eligible config race\n").unwrap();
    let before_head = head(&dir);
    let ready = barrier_path(&dir, "snapshot-config-publication.ready");
    let mut child = kio_process(&dir, &["snapshot", "auto", "--json"], T0);
    child.env("KIO_TEST_SNAPSHOT_AUTO_BEFORE_PUBLICATION_READY", &ready);
    let child = child.spawn().unwrap();
    wait_for_path(&ready);

    let config = dir.path().join(".kio/config.toml");
    let bytes = fs::read(&config).unwrap();
    let replacement = dir.path().join(".kio/config.replacement");
    fs::write(&replacement, bytes).unwrap();
    let attacked = attack_succeeded(fs::rename(&replacement, &config));
    fs::write(ready.with_extension("release"), b"release").unwrap();

    let output = child.wait_with_output().unwrap();
    if !attacked {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(dir.path().join(".kio/snapshot-auto.json").is_file());
        return;
    }
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        output_json(&output)["error_code"],
        "KIO-E-SNAPSHOT-AUTHORITY-CHANGED-001"
    );
    assert_eq!(head(&dir), before_head);
    assert!(!dir.path().join(".kio/snapshot-auto.json").exists());
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn ignore_policy_change_at_writer_boundary_is_rejected_before_cas() {
    let dir = indexed_fixture(60, 1);
    let secret = b"must remain outside immutable history\n";
    fs::write(dir.path().join("secret.md"), secret).unwrap();
    let before_head = head(&dir);
    let ready = barrier_path(&dir, "snapshot-ignore-policy.ready");
    let mut child = kio_process(&dir, &["snapshot", "auto", "--json"], T0);
    child.env("KIO_TEST_SNAPSHOT_AUTO_WRITER_BOUNDARY_READY", &ready);
    let child = child.spawn().unwrap();
    wait_for_path(&ready);

    fs::write(dir.path().join(".kioignore"), "secret.md\n").unwrap();
    fs::write(ready.with_extension("release"), b"release").unwrap();

    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        output_json(&output)["error_code"],
        "KIO-E-SNAPSHOT-AUTHORITY-CHANGED-001"
    );
    assert_eq!(head(&dir), before_head);
    assert!(!dir.path().join(".kio/snapshot-auto.json").exists());
    assert!(
        !ObjectStore::new(dir.path().join(".kio"))
            .object_path(ObjectKind::Raw, &hash_bytes(secret))
            .unwrap()
            .exists()
    );
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn working_file_change_after_final_observation_is_not_published() {
    let dir = indexed_fixture(60, 1);
    fs::write(dir.path().join("new.md"), "observed bytes\n").unwrap();
    let before_head = head(&dir);
    let ready = barrier_path(&dir, "snapshot-before-publication.ready");
    let mut child = kio_process(&dir, &["snapshot", "auto", "--json"], T0);
    child.env("KIO_TEST_SNAPSHOT_AUTO_BEFORE_PUBLICATION_READY", &ready);
    let child = child.spawn().unwrap();
    wait_for_path(&ready);

    let replacement = b"changed after final observation\n";
    let attacked = attack_succeeded(fs::write(dir.path().join("new.md"), replacement));
    fs::write(ready.with_extension("release"), b"release").unwrap();
    let output = child.wait_with_output().unwrap();
    if !attacked {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(dir.path().join(".kio/snapshot-auto.json").is_file());
        let repo = Repository::open(dir.path()).unwrap();
        let commit = repo.read_commit(&head(&dir).unwrap()).unwrap();
        let tree = repo.read_tree(&commit.tree).unwrap();
        assert_eq!(
            tree.entries
                .iter()
                .find(|entry| entry.path == "new.md")
                .unwrap()
                .raw_hash,
            hash_bytes(b"observed bytes\n")
        );
        return;
    }
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        output_json(&output)["error_code"],
        "KIO-E-SNAPSHOT-AUTHORITY-CHANGED-001"
    );
    assert_eq!(head(&dir), before_head);
    assert!(!dir.path().join(".kio/snapshot-auto.json").exists());
    assert!(
        !ObjectStore::new(dir.path().join(".kio"))
            .object_path(ObjectKind::Raw, &hash_bytes(replacement))
            .unwrap()
            .exists()
    );
}

#[cfg(all(unix, any(target_os = "macos", target_os = "linux", windows)))]
#[test]
fn unsafe_symlink_added_at_writer_boundary_is_rejected_before_cas() {
    use std::os::unix::fs::symlink;

    let dir = indexed_fixture(60, 1);
    fs::write(dir.path().join("new.md"), "eligible safe input\n").unwrap();
    let before_head = head(&dir);
    let ready = barrier_path(&dir, "snapshot-writer-symlink.ready");
    let mut child = kio_process(&dir, &["snapshot", "auto", "--json"], T0);
    child.env("KIO_TEST_SNAPSHOT_AUTO_WRITER_BOUNDARY_READY", &ready);
    let child = child.spawn().unwrap();
    wait_for_path(&ready);

    symlink("new.md", dir.path().join("late-link.md")).unwrap();
    fs::write(ready.with_extension("release"), b"release").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(
        output_json(&output)["error_code"],
        "KIO-E-SNAPSHOT-UNSAFE-ENTRY-001"
    );
    assert_eq!(head(&dir), before_head);
    assert!(!dir.path().join(".kio/snapshot-auto.json").exists());
    assert!(
        !ObjectStore::new(dir.path().join(".kio"))
            .object_path(ObjectKind::Raw, &hash_bytes(b"eligible safe input\n"))
            .unwrap()
            .exists()
    );
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn hardlink_added_at_writer_boundary_is_rejected_before_cas() {
    let dir = indexed_fixture(60, 1);
    fs::write(dir.path().join("new.md"), "eligible hardlink race\n").unwrap();
    let before_head = head(&dir);
    let outside = tempfile::tempdir_in(dir.path().parent().unwrap()).unwrap();
    let ready = barrier_path(&dir, "snapshot-writer-hardlink.ready");
    let mut child = kio_process(&dir, &["snapshot", "auto", "--json"], T0);
    child.env("KIO_TEST_SNAPSHOT_AUTO_WRITER_BOUNDARY_READY", &ready);
    let child = child.spawn().unwrap();
    wait_for_path(&ready);

    let attacked = attack_succeeded(fs::hard_link(
        dir.path().join("note.md"),
        outside.path().join("external-note-link"),
    ));
    fs::write(ready.with_extension("release"), b"release").unwrap();
    let output = child.wait_with_output().unwrap();
    if !attacked {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(dir.path().join(".kio/snapshot-auto.json").is_file());
        assert!(!outside.path().join("external-note-link").exists());
        return;
    }
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(
        output_json(&output)["error_code"],
        "KIO-E-SNAPSHOT-UNSAFE-ENTRY-001"
    );
    assert_eq!(head(&dir), before_head);
    assert!(!dir.path().join(".kio/snapshot-auto.json").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn non_utf8_leaf_added_at_writer_boundary_is_rejected_before_cas() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let dir = indexed_fixture(60, 1);
    fs::write(dir.path().join("new.md"), "eligible utf8 input\n").unwrap();
    let before_head = head(&dir);
    let ready = barrier_path(&dir, "snapshot-writer-nonutf8.ready");
    let mut child = kio_process(&dir, &["snapshot", "auto", "--json"], T0);
    child.env("KIO_TEST_SNAPSHOT_AUTO_WRITER_BOUNDARY_READY", &ready);
    let child = child.spawn().unwrap();
    wait_for_path(&ready);

    fs::write(
        dir.path()
            .join(OsString::from_vec(vec![b'l', b'a', b't', b'e', 0xff])),
        b"unsafe name",
    )
    .unwrap();
    fs::write(ready.with_extension("release"), b"release").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(
        output_json(&output)["error_code"],
        "KIO-E-SNAPSHOT-UNSAFE-ENTRY-001"
    );
    assert_eq!(head(&dir), before_head);
    assert!(!dir.path().join(".kio/snapshot-auto.json").exists());
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn scope_replacement_at_writer_boundary_cannot_mutate_either_store() {
    let dir = indexed_fixture(60, 1);
    let victim = indexed_fixture(60, 1);
    let new_bytes = b"eligible retained source\n";
    fs::write(dir.path().join("new.md"), new_bytes).unwrap();
    let before_head = head(&dir).unwrap();
    let victim_before = kio_bytes(&victim.path().join(".kio"));
    let ready = barrier_path(&dir, "snapshot-writer-scope.ready");
    let mut child = kio_process(&dir, &["snapshot", "auto", "--json"], T0);
    child.env("KIO_TEST_SNAPSHOT_AUTO_WRITER_BOUNDARY_READY", &ready);
    let child = child.spawn().unwrap();
    wait_for_path(&ready);

    let retained = dir.path().join(".kio-retained");
    let attacked = attack_succeeded(fs::rename(dir.path().join(".kio"), &retained));
    if !attacked {
        fs::write(ready.with_extension("release"), b"release").unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(kio_bytes(&victim.path().join(".kio")), victim_before);
        assert!(dir.path().join(".kio/snapshot-auto.json").is_file());
        return;
    }
    fs::rename(victim.path().join(".kio"), dir.path().join(".kio")).unwrap();
    fs::write(ready.with_extension("release"), b"release").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(
        output_json(&output)["error_code"],
        "KIO-E-STORE-CORRUPT-001"
    );
    assert_eq!(kio_bytes(&dir.path().join(".kio")), victim_before);
    assert_eq!(
        String::from_utf8(fs::read(retained.join("HEAD")).unwrap()).unwrap(),
        format!("{before_head}\n")
    );
    assert!(!retained.join("snapshot-auto.json").exists());
    assert!(
        !ObjectStore::new(&retained)
            .object_path(ObjectKind::Raw, &hash_bytes(new_bytes))
            .unwrap()
            .exists()
    );
}

#[cfg(all(unix, any(target_os = "macos", target_os = "linux", windows)))]
#[test]
fn object_namespace_replacement_after_binding_cannot_redirect_publication() {
    use std::os::unix::fs::symlink;

    let dir = indexed_fixture(60, 1);
    let victim = indexed_fixture(60, 1);
    fs::write(
        dir.path().join("new.md"),
        "eligible retained object write\n",
    )
    .unwrap();
    let before_head = fs::read(dir.path().join(".kio/HEAD")).unwrap();
    let victim_before = kio_bytes(&victim.path().join(".kio"));
    let original_objects_before = kio_bytes(&dir.path().join(".kio/objects"));
    let ready = barrier_path(&dir, "snapshot-bound-layout.ready");
    let mut child = kio_process(&dir, &["snapshot", "auto", "--json"], T0);
    child.env("KIO_TEST_SNAPSHOT_AUTO_BOUND_LAYOUT_READY", &ready);
    let child = child.spawn().unwrap();
    wait_for_path(&ready);

    let retained_objects = dir.path().join(".kio/objects-retained");
    fs::rename(dir.path().join(".kio/objects"), &retained_objects).unwrap();
    symlink(
        victim.path().join(".kio/objects"),
        dir.path().join(".kio/objects"),
    )
    .unwrap();
    fs::write(ready.with_extension("release"), b"release").unwrap();

    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        output_json(&output)["error_code"],
        "KIO-E-SNAPSHOT-AUTHORITY-CHANGED-001"
    );
    assert_eq!(fs::read(dir.path().join(".kio/HEAD")).unwrap(), before_head);
    assert_eq!(kio_bytes(&victim.path().join(".kio")), victim_before);
    assert_eq!(kio_bytes(&retained_objects), original_objects_before);
    assert!(!dir.path().join(".kio/snapshot-auto.json").exists());
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn scope_replacement_before_lock_cannot_write_the_replacement_store() {
    let dir = indexed_fixture(60, 1);
    fs::write(dir.path().join("new.md"), "eligible scope race\n").unwrap();
    let victim = canonical_tempdir();
    fs::write(victim.path().join("victim.md"), "victim\n").unwrap();
    json(&victim, &["init"], T0);
    let victim_before = kio_bytes(&victim.path().join(".kio"));
    let ready = barrier_path(&dir, "snapshot-prelock.ready");
    let mut child = kio_process(&dir, &["snapshot", "auto", "--json"], T0);
    child.env("KIO_TEST_SNAPSHOT_AUTO_PRELOCK_READY", &ready);
    let child = child.spawn().unwrap();
    wait_for_path(&ready);

    let attacked = attack_succeeded(fs::rename(
        dir.path().join(".kio"),
        dir.path().join(".kio-retained"),
    ));
    if !attacked {
        fs::write(ready.with_extension("release"), b"release").unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(kio_bytes(&victim.path().join(".kio")), victim_before);
        assert!(dir.path().join(".kio/snapshot-auto.json").is_file());
        return;
    }
    fs::rename(victim.path().join(".kio"), dir.path().join(".kio")).unwrap();
    fs::write(ready.with_extension("release"), b"release").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(
        output_json(&output)["error_code"],
        "KIO-E-STORE-CORRUPT-001"
    );
    assert_eq!(kio_bytes(&dir.path().join(".kio")), victim_before);
    assert!(!dir.path().join(".kio/.lock").exists());
    assert!(!dir.path().join(".kio-retained/snapshot-auto.json").exists());
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn scope_replacement_before_gc_preflight_cannot_resume_the_victim_marker() {
    let dir = indexed_fixture(60, 1);
    configure_gc(&dir, "after_index", 60, 1);
    fs::write(dir.path().join("new.md"), "eligible retained source\n").unwrap();

    let (victim, _candidate, victim_tree) = stale_gc_candidate("after_index");
    let interrupted = kio(&victim, &["gc", "--yes", "--json"], T0)
        .env("KIO_TEST_GC_FAULT", "after_marker_fsync")
        .output()
        .unwrap();
    assert!(!interrupted.status.success());
    assert!(victim.path().join(".kio/gc/in_progress").is_file());
    let victim_before = kio_bytes(&victim.path().join(".kio"));

    let ready = barrier_path(&dir, "snapshot-pre-gc.ready");
    let mut child = kio_process(&dir, &["snapshot", "auto", "--json"], T0);
    child.env("KIO_TEST_SNAPSHOT_AUTO_PRE_GC_PREFLIGHT_READY", &ready);
    let child = child.spawn().unwrap();
    wait_for_path(&ready);

    let attacked = attack_succeeded(fs::rename(
        dir.path().join(".kio"),
        dir.path().join(".kio-retained"),
    ));
    if !attacked {
        fs::write(ready.with_extension("release"), b"release").unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(kio_bytes(&victim.path().join(".kio")), victim_before);
        assert!(!dir.path().join(".kio/gc/in_progress").exists());
        return;
    }
    fs::rename(victim.path().join(".kio"), dir.path().join(".kio")).unwrap();
    fs::write(ready.with_extension("release"), b"release").unwrap();

    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(
        output_json(&output)["error_code"],
        "KIO-E-STORE-CORRUPT-001"
    );
    assert_eq!(kio_bytes(&dir.path().join(".kio")), victim_before);
    assert!(dir.path().join(".kio/gc/in_progress").is_file());
    assert!(
        ObjectStore::new(dir.path().join(".kio"))
            .object_path(ObjectKind::Tree, &victim_tree)
            .unwrap()
            .is_file()
    );
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn active_purge_barrier_blocks_raw_republication_and_state_advance() {
    let dir = indexed_fixture(60, 1);
    auto(&dir, T0);
    let repo = Repository::open(dir.path()).unwrap();
    let before_head = head(&dir).unwrap();
    let tree = repo
        .read_tree(&repo.read_commit(&before_head).unwrap().tree)
        .unwrap();
    let raw = tree
        .entries
        .iter()
        .find(|entry| entry.path == "note.md")
        .unwrap()
        .raw_hash
        .clone();
    let original = fs::read(dir.path().join("note.md")).unwrap();
    fs::remove_file(dir.path().join("note.md")).unwrap();
    let interrupted = kio(
        &dir,
        &[
            "purge",
            "--raw-hash",
            &raw,
            "--reason",
            "privacy",
            "--yes",
            "--json",
        ],
        "2026-08-14T00:00:01Z",
    )
    .env("KIO_TEST_PURGE_FAIL_AFTER_PHASE", "tombstoned")
    .output()
    .unwrap();
    assert_eq!(interrupted.status.code(), Some(3));
    assert!(dir.path().join(".kio/purge/in-progress.json").is_file());
    fs::write(dir.path().join("note.md"), original).unwrap();
    let before_state = fs::read(dir.path().join(".kio/snapshot-auto.json")).unwrap();

    let blocked = kio(
        &dir,
        &["snapshot", "auto", "--json"],
        "2026-08-14T00:01:00Z",
    )
    .output()
    .unwrap();
    assert_eq!(blocked.status.code(), Some(3));
    assert_eq!(
        output_json(&blocked)["error_code"],
        "KIO-E-PURGE-INCOMPLETE-001"
    );
    assert_eq!(head(&dir).as_deref(), Some(before_head.as_str()));
    assert_eq!(
        fs::read(dir.path().join(".kio/snapshot-auto.json")).unwrap(),
        before_state
    );
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn purge_journal_created_at_writer_boundary_blocks_before_cas() {
    let dir = indexed_fixture(60, 1);
    let new_bytes = b"eligible purge race\n";
    fs::write(dir.path().join("new.md"), new_bytes).unwrap();
    let before_head = head(&dir);
    let ready = barrier_path(&dir, "snapshot-purge-boundary.ready");
    let mut child = kio_process(&dir, &["snapshot", "auto", "--json"], T0);
    child.env("KIO_TEST_SNAPSHOT_AUTO_WRITER_BOUNDARY_READY", &ready);
    let child = child.spawn().unwrap();
    wait_for_path(&ready);

    let purge = dir.path().join(".kio/purge");
    fs::create_dir_all(&purge).unwrap();
    fs::write(purge.join("in-progress.json"), b"concurrent purge\n").unwrap();
    fs::write(ready.with_extension("release"), b"release").unwrap();

    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        output_json(&output)["error_code"],
        "KIO-E-PURGE-INCOMPLETE-001"
    );
    assert_eq!(head(&dir), before_head);
    assert!(!dir.path().join(".kio/snapshot-auto.json").exists());
    assert!(
        !ObjectStore::new(dir.path().join(".kio"))
            .object_path(ObjectKind::Raw, &hash_bytes(new_bytes))
            .unwrap()
            .exists()
    );
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn disabled_scheduler_does_not_resume_an_after_index_marker() {
    let (dir, _, tree) = stale_gc_candidate("after_index");
    let interrupted = kio(&dir, &["gc", "--yes", "--json"], T0)
        .env("KIO_TEST_GC_FAULT", "after_marker_fsync")
        .output()
        .unwrap();
    assert_eq!(interrupted.status.code(), Some(7));
    let config = dir.path().join(".kio/config.toml");
    let disabled = fs::read_to_string(&config)
        .unwrap()
        .replace("enabled = true", "enabled = false");
    fs::write(&config, disabled).unwrap();
    let before = kio_bytes(&dir.path().join(".kio"));

    let report = auto(&dir, "2026-08-14T00:00:01Z");
    assert_eq!(report["status"], "skipped");
    assert_eq!(report["reason"], "disabled");
    assert_eq!(report["recovered_gc"], false);
    assert_eq!(kio_bytes(&dir.path().join(".kio")), before);
    assert!(
        ObjectStore::new(dir.path().join(".kio"))
            .object_path(ObjectKind::Tree, &tree)
            .unwrap()
            .is_file()
    );
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn active_manual_gc_marker_remains_an_ordinary_writer_barrier() {
    let (dir, _, _) = stale_gc_candidate("manual_only");
    let interrupted = kio(&dir, &["gc", "--yes", "--json"], T0)
        .env("KIO_TEST_GC_FAULT", "after_marker_fsync")
        .output()
        .unwrap();
    assert_eq!(interrupted.status.code(), Some(7));
    let marker = fs::read(dir.path().join(".kio/gc/in_progress")).unwrap();
    let before_head = head(&dir);

    let blocked = kio(&dir, &["snapshot", "auto", "--json"], T0)
        .output()
        .unwrap();
    assert_eq!(blocked.status.code(), Some(3));
    assert_eq!(
        output_json(&blocked)["error_code"],
        "KIO-E-GC-SWEEP-ACTIVE-001"
    );
    assert_eq!(
        fs::read(dir.path().join(".kio/gc/in_progress")).unwrap(),
        marker
    );
    assert_eq!(head(&dir), before_head);
    assert!(!dir.path().join(".kio/snapshot-auto.json").exists());
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn active_after_index_marker_is_recovered_before_scheduled_publication() {
    let (dir, commit, tree) = stale_gc_candidate("after_index");
    let interrupted = kio(&dir, &["gc", "--yes", "--json"], T0)
        .env("KIO_TEST_GC_FAULT", "after_marker_fsync")
        .output()
        .unwrap();
    assert_eq!(interrupted.status.code(), Some(7));
    assert!(dir.path().join(".kio/gc/in_progress").is_file());

    let report = auto(&dir, "2026-08-14T00:00:01Z");
    assert_eq!(report["status"], "noop");
    assert_eq!(report["recovered_gc"], true);
    assert!(!dir.path().join(".kio/gc/in_progress").exists());
    assert!(
        dir.path()
            .join(".kio/gc/shallowed")
            .join(commit.strip_prefix("sha256:").unwrap())
            .is_file()
    );
    assert!(
        !ObjectStore::new(dir.path().join(".kio"))
            .object_path(ObjectKind::Tree, &tree)
            .unwrap()
            .exists()
    );
}

#[cfg(windows)]
fn interrupted_windows_state_exchange(fault: &str) -> (TempDir, String) {
    let dir = indexed_fixture(60, 99);
    auto(&dir, T0);
    let original_head = head(&dir).unwrap();
    fs::write(dir.path().join("new.md"), "pending exchange input\n").unwrap();
    let interrupted = kio(
        &dir,
        &["snapshot", "auto", "--json"],
        "2026-08-14T00:01:00Z",
    )
    .env("KIO_TEST_GC_FAULT", fault)
    .output()
    .unwrap();
    assert_eq!(
        interrupted.status.code(),
        Some(7),
        "{fault}: {}",
        String::from_utf8_lossy(&interrupted.stderr)
    );
    assert_eq!(head(&dir).as_deref(), Some(original_head.as_str()));
    (dir, original_head)
}

#[cfg(windows)]
fn check_windows_state_exchange_recovery(fault: &str) {
    let (dir, original_head) = interrupted_windows_state_exchange(fault);
    let owner = dir.path().join(".kio/gc/internal/snapshot-state-exchange");
    let pending = !fault.ends_with("intent_remove");
    assert_eq!(owner.join("intent.json").is_file(), pending);
    if fault.ends_with("source_capture") {
        assert!(!dir.path().join(".kio/snapshot-auto.json").exists());
    }
    let before = kio_bytes(&dir.path().join(".kio"));
    {
        let session = kio_core::gc::GcSweepSession::bind(dir.path()).unwrap();
        assert_eq!(
            session.has_pending_snapshot_auto_state_exchange().unwrap(),
            pending
        );
        if pending {
            // In particular, the missing-public-state window must not become None.
            assert!(session.snapshot_auto_state().is_err());
        } else {
            assert!(session.snapshot_auto_state().is_ok());
        }
    }
    assert_eq!(kio_bytes(&dir.path().join(".kio")), before);
    if pending {
        for args in [
            &["gc", "--dry-run", "--json"][..],
            &[
                "snapshot",
                "create",
                "-m",
                "blocked ordinary writer",
                "--json",
            ][..],
        ] {
            let output = kio(&dir, args, "2026-08-14T00:01:01Z").output().unwrap();
            assert_eq!(output.status.code(), Some(4));
            assert_eq!(
                output_json(&output)["error_code"],
                "KIO-E-STORE-CORRUPT-001"
            );
            assert_eq!(kio_bytes(&dir.path().join(".kio")), before);
        }
    }
    let recovered = auto(&dir, "2026-08-14T00:01:01Z");
    assert_eq!(recovered["status"], "skipped");
    assert_eq!(recovered["reason"], "not_eligible");
    assert_eq!(recovered["next_eligible_at"], "2026-08-14T00:02:00Z");
    assert_eq!(head(&dir).as_deref(), Some(original_head.as_str()));
    assert!(!owner.join("intent.json").exists());
    assert!(!owner.join("backup").exists());
    let due = auto(&dir, "2026-08-14T00:02:00Z");
    assert_eq!(due["status"], "completed");
    let repo = Repository::open(dir.path()).unwrap();
    let commit = repo.read_commit(&head(&dir).unwrap()).unwrap();
    assert_eq!(commit.parent.as_deref(), Some(original_head.as_str()));
    assert_eq!(commit.commit_type, CommitType::Auto);
}

#[cfg(windows)]
macro_rules! windows_state_exchange_test {
    ($name:ident, $fault:literal) => {
        #[test]
        fn $name() {
            check_windows_state_exchange_recovery($fault);
        }
    };
}

#[cfg(windows)]
windows_state_exchange_test!(
    windows_snapshot_state_recovers_after_intent,
    "after_windows_snapshot_state_exchange_intent"
);
#[cfg(windows)]
windows_state_exchange_test!(
    windows_snapshot_state_recovers_missing_public_after_source_capture,
    "after_windows_snapshot_state_exchange_source_capture"
);
#[cfg(windows)]
windows_state_exchange_test!(
    windows_snapshot_state_recovers_after_target_publish,
    "after_windows_snapshot_state_exchange_target_publish"
);
#[cfg(windows)]
windows_state_exchange_test!(
    windows_snapshot_state_recovers_after_names_complete,
    "after_windows_snapshot_state_exchange_names_complete"
);
#[cfg(windows)]
windows_state_exchange_test!(
    windows_snapshot_state_recovers_after_source_retire,
    "after_windows_snapshot_state_exchange_source_retire"
);
#[cfg(windows)]
windows_state_exchange_test!(
    windows_snapshot_state_recovers_after_intent_remove,
    "after_windows_snapshot_state_exchange_intent_remove"
);

#[cfg(windows)]
#[test]
fn windows_snapshot_state_recovery_rejects_substituted_source_or_target() {
    for substitute_source in [true, false] {
        let (dir, original_head) = interrupted_windows_state_exchange(
            "after_windows_snapshot_state_exchange_source_capture",
        );
        let owner = dir.path().join(".kio/gc/internal/snapshot-state-exchange");
        let intent: Value =
            serde_json::from_slice(&fs::read(owner.join("intent.json")).unwrap()).unwrap();
        let path = if substitute_source {
            owner.join("backup")
        } else {
            dir.path()
                .join(".kio")
                .join(intent["prepared"].as_str().unwrap())
        };
        let bytes = fs::read(&path).unwrap();
        // Keep the old identity allocated, so this is a guaranteed same-byte substitution.
        let parked = canonical_tempdir();
        fs::rename(&path, parked.path().join("original")).unwrap();
        fs::write(&path, bytes).unwrap();
        let before = kio_bytes(&dir.path().join(".kio"));
        let output = kio(
            &dir,
            &["snapshot", "auto", "--json"],
            "2026-08-14T00:01:01Z",
        )
        .output()
        .unwrap();
        assert_eq!(output.status.code(), Some(4));
        assert_eq!(head(&dir).as_deref(), Some(original_head.as_str()));
        assert_eq!(kio_bytes(&dir.path().join(".kio")), before);
        assert!(!dir.path().join(".kio/snapshot-auto.json").exists());
    }
}

#[cfg(windows)]
#[test]
fn windows_snapshot_state_recovery_rechecks_current_policy() {
    let (dir, original_head) =
        interrupted_windows_state_exchange("after_windows_snapshot_state_exchange_source_capture");
    configure(&dir, true, 120, 99);
    let before = kio_bytes(&dir.path().join(".kio"));
    let output = kio(
        &dir,
        &["snapshot", "auto", "--json"],
        "2026-08-14T00:01:01Z",
    )
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(head(&dir).as_deref(), Some(original_head.as_str()));
    assert_eq!(kio_bytes(&dir.path().join(".kio")), before);
    assert!(!dir.path().join(".kio/snapshot-auto.json").exists());
}

#[cfg(windows)]
#[test]
fn windows_snapshot_state_recovery_requires_enabled_current_policy() {
    let (dir, original_head) =
        interrupted_windows_state_exchange("after_windows_snapshot_state_exchange_source_capture");
    let config = dir.path().join(".kio/config.toml");
    let authorized = fs::read(&config).unwrap();
    configure(&dir, false, 60, 99);
    let before = kio_bytes(&dir.path().join(".kio"));
    let output = kio(
        &dir,
        &["snapshot", "auto", "--json"],
        "2026-08-14T00:01:01Z",
    )
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        output_json(&output)["error_code"],
        "KIO-E-SNAPSHOT-AUTHORITY-CHANGED-001"
    );
    assert_eq!(kio_bytes(&dir.path().join(".kio")), before);
    assert_eq!(head(&dir).as_deref(), Some(original_head.as_str()));
    assert!(!dir.path().join(".kio/snapshot-auto.json").exists());
    fs::write(config, authorized).unwrap();
    let recovered = auto(&dir, "2026-08-14T00:01:01Z");
    assert_eq!(recovered["reason"], "not_eligible");
    assert_eq!(recovered["next_eligible_at"], "2026-08-14T00:02:00Z");
}
