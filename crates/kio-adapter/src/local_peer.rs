//! Explicit HTTP or managed-CA HTTPS transport for loopback adapters.
//!
//! A loopback address identifies a route, not the process listening on it.
//! The HTTP route is intentionally unauthenticated and never reads a CA. HTTPS
//! uses only the device-managed CA snapshot captured for the runtime and checks
//! it before every request. Both routes pin loopback names to loopback socket
//! addresses and disable proxies and redirects.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use ureq::tls::{Certificate, RootCerts, TlsConfig, parse_pem};
use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

use crate::http_policy::HttpPolicy;
use crate::tool_lock::{ManagedLocalTrustProvider, ManagedLocalTrustSnapshot};
use crate::{AdapterError, Result};

const MAX_CA_PEM_BYTES: u64 = 1024 * 1024;
pub const HTTP_UNAUTHENTICATED_TRUST_BINDING: &str = "http:unauthenticated";

/// A loopback destination. HTTPS endpoints retain a managed CA snapshot;
/// HTTP endpoints carry no trust material and are explicitly unauthenticated.
/// This type deliberately has no `From<String>` conversion: unvalidated URLs
/// must never reach a local client.
#[derive(Debug, Clone)]
pub struct LocalEndpoint {
    base_url: String,
    https_trust: Option<HttpsTrust>,
    control: kio_core::control::ControlHandle,
}

#[derive(Debug, Clone)]
struct HttpsTrust {
    ca_pem_path: PathBuf,
    expected_trust_digest: String,
    managed_local_trust: Option<std::sync::Arc<dyn ManagedLocalTrustProvider>>,
}

impl PartialEq for HttpsTrust {
    fn eq(&self, other: &Self) -> bool {
        self.ca_pem_path == other.ca_pem_path
            && self.expected_trust_digest == other.expected_trust_digest
    }
}
impl Eq for HttpsTrust {}

impl PartialEq for LocalEndpoint {
    fn eq(&self, other: &Self) -> bool {
        self.base_url == other.base_url && self.https_trust == other.https_trust
    }
}
impl Eq for LocalEndpoint {}

impl LocalEndpoint {
    /// Capture the current private CA digest for a direct fixture endpoint.
    /// Production runtime construction must use [`Self::new_https_bound`] with the
    /// composition root's pre-captured digest.
    #[cfg(test)]
    pub fn new_https(base_url: impl AsRef<str>, ca_pem_path: PathBuf) -> Result<Self> {
        let expected_trust_digest = capture_private_local_trust(&ca_pem_path)?;
        Self::new_https_bound(
            base_url,
            ca_pem_path,
            expected_trust_digest,
            kio_core::control::ControlHandle::default(),
        )
    }

    pub(crate) fn new_http_bound(
        base_url: impl AsRef<str>,
        control: kio_core::control::ControlHandle,
    ) -> Result<Self> {
        let base_url = validate_local_http_url(base_url.as_ref())?;
        Ok(Self {
            base_url,
            https_trust: None,
            control,
        })
    }

    /// Construct an endpoint bound to the CA digest captured by the command
    /// composition root. A later CA rotation is a fail-closed configuration
    /// change, never a silent change of the peer authorized by a grant.
    pub(crate) fn new_https_bound(
        base_url: impl AsRef<str>,
        ca_pem_path: PathBuf,
        expected_trust_digest: String,
        control: kio_core::control::ControlHandle,
    ) -> Result<Self> {
        let base_url = validate_local_https_url(base_url.as_ref())?;
        if !ca_pem_path.is_absolute() {
            return Err(config_error(
                "KIO-E-LOCAL-PEER-CA-PATH-001",
                "adapter.policy.offline_api.ca_pem_path must be an absolute path",
            ));
        }
        // Construction is also a fail-closed validation boundary.  The agent
        // deliberately reloads it later, but no client may be created with an
        // absent or unsafe trust anchor.
        let trust = load_ca_bundle(&ca_pem_path, &control)?;
        if trust.digest != expected_trust_digest {
            return Err(trust_changed(&ca_pem_path));
        }
        Ok(Self {
            base_url,
            https_trust: Some(HttpsTrust {
                ca_pem_path,
                expected_trust_digest,
                managed_local_trust: None,
            }),
            control,
        })
    }

    /// Retain the managed trust lease and read its private leaf from the
    /// acquired directory handle at construction and before every HTTPS use.
    pub(crate) fn new_https_managed_bound(
        base_url: impl AsRef<str>,
        snapshot: ManagedLocalTrustSnapshot,
        provider: std::sync::Arc<dyn ManagedLocalTrustProvider>,
        control: kio_core::control::ControlHandle,
    ) -> Result<Self> {
        let base_url = validate_local_https_url(base_url.as_ref())?;
        if !snapshot.path.is_absolute()
            || snapshot.directory.path().join(&snapshot.leaf) != snapshot.path
        {
            return Err(config_error(
                "KIO-E-LOCAL-PEER-CA-PATH-001",
                "managed local CA snapshot has an invalid path binding",
            ));
        }
        let trust = load_managed_ca_bundle(&snapshot)?;
        if trust.digest != snapshot.digest {
            return Err(trust_changed(&snapshot.path));
        }
        Ok(Self {
            base_url,
            https_trust: Some(HttpsTrust {
                ca_pem_path: snapshot.path,
                expected_trust_digest: snapshot.digest,
                managed_local_trust: Some(provider),
            }),
            control,
        })
    }

    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Stable, non-secret recipient binding used by the egress approval layer.
    #[must_use]
    pub fn destination_binding(&self) -> &str {
        &self.base_url
    }

