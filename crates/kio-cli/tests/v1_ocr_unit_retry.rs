//! Known OCR responses may fail every unit without making the provider outcome
//! unknown. Their immutable manifests retain each unit's error kind, and only
//! the manifest planner may requeue retryable units.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use kio_pipeline::task::{TaskOutputRef, TaskStore, validate_task_output_ref};
use rusqlite::Connection;
use serde_json::Value;
use tempfile::TempDir;

const CHILD_ENV_DENYLIST: &[&str] = &[
    "GEMINI_API_KEY",
    "MISTRAL_API_KEY",
    "KIO_TEST_GEMINI_EMBED",
    "KIO_TEST_MISTRAL_OCR",
    "KIO_TEST_MISTRAL_BATCH",
    "KIO_TEST_MARKDOWNIZE_ADAPTER",
];

struct Fixture {
    _temp: TempDir,
    scope: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        #[cfg(target_os = "macos")]
        let temp = tempfile::tempdir_in("/private/tmp").unwrap();
        #[cfg(not(target_os = "macos"))]
        let temp = tempfile::tempdir().unwrap();
        make_owner_private(temp.path());
        let scope = temp.path().join("scope");
        let home = temp.path().join("home");
        // The managed scope and operational home must be siblings: discovery must
        // never find device state as input. Create every path before init so the
        // private-directory guard sees a complete owner-controlled hierarchy.
        for path in [
            &scope,
            &home,
            &home.join("data"),
            &home.join("config"),
            &home.join("cache"),
            &home.join("tmp"),
        ] {
            fs::create_dir_all(path).unwrap();
            make_owner_private(path);
        }
        fs::write(scope.join("scan.pdf"), b"%PDF-1.4\nscanned page\n").unwrap();
        Self {
            _temp: temp,
            scope,
            home,
        }
    }

    fn command(&self, args: &[&str], seam: &str) -> Command {
        let mut command = Command::cargo_bin("kio").unwrap();
        command.env_clear();
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", system_root);
        }
        for name in CHILD_ENV_DENYLIST {
            command.env_remove(name);
        }
        command
            .current_dir(&self.scope)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("XDG_DATA_HOME", self.home.join("data"))
            .env("XDG_CONFIG_HOME", self.home.join("config"))
            .env("XDG_CACHE_HOME", self.home.join("cache"))
            .env("TMPDIR", self.home.join("tmp"))
            .env("TEMP", self.home.join("tmp"))
            .env("TMP", self.home.join("tmp"))
            .env("KIO_FIXED_NOW", "2026-09-08T00:00:00Z")
            .env("KIO_TEST_MISTRAL_OCR", seam)
            .args(args)
            .arg("--json");
        command
    }

    fn ok(&self, args: &[&str], seam: &str) -> Value {
        let output = self.command(args, seam).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn batch(&self, args: &[&str], seam: &str, expected_exit: i32) -> Value {
        let output = self.command(args, seam).output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(expected_exit),
            "{args:?}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn prepare_pending_online_task(&self) {
        self.ok(&["init"], "mock");
        self.ok(&["ledger", "init"], "mock");
        self.ok(
            &["adapter", "approve", "mistral_ocr_markdownize", "--yes"],
            "mock",
        );
        self.ok(&["index"], "mock");
    }

    fn markdown_task(&self) -> Value {
        self.ok(&["status"], "mock")["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|task| {
                task["type"] == "markdownize"
                    && (task["output_ref"]
                        .as_str()
                        .is_some_and(|value| value.starts_with("online:"))
                        || task["fallback_reason"] == "online_adapter_done")
            })
            .cloned()
            .expect("online markdownize task")
    }

    fn markdown_cost(&self) -> (i64, f64) {
        let connection = Connection::open(self.home.join("data/kio/cost-ledger.sqlite")).unwrap();
        connection
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(usd), 0) \
                 FROM cost_ledger WHERE adapter_kind = 'markdownize'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    }
}

