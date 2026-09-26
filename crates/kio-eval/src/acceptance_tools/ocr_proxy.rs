//! Bounded TLS boundary for the measured OCR HTTP service.
//!
//! The default listener is loopback-only. The only broader bind is the
//! container entrypoint's explicit `0.0.0.0`, whose exposure is constrained by
//! the reviewed Docker `127.0.0.1:18443:8443` mapping; it may receive Docker
//! NAT peers but does not widen the fixed loopback upstream.

use clap::Args as ClapArgs;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const MAX_BODY: usize = 256 * 1024 * 1024;
const MAX_HEADERS: usize = 64 * 1024;
const MAX_CLIENTS: usize = 2;
const HANDSHAKE_SECONDS: u64 = 5;
const UPSTREAM_CONNECT_SECONDS: u64 = 10;
const UPSTREAM_READ_SECONDS: u64 = 300;
const DEFAULT_UPSTREAM_PORT: u16 = 18080;
const DEFAULT_PORT: u16 = 18443;
const DEFAULT_INGEST_SECONDS: f64 = 20.0;
const DEFAULT_TOTAL_REQUEST_SECONDS: f64 = 330.0;

#[derive(Debug, ClapArgs)]
pub struct Args {
    #[arg(long)]
    pub cert: PathBuf,
    #[arg(long)]
    pub key: PathBuf,
    #[arg(long, default_value = "127.0.0.1")]
    pub host: IpAddr,
    #[arg(long, default_value_t = DEFAULT_PORT)]
    pub port: u16,
    #[arg(long, default_value_t = DEFAULT_UPSTREAM_PORT)]
    pub upstream_port: u16,
    #[arg(long, default_value_t = DEFAULT_INGEST_SECONDS)]
    pub ingest_seconds: f64,
    #[arg(long, default_value_t = DEFAULT_TOTAL_REQUEST_SECONDS)]
    pub total_request_seconds: f64,
}

#[derive(Clone, Copy)]
struct Limits {
    ingest: Duration,
    total: Duration,
}

#[derive(Clone, Copy)]
enum PeerPolicy {
    LoopbackOnly,
    DockerNatAllowed,
}

#[derive(Debug)]
struct Request {
    method: &'static str,
    path: &'static str,
    content_type: Option<String>,
    body: Vec<u8>,
}

#[derive(Debug)]
struct Response {
    status: u16,
    content_type: Option<String>,
    body: Vec<u8>,
}

pub fn run(args: Args) -> Result<(), String> {
    let peer_policy = peer_policy(args.host)?;
    let limits = Limits::new(args.ingest_seconds, args.total_request_seconds)?;
    let config = Arc::new(tls_config(&args.cert, &args.key)?);
    let listener = TcpListener::bind(SocketAddr::new(args.host, args.port))
        .map_err(|error| format!("cannot bind OCR TLS listener: {error}"))?;
    let active = Arc::new(AtomicUsize::new(0));
    for accepted in listener.incoming() {
        let stream = match accepted {
            Ok(value) => value,
            Err(error) => return Err(format!("TLS listener accept failed: {error}")),
        };
        let peer = match stream.peer_addr() {
            Ok(peer) => peer,
            Err(_) => {
                let _ = stream.shutdown(Shutdown::Both);
                continue;
            }
        };
        if !peer_allowed(peer, peer_policy) || !acquire_slot(&active) {
            let _ = stream.shutdown(Shutdown::Both);
            continue;
        }
        let config = Arc::clone(&config);
        let active = Arc::clone(&active);
        thread::spawn(move || {
            let _slot = Slot(active);
            let _ = serve_tls_connection(
                stream,
                config,
                peer,
                peer_policy,
                args.upstream_port,
                limits,
            );
        });
    }
    Ok(())
}

