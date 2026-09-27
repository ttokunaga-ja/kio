//! GC shallow-sweep fsck and export barriers.

mod support;

use support::canonical_tempdir;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use assert_cmd::Command;
use kio_core::cas::{ObjectKind, ObjectStore};
use kio_core::gc::{
    GcInProgressMarker, GcIndexState, GcMarkerCandidate, GcSweepPhase, GcSweepSession,
    ShallowReceipt,
};
use kio_core::scope::Repository;
use kio_index::fts::{FtsSchemaConfig, FtsTokenizer, read_bound_gc_index_metadata};
use serde_json::Value;
use tempfile::TempDir;

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

fn json_success(dir: &TempDir, args: &[&str]) -> Value {
    let output = kio(dir, args)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

fn json_success_at(dir: &TempDir, args: &[&str], now: &str) -> Value {
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

fn fixture() -> (TempDir, String, String) {
    fixture_with_index(true)
}

/// The no-index form models the valid `Absent -> Absent` pre-sweep barrier.
/// A real sweep whose initial index exists must rotate it before retiring a
/// tree, so tests for post-retirement phases cannot fake that state with an
/// indexed fixture.
fn fixture_without_index() -> (TempDir, String, String) {
    fixture_with_index(false)
}

fn fixture_with_index(with_index: bool) -> (TempDir, String, String) {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("doc.md"), "# receipt sweep\n").unwrap();
    json_success(&dir, &["init"]);
    if with_index {
        json_success(&dir, &["index", "--offline", "--yes"]);
    }
    let repo = Repository::open(dir.path()).unwrap();
    if !with_index {
        repo.auto_snapshot_with_normalize(
            Some("create fixture snapshot"),
            Some("2026-08-14T00:00:00Z"),
            &BTreeSet::new(),
            &BTreeMap::new(),
        )
        .unwrap();
    }
    let commit = repo.head_commit_hash().unwrap().unwrap();
    let tree = repo.read_commit(&commit).unwrap().tree;
    // A final shallow receipt is only valid for an eligible non-tip commit.
    // Advance HEAD with another auto snapshot while retaining this candidate.
    fs::write(dir.path().join("doc.md"), "# receipt sweep advanced\n").unwrap();
    repo.auto_snapshot_with_normalize(
        Some("advance fixture head"),
        Some("2026-08-14T00:00:01Z"),
        &BTreeSet::new(),
        &BTreeMap::new(),
    )
    .unwrap();
    (dir, commit, tree)
}

fn current_index_state(dir: &TempDir) -> GcIndexState {
    // Phase E keeps SQLite binding fail-closed outside macOS/Linux.  Frozen
    // no-index fixtures are nevertheless a valid `Absent -> Absent` state,
    // so avoid opening a non-existent SQLite leaf before reporting Absence.
    if !dir.path().join(".kio/index/sqlite.db").is_file() {
        return GcIndexState::Absent;
    }
    let session =
        GcSweepSession::bind(Repository::open(dir.path()).unwrap().root().to_path_buf()).unwrap();
    let kio = session.retained_kio_handle().unwrap();
    let metadata = read_bound_gc_index_metadata(
        &kio,
        &FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        },
    )
    .unwrap();
    let state = match metadata {
        Some(metadata) => GcIndexState::Present {
            generation: metadata.metadata.index_generation,
            identity: metadata.file_identity,
        },
        None => GcIndexState::Absent,
    };
    session.assert_public_identity().unwrap();
    state
}

fn receipt_path(dir: &TempDir, commit: &str) -> PathBuf {
    let leaf = commit.strip_prefix("sha256:").unwrap();
    dir.path().join(".kio/gc/shallowed").join(leaf)
}

fn write_receipt(dir: &TempDir, commit: &str, tree: &str) {
    let path = receipt_path(dir, commit);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let receipt =
        ShallowReceipt::new(commit.into(), tree.into(), "2026-08-14T00:00:00Z".into()).unwrap();
    fs::write(path, receipt.canonical_bytes().unwrap()).unwrap();
}

