mod support;

use support::canonical_tempdir;

use std::fs;

use assert_cmd::Command;
use kio_core::cas::{ContentObjectKind, ObjectKind, ObjectStore, canonical_json_bytes};
use kio_core::dag::{CommitObject, CommitStats, CommitType, build_tree};
use kio_core::gc::ShallowReceipt;
use kio_core::purge::{PurgeReason, PurgeState, TombstoneMode};
use kio_index::aggregator::{AggIndexStatus, AggSelector, Aggregator};
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

fn fixture() -> (TempDir, Value, String) {
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
    let uri = search["results"][0]["evidence_uri"]
        .as_str()
        .unwrap()
        .to_owned();
    (dir, pointer, uri)
}

fn write_purged_commit(dir: &TempDir, timestamp: &str, purged_raws: &[String]) -> String {
    let repo = kio_core::scope::Repository::open(dir.path()).unwrap();
    let parent_hash = repo.head_commit_hash().unwrap().unwrap();
    let parent = repo.read_commit(&parent_hash).unwrap();
    let purged = CommitObject::new_purged(
        parent.tree,
        Some(parent_hash),
        timestamp.to_owned(),
        "legal".to_owned(),
        parent.tool_lock_hash,
        CommitStats {
            files_added: 0,
            files_modified: 0,
            files_deleted: 0,
        },
        purged_raws.to_vec(),
    )
    .unwrap();
    ObjectStore::new(dir.path().join(".kio"))
        .write_json(ObjectKind::Commit, &serde_json::to_value(&purged).unwrap())
        .unwrap()
        .0
}

fn tag_commit(dir: &TempDir, name: &str, commit_hash: &str) {
    let tag = dir
        .path()
        .join(".kio/refs/tags-v1")
        .join(kio_core::portable::portable_tag_leaf(name));
    fs::write(tag, commit_hash).unwrap();
}

fn write_receipt(
    dir: &TempDir,
    raw_hash: &str,
    purged_hash: &str,
    timestamp: &str,
) -> std::path::PathBuf {
    let digest = raw_hash.trim_start_matches("sha256:");
    let receipt = dir
        .path()
        .join(".kio/purge/erase-receipts")
        .join(&digest[0..2])
        .join(&digest[2..4])
        .join(digest);
    fs::create_dir_all(receipt.parent().unwrap()).unwrap();
    fs::write(
        &receipt,
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 2,
            "raw_hash": raw_hash,
            "events": [{
                "kind": "erased",
                "at": timestamp,
                "in_commit": purged_hash,
                "actor": "operator",
                "reason": "legal",
                "epoch": 1,
                "lifecycle_epoch": 1,
            }],
        }))
        .unwrap(),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&receipt, fs::Permissions::from_mode(0o600)).unwrap();
    }
    receipt
}

fn write_tombstone(
    dir: &TempDir,
    raw_hash: &str,
    purged_hash: &str,
    timestamp: &str,
) -> std::path::PathBuf {
    let digest = raw_hash.trim_start_matches("sha256:");
    let path = dir
        .path()
        .join(".kio/tombstones")
        .join(&digest[0..2])
        .join(&digest[2..4])
        .join(digest);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "raw_hash": raw_hash,
            "events": [{
                "kind": "purged",
                "at": timestamp,
                "in_commit": purged_hash,
                "actor": "operator",
                "reason": "legal",
                "epoch": 1,
                "lifecycle_epoch": 1,
            }],
        }))
        .unwrap(),
    )
    .unwrap();
    path
}

#[test]
fn ct4_verify_alive_is_content_free_and_accepts_strict() {
    let (dir, pointer, _) = fixture();
    let pointer = serde_json::to_string(&pointer).unwrap();
    for args in [
        vec!["evidence", "verify", pointer.as_str()],
        vec!["evidence", "verify", pointer.as_str(), "--strict"],
    ] {
        let output = success(&dir, &args);
        assert_eq!(output["status"], "alive");
        assert!(output.get("text").is_none());
        assert!(output["details"].get("path_at_commit").is_none());
    }
}