fn peer_policy(host: IpAddr) -> Result<PeerPolicy, String> {
    if host.is_loopback() {
        Ok(PeerPolicy::LoopbackOnly)
    } else if host == IpAddr::V4(Ipv4Addr::UNSPECIFIED) {
        Ok(PeerPolicy::DockerNatAllowed)
    } else {
        Err("proxy host must be loopback or explicit 0.0.0.0 container bind".into())
    }
}

fn peer_allowed(peer: SocketAddr, policy: PeerPolicy) -> bool {
    peer.ip().is_loopback() || matches!(policy, PeerPolicy::DockerNatAllowed)
}

impl Limits {
    fn new(ingest_seconds: f64, total_seconds: f64) -> Result<Self, String> {
        if !ingest_seconds.is_finite()
            || !total_seconds.is_finite()
            || ingest_seconds <= 0.0
            || total_seconds < ingest_seconds
        {
            return Err("request deadlines must be positive and total >= ingestion".into());
        }
        Ok(Self {
            ingest: Duration::from_secs_f64(ingest_seconds),
            total: Duration::from_secs_f64(total_seconds),
        })
    }
}

struct Slot(Arc<AtomicUsize>);
impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}

struct DeadlineGuard {
    cancelled: Arc<AtomicBool>,
    ingest_finished: Arc<AtomicBool>,
    wake: Arc<(Mutex<bool>, Condvar)>,
    upstream: Arc<Mutex<Option<TcpStream>>>,
    tasks: Vec<thread::JoinHandle<()>>,
}

impl DeadlineGuard {
    fn new(client: &TcpStream, limits: Limits) -> Result<Self, String> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let ingest_finished = Arc::new(AtomicBool::new(false));
        let wake = Arc::new((Mutex::new(false), Condvar::new()));
        let upstream = Arc::new(Mutex::new(None));
        let ingest = deadline_task(
            client
                .try_clone()
                .map_err(|error| format!("cannot clone client socket: {error}"))?,
            limits.ingest,
            Arc::clone(&wake),
            Arc::clone(&cancelled),
            Some(Arc::clone(&ingest_finished)),
            None,
        );
        let total = deadline_task(
            client
                .try_clone()
                .map_err(|error| format!("cannot clone client socket: {error}"))?,
            limits.total,
            Arc::clone(&wake),
            Arc::clone(&cancelled),
            None,
            Some(Arc::clone(&upstream)),
        );
        Ok(Self {
            cancelled,
            ingest_finished,
            wake,
            upstream,
            tasks: vec![ingest, total],
        })
    }

    fn finish_ingest(&self) {
        self.ingest_finished.store(true, Ordering::Release);
    }

    fn register_upstream(&self, stream: &TcpStream) -> Result<(), ()> {
        let mut upstream = self.upstream.lock().map_err(|_| ())?;
        *upstream = Some(stream.try_clone().map_err(|_| ())?);
        if self.cancelled.load(Ordering::Acquire) {
            if let Some(stream) = upstream.as_ref() {
                let _ = stream.shutdown(Shutdown::Both);
            }
            return Err(());
        }
        Ok(())
    }

    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

impl Drop for DeadlineGuard {
    fn drop(&mut self) {
        if let Ok(mut done) = self.wake.0.lock() {
            *done = true;
            self.wake.1.notify_all();
        }
        for task in self.tasks.drain(..) {
            let _ = task.join();
        }
    }
}

fn deadline_task(
    client: TcpStream,
    timeout: Duration,
    wake: Arc<(Mutex<bool>, Condvar)>,
    cancelled: Arc<AtomicBool>,
    ingest_finished: Option<Arc<AtomicBool>>,
    upstream: Option<Arc<Mutex<Option<TcpStream>>>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let done = wake
            .0
            .lock()
            .ok()
            .and_then(|done| wake.1.wait_timeout(done, timeout).ok());
        let Some((_guard, wait)) = done else {
            return;
        };
        if !wait.timed_out()
            || ingest_finished
                .as_ref()
                .is_some_and(|finished| finished.load(Ordering::Acquire))
        {
            return;
        }
        cancelled.store(true, Ordering::Release);
        let _ = client.shutdown(Shutdown::Both);
        if let Some(upstream) = upstream.and_then(|slot| {
            slot.lock()
                .ok()
                .and_then(|stream| stream.as_ref().and_then(|stream| stream.try_clone().ok()))
        }) {
            let _ = upstream.shutdown(Shutdown::Both);
        }
    })
}

