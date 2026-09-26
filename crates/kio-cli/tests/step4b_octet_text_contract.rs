//! Sniffed octet-stream TEXT contract (R20-6 passthrough × 04 §3.2 V5).
//!
//! 2026-07-23 fixture-registration regression: `.xml` / `.html` (and any
//! other markup-bearing sniffed-text file) failed their offline markdownize
//! at `kio index` because the raw passthrough violated Normalized Markdown
//! v1's "raw HTML and autolinks are forbidden" acceptance check — an opaque
//! `contract_violation` task and a permanently exit-3 scope. The fix fences
//! every non-markdown text source (deterministic markdownize profile
//! 1.2.0). These tests pin the end-to-end behavior for exactly the two
//! corpus kinds that failed.

use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use tempfile::TempDir;

const KIO_CHILD_ENV_DENYLIST: &[&str] = &[
    "GEMINI_API_KEY",
    "MISTRAL_API_KEY",
    "KIO_FIXED_NOW",
    "KIO_TEST_GEMINI_EMBED",
    "KIO_TEST_MISTRAL_OCR",
    "KIO_TEST_MISTRAL_BATCH",
    "KIO_TEST_MARKDOWNIZE_ADAPTER",
    "KIO_TEST_OFFICE_CONVERT",
    "KIO_OFFICE_CONVERTER",
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

fn assert_search_hit(dir: &TempDir, needle: &str, title: &str) {
    let search = json_success(dir, &["search", needle, "--mode", "text"]);
    let results = search["results"].as_array().unwrap();
    assert!(
        results.iter().any(|result| result["title"] == title),
        "expected `{needle}` to hit {title}: {search}"
    );
}

#[test]
fn octet_xml_and_html_index_offline_and_search() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("record-037.xml"),
        "<record><title>quartzledger evidence</title><count>3600</count></record>",
    )
    .unwrap();
    fs::write(
        dir.path().join("digest.html"),
        "<!doctype html><html><body><p>heliotrope summary body</p></body></html>",
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    // The whole point: index must SUCCEED (no KIO-E-INDEX-PARTIAL-001, no
    // contract_violation task) for markup-bearing sniffed text.
    let index = json_success(&dir, &["index", "--yes"]);
    json_success(&dir, &["adapter", "approve", "--all", "--yes"]);
    assert_eq!(index["failed_files"], 0, "{index}");

    assert_search_hit(&dir, "quartzledger", "record-037.xml");
    assert_search_hit(&dir, "heliotrope", "digest.html");

    let status = json_success(&dir, &["status"]);
    let failed: Vec<&Value> = status["tasks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|task| task["status"] == "failed")
        .collect();
    assert!(failed.is_empty(), "no failed tasks expected: {status}");
}
