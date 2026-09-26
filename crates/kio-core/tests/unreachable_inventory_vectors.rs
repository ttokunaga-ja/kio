#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};

use kio_core::cas::{
    ChunkObject, ContentObjectKind, EmbeddingObject, ObjectKind, ObjectStore, canonical_json_bytes,
    hash_bytes,
};
use kio_core::dag::{CommitObject, CommitStats, CommitType, NormalizeRef, TreeEntry, build_tree};
use kio_core::gc::{ShallowReceipt, UnreachableInventoryLimits, UnreachableObjectInventory};
use kio_core::purge::{ClosureItem, LifecycleEvent, PurgeClosure, PurgeReason, PurgeState};
use kio_core::scope::{Repository, new_ulid};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const AT_0: &str = "2026-08-20T00:00:00Z";
const AT_1: &str = "2026-08-20T00:00:01Z";
const AT_2: &str = "2026-08-20T00:00:02Z";
const GOOD_TOOL_LOCK: &[u8] = br#"{"spec_version":1}"#;
const GOOD_TOOL_LOCK_HASH: &str =
    "sha256:194259188878490069afbf419a62d78154f4f15bb6b87a9a0650e24d8c5e258e";
const BAD_NONCANONICAL_TOOL_LOCK: &[u8] = br#"{ "spec_version": 1 }"#;
const BAD_NONCANONICAL_HASH: &str =
    "sha256:75d24e72efec618faeb2df5aa7952f7ad5ea490335f78721664ebd644247a5f6";

struct Fixture {
    root: TempDir,
}

