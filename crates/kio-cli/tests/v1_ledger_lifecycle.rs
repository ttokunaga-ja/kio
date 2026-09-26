//! Public CLI contracts for the explicit device-ledger lifecycle.
//!
//! The ledger is an opt-in device-global store.  These tests deliberately use
//! empty, isolated homes and never inherit credentials or account settings.

mod support;

use support::canonical_tempdir;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

#[derive(Debug, PartialEq, Eq)]
enum TreeEntry {
    Directory,
    File(FileFingerprint),
    Symlink(Vec<u8>),
}

#[derive(Debug, PartialEq, Eq)]
struct FileFingerprint {
    bytes: u64,
    sha256: String,
}

struct Fixture {
    _temp: TempDir,
    repo: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = canonical_tempdir();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
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

    fn ledger_dir(&self) -> PathBuf {
        self.data_root().join("kio")
    }

    fn ledger_path(&self) -> PathBuf {
        self.ledger_dir().join("cost-ledger.sqlite")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::cargo_bin("kio").unwrap();
        command.env_clear();
        // The Windows C runtime needs this base setting, but no user settings
        // or credentials are inherited into the child process.
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
            .env("KIO_TEST_MARKDOWNIZE_ADAPTER", "deterministic")
            .env("KIO_TEST_GEMINI_EMBED", "mock")
            .arg("--json")
            .args(args);
        command
    }

