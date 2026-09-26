//! The 2026-08-02 ruling: the Tier A secrets hold applies to an `offline_api`
//! markdownize adapter too, and the consent it records must name the adapter
//! that will actually see the file.
//!
//! 07 §3 (2) exempts `offline_api` from `approvals[]` and `allow_network`
//! because those gate *transmission off the machine*. The secrets hold asks a
//! different question — may this credential be handed to this tool — and a
//! local model server is a separate process that can log what it is given.
//!
//! The failure this pins down is quiet rather than loud. Before the split, the
//! `--send-secrets` consent was keyed off the *online* markdownize id
//! unconditionally, so approving a local pipeline wrote an audit row naming
//! Mistral: a durable record asserting the user consented to send a credential
//! to a cloud API that never received it.

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

const CHILD_ENV_DENYLIST: &[&str] = &[
    "GEMINI_API_KEY",
    "MISTRAL_API_KEY",
    "KIO_FIXED_NOW",
    "KIO_TEST_GEMINI_EMBED",
    "KIO_EVAL_DETERMINISTIC_EMBED",
    "KIO_TEST_LOCAL_OCR",
    "KIO_TEST_MISTRAL_OCR",
    "KIO_TEST_MARKDOWNIZE_ADAPTER",
];

const LOCAL_PEER_CA_PEM: &str = "-----BEGIN CERTIFICATE-----\nMIIBWTCB/6ADAgECAhR5P0J0YMFaZPlrOFyN8/jwpGzhqjAKBggqhkjOPQQDAjAh\nMR8wHQYDVQQDDBZyY2dlbiBzZWxmIHNpZ25lZCBjZXJ0MCAXDTc1MDEwMTAwMDAw\nMFoYDzQwOTYwMTAxMDAwMDAwWjAhMR8wHQYDVQQDDBZyY2dlbiBzZWxmIHNpZ25l\nZCBjZXJ0MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEGhIwEPQnvWSlH+iUQ5Ui\nq0khmBj4hlxmW+mTL5xjjyxwwopOnkxLs2AofasS2lYPdILzMaIeRE78g2S7Hz1n\nX6MTMBEwDwYDVR0RBAgwBocEfwAAATAKBggqhkjOPQQDAgNJADBGAiEA0GBG4uEL\nSJJea18R17+yhRTyn3rsD6PzDhdg7F/v2sQCIQCccgcgjDh+ABNajeb1deKZoqbx\nnP4qBrGe09azOI4jbg==\n-----END CERTIFICATE-----\n";

fn kio(dir: &TempDir, args: &[&str], env: &[(&str, &str)]) -> Command {
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
    for (name, value) in env {
        command.env(name, value);
    }
    command
}