    /// Append an adapter-owned route while preserving the configured URL as
    /// the approval identity. A terminal slash affects identity but is
    /// removed only for joining the fixed endpoint suffix.
    pub fn endpoint_url(&self, suffix: &str) -> String {
        format!("{}{suffix}", self.base_url.trim_end_matches('/'))
    }

    /// The digest bound to this endpoint, after confirming that the currently
    /// readable CA bytes still match it.
    pub fn trust_digest(&self) -> Result<String> {
        self.current_trust().map(|trust| trust.digest)
    }

    /// Build a no-proxy/no-redirect agent. HTTPS uses only the explicit CA
    /// bundle, with normal chain, validity-period and SAN validation. HTTP
    /// sends plaintext and never reads CA state.
    pub(crate) fn agent(&self, policy: HttpPolicy) -> Result<ureq::Agent> {
        let mut builder = ureq::Agent::config_builder()
            .max_redirects(0)
            .proxy(None)
            .http_status_as_error(false)
            .timeout_connect(Some(policy.connect_timeout))
            .timeout_global(Some(
                policy
                    .overall_timeout
                    .min(policy.read_timeout)
                    .min(policy.write_timeout),
            ));
        if self.https_trust.is_some() {
            let trust = self.current_trust()?;
            builder = builder.tls_config(
                TlsConfig::builder()
                    .root_certs(RootCerts::new_with_certs(&trust.certificates))
                    .build(),
            );
        }
        Ok(ureq::Agent::with_parts(
            builder.build(),
            DefaultConnector::default(),
            LiteralLoopbackResolver,
        ))
    }

    fn current_trust(&self) -> Result<TrustBundle> {
        let Some(expected) = &self.https_trust else {
            return Err(config_error(
                "KIO-E-LOCAL-PEER-TRUST-BINDING-001",
                "HTTP local endpoint has no TLS trust snapshot",
            ));
        };
        let trust = if let Some(provider) = &expected.managed_local_trust {
            let snapshot = provider.acquire().map_err(AdapterError::RuntimeTrust)?;
            if snapshot.path != expected.ca_pem_path
                || snapshot.directory.path().join(&snapshot.leaf) != snapshot.path
                || snapshot.digest != expected.expected_trust_digest
            {
                return Err(trust_changed(&expected.ca_pem_path));
            }
            load_managed_ca_bundle(&snapshot)?
        } else {
            load_ca_bundle(&expected.ca_pem_path, &self.control)?
        };
        if trust.digest != expected.expected_trust_digest {
            return Err(trust_changed(&expected.ca_pem_path));
        }
        Ok(trust)
    }
}

#[derive(Debug)]
struct LiteralLoopbackResolver;

impl Resolver for LiteralLoopbackResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        _config: &ureq::config::Config,
        _timeout: NextTimeout,
    ) -> std::result::Result<ResolvedSocketAddrs, ureq::Error> {
        let port = uri.port_u16().ok_or(ureq::Error::HostNotFound)?;
        let mut addresses = self.empty();
        match uri.host() {
            Some("localhost") => {
                // `localhost` is defined as a loopback name. Pin it here rather
                // than consulting system resolution, while retaining both
                // families for services bound to either loopback interface.
                addresses.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));
                addresses.push(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port));
            }
            Some("127.0.0.1") => {
                addresses.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));
            }
            Some("::1" | "[::1]") => {
                addresses.push(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port));
            }
            _ => return Err(ureq::Error::HostNotFound),
        }
        Ok(addresses)
    }
}

struct TrustBundle {
    certificates: Vec<Certificate<'static>>,
    digest: String,
}

/// Capture the non-secret digest of a currently valid private local CA bundle.
/// The returned value is suitable for `AdapterRuntimeSettings`; it contains no
/// certificate bytes and must be matched by every later local-peer request.
pub fn capture_private_local_trust(path: &Path) -> Result<String> {
    capture_private_local_trust_with_control(path, &kio_core::control::ControlHandle::default())
}

pub fn capture_private_local_trust_with_control(
    path: &Path,
    control: &kio_core::control::ControlHandle,
) -> Result<String> {
    Ok(load_ca_bundle(path, control)?.digest)
}

/// Compute the authenticated local-CA identity from already-read PEM bytes.
///
/// The caller remains responsible for obtaining these bytes through an
/// owner-private no-follow read.  Keeping parsing here makes registration bind
/// the same DER-normalized identity that the TLS endpoint verifies at runtime,
/// without reopening a mutable source path between its protected read and the
/// managed snapshot publication.
pub fn local_trust_digest_from_pem_bytes(bytes: &[u8]) -> Result<String> {
    Ok(load_ca_bundle_bytes(bytes)?.digest)
}