    fn json_ok(&self, args: &[&str]) -> Value {
        let output = self.command(args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn json_error(&self, args: &[&str]) -> Value {
        let output = self.command(args).output().unwrap();
        assert!(
            !output.status.success(),
            "{args:?} unexpectedly succeeded: {}",
            String::from_utf8_lossy(&output.stdout),
        );
        serde_json::from_slice(&output.stderr).unwrap()
    }

    fn initialize_scope_with_text(&self) {
        fs::write(
            self.repo.join("note.md"),
            "# Local note\n\nledger lifecycle text needle\n",
        )
        .unwrap();
        self.json_ok(&["init"]);
        self.json_ok(&["index", "--offline"]);
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

fn file_tree(root: &Path) -> BTreeMap<PathBuf, TreeEntry> {
    fn collect(root: &Path, directory: &Path, tree: &mut BTreeMap<PathBuf, TreeEntry>) {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                tree.insert(relative, TreeEntry::Directory);
                collect(root, &path, tree);
            } else if kind.is_file() {
                let bytes = fs::read(&path).unwrap();
                tree.insert(
                    relative,
                    TreeEntry::File(FileFingerprint {
                        bytes: bytes.len() as u64,
                        sha256: Sha256::digest(bytes)
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect(),
                    }),
                );
            } else if kind.is_symlink() {
                tree.insert(
                    relative,
                    TreeEntry::Symlink(
                        fs::read_link(path)
                            .unwrap()
                            .into_os_string()
                            .into_encoded_bytes(),
                    ),
                );
            }
        }
    }

    let mut tree = BTreeMap::new();
    collect(root, root, &mut tree);
    tree
}

fn ledger_artifacts(fixture: &Fixture) -> BTreeMap<PathBuf, TreeEntry> {
    const LIFECYCLE_LEAVES: [&str; 7] = [
        "cost-ledger.sqlite",
        "cost-ledger.sqlite-wal",
        "cost-ledger.sqlite-shm",
        "cost-ledger.sqlite.authority.json",
        "cost-ledger.sqlite.checkpoint.json",
        "cost-ledger.sqlite.init.pending",
        "ledger.lifecycle.lock",
    ];

    file_tree(&fixture.ledger_dir())
        .into_iter()
        .filter(|(relative, _)| {
            LIFECYCLE_LEAVES
                .iter()
                .any(|leaf| relative == Path::new(leaf))
        })
        .collect()
}

#[test]
fn fresh_ledger_status_is_uninitialized_and_creates_no_device_artifacts() {
    let fixture = Fixture::new();
    let before = file_tree(&fixture.home);

    let status = fixture.json_ok(&["ledger", "status"]);

    assert_eq!(status["status"], "uninitialized", "{status}");
    let expected_path = fixture.ledger_path().to_string_lossy().into_owned();
    assert_eq!(status["path"].as_str(), Some(expected_path.as_str()));
    assert_eq!(file_tree(&fixture.home), before);
    assert!(!fixture.data_root().exists());
}

#[test]
fn resume_requires_an_existing_initialization_intent_and_never_starts_one() {
    let fixture = Fixture::new();

    let error = fixture.json_error(&["ledger", "init", "--resume"]);

    assert_eq!(
        error["error_code"], "KIO-E-LEDGER-UNINITIALIZED-001",
        "{error}"
    );
    // The CLI records an error observation, but `--resume` itself must not
    // create an initialization intent, SQLite database, or authority.
    for leaf in [
        "cost-ledger.sqlite",
        "cost-ledger.sqlite.authority.json",
        "cost-ledger.sqlite.checkpoint.json",
        "cost-ledger.sqlite.init.pending",
    ] {
        assert!(!fixture.ledger_dir().join(leaf).exists(), "created {leaf}");
    }
}

#[test]
fn offline_scope_work_and_budget_observation_do_not_initialize_a_ledger() {
    let fixture = Fixture::new();
    fixture.initialize_scope_with_text();

    let search = fixture.json_ok(&["search", "needle", "--mode", "text"]);
    assert!(
        search["results"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty()),
        "{search}"
    );

    let status = fixture.json_ok(&["status"]);
    assert_eq!(
        status["budget"]["ledger_status"], "uninitialized",
        "{status}"
    );
    for field in [
        "device_spent_usd",
        "folder_spent_usd",
        "device_remaining_usd",
        "folder_remaining_usd",
        "cap_kind",
    ] {
        assert!(status["budget"][field].is_null(), "{field}: {status}");
    }
    assert!(
        !fixture.ledger_path().exists(),
        "offline work must not create a device ledger"
    );
    assert!(
        !fixture
            .ledger_dir()
            .join("cost-ledger.sqlite.authority.json")
            .exists(),
        "offline work must not create ledger authority"
    );
}

#[test]
fn explicit_init_makes_status_ready_without_mutating_ledger_artifacts() {
    let fixture = Fixture::new();
    fixture.json_ok(&["init"]);
    let initialized = fixture.json_ok(&["ledger", "init"]);
    assert_eq!(initialized["status"], "initialized", "{initialized}");
    let before = ledger_artifacts(&fixture);

    let status = fixture.json_ok(&["ledger", "status"]);

    assert_eq!(status["status"], "ready", "{status}");
    assert_eq!(
        ledger_artifacts(&fixture),
        before,
        "ledger status is a read-only lifecycle observation"
    );
    let scope_status = fixture.json_ok(&["status"]);
    assert_eq!(scope_status["budget"]["device_spent_usd"], 0.0);
    assert_eq!(scope_status["stalled_batch"], serde_json::json!([]));
    assert_eq!(
        ledger_artifacts(&fixture),
        before,
        "scope status must use an owned snapshot without touching ledger artifacts"
    );
}

#[test]
fn repeated_init_refuses_and_preserves_the_original_authority() {
    let fixture = Fixture::new();
    fixture.json_ok(&["ledger", "init"]);
    let before = ledger_artifacts(&fixture);
    let authority = fs::read(
        fixture
            .ledger_dir()
            .join("cost-ledger.sqlite.authority.json"),
    )
    .unwrap();

    let error = fixture.json_error(&["ledger", "init"]);

    assert_eq!(
        error["error_code"], "KIO-E-LEDGER-ALREADY-EXISTS-001",
        "{error}"
    );
    assert_eq!(
        fs::read(
            fixture
                .ledger_dir()
                .join("cost-ledger.sqlite.authority.json"),
        )
        .unwrap(),
        authority,
    );
    assert_eq!(ledger_artifacts(&fixture), before);
}

#[test]
fn partial_ledger_artifacts_refuse_status_and_search_without_resetting_state() {
    let fixture = Fixture::new();
    fixture.initialize_scope_with_text();
    fixture.json_ok(&["ledger", "init"]);
    let authority_path = fixture
        .ledger_dir()
        .join("cost-ledger.sqlite.authority.json");
    let authority = fs::read(&authority_path).unwrap();
    let checkpoint = fixture
        .ledger_dir()
        .join("cost-ledger.sqlite.checkpoint.json");
    fs::remove_file(&checkpoint).unwrap();

    let status_error = fixture.json_error(&["ledger", "status"]);
    let search_error = fixture.json_error(&["search", "needle", "--mode", "text"]);

    assert_ne!(status_error["error_code"], "", "{status_error}");
    assert_ne!(search_error["error_code"], "", "{search_error}");
    assert!(
        !checkpoint.exists(),
        "observation must not recreate a checkpoint"
    );
    assert_eq!(fs::read(&authority_path).unwrap(), authority);
    assert!(fixture.ledger_path().exists());

    fs::remove_file(fixture.ledger_path()).unwrap();
    let before_missing_db = ledger_artifacts(&fixture);
    let status_error = fixture.json_error(&["ledger", "status"]);
    let search_error = fixture.json_error(&["search", "needle", "--mode", "text"]);

    assert_ne!(status_error["error_code"], "", "{status_error}");
    assert_ne!(search_error["error_code"], "", "{search_error}");
    assert!(!fixture.ledger_path().exists());
    assert_eq!(fs::read(&authority_path).unwrap(), authority);
    assert_eq!(ledger_artifacts(&fixture), before_missing_db);
}

#[test]
fn ledger_lifecycle_bypasses_malformed_adapter_configuration() {
    let fixture = Fixture::new();
    let config = fixture.home.join("config/kio/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "[adapter\nthis is not valid toml").unwrap();

    let before = file_tree(&fixture.home);
    let status = fixture.json_ok(&["ledger", "status"]);
    assert_eq!(status["status"], "uninitialized", "{status}");
    assert_eq!(file_tree(&fixture.home), before);

    let initialized = fixture.json_ok(&["ledger", "init"]);
    assert_eq!(initialized["status"], "initialized", "{initialized}");
    assert_eq!(fixture.json_ok(&["ledger", "status"])["status"], "ready");
}

/// An existing paid ledger is no reason to take mutable billing authority for
/// local embeddings, including when the provider lane defaults to Batch.
#[test]
fn local_embedding_and_vector_search_preserve_an_initialized_ledger() {
    let fixture = Fixture::new();
    fs::write(
        fixture.repo.join("note.md"),
        "# Local vectors\n\nlocalvectorneedle is indexed and searchable.\n",
    )
    .unwrap();
    fixture.json_ok(&["init"]);
    fixture.json_ok(&["ledger", "init"]);
    let before = ledger_artifacts(&fixture);
    for args in [
        vec!["index", "--yes", "--offline"],
        vec![
            "search",
            "localvectorneedle",
            "--mode",
            "vector",
            "--offline",
        ],
    ] {
        let output = fixture
            .command(&args)
            .env_remove("KIO_TEST_GEMINI_EMBED")
            .env("KIO_EVAL_DETERMINISTIC_EMBED", "scale-v3")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        if args[0] == "search" {
            assert_eq!(result["resolved_mode"], "vector", "{result}");
            assert!(
                !result["results"].as_array().unwrap().is_empty(),
                "{result}"
            );
        }
        assert_eq!(
            ledger_artifacts(&fixture),
            before,
            "{args:?} mutated billing state"
        );
    }
}
