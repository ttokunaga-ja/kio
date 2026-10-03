//! Phase 4 milestone 2: receipt-first shallow sweep execution and recovery.
//!
//! Every mutating invocation below is confined to a freshly-created fixture.

// Successful mutation and process-interruption recovery use the same public
// CLI contract on every supported desktop platform.
#![cfg(any(target_os = "macos", target_os = "linux", windows))]

mod support;

use support::canonical_tempdir;

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use kio_core::cas::{ObjectKind, ObjectStore};
use kio_core::dag::{CommitObject, CommitStats, CommitType};
use kio_core::gc::GcInProgressMarker;
use kio_core::scope::Repository;
use rusqlite::Connection;
use serde_json::Value;
use tempfile::TempDir;

const NOW: &str = "2026-08-14T00:00:00Z";
const OLD: &str = "2025-01-01T00:00:00Z";

fn kio(dir: &TempDir, args: &[&str]) -> Command {
    let mut command = Command::cargo_bin("kio").unwrap();
    for name in [
        "GEMINI_API_KEY",
        "MISTRAL_API_KEY",
        "KIO_TEST_GEMINI_EMBED",
        "KIO_TEST_MISTRAL_OCR",
        "KIO_TEST_GC_FAULT",
        "KIO_TEST_GC_INDEX_COPY_READY",
        "KIO_TEST_GC_PRE_QUARANTINE_READY",
        "KIO_TEST_GC_RUNTIME_CHECKPOINTS",
        "KIO_TEST_GC_TREE_QUARANTINE_READY",
    ] {
        command.env_remove(name);
    }
    command
        .current_dir(dir.path())
        .env("HOME", dir.path().join("home"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env("XDG_CACHE_HOME", dir.path().join("cache"))
        .args(args);
    command
}

fn kio_process(dir: &TempDir, args: &[&str]) -> ProcessCommand {
    let mut command = ProcessCommand::new(env!("CARGO_BIN_EXE_kio"));
    for name in [
        "GEMINI_API_KEY",
        "MISTRAL_API_KEY",
        "KIO_TEST_GEMINI_EMBED",
        "KIO_TEST_MISTRAL_OCR",
        "KIO_TEST_GC_FAULT",
        "KIO_TEST_GC_INDEX_COPY_READY",
        "KIO_TEST_GC_PRE_QUARANTINE_READY",
        "KIO_TEST_GC_RUNTIME_CHECKPOINTS",
        "KIO_TEST_GC_TREE_QUARANTINE_READY",
    ] {
        command.env_remove(name);
    }
    command
        .current_dir(dir.path())
        .env("HOME", dir.path().join("home"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env("XDG_CACHE_HOME", dir.path().join("cache"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(args);
    command
}

fn json_success(dir: &TempDir, args: &[&str], now: &str) -> Value {
    let output = kio(dir, args)
        .env("KIO_FIXED_NOW", now)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

fn json_failure(dir: &TempDir, args: &[&str], now: &str, code: i32) -> Value {
    let output = kio(dir, args)
        .env("KIO_FIXED_NOW", now)
        .arg("--json")
        .assert()
        .code(code)
        .get_output()
        .stderr
        .clone();
    serde_json::from_slice(&output).unwrap()
}

/// A reachable, stale auto commit followed by a current ref tip.  The first
/// commit is a genuine planner candidate, never a hand-written plan.
fn candidate_fixture() -> (TempDir, String, String) {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("document.md"), "# old\n\nold candidate\n").unwrap();
    // Keep several current rows so the pagination contract is observable
    // without fabricating an index or a cursor.
    for name in ["second.md", "third.md"] {
        fs::write(
            dir.path().join(name),
            format!("# {name}\n\ncommon pagination needle\n"),
        )
        .unwrap();
    }
    json_success(&dir, &["init"], NOW);
    let old = json_success(&dir, &["index", "--offline", "--yes"], OLD);
    let old_commit = old["commit_hash"].as_str().unwrap().to_owned();
    fs::write(dir.path().join("document.md"), "# current\n\ncurrent tip\n").unwrap();
    json_success(&dir, &["index", "--offline", "--yes"], NOW);
    let repo = Repository::open(dir.path()).unwrap();
    let old_tree = repo.read_commit(&old_commit).unwrap().tree;
    let preview = json_success(&dir, &["gc", "--dry-run"], NOW);
    assert!(
        preview["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|candidate| candidate["commit_hash"] == old_commit)
    );
    (dir, old_commit, old_tree)
}

fn interrupt_after_marker(dir: &TempDir) {
    let interrupted = kio(dir, &["gc", "--yes"])
        .env("KIO_FIXED_NOW", NOW)
        .env("KIO_TEST_GC_FAULT", "after_marker_fsync")
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(
        interrupted.status.code(),
        Some(7),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&interrupted.stdout),
        String::from_utf8_lossy(&interrupted.stderr)
    );
}

fn wait_for_ready(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(6);
    while !path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        path.exists(),
        "child did not reach test barrier: {}",
        path.display()
    );
}

fn tree_path(dir: &TempDir, tree: &str) -> PathBuf {
    ObjectStore::new(dir.path().join(".kio"))
        .object_path(ObjectKind::Tree, tree)
        .unwrap()
}

fn receipt_path(dir: &TempDir, commit: &str) -> PathBuf {
    dir.path()
        .join(".kio/gc/shallowed")
        .join(commit.strip_prefix("sha256:").unwrap())
}

fn tree_archive_path(dir: &TempDir, marker: &GcInProgressMarker, tree: &str) -> PathBuf {
    dir.path().join(".kio/gc/internal/trees").join(format!(
        "{}-{}",
        marker.sweep_id,
        tree.strip_prefix("sha256:").unwrap()
    ))
}

fn objects_except_trees(dir: &TempDir) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, at: &Path, output: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(at).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            assert!(!metadata.file_type().is_symlink());
            if metadata.is_dir() {
                walk(root, &path, output);
            } else {
                output.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let root = dir.path().join(".kio/objects");
    let mut result = BTreeMap::new();
    for entry in fs::read_dir(&root).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() != "trees" {
            walk(&root, &entry.path(), &mut result);
        }
    }
    // These non-CAS state ledgers are explicitly out of sweep scope too.
    for name in ["index/chunks.jsonl", "tool-lock.json", "manifests"] {
        let path = dir.path().join(".kio").join(name);
        if path.is_file() {
            result.insert(PathBuf::from(name), fs::read(path).unwrap());
        } else if path.is_dir() {
            walk(dir.path().join(".kio").as_path(), &path, &mut result);
        }
    }
    result
}

fn regular_file_snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, at: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(at).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            assert!(
                !metadata.file_type().is_symlink(),
                "unexpected fixture symlink: {path:?}"
            );
            if metadata.is_dir() {
                walk(root, &path, out);
            } else if metadata.is_file() {
                out.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut snapshot = BTreeMap::new();
    walk(root, root, &mut snapshot);
    snapshot
}

#[test]
fn real_candidate_sweep_receipts_before_tree_removal_and_preserves_other_objects() {
    let (dir, commit, tree) = candidate_fixture();
    let other_before = objects_except_trees(&dir);
    let output = kio(&dir, &["gc", "--yes"])
        .env("KIO_FIXED_NOW", NOW)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let report: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(report["status"], "completed");
    assert!(!tree_path(&dir, &tree).exists());
    assert!(receipt_path(&dir, &commit).is_file());
    assert!(
        fs::read_to_string(receipt_path(&dir, &commit))
            .unwrap()
            .contains(&tree)
    );
    assert_eq!(objects_except_trees(&dir), other_before);
    assert!(!dir.path().join(".kio/gc/in_progress").exists());
    // The implementation quarantines the verified tree, unlinks the sole
    // name, then truncates the retained descriptor; no byte-bearing archive
    // remains after successful reclamation.
    assert_eq!(
        fs::read_dir(dir.path().join(".kio/gc/internal/trees"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(
        fs::read_dir(dir.path().join(".kio/gc/internal/markers"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn bounded_sweep_checkpoints_only_durable_progress_and_converges() {
    let (dir, commit, tree) = candidate_fixture();
    let mut phases = Vec::new();
    for _ in 0..20 {
        let output = kio(&dir, &["gc", "--yes"])
            .env("KIO_FIXED_NOW", NOW)
            // One durable checkpoint per invocation deterministically forces
            // every resumable boundary without depending on wall-clock speed.
            .env("KIO_TEST_GC_RUNTIME_CHECKPOINTS", "1")
            .arg("--json")
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let report: Value = serde_json::from_slice(&output).unwrap();
        if report["status"] == "completed" {
            assert!(receipt_path(&dir, &commit).exists());
            assert!(!tree_path(&dir, &tree).exists());
            assert!(!dir.path().join(".kio/gc/in_progress").exists());
            assert!(
                phases.len() >= 5,
                "bounded run skipped expected checkpoints"
            );
            return;
        }
        assert_eq!(report["status"], "deferred");
        assert_eq!(report["reason"], "max_runtime_seconds");
        assert_eq!(report["recovery_pending"], true);
        phases.push(report["phase"].as_str().unwrap().to_owned());
        assert!(dir.path().join(".kio/gc/in_progress").exists());
        // A missing tree is never observable before its exact receipt.
        if !tree_path(&dir, &tree).exists() {
            assert!(receipt_path(&dir, &commit).exists());
        }
    }
    panic!("bounded GC did not converge: {phases:?}");
}

#[test]
fn noninteractive_unconfirmed_candidate_rejects_without_marker_or_mutation() {
    let (dir, _commit, tree) = candidate_fixture();
    let before = fs::read(tree_path(&dir, &tree)).unwrap();
    let rejected = kio(&dir, &["gc"])
        .env("KIO_FIXED_NOW", NOW)
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(rejected.status.code(), Some(2));
    assert_eq!(fs::read(tree_path(&dir, &tree)).unwrap(), before);
    assert!(!dir.path().join(".kio/gc").exists());
}

#[test]
fn locked_replan_rejects_preview_changes_without_marker_receipt_or_tree_deletion() {
    for change in ["ref", "config", "tree"] {
        let (dir, old_commit, tree) = candidate_fixture();
        let path = tree_path(&dir, &tree);
        let before_tree = fs::read(&path).unwrap();
        let ready = dir.path().join(format!("gc-prelock-{change}"));
        let child = kio_process(&dir, &["gc", "--yes"])
            .env("KIO_FIXED_NOW", NOW)
            .env("KIO_TEST_GC_PRELOCK_READY", &ready)
            .arg("--json")
            .spawn()
            .unwrap();
        wait_for_ready(&ready);
        match change {
            "ref" => fs::write(dir.path().join(".kio/HEAD"), format!("{old_commit}\n")).unwrap(),
            // A syntactically inert configuration change is still part of the
            // plan's bound truth and must invalidate the preview.
            "config" => {
                let config = dir.path().join(".kio/config.toml");
                fs::OpenOptions::new()
                    .append(true)
                    .open(config)
                    .unwrap()
                    .write_all(b"\n# changed after gc preview\n")
                    .unwrap();
            }
            // A CAS tree replacement is corruption, not a plan change, but it
            // must still stop before marker/receipt publication or unlink.
            "tree" => fs::write(&path, b"replaced tree bytes\n").unwrap(),
            _ => unreachable!(),
        }
        fs::write(ready.with_extension("release"), b"release").unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(!output.status.success(), "{change} unexpectedly swept");
        if change != "tree" {
            let error: Value = serde_json::from_slice(&output.stderr).unwrap();
            assert_eq!(error["error_code"], "KIO-E-GC-PLAN-CHANGED-001", "{change}");
        }
        assert!(!dir.path().join(".kio/gc/in_progress").exists(), "{change}");
        assert!(!receipt_path(&dir, &old_commit).exists(), "{change}");
        assert!(path.exists(), "{change}");
        if change != "tree" {
            assert_eq!(fs::read(&path).unwrap(), before_tree, "{change}");
        }
    }
}

#[cfg(unix)]
#[test]
fn public_scope_replacement_before_bound_lock_never_touches_victim_lock_or_store() {
    use std::os::unix::fs::symlink;

    let (dir, _commit, _tree) = candidate_fixture();
    let victim = canonical_tempdir();
    fs::write(victim.path().join("victim.md"), "do not touch\n").unwrap();
    json_success(&victim, &["init"], NOW);
    let victim_before = regular_file_snapshot(&victim.path().join(".kio"));
    assert!(!victim.path().join(".kio/.lock").exists());

    let ready = dir.path().with_extension("gc-bound-lock-ready");
    let isolated_runtime = canonical_tempdir();
    let child = kio_process(&dir, &["gc", "--yes"])
        .env("KIO_FIXED_NOW", NOW)
        .env("KIO_TEST_GC_PRELOCK_READY", &ready)
        // The diagnostic logger is outside the Kio scope contract. Keep its
        // process-local paths out of the pathname-replacement target so this
        // test proves precisely that victim `.kio` was untouched.
        .env("HOME", isolated_runtime.path().join("home"))
        .env("XDG_CONFIG_HOME", isolated_runtime.path().join("config"))
        .env("XDG_DATA_HOME", isolated_runtime.path().join("data"))
        .env("XDG_CACHE_HOME", isolated_runtime.path().join("cache"))
        .arg("--json")
        .spawn()
        .unwrap();
    wait_for_ready(&ready);

    // Replace the exact pathname retained in the pre-lock preview with a
    // different valid scope.  The executor must reject the no-follow rebind
    // before it can create/remove `.kio/.lock` or write a marker in victim.
    let original = dir.path().to_path_buf();
    let parked = original.with_extension("gc-bound-lock-parked");
    fs::rename(&original, &parked).unwrap();
    symlink(victim.path(), &original).unwrap();
    fs::write(ready.with_extension("release"), b"release").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        !output.status.success(),
        "replacement unexpectedly proceeded"
    );
    assert_eq!(
        regular_file_snapshot(&victim.path().join(".kio")),
        victim_before
    );
    assert!(!victim.path().join(".kio/.lock").exists());

    fs::remove_file(&original).unwrap();
    fs::rename(parked, original).unwrap();
}

#[test]
fn every_fault_point_leaves_a_resumable_receipt_first_state_and_resume_is_idempotent() {
    for point in [
        "after_marker_fsync",
        "after_first_receipt",
        "after_all_receipts_before_tree_delete",
        "after_private_prepare",
        "after_rotation_marker_persist",
        "after_index_exchange",
        "after_temp_cleanup_before_marker_advance",
        "after_tree_quarantine",
        "after_tree_retirement_capture",
        "after_first_tree_delete",
        "after_all_trees_before_final_rotation",
        "after_final_rotation_before_marker_removal",
    ] {
        let (dir, commit, tree) = candidate_fixture();
        let interrupted = kio(&dir, &["gc", "--yes"])
            .env("KIO_FIXED_NOW", NOW)
            .env("KIO_TEST_GC_FAULT", point)
            .arg("--json")
            .output()
            .unwrap();
        assert_eq!(
            interrupted.status.code(),
            Some(7),
            "{point}: stdout={} stderr={}",
            String::from_utf8_lossy(&interrupted.stdout),
            String::from_utf8_lossy(&interrupted.stderr)
        );
        assert!(dir.path().join(".kio/gc/in_progress").exists(), "{point}");
        assert!(
            fs::read_dir(dir.path().join(".kio/index"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".gc-index-")),
            "{point}: private GC index copy escaped into public index namespace"
        );
        if point == "after_private_prepare" {
            let repeated = kio(&dir, &["gc", "--yes"])
                .env("KIO_FIXED_NOW", NOW)
                .env("KIO_TEST_GC_FAULT", point)
                .arg("--json")
                .output()
                .unwrap();
            assert_eq!(repeated.status.code(), Some(7));
            let private = dir.path().join(".kio/gc/internal/index");
            let copies = fs::read_dir(&private)
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".gc-index-")
                })
                .count();
            assert_eq!(
                copies, 1,
                "repeated private preparation must remain bounded"
            );
        }
        if matches!(
            point,
            "after_first_receipt" | "after_all_receipts_before_tree_delete"
        ) {
            assert!(receipt_path(&dir, &commit).exists(), "{point}");
            assert!(tree_path(&dir, &tree).exists(), "{point}");
        }
        let pending = json_success(&dir, &["gc", "--dry-run"], NOW);
        assert_eq!(pending["status"], "recovery_pending", "{point}");
        let completed = json_success(&dir, &["gc", "--yes"], NOW);
        assert_eq!(completed["status"], "completed", "{point}");
        assert!(receipt_path(&dir, &commit).exists(), "{point}");
        assert!(!tree_path(&dir, &tree).exists(), "{point}");
        assert!(!dir.path().join(".kio/gc/in_progress").exists(), "{point}");
        // A completed retry cannot mutate receipts or attempt another removal.
        let rerun = json_success(&dir, &["gc", "--yes"], NOW);
        assert_eq!(rerun["candidate_count"], 0, "{point}");
    }
}

#[cfg(windows)]
fn assert_search_only_appended_access_event(
    before: &BTreeMap<PathBuf, Vec<u8>>,
    mut after: BTreeMap<PathBuf, Vec<u8>>,
    search: &Value,
    point: &str,
) {
    let path = PathBuf::from("logs/access.jsonl");
    let previous = before.get(&path).unwrap();
    let current = after.get(&path).unwrap();
    assert!(
        current.starts_with(previous),
        "{point}: audit prefix changed"
    );
    let appended = &current[previous.len()..];
    let row = appended
        .strip_suffix(b"\n")
        .expect("search must append one newline-terminated audit event");
    assert!(
        !row.contains(&b'\n'),
        "{point}: multiple audit rows appended"
    );
    let event: Value = serde_json::from_slice(row).unwrap();
    assert_eq!(
        event,
        serde_json::json!({
            "ts": NOW,
            "level": "info",
            "code": "KIO-I-SEARCH-ACCESS-001",
            "component": "kio-cli",
            "message": "search access",
            "context": {
                "query": "[redacted]",
                "mode": "text",
                "result_count": search["results"].as_array().unwrap().len(),
            },
        }),
        "{point}: unexpected search access event"
    );
    // Restore only the validated append before comparing every store path/byte.
    after.insert(path, previous.clone());
    assert_eq!(
        &after, before,
        "{point}: search mutated non-audit store state"
    );
}

#[cfg(windows)]
fn assert_windows_exchange_owner_clean(path: &Path) {
    use kio_core::store_dir::{ATOMIC_WORKSPACE_DIR, AtomicWorkspaceState, StoreDirectory};

    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        result => assert!(result.unwrap().is_dir(), "{}", path.display()),
    }
    let owner = StoreDirectory::open(path).unwrap();
    for leaf in ["intent.json", "backup"] {
        assert!(
            !owner.contains_entry(Path::new(leaf)).unwrap(),
            "{}: {leaf} remains",
            path.display()
        );
    }
    let entries = owner.entries(Path::new("")).unwrap();
    for entry in &entries {
        assert_eq!(entry.name, ATOMIC_WORKSPACE_DIR, "{}", path.display());
        assert!(entry.is_directory && !entry.is_regular_file);
        owner.open_directory(Path::new(&entry.name)).unwrap();
    }
    // Atomic removal keeps its permanent gate after retiring the journal.
    assert_eq!(
        owner.inspect_atomic().unwrap(),
        if entries.is_empty() {
            AtomicWorkspaceState::Absent
        } else {
            AtomicWorkspaceState::Clean
        },
        "{}",
        path.display()
    );
    assert!(!kio_core::gc::windows_exchange::inspect_pending(&owner).unwrap());
}

#[cfg(windows)]
#[test]
fn windows_exchange_interruptions_gate_readers_and_writers_and_resume_idempotently() {
    for kind in ["marker", "index"] {
        for step in [
            "intent",
            "source_capture",
            "target_publish",
            "names_complete",
            "source_retire",
            "intent_remove",
        ] {
            let point = format!("after_windows_{kind}_exchange_{step}");
            let (dir, commit, tree) = candidate_fixture();
            let baseline = json_success(
                &dir,
                &[
                    "search",
                    "common pagination needle",
                    "--scope",
                    ".",
                    "--mode",
                    "text",
                    "--limit",
                    "100",
                ],
                NOW,
            );
            let kio_dir = dir.path().join(".kio");
            let owner = kio_dir.join(format!("gc/internal/{kind}-exchange"));
            let interrupted = kio(&dir, &["gc", "--yes", "--json"])
                .env("KIO_FIXED_NOW", NOW)
                .env("KIO_TEST_GC_FAULT", &point)
                .output()
                .unwrap();
            assert_eq!(
                interrupted.status.code(),
                Some(7),
                "{point}: {}",
                String::from_utf8_lossy(&interrupted.stderr)
            );
            let fault: Value = serde_json::from_slice(&interrupted.stderr).unwrap();
            assert_eq!(
                fault["error_code"], "KIO-E-GC-TEST-INTERRUPTED-001",
                "{point}"
            );
            assert_eq!(
                owner.join("intent.json").is_file(),
                step != "intent_remove",
                "{point}"
            );
            if step == "source_capture" {
                assert_eq!(
                    kio_dir.join("gc/in_progress").exists(),
                    kind != "marker",
                    "{point}"
                );
                assert_eq!(
                    kio_dir.join("index/sqlite.db").exists(),
                    kind != "index",
                    "{point}"
                );
            }
            let before = regular_file_snapshot(&kio_dir);
            let head_before = fs::read(kio_dir.join("HEAD")).unwrap();
            let pending = json_success(&dir, &["gc", "--dry-run"], NOW);
            assert_eq!(pending["status"], "recovery_pending", "{point}");
            assert_eq!(
                regular_file_snapshot(&kio_dir),
                before,
                "{point}: dry-run mutated store"
            );
            let unconfirmed = json_failure(&dir, &["gc"], NOW, 2);
            assert_eq!(
                unconfirmed["error_code"], "KIO-E-CONFIG-USAGE-001",
                "{point}"
            );
            assert_eq!(
                regular_file_snapshot(&kio_dir),
                before,
                "{point}: unconfirmed GC mutated store"
            );
            let marker_pending = kind == "marker" && step != "intent_remove";
            let (writer_exit, writer_error) = if marker_pending {
                (4, "KIO-E-STORE-CORRUPT-001")
            } else {
                (3, "KIO-E-GC-SWEEP-ACTIVE-001")
            };
            for args in [
                &["index", "--offline", "--yes"][..],
                &["snapshot", "create"][..],
                &["tag", "exchange-blocked"][..],
            ] {
                let error = json_failure(&dir, args, NOW, writer_exit);
                assert_eq!(error["error_code"], writer_error, "{point}: {args:?}");
                assert_eq!(
                    regular_file_snapshot(&kio_dir),
                    before,
                    "{point}: {args:?} mutated store"
                );
            }
            let search_args = [
                "search",
                "common pagination needle",
                "--scope",
                ".",
                "--mode",
                "text",
                "--limit",
                "100",
            ];
            if marker_pending {
                // The retained marker is not yet stable. The same observer
                // gate rejects search before it can publish any result rows.
                let denied = kio(&dir, &search_args)
                    .env("KIO_FIXED_NOW", NOW)
                    .arg("--json")
                    .output()
                    .unwrap();
                assert_eq!(denied.status.code(), Some(4), "{point}");
                assert!(denied.stdout.is_empty(), "{point}: search published rows");
                let error: Value = serde_json::from_slice(&denied.stderr).unwrap();
                assert_eq!(error["error_code"], "KIO-E-STORE-CORRUPT-001", "{point}");
                assert_eq!(
                    regular_file_snapshot(&kio_dir),
                    before,
                    "{point}: denied search mutated store"
                );
            } else {
                let search = json_success(&dir, &search_args, NOW);
                assert!(
                    search["paging"]["next_cursor"].is_null(),
                    "{point}: {search}"
                );
                assert_eq!(
                    search["gc_recovery_pending"]["next_cursor_suppressed"], true,
                    "{point}: {search}"
                );
                let authorized = baseline["results"].as_array().unwrap();
                assert!(
                    search["results"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|row| authorized.contains(row)),
                    "{point}: exposed a result outside the authorized current baseline: {search}"
                );
                assert_search_only_appended_access_event(
                    &before,
                    regular_file_snapshot(&kio_dir),
                    &search,
                    &point,
                );
            }
            let completed = json_success(&dir, &["gc", "--yes"], NOW);
            assert_eq!(completed["status"], "completed", "{point}");
            assert_eq!(
                fs::read(kio_dir.join("HEAD")).unwrap(),
                head_before,
                "{point}"
            );
            assert!(!tree_path(&dir, &tree).exists(), "{point}");
            let receipts = kio_core::gc::read_shallow_receipts(&kio_dir).unwrap();
            assert_eq!(receipts.len(), 1, "{point}");
            assert_eq!(receipts[&commit].tree_hash, tree, "{point}");
            assert!(!kio_dir.join("gc/in_progress").exists(), "{point}");
            for owner_kind in ["marker", "index"] {
                let owner = kio_dir.join(format!("gc/internal/{owner_kind}-exchange"));
                assert_windows_exchange_owner_clean(&owner);
            }
            let completed_state = regular_file_snapshot(&kio_dir);
            let repeated = json_success(&dir, &["gc", "--yes"], NOW);
            assert_eq!(repeated["candidate_count"], 0, "{point}");
            assert_eq!(
                regular_file_snapshot(&kio_dir),
                completed_state,
                "{point}: repeat mutated store"
            );
        }
    }
}

/// Windows sharing exclusions must reject the attack at the native operation,
/// not merely make a later integrity check fail with an unrelated error.
#[cfg(windows)]
fn assert_windows_mutation_denied(error: std::io::Error) {
    assert!(
        matches!(error.raw_os_error(), Some(5 | 32)),
        "expected access/sharing denial, got {error:?}"
    );
}

#[cfg(windows)]
#[test]
fn windows_retained_index_denies_writes_in_copy_and_tree_permit_windows() {
    for barrier in [
        "KIO_TEST_GC_INDEX_COPY_READY",
        "KIO_TEST_GC_PRE_QUARANTINE_READY",
    ] {
        let (dir, commit, tree) = candidate_fixture();
        let ready = dir.path().join("retained-index-ready");
        let child = kio_process(&dir, &["gc", "--yes", "--json"])
            .env("KIO_FIXED_NOW", NOW)
            .env(barrier, &ready)
            .spawn()
            .unwrap();
        wait_for_ready(&ready);
        let sqlite = dir.path().join(".kio/index/sqlite.db");
        let before = fs::read(&sqlite).unwrap();
        assert_windows_mutation_denied(
            fs::OpenOptions::new()
                .write(true)
                .open(&sqlite)
                .unwrap_err(),
        );
        assert_eq!(fs::read(&sqlite).unwrap(), before);
        fs::write(ready.with_extension("release"), b"release").unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{barrier}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!tree_path(&dir, &tree).exists());
        assert_eq!(
            kio_core::gc::read_shallow_receipts(&dir.path().join(".kio")).unwrap()[&commit]
                .tree_hash,
            tree
        );
    }
}

#[cfg(windows)]
#[test]
fn windows_retained_quarantine_denies_rename_and_preserves_hardlink_aliases() {
    for attack in ["rename", "hardlink"] {
        let (dir, commit, tree) = candidate_fixture();
        let before = fs::read(tree_path(&dir, &tree)).unwrap();
        assert_eq!(kio_core::cas::hash_bytes(&before), tree);
        let ready = dir.path().join("retained-tree-ready");
        let child = kio_process(&dir, &["gc", "--yes", "--json"])
            .env("KIO_FIXED_NOW", NOW)
            .env("KIO_TEST_GC_TREE_QUARANTINE_READY", &ready)
            .spawn()
            .unwrap();
        wait_for_ready(&ready);
        let marker_path = dir.path().join(".kio/gc/in_progress");
        let marker_bytes = fs::read(&marker_path).unwrap();
        let marker = GcInProgressMarker::parse_canonical(&marker_bytes).unwrap();
        let archive = tree_archive_path(&dir, &marker, &tree);
        assert_eq!(fs::read(&archive).unwrap(), before);
        let victim = dir.path().join("foreign-victim");
        fs::write(&victim, b"foreign victim must survive\n").unwrap();
        let destination = dir.path().join("attacker-tree");
        let result = if attack == "rename" {
            fs::rename(&archive, &destination)
        } else {
            fs::hard_link(&archive, &destination)
        };
        let aliased = match result {
            Ok(()) => {
                assert_eq!(attack, "hardlink", "retained HANDLE must deny rename");
                assert_eq!(fs::read(&destination).unwrap(), before);
                true
            }
            Err(error) => {
                assert_windows_mutation_denied(error);
                assert!(!destination.exists());
                false
            }
        };
        assert_eq!(fs::read(&archive).unwrap(), before);
        fs::write(ready.with_extension("release"), b"release").unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"foreign victim must survive\n");
        assert!(!tree_path(&dir, &tree).exists());
        if aliased {
            assert_eq!(output.status.code(), Some(4));
            let error: Value = serde_json::from_slice(&output.stderr).unwrap();
            assert_eq!(error["error_code"], "KIO-E-STORE-CORRUPT-001");
            assert_eq!(fs::read(&destination).unwrap(), before);
            assert_eq!(fs::read(&archive).unwrap(), before);
            assert_eq!(fs::read(&marker_path).unwrap(), marker_bytes);
            let repeated = json_failure(&dir, &["gc", "--yes"], NOW, 4);
            assert_eq!(repeated["error_code"], "KIO-E-STORE-CORRUPT-001");
            assert_eq!(fs::read(&destination).unwrap(), before);
            assert_eq!(fs::read(&archive).unwrap(), before);
            assert_eq!(fs::read(&marker_path).unwrap(), marker_bytes);
            assert_eq!(fs::read(&victim).unwrap(), b"foreign victim must survive\n");
        } else {
            assert!(
                output.status.success(),
                "{attack}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!archive.exists());
            assert!(!marker_path.exists());
        }
        assert_eq!(
            kio_core::gc::read_shallow_receipts(&dir.path().join(".kio")).unwrap()[&commit]
                .tree_hash,
            tree
        );
    }
}

#[cfg(windows)]
#[test]
fn windows_tree_gc_reclaims_bytes_held_by_a_retained_reader() {
    use std::io::Read;
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        GetFileInformationByHandle,
    };

    fn information(file: &fs::File) -> BY_HANDLE_FILE_INFORMATION {
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        assert_ne!(
            unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut information) },
            0
        );
        information
    }

    let (dir, commit, tree) = candidate_fixture();
    let mut reader = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(tree_path(&dir, &tree))
        .unwrap();
    let before = information(&reader);
    assert_eq!(before.nNumberOfLinks, 1);
    let ready = dir.path().join("retained-reader-tree-ready");
    let mut child = kio_process(&dir, &["gc", "--yes", "--json"])
        .env("KIO_FIXED_NOW", NOW)
        .env("KIO_TEST_GC_TREE_QUARANTINE_READY", &ready)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(6);
    while !ready.exists() && Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            let output = child.wait_with_output().unwrap();
            panic!(
                "GC failed before retaining the tree with its existing reader: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(ready.exists(), "GC did not retain the tree at its barrier");
    assert!(reader.metadata().unwrap().len() > 0);
    fs::write(ready.with_extension("release"), b"release").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let after = information(&reader);
    assert_eq!(after.dwVolumeSerialNumber, before.dwVolumeSerialNumber);
    assert_eq!(after.nFileIndexHigh, before.nFileIndexHigh);
    assert_eq!(after.nFileIndexLow, before.nFileIndexLow);
    assert_eq!(after.nNumberOfLinks, 0);
    assert_eq!(reader.metadata().unwrap().len(), 0);
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    assert!(bytes.is_empty());
    assert!(!tree_path(&dir, &tree).exists());
    assert!(!dir.path().join(".kio/gc/in_progress").exists());
    assert_eq!(
        kio_core::gc::read_shallow_receipts(&dir.path().join(".kio")).unwrap()[&commit].tree_hash,
        tree
    );
}

#[test]
fn interrupted_private_marker_stage_never_exposes_a_partial_public_marker() {
    let (dir, commit, tree) = candidate_fixture();
    let interrupted = kio(&dir, &["gc", "--yes"])
        .env("KIO_FIXED_NOW", NOW)
        .env("KIO_TEST_GC_FAULT", "after_marker_stage_fsync")
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(interrupted.status.code(), Some(7));
    assert!(!dir.path().join(".kio/gc/in_progress").exists());
    assert!(!receipt_path(&dir, &commit).exists());
    assert!(tree_path(&dir, &tree).exists());
    let private_marker = std::fs::read_dir(dir.path().join(".kio/gc/internal/markers"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("prepared-")
        })
        .unwrap();
    // A power loss may tear only the operation-private stage. It is not an
    // authority and must not wedge a later locked re-plan/publication.
    std::fs::write(private_marker, b"partial").unwrap();

    let completed = json_success(&dir, &["gc", "--yes"], NOW);
    assert_eq!(completed["status"], "completed");
    assert!(receipt_path(&dir, &commit).exists());
    assert!(!tree_path(&dir, &tree).exists());
}

#[test]
fn interrupted_private_receipt_stage_never_exposes_a_partial_final_receipt() {
    let (dir, commit, tree) = candidate_fixture();
    let interrupted = kio(&dir, &["gc", "--yes"])
        .env("KIO_FIXED_NOW", NOW)
        .env("KIO_TEST_GC_FAULT", "after_receipt_stage_fsync")
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(interrupted.status.code(), Some(7));
    assert!(dir.path().join(".kio/gc/in_progress").exists());
    assert!(!receipt_path(&dir, &commit).exists());
    assert!(tree_path(&dir, &tree).exists());
    let private_receipt = std::fs::read_dir(dir.path().join(".kio/gc/internal/receipts"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .next()
        .unwrap();
    // A torn private receipt stage is ignored; only the atomic final leaf can
    // authorize tree retirement.
    std::fs::write(private_receipt, b"partial").unwrap();

    let completed = json_success(&dir, &["gc", "--yes"], NOW);
    assert_eq!(completed["status"], "completed");
    assert!(receipt_path(&dir, &commit).exists());
    assert!(!tree_path(&dir, &tree).exists());
}

#[cfg(unix)]
#[test]
fn quarantine_crash_resumes_and_hardlink_race_never_changes_victim_bytes() {
    // A crash after the descriptor-relative rename leaves a full internal
    // archive. Recovery must finish that exact archive rather than treating a
    // missing canonical leaf as a completed sweep.
    let (dir, commit, tree) = candidate_fixture();
    let interrupted = kio(&dir, &["gc", "--yes"])
        .env("KIO_FIXED_NOW", NOW)
        .env("KIO_TEST_GC_FAULT", "after_tree_quarantine")
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(interrupted.status.code(), Some(7));
    let marker = GcInProgressMarker::parse_canonical(
        &fs::read(dir.path().join(".kio/gc/in_progress")).unwrap(),
    )
    .unwrap();
    let archive = tree_archive_path(&dir, &marker, &tree);
    assert!(!tree_path(&dir, &tree).exists());
    assert!(!fs::read(&archive).unwrap().is_empty());
    json_success(&dir, &["gc", "--yes"], NOW);
    assert!(receipt_path(&dir, &commit).exists());
    assert!(!archive.exists());
    assert!(!dir.path().join(".kio/gc/in_progress").exists());

    // Insert a hardlink in the exact post-validation/pre-unlink race window.
    // GC may fail closed, but must never truncate bytes visible at the victim.
    let (dir, _commit, tree) = candidate_fixture();
    let ready = dir.path().join("tree-quarantine-ready");
    let child = kio_process(&dir, &["gc", "--yes"])
        .env("KIO_FIXED_NOW", NOW)
        .env("KIO_TEST_GC_TREE_QUARANTINE_READY", &ready)
        .arg("--json")
        .spawn()
        .unwrap();
    wait_for_ready(&ready);
    let marker = GcInProgressMarker::parse_canonical(
        &fs::read(dir.path().join(".kio/gc/in_progress")).unwrap(),
    )
    .unwrap();
    let archive = tree_archive_path(&dir, &marker, &tree);
    let victim_dir = tempfile::tempdir().unwrap();
    let victim = victim_dir.path().join("must-not-be-truncated.tree");
    fs::hard_link(&archive, &victim).unwrap();
    let victim_before = fs::read(&victim).unwrap();
    fs::write(ready.with_extension("release"), b"release").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(fs::read(&victim).unwrap(), victim_before);
    assert!(dir.path().join(".kio/gc/in_progress").exists());
    // A linked archive is now deliberately non-resumable: the final
    // single-link revalidation refuses all further destructive mutation until
    // an operator resolves the foreign link. The external victim remains
    // exact throughout.
    let blocked = kio(&dir, &["gc", "--yes"])
        .env("KIO_FIXED_NOW", NOW)
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(blocked.status.code(), Some(4));
    assert_eq!(fs::read(&victim).unwrap(), victim_before);
}

#[cfg(unix)]
#[test]
fn quarantine_rename_replacement_is_refused_without_unlinking_the_victim() {
    let (dir, _commit, tree) = candidate_fixture();
    let ready = dir.path().join("tree-quarantine-rename-ready");
    let child = kio_process(&dir, &["gc", "--yes"])
        .env("KIO_FIXED_NOW", NOW)
        .env("KIO_TEST_GC_TREE_QUARANTINE_READY", &ready)
        .arg("--json")
        .spawn()
        .unwrap();
    wait_for_ready(&ready);
    let marker = GcInProgressMarker::parse_canonical(
        &fs::read(dir.path().join(".kio/gc/in_progress")).unwrap(),
    )
    .unwrap();
    let archive = tree_archive_path(&dir, &marker, &tree);
    let parked = archive.with_extension("verified-tree");
    let victim = archive.with_extension("foreign-victim");
    let victim_bytes = b"foreign tree victim must survive\n";
    fs::write(&victim, victim_bytes).unwrap();
    fs::rename(&archive, &parked).unwrap();
    fs::rename(&victim, &archive).unwrap();
    fs::write(ready.with_extension("release"), b"release").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(fs::read(&archive).unwrap(), victim_bytes);
    assert!(parked.exists());
    assert!(dir.path().join(".kio/gc/in_progress").exists());
}

#[test]
fn completed_shallow_commit_cannot_be_published_as_a_new_tag_tip() {
    let (dir, commit, _tree) = candidate_fixture();
    json_success(&dir, &["gc", "--yes"], NOW);
    let names = dir.path().join(".kio/refs/tags-v1/names.jsonl");
    let before = fs::read(&names).unwrap_or_default();
    let error = json_failure(&dir, &["tag", "too-late", &commit], NOW, 1);
    assert_eq!(error["error_code"], "KIO-E-COMMIT-SHALLOW-001");
    assert_eq!(fs::read(&names).unwrap_or_default(), before);
}

#[test]
fn finalizing_rechecks_a_recorded_generation_before_marker_removal() {
    let (dir, _commit, tree) = candidate_fixture();
    let interrupted = kio(&dir, &["gc", "--yes"])
        .env("KIO_FIXED_NOW", NOW)
        .env(
            "KIO_TEST_GC_FAULT",
            "after_final_rotation_before_marker_removal",
        )
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(interrupted.status.code(), Some(7));
    assert!(!tree_path(&dir, &tree).exists());
    let sqlite = dir.path().join(".kio/index/sqlite.db");
    assert!(sqlite.exists());
    // Simulate an interrupted sweep followed by external index loss.  Resume
    // must not delete the marker based only on its recorded final generation.
    fs::remove_file(&sqlite).unwrap();
    let blocked = kio(&dir, &["gc", "--yes"])
        .env("KIO_FIXED_NOW", NOW)
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(
        blocked.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&blocked.stderr)
    );
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("source index changed"));
    assert!(dir.path().join(".kio/gc/in_progress").exists());
}

#[test]
fn initial_index_generation_mismatch_rejects_before_tree_delete() {
    let (dir, _commit, tree) = candidate_fixture();
    let tree_path = tree_path(&dir, &tree);
    let tree_before = fs::read(&tree_path).unwrap();
    interrupt_after_marker(&dir);

    // The marker freezes the source index generation. Replacing its metadata
    // before the first rotation must not let receipts authorize a tree delete.
    let sqlite = dir.path().join(".kio/index/sqlite.db");
    let conn = Connection::open(&sqlite).unwrap();
    conn.execute(
        "UPDATE index_metadata SET index_generation = ?1 WHERE id = 1",
        ["01J00000000000000000000099"],
    )
    .unwrap();
    drop(conn);

    let blocked = kio(&dir, &["gc", "--yes"])
        .env("KIO_FIXED_NOW", NOW)
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(blocked.status.code(), Some(4));
    assert!(
        String::from_utf8_lossy(&blocked.stderr)
            .contains("source index changed before the pre-sweep rotation")
    );
    assert_eq!(fs::read(&tree_path).unwrap(), tree_before);
    assert!(dir.path().join(".kio/gc/in_progress").exists());
}

#[test]
fn pre_sweep_rotation_crash_resumes_only_the_durable_target_generation() {
    let (dir, _commit, tree) = candidate_fixture();
    let interrupted = kio(&dir, &["gc", "--yes"])
        .env("KIO_FIXED_NOW", NOW)
        .env("KIO_TEST_GC_FAULT", "after_pre_sweep_rotation")
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(interrupted.status.code(), Some(7));
    assert!(tree_path(&dir, &tree).exists());
    let marker = GcInProgressMarker::parse_canonical(
        &fs::read(dir.path().join(".kio/gc/in_progress")).unwrap(),
    )
    .unwrap();
    assert!(marker.index_pre_sweep.is_some());

    json_success(&dir, &["gc", "--yes"], NOW);
    assert!(!tree_path(&dir, &tree).exists());
}

#[test]
fn forged_present_pre_sweep_state_without_durable_attestation_blocks_retirement() {
    let (dir, _commit, tree) = candidate_fixture();
    let interrupted = kio(&dir, &["gc", "--yes"])
        .env("KIO_FIXED_NOW", NOW)
        .env("KIO_TEST_GC_FAULT", "after_pre_sweep_rotation")
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(interrupted.status.code(), Some(7));
    let marker_path = dir.path().join(".kio/gc/in_progress");
    let marker = GcInProgressMarker::parse_canonical(&fs::read(&marker_path).unwrap()).unwrap();
    assert!(matches!(
        marker.index_pre_sweep,
        Some(kio_core::gc::GcIndexState::Present { .. })
    ));
    // Keep the marker's real target identity/generation and all frozen
    // receipts, but strip the private-copy proof. A matching live database
    // without the same-transaction attestation is not deletion authority.
    let conn = Connection::open(dir.path().join(".kio/index/sqlite.db")).unwrap();
    conn.execute("DROP TABLE gc_rotation_attestation", [])
        .unwrap();
    drop(conn);

    let failure = json_failure(&dir, &["gc", "--yes"], NOW, 4);
    assert_eq!(failure["error_code"], "KIO-E-STORE-CORRUPT-001");
    assert!(tree_path(&dir, &tree).exists());
    assert!(marker_path.exists());
}

#[cfg(unix)]
#[test]
fn in_place_source_index_mutation_after_private_copy_is_rejected() {
    let (dir, _commit, tree_hash) = candidate_fixture();
    let tree = tree_path(&dir, &tree_hash);
    let before = fs::read(&tree).unwrap();
    let ready = dir.path().join("gc-index-copy-ready");
    let child = kio_process(&dir, &["gc", "--yes"])
        .env("KIO_FIXED_NOW", NOW)
        .env("KIO_TEST_GC_INDEX_COPY_READY", &ready)
        .arg("--json")
        .spawn()
        .unwrap();
    wait_for_ready(&ready);

    // The private bytes have been copied, but source stability has not yet
    // been accepted. A same-inode SQLite write must change the retained source
    // state and invalidate the prepared replacement.
    let conn = Connection::open(dir.path().join(".kio/index/sqlite.db")).unwrap();
    conn.execute(
        "UPDATE index_metadata
         SET last_lifecycle_epoch = last_lifecycle_epoch + 1 WHERE id = 1",
        [],
    )
    .unwrap();
    drop(conn);
    fs::write(ready.with_extension("release"), b"release").unwrap();

    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(fs::read(&tree).unwrap(), before);
    assert!(dir.path().join(".kio/gc/in_progress").exists());
}

#[test]
fn in_place_source_index_mutation_after_rotation_marker_blocks_exchange() {
    let (dir, _commit, tree_hash) = candidate_fixture();
    let tree = tree_path(&dir, &tree_hash);
    let before = fs::read(&tree).unwrap();
    let interrupted = kio(&dir, &["gc", "--yes"])
        .env("KIO_FIXED_NOW", NOW)
        .env("KIO_TEST_GC_FAULT", "after_rotation_marker_persist")
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(interrupted.status.code(), Some(7));

    // The private clone and its source-state digest are durable, but the
    // atomic exchange has not happened. A same-inode source update must not
    // be overwritten by publishing the now-stale clone during recovery.
    let conn = Connection::open(dir.path().join(".kio/index/sqlite.db")).unwrap();
    conn.execute(
        "UPDATE index_metadata
         SET last_lifecycle_epoch = last_lifecycle_epoch + 1 WHERE id = 1",
        [],
    )
    .unwrap();
    drop(conn);

    let failure = json_failure(&dir, &["gc", "--yes"], NOW, 4);
    assert_eq!(failure["error_code"], "KIO-E-STORE-CORRUPT-001");
    assert_eq!(fs::read(&tree).unwrap(), before);
    assert!(dir.path().join(".kio/gc/in_progress").exists());
}

#[cfg(unix)]
#[test]
fn in_place_index_attestation_mutation_after_permit_mint_blocks_tree_retirement() {
    let (dir, _commit, tree) = candidate_fixture();
    let tree = tree_path(&dir, &tree);
    let before = fs::read(&tree).unwrap();
    let ready = dir.path().join("gc-pre-quarantine-ready");
    let child = kio_process(&dir, &["gc", "--yes"])
        .env("KIO_FIXED_NOW", NOW)
        .env("KIO_TEST_GC_PRE_QUARANTINE_READY", &ready)
        .arg("--json")
        .spawn()
        .unwrap();
    wait_for_ready(&ready);

    // The coordinator has validated the exact DB and minted its opaque core
    // permit. Mutating that same inode afterward must still be detected by the
    // retained descriptor/state check at the final quarantine boundary.
    let conn = Connection::open(dir.path().join(".kio/index/sqlite.db")).unwrap();
    conn.execute(
        "UPDATE gc_rotation_attestation SET plan_digest = ?1 WHERE id = 1",
        [format!("sha256:{}", "f".repeat(64))],
    )
    .unwrap();
    drop(conn);
    fs::write(ready.with_extension("release"), b"release").unwrap();

    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(fs::read(&tree).unwrap(), before);
    assert!(dir.path().join(".kio/gc/in_progress").exists());
}

#[test]
fn same_generation_index_replacement_blocks_tree_retirement() {
    let (dir, _commit, tree) = candidate_fixture();
    let interrupted = kio(&dir, &["gc", "--yes"])
        .env("KIO_FIXED_NOW", NOW)
        .env("KIO_TEST_GC_FAULT", "after_pre_sweep_rotation")
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(interrupted.status.code(), Some(7));

    // Preserve the target generation bytes while changing the primary file's
    // inode.  Recovery must reject this before it can retire any tree.
    let sqlite = dir.path().join(".kio/index/sqlite.db");
    let replacement = dir.path().join(".kio/index/sqlite.replacement");
    fs::copy(&sqlite, &replacement).unwrap();
    fs::rename(&replacement, &sqlite).unwrap();

    let failure = json_failure(&dir, &["gc", "--yes"], NOW, 4);
    assert_eq!(failure["error_code"], "KIO-E-STORE-CORRUPT-001");
    assert!(tree_path(&dir, &tree).exists());
    assert!(dir.path().join(".kio/gc/in_progress").exists());
}

#[test]
fn substituted_old_temp_after_exchange_is_not_accepted_as_already_cleaned() {
    let (dir, _commit, tree) = candidate_fixture();
    let interrupted = kio(&dir, &["gc", "--yes"])
        .env("KIO_FIXED_NOW", NOW)
        .env("KIO_TEST_GC_FAULT", "after_index_exchange")
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(interrupted.status.code(), Some(7));
    let marker = GcInProgressMarker::parse_canonical(
        &fs::read(dir.path().join(".kio/gc/in_progress")).unwrap(),
    )
    .unwrap();
    let temp = marker.index_rotation.unwrap().temp_leaf;
    let sqlite = dir.path().join(".kio/index/sqlite.db");
    let private = dir.path().join(".kio/gc/internal/index");
    let replacement = private.join("temp.replacement");
    fs::copy(&sqlite, &replacement).unwrap();
    fs::rename(&replacement, private.join(temp)).unwrap();

    let failure = json_failure(&dir, &["gc", "--yes"], NOW, 4);
    assert_eq!(failure["error_code"], "KIO-E-STORE-CORRUPT-001");
    assert!(tree_path(&dir, &tree).exists());
}

#[test]
fn replaced_private_index_directory_after_exchange_fails_closed() {
    let (dir, _commit, tree) = candidate_fixture();
    let interrupted = kio(&dir, &["gc", "--yes"])
        .env("KIO_FIXED_NOW", NOW)
        .env("KIO_TEST_GC_FAULT", "after_index_exchange")
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(interrupted.status.code(), Some(7));
    let private = dir.path().join(".kio/gc/internal/index");
    let parked = dir.path().join(".kio/gc/internal/index.parked");
    fs::rename(&private, &parked).unwrap();
    fs::create_dir(&private).unwrap();

    let failure = json_failure(&dir, &["gc", "--yes"], NOW, 4);
    assert_eq!(failure["error_code"], "KIO-E-STORE-CORRUPT-001");
    assert!(tree_path(&dir, &tree).exists());
    assert!(dir.path().join(".kio/gc/in_progress").exists());
}

#[test]
fn finalizing_resume_always_rotates_again_before_completing_marker() {
    let (dir, _commit, _tree) = candidate_fixture();
    let interrupted = kio(&dir, &["gc", "--yes"])
        .env("KIO_FIXED_NOW", NOW)
        .env(
            "KIO_TEST_GC_FAULT",
            "after_final_rotation_before_marker_removal",
        )
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(interrupted.status.code(), Some(7));
    let marker = GcInProgressMarker::parse_canonical(
        &fs::read(dir.path().join(".kio/gc/in_progress")).unwrap(),
    )
    .unwrap();
    let first_generation = match marker.index_final.unwrap() {
        kio_core::gc::GcIndexState::Present { generation, .. } => generation,
        kio_core::gc::GcIndexState::Absent => panic!("fixture has an index"),
    };

    json_success(&dir, &["gc", "--yes"], NOW);
    let conn = Connection::open(dir.path().join(".kio/index/sqlite.db")).unwrap();
    let final_generation: String = conn
        .query_row(
            "SELECT index_generation FROM index_metadata WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_ne!(final_generation, first_generation);
}

#[test]
fn active_marker_blocks_init_snapshot_and_tag_before_their_writes() {
    let (dir, _commit, _tree) = candidate_fixture();
    let interrupted = kio(&dir, &["gc", "--yes"])
        .env("KIO_FIXED_NOW", NOW)
        .env("KIO_TEST_GC_FAULT", "after_marker_fsync")
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(interrupted.status.code(), Some(7));
    let marker_before = fs::read(dir.path().join(".kio/gc/in_progress")).unwrap();
    for args in [
        &["init"][..],
        &["snapshot", "create"][..],
        &["tag", "blocked"][..],
    ] {
        let output = kio(&dir, args).arg("--json").output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(3),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["error_code"], "KIO-E-GC-SWEEP-ACTIVE-001");
        assert_eq!(
            fs::read(dir.path().join(".kio/gc/in_progress")).unwrap(),
            marker_before
        );
    }
}

#[test]
fn active_marker_blocks_representative_writer_entrypoints_without_touching_marker() {
    let (dir, _commit, _tree) = candidate_fixture();
    interrupt_after_marker(&dir);
    let marker_path = dir.path().join(".kio/gc/in_progress");
    let marker_before = fs::read(&marker_path).unwrap();

    // These are deliberately distinct writer families.  They must all acquire
    // the normal GC-aware StoreLock before their first durable side effect.
    for args in [
        &["index", "--offline", "--yes"][..],
        &["batch", "resume", "--offline"][..],
        &["adapter", "revoke", "--all"][..],
        &["repair", "rebuild-db", "--offline"][..],
        &["reindex", "--regenerate", "--yes", "--offline"][..],
        &["purge", "document.md", "--reason", "other", "--yes"][..],
    ] {
        let output = kio(&dir, args).arg("--json").output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(3),
            "{args:?}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["error_code"], "KIO-E-GC-SWEEP-ACTIVE-001", "{args:?}");
        assert_eq!(fs::read(&marker_path).unwrap(), marker_before, "{args:?}");
    }
}

#[test]
fn active_marker_suppresses_new_search_cursor_rejects_replay_and_redacts_late_marker() {
    let (dir, _commit, _tree) = candidate_fixture();
    let search = [
        "search",
        "common pagination needle",
        "--scope",
        ".",
        "--mode",
        "text",
        "--limit",
        "1",
    ];
    let baseline = json_success(&dir, &search, NOW);
    let cursor = baseline["paging"]["next_cursor"]
        .as_str()
        .expect("fixture needs more than one result for cursor coverage")
        .to_owned();

    interrupt_after_marker(&dir);
    let page_one = json_success(&dir, &search, NOW);
    assert!(page_one["paging"]["next_cursor"].is_null(), "{page_one}");
    assert_eq!(
        page_one["gc_recovery_pending"]["next_cursor_suppressed"], true,
        "{page_one}"
    );
    let replay = json_failure(
        &dir,
        &[
            "search",
            "common pagination needle",
            "--scope",
            ".",
            "--mode",
            "text",
            "--cursor",
            &cursor,
        ],
        NOW,
        2,
    );
    assert_eq!(replay["error_code"], "KIO-E-SEARCH-CURSOR-001");

    // Use a separate non-shallow fixture for the response-boundary race. The
    // first scope intentionally remains interrupted above; attempting to
    // index through its now-shallow retained history would test an unrelated
    // writer contract instead of this cursor boundary.
    let (late_dir, _late_commit, _late_tree) = candidate_fixture();
    let ready = late_dir.path().join("search-response-ready");
    let child = kio_process(&late_dir, &search)
        .env("KIO_FIXED_NOW", NOW)
        .env("KIO_TEST_SEARCH_RESPONSE_BARRIER_READY", &ready)
        .arg("--json")
        .spawn()
        .unwrap();
    wait_for_ready(&ready);

    // This call is intentionally expected to leave a marker, not to finish.
    let marker = kio(&late_dir, &["gc", "--yes"])
        .env("KIO_FIXED_NOW", NOW)
        .env("KIO_TEST_GC_FAULT", "after_marker_fsync")
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(marker.status.code(), Some(7));
    fs::write(ready.with_extension("release"), b"release").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let late: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(late["paging"]["next_cursor"].is_null(), "{late}");
    assert_eq!(
        late["gc_recovery_pending"]["next_cursor_suppressed"], true,
        "{late}"
    );
}

#[test]
fn shared_tree_requires_all_receipts_before_one_removal() {
    let (dir, old_commit, old_tree) = candidate_fixture();
    let repo = Repository::open(dir.path()).unwrap();
    let head = repo.head_commit_hash().unwrap().unwrap();
    let head_object = repo.read_commit(&head).unwrap();
    let store = ObjectStore::new(repo.kio_dir());
    let sibling = CommitObject::new(
        old_tree.clone(),
        Some(old_commit.clone()),
        OLD.into(),
        "same stale tree".into(),
        head_object.tool_lock_hash.clone(),
        CommitStats {
            files_added: 0,
            files_modified: 0,
            files_deleted: 0,
        },
        CommitType::Auto,
    )
    .unwrap();
    let sibling_hash = store
        .write_json(ObjectKind::Commit, &serde_json::to_value(sibling).unwrap())
        .unwrap()
        .0;
    let merged = CommitObject::new(
        head_object.tree,
        Some(sibling_hash.clone()),
        NOW.into(),
        "current linear tip after two commits sharing the stale tree".into(),
        head_object.tool_lock_hash,
        CommitStats {
            files_added: 0,
            files_modified: 0,
            files_deleted: 0,
        },
        CommitType::Auto,
    )
    .unwrap();
    let merged_hash = store
        .write_json(ObjectKind::Commit, &serde_json::to_value(merged).unwrap())
        .unwrap()
        .0;
    fs::write(dir.path().join(".kio/HEAD"), format!("{merged_hash}\n")).unwrap();

    let interrupted = kio(&dir, &["gc", "--yes"])
        .env("KIO_FIXED_NOW", NOW)
        .env("KIO_TEST_GC_FAULT", "after_first_receipt")
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(
        interrupted.status.code(),
        Some(7),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&interrupted.stdout),
        String::from_utf8_lossy(&interrupted.stderr)
    );
    assert!(tree_path(&dir, &old_tree).exists());
    assert_eq!(
        fs::read_dir(dir.path().join(".kio/gc/shallowed"))
            .unwrap()
            .count(),
        1
    );
    json_success(&dir, &["gc", "--yes"], NOW);
    assert!(
        fs::read_to_string(receipt_path(&dir, &old_commit))
            .unwrap()
            .contains(&old_tree)
    );
    assert!(
        fs::read_to_string(receipt_path(&dir, &sibling_hash))
            .unwrap()
            .contains(&old_tree)
    );
    assert!(!tree_path(&dir, &old_tree).exists());
}

#[test]
fn malformed_marker_or_receipt_is_rejected_without_tree_mutation() {
    let (dir, commit, tree) = candidate_fixture();
    let before = fs::read(tree_path(&dir, &tree)).unwrap();
    fs::create_dir_all(dir.path().join(".kio/gc")).unwrap();
    fs::write(dir.path().join(".kio/gc/in_progress"), b"not canonical\n").unwrap();
    let marker = kio(&dir, &["gc", "--yes"])
        .env("KIO_FIXED_NOW", NOW)
        .arg("--json")
        .output()
        .unwrap();
    assert_ne!(marker.status.code(), Some(0));
    assert_eq!(fs::read(tree_path(&dir, &tree)).unwrap(), before);

    fs::remove_file(dir.path().join(".kio/gc/in_progress")).unwrap();
    fs::create_dir_all(receipt_path(&dir, &commit).parent().unwrap()).unwrap();
    fs::write(receipt_path(&dir, &commit), b"{}\n").unwrap();
    let receipt = kio(&dir, &["gc", "--yes"])
        .env("KIO_FIXED_NOW", NOW)
        .arg("--json")
        .output()
        .unwrap();
    assert_ne!(receipt.status.code(), Some(0));
    assert_eq!(fs::read(tree_path(&dir, &tree)).unwrap(), before);
}
