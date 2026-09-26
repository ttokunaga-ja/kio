//! Exact-only Evidence retarget integration contracts.

mod support;

use support::canonical_tempdir;

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use kio_core::cas::{ContentObjectKind, ObjectKind, ObjectStore, hash_bytes};
use kio_core::gc::ShallowReceipt;
use kio_core::purge::{LifecycleEvent, PurgeReason, PurgeState, TombstoneMode};
use kio_core::scope::Repository;
use kio_index::registry::{RegistryDb, RegistryEntry};
use serde_json::{Value, json};
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

fn run(dir: &TempDir, args: &[&str]) -> (i32, Value) {
    let output = kio(dir, args).arg("--json").output().unwrap();
    let stream = if output.stdout.is_empty() {
        &output.stderr
    } else {
        &output.stdout
    };
    (
        output.status.code().unwrap(),
        serde_json::from_slice(stream).unwrap(),
    )
}

/// Retarget failures are raw command failures: their structured error must be
/// emitted exactly once on stderr, with no partially-issued response on stdout.
fn failure(dir: &TempDir, args: &[&str], expected_code: i32, expected_error: &str) -> Value {
    let output = kio(dir, args).arg("--json").output().unwrap();
    assert_eq!(output.status.code(), Some(expected_code), "{output:?}");
    assert!(
        output.stdout.is_empty(),
        "retarget must not publish partial stdout on failure: {output:?}"
    );
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error_code"], expected_error, "{error}");
    error
}

fn make_commit_final_shallow(dir: &TempDir, commit_hash: &str) {
    let repo = Repository::open(dir.path()).unwrap();
    let commit = repo.read_commit(commit_hash).unwrap();
    let receipt_path = dir
        .path()
        .join(".kio/gc/shallowed")
        .join(commit_hash.strip_prefix("sha256:").unwrap());
    fs::create_dir_all(receipt_path.parent().unwrap()).unwrap();
    let receipt = ShallowReceipt::new(
        commit_hash.to_owned(),
        commit.tree.clone(),
        "2026-08-22T00:00:00Z".to_owned(),
    )
    .unwrap();
    fs::write(receipt_path, receipt.canonical_bytes().unwrap()).unwrap();
    let store = ObjectStore::new(dir.path().join(".kio"));
    fs::remove_file(store.object_path(ObjectKind::Tree, &commit.tree).unwrap()).unwrap();
}

fn registry_path(dir: &TempDir) -> std::path::PathBuf {
    dir.path().join(".test-data/kio/scope-registry.sqlite")
}

fn make_registry_duplicate(dir_a: &TempDir, scope_id: &str) -> TempDir {
    let dir_b = canonical_tempdir();
    success(&dir_b, &["init"]);
    let scope_path = dir_b.path().join(".kio/scope.json");
    let mut scope: Value = serde_json::from_slice(&fs::read(&scope_path).unwrap()).unwrap();
    scope["scope_id"] = json!(scope_id);
    fs::write(&scope_path, serde_json::to_vec_pretty(&scope).unwrap()).unwrap();

    let registry = RegistryDb::open(registry_path(dir_a)).unwrap();
    for (kio_path, root_path, last_seen_at) in [
        (
            dir_a.path().join(".kio"),
            dir_a.path().to_path_buf(),
            "2020-01-01T00:00:00Z",
        ),
        (
            dir_b.path().join(".kio"),
            dir_b.path().to_path_buf(),
            "2099-01-01T00:00:00Z",
        ),
    ] {
        registry
            .upsert(&RegistryEntry {
                scope_id: scope_id.to_owned(),
                kio_path: kio_path.display().to_string(),
                root_path: root_path.display().to_string(),
                participates_in_global_search: true,
                indexed: true,
                last_seen_at: last_seen_at.to_owned(),
            })
            .unwrap();
    }
    dir_b
}

fn bump_format_version(dir: &TempDir) {
    let path = dir.path().join(".kio/scope.json");
    let mut scope: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    scope["kio_format_version"] = json!("9.0.0");
    fs::write(path, serde_json::to_vec_pretty(&scope).unwrap()).unwrap();
}