struct LiveGraph {
    raw: String,
    manifest: String,
    unit: String,
    tool_lock: String,
    tree: String,
    commit: String,
    chunk: String,
    embedding: String,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        Repository::init(root.path()).unwrap();
        Self { root }
    }

    fn root(&self) -> &Path {
        self.root.path()
    }

    fn kio(&self) -> PathBuf {
        self.root().join(".kio")
    }

    fn store(&self) -> ObjectStore {
        ObjectStore::new(self.kio())
    }

    fn report(&self) -> Value {
        UnreachableObjectInventory::bind(self.root().canonicalize().unwrap())
            .unwrap()
            .inventory()
            .unwrap()
    }

    fn error(&self) -> kio_core::KioError {
        UnreachableObjectInventory::bind(self.root().canonicalize().unwrap())
            .unwrap()
            .inventory()
            .unwrap_err()
    }

    fn set_tip(&self, hash: &str) {
        fs::write(self.kio().join("HEAD"), format!("{hash}\n")).unwrap();
    }

    fn tool_lock(&self, discriminator: u64) -> String {
        let bytes = canonical_json_bytes(&json!({
            "spec_version": 1,
            "prepare": {
                "profile_hash": hash_bytes(&discriminator.to_be_bytes()),
                "tool_id": format!("fixture-{discriminator}")
            }
        }))
        .unwrap();
        self.store()
            .write_content_object(ContentObjectKind::Toollock, &bytes)
            .unwrap()
    }

    fn normalized_closure(&self, raw: &str, discriminator: u64) -> (String, String, String) {
        let profile = hash_bytes(format!("profile-{discriminator}").as_bytes());
        let prepared = hash_bytes(format!("prepared-{discriminator}").as_bytes());
        let preparation_profile = hash_bytes(format!("prepare-profile-{discriminator}").as_bytes());
        let unit_value = json!({
            "unit_key": "page:1",
            "unit_type": "page",
            "raw_hash": raw,
            "prepared_hash": prepared,
            "preparation_profile_hash": preparation_profile,
            "tool_profile_hash": profile,
            "gen": 0,
            "mode": "full",
            "markdown": format!("fixture body {discriminator}"),
            "owned_image_hashes": [],
            "metadata": {},
            "reused_from": null,
            "generated_at": AT_0
        });
        let unit_bytes = canonical_json_bytes(&unit_value).unwrap();
        let unit = self
            .store()
            .write_content_object(ContentObjectKind::NormalizedUnit, &unit_bytes)
            .unwrap();
        let unit_ref = {
            let digest = Sha256::digest(b"page:1");
            digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()[..16]
                .to_owned()
        };
        let manifest_value = json!({
            "raw_hash": raw,
            "tool_profile_hash": profile,
            "gen": 0,
            "parent_gen": null,
            "run_id": format!("run-{discriminator}"),
            "units": [{
                "order": 0,
                "unit_key": "page:1",
                "unit_ref": unit_ref,
                "unit_type": "page",
                "status": "done",
                "prepared_hash": prepared,
                "preparation_profile_hash": preparation_profile,
                "unit_object_hash": unit,
                "error_kind": null
            }],
            "generated_at": AT_0
        });
        let manifest_bytes = canonical_json_bytes(&manifest_value).unwrap();
        let manifest = self
            .store()
            .write_content_object(ContentObjectKind::Manifest, &manifest_bytes)
            .unwrap();
        (profile, unit, manifest)
    }

    fn tree(&self, raw: &str, profile: &str, manifest: &str, name: &str) -> String {
        let mut entry = TreeEntry::raw_file(name, raw).unwrap();
        entry.normalize = Some(NormalizeRef {
            tool_profile_hash: profile.to_owned(),
            r#gen: 0,
            manifest_hash: manifest.to_owned(),
        });
        let tree = build_tree(vec![entry]).unwrap();
        self.store()
            .write_json(ObjectKind::Tree, &serde_json::to_value(tree).unwrap())
            .unwrap()
            .0
    }

    fn commit(&self, tree: &str, parent: Option<String>, tool_lock: &str, at: &str) -> String {
        let commit = CommitObject::new(
            tree.to_owned(),
            parent,
            at.to_owned(),
            "fixture".to_owned(),
            tool_lock.to_owned(),
            CommitStats {
                files_added: 0,
                files_modified: 0,
                files_deleted: 0,
            },
            CommitType::Manual,
        )
        .unwrap();
        self.store()
            .write_json(ObjectKind::Commit, &serde_json::to_value(commit).unwrap())
            .unwrap()
            .0
    }

    fn live_graph(&self, discriminator: u64, parent: Option<String>) -> LiveGraph {
        let raw = self
            .store()
            .write_raw(format!("raw-{discriminator}").as_bytes())
            .unwrap();
        let (profile, unit, manifest) = self.normalized_closure(&raw, discriminator);
        let tool_lock = self.tool_lock(discriminator);
        let tree = self.tree(
            &raw,
            &profile,
            &manifest,
            &format!("doc-{discriminator}.md"),
        );
        let commit = self.commit(
            &tree,
            parent,
            &tool_lock,
            if discriminator == 0 { AT_0 } else { AT_1 },
        );
        let text = format!("chunk-{discriminator}");
        let chunk_object = ChunkObject {
            spec_version: 1,
            raw_hash: raw.clone(),
            tool_profile_hash: profile,
            r#gen: 0,
            unit_key: "page:1".to_owned(),
            unit_content_hash: hash_bytes(format!("fixture body {discriminator}").as_bytes()),
            heading_path: vec![],
            section_id: None,
            byte_start: 0,
            byte_end: text.len() as u64,
            text_hash: hash_bytes(text.as_bytes()),
            text,
        };
        let target = chunk_object.text_hash.clone();
        let chunk = self.store().write_chunk(&chunk_object).unwrap();
        let embedding_object = EmbeddingObject {
            spec_version: 1,
            target_type: "chunk".to_owned(),
            target_hash: target,
            profile_hash: hash_bytes(b"embedding-profile"),
            modality: "multimodal".to_owned(),
            dimensions: 1,
            distance: "cosine".to_owned(),
            context: None,
            vector: vec![0.5],
        };
        let embedding = self.store().write_embedding(&embedding_object).unwrap();
        LiveGraph {
            raw,
            manifest,
            unit,
            tool_lock,
            tree,
            commit,
            chunk,
            embedding,
        }
    }

    fn install_frozen(&self, directory: &str, hash: &str, bytes: &[u8]) -> PathBuf {
        let digest = hash.strip_prefix("sha256:").unwrap();
        let parent = self
            .kio()
            .join("objects")
            .join(directory)
            .join(&digest[..2])
            .join(&digest[2..4]);
        fs::create_dir_all(&parent).unwrap();
        let path = parent.join(digest);
        fs::write(&path, bytes).unwrap();
        path
    }
}

fn object<'a>(report: &'a Value, kind: &str, hash: &str) -> &'a Value {
    report["objects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|object| object["kind"] == kind && object["hash"] == hash)
        .unwrap()
}