fn acquire_slot(active: &AtomicUsize) -> bool {
    let mut observed = active.load(Ordering::Acquire);
    loop {
        if observed >= MAX_CLIENTS {
            return false;
        }
        match active.compare_exchange_weak(
            observed,
            observed + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(current) => observed = current,
        }
    }
}

fn tls_config(cert_path: &PathBuf, key_path: &PathBuf) -> Result<ServerConfig, String> {
    let certificates = CertificateDer::pem_file_iter(cert_path)
        .map_err(|error| format!("cannot read TLS certificate PEM: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot parse TLS certificate PEM: {error}"))?;
    if certificates.is_empty() {
        return Err("TLS certificate PEM contains no certificates".into());
    }
    let key = PrivateKeyDer::from_pem_file(key_path)
        .map_err(|error| format!("cannot parse TLS private key PEM: {error}"))?;
    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .map_err(|error| format!("cannot configure TLS server: {error}"))
}

fn serve_tls_connection(
    stream: TcpStream,
    config: Arc<ServerConfig>,
    peer: SocketAddr,
    peer_policy: PeerPolicy,
    upstream_port: u16,
    limits: Limits,
) -> Result<(), String> {
    if !peer_allowed(peer, peer_policy) {
        return Err("non-loopback peer rejected".into());
    }
    stream
        .set_read_timeout(Some(Duration::from_secs(HANDSHAKE_SECONDS)))
        .map_err(|error| format!("cannot bound TLS handshake read: {error}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(HANDSHAKE_SECONDS)))
        .map_err(|error| format!("cannot bound TLS handshake write: {error}"))?;
    let connection =
        ServerConnection::new(config).map_err(|error| format!("TLS setup failed: {error}"))?;
    let mut client = BufReader::new(StreamOwned::new(connection, stream));
    {
        let tls = client.get_mut();
        while tls.conn.is_handshaking() {
            tls.conn
                .complete_io(&mut tls.sock)
                .map_err(|error| format!("TLS handshake failed: {error}"))?;
        }
    }
    let started = Instant::now();
    let deadlines = DeadlineGuard::new(&client.get_ref().sock, limits)?;
    let request = match read_request(&mut client, started, limits, &deadlines) {
        Ok(request) => request,
        Err(ProxyError::TooLarge) => {
            send_error(client.get_mut(), 413, "body too large");
            return Ok(());
        }
        Err(ProxyError::BadRequest) => {
            send_error(client.get_mut(), 400, "invalid request framing");
            return Ok(());
        }
        Err(ProxyError::RouteNotAllowed) => {
            send_error(client.get_mut(), 404, "route not allowed");
            return Ok(());
        }
        Err(ProxyError::Deadline) => return Ok(()),
    };
    let response = match forward(request, upstream_port, started, limits, &deadlines) {
        Ok(response) => response,
        Err(()) => {
            send_error(client.get_mut(), 502, "upstream failure");
            return Ok(());
        }
    };
    send_response(client.get_mut(), response);
    Ok(())
}

#[derive(Clone, Copy)]
enum ProxyError {
    BadRequest,
    RouteNotAllowed,
    TooLarge,
    Deadline,
}

fn read_request(
    client: &mut BufReader<StreamOwned<ServerConnection, TcpStream>>,
    started: Instant,
    limits: Limits,
    deadlines: &DeadlineGuard,
) -> Result<Request, ProxyError> {
    set_client_deadline(client, started, limits.ingest, limits.total)?;
    let request_line = read_line(client)?;
    let mut parts = request_line.split_ascii_whitespace();
    let (method, path, version) = (parts.next(), parts.next(), parts.next());
    if parts.next().is_some() || version != Some("HTTP/1.1") {
        return Err(ProxyError::BadRequest);
    }
    let (method, path) = match (method, path) {
        (Some("GET"), Some("/health")) => ("GET", "/health"),
        (Some("POST"), Some("/layout-parsing")) => ("POST", "/layout-parsing"),
        _ => return Err(ProxyError::RouteNotAllowed),
    };
    let (content_length, content_type) = read_headers(client, method == "GET")?;
    let mut body = vec![0_u8; content_length];
    set_client_deadline(client, started, limits.ingest, limits.total)?;
    client
        .read_exact(&mut body)
        .map_err(|_| ProxyError::BadRequest)?;
    if started.elapsed() > limits.ingest || started.elapsed() > limits.total {
        return Err(ProxyError::Deadline);
    }
    deadlines.finish_ingest();
    Ok(Request {
        method,
        path,
        content_type,
        body,
    })
}

fn read_headers<R: BufRead>(
    client: &mut R,
    allow_missing_length: bool,
) -> Result<(usize, Option<String>), ProxyError> {
    let mut content_length = None;
    let mut content_type = None;
    let mut header_bytes = 0_usize;
    loop {
        let line = read_line(client)?;
        header_bytes = header_bytes
            .checked_add(line.len() + 2)
            .filter(|total| *total <= MAX_HEADERS)
            .ok_or(ProxyError::BadRequest)?;
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(ProxyError::BadRequest);
        };
        let name = name.trim();
        let value = value.trim();
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || value
                .bytes()
                .any(|byte| byte.is_ascii_control() && byte != b'\t')
        {
            return Err(ProxyError::BadRequest);
        }
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(ProxyError::BadRequest);
        }
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.replace(parse_length(value)?).is_some() {
                return Err(ProxyError::BadRequest);
            }
        } else if name.eq_ignore_ascii_case("content-type")
            && content_type.replace(value.to_owned()).is_some()
        {
            return Err(ProxyError::BadRequest);
        }
    }
    match content_length {
        Some(length) => Ok((length, content_type)),
        None if allow_missing_length => Ok((0, content_type)),
        None => Err(ProxyError::BadRequest),
    }
}

