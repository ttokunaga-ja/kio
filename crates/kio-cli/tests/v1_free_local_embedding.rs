//! APP-01 / docs/04 §5.4: free document/image/query work is independent of billing.
#![cfg(debug_assertions)]

mod support;

use assert_cmd::Command;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;
use tempfile::TempDir;

struct Fixture {
    _root: TempDir,
    scope: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new(initialized: bool) -> Self {
        let root = support::canonical_tempdir();
        let scope = support::create_private_child_dir(root.path(), "scope");
        let home = support::create_private_child_dir(root.path(), "home");
        for leaf in ["data", "config", "cache", "state", "tmp"] {
            support::create_private_child_dir(&home, leaf);
        }
        let fixture = Self {
            _root: root,
            scope,
            home,
        };
        fs::write(
            fixture.scope.join("one.md"),
            "# Local\n\nlocalvectorneedle\n",
        )
        .unwrap();
        // Existing native ingest + deterministic image fixture; no OCR/mock provider.
        fs::write(
            fixture.scope.join("golden-original.png"),
            include_bytes!("fixtures/o2_golden/png-original-control.png"),
        )
        .unwrap();
        fixture.json(&["init"]);
        if initialized {
            fixture.json(&["ledger", "init"]);
        }
        let config = fixture.home.join("config/kio");
        if !config.exists() {
            support::create_private_child_dir(&fixture.home.join("config"), "kio");
        }
        let bytes = b"[budget]\nmonthly_usd_cap = 0.0\nhard_stop = true\n[budget.per_adapter]\nembedding = 0.0\n";
        #[cfg(windows)]
        support::write_private_fixture_file(&config, "config.toml", bytes);
        #[cfg(not(windows))]
        fs::write(config.join("config.toml"), bytes).unwrap();
        fixture
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::cargo_bin("kio").unwrap();
        cmd.env_clear();
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            cmd.env("SystemRoot", system_root);
        }
        cmd.current_dir(&self.scope)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("XDG_DATA_HOME", self.home.join("data"))
            .env("XDG_CONFIG_HOME", self.home.join("config"))
            .env("XDG_CACHE_HOME", self.home.join("cache"))
            .env("XDG_STATE_HOME", self.home.join("state"))
            .env("TMPDIR", self.home.join("tmp"))
            .env("TEMP", self.home.join("tmp"))
            .env("TMP", self.home.join("tmp"))
            .env("KIO_FIXED_NOW", "2026-09-08T00:00:00Z")
            .env("KIO_TEST_MARKDOWNIZE_ADAPTER", "deterministic")
            .env("KIO_EVAL_DETERMINISTIC_EMBED", "scale-v3")
            .env("KIO_TEST_QUERY_EMBED_TRACE", self.home.join("query.trace"))
            .args(args)
            .arg("--json");
        cmd
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.command(args).output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: status={:?} stdout={} stderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn ledger(&self) -> PathBuf {
        self.home.join("data/kio/cost-ledger.sqlite")
    }
}

// No SQLite opener: bytes/mtime and the exact artifact name set remain observable
// even when the main database is deliberately malformed or the lifecycle is busy.
#[derive(Debug, PartialEq, Eq)]
struct Leaf {
    bytes: Vec<u8>,
    modified: SystemTime,
}
fn billing(f: &Fixture) -> BTreeMap<String, Leaf> {
    let root = f.home.join("data/kio");
    if !root.exists() {
        return BTreeMap::new();
    }
    fs::read_dir(root)
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            if !(name.starts_with("cost-ledger.sqlite") || name == "ledger.lifecycle.lock") {
                return None;
            }
            let meta = fs::symlink_metadata(entry.path()).unwrap();
            assert!(meta.is_file() && !meta.file_type().is_symlink(), "{name}");
            Some((
                name,
                Leaf {
                    bytes: fs::read(entry.path()).unwrap(),
                    modified: meta.modified().unwrap(),
                },
            ))
        })
        .collect()
}

#[derive(PartialEq)]
struct Observed {
    // Compare exact vectors only where the effective input is identical.
    // Generated image Markdown contains a scope URI and is checked against
    // its actual adapter input before its scope ID is normalized here.
    vectors: Vec<(String, String, Vec<f32>)>,
    generated_image_inputs: Vec<String>,
    document_hits: Vec<(String, String)>,
    image_hits: Vec<(String, String)>,
}
impl std::fmt::Debug for Observed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let vectors = self
            .vectors
            .iter()
            .map(|(kind, target, vector)| {
                let bytes = vector
                    .iter()
                    .flat_map(|value| value.to_le_bytes())
                    .collect::<Vec<_>>();
                (
                    kind,
                    target,
                    vector.len(),
                    kio_pipeline::prepare::hash_bytes(&bytes),
                )
            })
            .collect::<Vec<_>>();
        formatter
            .debug_struct("Observed")
            .field("vectors", &vectors)
            .field("generated_image_inputs", &self.generated_image_inputs)
            .field("document_hits", &self.document_hits)
            .field("image_hits", &self.image_hits)
            .finish()
    }
}

