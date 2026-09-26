//! Live ancestor policy must apply to durable projections without a watch pass.
//! All adapters and credentials in this fixture are synthetic and process-local.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use rusqlite::Connection;
use serde_json::Value;
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    child: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let child = root.join("child");
        let home = temp.path().join("home");
        fs::create_dir_all(&child).unwrap();
        fs::create_dir_all(home.join("tmp")).unwrap();
        let fixture = Self {
            _temp: temp,
            root,
            child,
            home,
        };
        fixture.ok_at(&fixture.root, &["init"]);
        fixture.ok_at(&fixture.root, &["ledger", "init"]);
        fs::write(
            fixture.child.join("denied.md"),
            "# orchid\n\norchid orchid orchid exact match\n",
        )
        .unwrap();
        fs::write(
            fixture.child.join("allowed.md"),
            "# Field notes\n\nAn orchid grows among many other flowers in this garden.\n",
        )
        .unwrap();
        fixture.ok_at(&fixture.root, &["index", "--offline"]);
        fixture
    }

    fn command(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("kio"));
        command.env_clear();
        // Windows needs its OS runtime path; neither account settings nor
        // provider credentials are inherited by these child processes.
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", system_root);
        }
        command
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("XDG_DATA_HOME", self.home.join("data"))
            .env("XDG_CONFIG_HOME", self.home.join("config"))
            .env("XDG_CACHE_HOME", self.home.join("cache"))
            .env("TMPDIR", self.home.join("tmp"))
            .env("TEMP", self.home.join("tmp"))
            .env("TMP", self.home.join("tmp"))
            .env("KIO_TEST_MARKDOWNIZE_ADAPTER", "deterministic")
            .env("KIO_TEST_GEMINI_EMBED", "mock")
            .arg("--json")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn ok_at(&self, cwd: &Path, args: &[&str]) -> Value {
        let output = self.command(cwd, args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn approve_online_at(&self, cwd: &Path, tool: &str) {
        let output = self
            .command(cwd, &["adapter", "approve", tool, "--yes"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "online approval failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn search(&self, extra: &[&str]) -> Value {
        let mut args = vec!["search", "orchid", "--scope", ".", "--mode", "text"];
        args.extend_from_slice(extra);
        self.ok_at(&self.child, &args)
    }

    fn ignore(&self, text: &str) {
        fs::write(self.root.join(".kioignore"), text).unwrap();
    }

    fn head(&self) -> String {
        fs::read_to_string(self.child.join(".kio/HEAD"))
            .unwrap()
            .trim()
            .to_owned()
    }

    fn ledger_counts(&self) -> (i64, i64) {
        let path = self.home.join("data/kio/cost-ledger.sqlite");
        if !path.exists() {
            return (0, 0);
        }
        let conn =
            Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        (
            conn.query_row("SELECT count(*) FROM batch_requests", [], |r| r.get(0))
                .unwrap(),
            conn.query_row("SELECT count(*) FROM cost_ledger", [], |r| r.get(0))
                .unwrap(),
        )
    }
}

fn paths(value: &Value) -> Vec<String> {
    value["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            row["evidence_pointer"]["path_at_commit"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

fn wait_ready(ready: &Path, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(4);
    while !ready.exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "search exited before its barrier"
        );
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("search did not reach its barrier");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn assert_policy_failure(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error_code"], "KIO-E-SEARCH-POLICY-001", "{error}");
    assert!(
        output.stdout.is_empty(),
        "policy failure must not emit materialized results"
    );
}

#[test]
fn ancestor_ignore_filters_current_and_historical_results_before_limit() {
    let fixture = Fixture::new();
    let head = fixture.head();
    let baseline = fixture.search(&[]);
    assert_eq!(paths(&baseline).len(), 2, "{baseline}");
    fixture.ignore("child/denied.md\n");
    for selector in [
        vec![],
        vec!["--at", head.as_str()],
        vec!["--all-history"],
        vec!["--include-deleted"],
    ] {
        let mut options = selector;
        options.extend(["--limit", "1"]);
        let page = fixture.search(&options);
        assert_eq!(paths(&page), vec!["allowed.md"], "{options:?}: {page}");
    }
    assert_eq!(
        fixture.head(),
        head,
        "search must not create a policy-repair commit"
    );
}

#[test]
fn one_root_index_enrolls_empty_descendants_through_their_immediate_parent() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.root.join("empty/second/third")).unwrap();
    fs::create_dir_all(fixture.child.join("hidden/deeper")).unwrap();
    fs::write(fixture.child.join(".kioignore"), "hidden/\n").unwrap();
    let output = fixture.ok_at(&fixture.root, &["index", "--offline"]);
    let parent: Value =
        serde_json::from_slice(&fs::read(fixture.root.join("empty/.kio/management.json")).unwrap())
            .unwrap();
    let second: Value = serde_json::from_slice(
        &fs::read(fixture.root.join("empty/second/.kio/management.json")).unwrap(),
    )
    .unwrap();
    let third: Value = serde_json::from_slice(
        &fs::read(fixture.root.join("empty/second/third/.kio/management.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        second["authority"]["parent_scope_id"], parent["scope_id"],
        "{output}"
    );
    assert_eq!(
        third["authority"]["parent_scope_id"], second["scope_id"],
        "{output}"
    );
    assert!(fixture.root.join("empty/second/third/.kio/HEAD").is_file());
    assert!(!fixture.child.join("hidden/.kio").exists());
    assert!(!fixture.child.join("hidden/deeper/.kio").exists());
    // Reconciliation reuses those bindings and also finds later empty additions.
    fs::create_dir(fixture.root.join("empty/later")).unwrap();
    fixture.ok_at(&fixture.root, &["index", "--offline"]);
    assert!(
        fixture
            .root
            .join("empty/later/.kio/management.json")
            .is_file()
    );
}

#[test]
fn root_confirmation_does_not_grant_external_sends_to_children() {
    let fixture = Fixture::new();
    fixture.approve_online_at(&fixture.root, "gemini_embedding_2");
    let capture = fixture.home.join("child-sends.jsonl");
    let script = serde_json::json!({
        "state_sequence": ["BATCH_STATE_PENDING"],
        "job_name": "batches/must-not-submit",
        "capture_path": capture,
    })
    .to_string();
    let before = fixture.ledger_counts();
    let output = fixture
        .command(&fixture.root, &["index", "--online", "--batch"])
        .env("KIO_TEST_GEMINI_BATCH", script)
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(if output.stdout.is_empty() {
        &output.stderr
    } else {
        &output.stdout
    })
    .unwrap();
    if report["error_code"] == "KIO-E-INDEX-PARTIAL-001" {
        let child = report["child_scopes"]
            .as_array()
            .expect("multi-scope result")
            .iter()
            .find(|row| row["path"] == "child")
            .expect("child result");
        assert_eq!(child["status"], "skipped_error");
        assert_eq!(child["error_code"], "KIO-E-CONFIG-USAGE-001");
    } else {
        assert_eq!(report["error_code"], "KIO-E-CONFIG-USAGE-001", "{report}");
        assert!(
            report["message"]
                .as_str()
                .unwrap_or_default()
                .contains("active external-send approval")
        );
    }
    assert!(
        !capture.exists(),
        "local descendant management must not authorize a send"
    );
    assert_eq!(fixture.ledger_counts(), before);
    let scope: Value =
        serde_json::from_slice(&fs::read(fixture.child.join(".kio/scope.json")).unwrap()).unwrap();
    assert!(
        scope["approvals"]
            .as_array()
            .is_none_or(|rows| rows.is_empty())
    );
}

#[test]
fn offline_index_does_not_poll_existing_embedding_jobs() {
    let fixture = Fixture::new();
    fixture.approve_online_at(&fixture.child, "gemini_embedding_2");
    let capture = fixture.home.join("offline-batch-calls.jsonl");
    let script = serde_json::json!({
        "state_sequence": ["BATCH_STATE_PENDING"],
        "job_name": "batches/offline-contract",
        "capture_path": capture,
    })
    .to_string();
    let submit = fixture
        .command(&fixture.child, &["index", "--online", "--batch"])
        .env("KIO_TEST_GEMINI_BATCH", &script)
        .output()
        .unwrap();
    assert!(
        submit.status.success(),
        "{}",
        String::from_utf8_lossy(&submit.stderr)
    );
    let before = fs::read_to_string(&capture).unwrap();
    assert!(
        before.contains("create_embedding_job"),
        "the positive control must have a provider job"
    );
    let ledger_before = fixture.ledger_counts();
    let offline = fixture
        .command(&fixture.child, &["index", "--offline"])
        .env("KIO_TEST_GEMINI_BATCH", &script)
        .output()
        .unwrap();
    assert!(
        offline.status.success(),
        "{}",
        String::from_utf8_lossy(&offline.stderr)
    );
    assert_eq!(
        fs::read_to_string(&capture).unwrap(),
        before,
        "offline means no provider calls, including polls"
    );
    assert_eq!(fixture.ledger_counts(), ledger_before);
    let online = fixture
        .command(&fixture.child, &["index", "--online", "--batch"])
        .env("KIO_TEST_GEMINI_BATCH", &script)
        .output()
        .unwrap();
    assert!(
        online.status.success(),
        "{}",
        String::from_utf8_lossy(&online.stderr)
    );
    assert!(fs::read_to_string(capture).unwrap()[before.len()..].contains("poll_job"));
}

#[test]
fn historical_batch_embedding_requires_current_ancestor_policy() {
    // Positive control: the same historical path and explicit scope consent
    // really reach the synthetic batch provider when policy allows them.
    for denied in [false, true] {
        let fixture = Fixture::new();
        fixture.ok_at(&fixture.child, &["index", "--offline"]);
        let head = fixture.head();
        fixture.approve_online_at(&fixture.child, "gemini_embedding_2");
        let capture = fixture.home.join("history-sends.jsonl");
        let script = serde_json::json!({
            "state_sequence": ["BATCH_STATE_PENDING"],
            "job_name": "batches/historical-policy",
            "capture_path": capture,
        })
        .to_string();
        if denied {
            fixture.ignore("child/\n");
        }
        let before = fixture.ledger_counts();
        let output = fixture
            .command(
                &fixture.child,
                &["reindex", "--at", &head, "--online", "--batch"],
            )
            .env("KIO_TEST_GEMINI_BATCH", script)
            .output()
            .unwrap();
        if denied {
            assert!(
                !output.status.success(),
                "denied historical send unexpectedly succeeded"
            );
            let error: Value = serde_json::from_slice(&output.stderr).unwrap();
            assert_eq!(
                error["error_code"], "KIO-E-ADAPTER-APPROVAL-REQUIRED-001",
                "{error}"
            );
            assert!(
                !capture.exists(),
                "denied historical content must not reach the adapter"
            );
            assert_eq!(
                fixture.ledger_counts(),
                before,
                "denied content cannot reserve budget"
            );
        } else {
            assert!(
                output.status.success(),
                "positive control failed: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let captured =
                fs::read_to_string(&capture).expect("positive control must submit a job");
            assert!(captured.contains("create_embedding_job"));
            assert!(fixture.ledger_counts().0 > before.0);
        }
    }
}

#[test]
fn ancestor_ignore_hides_deleted_historical_names() {
    let fixture = Fixture::new();
    let past = fixture.head();
    fs::remove_file(fixture.child.join("denied.md")).unwrap();
    fixture.ok_at(&fixture.child, &["index", "--offline"]);
    assert!(paths(&fixture.search(&["--at", &past])).contains(&"denied.md".to_owned()));
    fixture.ignore("child/denied.md\n");
    for selector in [
        vec!["--at", past.as_str()],
        vec!["--all-history"],
        vec!["--include-deleted"],
    ] {
        let result = fixture.search(&selector);
        assert!(
            !paths(&result).contains(&"denied.md".to_owned()),
            "{result}"
        );
    }
}

#[test]
fn cursor_replay_is_stable_until_ancestor_policy_changes() {
    let fixture = Fixture::new();
    let first = fixture.search(&["--limit", "1"]);
    let cursor = first["paging"]["next_cursor"]
        .as_str()
        .unwrap_or_else(|| panic!("two results require page two: {first}"));
    let second = fixture.search(&["--limit", "1", "--cursor", cursor]);
    let replay = fixture.search(&["--limit", "1", "--cursor", cursor]);
    assert_eq!(second["results"], replay["results"]);
    fixture.ignore("child/denied.md\n");
    let output = fixture
        .command(
            &fixture.child,
            &[
                "search", "orchid", "--scope", ".", "--mode", "text", "--limit", "1", "--cursor",
                cursor,
            ],
        )
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error_code"], "KIO-E-SEARCH-CURSOR-001", "{error}");
    assert!(output.stdout.is_empty());
}

#[test]
fn policy_change_during_response_assembly_emits_no_content() {
    let fixture = Fixture::new();
    let ready = fixture.home.join("response.ready");
    let mut child = fixture
        .command(
            &fixture.child,
            &["search", "orchid", "--scope", ".", "--mode", "text"],
        )
        .env("KIO_TEST_SEARCH_RESPONSE_BARRIER_READY", &ready)
        .spawn()
        .unwrap();
    wait_ready(&ready, &mut child);
    fixture.ignore("child/\n");
    fs::write(ready.with_extension("release"), b"go").unwrap();
    assert_policy_failure(&child.wait_with_output().unwrap());
}

#[test]
fn policy_change_at_final_query_admission_sends_nothing_and_reserves_nothing() {
    let fixture = Fixture::new();
    fixture.approve_online_at(&fixture.child, "gemini_embedding_2");
    fixture
        .command(&fixture.child, &["index", "--online", "--realtime"])
        .output()
        .map(|output| {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        })
        .unwrap();
    let before = fixture.ledger_counts();
    assert!(
        before.1 > 0,
        "the positive embedding setup must actually exercise the ledger"
    );
    let ready = fixture.home.join("query.ready");
    let trace = fixture.home.join("query-sends.txt");
    let mut child = fixture
        .command(
            &fixture.child,
            &["search", "orchid", "--scope", ".", "--mode", "vector"],
        )
        .env("KIO_TEST_SEARCH_FINAL_CONSENT_BARRIER_READY", &ready)
        .env("KIO_TEST_QUERY_EMBED_TRACE", &trace)
        .spawn()
        .unwrap();
    wait_ready(&ready, &mut child);
    fixture.ignore("child/\n");
    fs::write(ready.with_extension("release"), b"go").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_policy_failure(&output);
    assert!(
        !trace.exists(),
        "no provider admission is allowed after the observed policy change"
    );
    assert_eq!(
        fixture.ledger_counts(),
        before,
        "no claim, reservation or charge may be created"
    );
}
