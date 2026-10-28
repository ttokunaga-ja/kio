//! W1-c observable contracts: device-approved loopback HTTP, offline and secret boundaries.

mod support;

use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tempfile::TempDir;

const TOOL: &str = "qwen3_vl_embedding_local";
const MAX_BODY: usize = 2 * 1024 * 1024;
const SECRET_A: &str = "C_TEST_PRIVATE_SECRET_SCOPE_A_9271";
const SECRET_B: &str = "C_TEST_PRIVATE_SECRET_SCOPE_B_5824";
const WATCH_TEXT: &str = "C_TEST_WATCH_LOCAL_HTTP_7163";

#[derive(Clone)]
enum Reply {
    Embedding,
    Redirect(String),
}
#[derive(Clone)]
struct Request {
    path: String,
    body: Vec<u8>,
}

struct Peer {
    port: u16,
    seen: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Peer {
    fn start(reply: Reply) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let worker_seen = seen.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => serve(stream, &reply, &worker_seen),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(e) => panic!("loopback fixture accept failed: {e}"),
                }
            }
        });
        Self {
            port,
            seen,
            stop,
            worker: Some(worker),
        }
    }
    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
    fn requests(&self) -> Vec<Request> {
        self.seen.lock().unwrap().clone()
    }
    fn wait_for(&self, predicate: impl Fn(&[Request]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let seen = self.requests();
            if predicate(&seen) {
                return;
            }
            assert!(Instant::now() < deadline, "bounded local HTTP wait expired");
            thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

// Same bounded chunk decoder used by v1_free_local_embedding.rs. None means
// the chunk stream is incomplete and the accepted socket needs another read.
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
        assert!(
            decoded.len() + size < MAX_BODY,
            "decoded HTTP body exceeded fixture bound"
        );
        decoded.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

// Bounded HTTP/1 framing: blocking accepted socket and finite timeouts. Accept
// either Content-Length or chunked transfer framing without unbounded reads.
fn serve(mut stream: TcpStream, reply: &Reply, seen: &Arc<Mutex<Vec<Request>>>) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    let (path, body) = loop {
        let mut block = [0_u8; 4096];
        let n = stream.read(&mut block).unwrap();
        assert_ne!(n, 0, "HTTP EOF before framed request completed");
        bytes.extend_from_slice(&block[..n]);
        assert!(
            bytes.len() <= MAX_BODY,
            "HTTP request exceeded fixture bound"
        );
        let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&bytes[..end]).unwrap();
        let mut lines = headers.split("\r\n");
        let mut first = lines.next().unwrap().split_whitespace();
        assert_eq!(first.next(), Some("POST"));
        let path = first.next().unwrap().to_owned();
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .collect::<Vec<_>>();
        let content_length = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value.trim().parse::<usize>().unwrap());
        let chunked = headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("transfer-encoding")
                && value
                    .split(',')
                    .any(|coding| coding.trim().eq_ignore_ascii_case("chunked"))
        });
        let body_start = end + 4;
        match (content_length, chunked) {
            (Some(size), false) => {
                assert!(size <= MAX_BODY);
                if bytes.len() >= body_start + size {
                    break (path, bytes[body_start..body_start + size].to_vec());
                }
            }
            (None, true) => {
                if let Some(body) = decode_fixture_chunks(&bytes[body_start..]) {
                    break (path, body);
                }
            }
            _ => panic!("request must use exactly one supported bounded body framing"),
        }
    };
    let _: Value = serde_json::from_slice(&body).expect("embedding request JSON");
    seen.lock().unwrap().push(Request { path, body });
    match reply {
        Reply::Embedding => {
            let mut vector = vec![0.0_f32; 768];
            vector[0] = 1.0;
            let body = json!({"data":[{"embedding":vector}]}).to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), body).unwrap();
        }
        Reply::Redirect(target) => write!(stream,
            "HTTP/1.1 302 Found\r\nLocation: {target}/v1/embeddings\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ).unwrap(),
    }
    stream.flush().unwrap();
}