fn run(dir: &TempDir, args: &[&str], env: &[(&str, &str)]) -> (bool, String) {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    let output = kio(dir, &full, env).output().unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

/// Every current device-private `send_secrets` grant this run wrote, as tool
/// ids. The old append-only `consents.jsonl` ledger is no longer the authority.
fn secrets_consent_tool_ids(dir: &TempDir) -> Vec<String> {
    let path = dir
        .path()
        .join(".test-data")
        .join("kio")
        .join("grants")
        .join("grants.json");
    if !path.exists() {
        return Vec::new();
    }
    serde_json::from_slice::<Value>(&fs::read(path).unwrap()).unwrap()["grants"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| row["binding"]["operation"].as_str() == Some("send_secrets"))
        .filter_map(|row| {
            row["binding"]
                .get("tool_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect()
}

fn configure_local_ocr(dir: &TempDir, env: &[(&str, &str)]) {
    #[cfg(unix)]
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let config = dir.path().join(".test-config/kio/tools.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        config,
        "[markdown.paddleocr_vl_local]\nkind = \"offline_api\"\nurl = \"https://127.0.0.1:8443\"\nmodel = \"PaddleOCR-VL-0.9B\"\n",
    )
    .unwrap();
    let ca_path = dir.path().join("local-peer-ca.pem");
    fs::write(&ca_path, LOCAL_PEER_CA_PEM).unwrap();
    #[cfg(unix)]
    fs::set_permissions(&ca_path, fs::Permissions::from_mode(0o600)).unwrap();
    let ca_path = ca_path.to_str().unwrap();
    let (ok, out) = run(
        dir,
        &["adapter", "trust", "register", "--ca-pem", ca_path, "--yes"],
        env,
    );
    assert!(ok, "{out}");
}

fn fixture() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("credentials_scan.pdf"),
        "%PDF-1.4\nscanned page\n",
    )
    .unwrap();
    dir
}

fn markdown_task(dir: &TempDir) -> Value {
    fs::read_to_string(dir.path().join(".kio/tasks.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|task| task["type"].as_str() == Some("markdownize"))
        .expect("secret OCR fixture must create a markdownize task")
}

/// With a local OCR pipeline declared, `--send-secrets` must record consent
/// against the local adapter — not against the cloud OCR that is not running.
#[test]
fn the_secrets_consent_names_the_adapter_that_will_see_the_file() {
    let dir = fixture();
    // The configured embedding adapter has no send-secrets grant. Its missing
    // permission must not hold an OCR send that is authorized for the actual
    // local OCR peer.
    let local = [
        ("KIO_TEST_LOCAL_OCR", "mock"),
        ("KIO_TEST_GEMINI_EMBED", "mock"),
    ];
    configure_local_ocr(&dir, &local);
    let (ok, out) = run(&dir, &["init"], &local);
    assert!(ok, "{out}");
    let (ok, out) = run(
        &dir,
        &[
            "adapter",
            "approve",
            "paddleocr_vl_local",
            "--yes",
            "--send-secrets",
        ],
        &local,
    );
    assert!(ok, "{out}");
    let (ok, out) = run(&dir, &["index"], &local);
    assert!(ok, "{out}");
    let task = markdown_task(&dir);
    assert_eq!(task["status"], "done", "{task}");
    assert_eq!(task["fallback_reason"], "local_adapter_done", "{task}");

    let recorded = secrets_consent_tool_ids(&dir);
    assert!(
        !recorded.is_empty(),
        "--send-secrets must record a consent row: {recorded:?}"
    );
    assert!(
        recorded.iter().all(|id| id != "mistral_ocr_markdownize"),
        "a local pipeline must not have its secrets consent recorded against \
         the cloud OCR adapter: {recorded:?}"
    );
    assert!(
        recorded.iter().any(|id| id == "paddleocr_vl_local"),
        "the local pipeline must be named in the consent it needs: {recorded:?}"
    );
}

#[test]
fn mistral_secret_consent_does_not_authorize_the_local_ocr_peer() {
    let dir = fixture();
    let local = [("KIO_TEST_LOCAL_OCR", "mock")];
    // Establish an otherwise valid Mistral grant before switching the active
    // OCR target. The old all-configured predicate treated it as sufficient.
    let (ok, out) = run(&dir, &["init"], &[]);
    assert!(ok, "{out}");
    let (ok, out) = run(
        &dir,
        &[
            "adapter",
            "approve",
            "mistral_ocr_markdownize",
            "--yes",
            "--send-secrets",
        ],
        &[],
    );
    assert!(ok, "{out}");
    configure_local_ocr(&dir, &local);
    let (ok, out) = run(&dir, &["init"], &local);
    assert!(ok, "{out}");

    let (ok, out) = run(&dir, &["index"], &local);
    assert!(ok, "{out}");
    let task = markdown_task(&dir);
    assert_eq!(task["status"], "paused", "{task}");
    assert_eq!(task["fallback_reason"], "secrets_tier_b_hold", "{task}");
}

/// Without a local pipeline, nothing changes: the consent still names the
/// online OCR adapter, exactly as it did before the split.
#[test]
fn the_online_route_records_its_consent_unchanged() {
    let dir = fixture();
    let (ok, out) = run(&dir, &["init"], &[]);
    assert!(ok, "{out}");
    let (ok, out) = run(
        &dir,
        &[
            "adapter",
            "approve",
            "mistral_ocr_markdownize",
            "--yes",
            "--send-secrets",
        ],
        &[],
    );
    assert!(ok, "{out}");
    let (ok, out) = run(&dir, &["index", "--offline"], &[]);
    assert!(ok, "{out}");

    let recorded = secrets_consent_tool_ids(&dir);
    assert!(
        recorded.iter().any(|id| id == "mistral_ocr_markdownize"),
        "the pre-existing online consent must be untouched: {recorded:?}"
    );
    assert!(
        recorded.iter().all(|id| id != "paddleocr_vl_local"),
        "an undeclared local pipeline must not appear in any consent: {recorded:?}"
    );
}