fn oracle_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sha256:{hex}")
}

#[test]
fn empty_inventory_has_exact_zero_byte_invariant() {
    let fixture = Fixture::new();
    let report = fixture.report();
    assert_eq!(report["objects"], json!([]));
    assert_eq!(report["summary"]["object_count"], 0);
    assert_eq!(report["summary"]["physical_bytes"], 0);
    assert_eq!(report["summary"]["candidate_count"], 0);
    assert_eq!(report["summary"]["candidate_bytes"], 0);
}

#[test]
fn frozen_good_and_bad_vectors_are_independent_of_the_store_writer() {
    assert_eq!(oracle_hash(GOOD_TOOL_LOCK), GOOD_TOOL_LOCK_HASH);
    assert_eq!(
        oracle_hash(BAD_NONCANONICAL_TOOL_LOCK),
        BAD_NONCANONICAL_HASH
    );

    let good = Fixture::new();
    good.install_frozen("toollocks", GOOD_TOOL_LOCK_HASH, GOOD_TOOL_LOCK);
    let report = good.report();
    assert_eq!(
        object(&report, "toollock", GOOD_TOOL_LOCK_HASH)["classification"],
        "candidate"
    );
    assert_eq!(
        object(&report, "toollock", GOOD_TOOL_LOCK_HASH)["physical_bytes"],
        GOOD_TOOL_LOCK.len() as u64
    );

    let bad = Fixture::new();
    bad.install_frozen(
        "toollocks",
        BAD_NONCANONICAL_HASH,
        BAD_NONCANONICAL_TOOL_LOCK,
    );
    assert_eq!(bad.error().error_code(), "KIO-E-STORE-CORRUPT-001");
}

#[test]
fn unreferenced_semantic_objects_are_candidates_only_when_proven() {
    let fixture = Fixture::new();
    let raw = hash_bytes(b"absent-raw-identity");
    let (_, _manifest_unit, manifest) = fixture.normalized_closure(&raw, 7);
    let orphan_raw = hash_bytes(b"second-absent-raw-identity");
    let (_, unit, removed_manifest) = fixture.normalized_closure(&orphan_raw, 8);
    fs::remove_file(
        fixture
            .store()
            .content_path(ContentObjectKind::Manifest, &removed_manifest)
            .unwrap(),
    )
    .unwrap();
    let tool_lock = fixture.tool_lock(7);
    let embedding_object = EmbeddingObject {
        spec_version: 1,
        target_type: "chunk".to_owned(),
        target_hash: hash_bytes(b"absent-chunk-text"),
        profile_hash: hash_bytes(b"profile"),
        modality: "multimodal".to_owned(),
        dimensions: 1,
        distance: "cosine".to_owned(),
        context: None,
        vector: vec![0.25],
    };
    let embedding = fixture.store().write_embedding(&embedding_object).unwrap();
    let report = fixture.report();
    for (kind, hash, reason) in [
        ("manifest", manifest, "zero_tree_references"),
        ("normalized_unit", unit, "zero_manifest_references"),
        ("toollock", tool_lock, "zero_commit_references"),
        ("embedding", embedding, "zero_target_references"),
    ] {
        let row = object(&report, kind, &hash);
        assert_eq!(row["classification"], "candidate");
        assert_eq!(row["reason"], reason);
    }
}

#[test]
fn live_graph_and_permanent_kinds_are_never_candidates() {
    let fixture = Fixture::new();
    let graph = fixture.live_graph(0, None);
    fixture.set_tip(&graph.commit);
    let prepared = fixture
        .store()
        .write_content_object(ContentObjectKind::Prepared, b"orphan prepared")
        .unwrap();
    let image = fixture
        .store()
        .write_content_object(ContentObjectKind::Image, b"orphan image")
        .unwrap();
    let report = fixture.report();
    for (kind, hash) in [
        ("commit", &graph.commit),
        ("tree", &graph.tree),
        ("raw", &graph.raw),
        ("chunk", &graph.chunk),
        ("manifest", &graph.manifest),
        ("normalized_unit", &graph.unit),
        ("toollock", &graph.tool_lock),
        ("embedding", &graph.embedding),
    ] {
        assert_eq!(object(&report, kind, hash)["classification"], "protected");
    }
    for (kind, hash) in [("prepared", prepared), ("image", image)] {
        let row = object(&report, kind, &hash);
        assert_eq!(row["classification"], "inventory_only");
        assert_eq!(row["reason"], "verify_objects_orphan_lifecycle");
    }
    assert_eq!(report["summary"]["candidate_count"], 0);
}

