//! A04 authenticated-local-peer acceptance executor.
//!
//! This is deliberately a bounded TLS loopback proof, rather than a provider
//! acceptance substitute.  Each operation starts the supplied release CLI in
//! a fresh process, and the loopback peers record HTTP request bodies so a
//! failed trust or grant check cannot be mistaken for a harmless client error.

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};

use crate::{
    acceptance::{
        AcceptanceCase, AcceptanceError, AcceptanceLane, AcceptanceReceipt, AcceptanceSubcase,
        ExpectedReceipt, MAX_BINARY_BYTES, MAX_FIXTURE_BYTES, current_evaluator_sha256_matches,
        empty_runtime, sha256_regular_file, validate_expected, write_receipt_create_only,
    },
    acceptance_environment::IsolatedChildEnvironment,
    runner::{BoundedProcessOptions, run_bounded_command},
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
const SERVER_TIMEOUT: Duration = Duration::from_secs(25);
const MAX_OUTPUT: usize = 128 * 1024;
const MAX_REQUEST: usize = 64 * 1024;
const TOOL_ID: &str = "qwen3_vl_embedding_local";
static PROBE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone)]
pub struct LocalTrustOptions {
    pub binary: PathBuf,
    pub fixture: PathBuf,
    pub expected: ExpectedReceipt,
    pub work_dir: PathBuf,
    pub receipt: PathBuf,
}

/// Execute A04/native-contract/local-trust using only the release candidate.
pub fn run_a04(options: &LocalTrustOptions) -> Result<AcceptanceReceipt, AcceptanceError> {
    validate_options(options)?;
    reject_test_controls()?;
    fs::create_dir(&options.work_dir).map_err(io)?;
    private_dir(&options.work_dir)?;
    let work = fs::canonicalize(&options.work_dir).map_err(io)?;
    let private = create_private_layout(&work)?;
    let scope = work.join("scope");
    fs::create_dir(&scope).map_err(io)?;
    private_dir(&scope)?;
    fs::copy(&options.fixture, scope.join("acceptance.md")).map_err(io)?;

    let peer_a = TlsPeer::start()?;
    let peer_b = TlsPeer::start()?;
    let ca_a = work.join("ca-a.pem");
    let ca_b = work.join("ca-b.pem");
    write_private(&ca_a, peer_a.pem().as_bytes())?;
    write_private(&ca_b, peer_b.pem().as_bytes())?;

    // Initialization consumes no peer request; configuration is installed
    // before every request so the real composition root reloads it.
    configure(&private, peer_b.url())?;
    json_ok(&options.binary, &private, Some(&scope), &["init"])?;
    json_ok(
        &options.binary,
        &private,
        None,
        &[
            "adapter",
            "trust",
            "register",
            "--ca-pem",
            path_arg(&ca_a)?,
            "--yes",
        ],
    )?;

    // Approval itself is scoped consent, not a transport probe.  Grant B while
    // A is the active anchor so the following index must reach the real TLS
    // client and reject B's untrusted certificate before it writes a body.
    json_ok(
        &options.binary,
        &private,
        Some(&scope),
        &["adapter", "approve", TOOL_ID, "--yes"],
    )?;

    // A CA which does not authenticate the configured peer must fail in TLS,
    // before the embedding JSON body is exposed to that peer.
    failed_after_tls_without_body(&options.binary, &private, &scope, &peer_b, "untrusted peer")?;

    configure(&private, peer_a.url())?;
    json_ok(
        &options.binary,
        &private,
        Some(&scope),
        &["adapter", "approve", TOOL_ID, "--yes"],
    )?;
    successful_request(&options.binary, &private, &scope, &peer_a)?;

    // Rotation changes the runtime trust binding and cannot revive A's scope
    // grant.  B sees no body until its own explicit scoped approval.
    json_ok(
        &options.binary,
        &private,
        None,
        &[
            "adapter",
            "trust",
            "rotate",
            "--ca-pem",
            path_arg(&ca_b)?,
            "--yes",
        ],
    )?;
    configure(&private, peer_b.url())?;
    failed_without_body(
        &options.binary,
        &private,
        &scope,
        &peer_b,
        "rotated unapproved peer",
    )?;
    json_ok(
        &options.binary,
        &private,
        Some(&scope),
        &["adapter", "approve", TOOL_ID, "--yes"],
    )?;
    successful_request(&options.binary, &private, &scope, &peer_b)?;

    // Reusing the original CA and endpoint is the important lifecycle case:
    // a matching old grant must remain dead after A -> B -> A.
    json_ok(
        &options.binary,
        &private,
        None,
        &[
            "adapter",
            "trust",
            "rotate",
            "--ca-pem",
            path_arg(&ca_a)?,
            "--yes",
        ],
    )?;
    configure(&private, peer_a.url())?;
    failed_without_body(
        &options.binary,
        &private,
        &scope,
        &peer_a,
        "A grant revival",
    )?;
    json_ok(
        &options.binary,
        &private,
        Some(&scope),
        &["adapter", "approve", TOOL_ID, "--yes"],
    )?;
    successful_request(&options.binary, &private, &scope, &peer_a)?;

    json_ok(
        &options.binary,
        &private,
        None,
        &["adapter", "trust", "revoke"],
    )?;
    failed_without_body(&options.binary, &private, &scope, &peer_a, "revoked trust")?;

    // Recheck immutable inputs only after all live-client checks have passed.
    if sha256_regular_file(&options.binary, MAX_BINARY_BYTES)?
        != options.expected.candidate.binary_sha256
        || sha256_regular_file(&options.fixture, MAX_FIXTURE_BYTES)?
            != options.expected.fixture.sha256
    {
        return Err(invalid("candidate binary or fixture changed during A04"));
    }
    let receipt = AcceptanceReceipt {
        schema: "kio.acceptance.receipt/v4".into(),
        requirement: options.expected.requirement.clone(),
        candidate: options.expected.candidate.clone(),
        fixture: options.expected.fixture.clone(),
        workflow: options.expected.workflow.clone(),
        evaluator_sha256: current_evaluator_sha256_matches(&options.expected.evaluator_sha256)?,
        service_identity_sha256: options.expected.service_identity_sha256.clone(),
        contract_binary_sha256: None,
        runtime: empty_runtime(),
        passed: true,
    };
    write_receipt_create_only(&options.receipt, &receipt)?;
    Ok(receipt)
}