fn write_marker(dir: &TempDir, commit: &str, tree: &str, phase: GcSweepPhase) {
    let index_pre_sweep = matches!(phase, GcSweepPhase::Sweeping | GcSweepPhase::Finalizing)
        .then_some(GcIndexState::Absent);
    let tree_size = fs::metadata(
        ObjectStore::new(dir.path().join(".kio"))
            .object_path(ObjectKind::Tree, tree)
            .unwrap(),
    )
    .map(|metadata| metadata.len())
    .unwrap_or(0);
    let mut marker = GcInProgressMarker {
        version: 1,
        sweep_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
        started_at: "2026-08-14T00:00:00Z".into(),
        phase: if matches!(phase, GcSweepPhase::Sweeping | GcSweepPhase::Finalizing) {
            GcSweepPhase::Receipting
        } else {
            phase.clone()
        },
        plan_digest: format!("sha256:{}", "a".repeat(64)),
        truth_digest: format!("sha256:{}", "b".repeat(64)),
        stable_truth_digest: format!("sha256:{}", "c".repeat(64)),
        baseline_receipts_digest: format!("sha256:{}", "d".repeat(64)),
        operation_receipts_digest: None,
        candidates: vec![GcMarkerCandidate {
            commit_hash: commit.into(),
            tree_hash: tree.into(),
            size_bytes: tree_size,
        }],
        trees: vec![tree.into()],
        estimated_bytes: tree_size,
        index_initial: current_index_state(dir),
        index_pre_sweep,
        index_final: None,
        index_rotation: None,
    };
    if matches!(phase, GcSweepPhase::Sweeping | GcSweepPhase::Finalizing) {
        marker = GcSweepSession::bind(Repository::open(dir.path()).unwrap().root().to_path_buf())
            .unwrap()
            .bind_operation_receipts(&marker)
            .unwrap();
        marker.phase = phase;
    }
    let path = dir.path().join(".kio/gc/in_progress");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, marker.canonical_bytes().unwrap()).unwrap();
}

#[test]
fn final_shallow_receipt_explains_missing_tree_but_markerless_coexistence_is_corrupt() {
    let (dir, commit, tree) = fixture();
    write_receipt(&dir, &commit, &tree);
    let store = ObjectStore::new(dir.path().join(".kio"));
    let tree_path = store.object_path(ObjectKind::Tree, &tree).unwrap();
    fs::remove_file(&tree_path).unwrap();

    let clean = json_success(&dir, &["repair", "verify-objects"]);
    assert_eq!(clean["status"], "ok");

    // A second fresh fixture exercises markerless receipt/tree coexistence.
    let (dir2, commit2, tree2) = fixture();
    write_receipt(&dir2, &commit2, &tree2);
    let output = kio(&dir2, &["repair", "verify-objects"])
        .arg("--json")
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).unwrap();
    assert!(
        value["remaining_findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| { f["kind"] == "gc_shallow_receipt_corrupt" })
    );
}

#[test]
fn active_marker_blocks_fsck_repair_and_restore_before_destination_side_effects() {
    let (dir, commit, tree) = fixture_without_index();
    write_marker(&dir, &commit, &tree, GcSweepPhase::Prepared);
    let marker_before = fs::read(dir.path().join(".kio/gc/in_progress")).unwrap();

    let fsck_output = kio(&dir, &["repair", "verify-objects"])
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(
        fsck_output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&fsck_output.stderr)
    );
    assert!(
        !fsck_output.stdout.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&fsck_output.stderr)
    );
    let fsck = fsck_output.stdout;
    let value: Value = serde_json::from_slice(&fsck).unwrap();
    let findings = value["remaining_findings"].as_array().unwrap();
    #[cfg(unix)]
    assert!(findings.iter().any(|f| f["kind"] == "gc_sweep_incomplete"));
    #[cfg(not(unix))]
    {
        // Phase E requires unsupported platforms to fail closed rather than
        // advertising a resumable descriptor-bound sweep.
        assert!(
            findings
                .iter()
                .any(|f| f["kind"] == "gc_sweep_marker_corrupt")
        );
        assert!(!findings.iter().any(|f| f["kind"] == "gc_sweep_incomplete"));
    }
    assert_eq!(
        marker_before,
        fs::read(dir.path().join(".kio/gc/in_progress")).unwrap()
    );

    let destination = dir.path().join("destination");
    let destination_text = destination.display().to_string();
    kio(&dir, &["export", &commit, "--to", &destination_text])
        .arg("--json")
        .assert()
        .code(3);
    assert!(!destination.exists());
    assert_eq!(
        marker_before,
        fs::read(dir.path().join(".kio/gc/in_progress")).unwrap()
    );
}

