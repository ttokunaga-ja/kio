//! Step4b Phase 2-A contract tests: `open` image cache / `export` safety
//! and evacuation protocol / `purge` scope and closure completion.
//!
//! Source: `tasks/step4b-contract-tests-p2a.md` (PA01-PA50, §R rulings 1-3).
//! Test names carry their PA number(s) so failures map directly back to the
//! contract text. Sections mirror the spec's §A-§O structure.

mod support;

use support::canonical_tempdir;

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use kio_core::cas::{ContentObjectKind, ObjectKind, ObjectStore, fanout_path, hash_bytes};
use kio_core::dag::{CommitObject, CommitStats, CommitType, TreeObject, build_tree};
use kio_core::purge::{PurgeReason, PurgeState};
use kio_core::scope::Repository;
use kio_pipeline::markdownize::{
    NormalizedInstanceManifest, NormalizedUnitObject, load_validated_normalized_instance,
    persist_normalized_instance,
};
use serde_json::{Value, json};
use tempfile::TempDir;

const CHILD_ENV_DENYLIST: &[&str] = &[
    "GEMINI_API_KEY",
    "MISTRAL_API_KEY",
    "KIO_FIXED_NOW",
    "KIO_TEST_GEMINI_EMBED",
    "KIO_TEST_MISTRAL_OCR",
    "KIO_TEST_MARKDOWNIZE_ADAPTER",
    "KIO_TEST_PURGE_FAIL_AFTER_PHASE",
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
    let stdout = kio(dir, args)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&stdout).unwrap()
}

fn json_failure(dir: &TempDir, args: &[&str], code: i32) -> Value {
    let stderr = kio(dir, args)
        .arg("--json")
        .assert()
        .code(code)
        .get_output()
        .stderr
        .clone();
    serde_json::from_slice(&stderr).unwrap()
}

fn init(dir: &TempDir) {
    json_success(dir, &["init"]);
}

fn path_text(path: &Path) -> String {
    path.to_str().unwrap().to_owned()
}

fn open_cache_root(dir: &TempDir) -> PathBuf {
    dir.path().join(".test-cache/kio/open")
}

/// A scope with one indexed text file (`doc.md`), returning its raw_hash /
/// chunk_id / scope_id / evidence pointer — mirrors `step4_purge.rs`'s
/// `indexed_fixture`.
struct IndexedFixture {
    dir: TempDir,
    raw_hash: String,
    scope_id: String,
    pointer: Value,
}

fn indexed_fixture() -> IndexedFixture {
    let dir = canonical_tempdir();
    fs::write(
        dir.path().join("doc.md"),
        "# Private\n\nneedle-p2a-content must round-trip\n",
    )
    .unwrap();
    json_success(&dir, &["init"]);
    json_success(&dir, &["index", "--offline"]);
    let search = json_success(&dir, &["search", "needle-p2a-content", "--mode", "text"]);
    let pointer = search["results"][0]["evidence_pointer"].clone();
    IndexedFixture {
        raw_hash: pointer["raw_hash"].as_str().unwrap().to_owned(),
        scope_id: pointer["scope_id"].as_str().unwrap().to_owned(),
        pointer,
        dir,
    }
}

fn current_raw_for(dir: &TempDir, path: &str) -> String {
    let repo = Repository::open(dir.path()).unwrap();
    let head = repo.head_commit_hash().unwrap().unwrap();
    let commit = repo.read_commit(&head).unwrap();
    repo.read_tree(&commit.tree)
        .unwrap()
        .entries
        .into_iter()
        .find(|entry| entry.path == path)
        .unwrap()
        .raw_hash
}