struct Device {
    home: PathBuf,
    data: PathBuf,
    config: PathBuf,
    cache: PathBuf,
    state: PathBuf,
    tmp: PathBuf,
    tools: PathBuf,
}
impl Device {
    fn new(temp: &TempDir) -> Self {
        let home = support::create_private_child_dir(temp.path(), "home");
        let data = support::create_private_child_dir(temp.path(), "data");
        let config = support::create_private_child_dir(temp.path(), "config");
        let cache = support::create_private_child_dir(temp.path(), "cache");
        let state = support::create_private_child_dir(temp.path(), "state");
        let tmp = support::create_private_child_dir(temp.path(), "tmp");
        let tools = support::create_private_child_dir(&config, "kio");
        Self {
            home,
            data,
            config,
            cache,
            state,
            tmp,
            tools,
        }
    }
    fn command(&self, root: &Path, args: &[&str]) -> Command {
        let mut c = Command::new(assert_cmd::cargo::cargo_bin("kio"));
        c.env_clear();
        if let Some(v) = std::env::var_os("SystemRoot") {
            c.env("SystemRoot", v);
        }
        c.current_dir(root)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("XDG_DATA_HOME", &self.data)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("XDG_CACHE_HOME", &self.cache)
            .env("XDG_STATE_HOME", &self.state)
            .env("TMPDIR", &self.tmp)
            .env("TEMP", &self.tmp)
            .env("TMP", &self.tmp)
            .arg("--json")
            .args(args)
            .stdin(Stdio::null());
        c
    }
    fn output(&self, root: &Path, args: &[&str]) -> std::process::Output {
        self.command(root, args).output().unwrap()
    }
    fn success(&self, root: &Path, args: &[&str]) -> Value {
        let out = self.output(root, args);
        assert!(
            out.status.success(),
            "{args:?}: {} {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn tools_text(&self, body: &str) {
        #[cfg(windows)]
        {
            use kio_core::store_dir::{Publication, StoreDirectory};
            let directory = StoreDirectory::open(&self.tools).unwrap();
            let leaf = Path::new("tools.toml");
            if directory.contains_entry(leaf).unwrap() {
                directory.ensure_owner_private(leaf).unwrap();
            }
            directory
                .write_atomic(leaf, body.as_bytes(), Publication::Upsert)
                .unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = self.tools.join("tools.toml");
            fs::write(path, body).unwrap();
            fs::set_permissions(
                self.tools.join("tools.toml"),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
    }
    fn embedding_url(&self, url: &str) {
        self.tools_text(&format!(
            "[embedding.{TOOL}]\nkind = \"offline_api\"\nurl = \"{url}\"\nmodel = \"Qwen/Qwen3-VL-Embedding-2B\"\n"
        ));
    }
}
fn scope(temp: &TempDir, name: &str, public: &str, secret: &str) -> PathBuf {
    let root = support::create_private_child_dir(temp.path(), name);
    fs::write(root.join("notes.md"), format!("# Notes\n\n{public}\n")).unwrap();
    fs::write(
        root.join("api_secret.md"),
        format!("# Secret\n\n{secret}\n"),
    )
    .unwrap();
    root
}
fn assert_secret_held(device: &Device, root: &Path) {
    let status = device.success(root, &["status"]);
    assert!(
        status["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["type"] == "embedding"
                && t["input_path"] == "api_secret.md"
                && t["fallback_reason"] == "secrets_tier_b_hold"),
        "{status}"
    );
    assert!(
        status["quarantine"]
            .as_array()
            .unwrap()
            .iter()
            .any(|q| q["path"] == "api_secret.md" && q["reason"] == "secrets_tier_b"),
        "{status}"
    );
}

fn chunk_vector_count(root: &Path) -> i64 {
    let source = support::immutable_sqlite(root.join(".kio/index/sqlite.db"));
    source
        .query_row(
            "SELECT COUNT(*) FROM embeddings WHERE target_type='chunk'",
            [],
            |row| row.get(0),
        )
        .unwrap()
}
fn assert_chunk_vectors(root: &Path) {
    let count = chunk_vector_count(root);
    assert!(
        count > 0,
        "successful local dispatch must publish chunk vectors"
    );
}

// Child::drop does not terminate a still-running process. Keep the watch
// subprocess owned by the fixture so an assertion failure cannot orphan it.
struct WatchGuard(Option<Child>);
impl WatchGuard {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        let status = self
            .0
            .as_mut()
            .expect("watch child still owned")
            .try_wait()?;
        if status.is_some() {
            self.0.take();
        }
        Ok(status)
    }
}
impl Drop for WatchGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

#[test]
fn http_device_approval_spans_private_scopes_offline_but_not_secret_consent() {
    let temp = support::canonical_tempdir();
    let device = Device::new(&temp);
    let peer = Peer::start(Reply::Embedding);
    let a = scope(&temp, "scope-a", "PUBLIC_SCOPE_A_311", SECRET_A);
    let b = scope(&temp, "scope-b", "PUBLIC_SCOPE_B_422", SECRET_B);

    // Init before selecting adapter metadata gives a real local baseline positive control.
    device.success(&a, &["init"]);
    device.success(&b, &["init"]);
    device.embedding_url(&peer.url());

    let preview = device.success(&a, &["adapter", "approve", TOOL, "--preview"]);
    let disclosure = preview.to_string().to_ascii_lowercase();
    assert!(
        disclosure.contains(&peer.url()),
        "preview must name exact destination: {preview}"
    );
    assert!(
        disclosure.contains("unauthenticated"),
        "HTTP must be disclosed as unauthenticated: {preview}"
    );
    assert_eq!(
        preview["device_endpoints"][0]["peer_authenticated"], false,
        "HTTP approval preview requires the documented machine boolean"
    );
    device.success(&a, &["adapter", "approve", TOOL, "--yes"]);
    assert!(!device.tools.join("local-peer-ca.pem").exists());
    assert!(!device.tools.join("ca.pem").exists());

    // Approval binds the complete configured destination. A path change
    // invalidates that approval before a request reaches the same loopback host.
    device.embedding_url(&format!("{}/changed", peer.url()));
    // Ordinary offline index can retain unapproved enrichment as pending;
    // destination drift must prevent the send and vector publication.
    device.success(&a, &["index", "--offline"]);
    assert_eq!(
        chunk_vector_count(&a),
        0,
        "changed endpoint path must not publish embeddings without fresh approval"
    );
    assert!(
        peer.requests().is_empty(),
        "stale destination approval must fail before dispatch"
    );
    device.embedding_url(&peer.url());

    // One endpoint approval on A must also admit independent scope B.
    // Both invocations are explicitly offline; no external provider is configured or inherited.
    device.success(&a, &["index", "--offline"]);
    assert_chunk_vectors(&a);
    device.success(&b, &["index", "--offline"]);
    assert_chunk_vectors(&b);
    peer.wait_for(|rows| {
        rows.iter()
            .any(|r| String::from_utf8_lossy(&r.body).contains("PUBLIC_SCOPE_A_311"))
            && rows
                .iter()
                .any(|r| String::from_utf8_lossy(&r.body).contains("PUBLIC_SCOPE_B_422"))
    });
    let requests = peer.requests();
    assert!(requests.iter().all(|r| r.path == "/v1/embeddings"));
    assert!(!requests.iter().any(|r| {
        let body = String::from_utf8_lossy(&r.body);
        body.contains(SECRET_A) || body.contains(SECRET_B)
    }));
    assert_secret_held(&device, &a);
    assert_secret_held(&device, &b);

    // Query embedding uses the same selected endpoint approval under offline.
    // Require both the resolved vector mode and the query at the actual peer;
    // a silent text fallback cannot satisfy this assertion.
    const QUERY: &str = "C_TEST_OFFLINE_LOOPBACK_QUERY_8302";
    let search = device.success(&b, &["search", QUERY, "--mode", "vector", "--offline"]);
    assert_eq!(search["resolved_mode"], "vector", "{search}");
    peer.wait_for(|rows| {
        rows.iter()
            .any(|r| String::from_utf8_lossy(&r.body).contains(QUERY))
    });

    // Endpoint approval alone does not authorize either secret. A separate
    // explicit send-secrets choice releases A only; B remains held.
    device.success(&a, &["adapter", "approve", TOOL, "--yes", "--send-secrets"]);
    device.success(&a, &["index", "--offline"]);
    peer.wait_for(|rows| {
        rows.iter()
            .any(|r| String::from_utf8_lossy(&r.body).contains(SECRET_A))
    });
    assert_secret_held(&device, &b);
    device.success(&b, &["index", "--offline"]);
    assert!(
        !peer
            .requests()
            .iter()
            .any(|r| String::from_utf8_lossy(&r.body).contains(SECRET_B))
    );

    // Bounded watch fixture/CLI already exists in v1_watch: watch run with a
    // 1-second reconciliation interval, status polling, then watch stop (25s).
    // The root is initialized and only loopback is configured/approved.
    let grants_path = device.data.join("kio/grants/grants.json");
    let grants_before = fs::read(&grants_path).unwrap();
    let vectors_before_watch = chunk_vector_count(&b);
    let mut watcher = WatchGuard::new(
        device
            .command(&b, &["watch", "run", "--reconcile-interval-seconds", "1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    thread::sleep(Duration::from_millis(500));
    assert!(
        watcher.try_wait().unwrap().is_none(),
        "watch failed startup"
    );
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        let status = device.success(&b, &["watch", "status"]);
        if status["last_observation"]["last_success_ms"].is_number() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "watch startup did not publish observation"
        );
        thread::sleep(Duration::from_millis(100));
    }
    fs::write(
        b.join("watch-note.md"),
        format!("# Watch\n\n{WATCH_TEXT}\n"),
    )
    .unwrap();
    peer.wait_for(|rows| {
        rows.iter()
            .any(|r| String::from_utf8_lossy(&r.body).contains(WATCH_TEXT))
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while chunk_vector_count(&b) <= vectors_before_watch {
        assert!(
            Instant::now() < deadline,
            "watch request did not publish a local chunk vector"
        );
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        device.success(&b, &["watch", "stop"])["status"],
        "stop_requested"
    );
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        if let Some(status) = watcher.try_wait().unwrap() {
            assert!(status.success(), "watch exited {status}");
            break;
        }
        assert!(Instant::now() < deadline, "watch stop timed out");
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        fs::read(grants_path).unwrap(),
        grants_before,
        "watch must not create or renew endpoint approval"
    );
    assert!(
        !peer
            .requests()
            .iter()
            .any(|r| String::from_utf8_lossy(&r.body).contains(SECRET_B))
    );
}

#[test]
fn offline_blocks_external_embedding_dispatch_and_online_mock_is_positive_control() {
    let temp = support::canonical_tempdir();
    let device = Device::new(&temp);
    let root = scope(
        &temp,
        "external-scope",
        "EXTERNAL_OFFLINE_CONTROL_903",
        "unused",
    );
    fs::write(
        root.join("plain.md"),
        "# Offline external guard\n\nEXTERNAL_OFFLINE_CONTROL_903\n",
    )
    .unwrap();
    device.success(&root, &["init"]);
    device.success(&root, &["ledger", "init"]);
    device.tools_text(
        "[embedding.gemini_embedding_2]\nkind = \"online_api\"\nauth = \"plain:C_TEST_EXTERNAL_MOCK_CREDENTIAL_624\"\n",
    );

    // The debug mock is an in-process observer/control only; all credentials
    // are synthetic and no provider key or network destination is available.
    let mut offline = device.command(&root, &["index", "--offline"]);
    offline.env("KIO_TEST_GEMINI_EMBED", "mock");
    let offline = offline.output().unwrap();
    assert!(
        offline.status.success(),
        "offline should defer external enrichment cleanly: {}",
        String::from_utf8_lossy(&offline.stderr)
    );
    let source = support::immutable_sqlite(root.join(".kio/index/sqlite.db"));
    let count: i64 = source
        .query_row("SELECT COUNT(*) FROM embeddings", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        count, 0,
        "offline external target must not publish mock embeddings"
    );
    drop(source);

    // Prove that the same mock route is viable when externally approved and
    // explicitly online; this rules out an unavailable fixture masquerading
    // as correct offline suppression.
    device.success(
        &root,
        &["adapter", "approve", "gemini_embedding_2", "--yes"],
    );
    let mut online = device.command(&root, &["index", "--online", "--realtime"]);
    online.env("KIO_TEST_GEMINI_EMBED", "mock");
    let output = online.output().unwrap();
    assert!(
        output.status.success(),
        "mock positive control failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_chunk_vectors(&root);
}

#[test]
fn http_transport_keeps_no_proxy_no_redirect_and_literal_loopback_guards() {
    let temp = support::canonical_tempdir();
    let device = Device::new(&temp);
    let origin = Peer::start(Reply::Embedding);
    let proxy = Peer::start(Reply::Embedding);
    let redirect_target = Peer::start(Reply::Embedding);
    let redirect = Peer::start(Reply::Redirect(redirect_target.url()));
    let root = scope(&temp, "scope", "PUBLIC_GUARD_608", "unused");
    device.success(&root, &["init"]);

    device.embedding_url(&origin.url());
    device.success(&root, &["adapter", "approve", TOOL, "--yes"]);
    let mut cmd = device.command(&root, &["index", "--offline"]);
    cmd.env("HTTP_PROXY", proxy.url())
        .env("http_proxy", proxy.url())
        .env("NO_PROXY", "")
        .env("no_proxy", "");
    let output = cmd.output().unwrap();
    assert!(
        output.status.success() || [3, 4, 5, 6].contains(&output.status.code().unwrap_or(-1)),
        "unexpected local result: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    origin.wait_for(|rows| !rows.is_empty());
    assert!(
        proxy.requests().is_empty(),
        "proxy environment rerouted loopback payload"
    );

    device.embedding_url(&redirect.url());
    // New input forces execution under the changed destination profile.
    fs::write(root.join("redirect-note.md"), "redirect guard input\n").unwrap();
    device.success(&root, &["adapter", "approve", TOOL, "--yes"]);
    let _ = device.output(&root, &["index", "--offline"]);
    redirect.wait_for(|rows| !rows.is_empty());
    assert!(
        redirect_target.requests().is_empty(),
        "HTTP redirect changed the approved destination"
    );

    device.embedding_url("http://192.0.2.1:9");
    let rejected = device.output(&root, &["adapter", "approve", TOOL, "--preview"]);
    assert_eq!(rejected.status.code(), Some(2));
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&rejected.stdout),
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert!(
        diagnostic.contains("KIO-E-LOCAL-PEER-URL-001"),
        "{diagnostic}"
    );
    assert_eq!(origin.requests().len(), 1);
    assert!(proxy.requests().is_empty());
    assert!(redirect_target.requests().is_empty());
}

#[test]
fn configured_unapproved_http_peer_status_is_unauthenticated_without_scope() {
    let temp = support::canonical_tempdir();
    let device = Device::new(&temp);
    let peer = Peer::start(Reply::Embedding);
    device.embedding_url(&peer.url().replacen("http://", "HTTP://", 1));
    let grants = device.data.join("kio/grants");
    assert!(!grants.exists());
    let status = device.success(&device.home, &["adapter", "status", TOOL]);
    assert!(
        status["scope_id"].is_null(),
        "device status must work outside a scope: {status}"
    );
    assert_eq!(
        status["device_endpoints"][0]["binding"]["destination"],
        peer.url()
    );
    assert_eq!(
        status["device_endpoints"][0]["peer_authenticated"], false,
        "valid configured HTTP endpoint must disclose its machine authentication state before approval: {status}"
    );
    assert!(
        status["grants"].as_array().unwrap().is_empty(),
        "status must not infer approval"
    );
    assert!(
        !grants.exists(),
        "read-only status must not create approval state"
    );
    assert!(
        peer.requests().is_empty(),
        "status must not contact the HTTP endpoint"
    );
}
