//! Step4b interrupted-approval contract tests. Persistent network-consent
//! checks are read-only: a pending approval left by an interrupted explicit
//! approval must fail closed until the operator runs the explicit approval
//! flow again. This prevents a search or index read from resurrecting consent
//! after a concurrent revoke.
//!
//! This file is deliberately SEPARATE from `step4b_p3a_contract.rs` (which
//! already owns QA21/22/25/26/27, the explicit-approval/revoke
//! side of this same 07 §3 area) — harness helpers below are self-contained
//! copies of that file's `kio`/`json_success`/`init`/`scope_json`/
//! `write_scope_allow_network_true`/`fake_pdf` (integration test binaries in
//! this crate do not share a `tests/common` module, so every file carries
//! its own copies; this mirrors the established convention rather than
//! introducing a new one).
//!
//! Each fixture first establishes the separate local scan grant with
//! `kio index --yes`. It then drives the external-consent gate through plain
//! `kio index`, which must leave portable consent state untouched. The positive
//! controls use the existing explicit `--approve` flow.

use std::fs;
use std::path::PathBuf;

use assert_cmd::Command;
use kio_adapter::catalog::standard_online_markdownize_profile_with_bbox;
use kio_core::scope::{publish_network_approval, write_network_approval_pending};
use serde_json::{Value, json};
use tempfile::TempDir;

const KIO_CHILD_ENV_DENYLIST: &[&str] = &[
    "GEMINI_API_KEY",
    "MISTRAL_API_KEY",
    "KIO_FIXED_NOW",
    "KIO_TEST_GEMINI_EMBED",
    "KIO_TEST_MISTRAL_OCR",
    "KIO_TEST_MISTRAL_BATCH",
    "KIO_TEST_MARKDOWNIZE_ADAPTER",
    "KIO_TEST_QUERY_EMBED_TRACE",
    "KIO_TEST_HOLD_LOCK_READY",
    "KIO_TEST_SCOPE_SEARCH_DELAY_SCOPE_ID",
    "KIO_TEST_SCOPE_SEARCH_DELAY_MS",
    "KIO_TEST_R13_2_AUTH",
    "KIO_TEST_R13_2_DECLARED",
    "KIO_TEST_R13_2_FALLBACK",
    "KIO_TEST_WINDOWS_PROFILE",
];

