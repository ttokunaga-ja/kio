//! Shared resource policy for authenticated adapter HTTP exchanges.

use std::io::Read as _;
use std::time::Duration;

use serde_json::Value;

use crate::local_peer::AuthenticatedLocalEndpoint;
use crate::types::ProviderIdempotency;
use crate::{AdapterError, Result};

pub(crate) const MODEL_CATALOG_MAX_BYTES: usize = 1024 * 1024;
pub(crate) const EMBEDDING_RESPONSE_MAX_BYTES: usize = 8 * 1024 * 1024;
/// A rerank response echoes each returned candidate's full text back
/// (`tasks/gpu-reranker-verification.md` §5.2), so the bound scales with
/// `top_n` x chunk size rather than with a vector width. 05 §1.3's
/// `candidate_depth` = 200 against 04 §4.2's 6,000-character chunk ceiling is
/// ~1.2M characters worst case; 8 MiB leaves room for that in UTF-8 without
/// letting a misconfigured server stream unboundedly.
pub(crate) const RERANK_RESPONSE_MAX_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const OCR_RESPONSE_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Canonical ureq 3 response type. Keeping the transport body explicit makes
/// the bounded-read boundary visible at every adapter call site.
pub(crate) type HttpResponse = ureq::http::Response<ureq::Body>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HttpPolicy {
    pub(crate) connect_timeout: Duration,
    pub(crate) read_timeout: Duration,
    pub(crate) write_timeout: Duration,
    pub(crate) overall_timeout: Duration,
}

impl Default for HttpPolicy {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(30),
            read_timeout: Duration::from_secs(30),
            write_timeout: Duration::from_secs(30),
            overall_timeout: Duration::from_secs(300),
        }
    }
}

impl HttpPolicy {
    /// D7: the policy for an execution mode whose `timeout_seconds` was set in
    /// `[adapter.policy.<execution_mode>]` (07 §7).
    ///
    /// **`overall_timeout` alone would do nothing.** `authenticated_agent`
    /// configures one deadline from [`effective_overall_timeout`], which is the
    /// *minimum* of overall / read / write — so raising overall to 1800 while
    /// read and write stay at 30 still gives a 30 second wall. The whole point
    /// of D7 is that a CPU-inference VLM sends nothing for minutes while it
    /// works, which is exactly what a 30 second read timeout kills. So the read
    /// and write limits move with it.
    ///
    /// `connect_timeout` deliberately does not. Establishing a TCP connection to
    /// a loopback address does not get slower because the model is large, and a
    /// long connect timeout turns "the server is not running" into a very slow
    /// hang instead of a quick error.
    pub(crate) fn with_timeout_seconds(seconds: u64) -> Self {
        let timeout = Duration::from_secs(seconds);
        Self {
            connect_timeout: Duration::from_secs(30),
            read_timeout: timeout,
            write_timeout: timeout,
            overall_timeout: timeout,
        }
    }
}

/// Authenticated provider requests use HTTPS without proxies or redirects,
/// matching the online runtime trust identity.
pub(crate) fn authenticated_agent(policy: HttpPolicy) -> ureq::Agent {
    // ureq 3's global timeout covers DNS through response-body completion.
    // Keep the strictest cap as the global deadline and explicitly return
    // status responses: callers retain their authenticated 401/403, 402, and
    // 429 classifications (including Retry-After) without losing the body or
    // headers to the transport error type.
    let effective_overall_timeout = effective_overall_timeout(policy);
    let config = ureq::Agent::config_builder()
        .https_only(true)
        .proxy(None)
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_connect(Some(policy.connect_timeout))
        .timeout_global(Some(effective_overall_timeout));
    #[cfg(test)]
    let config = match test_tls::current_config() {
        Some(tls) => config.tls_config(tls),
        None => config,
    };
    config.build().into()
}