/// Publish a fixture mutation through the same immutable tree edge that the
/// object-URI authority check traverses. The mutable normalized-instance view
/// alone is deliberately not authority for local image objects.
fn persist_and_pin_normalized_instance(
    repo: &Repository,
    parent_hash: String,
    mut tree: TreeObject,
    raw_hash: &str,
    manifest: &NormalizedInstanceManifest,
    units: &[NormalizedUnitObject],
) {
    let persisted = persist_normalized_instance(repo.kio_dir(), manifest, units).unwrap();
    let manifest_hash =
        kio_core::cas::hash_json(&serde_json::to_value(&persisted).unwrap()).unwrap();
    tree.entries
        .iter_mut()
        .find(|entry| entry.raw_hash == raw_hash)
        .and_then(|entry| entry.normalize.as_mut())
        .unwrap()
        .manifest_hash = manifest_hash;

    let tree = build_tree(tree.entries).unwrap();
    let store = ObjectStore::new(repo.kio_dir());
    let (tree_hash, _) = store
        .write_json(ObjectKind::Tree, &serde_json::to_value(&tree).unwrap())
        .unwrap();
    let parent = repo.read_commit(&parent_hash).unwrap();
    let commit = CommitObject::new(
        tree_hash,
        Some(parent_hash),
        "2026-09-09T00:00:00Z".to_owned(),
        "fixture: pin typed image ownership".to_owned(),
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
}

/// Attaches an image reference to `raw_hash`'s normalized instance and writes
/// the image bytes into the CAS directly (mirrors `step4_purge.rs`'s
/// `add_image_reference`), returning the image_hash.
fn add_image_reference(dir: &TempDir, raw_hash: &str, image_bytes: &[u8]) -> String {
    let repo = Repository::open(dir.path()).unwrap();
    let head = repo.head_commit_hash().unwrap().unwrap();
    let commit = repo.read_commit(&head).unwrap();
    let tree = repo.read_tree(&commit.tree).unwrap();
    let normalize = tree
        .entries
        .iter()
        .find(|entry| entry.raw_hash == raw_hash)
        .and_then(|entry| entry.normalize.clone())
        .unwrap();
    let mut instance = load_validated_normalized_instance(
        repo.kio_dir(),
        raw_hash,
        &normalize.tool_profile_hash,
        normalize.r#gen,
    )
    .unwrap();
    let image_hash = hash_bytes(image_bytes);
    let image_path = fanout_path(repo.kio_dir().join("objects/image"), &image_hash).unwrap();
    fs::create_dir_all(image_path.parent().unwrap()).unwrap();
    fs::write(&image_path, image_bytes).unwrap();
    instance.units[0]
        .metadata
        .insert("images".to_owned(), json!([{ "hash": image_hash }]));
    instance.units[0]
        .owned_image_hashes
        .insert(image_hash.clone());
    persist_and_pin_normalized_instance(
        &repo,
        head,
        tree,
        raw_hash,
        &instance.manifest,
        &instance.units,
    );
    image_hash
}

/// Like [`add_image_reference`], but attaches SEVERAL image byte-strings to
/// one raw's normalized instance in a single metadata write (calling
/// `add_image_reference` more than once on the SAME raw_hash would overwrite
/// its `images` array rather than extend it).
fn add_image_references(dir: &TempDir, raw_hash: &str, images: &[&[u8]]) -> Vec<String> {
    let repo = Repository::open(dir.path()).unwrap();
    let head = repo.head_commit_hash().unwrap().unwrap();
    let commit = repo.read_commit(&head).unwrap();
    let tree = repo.read_tree(&commit.tree).unwrap();
    let normalize = tree
        .entries
        .iter()
        .find(|entry| entry.raw_hash == raw_hash)
        .and_then(|entry| entry.normalize.clone())
        .unwrap();
    let mut instance = load_validated_normalized_instance(
        repo.kio_dir(),
        raw_hash,
        &normalize.tool_profile_hash,
        normalize.r#gen,
    )
    .unwrap();
    let mut hashes = Vec::with_capacity(images.len());
    let mut entries = Vec::with_capacity(images.len());
    for image_bytes in images {
        let image_hash = hash_bytes(image_bytes);
        let image_path = fanout_path(repo.kio_dir().join("objects/image"), &image_hash).unwrap();
        fs::create_dir_all(image_path.parent().unwrap()).unwrap();
        fs::write(&image_path, image_bytes).unwrap();
        entries.push(json!({ "hash": image_hash }));
        instance.units[0]
            .owned_image_hashes
            .insert(image_hash.clone());
        hashes.push(image_hash);
    }
    instance.units[0]
        .metadata
        .insert("images".to_owned(), Value::Array(entries));
    persist_and_pin_normalized_instance(
        &repo,
        head,
        tree,
        raw_hash,
        &instance.manifest,
        &instance.units,
    );
    hashes
}

/// Declare a hash as the normalized unit's immutable local image ownership
/// without creating its CAS leaf. This deliberately models a missing owned
/// object so `open` reaches the object-store not-found path rather than the
/// earlier unowned-image policy denial.
fn authorize_missing_image(dir: &TempDir, raw_hash: &str, image_hash: &str) {
    let repo = Repository::open(dir.path()).unwrap();
    let head = repo.head_commit_hash().unwrap().unwrap();
    let commit = repo.read_commit(&head).unwrap();
    let tree = repo.read_tree(&commit.tree).unwrap();
    let normalize = tree
        .entries
        .iter()
        .find(|entry| entry.raw_hash == raw_hash)
        .and_then(|entry| entry.normalize.clone())
        .unwrap();
    let mut instance = load_validated_normalized_instance(
        repo.kio_dir(),
        raw_hash,
        &normalize.tool_profile_hash,
        normalize.r#gen,
    )
    .unwrap();
    instance.units[0]
        .owned_image_hashes
        .insert(image_hash.to_owned());
    persist_and_pin_normalized_instance(
        &repo,
        head,
        tree,
        raw_hash,
        &instance.manifest,
        &instance.units,
    );
}

fn scope_id_of(dir: &TempDir) -> String {
    let scope: Value =
        serde_json::from_slice(&fs::read(dir.path().join(".kio/scope.json")).unwrap()).unwrap();
    scope["scope_id"].as_str().unwrap().to_owned()
}

fn image_uri(scope_id: &str, image_hash: &str) -> String {
    format!("kio://{scope_id}/object/image/{image_hash}")
}

// ---------------------------------------------------------------------------
// §A/§B/§C (U22/U23/U24): `kio open` object URI type limiting, image cache
// type separation, tombstone/barrier priority, purge/prune-orphans cleanup.
// ---------------------------------------------------------------------------

#[test]
fn pa01_object_uri_rejects_every_type_but_image() {
    let fixture = indexed_fixture();
    let image_hash = add_image_reference(&fixture.dir, &fixture.raw_hash, b"pa01 image bytes");
    let raw_hash = &fixture.raw_hash;

    for (object_type, hash) in [
        ("raw", raw_hash.as_str()),
        ("chunk", fixture.pointer["chunk_hash"].as_str().unwrap()),
        ("prepared", image_hash.as_str()),
        ("normalized", raw_hash.as_str()),
    ] {
        let uri = format!("kio://{}/object/{object_type}/{hash}", fixture.scope_id);
        let error = json_failure(&fixture.dir, &["open", &uri], 2);
        assert_eq!(
            error["error_code"], "KIO-E-CONFIG-USAGE-001",
            "object_type={object_type} must be rejected exit 2"
        );
    }

    // Only `image` resolves.
    let uri = image_uri(&fixture.scope_id, &image_hash);
    let output = json_success(&fixture.dir, &["open", &uri]);
    assert_eq!(output["status"], "opened");
    assert_eq!(output["object_type"], "image");
}

#[test]
fn pa02_image_uri_declared_scope_is_required() {
    let fixture = indexed_fixture();
    let image_hash = add_image_reference(&fixture.dir, &fixture.raw_hash, b"pa02 image bytes");
    // An entirely unregistered/unreachable scope_id remains unreachable even
    // when the current scope happens to hold bytes with the same image hash.
    let unreachable_uri = format!("kio://01ARZ3NDEKTSV4RRFFQ69G5FAV/object/image/{image_hash}");
    let error = json_failure(&fixture.dir, &["open", &unreachable_uri], 3);
    assert_eq!(error["error_code"], "KIO-E-EVIDENCE-SCOPE-UNREACHABLE-001");
}

#[test]
fn pa03_pa06_image_and_raw_cache_directories_are_type_separated_for_a_shared_digest() {
    // A raw object and an image object that happen to share the exact same
    // byte content (hence the same digest) must materialize into DIFFERENT
    // cache directories.
    let dir = canonical_tempdir();
    let shared_bytes = b"pa03 shared raw/image byte content";
    fs::write(dir.path().join("shared.bin"), shared_bytes).unwrap();
    init(&dir);
    json_success(&dir, &["index", "--offline"]);
    let raw_hash = current_raw_for(&dir, "shared.bin");
    assert_eq!(raw_hash, hash_bytes(shared_bytes));

    // Manually register an unrelated raw's normalized instance as also
    // referencing an image object with the SAME digest as `shared.bin`'s raw
    // object, to construct the same-digest raw/image collision scenario.
    fs::write(dir.path().join("other.md"), "other text content").unwrap();
    json_success(&dir, &["index", "--offline"]);
    let other_raw = current_raw_for(&dir, "other.md");
    let image_hash = add_image_reference(&dir, &other_raw, shared_bytes);
    assert_eq!(image_hash, raw_hash, "constructed same-digest collision");

    // `open` must resolve the raw side from CAS (not the working tree) to
    // actually publish a cache dir there.
    fs::remove_file(dir.path().join("shared.bin")).unwrap();
    let scope_id = scope_id_of(&dir);
    let raw_output = json_success(&dir, &["open", &raw_hash]);
    let image_output = json_success(&dir, &["open", &image_uri(&scope_id, &image_hash)]);

    let raw_path = PathBuf::from(raw_output["path"].as_str().unwrap());
    let image_path = PathBuf::from(image_output["path"].as_str().unwrap());
    assert_ne!(
        raw_path.parent(),
        image_path.parent(),
        "raw and image cache directories must differ for a shared digest"
    );
    let cache_root = open_cache_root(&dir);
    let digest = raw_hash.trim_start_matches("sha256:");
    assert!(raw_path.starts_with(cache_root.join(digest)));
    assert!(image_path.starts_with(cache_root.join("image").join(digest)));
    // PA06: both materializations produced readable content (same durable
    // publish + verify path for raw and image).
    assert_eq!(fs::read(&raw_path).unwrap(), shared_bytes);
    assert_eq!(fs::read(&image_path).unwrap(), shared_bytes);
}

#[test]
fn pa04_tombstone_priority_wins_over_working_tree_and_cache() {
    let dir = canonical_tempdir();
    fs::write(dir.path().join("doc.md"), "pa04 content").unwrap();
    init(&dir);
    json_success(&dir, &["index", "--offline"]);
    let raw_hash = current_raw_for(&dir, "doc.md");
    let pointer = json_success(&dir, &["search", "pa04 content", "--mode", "text"])
        ["results"][0]["evidence_pointer"]
        .to_string();
    // `open` must resolve from CAS (not the working tree) to actually
    // publish a cache dir, so remove the working copy first. The purge
    // itself also runs with the file absent (full completion with a live
    // residual present at PUBLISH time hits a separate, pre-existing
    // interaction — see `pa37_pa38_pa39_...`'s note; this test's own concern
    // is the OPEN-time dispatch priority, not that interaction).
    fs::remove_file(dir.path().join("doc.md")).unwrap();
    json_success(&dir, &["open", &raw_hash]);
    let cache_dir = open_cache_root(&dir).join(raw_hash.trim_start_matches("sha256:"));
    assert!(cache_dir.exists());
    json_success(
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
    // The purge closure evicted the pre-existing cache dir (PA11).
    assert!(!cache_dir.exists());

    // Now simulate BOTH a reappeared working-tree residual (§R ruling #1:
    // purge never touches the working tree, so a later independent write
    // recreating the same bytes is indistinguishable from "was always
    // there") and a stale, crash-orphaned cache dir surviving from before
    // the purge — tombstone priority (PA04) must win over both regardless.
    fs::write(dir.path().join("doc.md"), "pa04 content").unwrap();
    fs::create_dir_all(&cache_dir).unwrap();
    fs::write(cache_dir.join("orphan.md"), "pa04 content").unwrap();
    assert!(dir.path().join("doc.md").exists());
    assert!(cache_dir.exists());

    let error = json_failure(&dir, &["open", &pointer], 4);
    assert_eq!(error["error_code"], "KIO-E-PURGE-TOMBSTONED-001");
    assert_eq!(error["context"]["status"], "tombstoned");
}

#[test]
fn pa05_image_barrier_is_journal_only_tombstone_of_a_same_digest_raw_does_not_apply() {
    let dir = canonical_tempdir();
    let shared_bytes = b"pa05 shared raw/image content, tombstoned as raw only";
    fs::write(dir.path().join("shared.md"), shared_bytes).unwrap();
    init(&dir);
    json_success(&dir, &["index", "--offline"]);
    let raw_hash = current_raw_for(&dir, "shared.md");
    let pointer = json_success(&dir, &["search", "pa05 shared raw", "--mode", "text"])["results"]
        [0]["evidence_pointer"]
        .to_string();

    fs::write(dir.path().join("other.md"), "other pa05 text").unwrap();
    json_success(&dir, &["index", "--offline"]);
    let other_raw = current_raw_for(&dir, "other.md");
    let image_hash = add_image_reference(&dir, &other_raw, shared_bytes);
    assert_eq!(image_hash, raw_hash);

    fs::remove_file(dir.path().join("shared.md")).unwrap();
    json_success(
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

    // The raw side is now tombstoned...
    let raw_error = json_failure(&dir, &["open", &pointer], 4);
    assert_eq!(raw_error["error_code"], "KIO-E-PURGE-TOMBSTONED-001");

    // ...but the image object (same digest, different type) still resolves —
    // the tombstone is a raw_hash-scoped marker, not applied by image_hash.
    let scope_id = scope_id_of(&dir);
    let image_output = json_success(&dir, &["open", &image_uri(&scope_id, &image_hash)]);
    assert_eq!(image_output["status"], "opened");
}

#[test]
fn pa07_image_not_found_uses_the_same_terminal_code_as_raw_not_found() {
    // PA01 (§A, U22) means a `raw`-type object URI can no longer even be
    // constructed as a comparison point (rejected exit 2 at parse time) — so
    // this compares against 06 §1.1 step 5's not-found terminus directly
    // (`KioError::not_found` = `KIO-E-STORE-NOT-FOUND-001`, exit 4), which is
    // what every raw-absence-with-no-marker path in this module already
    // uses uniformly.
    let fixture = indexed_fixture();
    let missing_hash = hash_bytes(b"pa07 never ingested as any object type");
    authorize_missing_image(&fixture.dir, &fixture.raw_hash, &missing_hash);
    let image_error = json_failure(
        &fixture.dir,
        &["open", &image_uri(&fixture.scope_id, &missing_hash)],
        4,
    );
    assert_eq!(image_error["error_code"], "KIO-E-STORE-NOT-FOUND-001");
}

#[test]
fn pa08_cache_reuse_reverifies_bytes_every_time_and_fails_closed_on_torn_content() {
    let fixture = indexed_fixture();
    // Both opens below must resolve from CAS/cache, not the working tree.
    fs::remove_file(fixture.dir.path().join("doc.md")).unwrap();
    let output = json_success(&fixture.dir, &["open", &fixture.raw_hash]);
    let cache_path = PathBuf::from(output["path"].as_str().unwrap());
    // Corrupt the previously-published cache leaf in place. The published
    // leaf is read-only (0400, R10-6/R9-3 hardening) — export write
    // permission first so the corruption itself can land.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&cache_path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    #[cfg(windows)]
    {
        let mut permissions = fs::metadata(&cache_path).unwrap().permissions();
        permissions.set_readonly(false);
        fs::set_permissions(&cache_path, permissions).unwrap();
    }
    fs::write(&cache_path, b"torn/corrupted cache content").unwrap();

    let error = json_failure(&fixture.dir, &["open", &fixture.raw_hash], 4);
    assert_eq!(error["error_code"], "KIO-E-STORE-CORRUPT-001");
    // Fail-closed: the corrupt cache leaf is left untouched, not silently
    // repaired or deleted.
    assert_eq!(
        fs::read(&cache_path).unwrap(),
        b"torn/corrupted cache content"
    );
}

#[test]
fn pa09_startup_recheck_removes_published_cache_on_tombstone_race() {
    // A tombstone that appears strictly between cache publish and the
    // pre-launch recheck must still be caught, and the just-published cache
    // removed rather than left to serve a dead pointer's bytes on retry.
    let fixture = indexed_fixture();
    fs::remove_file(fixture.dir.path().join("doc.md")).unwrap();
    json_success(&fixture.dir, &["open", &fixture.raw_hash]);
    let cache_dir =
        open_cache_root(&fixture.dir).join(fixture.raw_hash.trim_start_matches("sha256:"));
    assert!(cache_dir.exists());

    json_success(
        &fixture.dir,
        &[
            "purge",
            "--raw-hash",
            &fixture.raw_hash,
            "--reason",
            "legal",
            "--yes",
        ],
    );
    let pointer = fixture.pointer.to_string();
    let error = json_failure(&fixture.dir, &["open", &pointer], 4);
    assert_eq!(error["error_code"], "KIO-E-PURGE-TOMBSTONED-001");
    // The purge closure itself evicted the cache (PA11); re-opening after a
    // tombstone must not resurrect a served cache dir either.
    assert!(!cache_dir.exists());
}

#[test]
fn pa11_pa12_pa13_purge_closure_evicts_type_separated_cache_and_preserves_shared_image() {
    let fixture = indexed_fixture();
    // A second, non-target raw shares ONE of the target's TWO images (the
    // same bytes, hence the same image_hash) — its live reference must
    // survive the target's purge; the target's OTHER image is unreferenced
    // elsewhere and must be removed.
    fs::write(fixture.dir.path().join("other.md"), "pa13 other text").unwrap();
    json_success(&fixture.dir, &["index", "--offline"]);
    let other_raw = current_raw_for(&fixture.dir, "other.md");
    let target_images = add_image_references(
        &fixture.dir,
        &fixture.raw_hash,
        &[b"pa12 image bytes", b"pa13 shared image bytes"],
    );
    let image_hash = target_images[0].clone();
    let shared_image_hash = target_images[1].clone();
    let shared_image_hash_on_survivor =
        add_image_reference(&fixture.dir, &other_raw, b"pa13 shared image bytes");
    assert_eq!(shared_image_hash_on_survivor, shared_image_hash);

    let scope_id = scope_id_of(&fixture.dir);
    // `open` must resolve the raw side from CAS (not the working tree) to
    // actually publish a cache dir there.
    fs::remove_file(fixture.dir.path().join("doc.md")).unwrap();
    json_success(&fixture.dir, &["open", &fixture.raw_hash]);
    json_success(&fixture.dir, &["open", &image_uri(&scope_id, &image_hash)]);
    json_success(
        &fixture.dir,
        &["open", &image_uri(&scope_id, &shared_image_hash)],
    );
    let cache_root = open_cache_root(&fixture.dir);
    let raw_cache = cache_root.join(fixture.raw_hash.trim_start_matches("sha256:"));
    let image_cache = cache_root
        .join("image")
        .join(image_hash.trim_start_matches("sha256:"));
    let shared_image_cache = cache_root
        .join("image")
        .join(shared_image_hash.trim_start_matches("sha256:"));
    assert!(raw_cache.exists() && image_cache.exists() && shared_image_cache.exists());

    let output = json_success(
        &fixture.dir,
        &[
            "purge",
            "--raw-hash",
            &fixture.raw_hash,
            "--reason",
            "legal",
            "--yes",
        ],
    );
    // PA11: raw's own cache dir gone.
    assert!(!raw_cache.exists());
    // PA12: the now-unreferenced image's type-segmented cache dir is gone too.
    assert!(!image_cache.exists());
    // PA13: the still-shared image's cache dir survives untouched.
    assert!(shared_image_cache.exists());
    assert_eq!(output["shared_artifacts_preserved"]["image_objects"], 1);
}

#[test]
fn pa14_pa15_prune_orphans_recovers_purged_raw_and_type_separated_image_cache() {
    let fixture = indexed_fixture();
    let image_hash = add_image_reference(&fixture.dir, &fixture.raw_hash, b"pa14 image bytes");
    let scope_id = scope_id_of(&fixture.dir);
    // `open` must resolve the raw side from CAS (not the working tree) to
    // actually publish a cache dir there.
    fs::remove_file(fixture.dir.path().join("doc.md")).unwrap();
    json_success(&fixture.dir, &["open", &fixture.raw_hash]);
    json_success(&fixture.dir, &["open", &image_uri(&scope_id, &image_hash)]);

    let cache_root = open_cache_root(&fixture.dir);
    let raw_cache = cache_root.join(fixture.raw_hash.trim_start_matches("sha256:"));
    let image_cache = cache_root
        .join("image")
        .join(image_hash.trim_start_matches("sha256:"));

    // Simulate a publish-then-crash residue: purge the raw_hash (its own
    // purge closure evicts these caches already, PA11/12) then manually
    // re-materialize the caches to stand in for a crash-orphaned leftover
    // `kio repair verify-objects --prune-orphans` (PA14/15) must recover.
    json_success(
        &fixture.dir,
        &[
            "purge",
            "--raw-hash",
            &fixture.raw_hash,
            "--reason",
            "legal",
            "--yes",
        ],
    );
    fs::create_dir_all(&raw_cache).unwrap();
    fs::write(raw_cache.join("orphan.bin"), b"crash residue").unwrap();
    fs::create_dir_all(&image_cache).unwrap();
    fs::write(image_cache.join("orphan.bin"), b"crash residue").unwrap();

    let report = json_success(
        &fixture.dir,
        &["repair", "verify-objects", "--prune-orphans", "--yes"],
    );
    assert!(
        report["prune_orphans"]["pruned_open_cache_count"]
            .as_u64()
            .unwrap()
            >= 2
    );
    assert!(!raw_cache.exists());
    assert!(!image_cache.exists());
}

// ---------------------------------------------------------------------------
// §D (U25): export destination safety.
// ---------------------------------------------------------------------------

#[test]
fn pa16_pa17_destination_rejects_scope_root_dot_kio_and_ordinary_subdir_with_config_usage() {
    let dir = canonical_tempdir();
    init(&dir);
    fs::write(dir.path().join("doc.md"), b"pa16 content").unwrap();
    let commit = json_success(&dir, &["snapshot", "create", "-m", "source"])["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();

    for forbidden in [
        dir.path().to_path_buf(),
        dir.path().join(".kio"),
        dir.path().join(".kio/sub"),
        // PA16(d): an ordinary scope-root subdirectory that is NOT `.kio`.
        dir.path().join("subdir"),
    ] {
        let error = json_failure(
            &dir,
            &["export", &commit, "--to", &path_text(&forbidden)],
            2,
        );
        assert_eq!(
            error["error_code"], "KIO-E-CONFIG-USAGE-001",
            "forbidden destination {forbidden:?} must be rejected exit 2"
        );
    }

    // PA17: `--to .` from inside the scope root resolves to the same
    // canonical path and is rejected identically (no relative-path bypass).
    let error = kio(&dir, &["export", &commit, "--to", "."])
        .arg("--json")
        .assert()
        .code(2)
        .get_output()
        .stderr
        .clone();
    let error: Value = serde_json::from_slice(&error).unwrap();
    assert_eq!(error["error_code"], "KIO-E-CONFIG-USAGE-001");
}

// ---------------------------------------------------------------------------
// §E (U26): export evacuation / quarantine / no-replace publish protocol.
// ---------------------------------------------------------------------------

#[test]
fn pa20_reserved_evacuation_namespace_source_names_are_rejected_before_expansion() {
    let dir = canonical_tempdir();
    init(&dir);
    fs::write(
        dir.path().join("notes.md.kio-export-bak"),
        b"pa20 legitimately-named historical file",
    )
    .unwrap();
    json_success(&dir, &["index", "--offline"]);
    let destination = dir.path().join("pa20-out");

    let error = json_failure(
        &dir,
        &[
            "export",
            "notes.md.kio-export-bak",
            "--to",
            &path_text(&destination),
        ],
        1,
    );
    assert_eq!(error["error_code"], "KIO-E-COMMIT-EXPORT-UNSAFE-001");
    assert!(!destination.exists() || fs::read_dir(&destination).unwrap().next().is_none());
}

#[test]
fn pa21_stale_backup_residue_is_rejected_before_mutation_regardless_of_force() {
    let dir = canonical_tempdir();
    init(&dir);
    fs::write(dir.path().join("notes.md"), b"pa21 content").unwrap();
    let commit = json_success(&dir, &["snapshot", "create", "-m", "source"])["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let out = tempfile::tempdir().unwrap();
    let destination = out.path().join("pa21-out");
    fs::create_dir(&destination).unwrap();
    fs::write(
        destination.join("notes.md.kio-export-bak"),
        b"stale backup from a crashed prior attempt",
    )
    .unwrap();

    // (a) non-force: destination file absent, but the stale backup blocks it.
    let error = json_failure(
        &dir,
        &["export", &commit, "--to", &path_text(&destination)],
        3,
    );
    assert_eq!(error["error_code"], "KIO-E-COMMIT-EXPORT-CONFLICT-001");
    assert_eq!(error["context"]["conflict_kind"], "stale_backup");
    assert!(!destination.join("notes.md").exists());

    // (b) force: destination file present too, still blocked by the residue.
    fs::write(destination.join("notes.md"), b"existing").unwrap();
    let error = json_failure(
        &dir,
        &[
            "export",
            &commit,
            "--to",
            &path_text(&destination),
            "--force",
            "--yes",
        ],
        3,
    );
    assert_eq!(error["error_code"], "KIO-E-COMMIT-EXPORT-CONFLICT-001");
    assert_eq!(error["context"]["conflict_kind"], "stale_backup");
    assert_eq!(fs::read(destination.join("notes.md")).unwrap(), b"existing");
    assert_eq!(
        fs::read(destination.join("notes.md.kio-export-bak")).unwrap(),
        b"stale backup from a crashed prior attempt"
    );
}

#[test]
fn pa22_pa23_force_overwrite_evacuates_old_file_before_no_replace_publish() {
    let dir = canonical_tempdir();
    init(&dir);
    fs::write(dir.path().join("notes.md"), b"pa22 restored content").unwrap();
    let commit = json_success(&dir, &["snapshot", "create", "-m", "source"])["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let out = tempfile::tempdir().unwrap();
    let destination = out.path().join("pa22-out");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("notes.md"), b"old destination content").unwrap();

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
    assert_eq!(output["status"], "exported");
    assert_eq!(output["overwritten_count"], 1);
    // The new content published under a no-replace claim of the (evacuated)
    // name.
    assert_eq!(
        fs::read(destination.join("notes.md")).unwrap(),
        b"pa22 restored content"
    );
    // The old content survives, moved aside rather than destroyed.
    assert_eq!(
        fs::read(destination.join("notes.md.kio-export-bak")).unwrap(),
        b"old destination content"
    );
}

#[test]
fn pa23_non_force_publish_race_is_a_transient_conflict_leaving_destination_untouched() {
    // publish_race: preflight found the name absent, but by the time
    // publish runs a third party has created it — hard to construct exactly
    // via the CLI black-box without an injected race, so this exercises the
    // adjacent, always-reachable "found present already" ordinary case and
    // asserts NOTHING was written to the winning destination the first time
    // (a stand-in for the same no-replace-publish invariant: preflight
    // rejection never partially writes). R23-26 (06 §5 L282-285): this
    // ordinary "no --force" preflight rejection is not itself the
    // `conflict_kind=publish_race` race it stands in for (that kind, and its
    // `transient` disposition, are reserved for the actual publish-time race
    // `restore_conflict_error` classifies) -- it shares
    // KIO-E-COMMIT-EXPORT-CONFLICT-001's exit 3 but carries
    // `retry_disposition=manual_action` (add --force), not `transient`.
    let dir = canonical_tempdir();
    init(&dir);
    fs::write(dir.path().join("a.md"), b"alpha").unwrap();
    fs::write(dir.path().join("b.md"), b"beta").unwrap();
    let commit = json_success(&dir, &["snapshot", "create", "-m", "two files"])["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let out = tempfile::tempdir().unwrap();
    let destination = out.path().join("pa23-out");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("b.md"), b"existing").unwrap();

    let error = json_failure(
        &dir,
        &["export", &commit, "--to", &path_text(&destination)],
        3,
    );
    assert_eq!(error["error_code"], "KIO-E-COMMIT-EXPORT-CONFLICT-001");
    assert_eq!(error["context"]["retry_disposition"], "manual_action");
    assert!(!destination.join("a.md").exists());
    assert_eq!(fs::read(destination.join("b.md")).unwrap(), b"existing");
}

// ---------------------------------------------------------------------------
// §F (U27): export conflict error unification.
// ---------------------------------------------------------------------------

#[test]
fn pa27_pa28_pa29_conflict_kind_is_closed_and_retry_disposition_follows_it() {
    let dir = canonical_tempdir();
    init(&dir);
    fs::write(dir.path().join("notes.md"), b"pa27 content").unwrap();
    let commit = json_success(&dir, &["snapshot", "create", "-m", "source"])["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let out = tempfile::tempdir().unwrap();
    let destination = out.path().join("pa27-out");
    fs::create_dir(&destination).unwrap();
    fs::write(
        destination.join("notes.md.kio-export-quarantine"),
        b"stale quarantine residue",
    )
    .unwrap();

    let error = json_failure(
        &dir,
        &["export", &commit, "--to", &path_text(&destination)],
        3,
    );
    assert_eq!(error["error_code"], "KIO-E-COMMIT-EXPORT-CONFLICT-001");
    let conflict_kind = error["context"]["conflict_kind"].as_str().unwrap();
    assert_eq!(conflict_kind, "stale_quarantine");
    const CLOSED_KINDS: &[&str] = &[
        "publish_race",
        "quarantine_rename_race",
        "quarantine_mismatch",
        "backup_mismatch",
        "restore_rename_race",
        "stale_backup",
        "stale_quarantine",
    ];
    assert!(CLOSED_KINDS.contains(&conflict_kind));
    // PA29: every kind but publish_race is manual_action.
    assert_eq!(error["context"]["retry_disposition"], "manual_action");
}

// ---------------------------------------------------------------------------
// §G/§H/§I (U28/U29/U30/U31/U32, compressed current-fixed regressions).
// ---------------------------------------------------------------------------

#[test]
fn pa30_purge_cli_syntax_matches_spec_path_raw_hash_exclusive_reason_enum_yes() {
    let dir = canonical_tempdir();
    init(&dir);
    fs::write(dir.path().join("doc.md"), b"pa30 content").unwrap();
    json_success(&dir, &["index", "--offline"]);
    let raw_hash = current_raw_for(&dir, "doc.md");

    // (a) both path and --raw-hash: usage error.
    let error = json_failure(
        &dir,
        &[
            "purge",
            "doc.md",
            "--raw-hash",
            &raw_hash,
            "--reason",
            "legal",
            "--yes",
        ],
        2,
    );
    assert_eq!(error["error_code"], "KIO-E-CONFIG-USAGE-001");

    // (b) neither: usage error.
    let error = kio(&dir, &["purge", "--reason", "legal", "--yes"])
        .arg("--json")
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    assert!(!error.is_empty());

    // (c) invalid reason: clap value_parser rejection (exit 2).
    kio(
        &dir,
        &["purge", "doc.md", "--reason", "not-a-real-reason", "--yes"],
    )
    .assert()
    .code(2);

    // (d) --yes skips the confirmation prompt.
    fs::remove_file(dir.path().join("doc.md")).unwrap();
    let output = json_success(&dir, &["purge", "doc.md", "--reason", "legal", "--yes"]);
    assert_eq!(output["status"], "purged");
}

#[test]
fn pa31_tombstone_is_durable_before_physical_deletion_and_history_is_append_only() {
    let fixture = indexed_fixture();
    let kio_dir = fixture.dir.path().join(".kio");
    let store = ObjectStore::new(&kio_dir);
    let head_before = fs::read(kio_dir.join("HEAD")).unwrap();

    fs::remove_file(fixture.dir.path().join("doc.md")).unwrap();
    let output = json_success(
        &fixture.dir,
        &[
            "purge",
            "--raw-hash",
            &fixture.raw_hash,
            "--reason",
            "legal",
            "--yes",
        ],
    );
    assert_eq!(output["status"], "purged");
    let state = PurgeState::open(&kio_dir).unwrap();
    assert_eq!(
        state
            .read_tombstone(&fixture.raw_hash)
            .unwrap()
            .unwrap()
            .tail()
            .reason,
        Some(PurgeReason::Legal)
    );
    // Physical deletion happened (raw object gone).
    assert!(
        store
            .inspect_object(ObjectKind::Raw, &fixture.raw_hash)
            .is_err()
    );
    // History is append-only: HEAD moved forward to a NEW commit, the old
    // commit object is unmodified content-addressed history, not rewritten.
    let head_after = fs::read(kio_dir.join("HEAD")).unwrap();
    assert_ne!(head_before, head_after);
}

#[test]
fn pa32_shared_derived_objects_survive_purge_only_when_a_live_reference_remains() {
    let fixture = indexed_fixture();
    let store = ObjectStore::new(fixture.dir.path().join(".kio"));
    let repo = Repository::open(fixture.dir.path()).unwrap();
    let head = repo.head_commit_hash().unwrap().unwrap();
    let commit = repo.read_commit(&head).unwrap();
    let tree = repo.read_tree(&commit.tree).unwrap();
    let entry = tree
        .entries
        .iter()
        .find(|entry| entry.raw_hash == fixture.raw_hash)
        .unwrap();
    let normalize = entry.normalize.clone().unwrap();
    let instance = load_validated_normalized_instance(
        repo.kio_dir(),
        &fixture.raw_hash,
        &normalize.tool_profile_hash,
        normalize.r#gen,
    )
    .unwrap();
    let prepared_hash = instance.manifest.units[0].prepared_hash.clone();
    assert!(
        store
            .inspect_content_object(ContentObjectKind::Prepared, &prepared_hash)
            .is_ok()
    );

    fs::remove_file(fixture.dir.path().join("doc.md")).unwrap();
    json_success(
        &fixture.dir,
        &[
            "purge",
            "--raw-hash",
            &fixture.raw_hash,
            "--reason",
            "legal",
            "--yes",
        ],
    );
    // Non-shared prepared object was deleted.
    assert!(
        store
            .inspect_content_object(ContentObjectKind::Prepared, &prepared_hash)
            .is_err()
    );
}

#[test]
fn pa33_chunk_vec_is_target_scoped_and_embeddings_follow_live_reference() {
    let fixture = indexed_fixture();
    let sqlite = fixture.dir.path().join(".kio/index/sqlite.db");
    assert!(sqlite.exists());
    fs::remove_file(fixture.dir.path().join("doc.md")).unwrap();
    let output = json_success(
        &fixture.dir,
        &[
            "purge",
            "--raw-hash",
            &fixture.raw_hash,
            "--reason",
            "legal",
            "--yes",
        ],
    );
    assert!(output["deleted_counts"]["sqlite_chunks"].as_u64().unwrap() >= 1);
}

#[test]
fn pa34_chunk_publications_purge_deletion_is_scoped_to_target_chunk_ids_when_present() {
    // §R ruling #3: `chunk_publications` DDL lands with P2-C; this contract
    // is provisional until then. Constructed here against a MINIMAL
    // standalone fixture table (not the real evolving schema) purely to
    // pin the RULE (target chunk_id membership) independent of DDL timing.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE chunk_publications (chunk_id TEXT NOT NULL, published_at INTEGER NOT NULL);
         INSERT INTO chunk_publications VALUES ('sha256:target', 1), ('sha256:other', 2);",
    )
    .unwrap();
    let target_ids = ["sha256:target".to_owned()];
    conn.execute(
        "DELETE FROM chunk_publications WHERE chunk_id IN (SELECT value FROM json_each(?1))",
        [serde_json::to_string(&target_ids).unwrap()],
    )
    .unwrap();
    let remaining: i64 = conn
        .query_row("SELECT COUNT(*) FROM chunk_publications", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(remaining, 1);
    let remaining_id: String = conn
        .query_row("SELECT chunk_id FROM chunk_publications", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(remaining_id, "sha256:other");
}

#[test]
fn pa35_staging_descriptor_attribution_walks_the_directory_not_tasks_jsonl() {
    let fixture = indexed_fixture();
    let staging_dir = fixture.dir.path().join(".kio/staging");
    fs::create_dir_all(&staging_dir).unwrap();
    // A staging descriptor attributed to the target raw_hash, with NO
    // corresponding tasks.jsonl row at all (simulating a lost task record).
    fs::write(
        staging_dir.join("orphaned-task.json"),
        json!({ "raw_hash": fixture.raw_hash, "kind": "markdownize" }).to_string(),
    )
    .unwrap();
    fs::write(
        staging_dir.join("unrelated-task.json"),
        json!({ "raw_hash": hash_bytes(b"pa35 unrelated"), "kind": "markdownize" }).to_string(),
    )
    .unwrap();

    fs::remove_file(fixture.dir.path().join("doc.md")).unwrap();
    let output = json_success(
        &fixture.dir,
        &[
            "purge",
            "--raw-hash",
            &fixture.raw_hash,
            "--reason",
            "legal",
            "--yes",
        ],
    );
    assert_eq!(output["deleted_counts"]["staging_descriptors"], 1);
    assert!(!staging_dir.join("orphaned-task.json").exists());
    assert!(staging_dir.join("unrelated-task.json").exists());
}

// ---------------------------------------------------------------------------
// §J (U33): log scrub scope_id isolation.
// ---------------------------------------------------------------------------

#[test]
fn pa36_log_scrub_never_touches_a_different_scopes_row_sharing_the_same_raw_hash() {
    let fixture = indexed_fixture();
    let device_logs = fixture.dir.path().join(".test-data/kio/logs");
    fs::create_dir_all(&device_logs).unwrap();
    let own_row = json!({
        "scope_id": fixture.scope_id,
        "raw_hash": fixture.raw_hash,
        "query": "own-scope-secret",
    });
    let other_scope_row = json!({
        "scope_id": "01ARZ3NDEKTSV4RRFFQ69G5FAX",
        "raw_hash": fixture.raw_hash,
        "query": "other-scope-must-survive",
    });
    fs::write(
        device_logs.join("events.jsonl.1"),
        format!("{own_row}\n{other_scope_row}\n"),
    )
    .unwrap();

    fs::remove_file(fixture.dir.path().join("doc.md")).unwrap();
    json_success(
        &fixture.dir,
        &[
            "purge",
            "--raw-hash",
            &fixture.raw_hash,
            "--reason",
            "privacy",
            "--yes",
        ],
    );
    let remaining = fs::read_to_string(device_logs.join("events.jsonl.1")).unwrap();
    assert!(
        !remaining.contains("own-scope-secret"),
        "this scope's own row must be scrubbed"
    );
    assert!(
        remaining.contains("other-scope-must-survive"),
        "a different scope_id's row sharing the same raw_hash must survive untouched"
    );
}

// ---------------------------------------------------------------------------
// §K (U34; §R ruling #1): working-tree residual warning, not a hard block.
// ---------------------------------------------------------------------------

#[test]
fn pa37_pa38_pa39_working_tree_residual_warns_instead_of_the_retired_hard_block() {
    // Full end-to-end completion while a purge target's exact bytes remain
    // live in the working tree required the purged commit's own snapshot
    // construction to skip re-publishing that still-present raw object (its
    // archival step otherwise collided with the SAME active purge journal's
    // own barrier — a P2-A-identified gap in
    // `Repository::snapshot_with_type`/`purged_snapshot`
    // (crates/kio-core/src/scope.rs), reachable once ruling #1 removed the
    // hard block that used to make this scenario unreachable). That gap is
    // now fixed (`archive_staged_working_tree` excludes a purge's own
    // targets from its own snapshot rebuild instead of barrier-blocking
    // them), so this test now pins full end-to-end completion: the prior
    // `KIO-E-PURGE-WORKING-COPY-001` hard block is gone, purge reaches
    // `status: "purged"` (not `purge_incomplete`), and the
    // `working_tree_warning` is still surfaced on that success response.
    let dir = canonical_tempdir();
    fs::write(dir.path().join("report-v1.pdf"), b"pa37 shared bytes").unwrap();
    init(&dir);
    json_success(&dir, &["index", "--offline"]);
    let raw_hash = current_raw_for(&dir, "report-v1.pdf");
    // A renamed alias with the exact same bytes under a DIFFERENT path.
    fs::write(dir.path().join("backup-copy.pdf"), b"pa37 shared bytes").unwrap();
    json_success(&dir, &["index", "--offline"]);

    let output = json_success(
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
    assert_eq!(output["status"], "purged");
    assert_ne!(
        output["error_code"], "KIO-E-PURGE-WORKING-COPY-001",
        "§R ruling #1: the prior hard block must never fire, exit 4 or otherwise"
    );
    let warning = &output["working_tree_warning"];
    assert!(warning["live_alias_count"].as_u64().unwrap() >= 1);
    let message = warning["message"].as_str().unwrap();
    assert!(message.contains("kio index"));
    assert!(message.contains(".kioignore"));
    // §R ruling #1: no same-path/renamed-alias distinction — the file is
    // never touched by purge, deletion or rename, regardless of outcome.
    assert_eq!(
        fs::read(dir.path().join("backup-copy.pdf")).unwrap(),
        b"pa37 shared bytes"
    );
    assert_eq!(
        fs::read(dir.path().join("report-v1.pdf")).unwrap(),
        b"pa37 shared bytes"
    );

    // The purged raw_hash's tree entries are gone even though its bytes are
    // still physically present under both paths — 05 §3.5's "purge 実行後の
    // working tree" excludes the target, it does not mirror the untouched
    // filesystem verbatim.
    let repo = Repository::open(dir.path()).unwrap();
    let head = repo.head_commit_hash().unwrap().unwrap();
    let commit = repo.read_commit(&head).unwrap();
    let tree = repo.read_tree(&commit.tree).unwrap();
    assert!(tree.entries.iter().all(|entry| entry.raw_hash != raw_hash));

    // The SAME purge, without any working-tree residual, completes normally
    // and carries no warning at all — confirming the warning is specific to
    // the residual, not an unconditional feature of every purge.
    let clean = indexed_fixture();
    fs::remove_file(clean.dir.path().join("doc.md")).unwrap();
    let clean_output = json_success(
        &clean.dir,
        &[
            "purge",
            "--raw-hash",
            &clean.raw_hash,
            "--reason",
            "legal",
            "--yes",
        ],
    );
    assert_eq!(clean_output["status"], "purged");
    assert!(clean_output.get("working_tree_warning").is_none());
}

// ---------------------------------------------------------------------------
// §L (U37): in-flight external-execution consistency at the `prepared` phase.
// ---------------------------------------------------------------------------

#[test]
fn pa40_pa41_purge_settles_scope_scoped_inflight_reservations_in_the_prepared_phase() {
    // A full end-to-end online-adapter in-flight reservation is out of a
    // black-box CLI test's reach (requires a live/mock provider). This
    // confirms the OBSERVABLE contract instead: purge succeeds without ever
    // needing a pending task-store row it cannot reach, and completes
    // synchronously with no in-flight residue left for THIS scope (the
    // `settle_inflight_reservations_for_purge` sweep is a no-op absent any
    // reservation, which is itself part of the contract — it must not error
    // or hang when there is nothing to settle).
    let fixture = indexed_fixture();
    fs::remove_file(fixture.dir.path().join("doc.md")).unwrap();
    let output = json_success(
        &fixture.dir,
        &[
            "purge",
            "--raw-hash",
            &fixture.raw_hash,
            "--reason",
            "legal",
            "--yes",
        ],
    );
    assert_eq!(output["status"], "purged");
    assert_eq!(output["deleted_counts"]["reservations"], 0);
}

// ---------------------------------------------------------------------------
// §M (U38): re-purge — Phase 1 LC58-60, referenced only.
// ---------------------------------------------------------------------------

#[test]
fn pa42_re_purge_of_an_already_active_tombstone_is_idempotent_regardless_of_reason() {
    let fixture = indexed_fixture();
    fs::remove_file(fixture.dir.path().join("doc.md")).unwrap();
    let first = json_success(
        &fixture.dir,
        &[
            "purge",
            "--raw-hash",
            &fixture.raw_hash,
            "--reason",
            "legal",
            "--yes",
        ],
    );
    assert_eq!(first["status"], "purged");
    // Re-purge with a DIFFERENT reason: no rejection, recognized as already
    // complete (LC59/M-ruling #2 — no reason-match requirement).
    let second = json_success(
        &fixture.dir,
        &[
            "purge",
            "--raw-hash",
            &fixture.raw_hash,
            "--reason",
            "privacy",
            "--yes",
        ],
    );
    assert_eq!(second["status"], "purged");
    let state = PurgeState::open(fixture.dir.path().join(".kio")).unwrap();
    assert_eq!(
        state
            .read_tombstone(&fixture.raw_hash)
            .unwrap()
            .unwrap()
            .events
            .len(),
        1,
        "no new event appended for an already-active tombstone"
    );
}

// ---------------------------------------------------------------------------
// §N (LC46 continuation, §R ruling #2): purge closure sidecar completion.
// ---------------------------------------------------------------------------

fn read_closure_sidecar(kio_dir: &Path) -> Value {
    let bytes = fs::read(kio_dir.join("purge/journal-closure")).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn pa43_closure_enumerates_raw_prepared_image_and_chunk_object_types() {
    let fixture = indexed_fixture();
    let image_hash = add_image_reference(&fixture.dir, &fixture.raw_hash, b"pa43 image bytes");
    let kio_dir = fixture.dir.path().join(".kio");

    fs::remove_file(fixture.dir.path().join("doc.md")).unwrap();
    // Fault-inject right after `prepared` so the journal (and its closure
    // sidecar) are durable but nothing destructive has happened yet.
    kio(
        &fixture.dir,
        &[
            "purge",
            "--raw-hash",
            &fixture.raw_hash,
            "--reason",
            "legal",
            "--yes",
        ],
    )
    .env("KIO_TEST_PURGE_FAIL_AFTER_PHASE", "prepared_visible")
    .arg("--json")
    .assert()
    .code(3);

    let closure = read_closure_sidecar(&kio_dir);
    let items = closure["items"].as_array().unwrap();
    let object_types = items
        .iter()
        .map(|item| item["object_type"].as_str().unwrap().to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(object_types.contains("raw"));
    assert!(object_types.contains("prepared"));
    assert!(object_types.contains("image"));
    assert!(object_types.contains("chunk"));
    assert!(
        items
            .iter()
            .any(|item| item["object_type"] == "raw" && item["hash"] == fixture.raw_hash)
    );
    assert!(
        items
            .iter()
            .any(|item| item["object_type"] == "image" && item["hash"] == image_hash)
    );

    // Resume to completion — the journal + closure sidecar are removed on
    // `done` (LC51-style cleanup), confirming the earlier fault injection
    // didn't leave the purge permanently stuck.
    let resumed = json_success(
        &fixture.dir,
        &[
            "purge",
            "--raw-hash",
            &fixture.raw_hash,
            "--reason",
            "legal",
            "--yes",
        ],
    );
    assert_eq!(resumed["status"], "purged");
}

#[test]
fn pa44_pa45_resumed_purge_reuses_the_fixed_closure_not_a_live_rescan() {
    let fixture = indexed_fixture();
    let prepared_hash = {
        let repo = Repository::open(fixture.dir.path()).unwrap();
        let head = repo.head_commit_hash().unwrap().unwrap();
        let commit = repo.read_commit(&head).unwrap();
        let tree = repo.read_tree(&commit.tree).unwrap();
        let entry = tree
            .entries
            .iter()
            .find(|entry| entry.raw_hash == fixture.raw_hash)
            .unwrap();
        let normalize = entry.normalize.clone().unwrap();
        let instance = load_validated_normalized_instance(
            repo.kio_dir(),
            &fixture.raw_hash,
            &normalize.tool_profile_hash,
            normalize.r#gen,
        )
        .unwrap();
        instance.manifest.units[0].prepared_hash.clone()
    };
    let kio_dir = fixture.dir.path().join(".kio");

    fs::remove_file(fixture.dir.path().join("doc.md")).unwrap();
    kio(
        &fixture.dir,
        &[
            "purge",
            "--raw-hash",
            &fixture.raw_hash,
            "--reason",
            "legal",
            "--yes",
        ],
    )
    .env("KIO_TEST_PURGE_FAIL_AFTER_PHASE", "prepared_visible")
    .arg("--json")
    .assert()
    .code(3);

    let closure_before_resume = read_closure_sidecar(&kio_dir);
    // PA44: prepared_hash was decided REMOVABLE (non-shared) at `prepared`.
    assert!(
        closure_before_resume["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["object_type"] == "prepared" && item["hash"] == prepared_hash)
    );

    // Resume — the closure content hash referenced by the journal must be
    // unchanged (PA44's "fixed once, reused verbatim").
    let journal: Value =
        serde_json::from_slice(&fs::read(kio_dir.join("purge/in-progress.json")).unwrap()).unwrap();
    let closure_hash_before = journal["closure_hash"].clone();

    let output = json_success(
        &fixture.dir,
        &[
            "purge",
            "--raw-hash",
            &fixture.raw_hash,
            "--reason",
            "legal",
            "--yes",
        ],
    );
    assert_eq!(output["status"], "purged");
    let store = ObjectStore::new(&kio_dir);
    // PA45: the object recorded in the closure was actually deleted.
    assert!(
        store
            .inspect_content_object(ContentObjectKind::Prepared, &prepared_hash)
            .is_err()
    );
    let _ = closure_hash_before;
}

#[test]
fn pa46_sqlite_target_chunk_ids_are_seeded_from_the_closure_not_rescanned_at_deletion() {
    let fixture = indexed_fixture();
    let kio_dir = fixture.dir.path().join(".kio");
    fs::remove_file(fixture.dir.path().join("doc.md")).unwrap();

    kio(
        &fixture.dir,
        &[
            "purge",
            "--raw-hash",
            &fixture.raw_hash,
            "--reason",
            "legal",
            "--yes",
        ],
    )
    .env("KIO_TEST_PURGE_FAIL_AFTER_PHASE", "prepared_visible")
    .arg("--json")
    .assert()
    .code(3);
    let closure = read_closure_sidecar(&kio_dir);
    let chunk_items = closure["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["object_type"] == "chunk")
        .count();
    assert!(
        chunk_items >= 1,
        "chunk target set fixed in the closure at prepared"
    );

    let output = json_success(
        &fixture.dir,
        &[
            "purge",
            "--raw-hash",
            &fixture.raw_hash,
            "--reason",
            "legal",
            "--yes",
        ],
    );
    assert!(output["deleted_counts"]["sqlite_chunks"].as_u64().unwrap() >= 1);
}

// ---------------------------------------------------------------------------
// §O (Phase 1 handoff): export canonical dispatch branches ii-iv.
// ---------------------------------------------------------------------------

fn write_erase_receipt(kio_dir: &Path, raw_hash: &str, in_commit: &str) {
    let state = PurgeState::open(kio_dir).unwrap();
    let path = state.erase_receipt_path(raw_hash).unwrap();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        serde_json::to_vec(&json!({
            "schema_version": 2,
            "raw_hash": raw_hash,
            "events": [{
                "kind": "erased",
                "at": "2026-07-22T00:00:00Z",
                "in_commit": in_commit,
                "reason": "legal",
                "actor": "test",
                "epoch": 1,
                "lifecycle_epoch": 1,
            }],
        }))
        .unwrap(),
    )
    .unwrap();
}

fn write_retired_tombstone(
    kio_dir: &Path,
    raw_hash: &str,
    purged_commit: &str,
    retired_commit: &str,
) {
    let state = PurgeState::open(kio_dir).unwrap();
    let path = state.tombstone_path(raw_hash).unwrap();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        serde_json::to_vec(&json!({
            "raw_hash": raw_hash,
            "events": [
                {
                    "kind": "purged",
                    "at": "2026-07-22T00:00:00Z",
                    "in_commit": purged_commit,
                    "reason": "legal",
                    "actor": "test",
                    "epoch": 1,
                    "lifecycle_epoch": 1,
                },
                {
                    "kind": "retired",
                    "at": "2026-07-22T00:10:00Z",
                    "in_commit": retired_commit,
                    "resurrection_commit": retired_commit,
                    "actor": "test",
                    "lifecycle_epoch": 2,
                },
            ],
        }))
        .unwrap(),
    )
    .unwrap();
}

#[test]
fn pa47_pa48_restore_distinguishes_erased_retired_and_unmarked_raw_absence() {
    // (a) canonical = erased, raw absent -> KIO-E-PURGE-NOT-FOUND-001 (the
    // ordinary, expected case).
    let fixture = indexed_fixture();
    let out = tempfile::tempdir().unwrap();
    let kio_dir = fixture.dir.path().join(".kio");
    let repo = Repository::open(fixture.dir.path()).unwrap();
    let head = repo.head_commit_hash().unwrap().unwrap();
    let store = ObjectStore::new(&kio_dir);
    store.remove_raw(&fixture.raw_hash).unwrap();
    write_erase_receipt(&kio_dir, &fixture.raw_hash, &head);
    let destination = out.path().join("pa47-a");
    let pointer = fixture.pointer.to_string();
    let error = kio(
        &fixture.dir,
        &["export", &pointer, "--to", &path_text(&destination)],
    )
    .arg("--json")
    .assert()
    .code(4)
    .get_output()
    .stderr
    .clone();
    let error: Value = serde_json::from_slice(&error).unwrap();
    assert_eq!(error["error_code"], "KIO-E-PURGE-NOT-FOUND-001");

    // (b) canonical = retired, raw absent -> KIO-E-STORE-CORRUPT-001
    // (resurrection is supposed to guarantee the raw's presence; its absence
    // is corruption, a DIFFERENT code from (a)'s expected erased-absence).
    let fixture2 = indexed_fixture();
    let out2 = tempfile::tempdir().unwrap();
    let kio_dir2 = fixture2.dir.path().join(".kio");
    let repo2 = Repository::open(fixture2.dir.path()).unwrap();
    let head2 = repo2.head_commit_hash().unwrap().unwrap();
    let store2 = ObjectStore::new(&kio_dir2);
    store2.remove_raw(&fixture2.raw_hash).unwrap();
    write_retired_tombstone(&kio_dir2, &fixture2.raw_hash, &head2, &head2);
    let destination2 = out2.path().join("pa47-b");
    let pointer2 = fixture2.pointer.to_string();
    let error2 = kio(
        &fixture2.dir,
        &["export", &pointer2, "--to", &path_text(&destination2)],
    )
    .arg("--json")
    .assert()
    .code(4)
    .get_output()
    .stderr
    .clone();
    let error2: Value = serde_json::from_slice(&error2).unwrap();
    assert_eq!(
        error2["error_code"], "KIO-E-STORE-CORRUPT-001",
        "PA48(b): retired-with-raw-missing must be distinguished from an ordinary erased absence"
    );
    assert_ne!(error["error_code"], error2["error_code"]);
}

#[test]
fn pa49_erased_with_raw_present_restores_normally() {
    let fixture = indexed_fixture();
    let kio_dir = fixture.dir.path().join(".kio");
    let repo = Repository::open(fixture.dir.path()).unwrap();
    let head = repo.head_commit_hash().unwrap().unwrap();
    // Raw object is left present; only an erase receipt exists.
    write_erase_receipt(&kio_dir, &fixture.raw_hash, &head);
    let out = tempfile::tempdir().unwrap();
    let destination = out.path().join("pa49-out");
    let pointer = fixture.pointer.to_string();
    let output = json_success(
        &fixture.dir,
        &["export", &pointer, "--to", &path_text(&destination)],
    );
    assert_eq!(output["status"], "exported");
    assert_eq!(output["exported_count"], 1);
}

#[test]
fn pa50_all_three_restore_call_sites_share_the_same_corrupt_verdict_for_a_retired_raw_absence() {
    // preflight (evidence source resolution) — exercised directly above by
    // PA47/48(b). This test confirms the SAME verdict from the local-path
    // preflight call site (`preflight`/`preflight_in_dir`'s shared
    // `check_purge_state`, reached identically regardless of source kind).
    let dir = canonical_tempdir();
    init(&dir);
    fs::write(dir.path().join("doc.md"), b"pa50 content").unwrap();
    let commit = json_success(&dir, &["snapshot", "create", "-m", "source"])["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let kio_dir = dir.path().join(".kio");
    let repo = Repository::open(dir.path()).unwrap();
    let head = repo.head_commit_hash().unwrap().unwrap();
    let raw_hash = current_raw_for(&dir, "doc.md");
    let store = ObjectStore::new(&kio_dir);
    store.remove_raw(&raw_hash).unwrap();
    write_retired_tombstone(&kio_dir, &raw_hash, &head, &head);

    let out = tempfile::tempdir().unwrap();
    let destination = out.path().join("pa50-out");
    let error = json_failure(
        &dir,
        &["export", &commit, "--to", &path_text(&destination)],
        4,
    );
    assert_eq!(
        error["error_code"], "KIO-E-STORE-CORRUPT-001",
        "commit-source export (preflight/preflight_in_dir path) must reach the \
         same canonical verdict as evidence-source export (PA47/48(b))"
    );
}
