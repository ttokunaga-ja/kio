//! Local embedding transport must honor `index --offline` even with a current
//! loopback trust root and explicit adapter grant. The positive control proves
//! this fixture reaches the real local TLS transport when offline is absent.

mod support;

use assert_cmd::Command;
use std::fs;
use std::net::TcpListener;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

// Public loopback-SAN test CA, copied from step5_local_ocr.rs. It is not a key.
const LOCAL_PEER_CA_PEM: &str = "-----BEGIN CERTIFICATE-----\nMIIBWTCB/6ADAgECAhR5P0J0YMFaZPlrOFyN8/jwpGzhqjAKBggqhkjOPQQDAjAh\nMR8wHQYDVQQDDBZyY2dlbiBzZWxmIHNpZ25lZCBjZXJ0MCAXDTc1MDEwMTAwMDAw\nMFoYDzQwOTYwMTAxMDAwMDAwWjAhMR8wHQYDVQQDDBZyY2dlbiBzZWxmIHNpZ25l\nZCBjZXJ0MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEGhIwEPQnvWSlH+iUQ5Ui\nq0khmBj4hlxmW+mTL5xjjyxwwopOnkxLs2AofasS2lYPdILzMaIeRE78g2S7Hz1n\nX6MTMBEwDwYDVR0RBAgwBocEfwAAATAKBggqhkjOPQQDAgNJADBGAiEA0GBG4uEL\nSJJea18R17+yhRTyn3rsD6PzDhdg7F/v2sQCIQCccgcgjDh+ABNajeb1deKZoqbx\nnP4qBrGe09azOI4jbg==\n-----END CERTIFICATE-----\n";

struct ClosingLoopback {
    port: u16,
    attempts: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl ClosingLoopback {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let attempts = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let attempts_worker = Arc::clone(&attempts);
        let stop_worker = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !stop_worker.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        attempts_worker.fetch_add(1, Ordering::SeqCst);
                        drop(stream); // TLS EOF: no server, no HTTP request body.
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("loopback listener failed: {error}"),
                }
            }
        });
        Self {
            port,
            attempts,
            stop,
            thread: Some(thread),
        }
    }

    fn count(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }

    fn wait_for_attempt(&self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.count() == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            self.count() > 0,
            "positive control must reach the loopback TLS transport"
        );
    }
}

impl Drop for ClosingLoopback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn kio(dir: &TempDir, args: &[&str]) -> Command {
    let mut command = Command::cargo_bin("kio").unwrap();
    command
        .current_dir(dir.path())
        .env_clear()
        .env("HOME", dir.path().join(".home"))
        .env("USERPROFILE", dir.path().join(".home"))
        .env("TMPDIR", dir.path().join(".tmp"))
        .env("TEMP", dir.path().join(".tmp"))
        .env("TMP", dir.path().join(".tmp"))
        .env("XDG_CONFIG_HOME", dir.path().join(".test-config"))
        .env("XDG_DATA_HOME", dir.path().join(".test-data"))
        .env("XDG_CACHE_HOME", dir.path().join(".test-cache"))
        .args(args);
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    command
}

fn setup(dir: &TempDir, peer: &ClosingLoopback) {
    #[cfg(unix)]
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir_all(dir.path().join(".home")).unwrap();
    fs::create_dir_all(dir.path().join(".tmp")).unwrap();
    let config = dir.path().join(".test-config/kio/tools.toml");
    #[cfg(windows)]
    {
        let config_root = dir.path().join(".test-config");
        if !config_root.exists() {
            support::create_private_child_dir(dir.path(), ".test-config");
        }
        if !config.parent().unwrap().exists() {
            support::create_private_child_dir(&config_root, "kio");
        }
    }
    #[cfg(not(windows))]
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, format!(
        "[embedding.qwen3_vl_embedding_local]\nkind = \"offline_api\"\nurl = \"https://127.0.0.1:{}\"\nmodel = \"Qwen/Qwen3-VL-Embedding-2B\"\n",
        peer.port,
    )).unwrap();
    #[cfg(windows)]
    let ca_path = dir.path().join(".test-config/kio/local-peer-ca.pem");
    #[cfg(not(windows))]
    let ca_path = dir.path().join("local-peer-ca.pem");
    #[cfg(windows)]
    support::write_private_fixture_file(
        ca_path.parent().unwrap(),
        "local-peer-ca.pem",
        LOCAL_PEER_CA_PEM.as_bytes(),
    );
    #[cfg(not(windows))]
    fs::write(&ca_path, LOCAL_PEER_CA_PEM).unwrap();
    #[cfg(unix)]
    fs::set_permissions(&ca_path, fs::Permissions::from_mode(0o600)).unwrap();
    kio(
        dir,
        &[
            "adapter",
            "trust",
            "register",
            "--ca-pem",
            ca_path.to_str().unwrap(),
            "--yes",
        ],
    )
    .assert()
    .success();
    fs::write(
        dir.path().join("plain.md"),
        "# Local vector test\n\nA plain Markdown chunk that needs an embedding.\n",
    )
    .unwrap();
    kio(dir, &["init"]).assert().success();
    kio(dir, &["ledger", "init"]).assert().success();
    kio(
        dir,
        &["adapter", "approve", "qwen3_vl_embedding_local", "--yes"],
    )
    .assert()
    .success();
}