fn hits(payload: &Value) -> Vec<(String, String)> {
    let mut rows = payload["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["result_type"].as_str().unwrap().to_owned(),
                row["evidence_pointer"]["path_at_commit"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            )
        })
        .collect::<Vec<_>>();
    rows.sort();
    rows
}
fn assert_deterministic_input_vector(input: &str, profile_hash: &str, actual: &[f32]) {
    use kio_adapter::types::{EmbeddingInputType, EmbeddingItem, EmbeddingMemberResult};
    let expected = kio_adapter::catalog::run_embedding(
        std::sync::Arc::new(kio_adapter::tool_lock::AdapterRuntimeSettings::default()),
        kio_adapter::catalog::EmbeddingExecution::DeterministicEvaluator,
        vec![EmbeddingItem::text("oracle", input)],
        EmbeddingInputType::MarkdownChunk,
        None,
    )
    .unwrap()
    .into_completed()
    .unwrap();
    assert_eq!(expected.embedding_profile_hash, profile_hash);
    let EmbeddingMemberResult::Vector { vector } = &expected.members[0].result else {
        panic!("real deterministic adapter must complete");
    };
    assert_eq!(vector.len(), actual.len());
    let dot: f64 = vector
        .iter()
        .zip(actual)
        .map(|(a, b)| f64::from(*a) * f64::from(*b))
        .sum();
    let expected_norm = vector
        .iter()
        .map(|v| f64::from(*v).powi(2))
        .sum::<f64>()
        .sqrt();
    let actual_norm = actual
        .iter()
        .map(|v| f64::from(*v).powi(2))
        .sum::<f64>()
        .sqrt();
    assert!(
        dot / (expected_norm * actual_norm) > 1.0 - 1e-7,
        "published scope-bound chunk must match the real adapter on its actual input"
    );
}

fn execute(f: &Fixture) -> Observed {
    f.json(&["index", "--offline"]);
    let source = support::immutable_sqlite(f.scope.join(".kio/index/sqlite.db"));
    for target in ["chunk", "image"] {
        let count: i64 = source
            .query_row(
                "SELECT COUNT(*) FROM embeddings WHERE target_type=?1",
                [target],
                |row| row.get(0),
            )
            .unwrap();
        assert!(count > 0, "actual source must contain {target} vectors");
    }
    let mut statement = source
        .prepare("SELECT text_hash, text, raw_path FROM chunks")
        .unwrap();
    let chunks = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                (row.get::<_, String>(1)?, row.get::<_, String>(2)?),
            ))
        })
        .unwrap()
        .collect::<std::result::Result<BTreeMap<_, _>, _>>()
        .unwrap();
    drop(statement);
    drop(source);
    let scope: Value =
        serde_json::from_slice(&fs::read(f.scope.join(".kio/scope.json")).unwrap()).unwrap();
    let scope_id = scope["scope_id"].as_str().unwrap();
    let mut generated_image_inputs = Vec::new();
    let store = kio_core::cas::ObjectStore::new(f.scope.join(".kio"));
    let mut vectors = store
        .embedding_hashes()
        .unwrap()
        .into_iter()
        .filter_map(|hash| {
            let object = store.read_embedding(&hash).unwrap();
            assert_eq!(object.vector.len() as u64, object.dimensions);
            assert!(object.vector.iter().all(|x| x.is_finite()));
            let norm: f32 = object.vector.iter().map(|x| x * x).sum();
            assert!(
                (norm - 1.0).abs() < 0.001,
                "actual vector must be normalized: {norm}"
            );
            if object.target_type == "chunk" {
                let (text, path) = chunks
                    .get(&object.target_hash)
                    .expect("every published chunk vector needs its exact source text hash");
                assert_eq!(
                    object.target_hash,
                    kio_pipeline::prepare::hash_bytes(text.as_bytes())
                );
                let context = kio_index::embedding_store::chunk_embedding_context(path);
                if path == "golden-original.png" {
                    let uri = format!("kio://{scope_id}/object/image/");
                    assert!(
                        text.contains(&uri),
                        "generated image Markdown must bind this actual scope"
                    );
                    let input = kio_index::embedding_store::contextualized_embedding_input(
                        context.as_deref(),
                        text,
                    );
                    assert_deterministic_input_vector(&input, &object.profile_hash, &object.vector);
                    generated_image_inputs.push(input.replace(scope_id, "<fixture-scope>"));
                    return None;
                }
                assert_eq!(
                    path, "one.md",
                    "all other chunk inputs must be the fixed text fixture"
                );
            }
            Some((object.target_type, object.target_hash, object.vector))
        })
        .collect::<Vec<_>>();
    vectors.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    assert!(vectors.iter().any(|v| v.0 == "chunk"));
    assert!(vectors.iter().any(|v| v.0 == "image"));
    let document = f.json(&[
        "search",
        "localvectorneedle",
        "--mode",
        "vector",
        "--offline",
        "--limit",
        "20",
    ]);
    assert_eq!(document["resolved_mode"], "vector", "{document}");
    let image = f.json(&[
        "search",
        "localvectorneedle",
        "--mode",
        "image",
        "--offline",
        "--limit",
        "20",
    ]);
    assert_eq!(image["resolved_mode"], "image", "{image}");
    let document_hits = hits(&document);
    let image_hits = hits(&image);
    assert!(
        document_hits.iter().any(|(_, path)| path == "one.md"),
        "{document}"
    );
    assert!(
        image_hits
            .iter()
            .any(|(kind, path)| kind == "image" && path == "golden-original.png"),
        "{image}"
    );
    assert_eq!(
        fs::read_to_string(f.home.join("query.trace"))
            .unwrap()
            .lines()
            .count(),
        2,
        "fresh vector and image searches must both execute the existing query seam"
    );
    generated_image_inputs.sort();
    assert_eq!(
        generated_image_inputs.len(),
        1,
        "the image-derived chunk is validated, not omitted"
    );
    Observed {
        vectors,
        generated_image_inputs,
        document_hits,
        image_hits,
    }
}

/// Fixture prerequisite only: current initialized-ledger fixture must produce
/// real source/CAS vectors and document/image hits before APP-01 comparison.
#[test]
fn app01_docs04_5_4_initialized_fixture_has_real_document_image_and_query_outputs() {
    let _env = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    execute(&Fixture::new(true));
}

/// APP-01: absent billing authority cannot hold free document/image/query work.
#[test]
fn app01_docs04_5_4_absent_ledger_preserves_free_document_image_query_outputs() {
    let _env = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let control = execute(&Fixture::new(true));
    let f = Fixture::new(false);
    let before = billing(&f);
    assert!(
        before.is_empty(),
        "fixture must start without billing artifacts"
    );
    let actual = execute(&f);
    assert_eq!(actual, control);
    assert_eq!(
        billing(&f),
        before,
        "free operations must not create billing artifacts"
    );
}