/// A deterministic content-only snapshot: intentionally excludes inode,
/// timestamp, permission, and atime metadata so read-only assertions cover
/// precisely every durable `.kio` pathname and byte payload.
fn kio_file_snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn visit(root: &Path, directory: &Path, output: &mut Vec<(PathBuf, Vec<u8>)>) {
        let mut entries = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                visit(root, &path, output);
            } else {
                assert!(
                    kind.is_file(),
                    "unexpected non-file fixture entry: {path:?}"
                );
                output.push((
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(path).unwrap(),
                ));
            }
        }
    }

    let mut snapshot = Vec::new();
    visit(root, root, &mut snapshot);
    snapshot
}

fn fixture_with_later_commit() -> (TempDir, Value, String) {
    let dir = canonical_tempdir();
    fs::write(
        dir.path().join("evidence.md"),
        "# Evidence\n\nTTL is 3600 seconds.\n",
    )
    .unwrap();
    success(&dir, &["init"]);
    success(&dir, &["index", "--offline", "--yes"]);
    let search = success(&dir, &["search", "3600", "--mode", "text"]);
    let pointer = search["results"][0]["evidence_pointer"].clone();

    // The source document remains byte-identical; a new history commit gives
    // retarget an exact later tree without relying on HEAD aliases.
    fs::write(
        dir.path().join("unrelated.md"),
        "# Later\n\nNo Evidence change.\n",
    )
    .unwrap();
    success(&dir, &["index", "--offline", "--yes"]);
    let repo = Repository::open(dir.path()).unwrap();
    let target = repo.head_commit_hash().unwrap().unwrap();
    (dir, pointer, target)
}

fn fixture_with_duplicate_heading_candidates() -> (TempDir, Value, String) {
    let dir = canonical_tempdir();
    fs::write(
        dir.path().join("evidence.md"),
        "# Evidence\n\nFirst 3600-second statement.\n\n# Evidence\n\nSecond 3600-second statement.\n",
    )
    .unwrap();
    success(&dir, &["init"]);
    success(&dir, &["index", "--offline", "--yes"]);
    let search = success(&dir, &["search", "3600", "--mode", "text"]);
    let pointer = search["results"][0]["evidence_pointer"].clone();
    fs::write(dir.path().join("unrelated.md"), "# Later\n\nNo change.\n").unwrap();
    success(&dir, &["index", "--offline", "--yes"]);
    let target = Repository::open(dir.path())
        .unwrap()
        .head_commit_hash()
        .unwrap()
        .unwrap();
    (dir, pointer, target)
}

#[test]
fn retarget_is_exact_deterministic_and_preserves_scope_bytes() {
    let (dir, pointer, target) = fixture_with_later_commit();
    let before = fs::read(dir.path().join(".kio/scope.json")).unwrap();
    let store_before = kio_file_snapshot(&dir.path().join(".kio"));
    let pointer_text = serde_json::to_string(&pointer).unwrap();
    let args = [
        "evidence",
        "retarget",
        pointer_text.as_str(),
        "--at",
        target.as_str(),
    ];
    let first = success(&dir, &args);
    assert_eq!(kio_file_snapshot(&dir.path().join(".kio")), store_before);
    let second = success(&dir, &args);
    assert_eq!(kio_file_snapshot(&dir.path().join(".kio")), store_before);
    assert_eq!(first, second);
    assert_eq!(
        first
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![
            "match_method",
            "new_pointer",
            "retargeted_from",
            "schema",
            "schema_version",
            "status",
            "target_commit",
        ]
    );
    assert_eq!(first["schema"], "kio.evidence.retarget");
    assert_eq!(first["schema_version"], 1);
    assert_eq!(first["status"], "retargeted");
    assert_eq!(first["target_commit"], target);
    assert_eq!(first["match_method"], "heading_path_exact");
    assert!(first.get("fuzzy").is_none());
    assert!(first.get("confidence").is_none());
    assert!(first.get("latest").is_none());
    assert!(first.get("old").is_none());
    assert_eq!(
        fs::read(dir.path().join(".kio/scope.json")).unwrap(),
        before
    );

    let issued = serde_json::to_string(&first["new_pointer"]).unwrap();
    let verified = success(&dir, &["evidence", "verify", &issued, "--strict"]);
    assert_eq!(verified["status"], "alive");
    let batch = dir.path().join("retarget-pointer.jsonl");
    fs::write(&batch, format!("{issued}\n")).unwrap();
    let batch_verified = success(
        &dir,
        &[
            "evidence",
            "verify",
            "--batch",
            batch.to_str().unwrap(),
            "--strict",
        ],
    );
    assert_eq!(batch_verified["results"][0]["result"]["status"], "alive");
}