/// The only transport constructor for an `offline_api` request.  Unlike the
/// online helper above it uses an explicit device CA instead of web roots.
/// Both constructors disable proxy inheritance.
pub(crate) fn authenticated_local_agent(
    endpoint: &AuthenticatedLocalEndpoint,
    policy: HttpPolicy,
) -> Result<ureq::Agent> {
    endpoint.agent(policy)
}

/// Convert every non-success response into the adapter-specific error while
/// retaining direct access to response headers before the bounded body read
/// begins. In particular, redirects are returned (not followed) by the agent
/// and must still fail closed here.
pub(crate) fn require_success(
    response: HttpResponse,
    map_status: impl FnOnce(&HttpResponse) -> AdapterError,
) -> Result<HttpResponse> {
    if !response.status().is_success() {
        Err(map_status(&response))
    } else {
        Ok(response)
    }
}

fn effective_overall_timeout(policy: HttpPolicy) -> Duration {
    policy
        .overall_timeout
        .min(policy.read_timeout)
        .min(policy.write_timeout)
}

/// Read a response body under `max_bytes` with the shared identity-encoding
/// posture (reject any transparent decompression, precheck `Content-Length`,
/// hard-stop the wire read at the ceiling). Returns the raw bytes for
/// non-JSON payloads (e.g. the Batch output-file JSONL, 07 §5.5);
/// [`read_json_bounded`] layers JSON parsing on top for everything else.
pub(crate) fn read_bytes_bounded(
    mut response: HttpResponse,
    max_bytes: usize,
    context: &str,
) -> Result<Vec<u8>> {
    if response
        .headers()
        .get("Content-Encoding")
        .and_then(|encoding| encoding.to_str().ok())
        .is_some_and(|encoding| !encoding.eq_ignore_ascii_case("identity"))
    {
        return Err(AdapterError::ContractViolation(format!(
            "{context} uses unsupported content encoding"
        )));
    }
    if let Some(content_length) = response
        .headers()
        .get("Content-Length")
        .and_then(|value| value.to_str().ok())
        && content_length
            .parse::<u64>()
            .ok()
            .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(AdapterError::ContractViolation(format!(
            "{context} exceeds {max_bytes} bytes"
        )));
    }

    let read_limit = max_bytes
        .checked_add(1)
        .ok_or_else(|| AdapterError::ContractViolation("response limit overflow".to_owned()))?;
    let mut body = Vec::with_capacity(max_bytes.min(64 * 1024));
    response
        .body_mut()
        .as_reader()
        .take(read_limit as u64)
        .read_to_end(&mut body)
        .map_err(|err| AdapterError::Network(format!("{context} read failed: {err}")))?;
    if body.len() > max_bytes {
        return Err(AdapterError::ContractViolation(format!(
            "{context} exceeds {max_bytes} bytes"
        )));
    }
    Ok(body)
}

pub(crate) fn read_json_bounded(
    response: HttpResponse,
    max_bytes: usize,
    context: &str,
) -> Result<Value> {
    let body = read_bytes_bounded(response, max_bytes, context)?;
    parse_json_bytes_bounded(&body, max_bytes, context)
}

/// QA16 (step4b-contract-tests-p3a.md §F, 07 §4 L290): parse an HTTP
/// `Retry-After` header value into milliseconds. Only the numeric
/// delay-seconds form (RFC 9110 §9.2.3) is supported — the rarer HTTP-date
/// form is not parsed and degrades to `None`, same as a missing header
/// (`AdapterRun.retry_after_ms` is documented `optional`; callers already
/// fall back to the existing exponential backoff when it is absent).
/// Negative, non-finite, or overflowing input also returns `None` rather than
/// a fabricated delay.
pub(crate) fn parse_retry_after_ms(header_value: &str) -> Option<u64> {
    let seconds: f64 = header_value.trim().parse().ok()?;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    let millis = (seconds * 1000.0).round();
    if millis > u64::MAX as f64 {
        return None;
    }
    Some(millis as u64)
}

/// QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): resolve the
/// idempotency header a sync provider call must carry, or fail closed.
/// `NotProvided` never inspects `idempotency_token` — the ledger's own
/// §5.4/§5.8 2-phase record (a `batch_requests` row) is the sole dedup guard,
/// and Adapter-layer idempotency is never required unconditionally (04 §5.5:
/// "Adapter 層への idempotency_key 一律要求はしない"). `HttpHeader(name)`
/// REQUIRES a token; a missing one is fail-closed (04 §5.5 「要求」), never a
/// silent unauthenticated send. Factored out as a pure function (no `ureq`
/// dependency) so both real clients' header assembly is unit-testable
/// without a live HTTP request.
pub(crate) fn resolve_idempotency_header(
    provider_idempotency: &ProviderIdempotency,
    idempotency_token: Option<&str>,
) -> Result<Option<(String, String)>> {
    match provider_idempotency {
        ProviderIdempotency::NotProvided => Ok(None),
        ProviderIdempotency::HttpHeader(name) => {
            let token = idempotency_token.ok_or_else(|| {
                AdapterError::ContractViolation(
                    "provider declares an idempotency key but the caller supplied no token — \
                     04 §5.5 要求 fail-closed"
                        .to_owned(),
                )
            })?;
            Ok(Some((name.clone(), token.to_owned())))
        }
    }
}

pub(crate) fn parse_json_bytes_bounded(
    body: &[u8],
    max_bytes: usize,
    context: &str,
) -> Result<Value> {
    if body.len() > max_bytes {
        return Err(AdapterError::ContractViolation(format!(
            "{context} exceeds {max_bytes} bytes"
        )));
    }
    serde_json::from_slice(body)
        .map_err(|err| AdapterError::ContractViolation(format!("invalid {context} JSON: {err}")))
}

/// TLS fixtures keep the production HTTPS/proxy/redirect policy, replacing only
/// the trust roots within an explicit test-thread scope. No production override
/// or certificate-verification bypass is exposed.
#[cfg(test)]
pub(crate) mod test_tls {
    use std::cell::RefCell;
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use ureq::tls::{Certificate, RootCerts, TlsConfig};

    thread_local! {
        static CONFIG: RefCell<Option<TlsConfig>> = const { RefCell::new(None) };
    }

    pub(super) fn current_config() -> Option<TlsConfig> {
        CONFIG.with(|config| config.borrow().clone())
    }

    pub(crate) struct TrustGuard {
        previous: Option<TlsConfig>,
        _thread: std::marker::PhantomData<std::rc::Rc<()>>,
    }

    impl Drop for TrustGuard {
        fn drop(&mut self) {
            CONFIG.with(|config| *config.borrow_mut() = self.previous.take());
        }
    }

    pub(crate) struct Fixture {
        server: Arc<rustls::ServerConfig>,
        client: TlsConfig,
    }

