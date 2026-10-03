//! Rust-owned U7 wire, comparison, report, and Python adapter boundary.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    io::{BufRead, BufReader},
    path::PathBuf,
    process::Command,
    time::Duration,
};

use kio_core::cas::hash_bytes;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::runner::{
    BoundedProcessError, BoundedProcessOptions, BoundedStdin, run_bounded_command,
};

pub const ADAPTER_REQUEST_SCHEMA: &str = "kio.u7.reference-embedding-request/v1";
pub const ADAPTER_RESPONSE_SCHEMA: &str = "kio.u7.reference-embedding-response/v1";
pub const REPORT_SCHEMA: &str = "kio.u7.same-space-report/v1";
pub const DEFAULT_THRESHOLD: f64 = 0.999;
pub const DEFAULT_MAX_STDOUT_BYTES: usize = 8 * 1024 * 1024;
pub const DEFAULT_MAX_STDERR_BYTES: usize = 256 * 1024;
pub const MAX_ADAPTER_REQUESTS: usize = 128;
// The CLI accepts at most 64 MiB of raw controls. Base64 expansion plus the
// bounded JSON envelope remains below this fixed aggregate limit.
pub const MAX_ADAPTER_REQUEST_BYTES: usize = 96 * 1024 * 1024;
pub const MAX_ADAPTER_TEXT_BYTES: usize = 1024 * 1024;
pub const MAX_ADAPTER_IMAGE_BASE64_BYTES: usize = 12 * 1024 * 1024;
pub const MAX_ADAPTER_MIME_BYTES: usize = 128;
pub const MAX_ADAPTER_REQUEST_ID_BYTES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Modality {
    Text,
    Image,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestIdentity {
    pub request_id: String,
    /// sha256 of the exact input Rust supplied to both runtimes.
    pub input_digest: String,
    pub modality: Modality,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceRequest {
    pub schema: String,
    #[serde(flatten)]
    pub identity: RequestIdentity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Base64 bytes, never a filesystem pathname.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_base64: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceResponse {
    pub schema: String,
    #[serde(flatten)]
    pub identity: RequestIdentity,
    pub dimensions: usize,
    pub vector: Vec<f64>,
}

#[must_use]
pub fn reference_text_request(
    request_id: impl Into<String>,
    text: impl Into<String>,
) -> ReferenceRequest {
    let text = text.into();
    ReferenceRequest {
        schema: ADAPTER_REQUEST_SCHEMA.into(),
        identity: RequestIdentity {
            request_id: request_id.into(),
            input_digest: hash_bytes(text.as_bytes()),
            modality: Modality::Text,
        },
        text: Some(text),
        image_base64: None,
        mime: None,
    }
}

#[must_use]
pub fn reference_image_request(
    request_id: impl Into<String>,
    mime: impl Into<String>,
    image: &[u8],
) -> ReferenceRequest {
    ReferenceRequest {
        schema: ADAPTER_REQUEST_SCHEMA.into(),
        identity: RequestIdentity {
            request_id: request_id.into(),
            input_digest: hash_bytes(image),
            modality: Modality::Image,
        },
        text: None,
        image_base64: Some(base64(image)),
        mime: Some(mime.into()),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServedRequest {
    pub method: &'static str,
    pub path: &'static str,
    pub body: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    pub count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mean: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum VerdictReason {
    BothAgree,
    ImageDiverged,
    HarnessSuspect,
    ImageNotMeasured,
    HarnessUnusable,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Verdict {
    pub adoptable: bool,
    pub reason: VerdictReason,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub schema: &'static str,
    pub served_model: String,
    pub reference_model: String,
    pub threshold: f64,
    pub native_dimensions: usize,
    pub text_agreement: Summary,
    pub image_agreement: Summary,
    pub verdict: Verdict,
}

#[derive(Debug, Error, PartialEq)]
pub enum U7Error {
    #[error("U7 threshold must be finite and in (0, 1]")]
    Threshold,
    #[error("U7 vectors have different dimensions: {left} and {right}")]
    Dimension { left: usize, right: usize },
    #[error("U7 vector is empty, zero, or contains a non-finite value")]
    InvalidVector,
    #[error("U7 served response is malformed: {0}")]
    ServedResponse(String),
    #[error("U7 adapter response is malformed: {0}")]
    AdapterResponse(String),
    #[error("U7 adapter process failed: {0}")]
    AdapterProcess(String),
    #[error("U7 adapter timed out after {0:?}")]
    AdapterTimeout(Duration),
    #[error("U7 adapter command is invalid: {0}")]
    AdapterCommand(String),
}

/// The exact one-item `messages` form used by Kio's local adapter.
#[must_use]
pub fn served_text_request(model: &str, text: &str) -> ServedRequest {
    served_request(model, json!([{ "type": "text", "text": text }]))
}
/// Image bytes come from the Rust runner; Python receives bytes over JSONL.
#[must_use]
pub fn served_image_request(model: &str, mime: &str, image: &[u8]) -> ServedRequest {
    served_request(
        model,
        json!([{ "type": "image_url", "image_url": { "url": format!("data:{mime};base64,{}", base64(image)) } }]),
    )
}
fn served_request(model: &str, content: Value) -> ServedRequest {
    ServedRequest {
        method: "POST",
        path: "/v1/embeddings",
        body: json!({"model":model,"encoding_format":"float","messages":[{"role":"user","content":content}]}),
    }
}

/// Strictly parse one finite served vector; U7 never chooses one from a batch.
pub fn parse_served_embedding(value: &Value) -> Result<Vec<f64>, U7Error> {
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| U7Error::ServedResponse("missing data array".into()))?;
    let [entry] = data.as_slice() else {
        return Err(U7Error::ServedResponse(format!(
            "expected exactly one data entry, got {}",
            data.len()
        )));
    };
    let vector = entry
        .get("embedding")
        .and_then(Value::as_array)
        .ok_or_else(|| U7Error::ServedResponse("missing embedding array".into()))?
        .iter()
        .map(|n| {
            n.as_f64().filter(|n| n.is_finite()).ok_or_else(|| {
                U7Error::ServedResponse("embedding contains non-finite number".into())
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    validate_vector(&vector)?;
    Ok(vector)
}

pub fn cosine(left: &[f64], right: &[f64]) -> Result<f64, U7Error> {
    validate_vector(left)?;
    validate_vector(right)?;
    if left.len() != right.len() {
        return Err(U7Error::Dimension {
            left: left.len(),
            right: right.len(),
        });
    }
    let numerator = left.iter().zip(right).map(|(a, b)| a * b).sum::<f64>();
    let result = numerator
        / (left.iter().map(|v| v * v).sum::<f64>().sqrt()
            * right.iter().map(|v| v * v).sum::<f64>().sqrt());
    if result.is_finite() {
        Ok(result)
    } else {
        Err(U7Error::InvalidVector)
    }
}
pub fn summarize(scores: &[f64]) -> Result<Summary, U7Error> {
    if scores.iter().any(|v| !v.is_finite()) {
        return Err(U7Error::InvalidVector);
    }
    let Some(min) = scores.iter().copied().reduce(f64::min) else {
        return Ok(Summary {
            count: 0,
            min: None,
            mean: None,
            max: None,
        });
    };
    Ok(Summary {
        count: scores.len(),
        min: Some(min),
        mean: Some(scores.iter().sum::<f64>() / scores.len() as f64),
        max: scores.iter().copied().reduce(f64::max),
    })
}
/// Text control failure takes precedence over every image observation.
pub fn verdict(text: &[f64], image: &[f64], threshold: f64) -> Result<Verdict, U7Error> {
    if !threshold.is_finite() || !(0.0 < threshold && threshold <= 1.0) {
        return Err(U7Error::Threshold);
    }
    if text.iter().chain(image).any(|v| !v.is_finite()) {
        return Err(U7Error::InvalidVector);
    }
    let Some(text_min) = text.iter().copied().reduce(f64::min) else {
        return Ok(Verdict {
            adoptable: false,
            reason: VerdictReason::HarnessUnusable,
            detail: "text control is missing; U7 cannot decide".into(),
        });
    };
    if text_min < threshold {
        return Ok(Verdict {
            adoptable: false,
            reason: VerdictReason::HarnessSuspect,
            detail: format!(
                "text control minimum {text_min:.6} is below {threshold}; do not interpret image scores"
            ),
        });
    }
    let Some(image_min) = image.iter().copied().reduce(f64::min) else {
        return Ok(Verdict {
            adoptable: false,
            reason: VerdictReason::ImageNotMeasured,
            detail: "text agrees but no image was measured".into(),
        });
    };
    if image_min < threshold {
        return Ok(Verdict {
            adoptable: false,
            reason: VerdictReason::ImageDiverged,
            detail: format!(
                "text agrees but image minimum {image_min:.6} is below {threshold}; do not adopt this path"
            ),
        });
    }
    Ok(Verdict {
        adoptable: true,
        reason: VerdictReason::BothAgree,
        detail: format!(
            "text minimum {text_min:.6} and image minimum {image_min:.6} meet {threshold}"
        ),
    })
}
pub fn report(
    served_model: impl Into<String>,
    reference_model: impl Into<String>,
    dimensions: usize,
    text: &[f64],
    image: &[f64],
    threshold: f64,
) -> Result<Report, U7Error> {
    Ok(Report {
        schema: REPORT_SCHEMA,
        served_model: served_model.into(),
        reference_model: reference_model.into(),
        threshold,
        native_dimensions: dimensions,
        text_agreement: summarize(text)?,
        image_agreement: summarize(image)?,
        verdict: verdict(text, image, threshold)?,
    })
}

/// Identity, digest, count, dimension, finiteness, and non-zero norm are all
/// checked by Rust after every JSONL response.
pub fn parse_adapter_response(
    line: &[u8],
    expected: &RequestIdentity,
    dimensions: usize,
) -> Result<Vec<f64>, U7Error> {
    let response: ReferenceResponse =
        serde_json::from_slice(line).map_err(|e| U7Error::AdapterResponse(e.to_string()))?;
    if response.schema != ADAPTER_RESPONSE_SCHEMA {
        return Err(U7Error::AdapterResponse("unsupported schema".into()));
    }
    if &response.identity != expected {
        return Err(U7Error::AdapterResponse(
            "request identity or input digest mismatch".into(),
        ));
    }
    if response.dimensions != dimensions || response.vector.len() != dimensions {
        return Err(U7Error::AdapterResponse("dimension mismatch".into()));
    }
    validate_vector(&response.vector)
        .map_err(|_| U7Error::AdapterResponse("zero or non-finite vector".into()))?;
    Ok(response.vector)
}
fn validate_vector(vector: &[f64]) -> Result<(), U7Error> {
    if vector.is_empty()
        || vector.iter().any(|v| !v.is_finite())
        || vector.iter().map(|v| v * v).sum::<f64>() <= 0.0
    {
        Err(U7Error::InvalidVector)
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct AdapterLimits {
    pub timeout: Duration,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
}
impl Default for AdapterLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
            max_stdout_bytes: DEFAULT_MAX_STDOUT_BYTES,
            max_stderr_bytes: DEFAULT_MAX_STDERR_BYTES,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterCommand {
    /// An absolute, non-symlink, regular executable owned by the caller's
    /// explicit integration boundary.
    pub program: PathBuf,
    pub args: Vec<String>,
    /// The complete child environment. The parent environment is never used.
    pub environment: BTreeMap<OsString, OsString>,
}
/// Execute an explicit adapter program with bounded time and output. `requests`
/// carries expected served dimensions; response line count must match exactly.
pub fn run_adapter(
    command: &AdapterCommand,
    requests: &[(ReferenceRequest, usize)],
    limits: &AdapterLimits,
) -> Result<Vec<Vec<f64>>, U7Error> {
    let input = encode_requests(requests)?;
    validate_adapter_command(command)?;
    spawn_adapter(command, input, requests, limits)
}

/// Validate the complete adapter input before any served-side HTTP request is
/// issued. This prevents an expensive served measurement from succeeding only
/// for the local reference boundary to reject a size or wire-shape mismatch.
pub fn validate_adapter_requests(requests: &[ReferenceRequest]) -> Result<(), U7Error> {
    encode_request_refs(&requests.iter().collect::<Vec<_>>()).map(|_| ())
}

fn encode_requests(requests: &[(ReferenceRequest, usize)]) -> Result<Vec<u8>, U7Error> {
    encode_request_refs(
        &requests
            .iter()
            .map(|(request, _)| request)
            .collect::<Vec<_>>(),
    )
}

fn encode_request_refs(requests: &[&ReferenceRequest]) -> Result<Vec<u8>, U7Error> {
    if requests.len() > MAX_ADAPTER_REQUESTS {
        return Err(U7Error::AdapterResponse(
            "request count exceeds bound".into(),
        ));
    }
    let mut bytes = Vec::new();
    for request in requests {
        validate_reference_request(request)?;
        serde_json::to_writer(&mut bytes, request)
            .map_err(|e| U7Error::AdapterResponse(e.to_string()))?;
        bytes.push(b'\n');
        if bytes.len() > MAX_ADAPTER_REQUEST_BYTES {
            return Err(U7Error::AdapterResponse(
                "aggregate request bytes exceed bound".into(),
            ));
        }
    }
    Ok(bytes)
}

fn spawn_adapter(
    command: &AdapterCommand,
    input: Vec<u8>,
    requests: &[(ReferenceRequest, usize)],
    limits: &AdapterLimits,
) -> Result<Vec<Vec<f64>>, U7Error> {
    let mut process = Command::new(&command.program);
    process
        .args(&command.args)
        .env_clear()
        .envs(&command.environment);
    let output = run_bounded_command(
        &mut process,
        BoundedProcessOptions {
            timeout: limits.timeout,
            max_stdout_bytes: limits.max_stdout_bytes,
            max_stderr_bytes: limits.max_stderr_bytes,
        },
        Some(BoundedStdin::new(input, MAX_ADAPTER_REQUEST_BYTES)),
    )
    .map_err(|error| match error {
        BoundedProcessError::Timeout { .. } => U7Error::AdapterTimeout(limits.timeout),
        other => U7Error::AdapterProcess(other.to_string()),
    })?;
    if !output.status.success() {
        return Err(U7Error::AdapterProcess(output.stderr));
    }
    let lines = BufReader::new(output.stdout.as_bytes())
        .lines()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| U7Error::AdapterProcess(e.to_string()))?;
    if lines.len() != requests.len() {
        return Err(U7Error::AdapterResponse(format!(
            "expected {} response lines, got {}",
            requests.len(),
            lines.len()
        )));
    }
    requests
        .iter()
        .zip(lines)
        .map(|((request, dimensions), line)| {
            parse_adapter_response(line.as_bytes(), &request.identity, *dimensions)
        })
        .collect()
}

fn validate_adapter_command(command: &AdapterCommand) -> Result<(), U7Error> {
    if !command.program.is_absolute() {
        return Err(U7Error::AdapterCommand("program must be absolute".into()));
    }
    let metadata = fs::symlink_metadata(&command.program)
        .map_err(|e| U7Error::AdapterCommand(e.to_string()))?;
    if !metadata.file_type().is_file() {
        return Err(U7Error::AdapterCommand(
            "program must be a regular non-symlink file".into(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(U7Error::AdapterCommand("program is not executable".into()));
        }
    }
    Ok(())
}

fn validate_reference_request(request: &ReferenceRequest) -> Result<(), U7Error> {
    if request.schema != ADAPTER_REQUEST_SCHEMA
        || request.identity.request_id.is_empty()
        || request.identity.request_id.len() > MAX_ADAPTER_REQUEST_ID_BYTES
        || !is_lower_sha256(&request.identity.input_digest)
    {
        return Err(U7Error::AdapterResponse("invalid request identity".into()));
    }
    match request.identity.modality {
        Modality::Text
            if request.text.is_some()
                && request.image_base64.is_none()
                && request.mime.is_none()
                && request
                    .text
                    .as_ref()
                    .is_some_and(|text| text.len() <= MAX_ADAPTER_TEXT_BYTES) =>
        {
            Ok(())
        }
        Modality::Image
            if request.text.is_none()
                && request.image_base64.is_some()
                && request.image_base64.as_ref().is_some_and(|encoded| {
                    encoded.len() <= MAX_ADAPTER_IMAGE_BASE64_BYTES && valid_base64(encoded)
                })
                && request.mime.as_deref().is_some_and(|mime| {
                    !mime.is_empty() && mime.len() <= MAX_ADAPTER_MIME_BYTES
                }) =>
        {
            Ok(())
        }
        _ => Err(U7Error::AdapterResponse(
            "request modality shape mismatch".into(),
        )),
    }
}

fn is_lower_sha256(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value.as_bytes()[7..].iter().all(u8::is_ascii_hexdigit)
        && !value.as_bytes()[7..].iter().any(u8::is_ascii_uppercase)
}

fn valid_base64(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return false;
    }
    let padding = bytes.iter().rev().take_while(|byte| **byte == b'=').count();
    padding <= 2
        && bytes[..bytes.len() - padding]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'+' || *byte == b'/')
        && bytes[bytes.len() - padding..]
            .iter()
            .all(|byte| *byte == b'=')
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        out.push(char::from(T[((n >> 18) & 63) as usize]));
        out.push(char::from(T[((n >> 12) & 63) as usize]));
        out.push(if c.len() > 1 {
            char::from(T[((n >> 6) & 63) as usize])
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            char::from(T[(n & 63) as usize])
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::PathBuf};

    #[cfg(unix)]
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        sync::{Mutex, OnceLock},
    };

    use super::*;

    #[cfg(unix)]
    fn adapter_test_shell(body: &str) -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let shell = directory.path().join("shell");
        fs::write(&shell, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
        let metadata = fs::symlink_metadata(&shell).unwrap();
        assert!(metadata.file_type().is_file());
        assert!(!metadata.file_type().is_symlink());
        (directory, shell)
    }

    #[test]
    fn both_agree() {
        assert_eq!(
            verdict(&[0.9999], &[0.9999], DEFAULT_THRESHOLD)
                .unwrap()
                .reason,
            VerdictReason::BothAgree
        )
    }
    #[test]
    fn image_diverged() {
        assert_eq!(
            verdict(&[0.9999], &[0.87], DEFAULT_THRESHOLD)
                .unwrap()
                .reason,
            VerdictReason::ImageDiverged
        )
    }
    #[test]
    fn harness_suspect() {
        assert_eq!(
            verdict(&[0.6], &[0.1], DEFAULT_THRESHOLD).unwrap().reason,
            VerdictReason::HarnessSuspect
        )
    }
    #[test]
    fn image_not_measured() {
        assert_eq!(
            verdict(&[1.0], &[], DEFAULT_THRESHOLD).unwrap().reason,
            VerdictReason::ImageNotMeasured
        )
    }
    #[test]
    fn missing_control() {
        assert_eq!(
            verdict(&[], &[1.0], DEFAULT_THRESHOLD).unwrap().reason,
            VerdictReason::HarnessUnusable
        )
    }
    #[test]
    fn minimum_priority() {
        assert_eq!(
            verdict(&[1.0; 4], &[0.98], 0.99).unwrap().reason,
            VerdictReason::ImageDiverged
        )
    }
    #[test]
    fn zero_and_nonfinite() {
        assert_eq!(cosine(&[0.0], &[1.0]), Err(U7Error::InvalidVector));
        assert_eq!(cosine(&[f64::NAN], &[1.0]), Err(U7Error::InvalidVector))
    }
    #[test]
    fn adapter_rejects_zero_and_nonfinite() {
        let identity = RequestIdentity {
            request_id: "r".into(),
            input_digest: "sha256:x".into(),
            modality: Modality::Text,
        };
        for vector in [vec![0.0], vec![f64::INFINITY]] {
            let line = serde_json::to_vec(&ReferenceResponse {
                schema: ADAPTER_RESPONSE_SCHEMA.into(),
                identity: identity.clone(),
                dimensions: 1,
                vector,
            })
            .unwrap();
            assert!(matches!(
                parse_adapter_response(&line, &identity, 1),
                Err(U7Error::AdapterResponse(_))
            ));
        }
    }
    #[test]
    fn text_wire_shape() {
        assert_eq!(
            served_text_request("m", "x").body,
            json!({"model":"m","encoding_format":"float","messages":[{"role":"user","content":[{"type":"text","text":"x"}]}]})
        )
    }
    #[test]
    fn image_wire_shape() {
        assert_eq!(
            served_image_request("m", "image/png", &[1, 2, 3]).body["messages"][0]["content"][0]["image_url"]
                ["url"],
            "data:image/png;base64,AQID"
        )
    }
    #[test]
    fn malformed_and_oversize_requests_fail_before_spawn() {
        let invalid_command = AdapterCommand {
            program: PathBuf::from("not-absolute"),
            args: vec![],
            environment: BTreeMap::new(),
        };
        let mut malformed = reference_text_request("r", "x");
        malformed.identity.input_digest = "sha256:ABC".into();
        assert!(matches!(
            run_adapter(
                &invalid_command,
                &[(malformed, 1)],
                &AdapterLimits::default()
            ),
            Err(U7Error::AdapterResponse(_))
        ));
        let oversized = reference_text_request("r", "x".repeat(MAX_ADAPTER_TEXT_BYTES + 1));
        assert!(matches!(
            run_adapter(
                &invalid_command,
                &[(oversized, 1)],
                &AdapterLimits::default()
            ),
            Err(U7Error::AdapterResponse(_))
        ));
        let requests = vec![(reference_text_request("r", "x"), 1); MAX_ADAPTER_REQUESTS + 1];
        assert!(matches!(
            run_adapter(&invalid_command, &requests, &AdapterLimits::default()),
            Err(U7Error::AdapterResponse(_))
        ));
    }

    #[test]
    fn cli_sized_image_fits_the_reference_adapter_envelope() {
        let request = reference_image_request("image-0", "image/png", &vec![0_u8; 8 * 1024 * 1024]);
        validate_adapter_requests(&[request]).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn adapter_child_does_not_inherit_parent_credentials_or_kio_test_environment() {
        static ENVIRONMENT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _guard = ENVIRONMENT_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        let old_aws = std::env::var_os("AWS_SECRET_ACCESS_KEY");
        let old_kio_test = std::env::var_os("KIO_TEST_SENTINEL");
        // SAFETY: the lock serializes this test's temporary process-environment
        // mutation, and it is removed before the test returns.
        unsafe {
            std::env::set_var("AWS_SECRET_ACCESS_KEY", "parent-secret");
            std::env::set_var("KIO_TEST_SENTINEL", "parent-test-only");
        }
        let request = reference_text_request("r", "x");
        let response = serde_json::to_string(&ReferenceResponse {
            schema: ADAPTER_RESPONSE_SCHEMA.into(),
            identity: request.identity.clone(),
            dimensions: 1,
            vector: vec![1.0],
        })
        .unwrap();
        let script = format!(
            "read _; if test -z \"$AWS_SECRET_ACCESS_KEY\" && test -z \"$KIO_TEST_SENTINEL\" && test \"$U7_ALLOWED\" = yes; then printf '%s\\n' '{response}'; else exit 9; fi"
        );
        let mut environment = BTreeMap::new();
        environment.insert("U7_ALLOWED".into(), "yes".into());
        let (_shell_directory, shell) = adapter_test_shell(&script);
        let command = AdapterCommand {
            program: shell,
            args: vec![],
            environment,
        };
        let result = run_adapter(&command, &[(request, 1)], &AdapterLimits::default());
        // SAFETY: paired with the test-local set_var above while holding the lock.
        unsafe {
            if let Some(value) = old_aws {
                std::env::set_var("AWS_SECRET_ACCESS_KEY", value);
            } else {
                std::env::remove_var("AWS_SECRET_ACCESS_KEY");
            }
            if let Some(value) = old_kio_test {
                std::env::set_var("KIO_TEST_SENTINEL", value);
            } else {
                std::env::remove_var("KIO_TEST_SENTINEL");
            }
        }
        assert_eq!(result.unwrap(), vec![vec![1.0]]);
    }
    #[cfg(unix)]
    #[test]
    fn stalled_stdin_writer_obeys_lifecycle_timeout() {
        let (_shell_directory, shell) = adapter_test_shell("exec /bin/sleep 5");
        let command = AdapterCommand {
            program: shell,
            args: vec![],
            environment: BTreeMap::new(),
        };
        let limits = AdapterLimits {
            timeout: Duration::from_millis(40),
            ..AdapterLimits::default()
        };
        let request = reference_text_request("r", "x".repeat(MAX_ADAPTER_TEXT_BYTES));
        assert!(matches!(
            run_adapter(&command, &[(request, 1)], &limits),
            Err(U7Error::AdapterTimeout(_))
        ));
    }
}