#[test]
fn retarget_ignores_forged_optional_heading_but_requires_exact_at() {
    let (dir, mut pointer, target) = fixture_with_later_commit();
    pointer["heading_path"] = json!(["forged", "presentation", "only"]);
    pointer["section_id"] = json!("forged");
    pointer["byte_start"] = json!(999_u64);
    pointer["byte_end"] = json!(1000_u64);
    let forged = serde_json::to_string(&pointer).unwrap();
    let output = success(&dir, &["evidence", "retarget", &forged, "--at", &target]);
    assert_eq!(output["target_commit"], target);
    assert_eq!(output["new_pointer"]["heading_path"], json!(["Evidence"]));

    let missing_at = kio(&dir, &["evidence", "retarget", &forged])
        .arg("--json")
        .assert();
    missing_at.code(2);
    let latest = kio(&dir, &["evidence", "retarget", &forged, "--at", "latest"])
        .arg("--json")
        .assert();
    latest.code(2);

    let oversized = "x".repeat(64 * 1024 + 1);
    #[cfg(not(windows))]
    failure(
        &dir,
        &["evidence", "retarget", &oversized, "--at", &target],
        2,
        "KIO-E-CONFIG-USAGE-001",
    );
    #[cfg(windows)]
    {
        let output = kio(&dir, &["evidence", "retarget", "-", "--at", &target])
            .arg("--json")
            .write_stdin(oversized)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(
            output.stdout.is_empty(),
            "retarget must not publish partial stdout on failure: {output:?}"
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stderr).unwrap()["error_code"],
            "KIO-E-CONFIG-USAGE-001"
        );
    }
}

#[test]
fn retarget_at_is_a_direct_exact_commit_object_not_a_ref() {
    let (dir, pointer, target) = fixture_with_later_commit();
    // Optional path remains presentation-only; `--at` itself must still be a
    // direct object read rather than a ref/alias resolver.
    let mut wrong_path = pointer.clone();
    wrong_path["path_at_commit"] = json!("other.md");
    // A well-formed absent object is rejected as usage rather than being
    // interpreted as a ref, tag, HEAD, or latest selector.
    let wrong_target = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
    let text = serde_json::to_string(&wrong_path).unwrap();
    let (code, error) = run(&dir, &["evidence", "retarget", &text, "--at", wrong_target]);
    assert_eq!(code, 2);
    assert_eq!(error["error_code"], "KIO-E-CONFIG-USAGE-001");
    assert_ne!(target, wrong_target);
}

#[test]
fn retarget_zero_target_match_is_dedicated_not_found_without_stdout() {
    let dir = canonical_tempdir();
    fs::write(
        dir.path().join("evidence.md"),
        "# Evidence\n\nTTL is 3600 seconds.\n",
    )
    .unwrap();
    success(&dir, &["init"]);
    success(&dir, &["index", "--offline", "--yes"]);
    let old =
        success(&dir, &["search", "3600", "--mode", "text"])["results"][0]["evidence_pointer"]
            .clone();

    // A new exact target whose tree has no entry for the old raw hash must
    // not fall back to a historical SQLite placement or an alternate path.
    fs::write(
        dir.path().join("evidence.md"),
        "# Evidence\n\nTTL changed to 7200 seconds.\n",
    )
    .unwrap();
    success(&dir, &["index", "--offline", "--yes"]);
    let target = Repository::open(dir.path())
        .unwrap()
        .head_commit_hash()
        .unwrap()
        .unwrap();
    let old = serde_json::to_string(&old).unwrap();
    failure(
        &dir,
        &["evidence", "retarget", &old, "--at", &target],
        4,
        "KIO-E-EVIDENCE-RETARGET-NOT-FOUND-001",
    );
}