fn validate_options(options: &LocalTrustOptions) -> Result<(), AcceptanceError> {
    validate_expected(&options.expected)?;
    current_evaluator_sha256_matches(&options.expected.evaluator_sha256)?;
    let requirement = &options.expected.requirement;
    if requirement.case != AcceptanceCase::A04
        || requirement.lane != AcceptanceLane::NativeContract
        || requirement.subcase != AcceptanceSubcase::LocalTrust
    {
        return Err(invalid("A04 local trust received a different requirement"));
    }
    if options.expected.contract_binary_sha256.is_some() {
        return Err(invalid(
            "A04 local trust must not carry a contract binary binding",
        ));
    }
    if options.work_dir.exists() || options.receipt.exists() {
        return Err(invalid(
            "A04 work directory and receipt must be create-only",
        ));
    }
    if sha256_regular_file(&options.binary, MAX_BINARY_BYTES)?
        != options.expected.candidate.binary_sha256
        || sha256_regular_file(&options.fixture, MAX_FIXTURE_BYTES)?
            != options.expected.fixture.sha256
    {
        return Err(invalid(
            "candidate binary or fixture differs from expected binding",
        ));
    }
    Ok(())
}

fn reject_test_controls() -> Result<(), AcceptanceError> {
    if std::env::vars_os().any(|(key, _)| {
        let key = key.to_string_lossy();
        key.starts_with("KIO_TEST_") || key.starts_with("KIO_MOCK_")
    }) {
        return Err(invalid(
            "A04 refuses KIO_TEST_ or KIO_MOCK_ environment controls",
        ));
    }
    Ok(())
}