fn parse_length(value: &str) -> Result<usize, ProxyError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ProxyError::BadRequest);
    }
    let length = value.parse::<usize>().map_err(|_| ProxyError::TooLarge)?;
    if length > MAX_BODY {
        Err(ProxyError::TooLarge)
    } else {
        Ok(length)
    }
}

fn read_line<R: BufRead>(reader: &mut R) -> Result<String, ProxyError> {
    let mut bytes = Vec::new();
    let count = reader
        .read_until(b'\n', &mut bytes)
        .map_err(|_| ProxyError::BadRequest)?;
    if count == 0 || bytes.len() > MAX_HEADERS || !bytes.ends_with(b"\r\n") {
        return Err(ProxyError::BadRequest);
    }
    bytes.truncate(bytes.len() - 2);
    if bytes.iter().any(|byte| *byte == 0 || *byte > 0x7f) {
        return Err(ProxyError::BadRequest);
    }
    String::from_utf8(bytes).map_err(|_| ProxyError::BadRequest)
}

fn set_client_deadline(
    client: &mut BufReader<StreamOwned<ServerConnection, TcpStream>>,
    started: Instant,
    phase: Duration,
    total: Duration,
) -> Result<(), ProxyError> {
    let remaining = total
        .checked_sub(started.elapsed())
        .ok_or(ProxyError::Deadline)?;
    client
        .get_mut()
        .sock
        .set_read_timeout(Some(phase.min(remaining)))
        .map_err(|_| ProxyError::Deadline)
}

