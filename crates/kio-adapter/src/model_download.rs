//! Streaming HTTP transport for the explicitly requested EG2 artifact.

use std::{io::Read, time::Instant};

use kio_core::control::ControlHandle;

use crate::http_policy::{
    HttpPolicy, HttpResponse, authenticated_agent, require_identity_encoding,
};
use crate::{AdapterError, Result};

pub fn open(
    source: &str,
    expected_bytes: u64,
    control: &ControlHandle,
) -> Result<impl Read + use<>> {
    let policy = HttpPolicy::with_timeout_seconds(1800);
    #[cfg(debug_assertions)]
    let fixture = match &control.debug().adapters.model_source {
        kio_core::test_control::Selector::Unset => None,
        kio_core::test_control::Selector::Known(url) => Some(url),
        kio_core::test_control::Selector::Unknown(_) => {
            return Err(AdapterError::InvalidInput(
                "invalid model test source".into(),
            ));
        }
    };
    #[cfg(not(debug_assertions))]
    let _ = control;

    // This public GET carries no credentials or scope data. The fixed
    // Hugging Face resolve URL can redirect to an HTTPS artifact CDN.
    #[cfg(debug_assertions)]
    if let Some(url) = fixture {
        let peer = crate::local_peer::LocalEndpoint::new_http_bound(url, control.clone())?;
        let agent = crate::http_policy::local_peer_agent(&peer, policy)?;
        return response_reader(
            get_response(&agent, &peer.endpoint_url("/"))?,
            expected_bytes,
        );
    }
    response_reader(
        get_response(&authenticated_agent(policy), source)?,
        expected_bytes,
    )
}

fn get_response(agent: &ureq::Agent, source: &str) -> Result<HttpResponse> {
    use ureq::http::{StatusCode, header};

    let mut redirects_left = 5;
    let mut url = url::Url::parse(source)
        .map_err(|_| AdapterError::InvalidInput("invalid model download URL".into()))?;
    let origin = url.origin();
    let https_only = agent.config().https_only();
    let started = Instant::now();
    let timeout = agent.config().timeouts().global;

    loop {
        let remaining = timeout
            .map(|limit| {
                limit
                    .checked_sub(started.elapsed())
                    .ok_or_else(|| AdapterError::Network("model download deadline exceeded".into()))
            })
            .transpose()?;
        let response = agent
            .get(url.as_str())
            .header(header::ACCEPT_ENCODING, "identity")
            .header(header::CONNECTION, "close")
            .config()
            .timeout_global(remaining)
            .build()
            .call()
            // A CDN redirect URL can contain a temporary signature.
            .map_err(|_| AdapterError::Network("model download request failed".into()))?;
        if !matches!(
            response.status(),
            StatusCode::MOVED_PERMANENTLY
                | StatusCode::FOUND
                | StatusCode::SEE_OTHER
                | StatusCode::TEMPORARY_REDIRECT
                | StatusCode::PERMANENT_REDIRECT
        ) {
            return Ok(response);
        }
        if redirects_left == 0 {
            return Err(AdapterError::Network(
                "model download exceeded the redirect limit".into(),
            ));
        }

        let next = response
            .headers()
            .get(header::LOCATION)
            .and_then(|location| location.to_str().ok())
            .and_then(|location| url.join(location).ok());
        // Discard the transport at the response headers. Reading the body of
        // a redirect would put it outside the pinned artifact's byte budget.
        drop(response);
        let mut next = next.ok_or_else(|| {
            AdapterError::Network("model download redirect has an invalid Location".into())
        })?;
        if (https_only && next.scheme() != "https")
            || (!https_only && next.origin() != origin)
            || !next.username().is_empty()
            || next.password().is_some()
        {
            return Err(AdapterError::Network(
                "model download redirect has an unsafe destination".into(),
            ));
        }
        next.set_fragment(None);
        url = next;
        redirects_left -= 1;
    }
}

fn response_reader(response: HttpResponse, expected_bytes: u64) -> Result<impl Read> {
    if response.status() != ureq::http::StatusCode::OK {
        return Err(AdapterError::Network(format!(
            "model download returned HTTP {}",
            response.status().as_u16()
        )));
    }
    require_identity_encoding(&response, "model download")?;
    if let Some(length) = response.headers().get(ureq::http::header::CONTENT_LENGTH)
        && length
            .to_str()
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            != Some(expected_bytes)
    {
        return Err(AdapterError::ContractViolation(format!(
            "model download size differs from the expected {expected_bytes} bytes"
        )));
    }
    Ok(response.into_body().into_reader())
}

