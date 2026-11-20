//! Streaming HTTP transport for the explicitly requested EG2 artifact.

use std::io::Read;

use kio_core::control::ControlHandle;

use crate::http_policy::{
    AuthenticatedHttpClient, HttpPolicy, HttpResponse, require_identity_encoding,
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
    let agent = AuthenticatedHttpClient::new(policy).agent_with_redirect_limit(5);
    #[cfg(debug_assertions)]
    let (agent, source) = if let Some(url) = fixture {
        let peer = crate::local_peer::LocalEndpoint::new_http_bound(url, control.clone())?;
        (
            crate::http_policy::local_peer_agent(&peer, policy)?,
            peer.endpoint_url("/"),
        )
    } else {
        (agent, source.to_owned())
    };
    let response = agent
        .get(source)
        .header("Accept-Encoding", "identity")
        .call()
        // A CDN redirect URL can contain a temporary signature.
        .map_err(|_| AdapterError::Network("model download request failed".into()))?;

    response_reader(response, expected_bytes)
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