fn trust_changed(path: &Path) -> AdapterError {
    config_error(
        "KIO-E-LOCAL-PEER-TRUST-CHANGED-001",
        format!(
            "adapter.policy.offline_api.ca_pem_path changed after runtime trust capture: {}",
            path.display()
        ),
    )
}

pub(crate) fn validate_local_http_url(input: &str) -> Result<String> {
    validate_local_url_for_scheme(input, "http")
}

pub(crate) fn validate_local_https_url(input: &str) -> Result<String> {
    validate_local_url_for_scheme(input, "https")
}

pub fn validate_local_url(input: &str) -> Result<String> {
    let uri: ureq::http::Uri = input.parse().map_err(|_| {
        config_error(
            "KIO-E-LOCAL-PEER-URL-001",
            "offline_api url must be an absolute HTTP or HTTPS loopback URL",
        )
    })?;
    match uri.scheme_str() {
        Some("http") => validate_local_http_url(input),
        Some("https") => validate_local_https_url(input),
        _ => Err(config_error(
            "KIO-E-LOCAL-PEER-URL-001",
            "offline_api url must use HTTP or HTTPS",
        )),
    }
}

/// Check that a non-secret endpoint trust identity matches its transport.
/// HTTP is unauthenticated and has one fixed marker; HTTPS must carry a
/// managed lifecycle identity rather than that marker.
pub fn validate_local_trust_binding(url: &str, binding: &str) -> Result<()> {
    let normalized = validate_local_url(url)?;
    let uri: ureq::http::Uri = normalized.parse().map_err(|_| {
        config_error(
            "KIO-E-LOCAL-PEER-URL-001",
            "offline_api URL could not be parsed after validation",
        )
    })?;
    let valid = match uri.scheme_str() {
        Some("http") => binding == HTTP_UNAUTHENTICATED_TRUST_BINDING,
        Some("https") => !binding.is_empty() && binding != HTTP_UNAUTHENTICATED_TRUST_BINDING,
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(config_error(
            "KIO-E-LOCAL-PEER-TRUST-BINDING-001",
            "local endpoint trust identity does not match its transport",
        ))
    }
}

fn validate_local_url_for_scheme(input: &str, expected_scheme: &str) -> Result<String> {
    let uri: ureq::http::Uri = input.parse().map_err(|_| {
        config_error(
            "KIO-E-LOCAL-PEER-URL-001",
            "offline_api url must be an absolute HTTP or HTTPS loopback URL",
        )
    })?;
    if uri.scheme_str() != Some(expected_scheme) || uri.query().is_some() || input.contains('#') {
        return Err(config_error(
            "KIO-E-LOCAL-PEER-URL-001",
            "offline_api url uses the wrong scheme or includes a query or fragment",
        ));
    }
    let authority = uri.authority().ok_or_else(|| {
        config_error(
            "KIO-E-LOCAL-PEER-URL-001",
            "offline_api url must include a loopback authority",
        )
    })?;
    if authority.as_str().contains('@') {
        return Err(config_error(
            "KIO-E-LOCAL-PEER-URL-001",
            "offline_api url must not include userinfo",
        ));
    }
    match uri.host() {
        Some("127.0.0.1" | "localhost" | "::1" | "[::1]") => {}
        _ => {
            return Err(config_error(
                "KIO-E-LOCAL-PEER-URL-001",
                "offline_api url host must be a literal loopback destination",
            ));
        }
    }
    if authority.port_u16().is_none_or(|port| port == 0) {
        return Err(config_error(
            "KIO-E-LOCAL-PEER-URL-001",
            "offline_api url must include an explicit port",
        ));
    }
    // Canonicalize only the scheme so downstream transport classification and
    // approval identity agree. Keep the literal host, explicit port spelling,
    // and configured path byte-for-byte; in particular, `/model` and
    // `/model/` are distinct approvals.
    let authority_start = input.find("://").ok_or_else(|| {
        config_error(
            "KIO-E-LOCAL-PEER-URL-001",
            "offline_api url must be an absolute HTTP or HTTPS loopback URL",
        )
    })? + 3;
    Ok(format!("{expected_scheme}://{}", &input[authority_start..]))
}

fn load_ca_bundle(path: &Path, control: &kio_core::control::ControlHandle) -> Result<TrustBundle> {
    let read = || -> kio_core::Result<Vec<u8>> {
        let parent = path
            .parent()
            .ok_or_else(|| kio_core::KioError::invalid_usage("local CA path has no parent"))?;
        let leaf = path
            .file_name()
            .ok_or_else(|| kio_core::KioError::invalid_usage("local CA path has no leaf"))?;
        let retained =
            kio_core::private_fs::retain_external_directory_with_control(parent, control)?;
        kio_core::private_fs::read_private_file_at_path(
            &retained,
            Path::new(leaf),
            MAX_CA_PEM_BYTES,
        )
    };
    let bytes = read().map_err(|error| {
        config_error(
            "KIO-E-LOCAL-PEER-CA-PROTECTION-001",
            format!("{}: {}", error.error_code(), error.message()),
        )
    })?;
    load_ca_bundle_bytes(&bytes)
}

