//! Current-policy egress regressions for retained and historical embeddings.

use assert_cmd::Command;
use tempfile::TempDir;

fn kio(dir: &TempDir) -> Command {
    let mut command = Command::cargo_bin("kio").unwrap();
    command.env_clear();
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    command
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("USERPROFILE", dir.path())
        .env("XDG_DATA_HOME", dir.path().join(".test-data"))
        .env("XDG_CONFIG_HOME", dir.path().join(".test-config"))
        .env("XDG_CACHE_HOME", dir.path().join(".test-cache"))
        .env("KIO_TEST_MARKDOWNIZE_ADAPTER", "deterministic")
        .env("KIO_TEST_GEMINI_EMBED", "mock");
    command
}

fn embedding_batch_rows(dir: &TempDir) -> i64 {
    let db = dir.path().join(".test-data/kio/cost-ledger.sqlite");
    if !db.exists() {
        return 0;
    }
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM batch_requests WHERE adapter_kind = 'embedding'",
        [],
        |row| row.get(0),
    )
    .unwrap()
}

/// A historical retained unit may no longer have a source file, but its name
/// remains subject to the live Ignore authority before any batch reservation.
#[test]
fn ignored_historical_owner_never_submits_or_reserves_a_new_embedding_job() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("history.md"),
        "# Historical\n\nretained body\n",
    )
    .unwrap();
    kio(&dir).arg("init").assert().success();
    kio(&dir).args(["ledger", "init"]).assert().success();

    // Keep the first pass offline, so this exact historical chunk is still
    // pending. The explicit embedding grant is made only before the online
    // reindex whose live Ignore admission this test exercises.
    kio(&dir).args(["index", "--offline"]).assert().success();
    let reservations_before = embedding_batch_rows(&dir);
    assert_eq!(reservations_before, 0);
    let head = std::fs::read_to_string(dir.path().join(".kio/HEAD"))
        .unwrap()
        .trim()
        .to_owned();
    std::fs::write(dir.path().join(".kioignore"), "history.md\n").unwrap();
    std::fs::remove_file(dir.path().join("history.md")).unwrap();
    kio(&dir)
        .args(["adapter", "approve", "gemini_embedding_2", "--yes"])
        .assert()
        .success();

    let capture = dir.path().join("embedding-batch-capture.jsonl");
    let script = serde_json::json!({
        "state_sequence": ["BATCH_STATE_PENDING"],
        "job_name": "batches/policy-denied",
        "capture_path": capture.to_string_lossy(),
    })
    .to_string();
    kio(&dir)
        .env("KIO_TEST_GEMINI_BATCH", script)
        .args(["reindex", "--at", &head, "--online"])
        .assert()
        .success();

    assert!(
        !capture.exists(),
        "current Ignore must prevent historical provider admission"
    );
    assert_eq!(
        embedding_batch_rows(&dir),
        reservations_before,
        "current Ignore must run before any new embedding ledger reservation"
    );
}