#[test]
fn every_frozen_receipt_tree_transition_is_recovery_pending() {
    for (phase, write_receipt_first, remove_tree) in [
        (GcSweepPhase::Prepared, false, false),
        (GcSweepPhase::Receipting, true, false),
        (GcSweepPhase::Sweeping, true, true),
        (GcSweepPhase::Finalizing, true, true),
    ] {
        let (dir, commit, tree) = fixture_without_index();
        if write_receipt_first {
            write_receipt(&dir, &commit, &tree);
        }
        write_marker(&dir, &commit, &tree, phase);
        if remove_tree {
            let store = ObjectStore::new(dir.path().join(".kio"));
            fs::remove_file(store.object_path(ObjectKind::Tree, &tree).unwrap()).unwrap();
        }
        let marker_before = fs::read(dir.path().join(".kio/gc/in_progress")).unwrap();
        let receipt_before = fs::read(receipt_path(&dir, &commit)).ok();

        let output = kio(&dir, &["repair", "verify-objects"])
            .arg("--json")
            .assert()
            .code(3)
            .get_output()
            .stdout
            .clone();
        let value: Value = serde_json::from_slice(&output).unwrap();
        let findings = value["remaining_findings"].as_array().unwrap();
        #[cfg(unix)]
        assert!(findings.iter().any(|f| f["kind"] == "gc_sweep_incomplete"));
        #[cfg(not(unix))]
        {
            // Phase E requires unsupported platforms to fail closed rather
            // than advertising a resumable descriptor-bound sweep.
            assert!(
                findings
                    .iter()
                    .any(|f| f["kind"] == "gc_sweep_marker_corrupt")
            );
            assert!(!findings.iter().any(|f| f["kind"] == "gc_sweep_incomplete"));
        }
        assert_eq!(
            marker_before,
            fs::read(dir.path().join(".kio/gc/in_progress")).unwrap()
        );
        assert_eq!(receipt_before, fs::read(receipt_path(&dir, &commit)).ok());
    }
}

