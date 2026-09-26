#![cfg(unix)]

use std::{fs, thread};

use kio_core::scope::Repository;
use serde_json::{Value, json};

const TOOL_ID: &str = "mistral_ocr_markdownize";

fn pending(scope_id: &str) -> Value {
    json!({
        "scope_id": scope_id,
        "tool_id": TOOL_ID,
        "execution_mode": "online_api",
        "tool_profile_hash": format!("sha256:{}", "a".repeat(64)),
        "approved_at": "2026-09-08T00:00:00Z",
        "approval_method": "approve",
    })
}

fn active(scope_id: &str) -> Value {
    let mut row = pending(scope_id);
    row.as_object_mut()
        .unwrap()
        .insert("status".to_owned(), json!("active"));
    row
}

#[test]
fn retained_approval_operations_refuse_replaced_kio_without_writing_either_store() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("scope");
    let replacement_root = fixture.path().join("replacement");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&replacement_root).unwrap();
    let repo = Repository::init(&root).unwrap();
    Repository::init(&replacement_root).unwrap();
    let scope_id = repo.scope_identity().unwrap().scope_id;

    let retained = fixture.path().join("retained-kio");
    fs::rename(root.join(".kio"), &retained).unwrap();
    fs::rename(replacement_root.join(".kio"), root.join(".kio")).unwrap();
    let retained_before = fs::read(retained.join("scope.json")).unwrap();
    let replacement_before = fs::read(root.join(".kio/scope.json")).unwrap();

    let error = repo
        .write_network_approval_pending(pending(&scope_id))
        .unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-MANAGEMENT-AUTHORITY-001");
    assert_eq!(
        fs::read(retained.join("scope.json")).unwrap(),
        retained_before
    );
    assert_eq!(
        fs::read(root.join(".kio/scope.json")).unwrap(),
        replacement_before
    );
}

#[test]
fn retained_approval_operations_refuse_replaced_root_without_writing_either_store() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("scope");
    fs::create_dir(&root).unwrap();
    let repo = Repository::init(&root).unwrap();
    let scope_id = repo.scope_identity().unwrap().scope_id;

    let retained_root = fixture.path().join("retained-root");
    fs::rename(&root, &retained_root).unwrap();
    fs::create_dir(&root).unwrap();
    Repository::init(&root).unwrap();
    let retained_before = fs::read(retained_root.join(".kio/scope.json")).unwrap();
    let replacement_before = fs::read(root.join(".kio/scope.json")).unwrap();

    let error = repo
        .write_network_approval_pending(pending(&scope_id))
        .unwrap_err();
    // The controlled-root guard rejects the replaced root before store access.
    assert_eq!(
        error.error_code(),
        kio_core::management::CONTROLLED_ROOT_ERROR
    );
    assert_eq!(
        fs::read(retained_root.join(".kio/scope.json")).unwrap(),
        retained_before
    );
    assert_eq!(
        fs::read(root.join(".kio/scope.json")).unwrap(),
        replacement_before
    );
}

#[test]
fn retained_writes_block_concurrent_revoke_and_never_resurrect_stale_pending() {
    let fixture = tempfile::tempdir().unwrap();
    let repo = Repository::init(fixture.path()).unwrap();
    let scope_id = repo.scope_identity().unwrap().scope_id;
    let pending = pending(&scope_id);
    let row = active(&scope_id);
    repo.write_network_approval_pending(pending.clone())
        .unwrap();

    // The owned guard determines ordering: the other repository must fail
    // before it can enter revoke's read/modify/write section.
    let held = repo.lock_store().unwrap();
    let root = fixture.path().to_path_buf();
    let blocked = thread::spawn(move || {
        Repository::open(&root)
            .unwrap()
            .revoke_network_approval(Some(TOOL_ID), "2026-09-08T00:00:01Z")
    })
    .join()
    .unwrap()
    .unwrap_err();
    assert_eq!(blocked.error_code(), "KIO-E-STORE-LOCKED-001");
    assert_eq!(
        repo.read_network_approval_pending().unwrap(),
        Some(pending.clone())
    );
    drop(held);

    let revoked = repo
        .revoke_network_approval(Some(TOOL_ID), "2026-09-08T00:00:01Z")
        .unwrap();
    assert!(revoked.pending_removed);
    let error = repo
        .publish_network_approval(row, Some(&pending))
        .unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-ADAPTER-APPROVAL-CONFLICT-001");
    assert!(repo.read_network_approvals().unwrap().is_empty());
    assert_eq!(repo.read_network_approval_pending().unwrap(), None);
    assert!(
        !repo
            .network_approval_active(TOOL_ID, "online_api", &format!("sha256:{}", "a".repeat(64)),)
            .unwrap()
    );
}

#[test]
fn retained_config_compare_replace_preserves_document_and_refuses_stale_expected_bytes() {
    let fixture = tempfile::tempdir().unwrap();
    let repo = Repository::init(fixture.path()).unwrap();
    let initial = repo.read_config_document().unwrap();
    let replacement =
        "# application-owned comment\n[chunking]\nstrategy = \"heading\"\nmax_chars = 123\n";

    repo.compare_replace_config_document(&initial, replacement)
        .unwrap();
    assert_eq!(repo.read_config_document().unwrap(), replacement);

    let error = repo
        .compare_replace_config_document(&initial, "# stale replacement\n")
        .unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-CONFIG-CONFLICT-001");
    assert_eq!(repo.read_config_document().unwrap(), replacement);
}