#[test]
fn ct4_verify_rejects_non_pointer_and_ambiguous_cli_forms() {
    let (dir, mut pointer, _) = fixture();
    let valid = serde_json::to_string(&pointer).unwrap();
    for args in [
        vec!["evidence", "verify", valid.as_str(), "extra"],
        vec![
            "evidence",
            "verify",
            pointer["chunk_hash"].as_str().unwrap(),
        ],
    ] {
        kio(&dir, &args).arg("--json").assert().code(2);
    }
    pointer["schema_version"] = Value::from(999);
    let future = serde_json::to_string(&pointer).unwrap();
    kio(&dir, &["evidence", "verify", &future])
        .arg("--json")
        .assert()
        .code(2);
}

// step4b-contract-tests-p2b.md PB65(c) / LC14(a): a raw object missing with
// NO tombstone/erase-receipt marker at all to explain the absence is
// unmarked corruption (`KIO-E-STORE-CORRUPT-001`, a raw command error, exit
// 4) — distinct from the expected-absence `not_found` status this used to
// report unconditionally. `not_found` is now reserved for a canonical
// `erased`/`purged` explanation (PB65(a), see
// crates/kio-cli/tests/step4b_p2b_contract.rs's pb65_* tests); an *unmarked*
// absence is corruption regardless of `--strict`, matching fsck's own
// long-standing `missing_raw` finding for the identical unmarked-absence
// case.
#[test]
fn ct4_verify_missing_raw_with_no_marker_is_store_corrupt_regardless_of_strict() {
    let (dir, pointer, _) = fixture();
    let raw_hash = pointer["raw_hash"].as_str().unwrap();
    let store = ObjectStore::new(dir.path().join(".kio"));
    fs::remove_file(store.object_path(ObjectKind::Raw, raw_hash).unwrap()).unwrap();
    let pointer = serde_json::to_string(&pointer).unwrap();

    for args in [
        vec!["evidence", "verify", pointer.as_str()],
        vec!["evidence", "verify", pointer.as_str(), "--strict"],
    ] {
        let output = kio(&dir, &args)
            .arg("--json")
            .assert()
            .code(4)
            .get_output()
            .stderr
            .clone();
        let output: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(output["error_code"], "KIO-E-STORE-CORRUPT-001", "{output}");
    }
}

#[test]
fn ct4_fsck_healthy_graph_is_unchanged() {
    let (dir, _, _) = fixture();
    let head_before = fs::read(dir.path().join(".kio/HEAD")).unwrap();
    let output = success(&dir, &["repair", "verify-objects"]);
    assert_eq!(output["status"], "ok", "{output}");
    assert!(output["checked"]["raw"].as_u64().unwrap() > 0);
    assert!(output["checked"]["chunks"].as_u64().unwrap() > 0);
    assert_eq!(fs::read(dir.path().join(".kio/HEAD")).unwrap(), head_before);
}

#[test]
fn ct4_fsck_recovers_identical_working_raw_and_records_repaired_commit() {
    let (dir, pointer, _) = fixture();
    let raw_hash = pointer["raw_hash"].as_str().unwrap();
    let store = ObjectStore::new(dir.path().join(".kio"));
    let head_before = fs::read_to_string(dir.path().join(".kio/HEAD")).unwrap();
    fs::remove_file(store.object_path(ObjectKind::Raw, raw_hash).unwrap()).unwrap();
    let output = success(&dir, &["repair", "verify-objects"]);
    assert_eq!(output["status"], "ok", "{output}");
    assert_eq!(output["repaired_raw_count"], 1);
    assert!(output["repaired_commit_hash"].as_str().is_some());
    assert!(store.inspect_object(ObjectKind::Raw, raw_hash).is_ok());

    let head_after = fs::read_to_string(dir.path().join(".kio/HEAD")).unwrap();
    assert_ne!(head_after, head_before, "repair recovery must move HEAD");
    let scope: Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join(".kio/scope.json")).unwrap())
            .unwrap();
    let scope_id = scope["scope_id"].as_str().unwrap();
    let replica = Aggregator::open(&dir.path().join(".test-cache/kio/aggregator.sqlite")).unwrap();
    let header = replica.scope_header(scope_id).unwrap().unwrap();
    assert_eq!(header.index_status, AggIndexStatus::Ready);
    assert_eq!(
        header.current_snapshot_commit.as_deref(),
        Some(head_after.trim()),
        "verify-objects recovery must publish the repaired HEAD to the replica"
    );
    assert!(
        replica
            .has_binding(scope_id, AggSelector::Current, head_after.trim())
            .unwrap(),
        "the repaired commit has the prior tree but still needs current-snapshot bindings"
    );
}