fn forward(
    request: Request,
    upstream_port: u16,
    started: Instant,
    limits: Limits,
    deadlines: &DeadlineGuard,
) -> Result<Response, ()> {
    let remaining = limits.total.checked_sub(started.elapsed()).ok_or(())?;
    let upstream = TcpStream::connect_timeout(
        &SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), upstream_port),
        Duration::from_secs(UPSTREAM_CONNECT_SECONDS).min(remaining),
    )
    .map_err(|_| ())?;
    deadlines.register_upstream(&upstream)?;
    let timeout = Duration::from_secs(UPSTREAM_READ_SECONDS)
        .min(limits.total.checked_sub(started.elapsed()).ok_or(())?);
    upstream.set_read_timeout(Some(timeout)).map_err(|_| ())?;
    upstream.set_write_timeout(Some(timeout)).map_err(|_| ())?;
    let mut upstream = BufReader::new(upstream);
    let mut headers = format!(
        "{} {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: {}\r\nConnection: close\r\n",
        request.method,
        request.path,
        upstream_port,
        request.body.len()
    );
    if let Some(content_type) = request.content_type {
        headers.push_str("Content-Type: ");
        headers.push_str(&content_type);
        headers.push_str("\r\n");
    }
    headers.push_str("\r\n");
    upstream
        .get_mut()
        .write_all(headers.as_bytes())
        .map_err(|_| ())?;
    upstream
        .get_mut()
        .write_all(&request.body)
        .map_err(|_| ())?;
    upstream.get_mut().flush().map_err(|_| ())?;
    let status_line = read_line(&mut upstream).map_err(|_| ())?;
    let mut parts = status_line.split_ascii_whitespace();
    let (version, status) = (parts.next(), parts.next());
    if parts.next().is_none() || version != Some("HTTP/1.1") {
        return Err(());
    }
    let status = status
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|value| (100..=599).contains(value))
        .ok_or(())?;
    let (content_length, content_type) = read_headers(&mut upstream, false).map_err(|_| ())?;
    let mut body = vec![0_u8; content_length];
    upstream.read_exact(&mut body).map_err(|_| ())?;
    if deadlines.cancelled() || started.elapsed() > limits.total {
        return Err(());
    }
    Ok(Response {
        status,
        content_type,
        body,
    })
}

fn send_error(client: &mut StreamOwned<ServerConnection, TcpStream>, status: u16, message: &str) {
    send_response(
        client,
        Response {
            status,
            content_type: Some("text/plain; charset=utf-8".into()),
            body: message.as_bytes().to_vec(),
        },
    );
}