fn embedding_rows(dir: &TempDir) -> (i64, i64) {
    let ledger = support::immutable_sqlite(dir.path().join(".test-data/kio/cost-ledger.sqlite"));
    (
        ledger
            .query_row(
                "SELECT COUNT(*) FROM batch_requests WHERE adapter_kind='embedding'",
                [],
                |row| row.get(0),
            )
            .unwrap(),
        ledger
            .query_row(
                "SELECT COUNT(*) FROM cost_ledger WHERE adapter_kind='embedding'",
                [],
                |row| row.get(0),
            )
            .unwrap(),
    )
}

fn source_embedding_count(dir: &TempDir) -> i64 {
    let source = support::immutable_sqlite(dir.path().join(".kio/index/sqlite.db"));
    source
        .query_row("SELECT COUNT(*) FROM embeddings", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn offline_index_never_contacts_approved_local_embedding_peer() {
    let dir = support::canonical_tempdir();
    let peer = ClosingLoopback::start();
    setup(&dir, &peer);

    let offline = kio(&dir, &["--json", "index", "--offline"])
        .output()
        .unwrap();
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        peer.count(),
        0,
        "--offline must not attempt local TLS: status={} stdout={} stderr={}",
        offline.status,
        String::from_utf8_lossy(&offline.stdout),
        String::from_utf8_lossy(&offline.stderr),
    );
    assert_eq!(
        offline.status.code(),
        Some(0),
        "offline index must complete: stdout={} stderr={}",
        String::from_utf8_lossy(&offline.stdout),
        String::from_utf8_lossy(&offline.stderr),
    );
    assert_eq!(
        embedding_rows(&dir),
        (0, 0),
        "--offline must not reserve or charge an embedding"
    );
    assert_eq!(
        source_embedding_count(&dir),
        0,
        "--offline must not publish a vector"
    );

    // The same trust root, grant and document are a positive transport control.
    let normal = kio(&dir, &["--json", "index"]).output().unwrap();
    peer.wait_for_attempt();
    assert!(
        normal.status.success() || [3, 4, 5, 6].contains(&normal.status.code().unwrap_or(-1)),
        "unexpected local TLS EOF outcome: stdout={} stderr={}",
        String::from_utf8_lossy(&normal.stdout),
        String::from_utf8_lossy(&normal.stderr),
    );
    let ledger = support::immutable_sqlite(dir.path().join(".test-data/kio/cost-ledger.sqlite"));
    let (outcome, usd, estimated, state, null_handle): (String, f64, bool, i64, bool) = ledger.query_row(
        "SELECT l.outcome, l.usd, l.estimated, b.state, b.batch_job_id IS NULL FROM cost_ledger l JOIN batch_requests b USING (scope_id,adapter_kind,input_hash,tool_profile_hash,submission_seq) WHERE l.adapter_kind='embedding'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
    ).unwrap();
    assert_ne!(outcome, "succeeded", "TLS EOF cannot be a successful send");
    assert_ne!(
        outcome, "unknown_settled",
        "local TLS EOF has no paid remote uncertainty"
    );
    assert_eq!(usd, 0.0);
    assert!(
        !estimated,
        "local confirmed-zero failure is not an estimate"
    );
    assert_eq!(
        state, 3,
        "local transport failure must reach terminal state"
    );
    assert!(null_handle, "local execution has no remote job handle");
    let tasks = fs::read_to_string(dir.path().join(".kio/tasks.jsonl")).unwrap();
    let embedding = tasks
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .rfind(|task| task["type"] == "embedding")
        .expect("local attempt must have a typed embedding task");
    assert_eq!(embedding["status"], "failed", "{embedding}");
    assert_ne!(
        embedding["fallback_reason"], "result_unknown",
        "{embedding}"
    );
    assert!(
        embedding["fallback_reason"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty()),
        "failed task needs a concrete reason: {embedding}"
    );
    assert_eq!(source_embedding_count(&dir), 0);
}