/// APP-01: malformed existing billing bytes are retained and cannot block free work.
#[test]
fn app01_docs04_5_4_corrupt_ledger_preserves_free_outputs_and_billing_bytes() {
    let _env = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let control = execute(&Fixture::new(true));
    let f = Fixture::new(true);
    fs::write(
        f.ledger(),
        b"APP01 deliberately malformed billing database\n",
    )
    .unwrap();
    let before = billing(&f);
    let actual = execute(&f);
    assert_eq!(actual, control);
    assert_eq!(
        billing(&f),
        before,
        "free work must not repair or mutate billing artifacts"
    );
}

/// APP-01: hold the actual lifecycle inode across free CLI subprocesses.
#[test]
fn app01_docs04_5_4_busy_ledger_preserves_free_outputs_and_billing_bytes() {
    let _env = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let control = execute(&Fixture::new(true));
    let f = Fixture::new(true);
    let before = billing(&f);
    let path = f.ledger().parent().unwrap().join("ledger.lifecycle.lock");
    let held = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    held.try_lock().unwrap();
    let contender = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    assert!(
        matches!(contender.try_lock(), Err(fs::TryLockError::WouldBlock)),
        "fixture prerequisite: independent handle must observe actual lock contention"
    );
    let actual = execute(&f);
    assert_eq!(actual, control);
    // Windows denies data reads of the locked range through another handle.
    // Release only after every subject subprocess has completed, then observe.
    drop(contender);
    drop(held);
    assert_eq!(billing(&f), before);
}

/// A completed query is retired, so repeated fresh calls never become a cache
/// or exhaust a retry budget. Device-local records retain only its digest.
#[test]
fn app01_fresh_query_retirement_and_state_never_store_query_plaintext() {
    let _env = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let f = Fixture::new(false);
    f.json(&["index", "--offline"]);
    let before = billing(&f);
    let query = "APP01_private_query_canary_9e93be765";
    for _ in 0..8 {
        let result = f.json(&["search", query, "--mode", "vector", "--offline"]);
        assert_eq!(result["resolved_mode"], "vector", "{result}");
    }
    assert_eq!(
        fs::read_to_string(f.home.join("query.trace"))
            .unwrap()
            .lines()
            .count(),
        8
    );
    let state = f.home.join("state/kio/local-execution");
    assert!(
        state.is_dir(),
        "actual free jobs need durable local records"
    );
    let mut records = 0;
    let mut entries = 0_usize;
    let mut total_bytes = 0_u64;
    let mut pending = vec![(state, 0_usize)];
    while let Some((directory, depth)) = pending.pop() {
        assert!(depth <= 16, "bounded local-state fixture directory depth");
        let metadata = fs::symlink_metadata(&directory).unwrap();
        assert!(metadata.is_dir() && !metadata.file_type().is_symlink());
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            entries += 1;
            assert!(entries <= 4096, "bounded local-state fixture entry count");
            let metadata = fs::symlink_metadata(entry.path()).unwrap();
            assert!(
                !metadata.file_type().is_symlink(),
                "local state contains a symlink"
            );
            if metadata.is_dir() {
                pending.push((entry.path(), depth + 1));
                continue;
            }
            assert!(metadata.is_file(), "local state contains a non-file leaf");
            total_bytes = total_bytes.checked_add(metadata.len()).unwrap();
            assert!(
                total_bytes <= 64 * 1024 * 1024,
                "bounded local-state fixture bytes"
            );
            let bytes = fs::read(entry.path()).unwrap();
            assert_eq!(bytes.len() as u64, metadata.len());
            assert!(
                !bytes
                    .windows(query.len())
                    .any(|part| part == query.as_bytes()),
                "query plaintext persisted in {}",
                entry.path().display()
            );
            if entry.file_name() == "record.json" {
                records += 1;
            }
        }
    }
    assert!(records > 0);
    assert_eq!(billing(&f), before);
}

