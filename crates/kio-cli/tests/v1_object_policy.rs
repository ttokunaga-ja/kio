//! Current-policy authorization for image object URIs.

mod support;

use support::canonical_tempdir;

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use base64::Engine as _;
use kio_core::cas::ContentObjectKind;
use kio_core::scope::Repository;
use serde_json::Value;
use tempfile::TempDir;

fn command(dir: &TempDir, args: &[&str]) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin("kio"));
    command
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env("XDG_CACHE_HOME", dir.path().join("cache"))
        .arg("--json")
        .args(args);
    command
}

fn ok(dir: &TempDir, args: &[&str]) -> Value {
    let output = command(dir, args).output().unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn deterministic_ok(dir: &TempDir, args: &[&str]) -> Value {
    let output = command(dir, args)
        .env("KIO_EVAL_DETERMINISTIC_EMBED", "scale-v3")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn png() -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGP4DwQACfsD/fteaysAAAAASUVORK5CYII=")
        .unwrap()
}

fn indexed_image_uri(dir: &TempDir) -> String {
    ok(dir, &["init"]);
    fs::write(dir.path().join("scan.png"), png()).unwrap();
    ok(dir, &["index", "--offline"]);
    ok(dir, &["search", "scan", "--mode", "text"])["results"]
        .as_array()
        .unwrap()[0]["related_images"]
        .as_array()
        .unwrap()[0]["image_uri"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn image_object_open_requires_a_currently_eligible_authenticated_owner() {
    let dir = canonical_tempdir();
    let uri = indexed_image_uri(&dir);
    ok(&dir, &["open", &uri]);

    fs::write(dir.path().join(".kioignore"), "scan.png\n").unwrap();
    let denied = command(&dir, &["open", &uri]).output().unwrap();
    assert!(!denied.status.success());
    let report: Value = serde_json::from_slice(&denied.stderr).unwrap();
    assert_eq!(report["error_code"], "KIO-E-OBJECT-POLICY-001", "{report}");
}

#[test]
fn copied_image_cas_without_an_authenticated_source_is_not_openable() {
    let source = canonical_tempdir();
    let uri = indexed_image_uri(&source);
    let hash = uri.rsplit('/').next().unwrap();
    let bytes = png();

    let target = canonical_tempdir();
    ok(&target, &["init"]);
    let target_repo = Repository::open(target.path()).unwrap();
    assert_eq!(
        target_repo
            .object_store()
            .write_content_object(ContentObjectKind::Image, &bytes)
            .unwrap(),
        hash
    );
    let foreign_uri = format!(
        "kio://{}/object/image/{hash}",
        target_repo.scope_identity().unwrap().scope_id
    );
    let denied = command(&target, &["open", &foreign_uri]).output().unwrap();
    assert!(!denied.status.success());
    let report: Value = serde_json::from_slice(&denied.stderr).unwrap();
    assert_eq!(report["error_code"], "KIO-E-OBJECT-POLICY-001", "{report}");
}

#[test]
fn same_scope_markdown_link_cannot_reauthorize_an_ignored_image_owner() {
    let dir = canonical_tempdir();
    let uri = indexed_image_uri(&dir);

    // `public.md` keeps a syntactically valid URI in the same scope, but its
    // deterministic Markdown unit never typed-owns the image bytes. The only
    // real owner is now ignored, so a fresh indexed HEAD must deny disclosure.
    fs::write(dir.path().join(".kioignore"), "scan.png\n").unwrap();
    fs::write(
        dir.path().join("public.md"),
        format!("public citation only: ![copied]({uri})\n"),
    )
    .unwrap();
    ok(&dir, &["index", "--offline"]);

    let denied = command(&dir, &["open", &uri]).output().unwrap();
    assert!(!denied.status.success());
    let report: Value = serde_json::from_slice(&denied.stderr).unwrap();
    assert_eq!(report["error_code"], "KIO-E-OBJECT-POLICY-001", "{report}");
}

#[test]
fn stale_replica_image_candidate_is_hidden_after_owner_ignore_without_reindex() {
    let dir = canonical_tempdir();
    ok(&dir, &["init"]);
    let repo = Repository::open(dir.path()).unwrap();
    let bytes = png();
    let hash = kio_pipeline::prepare::hash_bytes(&bytes);
    let uri = format!(
        "kio://{}/object/image/{hash}",
        repo.scope_identity().unwrap().scope_id
    );
    fs::write(dir.path().join("scan.png"), bytes).unwrap();
    // This public citation becomes an image association while scan.png is an
    // allowed typed owner. It must not keep that association searchable once
    // the owner is ignored, even though no writer refresh occurs afterwards.
    fs::write(
        dir.path().join("public.md"),
        format!("public citation witness ![copied]({uri})\n"),
    )
    .unwrap();
    deterministic_ok(&dir, &["index", "--offline"]);
    let before = deterministic_ok(
        &dir,
        &["search", "public citation witness", "--mode", "hybrid"],
    );
    assert!(
        before["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["result_type"] == "image"),
        "fixture must first create an image replica association: {before}"
    );

    fs::write(dir.path().join(".kioignore"), "scan.png\n").unwrap();
    let after = deterministic_ok(
        &dir,
        &["search", "public citation witness", "--mode", "hybrid"],
    );
    assert!(
        after["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["result_type"] != "image"),
        "stale replica image row bypassed current typed ownership: {after}"
    );
    assert!(
        after["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["result_type"] == "chunk"),
        "the public citing chunk remains searchable: {after}"
    );
}

fn evidence_pointer(dir: &TempDir, query: &str) -> Value {
    ok(dir, &["search", query, "--mode", "text"])["results"]
        .as_array()
        .unwrap()[0]["evidence_pointer"]
        .clone()
}

#[test]
fn evidence_and_chunk_short_hash_obey_current_ignore_without_hiding_allowed_file() {
    let dir = canonical_tempdir();
    ok(&dir, &["init"]);
    fs::write(dir.path().join("denied.md"), "denied witness\n").unwrap();
    fs::write(dir.path().join("allowed.md"), "allowed witness\n").unwrap();
    ok(&dir, &["index", "--offline"]);
    let denied_pointer = evidence_pointer(&dir, "denied witness");
    let allowed_pointer = evidence_pointer(&dir, "allowed witness");
    let denied_text = denied_pointer.to_string();
    let denied_chunk = denied_pointer["chunk_hash"].as_str().unwrap();
    ok(&dir, &["open", &denied_text]);
    ok(&dir, &["open", denied_chunk]);

    fs::write(dir.path().join(".kioignore"), "denied.md\n").unwrap();
    for operand in [&denied_text, denied_chunk] {
        let output = command(&dir, &["open", operand]).output().unwrap();
        assert!(!output.status.success(), "ignored {operand} opened");
    }
    ok(&dir, &["open", &allowed_pointer.to_string()]);
}

#[test]
fn pathless_evidence_uri_selects_an_allowed_exact_generation_alias() {
    let dir = canonical_tempdir();
    ok(&dir, &["init"]);
    fs::write(dir.path().join("a-denied.md"), "identical alias witness\n").unwrap();
    fs::write(dir.path().join("b-allowed.md"), "identical alias witness\n").unwrap();
    ok(&dir, &["index", "--offline"]);
    // Search deduplicates equal raw bytes, so it legitimately emits the
    // byte-order-first a-denied alias before the policy changes. An Evidence
    // URI omits that path; after Ignore, resolving it must select the retained
    // b-allowed alias with the exact same profile/gen attribution.
    let result = ok(
        &dir,
        &["search", "identical alias witness", "--mode", "text"],
    );
    let row = result["results"].as_array().unwrap()[0].clone();
    let uri = row["evidence_uri"].as_str().unwrap();
    fs::write(dir.path().join(".kioignore"), "a-denied.md\n").unwrap();
    let opened = ok(&dir, &["open", uri]);
    assert_ne!(
        PathBuf::from(opened["path"].as_str().unwrap()),
        dir.path().join("a-denied.md"),
        "the authorized b-allowed alias must not materialize the ignored raw twin: {opened}",
    );

    let mut explicit = row["evidence_pointer"].clone();
    explicit["path_at_commit"] = Value::String("a-denied.md".to_owned());
    let denied = command(&dir, &["open", &explicit.to_string()])
        .output()
        .unwrap();
    assert!(!denied.status.success());
}

#[test]
fn ancestor_ignore_denies_a_childs_direct_evidence_uri() {
    let root = canonical_tempdir();
    ok(&root, &["init"]);
    fs::create_dir(root.path().join("child")).unwrap();
    fs::write(root.path().join("child/child.md"), "child-only witness\n").unwrap();
    // Root indexing establishes the managed child boundary and its retained
    // parent authority before the child emits an Evidence Pointer.
    ok(&root, &["index", "--offline"]);
    let mut child_command = command(&root, &["search", "child-only witness", "--mode", "text"]);
    child_command.current_dir(root.path().join("child"));
    let output = child_command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let pointer: Value = serde_json::from_slice(&output.stdout).unwrap();
    let uri = pointer["results"].as_array().unwrap()[0]["evidence_uri"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::write(root.path().join(".kioignore"), "child/\n").unwrap();
    let denied = command(&root, &["open", &uri]).output().unwrap();
    assert!(!denied.status.success());
}
