//! CLI contracts for create-only, authority-bound device ledger recovery.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use rusqlite::Connection;
use serde_json::Value;
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    repo: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let repo = base.join("outside-a-scope");
        let home = base.join("home");
        fs::create_dir(&repo).unwrap();
        fs::create_dir(&home).unwrap();
        make_owner_private(&home);
        fs::create_dir(home.join("tmp")).unwrap();
        make_owner_private(&home.join("tmp"));
        Self {
            _temp: temp,
            repo,
            home,
        }
    }

    fn data_root(&self) -> PathBuf {
        self.home.join("data")
    }

    fn ledger_path(&self) -> PathBuf {
        self.data_root().join("kio/cost-ledger.sqlite")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::cargo_bin("kio").unwrap();
        command.env_clear();
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", system_root);
        }
        command
            .current_dir(&self.repo)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("XDG_DATA_HOME", self.data_root())
            .env("XDG_CONFIG_HOME", self.home.join("config"))
            .env("XDG_CACHE_HOME", self.home.join("cache"))
            .env("TMPDIR", self.home.join("tmp"))
            .env("TEMP", self.home.join("tmp"))
            .env("TMP", self.home.join("tmp"))
            .arg("--json")
            .args(args);
        command
    }

    fn ok(&self, args: &[&str]) -> Value {
        let output = self.command(args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn error(&self, args: &[&str]) -> Value {
        let output = self.command(args).output().unwrap();
        assert!(
            !output.status.success(),
            "{args:?} unexpectedly succeeded: {}",
            String::from_utf8_lossy(&output.stdout),
        );
        serde_json::from_slice(&output.stderr).unwrap()
    }
}

fn backup_dir(fixture: &Fixture) -> PathBuf {
    let path = fixture.home.join("ledger-backup");
    fs::create_dir(&path).unwrap();
    make_owner_private(&path);
    path
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

fn seed_pending_and_budget_rows(path: &Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute(
        "INSERT INTO cost_ledger \
         (scope_id, adapter_kind, input_hash, tool_profile_hash, submission_seq, batch_job_id, \
          usd, estimated, outcome, month, recorded_at) \
         VALUES ('scope', 'markdownize', 'sha256:input', 'sha256:tool', 1, 'job-1', \
                 1.25, 1, 'succeeded', '2026-09', 1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO batch_requests \
         (scope_id, adapter_kind, input_hash, tool_profile_hash, estimated_usd, created_at) \
         VALUES ('scope', 'markdownize', 'sha256:pending', 'sha256:tool', 2.5, 2)",
        [],
    )
    .unwrap();
}

#[test]
fn backup_is_wal_coherent_and_restores_all_budget_and_pending_rows_outside_a_scope() {
    let fixture = Fixture::new();
    fixture.ok(&["ledger", "init"]);
    seed_pending_and_budget_rows(&fixture.ledger_path());
    let backup = backup_dir(&fixture);

    let result = fixture.ok(&["ledger", "backup", "--to", backup.to_str().unwrap()]);
    assert_eq!(result["status"], "backed_up", "{result}");
    for name in [
        "ledger.sqlite",
        "authority.json",
        "checkpoint.json",
        "manifest.json",
    ] {
        assert!(backup.join(name).is_file(), "missing {name}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(backup.join("manifest.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o077,
            0
        );
    }

    fs::remove_file(fixture.ledger_path()).unwrap();
    let restored = fixture.ok(&["ledger", "restore", "--from", backup.to_str().unwrap()]);
    assert_eq!(restored["status"], "restored", "{restored}");
    let conn = Connection::open(fixture.ledger_path()).unwrap();
    let costs: i64 = conn
        .query_row("SELECT COUNT(*) FROM cost_ledger", [], |row| row.get(0))
        .unwrap();
    let pending: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM batch_requests WHERE state = 0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(costs, 1);
    assert_eq!(pending, 1);
    assert_eq!(
        conn.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
}

#[test]
fn backup_is_create_only() {
    let fixture = Fixture::new();
    fixture.ok(&["ledger", "init"]);
    let backup = backup_dir(&fixture);
    fixture.ok(&["ledger", "backup", "--to", backup.to_str().unwrap()]);

    let exists = fixture.error(&["ledger", "backup", "--to", backup.to_str().unwrap()]);
    assert_eq!(exists["error_code"], "KIO-E-LEDGER-BACKUP-EXISTS-001");
}

#[test]
fn backup_operands_must_be_absolute() {
    let fixture = Fixture::new();
    fixture.ok(&["ledger", "init"]);
    let backup = fixture.error(&["ledger", "backup", "--to", "relative-backup"]);
    assert_eq!(backup["error_code"], "KIO-E-CONFIG-USAGE-001");
    let restore = fixture.error(&["ledger", "restore", "--from", "relative-backup"]);
    assert_eq!(restore["error_code"], "KIO-E-CONFIG-USAGE-001");
}

#[test]
fn restore_rejects_a_backup_from_another_ledger_without_creating_a_database() {
    let source = Fixture::new();
    source.ok(&["ledger", "init"]);
    let source_backup = backup_dir(&source);
    source.ok(&["ledger", "backup", "--to", source_backup.to_str().unwrap()]);

    let target = Fixture::new();
    target.ok(&["ledger", "init"]);
    fs::remove_file(target.ledger_path()).unwrap();
    let error = target.error(&[
        "ledger",
        "restore",
        "--from",
        source_backup.to_str().unwrap(),
    ]);
    assert_eq!(error["error_code"], "KIO-E-LEDGER-RESTORE-EVIDENCE-001");
    assert!(!target.ledger_path().exists());
}

#[cfg(unix)]
#[test]
fn restore_rejects_a_symlinked_backup_manifest() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    fixture.ok(&["ledger", "init"]);
    let backup = backup_dir(&fixture);
    fixture.ok(&["ledger", "backup", "--to", backup.to_str().unwrap()]);
    let manifest = backup.join("manifest.json");
    fs::remove_file(&manifest).unwrap();
    symlink(backup.join("checkpoint.json"), &manifest).unwrap();
    fs::remove_file(fixture.ledger_path()).unwrap();

    let error = fixture.error(&["ledger", "restore", "--from", backup.to_str().unwrap()]);
    assert_eq!(error["error_code"], "KIO-E-LEDGER-BACKUP-PRIVATE-001");
}