fn create_private_layout(work: &Path) -> Result<PathBuf, AcceptanceError> {
    let private = work.join("device");
    for path in [
        &private,
        &private.join("xdg-config"),
        &private.join("xdg-data"),
        &private.join("xdg-cache"),
        &private.join("tmp"),
    ] {
        fs::create_dir(path).map_err(io)?;
        private_dir(path)?;
    }
    fs::canonicalize(private).map_err(io)
}

fn configure(private: &Path, endpoint: &str) -> Result<(), AcceptanceError> {
    let config = private.join("xdg-config/kio");
    if !config.exists() {
        fs::create_dir(&config).map_err(io)?;
        private_dir(&config)?;
    }
    let tools = config.join("tools.toml");
    let body = format!(
        "[embedding.{TOOL_ID}]\nkind = \"offline_api\"\nurl = \"{endpoint}\"\nmodel = \"Qwen/Qwen3-VL-Embedding-2B\"\n"
    );
    fs::write(&tools, body).map_err(io)?;
    private_file(&tools)
}

/// For the untrusted certificate case, prove the product attempted TLS rather
/// than merely rejecting a missing grant before it selected the local client.
fn failed_after_tls_without_body(
    binary: &Path,
    private: &Path,
    scope: &Path,
    peer: &TlsPeer,
    label: &str,
) -> Result<(), AcceptanceError> {
    let attempts = peer.attempts();
    let bodies = peer.body_count();
    write_probe(scope)?;
    let _output = cli(binary, private, Some(scope), &["index", "--online"])?;
    peer.wait_for_transport(attempts)?;
    peer.wait_quiet(bodies)?;
    if peer.body_count() != bodies {
        return Err(invalid(format!("{label} received an HTTP request body")));
    }
    Ok(())
}

/// Grant and revocation checks are expected to reject before connecting; their
/// evidence is the stronger body non-disclosure condition, not a handshake.
fn failed_without_body(
    binary: &Path,
    private: &Path,
    scope: &Path,
    peer: &TlsPeer,
    label: &str,
) -> Result<(), AcceptanceError> {
    let before = peer.body_count();
    write_probe(scope)?;
    let _output = cli(binary, private, Some(scope), &["index", "--online"])?;
    // Give the TLS server a bounded opportunity to finish a failed handshake.
    peer.wait_quiet(before)?;
    if peer.body_count() != before {
        return Err(invalid(format!("{label} received an HTTP request body")));
    }
    Ok(())
}

fn successful_request(
    binary: &Path,
    private: &Path,
    scope: &Path,
    peer: &TlsPeer,
) -> Result<(), AcceptanceError> {
    let before = peer.body_count();
    write_probe(scope)?;
    // `--online` opens the already-recorded exact scoped grant. The configured
    // destination remains literal loopback HTTPS, so this cannot contact an
    // external provider; `--offline` intentionally keeps this transport out
    // of the execution set and would make the TLS proof vacuous.
    json_ok(binary, private, Some(scope), &["index", "--online"])?;
    peer.wait_for_body(before + 1)?;
    Ok(())
}

fn json_ok(
    binary: &Path,
    private: &Path,
    scope: Option<&Path>,
    args: &[&str],
) -> Result<(), AcceptanceError> {
    let output = cli(binary, private, scope, args)?;
    if !output.status.success() {
        return Err(AcceptanceError::Command(format!(
            "{} failed: {}",
            args.join(" "),
            output.stderr.trim()
        )));
    }
    serde_json::from_str::<serde_json::Value>(&output.stdout)
        .map_err(|error| AcceptanceError::Json(error.to_string()))?;
    Ok(())
}