#[test]
fn retarget_duplicate_raw_heading_candidates_are_ambiguous_without_stdout() {
    let (dir, pointer, target) = fixture_with_duplicate_heading_candidates();
    let before = kio_file_snapshot(&dir.path().join(".kio"));
    let pointer_text = serde_json::to_string(&pointer).unwrap();
    failure(
        &dir,
        &["evidence", "retarget", &pointer_text, "--at", &target],
        4,
        "KIO-E-EVIDENCE-RETARGET-AMBIG-001",
    );
    assert_eq!(kio_file_snapshot(&dir.path().join(".kio")), before);
}

#[test]
fn retarget_duplicate_raw_placement_uses_only_the_exact_old_path() {
    let (dir, pointer, _) = fixture_with_later_commit();
    fs::copy(
        dir.path().join("evidence.md"),
        dir.path().join("duplicate.md"),
    )
    .unwrap();
    success(&dir, &["index", "--offline", "--yes"]);
    let repo = Repository::open(dir.path()).unwrap();
    let duplicate_target = repo.head_commit_hash().unwrap().unwrap();
    let pointer_text = serde_json::to_string(&pointer).unwrap();
    let retargeted = success(
        &dir,
        &[
            "evidence",
            "retarget",
            &pointer_text,
            "--at",
            &duplicate_target,
        ],
    );
    assert_eq!(retargeted["new_pointer"]["path_at_commit"], "evidence.md");

    // Once only the alternate placement remains, exact retargeting must not
    // treat it as a rename/move fallback.
    fs::remove_file(dir.path().join("evidence.md")).unwrap();
    success(&dir, &["index", "--offline", "--yes"]);
    let moved_target = Repository::open(dir.path())
        .unwrap()
        .head_commit_hash()
        .unwrap()
        .unwrap();
    failure(
        &dir,
        &["evidence", "retarget", &pointer_text, "--at", &moved_target],
        4,
        "KIO-E-EVIDENCE-RETARGET-NOT-FOUND-001",
    );
}

#[test]
fn retarget_old_shallow_is_retryable_without_stdout() {
    let (old_dir, old_pointer, target) = fixture_with_later_commit();
    make_commit_final_shallow(&old_dir, old_pointer["commit"].as_str().unwrap());
    let old_text = serde_json::to_string(&old_pointer).unwrap();
    failure(
        &old_dir,
        &["evidence", "retarget", &old_text, "--at", &target],
        3,
        "KIO-E-COMMIT-SHALLOW-001",
    );
}

#[test]
fn retarget_target_shallow_is_retryable_without_stdout() {
    let (target_dir, target_pointer, target) = fixture_with_later_commit();
    // A final shallow receipt is valid only for a non-tip commit. Advance the
    // head before discarding the requested target tree.
    fs::write(target_dir.path().join("advance.md"), "# Advance\n\nbody\n").unwrap();
    success(&target_dir, &["index", "--offline", "--yes"]);
    make_commit_final_shallow(&target_dir, &target);
    let target_text = serde_json::to_string(&target_pointer).unwrap();
    failure(
        &target_dir,
        &["evidence", "retarget", &target_text, "--at", &target],
        3,
        "KIO-E-COMMIT-SHALLOW-001",
    );
}

#[test]
fn retarget_missing_target_manifest_is_store_corruption() {
    let (manifest_dir, pointer, target) = fixture_with_later_commit();
    let repo = Repository::open(manifest_dir.path()).unwrap();
    let commit = repo.read_commit(&target).unwrap();
    let tree = repo.read_tree(&commit.tree).unwrap();
    let entry = tree
        .entries
        .iter()
        .find(|entry| entry.raw_hash == pointer["raw_hash"].as_str().unwrap())
        .unwrap();
    let manifest = &entry.normalize.as_ref().unwrap().manifest_hash;
    let store = ObjectStore::new(manifest_dir.path().join(".kio"));
    fs::remove_file(
        store
            .content_path(ContentObjectKind::Manifest, manifest)
            .unwrap(),
    )
    .unwrap();
    let pointer_text = serde_json::to_string(&pointer).unwrap();
    failure(
        &manifest_dir,
        &["evidence", "retarget", &pointer_text, "--at", &target],
        4,
        "KIO-E-STORE-CORRUPT-001",
    );
}

