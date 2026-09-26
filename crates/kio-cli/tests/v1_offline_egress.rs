//! `batch resume` and `batch retry` must treat `--offline` as an egress
//! boundary even when the persisted scope has already approved OCR work.
//!
//! The fixture deliberately prepares a real pending online-OCR task through
//! normal CLI management commands, initializes the paid ledger explicitly,
//! and then observes the built-in test-provider seams.  A final online resume
//! is the positive control: a zero offline trace cannot be explained by an
//! invalid task or a missing approval.

mod support;

use support::canonical_tempdir;

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use tempfile::TempDir;

const KIO_CHILD_ENV_DENYLIST: &[&str] = &[
    "GEMINI_API_KEY",
    "MISTRAL_API_KEY",
    "KIO_FIXED_NOW",
    "KIO_TEST_GEMINI_EMBED",
    "KIO_TEST_GEMINI_BATCH",
    "KIO_TEST_MISTRAL_OCR",
    "KIO_TEST_MISTRAL_BATCH",
    "KIO_TEST_BATCH_INVENTORY",
    "KIO_TEST_MARKDOWNIZE_ADAPTER",
    "KIO_TEST_CAPTURE_SENT_MEDIA",
];

#[derive(Clone, Copy, Debug)]
enum Lane {
    Sync,
    Batch,
}