    impl Fixture {
        pub(crate) fn new() -> Self {
            let rcgen::CertifiedKey { cert, signing_key } =
                rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
            let certificate = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
            let key = rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der());
            let server = rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![certificate], key.into())
                .unwrap();
            let client = TlsConfig::builder()
                .root_certs(RootCerts::new_with_certs(&[Certificate::from_der(
                    cert.der(),
                )
                .to_owned()]))
                .build();
            Self {
                server: Arc::new(server),
                client,
            }
        }

        pub(crate) fn trust(&self) -> TrustGuard {
            let previous = CONFIG.with(|config| config.replace(Some(self.client.clone())));
            TrustGuard {
                previous,
                _thread: std::marker::PhantomData,
            }
        }

        pub(crate) fn accept(
            &self,
            listener: &TcpListener,
        ) -> rustls::StreamOwned<rustls::ServerConnection, TcpStream> {
            const TIMEOUT: Duration = Duration::from_secs(5);
            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + TIMEOUT;
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "TLS fixture accept timed out");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("TLS fixture accept failed: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(TIMEOUT)).unwrap();
            stream.set_write_timeout(Some(TIMEOUT)).unwrap();
            let connection = rustls::ServerConnection::new(self.server.clone()).unwrap();
            rustls::StreamOwned::new(connection, stream)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D7's whole effect depends on this. `authenticated_agent` builds one
    /// deadline from the *minimum* of overall/read/write, so a policy that
    /// raised only `overall_timeout` would still wall at the default 30 second
    /// read timeout and the configured value would do nothing — the silent
    /// no-op D7 exists to avoid.
    #[test]
    fn a_configured_timeout_is_not_clamped_by_the_default_read_and_write_limits() {
        let policy = HttpPolicy::with_timeout_seconds(1800);
        assert_eq!(effective_overall_timeout(policy), Duration::from_secs(1800));
    }

    /// The default is unchanged by D7: min(300, 30, 30).
    #[test]
    fn the_default_policy_still_walls_at_thirty_seconds() {
        assert_eq!(
            effective_overall_timeout(HttpPolicy::default()),
            Duration::from_secs(30)
        );
    }

    /// Connect deliberately does not follow. A local model being slow to think
    /// does not make the TCP handshake slow, and stretching connect turns "the
    /// server is not running" into a long hang instead of a fast error.
    #[test]
    fn connect_timeout_does_not_follow_the_configured_timeout() {
        let policy = HttpPolicy::with_timeout_seconds(1800);
        assert_eq!(policy.connect_timeout, Duration::from_secs(30));
    }

    #[test]
    fn retry_after_ms_parses_numeric_seconds_form() {
        assert_eq!(parse_retry_after_ms("30"), Some(30_000));
        assert_eq!(parse_retry_after_ms("0"), Some(0));
        assert_eq!(parse_retry_after_ms("  120  "), Some(120_000));
        assert_eq!(parse_retry_after_ms("2.5"), Some(2_500));
    }

    #[test]
    fn retry_after_ms_rejects_invalid_or_unsupported_forms() {
        // Negative, non-finite, and the (rare, unsupported) HTTP-date form all
        // degrade to `None` rather than a fabricated delay.
        assert_eq!(parse_retry_after_ms("-1"), None);
        assert_eq!(parse_retry_after_ms("NaN"), None);
        assert_eq!(parse_retry_after_ms("Wed, 21 Oct 2026 07:28:00 GMT"), None);
        assert_eq!(parse_retry_after_ms(""), None);
        assert_eq!(parse_retry_after_ms("inf"), None);
    }

    #[test]
    fn bounded_json_accepts_exact_limit_and_rejects_one_over() {
        let exact = br#"{"ok":true}"#;
        assert_eq!(
            parse_json_bytes_bounded(exact, exact.len(), "test response").unwrap()["ok"],
            true
        );

        let err = parse_json_bytes_bounded(exact, exact.len() - 1, "test response").unwrap_err();
        assert!(err.to_string().contains("exceeds"));

        let response = ureq::http::Response::builder()
            .status(200)
            .body(ureq::Body::builder().data(exact.to_vec()))
            .expect("synthetic response");
        assert_eq!(
            read_json_bounded(response, exact.len(), "test response").unwrap()["ok"],
            true
        );
        let response = ureq::http::Response::builder()
            .status(200)
            .body(ureq::Body::builder().data(exact.to_vec()))
            .expect("synthetic response");
        assert!(read_json_bounded(response, exact.len() - 1, "test response").is_err());
    }

    #[test]
    fn authenticated_agent_effective_config_matches_online_trust_identity() {
        // Exercise inherited proxy settings without mutating the environment
        // shared by concurrent tests. The child runs this exact test once.
        const CHILD: &str = "KIO_TEST_ONLINE_PROXY_CONFIG_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "http_policy::tests::authenticated_agent_effective_config_matches_online_trust_identity",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("HTTPS_PROXY", "http://127.0.0.1:9")
                .env("https_proxy", "http://127.0.0.1:9")
                .env("ALL_PROXY", "http://127.0.0.1:9")
                .env("all_proxy", "http://127.0.0.1:9")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "proxy-environment child failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let agent = authenticated_agent(HttpPolicy::default());
        let config = agent.config();
        assert!(config.https_only());
        assert!(config.proxy().is_none());
        assert_eq!(config.max_redirects(), 0);
        assert!(matches!(
            config.tls_config().root_certs(),
            ureq::tls::RootCerts::WebPki
        ));
    }

    #[test]
    fn authenticated_agent_rejects_http_before_connecting() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let error = authenticated_agent(HttpPolicy::default())
            .post(&format!(
                "http://{}/capture",
                listener.local_addr().unwrap()
            ))
            .header("x-provider-secret", "synthetic")
            .send("synthetic private payload")
            .expect_err("online authenticated transport requires HTTPS");
        assert!(matches!(error, ureq::Error::RequireHttpsOnly(_)));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn authenticated_agent_rejects_redirect_responses() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let redirect_target = TcpListener::bind("127.0.0.1:0").unwrap();
        redirect_target.set_nonblocking(true).unwrap();
        let redirect_address = redirect_target.local_addr().unwrap();
        let tls = std::sync::Arc::new(test_tls::Fixture::new());
        let _trust = tls.trust();
        let server_tls = tls.clone();
        let server = std::thread::spawn(move || {
            let mut stream = server_tls.accept(&listener);
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).unwrap();
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{redirect_address}/capture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
        });

        let result = authenticated_agent(HttpPolicy::default())
            .get(&format!("https://{address}/start"))
            .header("x-provider-secret", "synthetic")
            .call();
        let response = result.expect("redirect is returned instead of followed");
        assert_eq!(response.status(), 302);
        assert!(
            require_success(response, |_| AdapterError::Network(
                "redirect response".to_owned()
            ))
            .is_err(),
            "a returned redirect must not enter provider response parsing"
        );
        server.join().unwrap();
        let err = redirect_target
            .accept()
            .expect_err("redirect target must receive no credential-bearing request");
        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
    }

    #[test]
    fn authenticated_agent_uses_strictest_io_timeout_as_overall_deadline() {
        let policy = HttpPolicy {
            connect_timeout: Duration::from_secs(7),
            read_timeout: Duration::from_secs(2),
            write_timeout: Duration::from_secs(3),
            overall_timeout: Duration::from_secs(5),
        };
        assert_eq!(effective_overall_timeout(policy), Duration::from_secs(2));
        assert_eq!(policy.connect_timeout, Duration::from_secs(7));
    }

    #[test]
    fn authenticated_agent_bounds_slow_response_reads() {
        use std::io::Read as _;
        use std::net::{Shutdown, TcpListener};
        use std::sync::mpsc;
        use std::time::Instant;

        const READ_TIMEOUT: Duration = Duration::from_millis(100);
        const CLIENT_RETURN_DEADLINE: Duration = Duration::from_secs(1);
        const CLEANUP_DEADLINE: Duration = Duration::from_secs(5);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (request_seen_tx, request_seen_rx) = mpsc::channel();
        let (release_server_tx, release_server_rx) = mpsc::channel();
        let tls = std::sync::Arc::new(test_tls::Fixture::new());
        let _trust = tls.trust();
        let server_tls = tls.clone();
        let server = std::thread::spawn(move || {
            let mut stream = server_tls.accept(&listener);
            stream
                .sock
                .set_read_timeout(Some(CLEANUP_DEADLINE))
                .unwrap();
            let mut request = [0_u8; 2048];
            let request_bytes = stream.read(&mut request).unwrap();
            assert!(request_bytes > 0, "client must send a request");
            request_seen_tx.send(()).unwrap();

            // Keep the response read blocked until the test has observed the
            // client-side timeout. The cleanup deadline prevents a panic in
            // the test thread from leaving this server thread blocked.
            let _ = release_server_rx.recv_timeout(CLEANUP_DEADLINE);
            let _ = stream.sock.shutdown(Shutdown::Both);
        });
        let policy = HttpPolicy {
            connect_timeout: Duration::from_secs(2),
            read_timeout: READ_TIMEOUT,
            write_timeout: Duration::from_secs(2),
            // This deliberately exceeds the assertion deadline so the test
            // fails if the dedicated response/read timeout is removed.
            overall_timeout: CLEANUP_DEADLINE,
        };

        let (client_result_tx, client_result_rx) = mpsc::channel();
        let client = std::thread::spawn(move || {
            let _trust = tls.trust();
            let started = Instant::now();
            let result = authenticated_agent(policy)
                .get(&format!("https://{address}/slow"))
                .call();
            client_result_tx.send((started.elapsed(), result)).unwrap();
        });

        let request_seen = request_seen_rx.recv_timeout(CLEANUP_DEADLINE);
        let client_result = request_seen
            .as_ref()
            .map_err(|err| err.to_string())
            .and_then(|_| {
                client_result_rx
                    .recv_timeout(CLIENT_RETURN_DEADLINE)
                    .map_err(|err| err.to_string())
            });

        // Release the intentionally blocked server only after observing the
        // client result (or the assertion deadline), then join both threads.
        let _ = release_server_tx.send(());
        client.join().unwrap();
        server.join().unwrap();

        request_seen.expect("server must receive the local synthetic request");
        let (elapsed, result) = client_result
            .expect("client must enforce the read timeout before the server is released");
        assert!(
            elapsed <= CLIENT_RETURN_DEADLINE,
            "client returned after {elapsed:?}, exceeding {CLIENT_RETURN_DEADLINE:?}"
        );
        let err = result.expect_err("slow response must time out");
        assert!(matches!(err, ureq::Error::Timeout(_)));
    }

    // QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): the pure
    // idempotency-header resolution function's 3 cases — see kio-adapter unit
    // tests (a)/(b)/(c) in the implementation report.
    #[test]
    fn qa13_http_header_with_token_resolves_the_declared_header() {
        let resolved = resolve_idempotency_header(
            &ProviderIdempotency::HttpHeader("Idempotency-Key".to_owned()),
            Some("intent-token-abc"),
        )
        .unwrap();
        assert_eq!(
            resolved,
            Some(("Idempotency-Key".to_owned(), "intent-token-abc".to_owned()))
        );
    }

    #[test]
    fn qa13_http_header_without_token_is_a_fail_closed_contract_violation() {
        let error = resolve_idempotency_header(
            &ProviderIdempotency::HttpHeader("Idempotency-Key".to_owned()),
            None,
        )
        .unwrap_err();
        assert!(
            matches!(error, AdapterError::ContractViolation(_)),
            "a provider that declares an idempotency key must fail closed when the \
             caller supplies no token, not silently send unauthenticated: {error:?}"
        );
    }

    #[test]
    fn qa13_not_provided_ignores_a_present_token_and_never_errors() {
        // NotProvided must ignore the field entirely — with a token present...
        assert_eq!(
            resolve_idempotency_header(&ProviderIdempotency::NotProvided, Some("intent-token-abc"))
                .unwrap(),
            None
        );
        // ...and with no token, the common real-adapter case (04 §5.5: neither
        // built-in adapter's pinned endpoint offers a provider idempotency key).
        assert_eq!(
            resolve_idempotency_header(&ProviderIdempotency::NotProvided, None).unwrap(),
            None
        );
    }

    #[test]
    fn bounded_reader_rejects_compressed_content_encoding() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let tls = std::sync::Arc::new(test_tls::Fixture::new());
        let _trust = tls.trust();
        let server_tls = tls.clone();
        let server = std::thread::spawn(move || {
            let mut stream = server_tls.accept(&listener);
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                )
                .unwrap();
        });
        let response = authenticated_agent(HttpPolicy::default())
            .get(&format!("https://{address}/compressed"))
            .call()
            .unwrap();
        let err = read_json_bounded(response, 1024, "test response").unwrap_err();
        assert!(err.to_string().contains("unsupported content encoding"));
        server.join().unwrap();
    }
}
