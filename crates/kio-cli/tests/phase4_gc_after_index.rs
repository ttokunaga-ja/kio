//! Phase 4 milestone 3: explicit `gc.mode = "after_index"` integration.
//!
//! These tests only create disposable scopes.  They deliberately use the
//! debug-only checkpoint seam so bounded recovery does not depend on machine
//! speed.

mod support;

use support::canonical_tempdir;

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use kio_core::cas::{ObjectKind, ObjectStore};
use kio_core::dag::{CommitObject, CommitStats, CommitType, TreeEntry, build_tree};
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
        "KIO_TEST_GC_RUNTIME_CHECKPOINTS",
        "KIO_TEST_GC_POST_PUBLICATION_READY",
        "KIO_TEST_GC_FAULT",
    ] {
        command.env_remove(name);
    }
    command
        .current_dir(dir.path())
        .env("HOME", dir.path().join("home"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env("XDG_CACHE_HOME", dir.path().join("cache"))
        .env("KIO_FIXED_NOW", NOW)
        .args(args);
    command
}

fn kio_at(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::cargo_bin("kio").unwrap();
    for name in [
        "GEMINI_API_KEY",
        "MISTRAL_API_KEY",
        "KIO_TEST_GEMINI_EMBED",
        "KIO_TEST_MISTRAL_OCR",
        "KIO_TEST_GC_RUNTIME_CHECKPOINTS",
        "KIO_TEST_GC_POST_PUBLICATION_READY",
        "KIO_TEST_GC_FAULT",
    ] {
        command.env_remove(name);
    }
    command
        .current_dir(root)
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("KIO_FIXED_NOW", NOW)
        .args(args);
    command
}

fn json_success_at(root: &Path, args: &[&str], now: &str) -> Value {
    let output = kio_at(root, args)
        .env("KIO_FIXED_NOW", now)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .clone();
    output_json(&output)
}

#[test]
fn config_flip_after_publication_cannot_activate_manual_only_gc() {
    let (dir, _commit, tree) = stale_candidate();
    configure(&dir, "manual_only");
    let tree_before = fs::read(tree_path(&dir, &tree)).unwrap();
    let ready = dir.path().join("post-publication.ready");

    let child = std::process::Command::new(assert_cmd::cargo::cargo_bin("kio"))
        .current_dir(dir.path())
        .env("HOME", dir.path().join("home"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env("XDG_CACHE_HOME", dir.path().join("cache"))
        .env("KIO_FIXED_NOW", NOW)
        .env("KIO_TEST_GC_POST_PUBLICATION_READY", &ready)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .args(["index", "--offline", "--yes", "--json"])
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !ready.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(
        ready.exists(),
        "index did not reach the post-publication barrier"
    );
    // This write lands strictly after preflight and the durable index result,
    // but before the automatic hook's second capability-bound config read.
    configure(&dir, "after_index");
    fs::write(ready.with_extension("release"), b"release").unwrap();

    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value = output_json(&output);
    assert_eq!(value["publication_status"], "completed");
    assert_eq!(value["error_code"], "KIO-E-GC-CONFIG-CHANGED-001");
    assert_eq!(value["gc"]["status"], "failed");
    assert_eq!(fs::read(tree_path(&dir, &tree)).unwrap(), tree_before);
    assert!(!dir.path().join(".kio/gc/in_progress").exists());
    assert!(!dir.path().join(".kio/gc/shallowed").exists());
}

#[cfg(unix)]
#[test]
fn scope_replacement_after_publication_cannot_redirect_automatic_gc() {
    let (dir, _commit, _tree) = stale_candidate();
    configure(&dir, "after_index");
    let (victim, _victim_commit, victim_tree) = stale_candidate();
    configure(&victim, "after_index");
    let victim_tree_before = fs::read(tree_path(&victim, &victim_tree)).unwrap();
    let control = canonical_tempdir();
    let ready = control.path().join("post-publication.ready");

    let child = std::process::Command::new(assert_cmd::cargo::cargo_bin("kio"))
        .current_dir(dir.path())
        .env("HOME", control.path().join("home"))
        .env("XDG_CONFIG_HOME", control.path().join("config"))
        .env("XDG_DATA_HOME", control.path().join("data"))
        .env("XDG_CACHE_HOME", control.path().join("cache"))
        .env("KIO_FIXED_NOW", NOW)
        .env("KIO_TEST_GC_POST_PUBLICATION_READY", &ready)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .args(["snapshot", "create", "-m", "bind original scope", "--json"])
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !ready.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(ready.exists(), "snapshot did not reach the handoff barrier");

    let original = dir.path().to_path_buf();
    let parked = original.with_extension("after-index-parked");
    let victim_original = victim.path().to_path_buf();
    fs::rename(&original, &parked).unwrap();
    fs::rename(&victim_original, &original).unwrap();
    fs::write(ready.with_extension("release"), b"release").unwrap();

    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(4),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value = output_json(&output);
    assert_eq!(value["publication_status"], "completed");
    // Replacing the whole scope invalidates the retained identity, a store
    // integrity failure. The already-published snapshot remains completed.
    assert_eq!(value["error_code"], "KIO-E-STORE-CORRUPT-001");
    assert_eq!(
        fs::read(tree_path_at(&original, &victim_tree)).unwrap(),
        victim_tree_before
    );
    assert!(!original.join(".kio/gc/in_progress").exists());
    assert!(!original.join(".kio/gc/shallowed").exists());

    // Restore both TempDir-owned names before their destructors run.
    fs::rename(&original, &victim_original).unwrap();
    fs::rename(&parked, &original).unwrap();
}

fn output_json(output: &std::process::Output) -> Value {
    serde_json::from_slice(&output.stdout)
        .or_else(|_| serde_json::from_slice(&output.stderr))
        .unwrap_or_else(|error| {
            panic!(
                "expected JSON output ({error}); stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
}

fn json_success(dir: &TempDir, args: &[&str], now: &str) -> Value {
    let output = kio(dir, args)
        .env("KIO_FIXED_NOW", now)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .clone();
    output_json(&output)
}

fn configure(dir: &TempDir, mode: &str) {
    let runtime = if mode == "manual_only" {
        String::new()
    } else {
        "max_runtime_seconds = 60\n".to_owned()
    };
    fs::write(
        dir.path().join(".kio/config.toml"),
        // Completion-path tests must not depend on machine load. Tests of the
        // soft bound use `KIO_TEST_GC_RUNTIME_CHECKPOINTS`, so leave ample real
        // monotonic time for every other fixture.
        format!("[gc]\nmode = \"{mode}\"\n{runtime}"),
    )
    .unwrap();
}

/// Create a genuine, stale shallow-GC candidate and return its commit/tree.
fn stale_candidate() -> (TempDir, String, String) {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("document.md"), "# old\n\nold candidate\n").unwrap();
    json_success(&dir, &["init"], NOW);
    let old = json_success(&dir, &["index", "--offline", "--yes"], OLD);
    let commit = old["commit_hash"].as_str().unwrap().to_owned();
    fs::write(dir.path().join("document.md"), "# current\n\ncurrent tip\n").unwrap();
    json_success(&dir, &["index", "--offline", "--yes"], NOW);
    let tree = Repository::open(dir.path())
        .unwrap()
        .read_commit(&commit)
        .unwrap()
        .tree;
    (dir, commit, tree)
}

#[test]
fn legacy_repaired_retention_key_is_rejected_before_gc_store_mutation() {
    fn snapshot(root: &Path) -> std::collections::BTreeMap<PathBuf, Option<Vec<u8>>> {
        fn visit(
            root: &Path,
            current: &Path,
            entries: &mut std::collections::BTreeMap<PathBuf, Option<Vec<u8>>>,
        ) {
            for entry in fs::read_dir(current).unwrap() {
                let path = entry.unwrap().path();
                let relative = path.strip_prefix(root).unwrap().to_owned();
                if path.is_dir() {
                    entries.insert(relative, None);
                    visit(root, &path, entries);
                } else {
                    entries.insert(relative, Some(fs::read(&path).unwrap()));
                }
            }
        }
        let mut entries = std::collections::BTreeMap::new();
        visit(root, root, &mut entries);
        entries
    }

    let (dir, _commit, _tree) = stale_candidate();
    for fields in [
        "keep_repaired_per_branch=1",
        "keep_repaired=1\nkeep_repaired_per_branch=1",
    ] {
        fs::write(
            dir.path().join(".kio/config.toml"),
            format!("[gc]\nmode=\"manual_only\"\n[gc.derived_retention]\n{fields}\n"),
        )
        .unwrap();
        let before = snapshot(&dir.path().join(".kio"));
        let output = kio(&dir, &["gc", "--yes", "--json"])
            .assert()
            .code(2)
            .get_output()
            .clone();
        assert_eq!(
            output_json(&output)["error_code"],
            "KIO-E-CONFIG-SCHEMA-001"
        );
        assert_eq!(snapshot(&dir.path().join(".kio")), before);
    }
}

fn tree_path(dir: &TempDir, tree: &str) -> PathBuf {
    tree_path_at(dir.path(), tree)
}

fn tree_path_at(root: &Path, tree: &str) -> PathBuf {
    ObjectStore::new(root.join(".kio"))
        .object_path(ObjectKind::Tree, tree)
        .unwrap()
}

fn receipt_path(dir: &TempDir, commit: &str) -> PathBuf {
    receipt_path_at(dir.path(), commit)
}

fn receipt_path_at(root: &Path, commit: &str) -> PathBuf {
    root.join(".kio/gc/shallowed")
        .join(commit.strip_prefix("sha256:").unwrap())
}

fn index_generation(dir: &TempDir) -> String {
    Connection::open(dir.path().join(".kio/index/sqlite.db"))
        .unwrap()
        .query_row(
            "SELECT index_generation FROM index_metadata WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

fn fake_pdf(pages: &[&str]) -> String {
    let kids = (0..pages.len())
        .map(|index| format!("{} 0 R", index + 2))
        .collect::<Vec<_>>()
        .join(" ");
    let mut output = format!(
        "%PDF-1.4\n1 0 obj << /Type /Pages /Kids [{kids}] /Count {} >> endobj\n",
        pages.len()
    );
    for (index, text) in pages.iter().enumerate() {
        output.push_str(&format!(
            "{} 0 obj << /Type /Page /Parent 1 0 R >> stream\nBT ({text}) Tj ET\nendstream endobj\n",
            index + 2
        ));
    }
    output.push_str("%%EOF\n");
    output
}

#[test]
fn manual_only_leaves_a_stale_candidate_byte_for_byte_unswept() {
    let (dir, _commit, tree) = stale_candidate();
    configure(&dir, "manual_only");
    let before_tree = fs::read(tree_path(&dir, &tree)).unwrap();

    let result = json_success(&dir, &["index", "--offline", "--yes"], NOW);
    assert_eq!(result["gc"]["status"], "disabled");
    assert_eq!(result["gc"]["reason"], "manual_only");
    assert_eq!(fs::read(tree_path(&dir, &tree)).unwrap(), before_tree);
    assert!(!dir.path().join(".kio/gc").exists());
}

#[test]
fn after_index_with_no_candidates_skips_without_creating_gc_state() {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("note.md"), "candidate-free\n").unwrap();
    json_success(&dir, &["init"], NOW);
    configure(&dir, "after_index");

    let result = json_success(&dir, &["index", "--offline", "--yes"], NOW);
    assert_eq!(result["gc"]["status"], "skipped");
    assert_eq!(result["gc"]["reason"], "no_candidates");
    assert!(!dir.path().join(".kio/gc/in_progress").exists());
    assert!(!dir.path().join(".kio/gc/internal").exists());
}

#[test]
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn after_index_with_candidates_fails_before_marker_on_unsupported_rotation_platforms() {
    let (dir, _commit, tree) = stale_candidate();
    configure(&dir, "after_index");
    let tree_before = fs::read(tree_path(&dir, &tree)).unwrap();

    let output = kio(
        &dir,
        &[
            "snapshot",
            "create",
            "-m",
            "unsupported automatic gc",
            "--json",
        ],
    )
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(4));
    let value = output_json(&output);
    assert_eq!(value["publication_status"], "completed");
    assert_eq!(value["gc"]["status"], "failed");
    assert_eq!(
        value["gc"]["error"]["error_code"],
        "KIO-E-STORE-CORRUPT-001"
    );
    assert_eq!(fs::read(tree_path(&dir, &tree)).unwrap(), tree_before);
    assert!(!dir.path().join(".kio/gc").exists());
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn after_index_sweeps_a_real_stale_tree_after_successful_index() {
    let (dir, commit, tree) = stale_candidate();
    configure(&dir, "after_index");
    fs::write(dir.path().join("document.md"), "# newest\n\ncurrent tip\n").unwrap();

    let result = json_success(&dir, &["index", "--offline", "--yes"], NOW);
    assert_eq!(result["gc"]["status"], "completed");
    assert!(!tree_path(&dir, &tree).exists());
    assert!(receipt_path(&dir, &commit).is_file());
    assert!(!dir.path().join(".kio/gc/in_progress").exists());
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn after_index_timeout_is_observable_and_next_invocations_converge_recovery() {
    let (dir, commit, tree) = stale_candidate();
    configure(&dir, "after_index");
    fs::write(dir.path().join("document.md"), "# newest\n\ncurrent tip\n").unwrap();

    let first = kio(&dir, &["index", "--offline", "--yes", "--json"])
        .env("KIO_TEST_GC_RUNTIME_CHECKPOINTS", "1")
        .output()
        .unwrap();
    assert_eq!(first.status.code(), Some(3));
    let first = output_json(&first);
    assert_eq!(first["publication_status"], "completed");
    assert_eq!(first["gc"]["status"], "deferred");
    assert_eq!(first["gc"]["reason"], "max_runtime_seconds");
    assert_eq!(first["gc"]["recovery_pending"], true);
    assert!(dir.path().join(".kio/gc/in_progress").exists());

    for _ in 0..20 {
        let output = kio(&dir, &["index", "--offline", "--yes", "--json"])
            .env("KIO_TEST_GC_RUNTIME_CHECKPOINTS", "1")
            .output()
            .unwrap();
        let value = output_json(&output);
        if output.status.success() {
            assert!(!dir.path().join(".kio/gc/in_progress").exists());
            assert!(receipt_path(&dir, &commit).is_file());
            assert!(!tree_path(&dir, &tree).exists());
            return;
        }
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(value["publication_status"], "not_started");
        assert_eq!(value["gc"]["status"], "deferred");
        assert_eq!(value["gc"]["recovery_pending"], true);
    }
    panic!("after_index recovery did not converge");
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn automatic_receipt_crash_recovers_in_preflight_before_next_publication() {
    let (dir, commit, tree) = stale_candidate();
    configure(&dir, "after_index");
    fs::write(dir.path().join("document.md"), "# newest\n\ncurrent tip\n").unwrap();

    let crashed = kio(&dir, &["index", "--offline", "--yes", "--json"])
        .env("KIO_TEST_GC_FAULT", "after_first_receipt")
        .output()
        .unwrap();
    assert_eq!(crashed.status.code(), Some(3));
    let crashed = output_json(&crashed);
    assert_eq!(crashed["publication_status"], "completed");
    assert_eq!(crashed["gc"]["status"], "failed");
    assert!(receipt_path(&dir, &commit).is_file());
    assert!(dir.path().join(".kio/gc/in_progress").is_file());
    assert!(tree_path(&dir, &tree).exists());

    // A new writer first resumes the frozen marker, then publishes its own
    // result.  No manual `kio gc` invocation is required for recovery.
    let recovered = json_success(&dir, &["snapshot", "create", "-m", "resume automatic"], NOW);
    assert_eq!(recovered["gc"]["recovered_before_publication"], true);
    assert!(!dir.path().join(".kio/gc/in_progress").exists());
    assert!(receipt_path(&dir, &commit).is_file());
    assert!(!tree_path(&dir, &tree).exists());
}

#[cfg(windows)]
fn windows_exchange_scope_image(root: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, at: &Path, image: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(at).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            assert!(!kind.is_symlink());
            if kind.is_dir() {
                walk(root, &path, image);
            } else {
                image.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut image = std::collections::BTreeMap::new();
    walk(root, root, &mut image);
    image
}

#[cfg(windows)]
fn assert_windows_automatic_exchange_completed(dir: &TempDir, commit: &str, tree: &str) {
    let kio_dir = dir.path().join(".kio");
    assert!(!kio_dir.join("gc/in_progress").exists());
    assert!(!tree_path(dir, tree).exists());
    let receipts = kio_core::gc::read_shallow_receipts(&kio_dir).unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[commit].tree_hash, tree);
    for kind in ["marker", "index"] {
        let owner = kio_dir.join(format!("gc/internal/{kind}-exchange"));
        if owner.exists() {
            assert_eq!(fs::read_dir(owner).unwrap().count(), 0);
        }
    }
}

#[cfg(windows)]
#[test]
fn automatic_windows_exchange_interruptions_resume_before_next_publication() {
    for point in [
        "after_windows_marker_exchange_source_capture",
        "after_windows_marker_exchange_source_retire",
        "after_windows_marker_exchange_intent_remove",
        "after_windows_index_exchange_source_capture",
    ] {
        let (dir, commit, tree) = stale_candidate();
        configure(&dir, "after_index");
        fs::write(
            dir.path().join("document.md"),
            "# automatic exchange first publication\n",
        )
        .unwrap();
        let crashed = kio(&dir, &["index", "--offline", "--yes", "--json"])
            .env("KIO_TEST_GC_FAULT", point)
            .output()
            .unwrap();
        assert_eq!(
            crashed.status.code(),
            Some(3),
            "{point}: {}",
            String::from_utf8_lossy(&crashed.stderr)
        );
        let crashed = output_json(&crashed);
        assert_eq!(crashed["publication_status"], "completed", "{point}");
        assert_eq!(crashed["gc"]["status"], "failed", "{point}");
        assert_eq!(
            crashed["gc"]["error"]["error_code"], "KIO-E-GC-TEST-INTERRUPTED-001",
            "{point}"
        );
        let published = crashed["commit_hash"].as_str().unwrap();
        let kio_dir = dir.path().join(".kio");
        assert_eq!(
            fs::read_to_string(kio_dir.join("HEAD")).unwrap().trim(),
            published
        );
        if point == "after_windows_marker_exchange_source_capture" {
            assert!(!kio_dir.join("gc/in_progress").exists());
            assert!(
                kio_dir
                    .join("gc/internal/marker-exchange/intent.json")
                    .is_file()
            );
        }
        if point == "after_windows_marker_exchange_source_retire" {
            assert!(kio_dir.join("gc/in_progress").is_file());
            assert!(
                kio_dir
                    .join("gc/internal/marker-exchange/intent.json")
                    .is_file()
            );
            assert!(!kio_dir.join("gc/internal/marker-exchange/backup").exists());
        }
        if point == "after_windows_marker_exchange_intent_remove" {
            assert!(kio_dir.join("gc/in_progress").is_file());
            assert!(
                !kio_dir
                    .join("gc/internal/marker-exchange/intent.json")
                    .exists()
            );
        }
        if point == "after_windows_index_exchange_source_capture" {
            assert!(!kio_dir.join("index/sqlite.db").exists());
            assert!(kio_dir.join("gc/in_progress").is_file());
            assert!(
                kio_dir
                    .join("gc/internal/index-exchange/intent.json")
                    .is_file()
            );
        }
        fs::write(
            dir.path().join("document.md"),
            "# automatic exchange next publication\n",
        )
        .unwrap();
        let recovered = json_success(&dir, &["index", "--offline", "--yes"], NOW);
        assert_eq!(
            recovered["gc"]["recovered_before_publication"], true,
            "{point}"
        );
        let next = recovered["commit_hash"].as_str().unwrap();
        assert_ne!(next, published);
        let repo = Repository::open(dir.path()).unwrap();
        assert_eq!(
            repo.read_commit(next).unwrap().parent.as_deref(),
            Some(published)
        );
        assert_eq!(
            fs::read_to_string(kio_dir.join("HEAD")).unwrap().trim(),
            next
        );
        assert_windows_automatic_exchange_completed(&dir, &commit, &tree);
        let receipts_before = windows_exchange_scope_image(&kio_dir.join("gc/shallowed"));
        let repeated = json_success(&dir, &["index", "--offline", "--yes"], NOW);
        assert_eq!(
            repeated["gc"]["recovered_before_publication"], false,
            "{point}"
        );
        assert_eq!(repeated["gc"]["status"], "skipped", "{point}");
        assert_eq!(
            fs::read_to_string(kio_dir.join("HEAD")).unwrap().trim(),
            next
        );
        assert_eq!(
            windows_exchange_scope_image(&kio_dir.join("gc/shallowed")),
            receipts_before
        );
    }
}

#[cfg(windows)]
#[test]
fn automatic_windows_exchange_mismatched_backup_blocks_next_publication() {
    for kind in ["marker", "index"] {
        let (dir, _commit, tree) = stale_candidate();
        configure(&dir, "after_index");
        fs::write(
            dir.path().join("document.md"),
            "# automatic before mismatch\n",
        )
        .unwrap();
        let point = format!("after_windows_{kind}_exchange_source_capture");
        let crashed = kio(&dir, &["index", "--offline", "--yes", "--json"])
            .env("KIO_TEST_GC_FAULT", &point)
            .output()
            .unwrap();
        assert_eq!(crashed.status.code(), Some(3));
        let crashed = output_json(&crashed);
        assert_eq!(crashed["publication_status"], "completed");
        assert_eq!(
            crashed["gc"]["error"]["error_code"],
            "KIO-E-GC-TEST-INTERRUPTED-001"
        );
        let kio_dir = dir.path().join(".kio");
        let backup = kio_dir.join(format!("gc/internal/{kind}-exchange/backup"));
        assert!(backup.is_file());
        fs::write(&backup, b"foreign replacement source bytes\n").unwrap();
        fs::write(
            dir.path().join("document.md"),
            "# must not publish after mismatch\n",
        )
        .unwrap();
        let before = windows_exchange_scope_image(&kio_dir);
        let failure = kio(&dir, &["index", "--offline", "--yes", "--json"])
            .output()
            .unwrap();
        assert_eq!(
            failure.status.code(),
            Some(4),
            "{kind}: {}",
            String::from_utf8_lossy(&failure.stderr)
        );
        assert!(
            failure.stdout.is_empty(),
            "{kind}: published despite mismatch"
        );
        let error = output_json(&failure);
        assert_eq!(error["error_code"], "KIO-E-GC-EXCHANGE-001", "{kind}");
        assert_eq!(
            windows_exchange_scope_image(&kio_dir),
            before,
            "{kind}: failed recovery mutated store"
        );
        assert!(tree_path(&dir, &tree).is_file());
    }
}

#[cfg(windows)]
#[test]
fn automatic_hook_resumes_marker_exchange_created_after_its_publication() {
    let (dir, commit, tree) = stale_candidate();
    configure(&dir, "after_index");
    fs::write(
        dir.path().join("document.md"),
        "# publication before concurrent exchange\n",
    )
    .unwrap();
    let ready = dir.path().join("post-publication-exchange.ready");
    let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("kio"));
    for name in [
        "GEMINI_API_KEY",
        "MISTRAL_API_KEY",
        "KIO_TEST_GC_FAULT",
        "KIO_TEST_GC_RUNTIME_CHECKPOINTS",
    ] {
        command.env_remove(name);
    }
    let child = command
        .current_dir(dir.path())
        .env("HOME", dir.path().join("home"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env("XDG_CACHE_HOME", dir.path().join("cache"))
        .env("KIO_FIXED_NOW", NOW)
        .env("KIO_TEST_GC_POST_PUBLICATION_READY", &ready)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .args(["index", "--offline", "--yes", "--json"])
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !ready.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(
        ready.exists(),
        "index did not reach post-publication barrier"
    );
    let head = fs::read(dir.path().join(".kio/HEAD")).unwrap();
    let interrupted = kio(&dir, &["gc", "--yes", "--json"])
        .env(
            "KIO_TEST_GC_FAULT",
            "after_windows_marker_exchange_source_capture",
        )
        .output()
        .unwrap();
    assert_eq!(interrupted.status.code(), Some(7));
    assert!(!dir.path().join(".kio/gc/in_progress").exists());
    assert!(
        dir.path()
            .join(".kio/gc/internal/marker-exchange/intent.json")
            .is_file()
    );
    fs::write(ready.with_extension("release"), b"release").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let completed = output_json(&output);
    assert_eq!(completed["gc"]["status"], "completed");
    assert_eq!(completed["gc"]["recovered_before_publication"], false);
    assert_eq!(fs::read(dir.path().join(".kio/HEAD")).unwrap(), head);
    assert_windows_automatic_exchange_completed(&dir, &commit, &tree);
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn completed_automatic_sweep_rotates_sqlite_before_and_after_tree_retirement() {
    let (dir, commit, tree) = stale_candidate();
    let generation_before = index_generation(&dir);
    configure(&dir, "after_index");

    // Manual snapshot publication itself does not rebuild SQLite.  Therefore
    // this generation change is attributable to the hook's pre/final sweep
    // rotations, not to index publication.
    let result = json_success(&dir, &["snapshot", "create", "-m", "rotate around gc"], NOW);
    assert_eq!(result["gc"]["status"], "completed");
    assert!(!tree_path(&dir, &tree).exists());
    assert!(receipt_path(&dir, &commit).is_file());
    let initial = result["gc"]["index_initial"]["generation"]
        .as_str()
        .unwrap();
    let pre_sweep = result["gc"]["index_pre_sweep"]["generation"]
        .as_str()
        .unwrap();
    let final_generation = result["gc"]["index_final"]["generation"].as_str().unwrap();
    assert_eq!(initial, generation_before);
    assert_ne!(initial, pre_sweep);
    assert_ne!(pre_sweep, final_generation);
    assert_ne!(initial, final_generation);
    assert_eq!(index_generation(&dir), final_generation);
}

#[test]
fn manual_only_snapshots_preserve_gc_state_tree_and_sqlite_generation() {
    let (dir, _commit, tree) = stale_candidate();
    configure(&dir, "manual_only");
    let tree_before = fs::read(tree_path(&dir, &tree)).unwrap();
    let generation_before = index_generation(&dir);

    fs::write(dir.path().join("document.md"), "manual-only changed\n").unwrap();
    for (message, expected_status) in [
        ("manual-only first", "created"),
        ("manual-only noop", "noop"),
    ] {
        let output = json_success(&dir, &["snapshot", "create", "-m", message], NOW);
        assert_eq!(output["status"], expected_status);
        assert_eq!(output["gc"]["status"], "disabled");
        assert_eq!(output["gc"]["reason"], "manual_only");
    }
    assert_eq!(fs::read(tree_path(&dir, &tree)).unwrap(), tree_before);
    assert_eq!(index_generation(&dir), generation_before);
    assert!(!dir.path().join(".kio/gc").exists());
}

#[test]
fn snapshot_success_and_noop_report_after_index_without_gc_state_for_empty_plan() {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("note.md"), "snapshot body\n").unwrap();
    json_success(&dir, &["init"], NOW);
    // Establish the public index first.  A candidate-free automatic hook must
    // not rotate this SQLite generation around either manual snapshot.
    json_success(&dir, &["index", "--offline", "--yes"], NOW);
    let generation_before = index_generation(&dir);
    configure(&dir, "after_index");

    let first = json_success(&dir, &["snapshot", "create", "-m", "first"], NOW);
    assert_eq!(first["gc"]["status"], "skipped");
    let second = json_success(&dir, &["snapshot", "create", "-m", "noop"], NOW);
    assert_eq!(second["gc"]["status"], "skipped");
    assert_eq!(index_generation(&dir), generation_before);

    let human = kio(&dir, &["snapshot", "create", "-m", "human observable"])
        .output()
        .unwrap();
    assert!(human.status.success());
    assert!(
        String::from_utf8_lossy(&human.stdout).contains("gc:"),
        "automatic GC state must be visible in human output: {}",
        String::from_utf8_lossy(&human.stdout)
    );
    assert!(!dir.path().join(".kio/gc").exists());
}

#[test]
fn preview_and_failed_index_never_start_after_index_gc() {
    let (dir, _commit, tree) = stale_candidate();
    configure(&dir, "after_index");
    let before = fs::read(tree_path(&dir, &tree)).unwrap();

    let preview = json_success(&dir, &["index", "--preview"], NOW);
    assert_eq!(preview["status"], "preview");
    assert!(preview.get("gc").is_none());

    // A rejected index invocation is never a successful index result, and
    // must not create a marker or consume the stale tree.
    let failed = kio(&dir, &["index", "--offline", "--online", "--json"])
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert_eq!(fs::read(tree_path(&dir, &tree)).unwrap(), before);
    assert!(!dir.path().join(".kio/gc").exists());
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn child_scope_runs_its_explicit_after_index_hook_once_and_reports_it_to_parent() {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("parent.md"), "parent\n").unwrap();
    fs::create_dir_all(dir.path().join("child")).unwrap();
    fs::write(dir.path().join("child/note.md"), "child\n").unwrap();
    json_success(&dir, &["init"], NOW);

    // The first parent pass initializes the discovered child.  Configure the
    // two independently-owned scopes explicitly, then the second pass makes
    // one parent publication and exactly one child subprocess publication.
    json_success(&dir, &["index", "--offline", "--yes"], NOW);
    configure(&dir, "after_index");
    let child_root = dir.path().join("child");
    // Make a genuine stale auto commit inside the already-initialized child.
    // The parent must invoke this child's hook once, and the child must sweep
    // its own tree without lending that work to the parent scope.
    fs::write(child_root.join("note.md"), "child stale\n").unwrap();
    let stale = json_success_at(&child_root, &["index", "--offline", "--yes"], OLD);
    let stale_commit = stale["commit_hash"].as_str().unwrap().to_owned();
    let stale_tree = Repository::open(&child_root)
        .unwrap()
        .read_commit(&stale_commit)
        .unwrap()
        .tree;
    fs::write(child_root.join("note.md"), "child current\n").unwrap();
    fs::write(
        dir.path().join("child/.kio/config.toml"),
        "[gc]\nmode = \"after_index\"\nmax_runtime_seconds = 60\n",
    )
    .unwrap();

    let result = json_success(&dir, &["index", "--offline", "--yes"], NOW);
    assert_eq!(result["gc"]["trigger"], "index");
    let children = result["child_scopes"].as_array().unwrap();
    let child = children
        .iter()
        .find(|row| row["path"] == "child")
        .expect("discovered child row");
    assert_eq!(child["status"], "indexed");
    assert_eq!(child["gc"]["trigger"], "index");
    assert_eq!(child["gc"]["mode"], "after_index");
    assert_eq!(child["gc"]["status"], "completed");
    assert!(receipt_path_at(&child_root, &stale_commit).is_file());
    assert!(!tree_path_at(&child_root, &stale_tree).exists());
    assert_eq!(
        children.iter().filter(|row| row["path"] == "child").count(),
        1,
        "parent output must retain exactly one child result"
    );
}

#[test]
fn genuine_partial_index_result_never_starts_after_index_gc() {
    let dir = canonical_tempdir();
    fs::write(
        dir.path().join("report.pdf"),
        fake_pdf(&["one", "two", "three", "four"]),
    )
    .unwrap();
    json_success(&dir, &["init"], NOW);
    json_success(&dir, &["ledger", "init"], NOW);
    json_success(&dir, &["adapter", "approve", "--all", "--yes"], NOW);
    // Build a real prior normalized instance, then force its changed version
    // through the adapter's full-fallback failure seam.  This is an actual
    // result+exit-3 partial index, not a command-line rejection.
    let initial = kio(&dir, &["index", "--yes", "--json"])
        .env("KIO_TEST_MARKDOWNIZE_ADAPTER", "incremental")
        .output()
        .unwrap();
    assert!(
        initial.status.success(),
        "{}",
        String::from_utf8_lossy(&initial.stderr)
    );
    configure(&dir, "after_index");
    fs::write(
        dir.path().join("report.pdf"),
        fake_pdf(&["one changed", "two", "three", "four"]),
    )
    .unwrap();

    let partial = kio(&dir, &["index", "--yes", "--json"])
        .env(
            "KIO_TEST_MARKDOWNIZE_ADAPTER",
            "reject_incremental_and_full",
        )
        .output()
        .unwrap();
    assert_eq!(partial.status.code(), Some(3));
    let partial = output_json(&partial);
    assert_eq!(partial["error_code"], "KIO-E-INDEX-PARTIAL-001");
    assert!(partial["failed_files"].as_u64().unwrap() > 0);
    assert!(partial.get("gc").is_none());
    assert!(!dir.path().join(".kio/gc").exists());
}

#[test]
fn after_index_retains_a_tree_still_shared_by_a_protected_ref_tip() {
    let (dir, _old_commit, tree) = stale_candidate();
    let shared_tip = CommitObject::new(
        tree.clone(),
        None,
        NOW.to_owned(),
        "protected shared tree".to_owned(),
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
        CommitStats {
            files_added: 0,
            files_modified: 0,
            files_deleted: 0,
        },
        CommitType::Manual,
    )
    .unwrap();
    let protected_hash = ObjectStore::new(dir.path().join(".kio"))
        .write_json(
            ObjectKind::Commit,
            &serde_json::to_value(shared_tip).unwrap(),
        )
        .unwrap()
        .0;
    fs::write(
        dir.path()
            .join(".kio/refs/tags-v1")
            .join(kio_core::portable::portable_tag_leaf("protected-shared")),
        format!("{protected_hash}\n"),
    )
    .unwrap();
    configure(&dir, "after_index");

    let result = json_success(
        &dir,
        &["snapshot", "create", "-m", "trigger retention"],
        NOW,
    );
    assert_eq!(result["gc"]["status"], "skipped");
    assert_eq!(result["gc"]["reason"], "no_candidates");
    assert!(
        tree_path(&dir, &tree).exists(),
        "a tree named by a protected ref tip must never be swept"
    );
    assert!(!dir.path().join(".kio/gc/in_progress").exists());
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn automatic_sweep_receipts_all_eligible_repaired_sharers_before_one_tree_removal() {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("current.md"), "current\n").unwrap();
    json_success(&dir, &["init"], NOW);
    json_success(&dir, &["index", "--offline", "--yes"], NOW);

    let store = ObjectStore::new(dir.path().join(".kio"));
    let shared_tree =
        store
            .write_json(
                ObjectKind::Tree,
                &serde_json::to_value(
                    build_tree(vec![TreeEntry::raw_file(
                    "old.md",
                    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                )
                .unwrap()])
                    .unwrap(),
                )
                .unwrap(),
            )
            .unwrap()
            .0;
    let current_tree = Repository::open(dir.path())
        .unwrap()
        .read_commit(
            fs::read_to_string(dir.path().join(".kio/HEAD"))
                .unwrap()
                .trim(),
        )
        .unwrap()
        .tree;
    let mut parent = Repository::open(dir.path())
        .unwrap()
        .head_commit_hash()
        .unwrap();
    let mut repaired = Vec::new();
    for index in 0..7 {
        let tree = if index < 2 {
            shared_tree.clone()
        } else {
            current_tree.clone()
        };
        let commit = CommitObject::new(
            tree,
            parent,
            format!("2025-01-{:02}T00:00:00Z", index + 1),
            format!("repaired fixture {index}"),
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            CommitStats {
                files_added: 0,
                files_modified: 0,
                files_deleted: 0,
            },
            CommitType::Repaired,
        )
        .unwrap();
        let hash = store
            .write_json(ObjectKind::Commit, &serde_json::to_value(commit).unwrap())
            .unwrap()
            .0;
        parent = Some(hash.clone());
        repaired.push(hash);
    }
    let protected_tip = CommitObject::new(
        current_tree,
        parent,
        NOW.to_owned(),
        "protected repaired tip".to_owned(),
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
        CommitStats {
            files_added: 0,
            files_modified: 0,
            files_deleted: 0,
        },
        CommitType::Manual,
    )
    .unwrap();
    let protected_tip = store
        .write_json(
            ObjectKind::Commit,
            &serde_json::to_value(protected_tip).unwrap(),
        )
        .unwrap()
        .0;
    fs::write(dir.path().join(".kio/HEAD"), format!("{protected_tip}\n")).unwrap();
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[gc]\nmode = \"after_index\"\nmax_runtime_seconds = 60\n\
         [gc.derived_retention]\nkeep_repaired = 5\n",
    )
    .unwrap();

    let preview = json_success(&dir, &["gc", "--dry-run"], NOW);
    assert_eq!(preview["candidate_count"], 2, "{preview}");
    assert_eq!(preview["candidate_tree_count"], 1, "{preview}");

    // Stop after the first durable receipt. Even though both commits share a
    // physical tree, the executor must not retire it until every frozen
    // authorizing receipt is durable.
    let interrupted = kio(&dir, &["index", "--offline", "--yes", "--json"])
        .env("KIO_TEST_GC_FAULT", "after_first_receipt")
        .output()
        .unwrap();
    assert_eq!(
        interrupted.status.code(),
        Some(3),
        "{}\n{}",
        String::from_utf8_lossy(&interrupted.stdout),
        String::from_utf8_lossy(&interrupted.stderr)
    );
    assert!(tree_path(&dir, &shared_tree).exists());
    assert_eq!(
        repaired
            .iter()
            .take(2)
            .filter(|commit| receipt_path(&dir, commit).is_file())
            .count(),
        1
    );

    let result = json_success(
        &dir,
        &["snapshot", "create", "-m", "resume repaired sweep"],
        NOW,
    );
    assert_eq!(
        result["gc"]["recovered_before_publication"], true,
        "{result}"
    );
    assert!(receipt_path(&dir, &repaired[0]).is_file());
    assert!(receipt_path(&dir, &repaired[1]).is_file());
    assert!(!tree_path(&dir, &shared_tree).exists());
    assert!(!dir.path().join(".kio/gc/in_progress").exists());
}