fn cli(
    binary: &Path,
    private: &Path,
    scope: Option<&Path>,
    args: &[&str],
) -> Result<crate::runner::BoundedProcessOutput, AcceptanceError> {
    let mut command = Command::new(binary);
    IsolatedChildEnvironment::new(
        private,
        private.join("xdg-config"),
        private.join("xdg-data"),
        private.join("xdg-cache"),
        private.join("tmp"),
    )
    .apply(&mut command)?;
    command.arg("--json").args(args);
    if let Some(scope) = scope {
        command.current_dir(scope);
    }
    run_bounded_command(
        &mut command,
        BoundedProcessOptions {
            timeout: COMMAND_TIMEOUT,
            max_stdout_bytes: MAX_OUTPUT,
            max_stderr_bytes: MAX_OUTPUT,
        },
        None,
    )
    .map_err(|error| AcceptanceError::Command(format!("bounded A04 command failed: {error}")))
}

struct TlsPeer {
    url: String,
    pem: String,
    stop: Arc<AtomicBool>,
    tcp_attempts: Arc<AtomicUsize>,
    tls_attempts: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<Vec<u8>>>>,
    task: Option<thread::JoinHandle<()>>,
}

impl TlsPeer {
    fn start() -> Result<Self, AcceptanceError> {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).map_err(|error| {
                invalid(format!("could not generate local TLS certificate: {error}"))
            })?;
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(cert.der().to_vec())],
                PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
            )
            .map_err(|error| invalid(format!("could not configure local TLS peer: {error}")))?;
        let listener = TcpListener::bind("127.0.0.1:0").map_err(io)?;
        listener.set_nonblocking(true).map_err(io)?;
        let url = format!("https://{}", listener.local_addr().map_err(io)?);
        let stop = Arc::new(AtomicBool::new(false));
        let tcp_attempts = Arc::new(AtomicUsize::new(0));
        let tls_attempts = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let task_stop = Arc::clone(&stop);
        let task_tcp_attempts = Arc::clone(&tcp_attempts);
        let task_tls_attempts = Arc::clone(&tls_attempts);
        let task_bodies = Arc::clone(&bodies);
        let config = Arc::new(config);
        let task = thread::spawn(move || {
            let deadline = Instant::now() + SERVER_TIMEOUT;
            while !task_stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((stream, _)) => {
                        task_tcp_attempts.fetch_add(1, Ordering::Relaxed);
                        handle_connection(
                            stream,
                            Arc::clone(&config),
                            &task_tls_attempts,
                            &task_bodies,
                        )
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10))
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            url,
            pem: cert.pem(),
            stop,
            tcp_attempts,
            tls_attempts,
            bodies,
            task: Some(task),
        })
    }

    fn url(&self) -> &str {
        &self.url
    }
    fn pem(&self) -> &str {
        &self.pem
    }
    fn body_count(&self) -> usize {
        self.bodies.lock().map_or(0, |bodies| bodies.len())
    }
    fn attempts(&self) -> (usize, usize) {
        (
            self.tcp_attempts.load(Ordering::Relaxed),
            self.tls_attempts.load(Ordering::Relaxed),
        )
    }
    fn wait_for_transport(&self, baseline: (usize, usize)) -> Result<(), AcceptanceError> {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let attempts = self.attempts();
            if attempts.0 > baseline.0 && attempts.1 > baseline.1 {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }
        Err(invalid(
            "untrusted local peer did not observe a TCP and TLS handshake attempt",
        ))
    }
    fn wait_for_body(&self, count: usize) -> Result<(), AcceptanceError> {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if self.body_count() >= count {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }
        Err(invalid(
            "trusted local peer did not receive an embedding request body",
        ))
    }
    fn wait_quiet(&self, count: usize) -> Result<(), AcceptanceError> {
        let deadline = Instant::now() + Duration::from_millis(150);
        while Instant::now() < deadline {
            if self.body_count() != count {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }
}

impl Drop for TlsPeer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

fn handle_connection(
    stream: TcpStream,
    config: Arc<ServerConfig>,
    tls_attempts: &Arc<AtomicUsize>,
    bodies: &Arc<Mutex<Vec<Vec<u8>>>>,
) {
    // The listener accepts in nonblocking mode so its owner can observe the
    // shutdown flag. Accepted sockets inherit that mode on the platforms this
    // evaluator supports. Rustls then sees `WouldBlock` before it can complete
    // the client handshake, which made an approved peer look like a network
    // failure. Each connection has its own bounded read/write deadlines, so it
    // can safely switch back to blocking mode before driving the TLS stream.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let connection = match ServerConnection::new(config) {
        Ok(connection) => connection,
        Err(_) => return,
    };
    let mut stream = StreamOwned::new(connection, stream);
    tls_attempts.fetch_add(1, Ordering::Relaxed);
    let request = read_request(&mut stream);
    if request.is_empty() {
        return;
    }
    if let Some(body) = embedding_request_body(&request) {
        if let Ok(mut recorded) = bodies.lock() {
            recorded.push(body);
        }
        let payload = embedding_response();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            payload.len(),
            payload
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    }
}