#[test]
fn absent_pre_sweep_marker_cannot_authorize_retirement_against_live_index() {
    let (dir, commit, tree) = fixture();
    // Deliberately forge a structurally valid Absent -> Absent sweep marker
    // after indexing. Its receipt/tree state looks like a crash after
    // retirement, but the public SQLite leaf proves this was never the
    // durable pre-sweep generation.
    write_receipt(&dir, &commit, &tree);
    let tree_path = ObjectStore::new(dir.path().join(".kio"))
        .object_path(ObjectKind::Tree, &tree)
        .unwrap();
    let tree_size = fs::metadata(&tree_path).unwrap().len();
    let original_tree_bytes = fs::read(&tree_path).unwrap();
    let mut marker = GcInProgressMarker {
        version: 1,
        sweep_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
        started_at: "2026-08-14T00:00:00Z".into(),
        phase: GcSweepPhase::Receipting,
        plan_digest: format!("sha256:{}", "a".repeat(64)),
        truth_digest: format!("sha256:{}", "b".repeat(64)),
        stable_truth_digest: format!("sha256:{}", "c".repeat(64)),
        baseline_receipts_digest: format!("sha256:{}", "d".repeat(64)),
        operation_receipts_digest: None,
        candidates: vec![GcMarkerCandidate {
            commit_hash: commit.clone(),
            tree_hash: tree.clone(),
            size_bytes: tree_size,
        }],
        trees: vec![tree.clone()],
        estimated_bytes: tree_size,
        index_initial: GcIndexState::Absent,
        index_pre_sweep: Some(GcIndexState::Absent),
        index_final: None,
        index_rotation: None,
    };
    let session =
        GcSweepSession::bind(Repository::open(dir.path()).unwrap().root().to_path_buf()).unwrap();
    marker = session.bind_operation_receipts(&marker).unwrap();
    marker.phase = GcSweepPhase::Sweeping;
    // This models a forged on-disk marker.  Do not invoke the production
    // publication primitive: Phase E keeps that descriptor-bound mutation
    // fail-closed on unsupported platforms.
    let marker_path = dir.path().join(".kio/gc/in_progress");
    fs::create_dir_all(marker_path.parent().unwrap()).unwrap();
    fs::write(&marker_path, marker.canonical_bytes().unwrap()).unwrap();

    // The public core mutator cannot be called from marker JSON alone: its
    // permit constructor is unsafe and reserved for the trusted index
    // coordinator after a descriptor-bound SQLite attestation check.  This
    // forged marker therefore remains observable only as corruption below.
    assert_eq!(fs::read(&tree_path).unwrap(), original_tree_bytes);

    // Model the distinct on-disk crash/forgery state for fsck after a tree has
    // disappeared; observer validation must classify it as corruption.
    fs::remove_file(&tree_path).unwrap();

    let output = kio(&dir, &["repair", "verify-objects"])
        .arg("--json")
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).unwrap();
    assert!(
        value["remaining_findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["kind"] == "gc_sweep_marker_corrupt")
    );
    assert!(
        !value["remaining_findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["kind"] == "gc_sweep_incomplete")
    );
    assert!(!tree_path.exists());
    assert!(dir.path().join(".kio/index/sqlite.db").is_file());
}

#[test]
fn mismatched_frozen_marker_is_corruption_not_recovery_pending() {
    let (dir, commit, tree) = fixture_without_index();
    let wrong_tree = format!("sha256:{}", "f".repeat(64));
    assert_ne!(tree, wrong_tree);
    write_marker(&dir, &commit, &wrong_tree, GcSweepPhase::Prepared);

    let output = kio(&dir, &["repair", "verify-objects"])
        .arg("--json")
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).unwrap();
    assert!(
        value["remaining_findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["kind"] == "gc_sweep_marker_corrupt")
    );
}

#[test]
fn phase_impossible_marker_is_corruption_not_recovery_pending() {
    let (dir, commit, tree) = fixture_without_index();
    // `prepared` is the only phase before receipt publication.  A matching
    // receipt with an absent tree is syntactically plausible but cannot arise
    // from the durable state machine and must not be advertised as resumable.
    write_receipt(&dir, &commit, &tree);
    let store = ObjectStore::new(dir.path().join(".kio"));
    fs::remove_file(store.object_path(ObjectKind::Tree, &tree).unwrap()).unwrap();

    write_marker(&dir, &commit, &tree, GcSweepPhase::Prepared);

    let output = kio(&dir, &["repair", "verify-objects"])
        .arg("--json")
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).unwrap();
    assert!(
        value["remaining_findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["kind"] == "gc_sweep_marker_corrupt")
    );
    assert!(
        !value["remaining_findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["kind"] == "gc_sweep_incomplete")
    );
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn final_shallow_ancestor_with_chunks_keeps_verify_and_rebuild_available() {
    let dir = canonical_tempdir();
    fs::write(
        dir.path().join("doc.md"),
        "# old snapshot\n\nA sufficiently long chunked paragraph for the old snapshot.\n",
    )
    .unwrap();
    const OLD: &str = "2025-01-01T00:00:00Z";
    const NOW: &str = "2026-08-14T00:00:00Z";
    json_success_at(&dir, &["init"], NOW);
    let old = json_success_at(&dir, &["index", "--offline", "--yes"], OLD);
    let old_commit = old["commit_hash"].as_str().unwrap().to_owned();
    assert!(dir.path().join(".kio/index/chunks.jsonl").is_file());
    fs::write(
        dir.path().join("doc.md"),
        "# current snapshot\n\nnew content\n",
    )
    .unwrap();
    json_success_at(&dir, &["index", "--offline", "--yes"], NOW);
    let repo = Repository::open(dir.path()).unwrap();
    let old_tree = repo.read_commit(&old_commit).unwrap().tree;
    let old_tree_path = ObjectStore::new(repo.kio_dir())
        .object_path(ObjectKind::Tree, &old_tree)
        .unwrap();

    json_success_at(&dir, &["gc", "--yes"], NOW);
    assert!(!old_tree_path.exists());
    assert_eq!(
        json_success_at(&dir, &["repair", "verify-objects"], NOW)["status"],
        "ok"
    );
    assert_eq!(
        json_success_at(&dir, &["repair", "rebuild-db"], NOW)["status"],
        "rebuilt"
    );
}

#[test]
fn unrelated_ledger_introduction_cannot_hide_an_unreachable_chunk() {
    let (dir, commit, tree) = fixture();
    write_receipt(&dir, &commit, &tree);
    let store = ObjectStore::new(dir.path().join(".kio"));
    fs::remove_file(store.object_path(ObjectKind::Tree, &tree).unwrap()).unwrap();

    fn remove_files(directory: &std::path::Path) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                remove_files(&path);
            } else {
                fs::remove_file(path).unwrap();
            }
        }
    }
    remove_files(&dir.path().join(".kio/objects/manifests"));

    // Add a tagged introduction naming the shallow commit, but leave its
    // immutable manifest closure deliberately absent. The event cannot
    // authenticate without that commit's tree and must not hide the orphan.
    let ledger = dir.path().join(".kio/index/chunks.jsonl");
    let rows = fs::read_to_string(&ledger).unwrap();
    assert!(!rows.is_empty());
    let creation: Value = serde_json::from_str(rows.lines().next().unwrap()).unwrap();
    let event = serde_json::json!({
        "event": "publication",
        "chunk_id": creation["chunk_id"],
        "chunking_config_hash": creation["chunking_config_hash"],
        "introduction_commit": commit,
    });
    let rows = format!(
        "{}\n{}\n",
        serde_json::to_string(&creation).unwrap(),
        serde_json::to_string(&event).unwrap(),
    );
    fs::write(&ledger, rows).unwrap();

    let output = kio(&dir, &["repair", "verify-objects"])
        .arg("--json")
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let report: Value = serde_json::from_slice(&output).unwrap();
    assert!(
        report["remaining_findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["kind"] == "chunk_unit_content_unreachable")
    );
}