fn normalized_instance_dir_for_task(scope: &Path, task: &Value) -> PathBuf {
    let kio_dir = scope.join(".kio");
    let output_ref = task["output_ref"]
        .as_str()
        .unwrap_or_else(|| panic!("task must have an output_ref: {task}"));
    let descriptor = TaskStore::new(&kio_dir)
        .all()
        .unwrap()
        .into_iter()
        .find(|candidate| candidate.output_ref == output_ref)
        .unwrap_or_else(|| panic!("task store must contain output_ref {output_ref}"));
    let TaskOutputRef::NormalizedInstance { path, .. } =
        validate_task_output_ref(&kio_dir, &descriptor).unwrap()
    else {
        panic!("task must have a normalized instance output reference");
    };
    path
}

fn make_owner_private(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[test]
fn all_failed_network_units_retry_only_to_the_declared_finite_budget() {
    let fixture = Fixture::new();
    fixture.prepare_pending_online_task();

    fixture.batch(&["batch", "resume", "--realtime"], "all_failed_network", 3);
    let task = fixture.markdown_task();
    assert_eq!(task["status"], "failed", "{task}");
    let first_cost = fixture.markdown_cost();
    assert_eq!(
        first_cost.0, 1,
        "one settled known response: {first_cost:?}"
    );
    assert!(
        first_cost.1 > 0.0,
        "known response must settle cost: {first_cost:?}"
    );
    assert_eq!(task["fallback_reason"], "online_adapter_done", "{task}");
    assert_eq!(task["attempts"], 1, "{task}");
    let manifest_path =
        normalized_instance_dir_for_task(&fixture.scope, &task).join("manifest.json");
    let manifest: Value = serde_json::from_slice(&fs::read(manifest_path).unwrap()).unwrap();
    assert!(
        manifest["units"]
            .as_array()
            .unwrap()
            .iter()
            .all(|unit| unit["status"] == "failed" && unit["error_kind"] == "network_error")
    );

    // The NetworkError policy declares five total attempts. The four scheduled
    // retries each receive another known all-failed response; a sixth retry is
    // not sent and cannot create another paid request.
    for attempt in 2..=5 {
        fixture.batch(
            &["batch", "retry", "--realtime"],
            "all_failed_network",
            if attempt < 5 { 3 } else { 4 },
        );
        let task = fixture.markdown_task();
        assert_eq!(task["status"], "failed", "{task}");
        assert_eq!(task["attempts"], attempt, "{task}");
        let cost = fixture.markdown_cost();
        assert_eq!(
            cost.0,
            i64::from(attempt),
            "one settled charge per actual send: {cost:?}"
        );
        assert!(
            cost.1 > first_cost.1,
            "a fresh retry has a fresh charge: {cost:?}"
        );
    }
    let before_exhausted_cost = fixture.markdown_cost();
    let exhausted = fixture.batch(&["batch", "retry", "--realtime"], "all_failed_network", 0);
    assert_eq!(exhausted["tasks_updated"], 0, "{exhausted}");
    assert_eq!(fixture.markdown_task()["attempts"], 5);
    assert_eq!(fixture.markdown_cost(), before_exhausted_cost);
}

#[test]
fn all_failed_invalid_input_units_are_not_reenqueued_by_generic_failed_retry() {
    let fixture = Fixture::new();
    fixture.prepare_pending_online_task();

    fixture.batch(
        &["batch", "resume", "--realtime"],
        "all_failed_invalid_input",
        4,
    );
    let before = fixture.markdown_task();
    assert_eq!(before["status"], "failed", "{before}");
    assert_eq!(before["attempts"], 1, "{before}");
    assert_eq!(before["fallback_reason"], "online_adapter_done", "{before}");
    let manifest_path =
        normalized_instance_dir_for_task(&fixture.scope, &before).join("manifest.json");
    let manifest: Value = serde_json::from_slice(&fs::read(manifest_path).unwrap()).unwrap();
    assert!(
        manifest["units"]
            .as_array()
            .unwrap()
            .iter()
            .all(|unit| unit["status"] == "failed" && unit["error_kind"] == "invalid_input")
    );

    let before_retry_cost = fixture.markdown_cost();
    let retry = fixture.batch(
        &["batch", "retry", "--realtime"],
        "all_failed_invalid_input",
        0,
    );
    assert_eq!(retry["tasks_updated"], 0, "{retry}");
    assert_eq!(fixture.markdown_task()["attempts"], 1);
    assert_eq!(fixture.markdown_cost(), before_retry_cost);
}