#[test]
fn ct4_fsck_replaces_corrupt_present_raw_from_identical_working_bytes() {
    let (dir, pointer, _) = fixture();
    let raw_hash = pointer["raw_hash"].as_str().unwrap();
    let store = ObjectStore::new(dir.path().join(".kio"));
    let raw_path = store.object_path(ObjectKind::Raw, raw_hash).unwrap();
    fs::write(&raw_path, b"corrupt-present-raw").unwrap();

    let output = success(&dir, &["repair", "verify-objects"]);
    assert_eq!(output["status"], "ok", "{output}");
    assert_eq!(output["repaired_raw_count"], 1);
    assert!(output["repaired_commit_hash"].as_str().is_some());
    assert!(store.inspect_object(ObjectKind::Raw, raw_hash).is_ok());
}

#[test]
fn ct4_verify_uri_and_bounded_stdin_are_read_only() {
    let (dir, pointer, uri) = fixture();
    assert_eq!(
        success(&dir, &["evidence", "verify", &uri])["status"],
        "alive"
    );
    let inline = serde_json::to_string(&pointer).unwrap();
    let stdin = kio(&dir, &["evidence", "verify", "-"])
        .arg("--json")
        .write_stdin(inline)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(
        serde_json::from_slice::<Value>(&stdin).unwrap()["status"],
        "alive"
    );
    assert!(!dir.path().join(".test-cache/kio/open").exists());

    let oversized = "x".repeat(64 * 1024 + 1);
    let error = kio(&dir, &["evidence", "verify", "-"])
        .arg("--json")
        .write_stdin(oversized)
        .assert()
        .code(2)
        .get_output()
        .stderr
        .clone();
    assert_eq!(
        serde_json::from_slice::<Value>(&error).unwrap()["error_code"],
        "KIO-E-CONFIG-USAGE-001"
    );
}

#[test]
fn ct4_verify_genuine_shallow_commit_uses_durable_chunk_cas() {
    let (dir, pointer, _) = fixture();
    let repo = kio_core::scope::Repository::open(dir.path()).unwrap();
    let commit_hash = pointer["commit"].as_str().unwrap();
    let commit = repo.read_commit(commit_hash).unwrap();
    // The original indexed auto snapshot must no longer be a ref tip before
    // its durable receipt can represent a completed markerless shallow state.
    fs::write(
        dir.path().join("advance.md"),
        "# Advance\n\nadvance the fixture head\n",
    )
    .unwrap();
    success(&dir, &["index", "--offline", "--yes"]);
    let receipt_path = dir
        .path()
        .join(".kio/gc/shallowed")
        .join(commit_hash.strip_prefix("sha256:").unwrap());
    fs::create_dir_all(receipt_path.parent().unwrap()).unwrap();
    let receipt = ShallowReceipt::new(
        commit_hash.to_owned(),
        commit.tree.clone(),
        "2026-08-14T00:00:00Z".into(),
    )
    .unwrap();
    fs::write(receipt_path, receipt.canonical_bytes().unwrap()).unwrap();
    let store = ObjectStore::new(dir.path().join(".kio"));
    fs::remove_file(store.object_path(ObjectKind::Tree, &commit.tree).unwrap()).unwrap();
    let pointer = serde_json::to_string(&pointer).unwrap();
    let output = success(&dir, &["evidence", "verify", &pointer]);
    assert_eq!(output["status"], "alive");
    assert_eq!(output["details"]["commit_shallow"], true);
}

