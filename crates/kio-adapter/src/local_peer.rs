//! Authenticated local-adapter transport.
//!
//! A loopback address identifies a route, not the process listening on it.  The
//! local adapters therefore accept only an opaque HTTPS endpoint which owns an
//! explicit, device-local CA file. The CA is re-read before every request and
//! must equal the digest captured for the runtime, so rotation or revocation
//! fails closed instead of changing the peer under an existing grant.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use ureq::tls::{Certificate, RootCerts, TlsConfig, parse_pem};

use crate::http_policy::HttpPolicy;
use crate::{AdapterError, Result};

const MAX_CA_PEM_BYTES: u64 = 1024 * 1024;

/// A local service destination whose server identity is authenticated with a
/// user-provided device CA.  It deliberately has no `From<String>` conversion:
/// raw URLs must never reach a local client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedLocalEndpoint {
    base_url: String,
    ca_pem_path: PathBuf,
    expected_trust_digest: String,
}

impl AuthenticatedLocalEndpoint {
    /// Capture the current private CA digest for a direct fixture endpoint.
    /// Production runtime construction must use [`Self::new_bound`] with the
    /// composition root's pre-captured digest.
    #[cfg(test)]
    pub fn new(base_url: impl AsRef<str>, ca_pem_path: PathBuf) -> Result<Self> {
        let expected_trust_digest = capture_private_local_trust(&ca_pem_path)?;
        Self::new_bound(base_url, ca_pem_path, expected_trust_digest)
    }

