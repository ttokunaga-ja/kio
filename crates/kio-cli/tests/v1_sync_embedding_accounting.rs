use std::fs;

use assert_cmd::Command;
use rusqlite::Connection;
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
}

impl Fixture {
    fn scope(&self) -> std::path::PathBuf {
        self.root.path().join("scope")
    }

    fn home(&self) -> std::path::PathBuf {
        self.root.path().join("home")
    }
}

fn fixture_dir() -> Fixture {
    #[cfg(target_os = "macos")]
    let root = tempfile::tempdir_in("/private/tmp").unwrap();
    #[cfg(not(target_os = "macos"))]
    let root = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let scope = root.path().join("scope");
    let home = root.path().join("home");
    fs::create_dir_all(&scope).unwrap();
    for path in [
        &home,
        &home.join("data"),
        &home.join("config"),
        &home.join("cache"),
        &home.join("tmp"),
    ] {
        fs::create_dir_all(path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
    Fixture { root }
}

fn kio(dir: &Fixture, seam: &str, now: &str) -> Command {
    let mut command = Command::cargo_bin("kio").unwrap();
    command.env_clear();
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    let home = dir.home();
    command
        .current_dir(dir.scope())
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("TMPDIR", home.join("tmp"))
        .env("TEMP", home.join("tmp"))
        .env("TMP", home.join("tmp"))
        .env("KIO_FIXED_NOW", now)
        .env("KIO_TEST_MARKDOWNIZE_ADAPTER", "deterministic")
        .env("KIO_TEST_GEMINI_EMBED", seam)
        .env_remove("GEMINI_API_KEY")
        .env_remove("MISTRAL_API_KEY");
    command
}

fn run_json_success(dir: &Fixture, seam: &str, now: &str, args: &[&str]) -> serde_json::Value {
    let output = kio(dir, seam, now)
        .args(args)
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "kio {:?} failed with status {:?}: {}",
        args,
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "kio {:?} did not return JSON: {error}; stdout={} stderr={}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn embedding_rows(dir: &Fixture) -> Vec<(i64, String, f64, Option<String>)> {
    let conn = Connection::open(dir.home().join("data/kio/cost-ledger.sqlite")).unwrap();
    conn.prepare(
        "SELECT state, COALESCE(error, ''), estimated_usd, intent_token
         FROM batch_requests WHERE adapter_kind = 'embedding' ORDER BY submission_seq",
    )
    .unwrap()
    .query_map([], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

fn source_embedding_count(dir: &Fixture) -> i64 {
    let conn = Connection::open(dir.scope().join(".kio/index/sqlite.db")).unwrap();
    // `chunk_vec` is a sqlite-vec virtual table; opening the source database
    // through plain `rusqlite` does not load that extension. `embeddings` is
    // the persisted source-of-truth and proves the same provider effect.
    conn.query_row("SELECT COUNT(*) FROM embeddings", [], |row| row.get(0))
        .unwrap()
}

fn embedding_ledger_outcomes(dir: &Fixture) -> Vec<(String, f64)> {
    let conn = Connection::open(dir.home().join("data/kio/cost-ledger.sqlite")).unwrap();
    conn.prepare(
        "SELECT outcome, usd FROM cost_ledger
         WHERE adapter_kind = 'embedding' ORDER BY submission_seq",
    )
    .unwrap()
    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

#[test]
fn sync_embedding_known_rejection_reserves_only_the_attempted_group_and_retries_fresh() {
    let dir = fixture_dir();
    fs::write(dir.scope().join("one.md"), "# One\n\nunique first group\n").unwrap();
    fs::write(dir.scope().join("two.md"), "# Two\n\nunique later group\n").unwrap();
    let first = "2026-09-08T00:00:00Z";
    kio(&dir, "mock", first).args(["init"]).assert().success();
    kio(&dir, "mock", first)
        .args(["ledger", "init"])
        .assert()
        .success();
    kio(&dir, "mock", first)
        .args(["index", "--offline"])
        .assert()
        .success();
    kio(&dir, "mock", first)
        .args(["adapter", "approve", "gemini_embedding_2", "--yes"])
        .assert()
        .success();

    // The first real synchronous group receives a confirmed 429. The second
    // group must not create a speculative reservation merely because it was in
    // the same planned batch.
    let failed = run_json_success(
        &dir,
        "rate_limit",
        first,
        &["index", "--online", "--realtime"],
    );
    assert!(
        failed["embedding_tasks_failed"]
            .as_u64()
            .unwrap_or_default()
            > 0,
        "the JSON result must disclose the rejected enrichment: {failed}"
    );
    let rows = embedding_rows(&dir);
    assert_eq!(
        rows.len(),
        1,
        "only the attempted group may reserve: {rows:?}"
    );
    assert_eq!(
        rows[0].1, "rate_limit",
        "the provider explicitly rejected it"
    );
    assert_eq!(rows[0].0, 3, "the rejected reservation is terminal");
    assert!(
        rows[0].3.is_none(),
        "the terminal row clears its intent token"
    );
    assert!(
        rows[0].2 > 0.0,
        "the request retains its original estimate; the zero belongs in cost_ledger"
    );

    // A later, due retry is a fresh provider attempt, not reuse of the
    // terminal rejected reservation. It can finish normally under the mock.
    let retried = run_json_success(
        &dir,
        "mock",
        "2026-09-08T00:01:00Z",
        &["batch", "retry", "--realtime"],
    );
    assert!(
        retried["tasks_executed"].as_u64().unwrap_or_default() > 0,
        "the authorized retry must execute current work: {retried}"
    );
    let rows = embedding_rows(&dir);
    assert!(
        rows.iter()
            .any(|(state, error, _, _)| *state == 2 && error.is_empty()),
        "retry must create and settle a fresh synchronous attempt: {rows:?}"
    );
    let outcomes = embedding_ledger_outcomes(&dir);
    assert!(
        outcomes
            .iter()
            .any(|(outcome, usd)| outcome == "submit_rejected" && *usd == 0.0)
            && outcomes.iter().any(|(outcome, _)| outcome == "succeeded"),
        "the 429 and fresh successful retry need separate terminal accounting: {outcomes:?}"
    );
}

#[test]
fn sync_embedding_network_result_unknown_fences_restart_until_explicit_resend() {
    let dir = fixture_dir();
    fs::write(dir.scope().join("one.md"), "unknown network result\n").unwrap();
    let first = "2026-09-08T00:00:00Z";
    kio(&dir, "mock", first).args(["init"]).assert().success();
    kio(&dir, "mock", first)
        .args(["ledger", "init"])
        .assert()
        .success();
    kio(&dir, "mock", first)
        .args(["index", "--offline"])
        .assert()
        .success();
    kio(&dir, "mock", first)
        .args(["adapter", "approve", "gemini_embedding_2", "--yes"])
        .assert()
        .success();

    let failed = run_json_success(
        &dir,
        "network_error",
        first,
        &["index", "--online", "--realtime"],
    );
    assert!(
        failed["embedding_tasks_failed"]
            .as_u64()
            .unwrap_or_default()
            > 0,
        "the local index result must disclose the unconfirmed provider result: {failed}"
    );
    let rows_before = embedding_rows(&dir);
    let outcomes_before = embedding_ledger_outcomes(&dir);
    assert_eq!(rows_before.len(), 1, "only the attempted group is charged");
    assert_eq!(rows_before[0].0, 3, "unknown result is terminal");
    assert_eq!(rows_before[0].1, "unknown_settled");
    assert!(
        rows_before[0].3.is_none(),
        "unknown settlement clears intent"
    );
    assert!(rows_before[0].2 > 0.0);
    assert!(
        outcomes_before
            .iter()
            .any(|(outcome, usd)| outcome == "unknown_settled" && *usd > 0.0),
        "unconfirmed provider work is conservatively charged: {outcomes_before:?}"
    );
    assert_eq!(
        source_embedding_count(&dir),
        0,
        "the failed request wrote no vector"
    );

    // A fresh CLI process with a healthy provider seam is still forbidden from
    // sending this known-unknown group. Only `batch --resend-unknown` can
    // authorize another charged provider attempt.
    let restarted = run_json_success(
        &dir,
        "mock",
        "2026-09-08T00:01:00Z",
        &["batch", "retry", "--realtime"],
    );
    assert_eq!(
        restarted["tasks_executed"].as_u64().unwrap_or_default(),
        0,
        "restart must not call the healthy provider without explicit authorization: {restarted}"
    );
    assert_eq!(
        embedding_rows(&dir),
        rows_before,
        "no new reservation on restart"
    );
    assert_eq!(
        embedding_ledger_outcomes(&dir),
        outcomes_before,
        "no additional provider accounting on restart"
    );
    assert_eq!(source_embedding_count(&dir), 0, "restart wrote no vector");
}