fn load_managed_ca_bundle(snapshot: &ManagedLocalTrustSnapshot) -> Result<TrustBundle> {
    let bytes = (|| -> kio_core::Result<Vec<u8>> {
        kio_core::private_fs::verify_private_directory_handle(&snapshot.directory)?;
        kio_core::private_fs::read_private_file_at_path(
            &snapshot.directory,
            &snapshot.leaf,
            MAX_CA_PEM_BYTES,
        )
    })()
    .map_err(|error| {
        config_error(
            "KIO-E-LOCAL-PEER-CA-PROTECTION-001",
            format!("{}: {}", error.error_code(), error.message()),
        )
    })?;
    load_ca_bundle_bytes(&bytes)
}

fn load_ca_bundle_bytes(bytes: &[u8]) -> Result<TrustBundle> {
    let mut certificates = Vec::new();
    for item in parse_pem(bytes) {
        match item.map_err(|error| {
            config_error(
                "KIO-E-LOCAL-PEER-CA-PEM-001",
                format!("local peer CA PEM is malformed: {error}"),
            )
        })? {
            ureq::tls::PemItem::Certificate(certificate) => certificates.push(certificate),
            ureq::tls::PemItem::PrivateKey(_) => {
                return Err(config_error(
                    "KIO-E-LOCAL-PEER-CA-PEM-001",
                    "local peer CA bundle must not contain a private key",
                ));
            }
            _ => {
                return Err(config_error(
                    "KIO-E-LOCAL-PEER-CA-PEM-001",
                    "local peer CA bundle contains an unsupported PEM item",
                ));
            }
        }
    }
    if certificates.is_empty() {
        return Err(config_error(
            "KIO-E-LOCAL-PEER-CA-PEM-001",
            "local peer CA bundle contains no certificates",
        ));
    }
    let mut hash = Sha256::new();
    for certificate in &certificates {
        hash.update((certificate.der().len() as u64).to_be_bytes());
        hash.update(certificate.der());
    }
    let digest = hash.finalize();
    let digest = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(TrustBundle {
        certificates,
        digest: format!("sha256:{digest}"),
    })
}

fn config_error(code: &'static str, message: impl Into<String>) -> AdapterError {
    AdapterError::LocalPeerConfig {
        code,
        message: message.into(),
    }
}

