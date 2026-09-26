mod support;

use support::canonical_tempdir;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use assert_cmd::Command;
use kio_core::cas::{ObjectKind, ObjectStore, fanout_path};
use kio_core::gc::ShallowReceipt;
use kio_core::scope::Repository;
use serde_json::{Value, json};
use tempfile::TempDir;

const CHILD_ENV_DENYLIST: &[&str] = &[
    "GEMINI_API_KEY",
    "MISTRAL_API_KEY",
    "KIO_FIXED_NOW",
    "KIO_TEST_GEMINI_EMBED",
    "KIO_TEST_MISTRAL_OCR",
    "KIO_TEST_MARKDOWNIZE_ADAPTER",
];

fn kio(dir: &TempDir, args: &[&str]) -> Command {
    let mut command = Command::cargo_bin("kio").unwrap();
    for name in CHILD_ENV_DENYLIST {
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
    let output = kio(dir, args)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

fn json_failure(dir: &TempDir, args: &[&str], code: i32) -> Value {
    let output = kio(dir, args)
        .arg("--json")
        .assert()
        .code(code)
        .get_output()
        .stderr
        .clone();
    serde_json::from_slice(&output).unwrap()
}

fn init(dir: &TempDir) {
    json_success(dir, &["init"]);
}

fn snapshot(dir: &TempDir, message: &str) -> String {
    json_success(dir, &["snapshot", "create", "-m", message])["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn inspect(dir: &TempDir, hash: &str) -> Value {
    json_success(dir, &["inspect", hash])
}

fn path_text(path: &std::path::Path) -> String {
    path.to_str().unwrap().to_owned()
}

#[test]
fn ct4_restore_commit_restores_verified_files_and_empty_commit() {
    let dir = canonical_tempdir();
    // PA16/17 (§D, U25): `--to` must resolve OUTSIDE the scope root
    // entirely (not merely outside `.kio`) — a sibling TempDir stands in for
    // "some other directory on disk" throughout this file.
    let out = TempDir::new().unwrap();
    init(&dir);
    fs::write(dir.path().join("a.md"), b"alpha").unwrap();
    fs::write(dir.path().join("b.md"), b"beta").unwrap();
    let commit = snapshot(&dir, "two files");
    let destination = out.path().join("recovered");
    let output = json_success(&dir, &["export", &commit, "--to", &path_text(&destination)]);
    assert_eq!(output["status"], "exported");
    assert_eq!(output["source_kind"], "commit");
    assert_eq!(output["source_commit"], commit);
    assert_eq!(output["exported_count"], 2);
    assert_eq!(output["overwritten_count"], 0);
    assert_eq!(fs::read(destination.join("a.md")).unwrap(), b"alpha");
    assert_eq!(fs::read(destination.join("b.md")).unwrap(), b"beta");

    fs::remove_file(dir.path().join("a.md")).unwrap();
    fs::remove_file(dir.path().join("b.md")).unwrap();
    let empty = snapshot(&dir, "empty");
    let empty_destination = out.path().join("empty-recovered");
    let output = json_success(
        &dir,
        &["export", &empty, "--to", &path_text(&empty_destination)],
    );
    assert_eq!(output["exported_count"], 0);
    assert!(empty_destination.is_dir());
}

#[test]
fn ct4_restore_deleted_path_uses_newest_first_parent_binding() {
    let dir = canonical_tempdir();
    let out = TempDir::new().unwrap();
    init(&dir);
    fs::write(dir.path().join("deleted.md"), b"historical").unwrap();
    let old_commit = snapshot(&dir, "old");
    fs::remove_file(dir.path().join("deleted.md")).unwrap();
    snapshot(&dir, "deleted");

    let destination = out.path().join("path-export");
    let output = json_success(
        &dir,
        &["export", "deleted.md", "--to", &path_text(&destination)],
    );
    assert_eq!(output["source_kind"], "path");
    assert_eq!(output["source_commit"], old_commit);
    assert_eq!(output["exported_count"], 1);
    assert_eq!(
        fs::read(destination.join("deleted.md")).unwrap(),
        b"historical"
    );
}

#[test]
fn ct4_restore_preflight_no_clobber_and_force_confirmation() {
    let dir = canonical_tempdir();
    let out = TempDir::new().unwrap();
    init(&dir);
    fs::write(dir.path().join("a.md"), b"alpha").unwrap();
    fs::write(dir.path().join("b.md"), b"beta").unwrap();
    let commit = snapshot(&dir, "source");
    let destination = out.path().join("conflict");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("b.md"), b"existing").unwrap();

    // R23-26 (06 §5 L282-285): KIO-E-COMMIT-EXPORT-CONFLICT-001 is
    // documented as always-retryable exit 3, not the generic exit 1 this
    // preflight rejection used before the fix.
    let error = json_failure(
        &dir,
        &["export", &commit, "--to", &path_text(&destination)],
        3,
    );
    assert_eq!(error["error_code"], "KIO-E-COMMIT-EXPORT-CONFLICT-001");
    assert_eq!(error["context"]["retry_disposition"], "manual_action");
    assert!(!destination.join("a.md").exists());
    assert_eq!(fs::read(destination.join("b.md")).unwrap(), b"existing");

    let error = json_failure(
        &dir,
        &[
            "export",
            &commit,
            "--to",
            &path_text(&destination),
            "--force",
        ],
        9,
    );
    assert_eq!(error["error_code"], "KIO-E-CONFIRM-REJECTED-001");

    let output = json_success(
        &dir,
        &[
            "export",
            &commit,
            "--to",
            &path_text(&destination),
            "--force",
            "--yes",
        ],
    );
    assert_eq!(output["exported_count"], 2);
    assert_eq!(output["overwritten_count"], 1);
    assert_eq!(fs::read(destination.join("a.md")).unwrap(), b"alpha");
    assert_eq!(fs::read(destination.join("b.md")).unwrap(), b"beta");
}

/// R23-26 (06 §5 L282-285): the plain "no --force, destination file already
/// exists" preflight rejection carries `KIO-E-COMMIT-EXPORT-CONFLICT-001`'s
/// documented exit 3 (retryable) and a `retry_disposition` in context, not
/// the generic exit 1 + bare `{"path": ...}` context it returned before the
/// fix. Distinct from `ct4_restore_preflight_no_clobber_and_force_confirmation`
/// above, which exercises the same rejection only as a setup step before its
/// own `--force`/`--yes` assertions.
#[test]
fn r23_26_restore_conflict_no_force_is_exit_3_with_retry_disposition() {
    let dir = canonical_tempdir();
    let out = TempDir::new().unwrap();
    init(&dir);
    fs::write(dir.path().join("notes.md"), b"fresh content").unwrap();
    let commit = snapshot(&dir, "source");
    let destination = out.path().join("r23-26-out");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("notes.md"), b"pre-existing content").unwrap();

    let error = json_failure(
        &dir,
        &["export", &commit, "--to", &path_text(&destination)],
        3,
    );
    assert_eq!(error["error_code"], "KIO-E-COMMIT-EXPORT-CONFLICT-001");
    assert_eq!(error["context"]["retry_disposition"], "manual_action");
    // context.path names the conflicting destination leaf (exact string may be
    // realpath-canonicalized on platforms with a symlinked temp dir, e.g.
    // macOS's /var -> /private/var, so this checks the suffix rather than an
    // exact match). Separators are normalized first because the value is a
    // native path: on Windows it arrives as `r23-26-out\notes.md` and the
    // suffix check fails on the separator alone, which says nothing about the
    // behaviour under test. Same `.replace('\\', "/")` idiom as
    // step4b_ledger_contract.rs.
    assert!(
        error["context"]["path"]
            .as_str()
            .unwrap()
            .replace('\\', "/")
            .ends_with("r23-26-out/notes.md")
    );
    // Preflight rejection must not have touched the pre-existing file.
    assert_eq!(
        fs::read(destination.join("notes.md")).unwrap(),
        b"pre-existing content"
    );
}

#[test]
fn ct4_restore_source_preflight_is_atomic_and_raw_shorthand_is_invalid() {
    let dir = canonical_tempdir();
    let out = TempDir::new().unwrap();
    init(&dir);
    fs::write(dir.path().join("a.md"), b"alpha").unwrap();
    fs::write(dir.path().join("b.md"), b"beta").unwrap();
    let commit = snapshot(&dir, "source");
    let tree_hash = inspect(&dir, &commit)["tree"].as_str().unwrap().to_owned();
    let tree = inspect(&dir, &tree_hash);
    let raw_hash = tree["entries"][0]["raw_hash"].as_str().unwrap().to_owned();

    let raw_error = json_failure(
        &dir,
        &[
            "export",
            &raw_hash,
            "--to",
            &path_text(&dir.path().join("raw")),
        ],
        2,
    );
    assert_eq!(raw_error["error_code"], "KIO-E-CONFIG-USAGE-001");

    // PA47-50 (§O): the raw object is gone with NO purge marker (tombstone
    // or erase receipt) at all — LC14(a)/PA48(c)'s unmarked-absence
    // corruption suspicion, not the old generic not-found every raw absence
    // used to collapse into.
    let store = ObjectStore::new(dir.path().join(".kio"));
    fs::remove_file(store.object_path(ObjectKind::Raw, &raw_hash).unwrap()).unwrap();
    let destination = out.path().join("missing-raw");
    let error = json_failure(
        &dir,
        &["export", &commit, "--to", &path_text(&destination)],
        4,
    );
    assert_eq!(error["error_code"], "KIO-E-STORE-CORRUPT-001");
    assert!(!destination.exists());
}

#[test]
fn ct4_restore_source_authorization_is_serialized_by_purge_publication_lock() {
    let dir = canonical_tempdir();
    let out = TempDir::new().unwrap();
    init(&dir);
    fs::write(dir.path().join("doc.md"), b"authorized bytes").unwrap();
    let commit = snapshot(&dir, "source");

    // Model a concurrent live purge publication window. Export intentionally
    // does not take `.kio/.lock`, but must fail before staging/publishing while
    // this narrower coordination lock is owned by another live process.
    let repo = Repository::open(dir.path()).unwrap();
    let _publication = repo.lock_purge_publication().unwrap();
    let destination = out.path().join("must-not-publish");
    let error = json_failure(
        &dir,
        &["export", &commit, "--to", &path_text(&destination)],
        3,
    );
    assert_eq!(error["error_code"], "KIO-E-STORE-LOCKED-001");
    assert!(destination.is_dir());
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
}

#[test]
fn ct4_restore_rejects_shallow_tombstoned_and_store_destinations() {
    let tombstoned = canonical_tempdir();
    let tombstoned_out = TempDir::new().unwrap();
    init(&tombstoned);
    fs::write(tombstoned.path().join("doc.md"), b"secret").unwrap();
    let commit = snapshot(&tombstoned, "source");
    let tree_hash = inspect(&tombstoned, &commit)["tree"]
        .as_str()
        .unwrap()
        .to_owned();
    let tree = inspect(&tombstoned, &tree_hash);
    let raw_hash = tree["entries"][0]["raw_hash"].as_str().unwrap().to_owned();
    let tombstone = fanout_path(tombstoned.path().join(".kio/tombstones"), &raw_hash).unwrap();
    fs::create_dir_all(tombstone.parent().unwrap()).unwrap();
    fs::write(
        tombstone,
        serde_json::to_vec(&json!({
            "raw_hash": raw_hash,
            "events": [{
                "kind": "purged",
                "at": "2026-07-13T00:00:00Z",
                "in_commit": commit,
                "actor": "operator",
                "reason": "legal",
                "epoch": 1,
                "lifecycle_epoch": 1,
            }],
        }))
        .unwrap(),
    )
    .unwrap();
    let error = json_failure(
        &tombstoned,
        &[
            "export",
            &commit,
            "--to",
            &path_text(&tombstoned_out.path().join("dead")),
        ],
        4,
    );
    assert_eq!(error["error_code"], "KIO-E-PURGE-TOMBSTONED-001");

    // PA16/17 (§D, U25): the scope-root/`.kio`-descendant destination
    // rejection now uses `KIO-E-CONFIG-USAGE-001` (exit 2), not the generic
    // `KIO-E-COMMIT-EXPORT-UNSAFE-001` (exit 1) every other structural
    // rejection in this test file still uses.
    for forbidden in [
        tombstoned.path().to_path_buf(),
        tombstoned.path().join(".kio/export"),
    ] {
        let error = json_failure(
            &tombstoned,
            &["export", &commit, "--to", &path_text(&forbidden)],
            2,
        );
        assert_eq!(error["error_code"], "KIO-E-CONFIG-USAGE-001");
    }

    let shallow = canonical_tempdir();
    let shallow_out = TempDir::new().unwrap();
    init(&shallow);
    fs::write(shallow.path().join("doc.md"), b"content").unwrap();
    // A final shallow receipt permits a missing tree only for an Auto/Repaired
    // commit that is no longer a ref tip.  Build that exact completed-GC state
    // rather than treating a deleted tree on a manual HEAD snapshot as shallow.
    let repo = Repository::open(shallow.path()).unwrap();
    let commit = repo
        .auto_snapshot_with_normalize(
            Some("source"),
            Some("2026-08-14T00:00:00Z"),
            &BTreeSet::new(),
            &BTreeMap::new(),
        )
        .unwrap()
        .commit_hash
        .unwrap();
    let tree_hash = inspect(&shallow, &commit)["tree"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::write(shallow.path().join("doc.md"), b"advanced").unwrap();
    repo.auto_snapshot_with_normalize(
        Some("advance shallow fixture head"),
        Some("2026-08-14T00:00:01Z"),
        &BTreeSet::new(),
        &BTreeMap::new(),
    )
    .unwrap();
    let store = ObjectStore::new(shallow.path().join(".kio"));
    let receipt_path = shallow
        .path()
        .join(".kio/gc/shallowed")
        .join(commit.strip_prefix("sha256:").unwrap());
    fs::create_dir_all(receipt_path.parent().unwrap()).unwrap();
    let receipt = ShallowReceipt::new(
        commit.clone(),
        tree_hash.clone(),
        "2026-08-14T00:00:02Z".into(),
    )
    .unwrap();
    fs::write(receipt_path, receipt.canonical_bytes().unwrap()).unwrap();
    fs::remove_file(store.object_path(ObjectKind::Tree, &tree_hash).unwrap()).unwrap();
    let error = json_failure(
        &shallow,
        &[
            "export",
            &commit,
            "--to",
            &path_text(&shallow_out.path().join("shallow")),
        ],
        1,
    );
    assert_eq!(error["error_code"], "KIO-E-COMMIT-SHALLOW-001");
}

#[test]
fn ct4_restore_tag_wins_over_same_named_historical_path() {
    let dir = canonical_tempdir();
    let out = TempDir::new().unwrap();
    init(&dir);
    fs::write(dir.path().join("tagged.md"), b"tag target").unwrap();
    let tagged_commit = snapshot(&dir, "tag target");
    json_success(&dir, &["tag", "same", &tagged_commit]);
    fs::write(dir.path().join("same"), b"path target").unwrap();
    snapshot(&dir, "path exists");

    let destination = out.path().join("tag-precedence");
    let output = json_success(&dir, &["export", "same", "--to", &path_text(&destination)]);
    assert_eq!(output["source_kind"], "commit");
    assert_eq!(output["source_commit"], tagged_commit);
    assert_eq!(
        fs::read(destination.join("tagged.md")).unwrap(),
        b"tag target"
    );
    assert!(!destination.join("same").exists());
}

#[test]
fn ct4_restore_evidence_uses_exact_attested_commit_path_and_raw() {
    let dir = canonical_tempdir();
    let out = TempDir::new().unwrap();
    init(&dir);
    fs::write(dir.path().join("evidence.md"), b"attested bytes").unwrap();
    json_success(&dir, &["index", "--offline", "--yes"]);

    let commit = fs::read_to_string(dir.path().join(".kio/HEAD"))
        .unwrap()
        .trim()
        .to_owned();
    let tree_hash = inspect(&dir, &commit)["tree"].as_str().unwrap().to_owned();
    let tree = inspect(&dir, &tree_hash);
    let entry = tree["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"] == "evidence.md")
        .unwrap();
    let raw_hash = entry["raw_hash"].as_str().unwrap();
    let profile_hash = entry["normalize"]["tool_profile_hash"].as_str().unwrap();
    let chunk_hash = fs::read_to_string(dir.path().join(".kio/index/chunks.jsonl"))
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|chunk| chunk["raw_hash"] == raw_hash)
        .and_then(|chunk| chunk["chunk_id"].as_str().map(str::to_owned))
        .unwrap();
    let scope: Value =
        serde_json::from_slice(&fs::read(dir.path().join(".kio/scope.json")).unwrap()).unwrap();
    let scope_id = scope["scope_id"].as_str().unwrap().to_owned();
    let uri = format!("kio://{scope_id}/{commit}/{raw_hash}/{profile_hash}/{chunk_hash}");
    let pointer = json!({
        "schema_version": 1,
        "commit": commit,
        "tree": tree_hash,
        "raw_hash": raw_hash,
        "tool_profile_hash": profile_hash,
        "chunk_hash": chunk_hash,
        "path_at_commit": "evidence.md",
        "scope_id": scope_id,
        "scope_path": dir.path().join(".kio"),
    })
    .to_string();
    fs::remove_file(dir.path().join("evidence.md")).unwrap();
    let destination = out.path().join("evidence-export");
    let output = json_success(
        &dir,
        &["export", &pointer, "--to", &path_text(&destination)],
    );
    assert_eq!(output["source_kind"], "evidence");
    assert_eq!(output["exported_count"], 1);
    assert_eq!(
        fs::read(destination.join("evidence.md")).unwrap(),
        b"attested bytes"
    );

    let stdin_destination = out.path().join("evidence-stdin");
    let stdin_output = kio(
        &dir,
        &["export", "-", "--to", &path_text(&stdin_destination)],
    )
    .arg("--json")
    .write_stdin(pointer.clone())
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let stdin_output: Value = serde_json::from_slice(&stdin_output).unwrap();
    assert_eq!(stdin_output["source_kind"], "evidence");
    assert_eq!(
        fs::read(stdin_destination.join("evidence.md")).unwrap(),
        b"attested bytes"
    );

    let uri_destination = out.path().join("evidence-uri");
    let uri_output = json_success(
        &dir,
        &["export", &uri, "--to", &path_text(&uri_destination)],
    );
    assert_eq!(uri_output["source_kind"], "evidence");
    assert_eq!(
        fs::read(uri_destination.join("evidence.md")).unwrap(),
        b"attested bytes"
    );
}