#[test]
fn unreachable_commit_still_protects_its_tree_manifest_unit_and_tool_lock() {
    let fixture = Fixture::new();
    let live = fixture.live_graph(0, None);
    fixture.set_tip(&live.commit);
    let unreachable = fixture.live_graph(1, Some(live.commit.clone()));
    let report = fixture.report();
    assert_eq!(
        object(&report, "commit", &unreachable.commit)["reason"],
        "append_only_unreachable_history"
    );
    for (kind, hash) in [
        ("tree", &unreachable.tree),
        ("manifest", &unreachable.manifest),
        ("normalized_unit", &unreachable.unit),
        ("toollock", &unreachable.tool_lock),
    ] {
        assert_ne!(object(&report, kind, hash)["classification"], "candidate");
    }
}

#[test]
fn shallow_receipt_classifies_commit_and_explains_only_its_missing_tree() {
    let fixture = Fixture::new();
    let old = fixture.live_graph(0, None);
    let empty_tree = build_tree(vec![]).unwrap();
    let empty_tree_hash = fixture
        .store()
        .write_json(ObjectKind::Tree, &serde_json::to_value(empty_tree).unwrap())
        .unwrap()
        .0;
    let tip = fixture.commit(
        &empty_tree_hash,
        Some(old.commit.clone()),
        &old.tool_lock,
        AT_1,
    );
    fixture.set_tip(&tip);
    let receipt =
        ShallowReceipt::new(old.commit.clone(), old.tree.clone(), AT_1.to_owned()).unwrap();
    let receipt_dir = fixture.kio().join("gc/shallowed");
    fs::create_dir_all(&receipt_dir).unwrap();
    fs::write(
        receipt_dir.join(old.commit.strip_prefix("sha256:").unwrap()),
        receipt.canonical_bytes().unwrap(),
    )
    .unwrap();
    fs::remove_file(
        fixture
            .store()
            .object_path(ObjectKind::Tree, &old.tree)
            .unwrap(),
    )
    .unwrap();
    let report = fixture.report();
    assert_eq!(
        object(&report, "commit", &old.commit)["reason"],
        "append_only_shallow_history"
    );
    assert_eq!(
        object(&report, "manifest", &old.manifest)["classification"],
        "inventory_only"
    );
    assert_eq!(
        object(&report, "manifest", &old.manifest)["reason"],
        "shallow_history_unavailable"
    );
    assert_eq!(report["shallow_boundaries"].as_array().unwrap().len(), 1);

    fs::remove_file(
        fixture
            .store()
            .object_path(ObjectKind::Tree, &empty_tree_hash)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(fixture.error().error_code(), "KIO-E-STORE-CORRUPT-001");
}

#[test]
fn shallow_receipt_makes_orphaned_semantic_closure_inventory_only() {
    let fixture = Fixture::new();
    let orphan_raw = hash_bytes(b"orphan-manifest-under-shallow-history");
    let (_, _pinned_unit, orphan_manifest) = fixture.normalized_closure(&orphan_raw, 21);
    let absent_manifest_raw = hash_bytes(b"orphan-unit-under-shallow-history");
    let (_, orphan_unit, absent_manifest) = fixture.normalized_closure(&absent_manifest_raw, 22);
    fs::remove_file(
        fixture
            .store()
            .content_path(ContentObjectKind::Manifest, &absent_manifest)
            .unwrap(),
    )
    .unwrap();

    let boundary = fixture.live_graph(0, None);
    let empty_tree = build_tree(vec![]).unwrap();
    let empty_tree_hash = fixture
        .store()
        .write_json(ObjectKind::Tree, &serde_json::to_value(empty_tree).unwrap())
        .unwrap()
        .0;
    let tip = fixture.commit(
        &empty_tree_hash,
        Some(boundary.commit.clone()),
        &boundary.tool_lock,
        AT_1,
    );
    fixture.set_tip(&tip);
    let receipt = ShallowReceipt::new(
        boundary.commit.clone(),
        boundary.tree.clone(),
        AT_1.to_owned(),
    )
    .unwrap();
    let receipt_dir = fixture.kio().join("gc/shallowed");
    fs::create_dir_all(&receipt_dir).unwrap();
    fs::write(
        receipt_dir.join(boundary.commit.strip_prefix("sha256:").unwrap()),
        receipt.canonical_bytes().unwrap(),
    )
    .unwrap();
    fs::remove_file(
        fixture
            .store()
            .object_path(ObjectKind::Tree, &boundary.tree)
            .unwrap(),
    )
    .unwrap();

    let report = fixture.report();
    for (kind, hash) in [
        ("manifest", orphan_manifest.as_str()),
        ("normalized_unit", orphan_unit.as_str()),
    ] {
        let row = object(&report, kind, hash);
        assert_eq!(row["classification"], "inventory_only");
        assert_eq!(row["reason"], "shallow_history_unavailable");
    }
    assert_eq!(report["summary"]["candidate_count"], 0);
}

#[test]
fn malformed_hash_mismatch_and_missing_graph_objects_fail_closed() {
    let corrupt = Fixture::new();
    let tool_lock = corrupt.tool_lock(4);
    fs::write(
        corrupt
            .store()
            .content_path(ContentObjectKind::Toollock, &tool_lock)
            .unwrap(),
        b"corrupt",
    )
    .unwrap();
    assert_eq!(corrupt.error().error_code(), "KIO-E-STORE-CORRUPT-001");

    let missing_tool = Fixture::new();
    let graph = missing_tool.live_graph(0, None);
    missing_tool.set_tip(&graph.commit);
    fs::remove_file(
        missing_tool
            .store()
            .content_path(ContentObjectKind::Toollock, &graph.tool_lock)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(missing_tool.error().error_code(), "KIO-E-STORE-CORRUPT-001");

    let missing_manifest = Fixture::new();
    let graph = missing_manifest.live_graph(0, None);
    missing_manifest.set_tip(&graph.commit);
    fs::remove_file(
        missing_manifest
            .store()
            .content_path(ContentObjectKind::Manifest, &graph.manifest)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        missing_manifest.error().error_code(),
        "KIO-E-STORE-CORRUPT-001"
    );
}

#[test]
fn tree_normalize_identity_must_match_its_manifest() {
    let fixture = Fixture::new();
    let first = fixture.live_graph(0, None);
    let second_raw = fixture.store().write_raw(b"second raw").unwrap();
    let (_second_profile, _second_unit, second_manifest) =
        fixture.normalized_closure(&second_raw, 1);
    let mismatched_tree = fixture.tree(
        &first.raw,
        &hash_bytes(b"profile-0"),
        &second_manifest,
        "mismatched.md",
    );
    let commit = fixture.commit(&mismatched_tree, None, &first.tool_lock, AT_1);
    fixture.set_tip(&commit);
    assert_eq!(fixture.error().error_code(), "KIO-E-STORE-CORRUPT-001");
}

#[test]
fn completed_purge_and_only_pre_resurrection_manifest_gaps_are_accepted() {
    let fixture = Fixture::new();
    let original = fixture.live_graph(0, None);
    fixture.set_tip(&original.commit);

    let empty_tree = build_tree(vec![]).unwrap();
    let empty_tree_hash = fixture
        .store()
        .write_json(ObjectKind::Tree, &serde_json::to_value(empty_tree).unwrap())
        .unwrap()
        .0;
    let purge_commit = CommitObject::new_purged(
        empty_tree_hash,
        Some(original.commit.clone()),
        AT_1.to_owned(),
        "fixture purge".to_owned(),
        original.tool_lock.clone(),
        CommitStats {
            files_added: 0,
            files_modified: 0,
            files_deleted: 1,
        },
        vec![original.raw.clone()],
    )
    .unwrap();
    let purge_commit_hash = fixture
        .store()
        .write_json(
            ObjectKind::Commit,
            &serde_json::to_value(purge_commit).unwrap(),
        )
        .unwrap()
        .0;
    fixture.set_tip(&purge_commit_hash);

    let purge = PurgeState::open(fixture.kio()).unwrap();
    purge.ensure_purge_epoch(1).unwrap();
    let purge_id = new_ulid(fixture.root());
    purge
        .write_closure(
            &PurgeClosure::new(
                purge_id,
                vec![ClosureItem {
                    object_type: "raw".to_owned(),
                    hash: original.raw.clone(),
                }],
                Vec::new(),
            )
            .unwrap(),
        )
        .unwrap();
    purge
        .append_tombstone_event(
            &original.raw,
            LifecycleEvent::purged(AT_1, &purge_commit_hash, PurgeReason::Legal, "fixture", 1),
        )
        .unwrap();

    for path in [
        fixture
            .store()
            .object_path(ObjectKind::Raw, &original.raw)
            .unwrap(),
        fixture
            .store()
            .content_path(ContentObjectKind::Manifest, &original.manifest)
            .unwrap(),
        fixture.store().chunk_path(&original.chunk).unwrap(),
        fixture
            .store()
            .content_path(ContentObjectKind::Embedding, &original.embedding)
            .unwrap(),
    ] {
        fs::remove_file(path).unwrap();
    }
    let completed_report = fixture.report();
    assert_eq!(
        object(&completed_report, "normalized_unit", &original.unit)["classification"],
        "inventory_only"
    );
    assert_eq!(
        object(&completed_report, "normalized_unit", &original.unit)["reason"],
        "historical_manifest_unavailable"
    );

    assert_eq!(
        fixture.store().write_raw(b"raw-0").expect("republish raw"),
        original.raw
    );
    let (profile, _unit, new_manifest) = fixture.normalized_closure(&original.raw, 2);
    let resurrection_tree = fixture.tree(&original.raw, &profile, &new_manifest, "resurrected.md");
    let resurrection_commit = fixture.commit(
        &resurrection_tree,
        Some(purge_commit_hash),
        &original.tool_lock,
        AT_2,
    );
    fixture.set_tip(&resurrection_commit);
    purge
        .retire_tombstone(&original.raw, &resurrection_commit, AT_2, "fixture")
        .unwrap();
    fixture.report();

    let tombstone_path = purge.tombstone_path(&original.raw).unwrap();
    let canonical_tombstone = fs::read(&tombstone_path).unwrap();
    let mut malformed_tombstone: Value = serde_json::from_slice(&canonical_tombstone).unwrap();
    let opening_epoch = malformed_tombstone["events"][0]["lifecycle_epoch"].clone();
    malformed_tombstone["events"][1]["lifecycle_epoch"] = opening_epoch;
    fs::write(
        &tombstone_path,
        canonical_json_bytes(&malformed_tombstone).unwrap(),
    )
    .unwrap();
    assert_eq!(fixture.error().error_code(), "KIO-E-STORE-CORRUPT-001");
    fs::write(&tombstone_path, canonical_tombstone).unwrap();
    fixture.report();

    fs::remove_file(
        fixture
            .store()
            .content_path(ContentObjectKind::Manifest, &new_manifest)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(fixture.error().error_code(), "KIO-E-STORE-CORRUPT-001");
}

#[test]
fn completed_purge_does_not_explain_post_purge_tree_gaps() {
    let fixture = Fixture::new();
    let original = fixture.live_graph(0, None);
    let empty_tree_hash = fixture
        .store()
        .write_json(
            ObjectKind::Tree,
            &serde_json::to_value(build_tree(vec![]).unwrap()).unwrap(),
        )
        .unwrap()
        .0;
    let purge_commit = CommitObject::new_purged(
        empty_tree_hash,
        Some(original.commit.clone()),
        AT_1.to_owned(),
        "fixture purge".to_owned(),
        original.tool_lock.clone(),
        CommitStats {
            files_added: 0,
            files_modified: 0,
            files_deleted: 1,
        },
        vec![original.raw.clone()],
    )
    .unwrap();
    let purge_commit_hash = fixture
        .store()
        .write_json(
            ObjectKind::Commit,
            &serde_json::to_value(purge_commit).unwrap(),
        )
        .unwrap()
        .0;
    let purge = PurgeState::open(fixture.kio()).unwrap();
    purge.ensure_purge_epoch(1).unwrap();
    purge
        .append_tombstone_event(
            &original.raw,
            LifecycleEvent::purged(AT_1, &purge_commit_hash, PurgeReason::Legal, "fixture", 1),
        )
        .unwrap();
    fs::remove_file(
        fixture
            .store()
            .object_path(ObjectKind::Raw, &original.raw)
            .unwrap(),
    )
    .unwrap();
    fs::remove_file(
        fixture
            .store()
            .content_path(ContentObjectKind::Manifest, &original.manifest)
            .unwrap(),
    )
    .unwrap();

    let post_purge_tree = fixture.tree(
        &original.raw,
        &hash_bytes(b"profile-0"),
        &original.manifest,
        "post-purge.md",
    );
    let post_purge_commit = fixture.commit(
        &post_purge_tree,
        Some(purge_commit_hash),
        &original.tool_lock,
        AT_2,
    );
    fixture.set_tip(&post_purge_commit);
    assert_eq!(fixture.error().error_code(), "KIO-E-STORE-CORRUPT-001");
}

#[cfg(unix)]
#[test]
fn symlink_and_unsafe_hardlink_are_rejected() {
    use std::os::unix::fs::symlink;

    let symlinked = Fixture::new();
    let hash = symlinked.tool_lock(1);
    let object_path = symlinked
        .store()
        .content_path(ContentObjectKind::Toollock, &hash)
        .unwrap();
    let outside = symlinked.root().join("outside-tool-lock");
    fs::write(&outside, fs::read(&object_path).unwrap()).unwrap();
    fs::remove_file(&object_path).unwrap();
    symlink(&outside, &object_path).unwrap();
    assert_eq!(symlinked.error().error_code(), "KIO-E-STORE-CORRUPT-001");

    let linked = Fixture::new();
    let hash = linked.tool_lock(2);
    let object_path = linked
        .store()
        .content_path(ContentObjectKind::Toollock, &hash)
        .unwrap();
    fs::hard_link(&object_path, linked.root().join("second-link")).unwrap();
    assert_eq!(linked.error().error_code(), "KIO-E-STORE-CORRUPT-001");
}

#[test]
fn active_gc_purge_and_uncertain_writer_states_fail_closed() {
    let active_gc = Fixture::new();
    fs::create_dir_all(active_gc.kio().join("gc")).unwrap();
    fs::write(active_gc.kio().join("gc/in_progress"), b"active").unwrap();
    assert_ne!(active_gc.error().exit_code().code(), 0);

    let active_purge = Fixture::new();
    fs::create_dir_all(active_purge.kio().join("purge")).unwrap();
    fs::write(active_purge.kio().join("purge/in-progress.json"), b"active").unwrap();
    assert_eq!(
        active_purge.error().error_code(),
        "KIO-E-PURGE-JOURNAL-ACTIVE-001"
    );

    let writer = Fixture::new();
    fs::write(writer.kio().join(".lock"), b"uncertain writer").unwrap();
    assert_ne!(writer.error().exit_code().code(), 0);

    let crashed_writer = Fixture::new();
    fs::write(
        crashed_writer.kio().join(".lock"),
        br#"{"pid":999999999,"token":"crashed","created_at":"2026-08-20T00:00:00Z"}"#,
    )
    .unwrap();
    assert_eq!(
        crashed_writer.error().error_code(),
        "KIO-E-STORE-LOCKED-001"
    );
}

#[test]
fn object_byte_and_history_limits_fail_instead_of_returning_empty_candidates() {
    let object_limited = Fixture::new();
    object_limited.tool_lock(1);
    let limits = UnreachableInventoryLimits {
        max_objects: 0,
        ..UnreachableInventoryLimits::default()
    };
    let error = UnreachableObjectInventory::bind(object_limited.root().canonicalize().unwrap())
        .unwrap()
        .with_limits(limits)
        .inventory()
        .unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-GC-PLAN-LIMIT-001");

    let byte_limited = Fixture::new();
    byte_limited.tool_lock(1);
    let limits = UnreachableInventoryLimits {
        max_physical_bytes: 0,
        ..UnreachableInventoryLimits::default()
    };
    let error = UnreachableObjectInventory::bind(byte_limited.root().canonicalize().unwrap())
        .unwrap()
        .with_limits(limits)
        .inventory()
        .unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-GC-PLAN-LIMIT-001");

    let history_limited = Fixture::new();
    let first = history_limited.live_graph(0, None);
    let second = history_limited.live_graph(1, Some(first.commit));
    history_limited.set_tip(&second.commit);
    let limits = UnreachableInventoryLimits {
        max_history_steps: 1,
        ..UnreachableInventoryLimits::default()
    };
    let error = UnreachableObjectInventory::bind(history_limited.root().canonicalize().unwrap())
        .unwrap()
        .with_limits(limits)
        .inventory()
        .unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-GC-PLAN-LIMIT-001");
}