#[test]
fn retarget_missing_old_chunk_is_store_corruption() {
    let (chunk_dir, pointer, target) = fixture_with_later_commit();
    let store = ObjectStore::new(chunk_dir.path().join(".kio"));
    fs::remove_file(
        store
            .chunk_path(pointer["chunk_hash"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    let pointer_text = serde_json::to_string(&pointer).unwrap();
    failure(
        &chunk_dir,
        &["evidence", "retarget", &pointer_text, "--at", &target],
        4,
        "KIO-E-STORE-CORRUPT-001",
    );
}

#[test]
fn retarget_active_purge_is_retryable_and_read_only() {
    let (dir, pointer, target) = fixture_with_later_commit();
    let raw_hash = pointer["raw_hash"].as_str().unwrap().to_owned();
    let purge = PurgeState::open(dir.path().join(".kio")).unwrap();
    purge
        .begin(
            vec![raw_hash],
            PurgeReason::Legal,
            TombstoneMode::Default,
            "test",
            "2026-08-22T00:00:00Z",
            1,
            hash_bytes(b"planned retarget purge commit"),
            hash_bytes(b"planned retarget purge closure"),
            kio_core::scope::new_ulid(dir.path()),
        )
        .unwrap();
    let scope_before = fs::read(dir.path().join(".kio/scope.json")).unwrap();
    let journal_before = fs::read(purge.journal_path()).unwrap();
    let pointer_text = serde_json::to_string(&pointer).unwrap();
    failure(
        &dir,
        &["evidence", "retarget", &pointer_text, "--at", &target],
        3,
        "KIO-E-PURGE-JOURNAL-ACTIVE-001",
    );
    assert_eq!(
        fs::read(dir.path().join(".kio/scope.json")).unwrap(),
        scope_before
    );
    assert_eq!(fs::read(purge.journal_path()).unwrap(), journal_before);
}

#[test]
fn retarget_preserves_canonical_tombstoned_and_not_found_failures() {
    for (erase_tombstone, expected_error) in [
        (false, "KIO-E-PURGE-TOMBSTONED-001"),
        (true, "KIO-E-PURGE-NOT-FOUND-001"),
    ] {
        let (dir, pointer, target) = fixture_with_later_commit();
        let raw_hash = pointer["raw_hash"].as_str().unwrap();
        let mut arguments = vec![
            "purge",
            "--raw-hash",
            raw_hash,
            "--reason",
            "legal",
            "--yes",
        ];
        if erase_tombstone {
            arguments.push("--erase-tombstone");
        }
        success(&dir, &arguments);
        let pointer_text = serde_json::to_string(&pointer).unwrap();
        failure(
            &dir,
            &["evidence", "retarget", &pointer_text, "--at", &target],
            4,
            expected_error,
        );
    }
}

#[test]
fn retarget_marker_classification_precedes_old_manifest_preflight() {
    let (dir, pointer, target) = fixture_with_later_commit();
    let raw_hash = pointer["raw_hash"].as_str().unwrap();
    let repo = Repository::open(dir.path()).unwrap();
    let old_commit = repo
        .read_commit(pointer["commit"].as_str().unwrap())
        .unwrap();
    let old_tree = repo.read_tree(&old_commit.tree).unwrap();
    let manifest_hash = &old_tree
        .entries
        .iter()
        .find(|entry| entry.raw_hash == raw_hash)
        .unwrap()
        .normalize
        .as_ref()
        .unwrap()
        .manifest_hash;
    let store = ObjectStore::new(dir.path().join(".kio"));
    let purge = PurgeState::open(dir.path().join(".kio")).unwrap();
    purge
        .append_tombstone_event(
            raw_hash,
            LifecycleEvent::purged(
                "2026-08-22T00:00:00Z",
                &target,
                PurgeReason::Legal,
                "test",
                1,
            ),
        )
        .unwrap();
    fs::remove_file(store.object_path(ObjectKind::Raw, raw_hash).unwrap()).unwrap();
    // Bring the derived index lifecycle checkpoint forward before corrupting
    // the retained historical manifest that marker classification must ignore.
    success(&dir, &["repair", "rebuild-db"]);
    fs::write(
        store
            .content_path(ContentObjectKind::Manifest, manifest_hash)
            .unwrap(),
        b"not a canonical manifest",
    )
    .unwrap();

    let pointer_text = serde_json::to_string(&pointer).unwrap();
    failure(
        &dir,
        &["evidence", "retarget", &pointer_text, "--at", &target],
        4,
        "KIO-E-PURGE-TOMBSTONED-001",
    );
}

#[test]
fn retarget_missing_sqlite_is_shared_index_rebuilding_failure() {
    let (dir, pointer, target) = fixture_with_later_commit();
    fs::remove_file(dir.path().join(".kio/index/sqlite.db")).unwrap();
    let scope_before = fs::read(dir.path().join(".kio/scope.json")).unwrap();
    let pointer_text = serde_json::to_string(&pointer).unwrap();
    failure(
        &dir,
        &["evidence", "retarget", &pointer_text, "--at", &target],
        3,
        "KIO-E-INDEX-REBUILDING-001",
    );
    assert_eq!(
        fs::read(dir.path().join(".kio/scope.json")).unwrap(),
        scope_before
    );
    assert!(
        !dir.path().join(".kio/index/sqlite.db").exists(),
        "read-only retarget must not recreate a missing index"
    );
}

#[test]
fn retarget_incompatible_format_is_shared_failure_and_does_not_rewrite_scope() {
    let (dir, pointer, target) = fixture_with_later_commit();
    bump_format_version(&dir);
    let scope_before = fs::read(dir.path().join(".kio/scope.json")).unwrap();
    let pointer_text = serde_json::to_string(&pointer).unwrap();
    failure(
        &dir,
        &["evidence", "retarget", &pointer_text, "--at", &target],
        8,
        "KIO-E-STORE-VERSION-001",
    );
    assert_eq!(
        fs::read(dir.path().join(".kio/scope.json")).unwrap(),
        scope_before
    );
}

#[test]
fn retarget_scope_unreachable_is_shared_retryable_failure() {
    let (dir, mut pointer, target) = fixture_with_later_commit();
    pointer["scope_id"] = json!("scope_unregistered_retarget_target");
    pointer["scope_path"] = json!(dir.path().join("gone/.kio").display().to_string());
    let scope_before = fs::read(dir.path().join(".kio/scope.json")).unwrap();
    let pointer_text = serde_json::to_string(&pointer).unwrap();
    failure(
        &dir,
        &["evidence", "retarget", &pointer_text, "--at", &target],
        3,
        "KIO-E-EVIDENCE-SCOPE-UNREACHABLE-001",
    );
    assert_eq!(
        fs::read(dir.path().join(".kio/scope.json")).unwrap(),
        scope_before
    );
}

#[test]
fn retarget_live_registry_duplicate_is_shared_retryable_failure() {
    let (dir, mut pointer, target) = fixture_with_later_commit();
    let _other = make_registry_duplicate(&dir, pointer["scope_id"].as_str().unwrap());
    // Force registry resolution rather than trusting the local presentation
    // path; duplicate live rows must still fail closed before target work.
    pointer["scope_path"] = json!(dir.path().join("gone/.kio").display().to_string());
    let scope_before = fs::read(dir.path().join(".kio/scope.json")).unwrap();
    let pointer_text = serde_json::to_string(&pointer).unwrap();
    failure(
        &dir,
        &["evidence", "retarget", &pointer_text, "--at", &target],
        3,
        "KIO-E-REGISTRY-DUP-001",
    );
    assert_eq!(
        fs::read(dir.path().join(".kio/scope.json")).unwrap(),
        scope_before
    );
}

/// Snapshot integrity failures are command-level for retarget too: a valid
/// pointer hint cannot bypass an unsafe registry leaf, and no new pointer is
/// emitted.
#[cfg(unix)]
#[test]
fn retarget_unsafe_registry_snapshot_has_the_shared_command_error() {
    let (dir, pointer, target) = fixture_with_later_commit();
    let registry = registry_path(&dir);
    let victim = dir.path().join("retarget-registry-snapshot-victim");
    fs::write(&victim, b"registry victim").unwrap();
    fs::remove_file(&registry).unwrap();
    std::os::unix::fs::symlink(&victim, &registry).unwrap();

    let pointer_text = serde_json::to_string(&pointer).unwrap();
    let error = failure(
        &dir,
        &["evidence", "retarget", &pointer_text, "--at", &target],
        4,
        "KIO-E-REGISTRY-SNAPSHOT-UNSAFE-001",
    );
    assert_eq!(error["context"]["reason"], "unsafe_integrity");
    assert_eq!(fs::read(&victim).unwrap(), b"registry victim");
}

#[test]
fn retarget_malformed_registry_snapshot_is_unsafe_without_hint_fallback() {
    let (dir, pointer, target) = fixture_with_later_commit();
    let registry = registry_path(&dir);
    fs::remove_file(&registry).unwrap();
    let malformed = rusqlite::Connection::open(&registry).unwrap();
    malformed
        .execute_batch("CREATE TABLE scopes(scope_id TEXT)")
        .unwrap();
    drop(malformed);
    let registry_before = fs::read(&registry).unwrap();
    let scope_before = fs::read(dir.path().join(".kio/scope.json")).unwrap();
    let pointer_text = serde_json::to_string(&pointer).unwrap();
    let error = failure(
        &dir,
        &["evidence", "retarget", &pointer_text, "--at", &target],
        4,
        "KIO-E-REGISTRY-SNAPSHOT-UNSAFE-001",
    );
    assert_eq!(error["context"]["reason"], "unsafe_integrity");
    assert_eq!(fs::read(&registry).unwrap(), registry_before);
    assert_eq!(
        fs::read(dir.path().join(".kio/scope.json")).unwrap(),
        scope_before
    );
}

#[cfg(unix)]
fn geteuid_is_root() -> bool {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: libc's geteuid has no preconditions and returns this process's
    // effective uid.
    unsafe { geteuid() == 0 }
}

#[cfg(unix)]
struct PermissionRestore {
    path: PathBuf,
    original: std::fs::Permissions,
}

#[cfg(unix)]
impl Drop for PermissionRestore {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.path, self.original.clone());
    }
}

#[cfg(unix)]
#[test]
fn retarget_unreadable_registry_snapshot_is_retryable_without_hint_fallback() {
    use std::os::unix::fs::PermissionsExt;

    if geteuid_is_root() {
        return;
    }
    let (dir, pointer, target) = fixture_with_later_commit();
    let registry = registry_path(&dir);
    let registry_before = fs::read(&registry).unwrap();
    let scope_before = fs::read(dir.path().join(".kio/scope.json")).unwrap();
    let original = fs::metadata(&registry).unwrap().permissions();
    let restore = PermissionRestore {
        path: registry.clone(),
        original,
    };
    fs::set_permissions(&registry, fs::Permissions::from_mode(0o000)).unwrap();
    let pointer_text = serde_json::to_string(&pointer).unwrap();
    let error = failure(
        &dir,
        &["evidence", "retarget", &pointer_text, "--at", &target],
        3,
        "KIO-E-REGISTRY-SNAPSHOT-001",
    );
    assert_eq!(error["context"]["reason"], "unstable_or_busy");
    drop(restore);
    assert_eq!(fs::read(&registry).unwrap(), registry_before);
    assert_eq!(
        fs::read(dir.path().join(".kio/scope.json")).unwrap(),
        scope_before
    );
}

#[cfg(unix)]
#[test]
fn retarget_rejects_linked_or_nonregular_old_chunk_without_touching_victim() {
    enum Replacement {
        Symlink,
        Hardlink,
        Directory,
    }

    for replacement in [
        Replacement::Symlink,
        Replacement::Hardlink,
        Replacement::Directory,
    ] {
        let (dir, pointer, target) = fixture_with_later_commit();
        let store = ObjectStore::new(dir.path().join(".kio"));
        let chunk_path = store
            .chunk_path(pointer["chunk_hash"].as_str().unwrap())
            .unwrap();
        let victim = dir.path().join("victim-do-not-touch");
        let victim_bytes = b"retarget link victim bytes".to_vec();
        fs::write(&victim, &victim_bytes).unwrap();
        fs::remove_file(&chunk_path).unwrap();
        match replacement {
            Replacement::Symlink => std::os::unix::fs::symlink(&victim, &chunk_path).unwrap(),
            Replacement::Hardlink => fs::hard_link(&victim, &chunk_path).unwrap(),
            Replacement::Directory => fs::create_dir(&chunk_path).unwrap(),
        }
        let pointer_text = serde_json::to_string(&pointer).unwrap();
        failure(
            &dir,
            &["evidence", "retarget", &pointer_text, "--at", &target],
            4,
            "KIO-E-STORE-CORRUPT-001",
        );
        assert_eq!(fs::read(&victim).unwrap(), victim_bytes);
    }
}