#[test]
fn ct4_restore_cli_requires_to_and_rejects_extras_and_yes_without_force() {
    let dir = canonical_tempdir();
    init(&dir);
    fs::write(dir.path().join("doc.md"), b"content").unwrap();
    let commit = snapshot(&dir, "source");
    let yes_only = dir.path().join("yes-only");

    for args in [
        vec!["export", commit.as_str()],
        vec![
            "export",
            commit.as_str(),
            "--to",
            dir.path().to_str().unwrap(),
            "extra",
        ],
        vec![
            "export",
            commit.as_str(),
            "--to",
            yes_only.to_str().unwrap(),
            "--yes",
        ],
    ] {
        let error = json_failure(&dir, &args, 2);
        assert_eq!(error["error_code"], "KIO-E-CONFIG-USAGE-001");
    }
}

#[cfg(unix)]
#[test]
fn ct4_restore_refuses_symlink_and_hardlink_destination_leaves() {
    use std::os::unix::fs::symlink;

    let dir = canonical_tempdir();
    let out = TempDir::new().unwrap();
    init(&dir);
    fs::write(dir.path().join("doc.md"), b"restored").unwrap();
    let commit = snapshot(&dir, "source");
    let outside = dir.path().join("outside.txt");
    fs::write(&outside, b"outside").unwrap();

    let symlink_destination = out.path().join("symlink-dest");
    fs::create_dir(&symlink_destination).unwrap();
    symlink(&outside, symlink_destination.join("doc.md")).unwrap();
    let error = json_failure(
        &dir,
        &[
            "export",
            &commit,
            "--to",
            &path_text(&symlink_destination),
            "--force",
            "--yes",
        ],
        1,
    );
    assert_eq!(error["error_code"], "KIO-E-COMMIT-EXPORT-UNSAFE-001");
    assert_eq!(fs::read(&outside).unwrap(), b"outside");

    let hardlink_destination = out.path().join("hardlink-dest");
    fs::create_dir(&hardlink_destination).unwrap();
    fs::hard_link(&outside, hardlink_destination.join("doc.md")).unwrap();
    let error = json_failure(
        &dir,
        &[
            "export",
            &commit,
            "--to",
            &path_text(&hardlink_destination),
            "--force",
            "--yes",
        ],
        1,
    );
    assert_eq!(error["error_code"], "KIO-E-COMMIT-EXPORT-UNSAFE-001");
    assert_eq!(fs::read(&outside).unwrap(), b"outside");
}