fn local_result_record(f: &Fixture) -> PathBuf {
    let root = f.home.join("state/kio/local-execution");
    let records = fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().path().join("record.json"))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    assert_eq!(
        records.len(),
        1,
        "first fault must leave exactly one actual free intent"
    );
    let record: Value = serde_json::from_slice(&fs::read(&records[0]).unwrap()).unwrap();
    assert_eq!(record["phase"]["phase"], "result_ready");
    records[0].clone()
}
fn traced_kind(f: &Fixture, kind: &str) -> usize {
    fs::read_to_string(f.home.join("adapter.trace"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|entry| entry["input_type"] == kind)
        .count()
}
fn interrupt_local_result(f: &Fixture, kind: &str) -> PathBuf {
    if kind == "chunk" {
        fs::remove_file(f.scope.join("golden-original.png")).unwrap();
    }
    let first = f
        .command(&["index", "--offline"])
        .env("KIO_TEST_TASK_EMBED_TRACE", f.home.join("adapter.trace"))
        .env(
            "KIO_TEST_EMBEDDING_PUBLICATION_FAULT",
            "before_source_write",
        )
        .output()
        .unwrap();
    assert!(
        !first.status.success(),
        "fixture must stop before publication"
    );
    assert_eq!(
        traced_kind(f, if kind == "chunk" { "text" } else { "image" }),
        1,
        "fault fixture must have one actual validated adapter completion"
    );
    let record = local_result_record(f);
    let source = support::immutable_sqlite(f.scope.join(".kio/index/sqlite.db"));
    let count: i64 = source
        .query_row(
            "SELECT COUNT(*) FROM embeddings WHERE target_type=?1",
            [kind],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "validated result has not reached the source");
    assert!(
        kio_core::cas::ObjectStore::new(f.scope.join(".kio"))
            .embedding_hashes()
            .unwrap()
            .is_empty()
    );
    record
}

#[test]
fn app01_document_and_image_result_ready_recover_in_new_process_without_resend() {
    let _env = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    for kind in ["chunk", "image"] {
        let f = Fixture::new(false);
        let before = billing(&f);
        interrupt_local_result(&f, kind);
        let recovered = f
            .command(&["index", "--offline"])
            .env("KIO_TEST_TASK_EMBED_TRACE", f.home.join("adapter.trace"))
            .output()
            .unwrap();
        assert!(
            recovered.status.success(),
            "{kind}: {} {}",
            String::from_utf8_lossy(&recovered.stdout),
            String::from_utf8_lossy(&recovered.stderr)
        );
        assert_eq!(
            traced_kind(&f, if kind == "chunk" { "text" } else { "image" }),
            1,
            "known local result must not be dispatched again"
        );
        let source = support::immutable_sqlite(f.scope.join(".kio/index/sqlite.db"));
        let count: i64 = source
            .query_row(
                "SELECT COUNT(*) FROM embeddings WHERE target_type=?1",
                [kind],
                |row| row.get(0),
            )
            .unwrap();
        assert!(count > 0, "known {kind} vector must publish");
        assert_eq!(billing(&f), before);
    }
}

#[test]
fn app01_tampered_local_result_or_record_never_publishes_or_resends() {
    let _env = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    for tamper in ["bytes", "profile", "member", "unknown_field"] {
        let f = Fixture::new(false);
        let record_path = interrupt_local_result(&f, "chunk");
        if tamper == "bytes" {
            fs::write(
                record_path.parent().unwrap().join("result.bin"),
                b"tampered local completion",
            )
            .unwrap();
        } else {
            let mut record: Value =
                serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
            match tamper {
                "profile" => record["identity"]["profile"] = Value::String("wrong-profile".into()),
                "member" => {
                    record["identity"]["contract"]["members"][0]["id"] =
                        Value::String("wrong-member".into())
                }
                _ => record["unexpected"] = Value::Bool(true),
            }
            fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();
        }
        let bad_record = fs::read(&record_path).unwrap();
        let failed = f
            .command(&["index", "--offline"])
            .env("KIO_TEST_TASK_EMBED_TRACE", f.home.join("adapter.trace"))
            .output()
            .unwrap();
        assert!(!failed.status.success(), "{tamper} must fail closed");
        assert_eq!(traced_kind(&f, "text"), 1, "{tamper} must not resend");
        let source = support::immutable_sqlite(f.scope.join(".kio/index/sqlite.db"));
        let count: i64 = source
            .query_row("SELECT COUNT(*) FROM embeddings", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "{tamper} must not publish");
        assert_eq!(
            fs::read(&record_path).unwrap(),
            bad_record,
            "corrupt state must not be repaired"
        );
        assert!(billing(&f).is_empty());
    }
}

// Reuses the authenticated loopback fixture construction from
// kio-adapter/src/local_peer.rs, with a response barrier for real CLI callers.
#[derive(Debug, Default)]
struct QueryPeerDiagnostics {
    accepted: usize,
    requests: usize,
    read_errors: Vec<String>,
    response_errors: Vec<String>,
}
struct QueryPeer {
    port: u16,
    pem: String,
    seen: std::sync::mpsc::Receiver<()>,
    release: std::sync::Arc<std::sync::atomic::AtomicBool>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    queries: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    diagnostics: std::sync::Arc<std::sync::Mutex<QueryPeerDiagnostics>>,
    task: Option<std::thread::JoinHandle<()>>,
}
fn decode_fixture_chunks(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut rest = bytes;
    let mut decoded = Vec::new();
    loop {
        let end = rest.windows(2).position(|part| part == b"\r\n")?;
        let size = std::str::from_utf8(&rest[..end]).ok()?.split(';').next()?;
        let size = usize::from_str_radix(size, 16).ok()?;
        rest = &rest[end + 2..];
        if size == 0 {
            return rest.starts_with(b"\r\n").then_some(decoded);
        }
        if rest.len() < size + 2 {
            return None;
        }
        assert_eq!(&rest[size..size + 2], b"\r\n");
        assert!(decoded.len() + size < 2 * 1024 * 1024);
        decoded.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

impl QueryPeer {
    fn new(query: &'static str) -> Self {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        };
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
        let certificate = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der());
        let config = Arc::new(
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![certificate], key.into())
                .unwrap(),
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let release = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let queries = Arc::new(AtomicUsize::new(0));
        let diagnostics = Arc::new(std::sync::Mutex::new(QueryPeerDiagnostics::default()));
        let worker_diagnostics = diagnostics.clone();
        let (send, seen) = std::sync::mpsc::channel();
        let (worker_release, worker_stop, worker_queries) =
            (release.clone(), stop.clone(), queries.clone());
        let task = std::thread::spawn(move || {
            let mut workers = Vec::new();
            while !worker_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        worker_diagnostics.lock().unwrap().accepted += 1;
                        let diagnostics = worker_diagnostics.clone();
                        let (config, release, stop, queries, send) = (
                            config.clone(),
                            worker_release.clone(),
                            worker_stop.clone(),
                            worker_queries.clone(),
                            send.clone(),
                        );
                        workers.push(std::thread::spawn(move || {
                            use std::io::{Read, Write};
                            // Match http_policy::test_tls: accepted BSD sockets may
                            // inherit the listener's nonblocking mode. Rustls' blocking
                            // StreamOwned needs a blocking socket with finite timeouts.
                            stream.set_nonblocking(false).unwrap();
                            stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
                            stream.set_write_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
                            let connection = rustls::ServerConnection::new(config).unwrap();
                            let mut stream = rustls::StreamOwned::new(connection, stream);
                            let mut request = Vec::new();
                            let body = loop {
                                let mut chunk = [0_u8; 4096];
                                let n = match stream.read(&mut chunk) {
                                    Ok(n) => n,
                                    Err(error) => {
                                        let mut diagnostics = diagnostics.lock().unwrap();
                                        assert!(diagnostics.read_errors.len() < 64, "bounded fixture read diagnostics");
                                        diagnostics.read_errors.push(format!("bytes={} kind={:?} error={error:?}", request.len(), error.kind()));
                                        return;
                                    }
                                };
                                if n == 0 {
                                    let mut diagnostics = diagnostics.lock().unwrap();
                                    assert!(diagnostics.read_errors.len() < 64, "bounded fixture read diagnostics");
                                    diagnostics.read_errors.push(format!("EOF before complete request: bytes={}", request.len()));
                                    return;
                                }
                                request.extend_from_slice(&chunk[..n]);
                                assert!(request.len() < 2 * 1024 * 1024, "bounded fixture request");
                                if let Some(end) = request.windows(4).position(|b| b == b"\r\n\r\n") {
                                    let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                                    if let Some(length) = headers.lines().find_map(|line| line.strip_prefix("content-length:")) {
                                        let length: usize = length.trim().parse().unwrap();
                                        if request.len() >= end + 4 + length { break request[end + 4..end + 4 + length].to_vec(); }
                                    } else if headers.contains("transfer-encoding: chunked") {
                                        if let Some(body) = decode_fixture_chunks(&request[end + 4..]) { break body; }
                                    } else { panic!("fixture request needs explicit body framing"); }
                                }
                            };
                            let _: Value = serde_json::from_slice(&body).expect("actual local embedding request must be JSON");
                            diagnostics.lock().unwrap().requests += 1;
                            if body.windows(query.len()).any(|part| part == query.as_bytes()) {
                                queries.fetch_add(1, Ordering::SeqCst);
                                send.send(()).unwrap();
                                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
                                while !release.load(Ordering::SeqCst) && !stop.load(Ordering::SeqCst) {
                                    assert!(std::time::Instant::now() < deadline, "query fixture barrier timed out");
                                    std::thread::sleep(std::time::Duration::from_millis(5));
                                }
                            }
                            let mut vector = vec![0.0_f32; 768]; vector[0] = 1.0;
                            let body = serde_json::json!({"data":[{"embedding":vector}]}).to_string();
                            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                            if let Err(error) = stream.write_all(response.as_bytes()).and_then(|()| stream.flush()) {
                                let mut diagnostics = diagnostics.lock().unwrap();
                                assert!(diagnostics.response_errors.len() < 64, "bounded fixture response diagnostics");
                                diagnostics.response_errors.push(format!("kind={:?} error={error:?}", error.kind()));
                            }
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5))
                    }
                    Err(error) => panic!("local TLS accept failed: {error}"),
                }
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        Self {
            port,
            pem: cert.pem(),
            seen,
            release,
            stop,
            queries,
            diagnostics,
            task: Some(task),
        }
    }
    fn diagnostics(&self) -> String {
        format!("{:?}", self.diagnostics.lock().unwrap())
    }
}
impl Drop for QueryPeer {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        self.release.store(true, Ordering::SeqCst);
        self.stop.store(true, Ordering::SeqCst);
        let joined = self.task.take().unwrap().join();
        if !std::thread::panicking() {
            joined.expect("local TLS fixture worker failed");
        }
    }
}

struct LocalQueryFixture {
    _root: TempDir,
    home: PathBuf,
    scopes: Vec<PathBuf>,
}
impl LocalQueryFixture {
    fn command(&self, scope: usize, args: &[&str]) -> std::process::Command {
        let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("kio"));
        command.env_clear().current_dir(&self.scopes[scope]);
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", system_root);
        }
        command
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("XDG_DATA_HOME", self.home.join("data"))
            .env("XDG_CONFIG_HOME", self.home.join("config"))
            .env("XDG_CACHE_HOME", self.home.join("cache"))
            .env("XDG_STATE_HOME", self.home.join("state"))
            .env("TMPDIR", self.home.join("tmp"))
            .env("TEMP", self.home.join("tmp"))
            .env("TMP", self.home.join("tmp"))
            .env("KIO_TEST_MARKDOWNIZE_ADAPTER", "deterministic")
            .env("KIO_FIXED_NOW", "2026-09-08T00:00:00Z")
            .env("KIO_TEST_QUERY_EMBED_TRACE", self.home.join("query.trace"))
            .args(args)
            .arg("--json");
        if args.first() == Some(&"search") {
            command.arg("--scope").arg(&self.scopes[scope]);
        }
        command
    }
    fn json(&self, scope: usize, args: &[&str]) -> Value {
        let out = self.command(scope, args).output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {} {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn assert_no_billing_artifacts(&self) {
        let root = self.home.join("data/kio");
        for entry in fs::read_dir(root).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            assert!(
                !name.starts_with("cost-ledger.sqlite") && name != "ledger.lifecycle.lock",
                "free loopback must not create billing artifact {name}"
            );
        }
    }
    fn new(peer: &QueryPeer) -> Self {
        let root = support::canonical_tempdir();
        let home = support::create_private_child_dir(root.path(), "home");
        for leaf in ["data", "config", "cache", "state", "tmp"] {
            support::create_private_child_dir(&home, leaf);
        }
        let config = support::create_private_child_dir(&home.join("config"), "kio");
        let tools = format!(
            "[embedding.qwen3_vl_embedding_local]\nkind = \"offline_api\"\nurl = \"https://127.0.0.1:{}\"\nmodel = \"Qwen/Qwen3-VL-Embedding-2B\"\n",
            peer.port
        );
        #[cfg(windows)]
        {
            support::write_private_fixture_file(&config, "tools.toml", tools.as_bytes());
            support::write_private_fixture_file(&config, "peer-ca.pem", peer.pem.as_bytes());
        }
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::write(config.join("tools.toml"), tools).unwrap();
            fs::write(config.join("peer-ca.pem"), &peer.pem).unwrap();
            fs::set_permissions(
                config.join("peer-ca.pem"),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
        let scopes = vec![
            support::create_private_child_dir(root.path(), "scope1"),
            support::create_private_child_dir(root.path(), "scope2"),
        ];
        let fixture = Self {
            _root: root,
            home,
            scopes,
        };
        fixture.json(
            0,
            &[
                "adapter",
                "trust",
                "register",
                "--ca-pem",
                config.join("peer-ca.pem").to_str().unwrap(),
                "--yes",
            ],
        );
        for scope in 0..2 {
            fs::write(
                fixture.scopes[scope].join("note.md"),
                "# Local\n\nsearchable local query fixture\n",
            )
            .unwrap();
            fixture.json(scope, &["init", "--independent", "--yes"]);
            fixture.json(
                scope,
                &["adapter", "approve", "qwen3_vl_embedding_local", "--yes"],
            );
            let indexed = fixture.command(scope, &["index"]).output().unwrap();
            assert!(
                indexed.status.success(),
                "local document fixture index failed: status={} stdout={} stderr={}",
                indexed.status,
                String::from_utf8_lossy(&indexed.stdout),
                String::from_utf8_lossy(&indexed.stderr),
            );
            let _: Value = serde_json::from_slice(&indexed.stdout).unwrap();
            let source =
                support::immutable_sqlite(fixture.scopes[scope].join(".kio/index/sqlite.db"));
            let count: i64 = source
                .query_row(
                    "SELECT COUNT(*) FROM embeddings WHERE target_type='chunk'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(
                count > 0,
                "trusted loopback fixture must complete actual local document embeddings before query faults: stdout={} stderr={} tasks={} peer={}",
                String::from_utf8_lossy(&indexed.stdout),
                String::from_utf8_lossy(&indexed.stderr),
                fs::read_to_string(fixture.scopes[scope].join(".kio/tasks.jsonl")).unwrap(),
                peer.diagnostics(),
            );
        }
        fixture
    }
}

struct QueryChild(Option<std::process::Child>);
impl Drop for QueryChild {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn app01_two_process_queries_share_one_live_local_claim_without_billing() {
    use std::sync::atomic::Ordering;
    let _env = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    const QUERY: &str = "APP01_concurrent_private_query_ef7232";
    let peer = QueryPeer::new(QUERY);
    let fixture = LocalQueryFixture::new(&peer);
    // Distinct scope writer leases isolate the query ownership assertion from
    // ordinary contention on a single scope's writer lock.
    let mut first = QueryChild(Some(
        fixture
            .command(0, &["search", QUERY, "--mode", "vector"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    peer.seen
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("first real local query must reach the TLS response barrier");
    let second = fixture
        .command(1, &["search", QUERY, "--mode", "hybrid"])
        .output()
        .unwrap();
    let second_text = format!(
        "{}{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        second_text.contains("embedding_in_flight"),
        "contender must report its live claim: {second_text}"
    );
    assert_eq!(
        peer.queries.load(Ordering::SeqCst),
        1,
        "contender must not reach the local adapter"
    );
    assert_eq!(
        fs::read_to_string(fixture.home.join("query.trace"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    peer.release.store(true, Ordering::SeqCst);
    let first = first.0.take().unwrap().wait_with_output().unwrap();
    assert!(
        first.status.success(),
        "owner must return its real vector: {} {}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    let result: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(result["resolved_mode"], "vector");
    fixture.assert_no_billing_artifacts();
}

#[test]
fn app01_killed_query_owner_preserves_real_attempt_and_retry_deadline() {
    use std::sync::atomic::Ordering;
    let _env = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    const QUERY: &str = "APP01_killed_query_owner_901ae2";
    let peer = QueryPeer::new(QUERY);
    let fixture = LocalQueryFixture::new(&peer);
    let mut owner = QueryChild(Some(
        fixture
            .command(0, &["search", QUERY, "--mode", "vector"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    peer.seen
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("actual owner must reach the TLS completion barrier");
    let child = owner.0.as_mut().unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    owner.0.take();
    let record_path = fs::read_dir(fixture.home.join("state/kio/local-execution"))
        .unwrap()
        .map(|entry| entry.unwrap().path().join("record.json"))
        .find(|path| {
            let value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
            value["identity"]["target"]["kind"] == "query"
        })
        .expect("actual query intent must be durable");
    let intent: Value = serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
    assert_eq!(intent["attempts"], 1);
    assert_eq!(intent["phase"]["phase"], "intent");
    let _early = fixture
        .command(1, &["search", QUERY, "--mode", "hybrid"])
        .output()
        .unwrap();
    assert_eq!(
        peer.queries.load(Ordering::SeqCst),
        1,
        "a killed owner's live attempt is not silently fresh work"
    );
    assert_eq!(
        fs::read_to_string(fixture.home.join("query.trace"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let backoff: Value = serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
    assert_eq!(backoff["attempts"], 1);
    assert_eq!(backoff["phase"]["phase"], "failed");
    assert_eq!(backoff["phase"]["kind"], "network_error");
    peer.release.store(true, Ordering::SeqCst);
    let resumed = fixture
        .command(1, &["search", QUERY, "--mode", "vector"])
        .env("KIO_FIXED_NOW", "2026-09-08T00:00:03Z")
        .output()
        .unwrap();
    assert!(
        resumed.status.success(),
        "bounded retry must complete: {} {}",
        String::from_utf8_lossy(&resumed.stdout),
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(peer.queries.load(Ordering::SeqCst), 2);
    let retired: Value = serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
    assert_eq!(retired["attempts"], 2);
    assert_eq!(retired["phase"]["phase"], "retired");
    fixture.assert_no_billing_artifacts();
}

/// The normal resume/repair callers publish an accepted local completion even
/// when billing cannot be acquired, without granting another adapter request.
#[test]
fn app01_default_resume_and_repair_publish_local_result_ready_without_billing() {
    let _env = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    for caller in [["batch", "resume"], ["repair", "rebuild-db"]] {
        for billing_state in ["absent", "corrupt", "busy"] {
            for kind in ["chunk", "image"] {
                let f = Fixture::new(billing_state != "absent");
                interrupt_local_result(&f, kind);
                if billing_state == "corrupt" {
                    fs::write(
                        f.ledger(),
                        b"APP01 accepted local completion billing canary\n",
                    )
                    .unwrap();
                }
                let before = billing(&f);
                let held = if billing_state == "busy" {
                    let path = f.ledger().parent().unwrap().join("ledger.lifecycle.lock");
                    let held = fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&path)
                        .unwrap();
                    held.try_lock().unwrap();
                    let contender = fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&path)
                        .unwrap();
                    assert!(matches!(
                        contender.try_lock(),
                        Err(fs::TryLockError::WouldBlock)
                    ));
                    drop(contender);
                    Some(held)
                } else {
                    None
                };
                let recovered = f
                    .command(&caller)
                    .env("KIO_TEST_TASK_EMBED_TRACE", f.home.join("adapter.trace"))
                    .output()
                    .unwrap();
                drop(held);
                assert!(
                    recovered.status.success(),
                    "{caller:?}/{billing_state}/{kind}: {} {}",
                    String::from_utf8_lossy(&recovered.stdout),
                    String::from_utf8_lossy(&recovered.stderr)
                );
                assert_eq!(
                    traced_kind(&f, if kind == "chunk" { "text" } else { "image" }),
                    1,
                    "{caller:?}/{billing_state}/{kind}: accepted result must never be resent"
                );
                let source = support::immutable_sqlite(f.scope.join(".kio/index/sqlite.db"));
                let count: i64 = source
                    .query_row(
                        "SELECT COUNT(*) FROM embeddings WHERE target_type=?1",
                        [kind],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert!(
                    count > 0,
                    "{caller:?}/{billing_state}/{kind}: accepted vector must publish"
                );
                assert_eq!(
                    billing(&f),
                    before,
                    "{caller:?}/{billing_state}/{kind}: billing authority must remain unchanged"
                );
            }
        }
    }
}

/// The local durable owner is the attempt authority for one fresh image send.
#[test]
fn app01_fresh_image_task_attempts_match_exact_local_owner_and_one_dispatch() {
    let _env = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let f = Fixture::new(false);
    let before = billing(&f);
    let out = f
        .command(&["index", "--offline"])
        .env("KIO_TEST_TASK_EMBED_TRACE", f.home.join("adapter.trace"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        traced_kind(&f, "image"),
        1,
        "one actual successful image dispatch"
    );
    let tasks = kio_pipeline::task::TaskStore::new(f.scope.join(".kio"))
        .all()
        .unwrap();
    let images: Vec<_> = tasks
        .iter()
        .filter(|task| {
            matches!(
                task.embedding_target,
                Some(kio_pipeline::task::EmbeddingTaskTarget::Image { .. })
            )
        })
        .collect();
    assert_eq!(images.len(), 1);
    let task = images[0];
    assert_eq!(task.status, kio_pipeline::task::TaskStatus::Done);
    let target = task.embedding_target.as_ref().unwrap();
    let kio_pipeline::task::EmbeddingTaskTarget::Image {
        image_hash,
        embedding_hash,
        profile_hash,
        ..
    } = target
    else {
        unreachable!()
    };
    let repo = kio_core::scope::Repository::open(&f.scope).unwrap();
    let vector = repo.object_store().read_embedding(embedding_hash).unwrap();
    assert_eq!(&vector.target_hash, image_hash);
    assert_eq!(&vector.profile_hash, profile_hash);
    let scope_id = repo.scope_identity().unwrap().scope_id;
    let records: Vec<Value> = fs::read_dir(f.home.join("state/kio/local-execution"))
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path().join("record.json");
            path.is_file()
                .then(|| serde_json::from_slice(&fs::read(path).unwrap()).unwrap())
        })
        .filter(|record: &Value| {
            record["identity"]["target"]["kind"] == "image"
                && record["identity"]["target"]["target"] == *embedding_hash
        })
        .collect();
    assert_eq!(records.len(), 1, "one matching scope/image/profile owner");
    let owner = &records[0];
    assert_eq!(owner["identity"]["target"]["scope_id"], scope_id);
    assert_eq!(owner["identity"]["profile"], *profile_hash);
    assert_eq!(
        owner["identity"]["input_sha256"],
        embedding_hash.strip_prefix("sha256:").unwrap()
    );
    assert_eq!(
        owner["identity"]["contract"]["members"][0]["target"],
        serde_json::to_value(target).unwrap(),
        "receipt/input/profile identity is identical"
    );
    assert_eq!(owner["phase"]["phase"], "published");
    assert_eq!(owner["attempts"], 1, "one actual local logical attempt");
    assert_eq!(
        task.attempts, 1,
        "Task mirrors the exact local owner, without a second completion increment"
    );
    assert!(
        task.reservation_claim().is_none(),
        "free completion has no paid SQL stamp"
    );
    assert_eq!(billing(&f), before);
}

fn completed_local_query_child(child: &mut QueryChild, label: &str) -> std::process::Output {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if child.0.as_mut().unwrap().try_wait().unwrap().is_some() {
            return child.0.take().unwrap().wait_with_output().unwrap();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{label} did not finish after explicit fixture release"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn endpoint_revoke_was_explicitly_busy(output: &std::process::Output) -> bool {
    output.status.code() == Some(3)
        && serde_json::from_slice::<Value>(&output.stderr)
            .is_ok_and(|error| error["error_code"] == "KIO-E-PRIVATE-STATE-LOCKED-001")
}

#[test]
fn endpoint_revoke_from_other_scope_cannot_complete_while_actual_query_send_is_held() {
    use std::sync::atomic::Ordering;
    let _env = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    const QUERY: &str = "C_endpoint_inflight_query_physical_canary_fa1b29";
    let peer = QueryPeer::new(QUERY);
    let fixture = LocalQueryFixture::new(&peer);
    let grants = fixture.home.join("data/kio/grants/grants.json");
    let grants_before = fs::read(&grants).unwrap();
    let mut query = QueryChild(Some(
        fixture
            .command(0, &["search", QUERY, "--mode", "vector"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    peer.seen
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("scope A query must enter the actual TLS response barrier");
    assert_eq!(peer.queries.load(Ordering::SeqCst), 1);
    assert!(query.0.as_mut().unwrap().try_wait().unwrap().is_none());
    let mut revoke = QueryChild(Some(
        fixture
            .command(1, &["adapter", "revoke", "qwen3_vl_embedding_local"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let early = loop {
        if revoke.0.as_mut().unwrap().try_wait().unwrap().is_some() {
            break Some(completed_local_query_child(&mut revoke, "early revoke"));
        }
        if std::time::Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert!(!peer.release.load(Ordering::SeqCst));
    assert!(
        query.0.as_mut().unwrap().try_wait().unwrap().is_none(),
        "physical query must remain held throughout revoke observation"
    );
    let succeeded_while_held = early.as_ref().is_some_and(|output| output.status.success());
    if let Some(output) = early.as_ref().filter(|output| !output.status.success()) {
        assert!(
            endpoint_revoke_was_explicitly_busy(output),
            "only explicit nonmutating lock contention is acceptable while held: status={} stdout={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read(&grants).unwrap(),
            grants_before,
            "refused revoke must not mutate endpoint authority"
        );
    }
    // Always release/reap before checking the intended exclusion assertion.
    peer.release.store(true, Ordering::SeqCst);
    let query_output = completed_local_query_child(&mut query, "physical query");
    assert!(
        query_output.status.success(),
        "query positive control failed: {} {}",
        String::from_utf8_lossy(&query_output.stdout),
        String::from_utf8_lossy(&query_output.stderr)
    );
    let response: Value = serde_json::from_slice(&query_output.stdout).unwrap();
    assert_eq!(response["resolved_mode"], "vector", "{response}");
    let first_revoke = early
        .unwrap_or_else(|| completed_local_query_child(&mut revoke, "revoke after query release"));
    let revoked = if endpoint_revoke_was_explicitly_busy(&first_revoke) {
        // The prior attempt explicitly refused without mutation; this is one
        // fresh authorized retry, never replay of an ambiguous mutation.
        fixture
            .command(1, &["adapter", "revoke", "qwen3_vl_embedding_local"])
            .output()
            .unwrap()
    } else {
        first_revoke
    };
    assert!(
        revoked.status.success(),
        "revoke positive control failed: {} {}",
        String::from_utf8_lossy(&revoked.stdout),
        String::from_utf8_lossy(&revoked.stderr)
    );
    let revoked_json: Value = serde_json::from_slice(&revoked.stdout).unwrap();
    assert_eq!(revoked_json["status"], "revoked", "{revoked_json}");
    let denied = fixture.json(0, &["search", QUERY, "--mode", "hybrid"]);
    assert_eq!(denied["resolved_mode"], "text", "{denied}");
    assert_eq!(
        denied["fallback_reason"], "embedding_not_authorized",
        "{denied}"
    );
    assert_eq!(
        peer.queries.load(Ordering::SeqCst),
        1,
        "completed cross-scope revoke must block the next actual query send"
    );
    fixture.assert_no_billing_artifacts();
    assert!(
        !succeeded_while_held,
        "scope B revoke succeeded while scope A's real TLS request was still held; scope writer leases alone do not protect device endpoint authority"
    );
}

#[test]
fn endpoint_revoke_from_other_scope_before_final_query_consent_sends_nothing() {
    use std::sync::atomic::Ordering;
    let _env = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    const QUERY: &str = "C_endpoint_predispatch_query_physical_canary_592a18";
    let peer = QueryPeer::new(QUERY);
    let fixture = LocalQueryFixture::new(&peer);
    let ready = fixture.home.join("endpoint-final-consent.ready");
    let mut query = QueryChild(Some(
        fixture
            .command(0, &["search", QUERY, "--mode", "hybrid"])
            .env("KIO_TEST_SEARCH_FINAL_CONSENT_BARRIER_READY", &ready)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !ready.exists() && query.0.as_mut().unwrap().try_wait().unwrap().is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "actual query must reach final-consent fixture barrier"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(ready.exists());
    assert_eq!(peer.queries.load(Ordering::SeqCst), 0);
    let start = std::time::Instant::now();
    let revoked = fixture.json(1, &["adapter", "revoke", "qwen3_vl_embedding_local"]);
    assert_eq!(revoked["status"], "revoked", "{revoked}");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(20),
        "revoke fixture must finish inside the bounded debug-only final-consent hold"
    );
    assert_eq!(
        peer.queries.load(Ordering::SeqCst),
        0,
        "revoke completed before a physical query send"
    );
    fs::write(ready.with_extension("release"), b"release").unwrap();
    // A wrong implementation must still be reaped promptly if it sent anyway.
    peer.release.store(true, Ordering::SeqCst);
    let output =
        completed_local_query_child(&mut query, "query after cross-scope final-consent revoke");
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["resolved_mode"], "text", "{response}");
    assert_eq!(
        response["fallback_reason"], "embedding_not_authorized",
        "{response}"
    );
    assert_eq!(
        peer.queries.load(Ordering::SeqCst),
        0,
        "fresh grant recheck must prevent any actual query send"
    );
    assert!(
        !fixture.home.join("query.trace").exists(),
        "pre-dispatch denial creates no query attempt"
    );
    fixture.assert_no_billing_artifacts();
}