/// Return whether ureq reported a TLS-authentication failure for the local
/// peer. `ureq` may preserve rustls failures as a direct error variant or wrap
/// them in `std::io::Error`; only the latter's typed source chain is inspected.
/// Plain I/O failures (including connection refusal) remain retryable network
/// failures.
pub(crate) fn is_local_peer_tls_error(error: &ureq::Error) -> bool {
    match error {
        ureq::Error::Tls(_)
        | ureq::Error::Pem(_)
        | ureq::Error::Rustls(_)
        | ureq::Error::TlsRequired => true,
        ureq::Error::Io(error) => {
            let mut source = error
                .get_ref()
                .map(|source| source as &(dyn std::error::Error + 'static));
            while let Some(error) = source {
                if error.is::<rustls::Error>() {
                    return true;
                }
                source = error.source();
            }
            false
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    #[test]
    fn local_http_requires_an_explicit_literal_loopback_destination() {
        for (url, expected) in [
            ("http://127.0.0.1:8080/base/", "http://127.0.0.1:8080/base/"),
            ("http://localhost:8080/base", "http://localhost:8080/base"),
            ("http://[::1]:8080/base", "http://[::1]:8080/base"),
        ] {
            assert_eq!(validate_local_http_url(url).unwrap(), expected, "{url}");
        }
        assert_ne!(
            validate_local_http_url("http://127.0.0.1:8080/model").unwrap(),
            validate_local_http_url("http://127.0.0.1:8080/model/").unwrap(),
            "configured path slash is part of approval identity"
        );
        for url in [
            "http://127.0.0.1/base",
            "http://127.0.0.1:0/base",
            "http://localhost.evil.example:8080/base",
            "http://127.0.0.1@evil.example:8080/base",
            "http://127.0.0.1:8080/base?redirect=1",
            "http://127.0.0.1:8080/base#fragment",
            "http://192.0.2.1:8080/base",
        ] {
            assert!(validate_local_http_url(url).is_err(), "must reject {url}");
        }
    }

    #[test]
    fn validated_local_destination_canonicalizes_only_the_scheme() {
        assert_eq!(
            validate_local_url("HTTP://127.0.0.1:8123/model/").unwrap(),
            "http://127.0.0.1:8123/model/"
        );
        assert_eq!(
            validate_local_url("HTTPS://[::1]:8443/model").unwrap(),
            "https://[::1]:8443/model"
        );
    }

    #[test]
    fn literal_resolver_returns_only_loopback_candidates_for_allowed_hosts() {
        let resolver = LiteralLoopbackResolver;
        let resolve = |host: &str| {
            let uri: ureq::http::Uri = format!("http://{host}:8123/").parse().unwrap();
            resolver
                .resolve(
                    &uri,
                    &ureq::config::Config::default(),
                    NextTimeout {
                        after: std::time::Duration::from_secs(1).into(),
                        reason: ureq::Timeout::Resolve,
                    },
                )
                .unwrap()
                .iter()
                .copied()
                .collect::<Vec<_>>()
        };

        assert_eq!(
            resolve("localhost"),
            vec![
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8123),
                SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 8123),
            ]
        );
        assert_eq!(
            resolve("127.0.0.1"),
            vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8123)]
        );
        assert_eq!(
            resolve("[::1]"),
            vec![SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 8123)]
        );
    }

    #[test]
    fn endpoint_trust_binding_marker_must_match_transport_scheme() {
        assert!(
            validate_local_trust_binding(
                "http://127.0.0.1:8080/model/",
                HTTP_UNAUTHENTICATED_TRUST_BINDING,
            )
            .is_ok()
        );
        assert!(
            validate_local_trust_binding(
                "https://127.0.0.1:8443/model",
                "local-ca:v1:g1:sha256:fixture",
            )
            .is_ok()
        );
        assert!(
            validate_local_trust_binding(
                "https://127.0.0.1:8443/model",
                HTTP_UNAUTHENTICATED_TRUST_BINDING,
            )
            .is_err()
        );
        assert!(
            validate_local_trust_binding(
                "http://127.0.0.1:8080/model",
                "local-ca:v1:g1:sha256:fixture",
            )
            .is_err()
        );
    }

    fn private_ca_file(pem: &str) -> (tempfile::TempDir, PathBuf) {
        // Exercise the system temp layout, including macOS's /var alias.
        let directory = tempfile::Builder::new()
            .prefix("kio-local-peer-")
            .tempdir()
            .unwrap();
        #[cfg(unix)]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        #[cfg(windows)]
        private_acl_fixture::protect_owner_only(directory.path(), true).unwrap();
        let path = directory.path().join("peer-ca.pem");
        fs::write(&path, pem).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        #[cfg(windows)]
        private_acl_fixture::protect_owner_only(&path, false).unwrap();
        (directory, path)
    }

    fn plaintext_server(response: String) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let task = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "HTTP fixture accept timed out"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(error) => panic!("HTTP fixture accept failed: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            loop {
                let read = stream.read(&mut chunk).unwrap();
                assert_ne!(read, 0, "HTTP peer closed before request headers");
                request.extend_from_slice(&chunk[..read]);
                assert!(
                    request.len() <= 64 * 1024,
                    "HTTP headers exceeded fixture cap"
                );
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
            request
        });
        (format!("http://127.0.0.1:{}/base", address.port()), task)
    }

    #[cfg(windows)]
    mod private_acl_fixture {
        use std::{
            fs::OpenOptions,
            mem,
            os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
            path::Path,
            ptr,
        };
        use windows_sys::Win32::{
            Foundation::{CloseHandle, HANDLE},
            Security::Authorization::{SE_FILE_OBJECT, SetSecurityInfo},
            Security::{
                ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAceEx,
                DACL_SECURITY_INFORMATION, GetLengthSid, GetTokenInformation, InitializeAcl,
                OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSID, TOKEN_QUERY,
                TOKEN_USER, TokenUser,
            },
            Storage::FileSystem::{
                FILE_ALL_ACCESS, FILE_READ_ATTRIBUTES, READ_CONTROL, WRITE_DAC, WRITE_OWNER,
            },
            System::Threading::{GetCurrentProcess, OpenProcessToken},
        };

        pub fn protect_owner_only(path: &Path, directory: bool) -> std::io::Result<()> {
            let owner = CurrentUserSid::current()?;
            let mut acl = owner_only_acl(&owner)?;
            let mut options = OpenOptions::new();
            options
                .read(true)
                .access_mode(FILE_READ_ATTRIBUTES | READ_CONTROL | WRITE_DAC | WRITE_OWNER);
            if directory {
                options.custom_flags(0x0200_0000);
            }
            let file = options.open(path)?;
            let status = unsafe {
                SetSecurityInfo(
                    file.as_raw_handle() as HANDLE,
                    SE_FILE_OBJECT,
                    OWNER_SECURITY_INFORMATION
                        | DACL_SECURITY_INFORMATION
                        | PROTECTED_DACL_SECURITY_INFORMATION,
                    owner.as_psid(),
                    ptr::null_mut(),
                    acl.as_mut_ptr().cast::<ACL>(),
                    ptr::null_mut(),
                )
            };
            if status == 0 {
                Ok(())
            } else {
                Err(std::io::Error::from_raw_os_error(status as i32))
            }
        }

        fn owner_only_acl(owner: &CurrentUserSid) -> std::io::Result<Vec<usize>> {
            let ace_size =
                mem::size_of::<ACCESS_ALLOWED_ACE>() - mem::size_of::<u32>() + owner.len();
            let acl_size = mem::size_of::<ACL>() + ace_size;
            let mut acl = vec![0_usize; acl_size.div_ceil(mem::size_of::<usize>())];
            if unsafe { InitializeAcl(acl.as_mut_ptr().cast(), acl_size as u32, ACL_REVISION) } == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            if unsafe {
                AddAccessAllowedAceEx(
                    acl.as_mut_ptr().cast(),
                    ACL_REVISION,
                    0,
                    FILE_ALL_ACCESS,
                    owner.as_psid(),
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(acl)
        }

        struct CurrentUserSid {
            bytes: Vec<usize>,
        }
        impl CurrentUserSid {
            fn current() -> std::io::Result<Self> {
                let mut token = ptr::null_mut();
                if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let result = Self::from_token(token);
                unsafe { CloseHandle(token) };
                result
            }
            fn from_token(token: HANDLE) -> std::io::Result<Self> {
                let mut needed = 0_u32;
                let _ = unsafe {
                    GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut needed)
                };
                if needed == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let mut bytes = vec![0_usize; (needed as usize).div_ceil(mem::size_of::<usize>())];
                if unsafe {
                    GetTokenInformation(
                        token,
                        TokenUser,
                        bytes.as_mut_ptr().cast(),
                        needed,
                        &mut needed,
                    )
                } == 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(Self { bytes })
            }
            fn as_psid(&self) -> PSID {
                unsafe { (*(self.bytes.as_ptr().cast::<TOKEN_USER>())).User.Sid }
            }
            fn len(&self) -> usize {
                unsafe { GetLengthSid(self.as_psid()) as usize }
            }
        }
    }

    fn read_http_request(
        stream: &mut rustls::StreamOwned<rustls::ServerConnection, TcpStream>,
    ) -> Vec<u8> {
        let mut request = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let Ok(read) = stream.read(&mut chunk) else {
                return Vec::new();
            };
            if read == 0 {
                return request;
            }
            request.extend_from_slice(&chunk[..read]);
            let Some(headers_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
            else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..headers_end]).to_ascii_lowercase();
            let content_length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if request.len() >= headers_end + 4 + content_length {
                return request;
            }
        }
    }

    fn localhost_server(
        response: &'static str,
    ) -> (String, String, std::thread::JoinHandle<Vec<u8>>) {
        tls_server_for_names(vec!["127.0.0.1".to_owned()], response)
    }

    fn tls_server_for_names(
        names: Vec<String>,
        response: &'static str,
    ) -> (String, String, std::thread::JoinHandle<Vec<u8>>) {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(names).unwrap();
        let certificate = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
        let private_key = rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der());
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate], private_key.into())
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let task = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let connection = rustls::ServerConnection::new(Arc::new(config)).unwrap();
            let mut stream = rustls::StreamOwned::new(connection, stream);
            let request = read_http_request(&mut stream);
            if request.is_empty() {
                return request;
            }
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            request
        });
        (format!("https://{address}"), cert.pem(), task)
    }

    fn expired_server(
        response: &'static str,
    ) -> (String, String, std::thread::JoinHandle<Vec<u8>>) {
        let mut params = rcgen::CertificateParams::new(vec!["127.0.0.1".to_owned()]).unwrap();
        params.not_before = rcgen::date_time_ymd(2020, 1, 1);
        params.not_after = rcgen::date_time_ymd(2021, 1, 1);
        let signing_key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&signing_key).unwrap();
        let certificate = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
        let private_key = rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der());
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate], private_key.into())
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let task = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let connection = rustls::ServerConnection::new(Arc::new(config)).unwrap();
            let mut stream = rustls::StreamOwned::new(connection, stream);
            let request = read_http_request(&mut stream);
            if request.is_empty() {
                return request;
            }
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            request
        });
        (format!("https://{address}"), cert.pem(), task)
    }

    type LocalClientCase = (&'static str, fn(LocalEndpoint) -> Result<()>);

    fn local_client_cases() -> Vec<LocalClientCase> {
        use crate::local_embedding::{EnvLocalEmbeddingClient, LocalEmbeddingClient};
        use crate::local_ocr_markdownize::{EnvLocalOcrClient, LayoutFileType, LocalOcrClient};
        use crate::local_rerank::{EnvLocalRerankClient, LocalRerankClient};
        use serde_json::json;

        vec![
            ("embedding", |endpoint| {
                EnvLocalEmbeddingClient::new(endpoint, "model", None)
                    .embed_messages(json!([]))
                    .map(|_| ())
            }),
            ("ocr", |endpoint| {
                EnvLocalOcrClient::new(endpoint, None)
                    .layout_parse("dGVzdA==", LayoutFileType::Pdf)
                    .map(|_| ())
            }),
            ("rerank", |endpoint| {
                EnvLocalRerankClient::new(endpoint, "model", None)
                    .rerank_documents("query", &[], 0)
                    .map(|_| ())
            }),
        ]
    }

    #[test]
    fn https_url_validator_requires_explicit_https_loopback_destination() {
        for rejected in [
            "http://127.0.0.1:8443",
            "https://example.test:8443",
            "https://127.0.0.1:0",
            "https://127.0.0.1:8443?leak=1",
            "https://user@127.0.0.1:8443",
            "unix:/tmp/kio.sock",
            "https://localhost",
        ] {
            assert!(validate_local_https_url(rejected).is_err(), "{rejected}");
        }
        assert_eq!(
            validate_local_https_url("https://127.0.0.1:8443/").unwrap(),
            "https://127.0.0.1:8443/"
        );
        assert_eq!(
            validate_local_https_url("https://[::1]:8443").unwrap(),
            "https://[::1]:8443"
        );
    }

    #[test]
    fn private_anchor_rejects_missing_relative_and_hardlinked_files() {
        #[cfg(unix)]
        let directory = tempfile::Builder::new()
            .prefix("kio-local-peer-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        #[cfg(windows)]
        let directory = tempfile::Builder::new()
            .prefix("kio-local-peer-")
            .tempdir()
            .unwrap();
        #[cfg(unix)]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        #[cfg(windows)]
        private_acl_fixture::protect_owner_only(directory.path(), true).unwrap();
        let path = directory.path().join("peer-ca.pem");
        assert!(LocalEndpoint::new_https("https://localhost:8443", path.clone()).is_err());
        assert!(
            LocalEndpoint::new_https("https://localhost:8443", PathBuf::from("ca.pem")).is_err()
        );
        fs::write(&path, b"not a certificate").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        #[cfg(windows)]
        private_acl_fixture::protect_owner_only(&path, false).unwrap();
        assert!(LocalEndpoint::new_https("https://localhost:8443", path.clone()).is_err());
        fs::hard_link(&path, directory.path().join("peer-ca-copy.pem")).unwrap();
        assert!(kio_core::private_fs::read_private_file(&path, MAX_CA_PEM_BYTES).is_err());
    }

    #[test]
    fn unauthenticated_http_uses_loopback_without_ca_and_does_not_follow_redirects() {
        let response =
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_owned();
        let (ip_url, server) = plaintext_server(response);
        let url = ip_url.replacen("127.0.0.1", "localhost", 1);
        let endpoint =
            LocalEndpoint::new_http_bound(&url, kio_core::control::ControlHandle::default())
                .unwrap();
        assert_eq!(endpoint.destination_binding(), url);
        let response = endpoint
            .agent(HttpPolicy::default())
            .unwrap()
            .get(endpoint.endpoint_url("/status"))
            .call()
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let request = server.join().unwrap();
        assert!(request.starts_with(b"GET /base/status "), "{request:?}");

        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let target_url = format!(
            "http://127.0.0.1:{}/target",
            target.local_addr().unwrap().port()
        );
        let redirect = format!(
            "HTTP/1.1 302 Found\r\nLocation: {target_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let (url, server) = plaintext_server(redirect);
        let endpoint =
            LocalEndpoint::new_http_bound(&url, kio_core::control::ControlHandle::default())
                .unwrap();
        let response = endpoint
            .agent(HttpPolicy::default())
            .unwrap()
            .get(endpoint.endpoint_url("/redirect"))
            .call()
            .unwrap();
        assert_eq!(response.status().as_u16(), 302);
        assert!(server.join().unwrap().starts_with(b"GET /base/redirect "));
        assert!(matches!(
            target.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
    }

    #[test]
    fn trusted_peer_completes_tls_and_anchor_rotation_is_reloaded() {
        let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
        let (url, pem, server) = localhost_server(response);
        let (_directory, ca_path) = private_ca_file(&pem);
        let endpoint = LocalEndpoint::new_https(&url, ca_path.clone()).unwrap();
        let digest = endpoint.trust_digest().unwrap();
        let response = endpoint
            .agent(HttpPolicy::default())
            .unwrap()
            .get(&url)
            .call()
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        assert!(!server.join().unwrap().is_empty());

        // The same endpoint must not retain the now-revoked CA bytes.
        fs::write(&ca_path, b"not a certificate").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&ca_path, fs::Permissions::from_mode(0o600)).unwrap();
        #[cfg(windows)]
        private_acl_fixture::protect_owner_only(&ca_path, false).unwrap();
        assert!(endpoint.agent(HttpPolicy::default()).is_err());
        assert_ne!(endpoint.trust_digest().unwrap_err().error_code(), "");
        assert!(!digest.is_empty());
    }

    #[test]
    fn bound_endpoint_refuses_a_valid_ca_rotation_before_any_tls_request() {
        let rcgen::CertifiedKey { cert, .. } =
            rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
        let (_directory, ca_path) = private_ca_file(&cert.pem());
        let digest = capture_private_local_trust(&ca_path).unwrap();
        let endpoint = LocalEndpoint::new_https_bound(
            "https://127.0.0.1:8443",
            ca_path.clone(),
            digest,
            kio_core::control::ControlHandle::default(),
        )
        .unwrap();
        let rcgen::CertifiedKey { cert, .. } =
            rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
        fs::write(&ca_path, cert.pem()).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&ca_path, fs::Permissions::from_mode(0o600)).unwrap();
        #[cfg(windows)]
        private_acl_fixture::protect_owner_only(&ca_path, false).unwrap();
        let error = match endpoint.agent(HttpPolicy::default()) {
            Ok(_) => panic!("rotated CA must be rejected before TLS configuration"),
            Err(error) => error,
        };
        assert_eq!(error.error_code(), "KIO-E-LOCAL-PEER-CONFIG-001");
        assert!(
            error
                .to_string()
                .contains("KIO-E-LOCAL-PEER-TRUST-CHANGED-001")
        );
    }

    #[test]
    fn wrong_ca_fails_before_an_http_body_is_sent() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let observer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = vec![0_u8; 16 * 1024];
            let read = stream.read(&mut bytes).unwrap();
            bytes.truncate(read);
            bytes
        });
        let rcgen::CertifiedKey { cert, .. } =
            rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
        let (_directory, ca_path) = private_ca_file(&cert.pem());
        let endpoint = LocalEndpoint::new_https(format!("https://{address}"), ca_path).unwrap();
        let error = endpoint
            .agent(HttpPolicy::default())
            .unwrap()
            .post(endpoint.base_url())
            .send("secret-body")
            .unwrap_err();
        assert!(matches!(
            error,
            ureq::Error::Io(_) | ureq::Error::ConnectionFailed
        ));
        let received = observer.join().unwrap();
        assert!(
            !received
                .windows(b"secret-body".len())
                .any(|window| window == b"secret-body")
        );
    }

    #[test]
    fn wrong_san_is_rejected_before_http_is_available() {
        let response = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let (url, pem, server) = tls_server_for_names(vec!["localhost".to_owned()], response);
        let (_directory, ca_path) = private_ca_file(&pem);
        let endpoint = LocalEndpoint::new_https(url, ca_path).unwrap();
        assert!(
            endpoint
                .agent(HttpPolicy::default())
                .unwrap()
                .post(endpoint.base_url())
                .send("secret-body")
                .is_err()
        );
        assert!(server.join().unwrap().is_empty());
    }

    #[test]
    fn expired_certificate_is_rejected_before_http_is_available() {
        let response = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let (url, pem, server) = expired_server(response);
        let (_directory, ca_path) = private_ca_file(&pem);
        let endpoint = LocalEndpoint::new_https(url, ca_path).unwrap();
        assert!(
            endpoint
                .agent(HttpPolicy::default())
                .unwrap()
                .post(endpoint.base_url())
                .send("secret-body")
                .is_err()
        );
        assert!(server.join().unwrap().is_empty());
    }

    #[test]
    fn every_local_client_posts_only_after_the_shared_peer_is_authenticated() {
        let cases = local_client_cases();
        let responses = [
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 30\r\nConnection: close\r\n\r\n{\"data\":[{\"embedding\":[1.0]}]}",
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 14\r\nConnection: close\r\n\r\n{\"results\":[]}",
        ];
        for ((name, client), response) in cases.iter().zip(responses) {
            let (url, pem, server) = localhost_server(response);
            let (_directory, ca_path) = private_ca_file(&pem);
            let endpoint = LocalEndpoint::new_https(url, ca_path).unwrap();
            client(endpoint).unwrap_or_else(|error| panic!("{name}: {error}"));
            let request = server.join().unwrap();
            assert!(
                request.starts_with(b"POST "),
                "{name} did not send an HTTP request after TLS authentication"
            );
        }
    }

    #[test]
    fn every_local_client_classifies_bad_tls_peers_as_permanent_before_request_body() {
        let response = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        for failure in ["wrong CA", "wrong SAN", "expired certificate"] {
            for (client_name, client) in local_client_cases() {
                // Each connection needs its own peer because the fixture has a
                // one-connection listener. Recreate it after the first client.
                let (url, pem, server) = match failure {
                    "wrong CA" => {
                        let (url, _server_pem, server) = localhost_server(response);
                        let rcgen::CertifiedKey { cert, .. } =
                            rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()])
                                .unwrap();
                        (url, cert.pem(), server)
                    }
                    "wrong SAN" => tls_server_for_names(vec!["localhost".to_owned()], response),
                    "expired certificate" => expired_server(response),
                    _ => unreachable!(),
                };
                let (_directory, ca_path) = private_ca_file(&pem);
                let endpoint = LocalEndpoint::new_https(url, ca_path).unwrap();
                let error = client(endpoint).expect_err("bad TLS peer must fail");
                assert!(
                    matches!(error, AdapterError::LocalPeerAuth(_)),
                    "{client_name} must classify {failure} as permanent: {error}"
                );
                assert!(
                    server.join().unwrap().is_empty(),
                    "{client_name} sent an HTTP request to {failure}"
                );
            }
        }
    }

    #[test]
    fn local_connection_refusal_remains_retryable_network_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let rcgen::CertifiedKey { cert, .. } =
            rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
        let (_directory, ca_path) = private_ca_file(&cert.pem());
        for (name, client) in local_client_cases() {
            let endpoint =
                LocalEndpoint::new_https(format!("https://{address}"), ca_path.clone()).unwrap();
            let error = client(endpoint).expect_err("closed port must fail");
            assert!(
                matches!(error, AdapterError::Network(_)),
                "{name} must keep connection refusal retryable: {error}"
            );
        }
    }
}
