//! Milestone 5: scheduler-owned `gc.mode = "on_idle"` integration.
//!
//! Keep the clock and all input material local so idle decisions are fully
//! repeatable and never require a network service.

mod support;

use support::canonical_tempdir;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::process::Stdio;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::thread;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::time::{Duration, Instant};

use assert_cmd::Command;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use kio_core::cas::{ObjectKind, ObjectStore};
#[cfg(any(target_os = "macos", target_os = "linux"))]
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

fn output_json(output: &std::process::Output) -> Value {
    serde_json::from_slice(&output.stdout)
        .or_else(|_| serde_json::from_slice(&output.stderr))
        .unwrap()
}

fn bytes(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, at: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(at).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                out.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn configure(dir: &TempDir, threshold: u64) {
    fs::write(
        dir.path().join(".kio/config.toml"),
        format!(
            "[gc]\nmode = \"on_idle\"\nidle_threshold_seconds = {threshold}\nmax_runtime_seconds = 60\n\
             \n[snapshot.auto]\nenabled = true\ninterval_seconds = 3600\non_change_threshold = 99\n"
        ),
    )
    .unwrap();
}

fn indexed(threshold: u64) -> TempDir {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("note.md"), "baseline\n").unwrap();
    json(&dir, &["init"], T0);
    json(&dir, &["index", "--offline", "--yes"], T0);
    configure(&dir, threshold);
    dir
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn stale_candidate() -> (TempDir, String) {
    let old = "2025-01-01T00:00:00Z";
    let dir = canonical_tempdir();
    fs::write(dir.path().join("note.md"), "old candidate\n").unwrap();
    json(&dir, &["init"], old);
    let first = json(&dir, &["index", "--offline", "--yes"], old);
    let commit = first["commit_hash"].as_str().unwrap().to_owned();
    fs::write(dir.path().join("note.md"), "current tip\n").unwrap();
    json(&dir, &["index", "--offline", "--yes"], T0);
    configure(&dir, 10);
    (dir, commit)
}

#[test]
fn only_enabled_and_indexed_scopes_activate_on_idle_without_store_mutation() {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("note.md"), "unindexed\n").unwrap();
    json(&dir, &["init"], T0);
    configure(&dir, 10);
    let before = bytes(&dir.path().join(".kio"));
    let report = json(&dir, &["snapshot", "auto"], T0);
    assert_eq!(report["status"], "skipped");
    assert_eq!(report["reason"], "not_indexed");
    assert_eq!(bytes(&dir.path().join(".kio")), before);

    fs::write(
        dir.path().join(".kio/config.toml"),
        "[gc]\nmode = \"on_idle\"\nidle_threshold_seconds = 10\nmax_runtime_seconds = 60\n\
         \n[snapshot.auto]\nenabled = false\ninterval_seconds = 3600\non_change_threshold = 99\n",
    )
    .unwrap();
    let disabled = json(&dir, &["snapshot", "auto"], T0);
    assert_eq!(disabled["reason"], "disabled");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn baseline_then_idle_boundary_reports_not_idle_and_no_candidate_noop() {
    let dir = indexed(10);
    let baseline = json(&dir, &["snapshot", "auto"], T0);
    assert_eq!(baseline["status"], "baseline_recorded");
    assert_eq!(baseline["publication_status"], "completed");
    assert_eq!(baseline["working_set_digest"].as_str().unwrap().len(), 71);
    assert_eq!(baseline["idle_observed_since"], T0);
    assert_eq!(baseline["idle_observed_seconds"], 0);
    assert_eq!(baseline["idle_threshold_seconds"], 10);

    let before = json(&dir, &["snapshot", "auto"], "2026-08-14T00:00:09Z");
    assert_eq!(before["status"], "not_idle");
    assert_eq!(before["idle_observed_seconds"], 9);
    let equal = json(&dir, &["snapshot", "auto"], "2026-08-14T00:00:10Z");
    assert_eq!(equal["status"], "noop");
    assert_eq!(equal["reason"], "no_gc_candidates");
    assert_eq!(equal["gc"]["reason"], "no_candidates");
    let after = json(&dir, &["snapshot", "auto"], "2026-08-14T00:00:11Z");
    assert_eq!(after["status"], "noop");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn human_on_idle_output_reports_gc_and_waiting_does_not_mutate_store() {
    let dir = indexed(10);
    json(&dir, &["snapshot", "auto"], T0);
    let before_waiting = bytes(&dir.path().join(".kio"));
    let waiting = kio(&dir, &["snapshot", "auto"], "2026-08-14T00:00:09Z")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let waiting = String::from_utf8(waiting).unwrap();
    assert!(waiting.contains("status: not_idle"));
    assert_eq!(bytes(&dir.path().join(".kio")), before_waiting);

    let eligible = kio(&dir, &["snapshot", "auto"], "2026-08-14T00:00:10Z")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let eligible = String::from_utf8(eligible).unwrap();
    assert!(eligible.contains("status: noop"));
    assert!(
        eligible.contains("gc: skipped (no_candidates)"),
        "on-idle GC outcome must be visible in human output: {eligible}"
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn idle_threshold_sweeps_a_real_stale_candidate() {
    let (dir, commit) = stale_candidate();
    let tree = Repository::open(dir.path())
        .unwrap()
        .read_commit(&commit)
        .unwrap()
        .tree;
    let tree_path = ObjectStore::new(dir.path().join(".kio"))
        .object_path(ObjectKind::Tree, &tree)
        .unwrap();
    assert!(tree_path.is_file());
    assert_eq!(
        json(&dir, &["snapshot", "auto"], T0)["status"],
        "baseline_recorded"
    );
    let report = json(&dir, &["snapshot", "auto"], "2026-08-14T00:00:10Z");
    assert_eq!(report["status"], "completed");
    assert_eq!(report["gc"]["status"], "completed");
    assert!(!tree_path.exists());
    assert!(
        dir.path()
            .join(".kio/gc/shallowed")
            .join(commit.trim_start_matches("sha256:"))
            .is_file()
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn idle_gc_timeout_defers_then_next_scheduler_invocations_resume_it() {
    let (dir, commit) = stale_candidate();
    let tree = Repository::open(dir.path())
        .unwrap()
        .read_commit(&commit)
        .unwrap()
        .tree;
    let tree_path = ObjectStore::new(dir.path().join(".kio"))
        .object_path(ObjectKind::Tree, &tree)
        .unwrap();
    json(&dir, &["snapshot", "auto"], T0);
    let first = kio(
        &dir,
        &["snapshot", "auto", "--json"],
        "2026-08-14T00:00:10Z",
    )
    .env("KIO_TEST_GC_RUNTIME_CHECKPOINTS", "1")
    .output()
    .unwrap();
    assert_eq!(first.status.code(), Some(3));
    let first = output_json(&first);
    assert_eq!(first["status"], "deferred");
    assert_eq!(first["publication_status"], "not_started");
    assert_eq!(first["gc"]["status"], "deferred");
    assert!(dir.path().join(".kio/gc/in_progress").is_file());

    for _ in 0..20 {
        let output = kio(
            &dir,
            &["snapshot", "auto", "--json"],
            "2026-08-14T00:00:10Z",
        )
        .env("KIO_TEST_GC_RUNTIME_CHECKPOINTS", "1")
        .output()
        .unwrap();
        if output.status.success() {
            assert!(!dir.path().join(".kio/gc/in_progress").exists());
            assert!(!tree_path.exists());
            assert!(
                dir.path()
                    .join(".kio/gc/shallowed")
                    .join(commit.trim_start_matches("sha256:"))
                    .is_file()
            );
            return;
        }
        let value = output_json(&output);
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(value["publication_status"], "not_started");
        assert_eq!(value["gc"]["status"], "deferred");
    }
    panic!("on-idle recovery did not converge");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn post_publication_index_replacement_fails_closed_without_retiring_stale_tree() {
    let (dir, commit) = stale_candidate();
    let tree = Repository::open(dir.path())
        .unwrap()
        .read_commit(&commit)
        .unwrap()
        .tree;
    let tree_path = ObjectStore::new(dir.path().join(".kio"))
        .object_path(ObjectKind::Tree, &tree)
        .unwrap();
    let tree_before = fs::read(&tree_path).unwrap();
    json(&dir, &["snapshot", "auto"], T0);
    let control = canonical_tempdir();
    let ready = control.path().join("post-publication.ready");
    let child = std::process::Command::new(assert_cmd::cargo::cargo_bin("kio"))
        .current_dir(dir.path())
        .env("HOME", control.path().join("home"))
        .env("XDG_CONFIG_HOME", control.path().join("config"))
        .env("XDG_DATA_HOME", control.path().join("data"))
        .env("XDG_CACHE_HOME", control.path().join("cache"))
        .env("KIO_FIXED_NOW", "2026-08-14T00:00:10Z")
        .env("KIO_TEST_GC_POST_PUBLICATION_READY", &ready)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(["snapshot", "auto", "--json"])
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        ready.exists(),
        "on-idle GC did not reach its handoff barrier"
    );

    let index = dir.path().join(".kio/index");
    let parked = control.path().join("parked-index");
    fs::rename(&index, &parked).unwrap();
    fs::create_dir(&index).unwrap();
    fs::write(ready.with_extension("release"), b"release").unwrap();
    let output = child.wait_with_output().unwrap();
    fs::remove_dir(&index).unwrap();
    fs::rename(&parked, &index).unwrap();

    assert_eq!(output.status.code(), Some(4));
    let value = output_json(&output);
    assert_eq!(value["status"], "skipped");
    assert_eq!(value["reason"], "gc_failed");
    assert_eq!(fs::read(&tree_path).unwrap(), tree_before);
    assert!(!dir.path().join(".kio/gc/in_progress").exists());
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn post_publication_snapshot_auto_disable_fails_closed_before_on_idle_sweep() {
    let (dir, commit) = stale_candidate();
    let tree = Repository::open(dir.path())
        .unwrap()
        .read_commit(&commit)
        .unwrap()
        .tree;
    let tree_path = ObjectStore::new(dir.path().join(".kio"))
        .object_path(ObjectKind::Tree, &tree)
        .unwrap();
    let tree_before = fs::read(&tree_path).unwrap();
    json(&dir, &["snapshot", "auto"], T0);
    let control = canonical_tempdir();
    let ready = control.path().join("post-publication.ready");
    let child = std::process::Command::new(assert_cmd::cargo::cargo_bin("kio"))
        .current_dir(dir.path())
        .env("HOME", control.path().join("home"))
        .env("XDG_CONFIG_HOME", control.path().join("config"))
        .env("XDG_DATA_HOME", control.path().join("data"))
        .env("XDG_CACHE_HOME", control.path().join("cache"))
        .env("KIO_FIXED_NOW", "2026-08-14T00:00:10Z")
        .env("KIO_TEST_GC_POST_PUBLICATION_READY", &ready)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(["snapshot", "auto", "--json"])
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        ready.exists(),
        "on-idle GC did not reach its handoff barrier"
    );
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[gc]\nmode = \"on_idle\"\nidle_threshold_seconds = 10\nmax_runtime_seconds = 60\n\
         \n[snapshot.auto]\nenabled = false\ninterval_seconds = 3600\non_change_threshold = 99\n",
    )
    .unwrap();
    fs::write(ready.with_extension("release"), b"release").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(3));
    let value = output_json(&output);
    assert_eq!(value["reason"], "gc_failed");
    assert_eq!(fs::read(&tree_path).unwrap(), tree_before);
    assert!(!dir.path().join(".kio/gc/in_progress").exists());
    assert!(
        !dir.path()
            .join(".kio/gc/shallowed")
            .join(commit.trim_start_matches("sha256:"))
            .exists()
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn manual_snapshot_and_index_commands_never_fresh_trigger_on_idle_gc() {
    let (dir, commit) = stale_candidate();
    let tree = Repository::open(dir.path())
        .unwrap()
        .read_commit(&commit)
        .unwrap()
        .tree;
    let tree_path = ObjectStore::new(dir.path().join(".kio"))
        .object_path(ObjectKind::Tree, &tree)
        .unwrap();
    let before = fs::read(&tree_path).unwrap();
    for args in [
        &["snapshot", "create", "-m", "ordinary writer", "--json"][..],
        &["index", "--preview", "--json"][..],
        &["index", "--offline", "--yes", "--json"][..],
    ] {
        let output = kio(&dir, args, T0).output().unwrap();
        assert!(
            output.status.success(),
            "ordinary command unexpectedly failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fs::read(&tree_path).unwrap(), before);
        assert!(!dir.path().join(".kio/gc/in_progress").exists());
        assert!(
            !dir.path()
                .join(".kio/gc/shallowed")
                .join(commit.trim_start_matches("sha256:"))
                .exists()
        );
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn parent_on_idle_does_not_consume_child_gc_state_and_child_handles_its_own_tree() {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("parent.md"), "parent\n").unwrap();
    fs::create_dir_all(dir.path().join("child")).unwrap();
    fs::write(dir.path().join("child/note.md"), "child\n").unwrap();
    json(&dir, &["init"], T0);
    json(&dir, &["index", "--offline", "--yes"], T0);
    configure(&dir, 10);
    let child_root = dir.path().join("child");
    let old = "2025-01-01T00:00:00Z";
    fs::write(child_root.join("note.md"), "child stale\n").unwrap();
    let parent_baseline = json(&dir, &["snapshot", "auto"], T0);
    assert_eq!(parent_baseline["status"], "baseline_recorded");
    let old_commit = {
        let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("kio"))
            .current_dir(&child_root)
            .env("HOME", dir.path().join("child-home"))
            .env("XDG_CONFIG_HOME", dir.path().join("child-config"))
            .env("XDG_DATA_HOME", dir.path().join("child-data"))
            .env("XDG_CACHE_HOME", dir.path().join("child-cache"))
            .env("KIO_FIXED_NOW", old)
            .args(["index", "--offline", "--yes", "--json"])
            .output()
            .unwrap();
        assert!(output.status.success());
        output_json(&output)["commit_hash"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    fs::write(child_root.join("note.md"), "child current\n").unwrap();
    let child_tree = Repository::open(&child_root)
        .unwrap()
        .read_commit(&old_commit)
        .unwrap()
        .tree;
    let child_tree_path = ObjectStore::new(child_root.join(".kio"))
        .object_path(ObjectKind::Tree, &child_tree)
        .unwrap();
    fs::write(
        child_root.join(".kio/config.toml"),
        "[gc]\nmode = \"on_idle\"\nidle_threshold_seconds = 10\nmax_runtime_seconds = 60\n\
         \n[snapshot.auto]\nenabled = true\ninterval_seconds = 3600\non_change_threshold = 99\n",
    )
    .unwrap();

    let parent = json(&dir, &["snapshot", "auto"], "2026-08-14T00:00:10Z");
    assert!(matches!(
        parent["status"].as_str(),
        Some("noop") | Some("not_idle")
    ));
    assert!(child_tree_path.is_file());
    assert!(!child_root.join(".kio/gc").exists());

    let child_baseline = std::process::Command::new(assert_cmd::cargo::cargo_bin("kio"))
        .current_dir(&child_root)
        .env("HOME", dir.path().join("child-home"))
        .env("XDG_CONFIG_HOME", dir.path().join("child-config"))
        .env("XDG_DATA_HOME", dir.path().join("child-data"))
        .env("XDG_CACHE_HOME", dir.path().join("child-cache"))
        .env("KIO_FIXED_NOW", T0)
        .args(["snapshot", "auto", "--json"])
        .output()
        .unwrap();
    assert!(child_baseline.status.success());
    let child_done = std::process::Command::new(assert_cmd::cargo::cargo_bin("kio"))
        .current_dir(&child_root)
        .env("HOME", dir.path().join("child-home"))
        .env("XDG_CONFIG_HOME", dir.path().join("child-config"))
        .env("XDG_DATA_HOME", dir.path().join("child-data"))
        .env("XDG_CACHE_HOME", dir.path().join("child-cache"))
        .env("KIO_FIXED_NOW", "2026-08-14T00:00:10Z")
        .args(["snapshot", "auto", "--json"])
        .output()
        .unwrap();
    assert!(child_done.status.success());
    assert!(!child_tree_path.exists());
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn content_and_path_changes_reset_idle_but_ignored_inputs_do_not() {
    let dir = indexed(60);
    json(&dir, &["snapshot", "auto"], T0);
    let baseline: Value =
        serde_json::from_slice(&fs::read(dir.path().join(".kio/snapshot-auto.json")).unwrap())
            .unwrap();
    fs::write(dir.path().join(".kioignore"), "ignored.txt\n").unwrap();
    fs::write(dir.path().join("ignored.txt"), "excluded\n").unwrap();
    let ignored = json(&dir, &["snapshot", "auto"], "2026-08-14T00:00:01Z");
    assert_eq!(ignored["status"], "not_idle");
    assert_eq!(
        ignored["working_set_digest"],
        baseline["working_set_digest"]
    );

    fs::write(dir.path().join("note.md"), "edited\n").unwrap();
    let changed = json(&dir, &["snapshot", "auto"], "2026-08-14T00:00:02Z");
    assert_eq!(changed["status"], "not_idle");
    assert_eq!(changed["reason"], "working_set_changed");
    assert_ne!(
        changed["working_set_digest"],
        baseline["working_set_digest"]
    );
    assert_eq!(changed["idle_observed_since"], "2026-08-14T00:00:02Z");

    fs::rename(dir.path().join("note.md"), dir.path().join("renamed.md")).unwrap();
    let renamed = json(&dir, &["snapshot", "auto"], "2026-08-14T00:00:03Z");
    assert_eq!(renamed["status"], "not_idle");
    assert_eq!(renamed["reason"], "working_set_changed");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn digest_uses_content_and_paths_not_mtime_or_ignored_tier_a_inputs() {
    let dir = indexed(60);
    fs::write(dir.path().join(".kioignore"), "ignored.txt\n").unwrap();
    let baseline = json(&dir, &["snapshot", "auto"], T0);
    let digest = baseline["working_set_digest"].clone();
    let note = dir.path().join("note.md");
    let modified = fs::metadata(&note).unwrap().modified().unwrap();
    fs::write(&note, "baseline\n").unwrap();
    fs::File::open(&note)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    let mtime_only = json(&dir, &["snapshot", "auto"], "2026-08-14T00:00:01Z");
    assert_eq!(mtime_only["working_set_digest"], digest);
    assert_eq!(mtime_only["idle_observed_since"], T0);

    // A content edit remains observable even when its mtime is restored.
    fs::write(&note, "different bytes\n").unwrap();
    fs::File::open(&note)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    let edited = json(&dir, &["snapshot", "auto"], "2026-08-14T00:00:02Z");
    assert_ne!(edited["working_set_digest"], digest);
    assert_eq!(edited["reason"], "working_set_changed");

    fs::write(dir.path().join("added.md"), "add\n").unwrap();
    let added = json(&dir, &["snapshot", "auto"], "2026-08-14T00:00:03Z");
    assert_eq!(added["reason"], "working_set_changed");
    fs::remove_file(dir.path().join("added.md")).unwrap();
    let deleted = json(&dir, &["snapshot", "auto"], "2026-08-14T00:00:04Z");
    assert_eq!(deleted["reason"], "working_set_changed");

    let stable_digest = deleted["working_set_digest"].clone();
    fs::write(dir.path().join("ignored.txt"), "ignored\n").unwrap();
    fs::write(dir.path().join(".env"), "TOKEN=secret\n").unwrap();
    let ignored = json(&dir, &["snapshot", "auto"], "2026-08-14T00:00:05Z");
    assert_eq!(ignored["working_set_digest"], stable_digest);
    assert_eq!(
        ignored["idle_observed_since"],
        deleted["idle_observed_since"]
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn rejects_noncanonical_or_incompatible_idle_state_and_clock_rollback() {
    let dir = indexed(10);
    for state in [
        "not json\n",
        "{\"last_successful_eligible_attempt_at\":\"2026-08-14T00:00:00Z\",\"version\":1}\n",
        "{\"idle_observed_since\":\"2026-08-14T00:00:00Z\",\"last_successful_eligible_attempt_at\":\"2026-08-14T00:00:00Z\",\"version\":9,\"working_set_digest\":\"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}\n",
    ] {
        fs::write(dir.path().join(".kio/snapshot-auto.json"), state).unwrap();
        let output = kio(&dir, &["snapshot", "auto", "--json"], T0)
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "invalid scheduler state was accepted"
        );
    }
    fs::remove_file(dir.path().join(".kio/snapshot-auto.json")).unwrap();
    json(&dir, &["snapshot", "auto"], T0);
    let rollback = kio(
        &dir,
        &["snapshot", "auto", "--json"],
        "2026-08-13T23:59:59Z",
    )
    .output()
    .unwrap();
    assert_eq!(rollback.status.code(), Some(3));
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
#[test]
fn indexed_on_idle_fails_before_scheduler_or_gc_mutation_on_unsupported_platform() {
    let dir = indexed(10);
    let before = bytes(&dir.path().join(".kio"));
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
    assert_eq!(bytes(&dir.path().join(".kio")), before);
    assert!(!dir.path().join(".kio/.lock").exists());
    assert!(!dir.path().join(".kio/snapshot-auto.json").exists());
    assert!(!dir.path().join(".kio/gc").exists());
}

#[test]
fn strict_gc_mode_contract_rejects_missing_irrelevant_invalid_and_unknown_values() {
    let dir = indexed(10);
    for gc in [
        "[gc]\nmode = \"on_idle\"\nmax_runtime_seconds = 60\n",
        "[gc]\nmode = \"after_index\"\nmax_runtime_seconds = 60\nidle_threshold_seconds = 1\n",
        "[gc]\nmode = \"on_idle\"\nidle_threshold_seconds = 0\nmax_runtime_seconds = 60\n",
        "[gc]\nmode = \"unknown\"\nmax_runtime_seconds = 60\n",
    ] {
        fs::write(
            dir.path().join(".kio/config.toml"),
            format!("{gc}\n[snapshot.auto]\nenabled = true\ninterval_seconds = 60\non_change_threshold = 1\n"),
        )
        .unwrap();
        let output = kio(&dir, &["snapshot", "auto", "--json"], T0)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
    }
}