#[cfg(test)]
mod tests {
    use super::*;

    use ureq::unversioned::resolver::DefaultResolver;
    use ureq::unversioned::transport::{
        Buffers, ConnectionDetails, Connector, LazyBuffers, NextTimeout, Transport,
    };

    #[derive(Debug)]
    struct ScriptedConnections;

    impl Connector for ScriptedConnections {
        type Out = OneResponse;

        fn connect(
            &self,
            details: &ConnectionDetails,
            _: Option<()>,
        ) -> std::result::Result<Option<Self::Out>, ureq::Error> {
            let response = match details.uri.path() {
                "/start" => redirect("/artifact"),
                "/downgrade" => redirect("http://127.0.0.1/unexpected"),
                "/artifact" | "/five-5" => {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\ntest model"
                        .to_vec()
                }
                path if path.starts_with("/five-") || path.starts_with("/six-") => {
                    let (prefix, number) = path.rsplit_once('-').unwrap();
                    let hop = number.parse::<u32>().unwrap();
                    assert!(hop <= 5, "request beyond the five-redirect limit");
                    redirect(&format!("{prefix}-{}", hop + 1))
                }
                path => panic!("unexpected request to {path}"),
            };
            Ok(Some(OneResponse {
                buffers: LazyBuffers::new(1024, 1024),
                response: Some(response),
            }))
        }
    }

    fn redirect(location: &str) -> Vec<u8> {
        format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 999999999\r\nConnection: close\r\n\r\n")
            .into_bytes()
    }

    #[derive(Debug)]
    struct OneResponse {
        buffers: LazyBuffers,
        response: Option<Vec<u8>>,
    }

    impl Transport for OneResponse {
        fn buffers(&mut self) -> &mut dyn Buffers {
            &mut self.buffers
        }

        fn transmit_output(
            &mut self,
            _: usize,
            _: NextTimeout,
        ) -> std::result::Result<(), ureq::Error> {
            Ok(())
        }

        fn await_input(&mut self, _: NextTimeout) -> std::result::Result<bool, ureq::Error> {
            let response = self.response.take().expect("redirect body was read");
            self.buffers.input_append_buf()[..response.len()].copy_from_slice(&response);
            self.buffers.input_appended(response.len());
            Ok(true)
        }

        fn is_open(&mut self) -> bool {
            self.response.is_some()
        }

        fn is_tls(&self) -> bool {
            true
        }
    }

    fn scripted_agent() -> ureq::Agent {
        let configured = authenticated_agent(HttpPolicy::default());
        ureq::Agent::with_parts(
            configured.config().clone(),
            ScriptedConnections,
            DefaultResolver::default(),
        )
    }

    #[test]
    fn model_download_follows_redirect_without_reading_its_body() {
        let agent = scripted_agent();
        let response = get_response(&agent, "https://127.0.0.1/start").unwrap();
        let mut reader = response_reader(response, 10).unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"test model");
    }

    #[test]
    fn model_download_enforces_https_and_the_five_redirect_limit() {
        let agent = scripted_agent();
        assert!(get_response(&agent, "https://127.0.0.1/five-0").is_ok());
        assert!(
            get_response(&agent, "https://127.0.0.1/six-0")
                .unwrap_err()
                .to_string()
                .contains("redirect limit")
        );
        assert!(
            get_response(&agent, "https://127.0.0.1/downgrade")
                .unwrap_err()
                .to_string()
                .contains("unsafe destination")
        );
    }

    #[test]
    fn model_download_admits_only_a_complete_identity_response() {
        let response = |status, encoding, length| {
            ureq::http::Response::builder()
                .status(status)
                .header("Content-Encoding", encoding)
                .header("Content-Length", length)
                .body(ureq::Body::builder().data(b"test model"))
                .unwrap()
        };
        let mut source = response_reader(response(200_u16, "identity", "10"), 10).unwrap();
        let mut bytes = Vec::new();
        source.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"test model");

        for (status, encoding, length) in [
            (206, "identity", "10"),
            (302, "identity", "10"),
            (200, "gzip", "10"),
            (200, "identity", "9"),
            (200, "identity", "11"),
        ] {
            assert!(response_reader(response(status, encoding, length), 10).is_err());
        }
    }
}