#[test]
fn ct4_verify_chunk_cas_is_truth_over_jsonl_and_sqlite() {
    let (dir, pointer, _) = fixture();
    let chunk_hash = pointer["chunk_hash"].as_str().unwrap();
    let store = ObjectStore::new(dir.path().join(".kio"));
    fs::remove_file(store.chunk_path(chunk_hash).unwrap()).unwrap();
    let pointer = serde_json::to_string(&pointer).unwrap();
    let error = kio(&dir, &["evidence", "verify", &pointer])
        .arg("--json")
        .assert()
        .code(8)
        .get_output()
        .stderr
        .clone();
    assert_eq!(
        serde_json::from_slice::<Value>(&error).unwrap()["error_code"],
        "KIO-E-EVIDENCE-RETARGET-REQUIRED-001"
    );
}

#[test]
fn ct4_fsck_unrecoverable_raw_is_bounded_partial_and_logged() {
    let (dir, pointer, _) = fixture();
    let raw_hash = pointer["raw_hash"].as_str().unwrap();
    let store = ObjectStore::new(dir.path().join(".kio"));
    fs::remove_file(store.object_path(ObjectKind::Raw, raw_hash).unwrap()).unwrap();
    fs::remove_file(dir.path().join("evidence.md")).unwrap();
    let stdout = kio(&dir, &["repair", "verify-objects"])
        .arg("--json")
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let output: Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(output["status"], "corrupt");
    assert_eq!(output["external_pointers_may_be_affected"], true);
    assert!(
        output["remaining_findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["kind"] == "missing_raw")
    );
    assert!(dir.path().join(".test-data/kio/logs/errors.jsonl").exists());
}

#[test]
fn ct4_verify_and_fsck_accept_valid_tombstone_terminal() {
    let (dir, pointer, _) = fixture();
    let raw_hash = pointer["raw_hash"].as_str().unwrap();
    let digest = raw_hash.trim_start_matches("sha256:");
    let tombstone = dir
        .path()
        .join(".kio/tombstones")
        .join(&digest[0..2])
        .join(&digest[2..4])
        .join(digest);
    fs::create_dir_all(tombstone.parent().unwrap()).unwrap();
    let repo = kio_core::scope::Repository::open(dir.path()).unwrap();
    let parent_hash = repo.head_commit_hash().unwrap().unwrap();
    let parent = repo.read_commit(&parent_hash).unwrap();
    let purged_at = "2026-07-13T00:00:00Z";
    let purged = CommitObject::new_purged(
        parent.tree,
        Some(parent_hash),
        purged_at.to_owned(),
        "legal".to_owned(),
        parent.tool_lock_hash,
        CommitStats {
            files_added: 0,
            files_modified: 0,
            files_deleted: 0,
        },
        vec![raw_hash.to_owned()],
    )
    .unwrap();
    let store = ObjectStore::new(dir.path().join(".kio"));
    let (purged_hash, _) = store
        .write_json(ObjectKind::Commit, &serde_json::to_value(&purged).unwrap())
        .unwrap();
    fs::write(dir.path().join(".kio/HEAD"), format!("{purged_hash}\n")).unwrap();
    fs::write(
        &tombstone,
        serde_json::to_vec(&serde_json::json!({
            "raw_hash": raw_hash,
            "events": [{
                "kind": "purged",
                "at": purged_at,
                "in_commit": purged_hash,
                "actor": "operator",
                "reason": "legal",
                "epoch": 1,
                "lifecycle_epoch": 1,
            }],
        }))
        .unwrap(),
    )
    .unwrap();
    fs::remove_file(store.object_path(ObjectKind::Raw, raw_hash).unwrap()).unwrap();
    fs::remove_dir_all(dir.path().join(".kio/objects/normalized_units")).unwrap();
    fs::remove_dir_all(dir.path().join(".kio/objects/chunks")).unwrap();
    let pointer = serde_json::to_string(&pointer).unwrap();
    assert_eq!(
        success(&dir, &["evidence", "verify", &pointer])["status"],
        "tombstoned"
    );
    let fsck = success(&dir, &["repair", "verify-objects"]);
    assert_eq!(fsck["status"], "ok", "{fsck}");
    assert_eq!(fsck["dead_by_tombstone_count"], 1);
}

#[test]
fn ct4_fsck_checks_failed_prepared_and_metadata_only_image_references() {
    let (dir, pointer, _) = fixture();
    let repo = kio_core::scope::Repository::open(dir.path()).unwrap();
    let parent_hash = pointer["commit"].as_str().unwrap().to_owned();
    let parent = repo.read_commit(&parent_hash).unwrap();
    let mut tree = repo.read_tree(&parent.tree).unwrap();
    let entry = tree
        .entries
        .iter_mut()
        .find(|entry| entry.raw_hash == pointer["raw_hash"].as_str().unwrap())
        .unwrap();
    let normalize = entry.normalize.as_mut().unwrap();
    let store = ObjectStore::new(repo.kio_dir());
    let manifest_bytes = store
        .read_content_object_bytes(
            ContentObjectKind::Manifest,
            &normalize.manifest_hash,
            8 * 1024 * 1024,
        )
        .unwrap();
    let mut manifest: Value = serde_json::from_slice(&manifest_bytes).unwrap();
    let template = manifest["units"][0].clone();
    let failed_key = "failed:image";
    let mut failed = template;
    failed["unit_key"] = Value::from(failed_key);
    failed["unit_ref"] = Value::from(kio_pipeline::prepare::unit_ref(failed_key));
    failed["status"] = Value::from("failed");
    failed["unit_object_hash"] = Value::Null;
    failed["prepared_hash"] = Value::from(format!("sha256:{}", "e".repeat(64)));
    failed["error_kind"] = Value::from("contract_violation");
    manifest["units"].as_array_mut().unwrap().push(failed);

    let unit_hash = manifest["units"][0]["unit_object_hash"].as_str().unwrap();
    let unit_bytes = store
        .read_content_object_bytes(
            ContentObjectKind::NormalizedUnit,
            unit_hash,
            8 * 1024 * 1024,
        )
        .unwrap();
    let mut unit: Value = serde_json::from_slice(&unit_bytes).unwrap();
    let missing_image_hash = format!("sha256:{}", "f".repeat(64));
    unit["metadata"]["images"] = serde_json::json!([{
        "hash": missing_image_hash.clone(),
        "media_type": "image/png"
    }]);
    // Metadata alone is descriptive. Make this a typed local ownership edge
    // so fsck must verify the missing image CAS object.
    unit["owned_image_hashes"] = serde_json::json!([missing_image_hash]);
    let unit_bytes = canonical_json_bytes(&unit).unwrap();
    let replacement_unit_hash = store
        .write_content_object(ContentObjectKind::NormalizedUnit, &unit_bytes)
        .unwrap();
    manifest["units"][0]["unit_object_hash"] = Value::from(replacement_unit_hash);
    let manifest_bytes = canonical_json_bytes(&manifest).unwrap();
    normalize.manifest_hash = store
        .write_content_object(ContentObjectKind::Manifest, &manifest_bytes)
        .unwrap();

    let tree = build_tree(tree.entries).unwrap();
    let (tree_hash, _) = store
        .write_json(ObjectKind::Tree, &serde_json::to_value(&tree).unwrap())
        .unwrap();
    let commit = CommitObject::new(
        tree_hash,
        Some(parent_hash),
        "2026-08-12T00:00:00Z".to_owned(),
        "fixture: immutable failed and image references".to_owned(),
        parent.tool_lock_hash,
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
    fs::write(repo.kio_dir().join("HEAD"), format!("{commit_hash}\n")).unwrap();

    let stdout = kio(&dir, &["repair", "verify-objects"])
        .arg("--json")
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let output: Value = serde_json::from_slice(&stdout).unwrap();
    let kinds = output["remaining_findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|finding| finding["kind"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(kinds.contains("prepared_corrupt"), "{output}");
    assert!(kinds.contains("image_corrupt"), "{output}");
}

#[test]
fn ct4_fsck_accepts_erase_receipt_in_current_head_ancestry() {
    let (dir, pointer, _) = fixture();
    let raw_hash = pointer["raw_hash"].as_str().unwrap();
    let timestamp = "2026-07-13T00:00:00Z";
    let purged_hash = write_purged_commit(&dir, timestamp, &[raw_hash.to_owned()]);
    tag_commit(&dir, "purged-only", &purged_hash);
    fs::write(dir.path().join(".kio/HEAD"), format!("{purged_hash}\n")).unwrap();
    write_receipt(&dir, raw_hash, &purged_hash, timestamp);
    let store = ObjectStore::new(dir.path().join(".kio"));
    fs::remove_file(store.object_path(ObjectKind::Raw, raw_hash).unwrap()).unwrap();
    fs::remove_dir_all(dir.path().join(".kio/objects/normalized_units")).unwrap();
    fs::remove_dir_all(dir.path().join(".kio/objects/chunks")).unwrap();
    let output = success(&dir, &["repair", "verify-objects"]);
    assert_eq!(output["status"], "ok", "{output}");
    assert_eq!(output["dead_by_erase_receipt_count"], 1);
}

/// LC36 (§F): verified raw + a canonical `erased` marker whose `in_commit` has
/// no ref-reachable, ancestor-respecting republication commit is an
/// *incomplete purge* (exit 3) — the receipt is never silently removed nor
/// retired without that causal justification (reversal of the old
/// "verified raw always wins" rule).
#[test]
fn ct4_fsck_live_raw_with_stale_receipt_and_no_republication_commit_is_incomplete_purge() {
    let (dir, pointer, _) = fixture();
    let raw_hash = pointer["raw_hash"].as_str().unwrap();
    let timestamp = "2026-07-13T00:00:00Z";
    let purged_hash = write_purged_commit(&dir, timestamp, &[raw_hash.to_owned()]);
    tag_commit(&dir, "stale-receipt", &purged_hash);
    fs::write(dir.path().join(".kio/HEAD"), format!("{purged_hash}\n")).unwrap();
    let receipt = write_receipt(&dir, raw_hash, &purged_hash, timestamp);
    let receipt_before = fs::read(&receipt).unwrap();
    let head_before = fs::read(dir.path().join(".kio/HEAD")).unwrap();
    let stdout = kio(&dir, &["repair", "verify-objects"])
        .arg("--json")
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let output: Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(output["error_code"], "KIO-E-PURGE-INCOMPLETE-001");
    assert!(
        output["remaining_findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["kind"] == "purge_incomplete")
    );
    assert_eq!(output["repaired_commit_hash"], Value::Null);
    assert!(receipt.exists());
    assert_eq!(fs::read(&receipt).unwrap(), receipt_before);
    assert_eq!(fs::read(dir.path().join(".kio/HEAD")).unwrap(), head_before);
}

/// LC27/LC35 (§F): the positive case — a ref-reachable commit that descends
/// from the erased event's `in_commit` (a republication) lets fsck backfill
/// `retired` (append-only; the receipt file is not removed, LC33).
#[test]
fn ct4_fsck_live_raw_with_republication_commit_backfills_retired_receipt() {
    let (dir, pointer, _) = fixture();
    let raw_hash = pointer["raw_hash"].as_str().unwrap().to_owned();
    let timestamp = "2026-07-13T00:00:00Z";
    let purged_hash = write_purged_commit(&dir, timestamp, std::slice::from_ref(&raw_hash));
    write_receipt(&dir, &raw_hash, &purged_hash, timestamp);

    // A republication commit is the new HEAD and a normal child of the purge
    // commit. The tag names that same linear ancestry; it grants no authority.
    let repo = kio_core::scope::Repository::open(dir.path()).unwrap();
    let purged_commit_object = repo.read_commit(&purged_hash).unwrap();
    let republication = CommitObject::new(
        purged_commit_object.tree.clone(),
        Some(purged_hash.clone()),
        "2026-07-14T00:00:00Z".to_owned(),
        "republished".to_owned(),
        purged_commit_object.tool_lock_hash.clone(),
        CommitStats {
            files_added: 0,
            files_modified: 0,
            files_deleted: 0,
        },
        CommitType::Manual,
    )
    .unwrap();
    let store = ObjectStore::new(dir.path().join(".kio"));
    let (republication_hash, _) = store
        .write_json(
            ObjectKind::Commit,
            &serde_json::to_value(&republication).unwrap(),
        )
        .unwrap();
    tag_commit(&dir, "republication", &republication_hash);
    fs::write(
        dir.path().join(".kio/HEAD"),
        format!("{republication_hash}\n"),
    )
    .unwrap();

    let output = success(&dir, &["repair", "verify-objects"]);
    assert_eq!(output["status"], "ok", "{output}");
    assert!(
        !output["remaining_findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["kind"] == "purge_incomplete")
    );

    let state = PurgeState::open(dir.path().join(".kio")).unwrap();
    let receipt = state.read_erase_receipt(&raw_hash).unwrap().unwrap();
    assert!(!receipt.is_active());
    assert_eq!(
        receipt.tail().resurrection_commit.as_deref(),
        Some(republication_hash.as_str())
    );
}

#[test]
fn ct4_fsck_unindexed_scope_recovers_lifecycle_epoch_before_retired_backfill() {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("evidence.md"), "# Evidence\n\nstill live\n").unwrap();
    let repo = kio_core::scope::Repository::init(dir.path()).unwrap();
    let snapshot = repo
        .snapshot(Some("initial"), Some("2026-07-12T00:00:00Z"))
        .unwrap();
    let raw_hash = snapshot
        .commit
        .as_ref()
        .and_then(|commit| repo.read_tree(&commit.tree).ok())
        .and_then(|tree| tree.entries.first().map(|entry| entry.raw_hash.clone()))
        .unwrap();
    assert!(!dir.path().join(".kio/index/sqlite.db").exists());

    let timestamp = "2026-07-13T00:00:00Z";
    let purged_hash = write_purged_commit(&dir, timestamp, std::slice::from_ref(&raw_hash));
    write_receipt(&dir, &raw_hash, &purged_hash, timestamp);
    let purged_commit_object = repo.read_commit(&purged_hash).unwrap();
    let republication = CommitObject::new(
        purged_commit_object.tree.clone(),
        Some(purged_hash.clone()),
        "2026-07-14T00:00:00Z".to_owned(),
        "republished".to_owned(),
        purged_commit_object.tool_lock_hash.clone(),
        CommitStats {
            files_added: 0,
            files_modified: 0,
            files_deleted: 0,
        },
        CommitType::Manual,
    )
    .unwrap();
    let (republication_hash, _) = ObjectStore::new(dir.path().join(".kio"))
        .write_json(
            ObjectKind::Commit,
            &serde_json::to_value(&republication).unwrap(),
        )
        .unwrap();
    tag_commit(&dir, "republication", &republication_hash);
    fs::write(
        dir.path().join(".kio/HEAD"),
        format!("{republication_hash}\n"),
    )
    .unwrap();

    let output = success(&dir, &["repair", "verify-objects"]);
    assert_eq!(output["status"], "ok", "{output}");
    assert!(!dir.path().join(".kio/index/sqlite.db").exists());
    let receipt = PurgeState::open(dir.path().join(".kio"))
        .unwrap()
        .read_erase_receipt(&raw_hash)
        .unwrap()
        .unwrap();
    assert_eq!(receipt.tail().lifecycle_epoch, Some(3));
    assert_eq!(
        receipt.tail().resurrection_commit.as_deref(),
        Some(republication_hash.as_str())
    );
}

#[test]
fn ct4_fsck_active_journal_suppresses_raw_recovery_and_ref_mutation() {
    let (dir, pointer, _) = fixture();
    let raw_hash = pointer["raw_hash"].as_str().unwrap().to_owned();
    let purge = PurgeState::open(dir.path().join(".kio")).unwrap();
    purge
        .begin(
            vec![raw_hash.clone()],
            PurgeReason::Legal,
            TombstoneMode::Default,
            "user",
            "2026-07-13T00:00:00Z",
            1,
            kio_pipeline::prepare::hash_bytes(b"planned purge commit placeholder"),
            kio_pipeline::prepare::hash_bytes(b"planned purge closure placeholder"),
            kio_core::scope::new_ulid(dir.path()),
        )
        .unwrap();
    let journal_before = fs::read(purge.journal_path()).unwrap();
    let head_before = fs::read(dir.path().join(".kio/HEAD")).unwrap();
    let store = ObjectStore::new(dir.path().join(".kio"));
    let raw_path = store.object_path(ObjectKind::Raw, &raw_hash).unwrap();
    fs::remove_file(&raw_path).unwrap();
    let stdout = kio(&dir, &["repair", "verify-objects"])
        .arg("--json")
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let output: Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(output["status"], "corrupt");
    assert_eq!(output["error_code"], "KIO-E-PURGE-INCOMPLETE-001");
    assert_eq!(output["checked"]["raw"], 0);
    assert_eq!(output["checked"]["chunks"], 0);
    assert_eq!(output["checked"]["normalized_instances"], 0);
    assert!(!raw_path.exists());
    assert_eq!(fs::read(purge.journal_path()).unwrap(), journal_before);
    assert_eq!(fs::read(dir.path().join(".kio/HEAD")).unwrap(), head_before);
}

#[cfg(unix)]
#[test]
fn ct4_fsck_rejects_linked_object_namespace_without_traversing_it() {
    use std::os::unix::fs::symlink;

    let (dir, _, _) = fixture();
    let chunks = dir.path().join(".kio/objects/chunks");
    fs::remove_dir_all(&chunks).unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), b"must not be inventoried").unwrap();
    symlink(outside.path(), &chunks).unwrap();

    let stdout = kio(&dir, &["repair", "verify-objects"])
        .arg("--json")
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let output: Value = serde_json::from_slice(&stdout).unwrap();
    assert!(
        output["remaining_findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["kind"] == "object_namespace_unavailable")
    );
    assert_eq!(output["status"], "verification_blocked");
    assert_eq!(output["repaired_raw_count"], 0);
    assert_eq!(fs::read_link(&chunks).unwrap(), outside.path());
    assert_eq!(
        fs::read(outside.path().join("secret")).unwrap(),
        b"must not be inventoried"
    );
}

/// §F/LC38: a verified raw coexisting with a tombstone (or with both a
/// tombstone and an erase receipt) is no longer an unconditional
/// `tombstone_conflict`/`purge_marker_conflict` finding — canonical-final-event
/// judgment (§C) decides. With no ref-reachable republication commit for the
/// canonical `purged` event, both scenarios here are `purge_incomplete`
/// (LC36), and neither marker is silently mutated.
#[test]
fn ct4_fsck_live_raw_tombstone_and_dual_terminal_markers_are_incomplete_purge_not_conflict() {
    let (dir, pointer, _) = fixture();
    let raw_hash = pointer["raw_hash"].as_str().unwrap();
    let timestamp = "2026-07-13T00:00:00Z";
    let purged_hash = write_purged_commit(&dir, timestamp, &[raw_hash.to_owned()]);
    tag_commit(&dir, "marker-conflict", &purged_hash);
    fs::write(dir.path().join(".kio/HEAD"), format!("{purged_hash}\n")).unwrap();
    let tombstone = write_tombstone(&dir, raw_hash, &purged_hash, timestamp);
    let stdout = kio(&dir, &["repair", "verify-objects"])
        .arg("--json")
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let output: Value = serde_json::from_slice(&stdout).unwrap();
    assert!(
        !output["remaining_findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["kind"] == "tombstone_conflict")
    );
    assert!(
        output["remaining_findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["kind"] == "purge_incomplete")
    );
    assert!(tombstone.exists());

    // Raw removed: this is no longer a "live raw + marker" incomplete-purge
    // scenario — canonical (tombstone, tie-break winner over the receipt per
    // LC8) is a normal, fully-explained dead terminal (exit 0).
    let store = ObjectStore::new(dir.path().join(".kio"));
    fs::remove_file(store.object_path(ObjectKind::Raw, raw_hash).unwrap()).unwrap();
    write_receipt(&dir, raw_hash, &purged_hash, timestamp);
    assert!(tombstone.exists());
    let output = success(&dir, &["repair", "verify-objects"]);
    assert!(
        !output["remaining_findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["kind"] == "purge_marker_conflict")
    );
    // Raw is no longer verified here, so the canonical marker (tombstone,
    // tie-break winner over the receipt per LC8) is a normal dead terminal.
    assert_eq!(output["status"], "ok", "{output}");
    assert_eq!(output["dead_by_tombstone_count"], 1);
}