fn kio(dir: &TempDir, args: &[&str]) -> Command {
    let mut command = Command::cargo_bin("kio").unwrap();
    for name in KIO_CHILD_ENV_DENYLIST {
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

fn init(dir: &TempDir) {
    kio(dir, &["init"]).assert().success();
}

fn scope_json(dir: &TempDir) -> Value {
    serde_json::from_str(&fs::read_to_string(dir.path().join(".kio/scope.json")).unwrap()).unwrap()
}

fn kio_dir(dir: &TempDir) -> PathBuf {
    fs::canonicalize(dir.path().join(".kio")).unwrap()
}

/// The SCOPE-local `.kio/config.toml` — 07 §3's (b) path (mirrors
/// `step4b_p3a_contract.rs`'s helper of the same name/content exactly).
fn write_scope_allow_network_true(dir: &TempDir) {
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[adapter.policy]\nallow_network = true\n",
    )
    .unwrap();
}

fn fake_pdf(pages: &[&str]) -> String {
    let kids = (0..pages.len())
        .map(|index| format!("{} 0 R", index + 2))
        .collect::<Vec<_>>()
        .join(" ");
    let mut out = format!(
        "%PDF-1.4\n1 0 obj << /Type /Pages /Kids [{kids}] /Count {} >> endobj\n",
        pages.len()
    );
    for (index, page) in pages.iter().enumerate() {
        out.push_str(&format!(
            "{} 0 obj << /Type /Page /Parent 1 0 R >> stream\nBT ({page}) Tj ET\nendstream endobj\n",
            index + 2
        ));
    }
    out.push_str("%%EOF\n");
    out
}

/// The `(tool_id, tool_profile_hash)` a fresh scope's `kio index --yes`
/// computes for the standard online markdownize adapter — the same
/// `standard_online_markdownize_profile_with_bbox(bbox_annotation_enabled)`
/// call `online_markdownize_profile_for` makes internally, with
/// `bbox_annotation` at its frozen default `true` (no `.kio/config.toml`
/// `[markdownize]` override is written by any scenario below, so the
/// default holds).
fn standard_markdownize_identity() -> (String, String) {
    let profile = standard_online_markdownize_profile_with_bbox(true);
    (profile.adapter_id, profile.tool_profile_hash)
}

fn init_with_allow_network(dir: &TempDir) -> String {
    fs::write(dir.path().join("a.pdf"), fake_pdf(&["hello"])).unwrap();
    init(dir);
    // Establish the local scan grant before testing the external-consent read
    // gate. `--yes` records that separate grant, so it must not be part of
    // the immutable gate invocation below.
    json_success(dir, &["index", "--yes"]);
    write_scope_allow_network_true(dir);
    scope_json(dir)["scope_id"]
        .as_str()
        .expect("fresh scope.json must carry scope_id")
        .to_owned()
}

/// An interrupted explicit approval is not recoverable through a read gate.
/// A matching pending keeps the gate closed and leaves all portable consent
/// state untouched. An operator can then intentionally replace and publish it
/// through the explicit approval flow.
#[test]
fn selfheal_01_pending_fails_closed_until_explicit_approval() {
    let dir = tempfile::tempdir().unwrap();
    let scope_id = init_with_allow_network(&dir);
    let (tool_id, tool_profile_hash) = standard_markdownize_identity();

    let pending = json!({
        "scope_id": scope_id,
        "tool_id": tool_id,
        "execution_mode": "online_api",
        "tool_profile_hash": tool_profile_hash,
        // Deliberately far from "now" (today is 2026-07-22) so a re-stamping
        // regression is caught by simple inequality, not just presence.
        "approved_at": "2020-01-01T00:00:00Z",
        "approval_method": "approve",
    });
    write_network_approval_pending(&kio_dir(&dir), pending).unwrap();
    let scope_path = kio_dir(&dir).join("scope.json");
    let before_scope = fs::read(&scope_path).unwrap();
    let before_config = fs::read(kio_dir(&dir).join("config.toml")).unwrap();
    assert!(
        scope_json(&dir).get("approvals").is_none(),
        "no row must exist before the read-only gate"
    );

    let output = json_success(&dir, &["index"]);
    assert_eq!(
        output["network_opt_in"], false,
        "a pending approval must not be published by a read-only gate: {output}"
    );
    assert_eq!(fs::read(&scope_path).unwrap(), before_scope);
    assert_eq!(
        fs::read(kio_dir(&dir).join("config.toml")).unwrap(),
        before_config
    );

    json_success(&dir, &["index", "--yes"]);
    let revoked = json_success(&dir, &["adapter", "revoke", "mistral_ocr_markdownize"]);
    assert_eq!(revoked["status"], "revoked", "{revoked}");
    let approved = json_success(
        &dir,
        &["adapter", "approve", "mistral_ocr_markdownize", "--yes"],
    );
    assert_eq!(approved["status"], "approved", "{approved}");
    let explicit = json_success(&dir, &["index"]);
    assert_eq!(explicit["network_opt_in"], true, "{explicit}");
    let approvals = scope_json(&dir)["approvals"].as_array().unwrap().clone();
    assert_eq!(approvals.len(), 1, "{approvals:?}");
    assert_eq!(approvals[0]["scope_id"], json!(scope_id));
    assert_eq!(approvals[0]["tool_id"], json!(tool_id));
    assert_eq!(approvals[0]["tool_profile_hash"], json!(tool_profile_hash));
    assert_eq!(approvals[0]["approval_method"], "approve");
    assert_eq!(approvals[0]["status"], "active");
    assert!(
        scope_json(&dir).get("approval_pending").is_none(),
        "the explicit approval must consume its own pending record"
    );
}

/// A mismatched pending also remains opaque to a read-only gate.
#[test]
fn selfheal_02_mismatched_profile_pending_is_left_untouched_and_gate_stays_closed() {
    let dir = tempfile::tempdir().unwrap();
    let scope_id = init_with_allow_network(&dir);
    let (tool_id, _real_hash) = standard_markdownize_identity();
    let stale_hash = format!("sha256:{}", "0".repeat(64));

    let pending = json!({
        "scope_id": scope_id,
        "tool_id": tool_id,
        "execution_mode": "online_api",
        "tool_profile_hash": stale_hash,
        "approved_at": "2020-01-01T00:00:00Z",
        "approval_method": "approve",
    });
    write_network_approval_pending(&kio_dir(&dir), pending.clone()).unwrap();

    let output = json_success(&dir, &["index"]);
    assert_eq!(
        output["network_opt_in"], false,
        "a profile-mismatched pending must not open the gate: {output}"
    );

    let scope = scope_json(&dir);
    assert!(
        scope.get("approvals").is_none(),
        "no row must be published by the read-only gate: {scope}"
    );
    assert!(
        scope.get("approvals_initialized").is_none(),
        "the marker must stay unset: {scope}"
    );
    assert_eq!(
        scope["approval_pending"], pending,
        "the stale pending must be left byte-for-byte untouched: {scope}"
    );
}

/// A present pending missing a required audit field is a current scope-schema
/// violation. It must fail closed without being cleaned up or inferred.
#[test]
fn selfheal_03_malformed_pending_fails_closed_without_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let scope_id = init_with_allow_network(&dir);
    let (tool_id, tool_profile_hash) = standard_markdownize_identity();

    let malformed_pending = json!({
        "scope_id": scope_id,
        "tool_id": tool_id,
        "execution_mode": "online_api",
        "tool_profile_hash": tool_profile_hash,
        "approved_at": "2020-01-01T00:00:00Z",
        // approval_method intentionally absent.
    });
    let scope_path = kio_dir(&dir).join("scope.json");
    let mut scope = scope_json(&dir);
    scope["approval_pending"] = malformed_pending;
    fs::write(&scope_path, serde_json::to_vec_pretty(&scope).unwrap()).unwrap();
    let before = fs::read(&scope_path).unwrap();

    let stderr = kio(&dir, &["index", "--json"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let error: Value = serde_json::from_slice(&stderr).unwrap();
    assert_eq!(error["error_code"], "KIO-E-CONFIG-SCHEMA-001");
    assert_eq!(fs::read(&scope_path).unwrap(), before);
}

/// An interrupted approval for a second tool must stay pending even if another
/// tool already has an active row. A later explicit approval can publish the
/// second row without changing the first.
///
/// The fixture models a SECOND tool's crash mid-flight —
/// `approvals_initialized` is already `true` and an unrelated tool_id
/// already carries an `active` row (a previously, fully-completed
/// approval), while a well-formed pending for THIS run's tool (the standard
/// markdownize identity) sits unpublished.
#[test]
fn selfheal_04_second_tool_pending_requires_explicit_approval() {
    let dir = tempfile::tempdir().unwrap();
    let scope_id = init_with_allow_network(&dir);
    let (tool_id_b, tool_profile_hash_b) = standard_markdownize_identity();
    let tool_id_a = "kio_selfheal_test_tool_a";
    let tool_profile_hash_a = format!("sha256:{}", "1".repeat(64));

    let row_a = json!({
        "scope_id": scope_id,
        "tool_id": tool_id_a,
        "execution_mode": "online_api",
        "tool_profile_hash": tool_profile_hash_a,
        "approved_at": "2019-01-01T00:00:00Z",
        "approval_method": "approve",
        "status": "active",
    });
    publish_network_approval(&kio_dir(&dir), row_a.clone(), None).unwrap();
    assert_eq!(scope_json(&dir)["approvals_initialized"], true);

    let pending_b = json!({
        "scope_id": scope_id,
        "tool_id": tool_id_b,
        "execution_mode": "online_api",
        "tool_profile_hash": tool_profile_hash_b,
        "approved_at": "2021-02-02T00:00:00Z",
        "approval_method": "approve",
    });
    write_network_approval_pending(&kio_dir(&dir), pending_b).unwrap();

    let before_scope = fs::read(kio_dir(&dir).join("scope.json")).unwrap();
    let output = json_success(&dir, &["index"]);
    assert_eq!(
        output["network_opt_in"], false,
        "tool A's approval must not open tool B's pending approval: {output}"
    );
    assert_eq!(
        fs::read(kio_dir(&dir).join("scope.json")).unwrap(),
        before_scope
    );

    json_success(&dir, &["index", "--yes"]);
    let revoked = json_success(&dir, &["adapter", "revoke", "mistral_ocr_markdownize"]);
    assert_eq!(revoked["status"], "revoked", "{revoked}");
    let approved = json_success(
        &dir,
        &["adapter", "approve", "mistral_ocr_markdownize", "--yes"],
    );
    assert_eq!(approved["status"], "approved", "{approved}");
    let explicit = json_success(&dir, &["index"]);
    assert_eq!(explicit["network_opt_in"], true, "{explicit}");

    let scope = scope_json(&dir);
    let approvals = scope["approvals"].as_array().unwrap();
    assert_eq!(
        approvals.len(),
        2,
        "tool A's row plus the explicitly approved tool B row: {approvals:?}"
    );
    let row_b_after = approvals
        .iter()
        .find(|row| row["tool_id"] == json!(tool_id_b))
        .expect("tool B's newly approved row must exist");
    assert_ne!(row_b_after["approved_at"], "2021-02-02T00:00:00Z");
    assert!(
        row_b_after["approved_at"]
            .as_str()
            .is_some_and(|at| at.ends_with('Z'))
    );
    assert_eq!(row_b_after["approval_method"], "approve");
    assert_eq!(row_b_after["status"], "active");
    assert_eq!(row_b_after["tool_profile_hash"], json!(tool_profile_hash_b));

    let row_a_after = approvals
        .iter()
        .find(|row| row["tool_id"] == json!(tool_id_a))
        .expect("tool A's row must still exist");
    assert_eq!(
        row_a_after, &row_a,
        "tool A's row must be completely unmodified by tool B's explicit approval: {row_a_after}"
    );
    assert!(
        scope.get("approval_pending").is_none(),
        "tool B's pending must be removed: {scope}"
    );
    assert_eq!(scope["approvals_initialized"], true);
}