fn send_response(client: &mut StreamOwned<ServerConnection, TcpStream>, response: Response) {
    let reason = match response.status {
        400 => "Bad Request",
        404 => "Not Found",
        413 => "Payload Too Large",
        502 => "Bad Gateway",
        _ => "OK",
    };
    let mut headers = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        reason,
        response.body.len()
    );
    if let Some(content_type) = response.content_type {
        headers.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    headers.push_str("\r\n");
    let _ = client.write_all(headers.as_bytes());
    let _ = client.write_all(&response.body);
    let _ = client.flush();
    let _ = client.sock.shutdown(Shutdown::Both);
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::generate_simple_self_signed;
    use rustls::pki_types::{PrivatePkcs8KeyDer, ServerName};
    use rustls::{ClientConfig, ClientConnection, RootCertStore};
    use std::{fs, io::Cursor};

    fn tls_pair() -> (Arc<ServerConfig>, Arc<ClientConfig>, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let rcgen::CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let certificate = CertificateDer::from(cert.der().to_vec());
        let _server = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![certificate.clone()],
                PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
            )
            .unwrap();
        fs::write(directory.path().join("cert.pem"), cert.pem()).unwrap();
        fs::write(
            directory.path().join("key.pem"),
            signing_key.serialize_pem(),
        )
        .unwrap();
        let loaded = tls_config(
            &directory.path().join("cert.pem"),
            &directory.path().join("key.pem"),
        )
        .unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(certificate).unwrap();
        let client = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        (Arc::new(loaded), Arc::new(client), directory)
    }

    #[test]
    fn tls_loopback_proxy_forwards_only_the_bounded_contract() {
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let upstream_task = thread::spawn(move || {
            let (mut stream, peer) = upstream.accept().unwrap();
            assert!(peer.ip().is_loopback());
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = [0_u8; 1024];
            let length = stream.read(&mut request).unwrap();
            let request = std::str::from_utf8(&request[..length]).unwrap();
            assert!(request.starts_with("POST /layout-parsing HTTP/1.1\r\n"));
            assert!(request.contains("Content-Type: application/json\r\n"));
            assert!(!request.contains("X-Untrusted:"));
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}").unwrap();
        });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (server, client_config, _directory) = tls_pair();
        let proxy_task = thread::spawn(move || {
            let (stream, peer) = listener.accept().unwrap();
            serve_tls_connection(
                stream,
                server,
                peer,
                PeerPolicy::LoopbackOnly,
                upstream_port,
                Limits::new(2.0, 5.0).unwrap(),
            )
            .unwrap();
        });
        let stream = TcpStream::connect(address).unwrap();
        let connection = ClientConnection::new(
            client_config,
            ServerName::try_from("localhost").unwrap().to_owned(),
        )
        .unwrap();
        let mut tls = StreamOwned::new(connection, stream);
        tls.write_all(b"POST /layout-parsing HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nX-Untrusted: dropped\r\nContent-Length: 2\r\n\r\n{}").unwrap();
        let mut response = String::new();
        let _ = tls.read_to_string(&mut response);
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.ends_with("\r\n\r\n{}"));
        proxy_task.join().unwrap();
        upstream_task.join().unwrap();
    }

    #[test]
    fn malformed_transfer_encoding_and_duplicate_lengths_are_rejected() {
        let mut transfer = BufReader::new(Cursor::new(b"Transfer-Encoding: chunked\r\n\r\n"));
        assert!(matches!(
            read_headers(&mut transfer, false),
            Err(ProxyError::BadRequest)
        ));
        let mut duplicate = BufReader::new(Cursor::new(
            b"Content-Length: 1\r\nContent-Length: 1\r\n\r\n",
        ));
        assert!(matches!(
            read_headers(&mut duplicate, false),
            Err(ProxyError::BadRequest)
        ));
        let mut oversized = BufReader::new(Cursor::new(
            format!("Content-Length: {}\r\n\r\n", MAX_BODY + 1).into_bytes(),
        ));
        assert!(matches!(
            read_headers(&mut oversized, false),
            Err(ProxyError::TooLarge)
        ));
    }

    #[test]
    fn arbitrary_non_loopback_bind_is_rejected_but_container_bind_loads_tls() {
        let error = run(Args {
            cert: PathBuf::from("missing-cert.pem"),
            key: PathBuf::from("missing-key.pem"),
            host: Ipv4Addr::new(192, 0, 2, 1).into(),
            port: 0,
            upstream_port: 0,
            ingest_seconds: 1.0,
            total_request_seconds: 1.0,
        })
        .unwrap_err();
        assert_eq!(
            error,
            "proxy host must be loopback or explicit 0.0.0.0 container bind"
        );

        let error = run(Args {
            cert: PathBuf::from("missing-cert.pem"),
            key: PathBuf::from("missing-key.pem"),
            host: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 0,
            upstream_port: 0,
            ingest_seconds: 1.0,
            total_request_seconds: 1.0,
        })
        .unwrap_err();
        assert!(error.starts_with("cannot read TLS certificate PEM:"));
    }
}