    /// Construct an endpoint bound to the CA digest captured by the command
    /// composition root. A later CA rotation is a fail-closed configuration
    /// change, never a silent change of the peer authorized by a grant.
    pub(crate) fn new_bound(
        base_url: impl AsRef<str>,
        ca_pem_path: PathBuf,
        expected_trust_digest: String,
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
        let trust = load_ca_bundle(&ca_pem_path)?;
        if trust.digest != expected_trust_digest {
            return Err(trust_changed(&ca_pem_path));
        }
        Ok(Self {
            base_url,
            ca_pem_path,
            expected_trust_digest,
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

    /// The digest bound to this endpoint, after confirming that the currently
    /// readable CA bytes still match it.
    pub fn trust_digest(&self) -> Result<String> {
        self.current_trust().map(|trust| trust.digest)
    }

    /// Build a no-proxy TLS agent using only the explicit CA bundle.  Rustls
    /// continues to perform normal chain, validity-period and SAN checks.
    pub(crate) fn agent(&self, policy: HttpPolicy) -> Result<ureq::Agent> {
        let trust = self.current_trust()?;
        Ok(ureq::Agent::config_builder()
            .max_redirects(0)
            .https_only(true)
            .proxy(None)
            .http_status_as_error(false)
            .timeout_connect(Some(policy.connect_timeout))
            .timeout_global(Some(
                policy
                    .overall_timeout
                    .min(policy.read_timeout)
                    .min(policy.write_timeout),
            ))
            .tls_config(
                TlsConfig::builder()
                    .root_certs(RootCerts::new_with_certs(&trust.certificates))
                    .build(),
            )
            .build()
            .into())
    }

    fn current_trust(&self) -> Result<TrustBundle> {
        let trust = load_ca_bundle(&self.ca_pem_path)?;
        if trust.digest != self.expected_trust_digest {
            return Err(trust_changed(&self.ca_pem_path));
        }
        Ok(trust)
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
    Ok(load_ca_bundle(path)?.digest)
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

pub(crate) fn validate_local_https_url(input: &str) -> Result<String> {
    let uri: ureq::http::Uri = input.parse().map_err(|_| {
        config_error(
            "KIO-E-LOCAL-PEER-URL-001",
            "offline_api url must be an absolute HTTPS loopback URL",
        )
    })?;
    if uri.scheme_str() != Some("https") || uri.query().is_some() {
        return Err(config_error(
            "KIO-E-LOCAL-PEER-URL-001",
            "offline_api url must use HTTPS and must not include a query",
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
    Ok(input.trim_end_matches('/').to_owned())
}

fn load_ca_bundle(path: &Path) -> Result<TrustBundle> {
    let bytes =
        kio_core::private_fs::read_private_file(path, MAX_CA_PEM_BYTES).map_err(|error| {
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
    #[cfg(any(unix, windows))]
    use std::io::{Read as _, Write as _};
    #[cfg(any(unix, windows))]
    use std::net::{TcpListener, TcpStream};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(any(unix, windows))]
    use std::sync::Arc;

    #[cfg(any(unix, windows))]
    fn private_ca_file(pem: &str) -> (tempfile::TempDir, PathBuf) {
        // The platform temp root can itself be a symlink (notably `/var` on
        // macOS), which the production trust reader must reject.  Place this
        // private fixture beneath the checkout instead.
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
        fs::write(&path, pem).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        #[cfg(windows)]
        private_acl_fixture::protect_owner_only(&path, false).unwrap();
        (directory, path)
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

    #[cfg(any(unix, windows))]
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

    #[cfg(any(unix, windows))]
    fn localhost_server(
        response: &'static str,
    ) -> (String, String, std::thread::JoinHandle<Vec<u8>>) {
        tls_server_for_names(vec!["127.0.0.1".to_owned()], response)
    }

    #[cfg(any(unix, windows))]
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

    #[cfg(any(unix, windows))]
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

    #[cfg(any(unix, windows))]
    type LocalClientCase = (&'static str, fn(AuthenticatedLocalEndpoint) -> Result<()>);

    #[cfg(any(unix, windows))]
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
    fn only_explicit_https_loopback_destinations_are_accepted() {
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
            "https://127.0.0.1:8443"
        );
        assert_eq!(
            validate_local_https_url("https://[::1]:8443").unwrap(),
            "https://[::1]:8443"
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn private_anchor_rejects_missing_relative_and_hardlinked_files() {
        let directory = tempfile::Builder::new()
            .prefix("kio-local-peer-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("peer-ca.pem");
        assert!(AuthenticatedLocalEndpoint::new("https://localhost:8443", path.clone()).is_err());
        assert!(
            AuthenticatedLocalEndpoint::new("https://localhost:8443", PathBuf::from("ca.pem"))
                .is_err()
        );
        fs::write(&path, b"not a certificate").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(AuthenticatedLocalEndpoint::new("https://localhost:8443", path.clone()).is_err());
        fs::hard_link(&path, directory.path().join("peer-ca-copy.pem")).unwrap();
        assert!(kio_core::private_fs::read_private_file(&path, MAX_CA_PEM_BYTES).is_err());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn trusted_peer_completes_tls_and_anchor_rotation_is_reloaded() {
        let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
        let (url, pem, server) = localhost_server(response);
        let (_directory, ca_path) = private_ca_file(&pem);
        let endpoint = AuthenticatedLocalEndpoint::new(&url, ca_path.clone()).unwrap();
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

    #[cfg(any(unix, windows))]
    #[test]
    fn bound_endpoint_refuses_a_valid_ca_rotation_before_any_tls_request() {
        let rcgen::CertifiedKey { cert, .. } =
            rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
        let (_directory, ca_path) = private_ca_file(&cert.pem());
        let digest = capture_private_local_trust(&ca_path).unwrap();
        let endpoint = AuthenticatedLocalEndpoint::new_bound(
            "https://127.0.0.1:8443",
            ca_path.clone(),
            digest,
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

    #[cfg(any(unix, windows))]
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
        let endpoint =
            AuthenticatedLocalEndpoint::new(format!("https://{address}"), ca_path).unwrap();
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

    #[cfg(any(unix, windows))]
    #[test]
    fn wrong_san_is_rejected_before_http_is_available() {
        let response = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let (url, pem, server) = tls_server_for_names(vec!["localhost".to_owned()], response);
        let (_directory, ca_path) = private_ca_file(&pem);
        let endpoint = AuthenticatedLocalEndpoint::new(url, ca_path).unwrap();
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

    #[cfg(any(unix, windows))]
    #[test]
    fn expired_certificate_is_rejected_before_http_is_available() {
        let response = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let (url, pem, server) = expired_server(response);
        let (_directory, ca_path) = private_ca_file(&pem);
        let endpoint = AuthenticatedLocalEndpoint::new(url, ca_path).unwrap();
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

    #[cfg(any(unix, windows))]
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
            let endpoint = AuthenticatedLocalEndpoint::new(url, ca_path).unwrap();
            client(endpoint).unwrap_or_else(|error| panic!("{name}: {error}"));
            let request = server.join().unwrap();
            assert!(
                request.starts_with(b"POST "),
                "{name} did not send an HTTP request after TLS authentication"
            );
        }
    }

    #[cfg(any(unix, windows))]
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
                let endpoint = AuthenticatedLocalEndpoint::new(url, ca_path).unwrap();
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

    #[cfg(any(unix, windows))]
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
                AuthenticatedLocalEndpoint::new(format!("https://{address}"), ca_path.clone())
                    .unwrap();
            let error = client(endpoint).expect_err("closed port must fail");
            assert!(
                matches!(error, AdapterError::Network(_)),
                "{name} must keep connection refusal retryable: {error}"
            );
        }
    }
}