fn read_request(stream: &mut StreamOwned<ServerConnection, TcpStream>) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    while request.len() < MAX_REQUEST {
        let Ok(count) = stream.read(&mut buffer) else {
            return Vec::new();
        };
        if count == 0 {
            return request;
        }
        request.extend_from_slice(&buffer[..count]);
        let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let length = String::from_utf8_lossy(&request[..end])
            .lines()
            .find_map(|line| {
                line.strip_prefix("Content-Length:")
                    .or_else(|| line.strip_prefix("content-length:"))
            })
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if end + 4 + length <= request.len() {
            return request;
        }
        if end + 4 + length > MAX_REQUEST {
            return Vec::new();
        }
    }
    Vec::new()
}

/// Accept only a real OpenAI-compatible embedding payload.  Counting an empty
/// request or arbitrary bytes as a receipt would weaken the positive control:
/// the body must be the chat-form wire contract used by the local adapter.
fn embedding_request_body(request: &[u8]) -> Option<Vec<u8>> {
    let end = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")? + 4;
    let body = request[end..].to_vec();
    let payload: serde_json::Value = serde_json::from_slice(&body).ok()?;
    (payload.get("model").and_then(serde_json::Value::as_str) == Some("Qwen/Qwen3-VL-Embedding-2B")
        && payload
            .get("encoding_format")
            .and_then(serde_json::Value::as_str)
            == Some("float")
        && payload
            .get("messages")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|messages| !messages.is_empty()))
    .then_some(body)
}

fn embedding_response() -> String {
    let vector = std::iter::repeat_n("1", 768).collect::<Vec<_>>().join(",");
    format!(
        "{{\"object\":\"list\",\"data\":[{{\"object\":\"embedding\",\"index\":0,\"embedding\":[{vector}]}}]}}"
    )
}

/// Every check changes the corpus, so a prior successful vector cannot make a
/// later `index` merely reuse a persisted embedding instead of using the peer.
fn write_probe(scope: &Path) -> Result<(), AcceptanceError> {
    let sequence = PROBE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    fs::write(
        scope.join(format!("peer-probe-{sequence}.md")),
        format!("A04 authenticated local peer probe {sequence}\n"),
    )
    .map_err(io)
}

fn path_arg(path: &Path) -> Result<&str, AcceptanceError> {
    path.to_str()
        .ok_or_else(|| invalid("A04 path is not UTF-8"))
}
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), AcceptanceError> {
    fs::write(path, bytes).map_err(io)?;
    private_file(path)
}
#[cfg(unix)]
fn private_dir(path: &Path) -> Result<(), AcceptanceError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(io)
}
#[cfg(not(unix))]
fn private_dir(_: &Path) -> Result<(), AcceptanceError> {
    Ok(())
}
#[cfg(unix)]
fn private_file(path: &Path) -> Result<(), AcceptanceError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(io)
}
#[cfg(not(unix))]
fn private_file(_: &Path) -> Result<(), AcceptanceError> {
    Ok(())
}
fn invalid(message: impl Into<String>) -> AcceptanceError {
    AcceptanceError::Invalid(message.into())
}
fn io(error: std::io::Error) -> AcceptanceError {
    AcceptanceError::Io(error.to_string())
}