impl Lane {
    fn invocation_flag(self) -> &'static str {
        match self {
            Self::Sync => "--realtime",
            Self::Batch => "--batch",
        }
    }
}

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new(lane: Lane) -> Self {
        let temp = canonical_tempdir();
        let root = temp.path().join("root");
        let home = temp.path().join("home");
        // The managed root and operational HOME must be siblings.  Otherwise
        // normal recursive discovery can mistake the ledger/XDG data for
        // managed input.
        fs::create_dir(&root).unwrap();
        fs::create_dir(&home).unwrap();
        make_owner_private(&home);
        fs::create_dir(home.join("tmp")).unwrap();
        make_owner_private(&home.join("tmp"));
        fs::write(root.join("scan.pdf"), scanned_pdf_bytes()).unwrap();

        let fixture = Self {
            _temp: temp,
            root,
            home,
        };
        fixture.ok(&["init"]);
        // Ledger initialization is intentional: this is a paid-online fixture,
        // and the offline assertions must prove that it creates no *new* rows.
        fixture.ok(&["ledger", "init"]);
        if matches!(lane, Lane::Batch) {
            // The built-in OCR adapter normally prefers Batch.  Keep this
            // explicit so the Batch provider trace is the observed boundary.
            fs::write(
                fixture.root().join(".kio/config.toml"),
                "[markdownize]\nbbox_annotation = false\n",
            )
            .unwrap();
        }
        fixture.ok_with_env(
            &["adapter", "approve", "mistral_ocr_markdownize", "--yes"],
            &[("KIO_TEST_MISTRAL_OCR", "mock")],
        );
        fixture.ok_with_env(&["index"], &[("KIO_TEST_MISTRAL_OCR", "mock")]);

        let status = fixture.ok(&["status"]);
        let task = markdownize_task(&status);
        assert_eq!(task["status"], "pending", "{status}");
        assert!(
            task["output_ref"]
                .as_str()
                .is_some_and(|value| value.starts_with("online:")),
            "{status}"
        );
        fixture
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn ledger_path(&self) -> PathBuf {
        self.home.join("data/kio/cost-ledger.sqlite")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::cargo_bin("kio").unwrap();
        for name in KIO_CHILD_ENV_DENYLIST {
            command.env_remove(name);
        }
        command.current_dir(self.root()).env_clear();
        // Windows needs this OS runtime setting, while no account settings or
        // provider credentials may cross the child-process boundary.
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", system_root);
        }
        command
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("XDG_DATA_HOME", self.home.join("data"))
            .env("XDG_CONFIG_HOME", self.home.join("config"))
            .env("XDG_CACHE_HOME", self.home.join("cache"))
            .env("TMPDIR", self.home.join("tmp"))
            .env("TEMP", self.home.join("tmp"))
            .env("TMP", self.home.join("tmp"))
            .args(args)
            .arg("--json");
        command
    }

    fn ok(&self, args: &[&str]) -> Value {
        self.ok_with_env(args, &[])
    }

    fn ok_with_env(&self, args: &[&str], env: &[(&str, &str)]) -> Value {
        let mut command = self.command(args);
        for (name, value) in env {
            command.env(name, value);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn ledger_counts(&self) -> (i64, i64) {
        let conn =
            Connection::open_with_flags(self.ledger_path(), OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        let reservations = conn
            .query_row("SELECT COUNT(*) FROM batch_requests", [], |row| row.get(0))
            .unwrap();
        let costs = conn
            .query_row("SELECT COUNT(*) FROM cost_ledger", [], |row| row.get(0))
            .unwrap();
        (reservations, costs)
    }
}

fn make_owner_private(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

fn scanned_pdf_bytes() -> Vec<u8> {
    let mut scan = b"%PDF-1.4\n".to_vec();
    scan.extend((0u32..4000).map(|i| (i.wrapping_mul(97) & 0x7f) as u8 | 0x80));
    scan
}

fn markdownize_task(status: &Value) -> &Value {
    let tasks: Vec<&Value> = status["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|task| task["type"] == "markdownize")
        .collect();
    assert_eq!(tasks.len(), 1, "expected one OCR task: {status}");
    tasks[0]
}

fn batch_script(capture: &Path) -> String {
    json!({
        "status_sequence": ["QUEUED"],
        "capture_path": capture.display().to_string(),
    })
    .to_string()
}

#[test]
fn offline_batch_resume_and_retry_block_sync_and_batch_ocr_egress() {
    for lane in [Lane::Sync, Lane::Batch] {
        let fixture = Fixture::new(lane);
        let trace = fixture.root().join(match lane {
            Lane::Sync => "sync-ocr-receipt.txt",
            Lane::Batch => "batch-ocr-trace.jsonl",
        });
        let scope_before = fs::read(fixture.root().join(".kio/scope.json")).unwrap();
        let ledger_before = fixture.ledger_counts();
        let output_before = markdownize_task(&fixture.ok(&["status"]))["output_ref"].clone();

        // The override must not weaken the explicit offline boundary.  Exercise
        // both recovery verbs: resume can dispatch Pending work; retry must
        // likewise leave it untouched when an operator selected --offline.
        for args in [
            vec![
                "batch",
                "resume",
                "--offline",
                "--override-budget",
                lane.invocation_flag(),
            ],
            vec!["batch", "retry", "--offline", lane.invocation_flag()],
        ] {
            let batch = batch_script(&trace);
            let mut command = fixture.command(&args);
            command
                .env("KIO_TEST_MISTRAL_OCR", "mock")
                .env("KIO_TEST_MISTRAL_BATCH", batch)
                .env("KIO_TEST_CAPTURE_SENT_MEDIA", &trace);
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{lane:?} {args:?}: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            assert_eq!(fixture.ledger_counts(), ledger_before, "{lane:?} {args:?}");
            assert!(
                !trace.exists(),
                "{lane:?} {args:?} reached a provider: {}",
                trace.display()
            );
            assert_eq!(
                fs::read(fixture.root().join(".kio/scope.json")).unwrap(),
                scope_before,
                "{lane:?} {args:?} must not create or change authorization"
            );
            let status = fixture.ok(&["status"]);
            let task = markdownize_task(&status);
            assert_eq!(task["status"], "pending", "{lane:?} {args:?}: {status}");
            assert_eq!(
                task["output_ref"], output_before,
                "{lane:?} {args:?}: {status}"
            );
        }

        // Positive control: the same approved, prepared task reaches the
        // selected hermetic provider once online.  This distinguishes a true
        // offline egress block from an invalid fixture or absent approval.
        let batch = batch_script(&trace);
        let mut command = fixture.command(&["batch", "resume", lane.invocation_flag()]);
        command
            .env("KIO_TEST_MISTRAL_OCR", "mock")
            .env("KIO_TEST_MISTRAL_BATCH", batch)
            .env("KIO_TEST_CAPTURE_SENT_MEDIA", &trace);
        let output = command.output().unwrap();
        assert!(
            !output.stdout.is_empty(),
            "{lane:?} online positive control produced no response: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            trace.exists(),
            "{lane:?} online positive control did not send"
        );
        match lane {
            Lane::Sync => {
                let receipt = fs::read_to_string(&trace).unwrap();
                assert_eq!(
                    receipt.lines().count(),
                    2,
                    "one sync OCR receipt has exactly two fields: {receipt}"
                );
                assert!(
                    receipt.starts_with("application/pdf\ntrue\n"),
                    "sync receipt must be written by the OCR mock"
                );
            }
            Lane::Batch => {
                let calls: Vec<Value> = fs::read_to_string(&trace)
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                assert_eq!(
                    calls.len(),
                    2,
                    "batch submit must make two calls: {calls:?}"
                );
                assert_eq!(calls[0]["call"], "upload", "{calls:?}");
                assert_eq!(calls[1]["call"], "create_job", "{calls:?}");
            }
        }
        assert!(
            fixture.ledger_counts().0 > ledger_before.0,
            "{lane:?} online positive control must create a reservation"
        );
    }
}
