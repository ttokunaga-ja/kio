//! Typed, bounded OCR evaluation primitives.
//!
//! Python is deliberately not an evaluation authority. Rust owns the direct
//! provider request, response normalization, metrics, thresholds, verdicts,
//! and report data. Python remains only the narrow fixture renderer.

use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use serde::{
    Deserialize, Serialize,
    de::{DeserializeSeed, MapAccess, SeqAccess, Visitor},
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

use crate::runner::{
    BoundedProcessError, BoundedProcessOptions, BoundedStdin, run_bounded_command,
};

pub const GROUND_TRUTH_SCHEMA: &str = "kio.ocr.ground-truth/v1";
pub const RESPONSE_SCHEMA: &str = "kio.ocr.response/v2";
pub const MISTRAL_OCR_MODEL: &str = "mistral-ocr-4-1";
pub const MISTRAL_OCR_ENDPOINT: &str = "https://api.mistral.ai/v1/ocr";
pub const TABLE_RECALL_THRESHOLD: f64 = 0.95;
pub const JAPANESE_CER_THRESHOLD: f64 = 0.02;
pub const MAX_JSON_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_PAGES: usize = 10_000;
pub const MAX_MARKDOWN_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_DOCUMENT_BYTES: u64 = 256 * 1024 * 1024;
/// Provider documents travel in the bounded direct-HTTP JSON request, not by a
/// pathname that a credentialed child could reopen after validation.
pub const MAX_PROVIDER_DOCUMENT_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_PROVIDER_REQUEST_BYTES: usize = 24 * 1024 * 1024;
pub const MAX_PROVIDER_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_IMAGES_PER_PAGE: usize = 10_000;
pub const MAX_IMAGES_TOTAL: usize = 100_000;
pub const MAX_RENDER_INPUT_IMAGES: usize = 10_000;
pub const MAX_RENDER_IMAGE_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_RENDER_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_RENDER_IMAGE_PIXELS: u64 = 32 * 1024 * 1024;
pub const MAX_RENDER_TOTAL_PIXELS: u64 = 96 * 1024 * 1024;
pub const MAX_RENDER_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum OcrEvalError {
    #[error("OCR JSON exceeds the {limit}-byte bound")]
    InputTooLarge { limit: usize },
    #[error("invalid OCR JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid OCR data: {0}")]
    Invalid(&'static str),
    #[error("OCR adapter path must be absolute: {0}")]
    AdapterPath(PathBuf),
    #[error("OCR document binding is invalid: {0}")]
    DocumentBinding(&'static str),
    #[error("OCR report path already exists: {0}")]
    ReportAlreadyExists(PathBuf),
    #[error("OCR adapter request exceeds the {limit}-byte bound")]
    RequestTooLarge { limit: usize },
    #[error("OCR adapter timed out after {0:?}")]
    Timeout(Duration),
    #[error("OCR adapter {stream} exceeded the {limit}-byte bound")]
    OutputTooLarge { stream: &'static str, limit: usize },
    #[error("OCR adapter failed with status {status}: {stderr}")]
    AdapterFailed { status: String, stderr: String },
    #[error("Mistral OCR returned HTTP status {0}")]
    ProviderStatus(u16),
    #[error("OCR adapter I/O: {0}")]
    Io(#[from] io::Error),
    #[error("OCR artifact publication: {0}")]
    Artifact(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroundTruth {
    pub schema: String,
    pub table: TableTruth,
    pub japanese: JapaneseTruth,
    pub images: ImageTruth,
    pub formula: FormulaTruth,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableTruth {
    pub page_index: u32,
    pub expected_cell_texts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JapaneseTruth {
    pub page_index: u32,
    pub full_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageTruth {
    pub page_index: u32,
    pub expected_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormulaTruth {
    pub page_index: u32,
    pub expected_tokens: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OcrResponse {
    pub schema: String,
    pub request_id: String,
    pub document_sha256: String,
    pub model: String,
    pub pages: Vec<OcrPage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OcrPage {
    pub index: u32,
    #[serde(default)]
    pub markdown: String,
    /// Only the bounded provider image count is retained; payloads never cross
    /// the evaluation boundary.
    pub image_count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EvaluationReport {
    pub schema: &'static str,
    pub verdict: Verdict,
    pub criteria: Criteria,
    pub table: TableMetric,
    pub japanese: JapaneseMetric,
    pub images: ImageMetric,
    pub formula: FormulaMetric,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Passed,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Criteria {
    pub table_recall_at_least: f64,
    pub japanese_cer_at_most: f64,
    pub image_count_exact: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TableMetric {
    pub matched_cells: usize,
    pub total_cells: usize,
    pub recall: f64,
    pub passed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JapaneseMetric {
    pub expected_chars: usize,
    pub observed_chars: usize,
    pub edit_distance: usize,
    pub cer: f64,
    pub passed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImageMetric {
    pub expected_count: u32,
    pub observed_count: usize,
    pub passed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FormulaMetric {
    pub expected_tokens: Vec<String>,
    pub matched_tokens: Vec<String>,
    pub classification: FormulaClassification,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FormulaClassification {
    Textized,
    ImageFallback,
    Missing,
}

pub fn parse_ground_truth(bytes: &[u8]) -> Result<GroundTruth, OcrEvalError> {
    if bytes.len() > MAX_JSON_BYTES {
        return Err(OcrEvalError::InputTooLarge {
            limit: MAX_JSON_BYTES,
        });
    }
    let truth: GroundTruth = serde_json::from_slice(bytes)?;
    if truth.schema != GROUND_TRUTH_SCHEMA {
        return Err(OcrEvalError::Invalid("unsupported ground-truth schema"));
    }
    if truth.table.expected_cell_texts.len() > 100_000
        || truth.formula.expected_tokens.len() > 100_000
    {
        return Err(OcrEvalError::Invalid(
            "ground truth collection exceeds bound",
        ));
    }
    Ok(truth)
}

pub fn parse_response(bytes: &[u8]) -> Result<OcrResponse, OcrEvalError> {
    if bytes.len() > MAX_JSON_BYTES {
        return Err(OcrEvalError::InputTooLarge {
            limit: MAX_JSON_BYTES,
        });
    }
    reject_duplicate_json_keys(bytes)?;
    let response: OcrResponse = serde_json::from_slice(bytes)?;
    if response.schema != RESPONSE_SCHEMA {
        return Err(OcrEvalError::Invalid("unsupported OCR response schema"));
    }
    if response.pages.len() > MAX_PAGES
        || response.request_id.is_empty()
        || response.request_id.len() > 256
        || response.request_id.chars().any(char::is_control)
        || !valid_sha256(&response.document_sha256)
        || response.model != MISTRAL_OCR_MODEL
        || response.pages.iter().any(|page| {
            page.markdown.len() > MAX_MARKDOWN_BYTES || page.image_count > MAX_IMAGES_PER_PAGE
        })
    {
        return Err(OcrEvalError::Invalid(
            "OCR response exceeds page or markdown bound",
        ));
    }
    let image_total = response
        .pages
        .iter()
        .try_fold(0_usize, |total, page| total.checked_add(page.image_count))
        .ok_or(OcrEvalError::Invalid("OCR response image count overflow"))?;
    let mut seen = std::collections::BTreeSet::new();
    if response.pages.iter().any(|page| !seen.insert(page.index))
        || response
            .pages
            .iter()
            .enumerate()
            .any(|(index, page)| page.index != index as u32)
        || image_total > MAX_IMAGES_TOTAL
    {
        return Err(OcrEvalError::Invalid(
            "OCR response has invalid page indexes or image count",
        ));
    }
    Ok(response)
}

/// `serde_json` intentionally accepts duplicate object members. Provider
/// payloads are an external authority boundary, so reject them recursively
/// before deserializing the typed response.
fn reject_duplicate_json_keys(bytes: &[u8]) -> Result<(), OcrEvalError> {
    struct Scan;
    impl<'de> Visitor<'de> for Scan {
        type Value = ();

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("any JSON value without duplicate object keys")
        }

        fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
            Ok(())
        }
        fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
            Ok(())
        }
        fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
            Ok(())
        }
        fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
            Ok(())
        }
        fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
            Ok(())
        }
        fn visit_string<E>(self, _: String) -> Result<Self::Value, E> {
            Ok(())
        }
        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(())
        }
        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(())
        }
        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            while seq.next_element_seed(Scan)?.is_some() {}
            Ok(())
        }
        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut keys = std::collections::BTreeSet::new();
            while let Some(key) = map.next_key::<String>()? {
                if !keys.insert(key) {
                    return Err(serde::de::Error::custom("duplicate JSON object key"));
                }
                map.next_value_seed(Scan)?;
            }
            Ok(())
        }
    }
    impl<'de> DeserializeSeed<'de> for Scan {
        type Value = ();
        fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_any(self)
        }
    }

    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    Scan.deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(())
}

pub fn evaluate(
    truth: &GroundTruth,
    response: &OcrResponse,
) -> Result<EvaluationReport, OcrEvalError> {
    let table_page = page(response, truth.table.page_index)?;
    let table_haystack = normalize_cell(&table_page.markdown);
    let matched_cells: Vec<_> = truth
        .table
        .expected_cell_texts
        .iter()
        .filter(|cell| table_haystack.contains(&normalize_cell(cell)))
        .cloned()
        .collect();
    let table_recall = ratio(matched_cells.len(), truth.table.expected_cell_texts.len());
    let table = TableMetric {
        matched_cells: matched_cells.len(),
        total_cells: truth.table.expected_cell_texts.len(),
        recall: table_recall,
        passed: table_recall >= TABLE_RECALL_THRESHOLD,
    };

    let japanese_page = page(response, truth.japanese.page_index)?;
    let expected = normalize_japanese(&truth.japanese.full_text);
    let observed = normalize_japanese(&japanese_page.markdown);
    let distance = best_window_distance(&expected, &observed);
    let cer = ratio(distance, expected.chars().count());
    let japanese = JapaneseMetric {
        expected_chars: expected.chars().count(),
        observed_chars: observed.chars().count(),
        edit_distance: distance,
        cer,
        passed: cer <= JAPANESE_CER_THRESHOLD,
    };

    let image_page = page(response, truth.images.page_index)?;
    let images = ImageMetric {
        expected_count: truth.images.expected_count,
        observed_count: image_page.image_count,
        passed: image_page.image_count == truth.images.expected_count as usize,
    };

    let formula_page = page(response, truth.formula.page_index)?;
    let normalized = normalize_formula(&formula_page.markdown);
    let matched_tokens: Vec<_> = truth
        .formula
        .expected_tokens
        .iter()
        .filter(|token| normalized.contains(&normalize_formula(token)))
        .cloned()
        .collect();
    let classification = if matched_tokens.len() == truth.formula.expected_tokens.len() {
        FormulaClassification::Textized
    } else if formula_page.image_count != 0 || formula_page.markdown.contains("![") {
        FormulaClassification::ImageFallback
    } else {
        FormulaClassification::Missing
    };
    let formula = FormulaMetric {
        expected_tokens: truth.formula.expected_tokens.clone(),
        matched_tokens,
        classification,
    };
    let verdict = if table.passed && japanese.passed && images.passed {
        Verdict::Passed
    } else {
        Verdict::Rejected
    };
    Ok(EvaluationReport {
        schema: "kio.ocr.evaluation-report/v1",
        verdict,
        criteria: Criteria {
            table_recall_at_least: TABLE_RECALL_THRESHOLD,
            japanese_cer_at_most: JAPANESE_CER_THRESHOLD,
            image_count_exact: true,
        },
        table,
        japanese,
        images,
        formula,
    })
}

fn page(response: &OcrResponse, index: u32) -> Result<&OcrPage, OcrEvalError> {
    response
        .pages
        .iter()
        .find(|page| page.index == index)
        .ok_or(OcrEvalError::Invalid(
            "ground truth refers to absent OCR page",
        ))
}
fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        1.0
    } else {
        numerator as f64 / denominator as f64
    }
}
fn normalize_cell(value: &str) -> String {
    value
        .nfc()
        .flat_map(char::to_lowercase)
        .filter(|ch| !ch.is_whitespace())
        .collect()
}
fn normalize_japanese(value: &str) -> String {
    value.nfc().filter(|ch| !ch.is_whitespace()).collect()
}
fn normalize_formula(value: &str) -> String {
    value.nfc().filter(|ch| !ch.is_whitespace()).collect()
}

fn best_window_distance(expected: &str, observed: &str) -> usize {
    let expected: Vec<_> = expected.chars().collect();
    let observed: Vec<_> = observed.chars().collect();
    if expected.is_empty() {
        return 0;
    }
    if observed.len() <= expected.len() {
        return levenshtein(&expected, &observed);
    }
    observed
        .windows(expected.len())
        .map(|window| levenshtein(&expected, window))
        .min()
        .unwrap_or(expected.len())
}
fn levenshtein(left: &[char], right: &[char]) -> usize {
    let mut row: Vec<usize> = (0..=right.len()).collect();
    for (i, lhs) in left.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, rhs) in right.iter().enumerate() {
            let old = row[j + 1];
            row[j + 1] = (row[j + 1] + 1)
                .min(row[j] + 1)
                .min(diagonal + usize::from(lhs != rhs));
            diagonal = old;
        }
    }
    row[right.len()]
}

#[derive(Serialize)]
struct MistralRequest<'a> {
    model: &'a str,
    document: MistralDocument<'a>,
    include_image_base64: bool,
    include_blocks: bool,
}
#[derive(Serialize)]
struct MistralDocument<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    document_url: &'a str,
}

#[derive(Debug, Clone, Copy)]
struct ProviderLimits {
    connect_timeout: Duration,
    global_timeout: Duration,
    max_response_bytes: usize,
}

impl Default for ProviderLimits {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            global_timeout: Duration::from_secs(120),
            max_response_bytes: MAX_PROVIDER_RESPONSE_BYTES,
        }
    }
}

fn provider_agent(limits: ProviderLimits) -> ureq::Agent {
    ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .proxy(None)
        .timeout_connect(Some(limits.connect_timeout))
        .timeout_global(Some(limits.global_timeout))
        .build()
        .into()
}

/// Direct, one-shot Mistral OCR call. The endpoint is fixed for production;
/// `endpoint` is an intentionally private test seam for loopback mocks only.
pub fn request_mistral_ocr(
    request_id: &str,
    model: &str,
    document: &Path,
    include_image_base64: bool,
    api_key: &str,
) -> Result<OcrResponse, OcrEvalError> {
    request_mistral_ocr_at(
        MISTRAL_OCR_ENDPOINT,
        request_id,
        model,
        document,
        include_image_base64,
        api_key,
    )
}

fn request_mistral_ocr_at(
    endpoint: &str,
    request_id: &str,
    model: &str,
    document: &Path,
    include_image_base64: bool,
    api_key: &str,
) -> Result<OcrResponse, OcrEvalError> {
    request_mistral_ocr_at_with_limits(
        endpoint,
        request_id,
        model,
        document,
        include_image_base64,
        api_key,
        ProviderLimits::default(),
    )
}

#[allow(clippy::too_many_arguments)]
fn request_mistral_ocr_at_with_limits(
    endpoint: &str,
    request_id: &str,
    model: &str,
    document: &Path,
    include_image_base64: bool,
    api_key: &str,
    limits: ProviderLimits,
) -> Result<OcrResponse, OcrEvalError> {
    if model != MISTRAL_OCR_MODEL
        || request_id.is_empty()
        || request_id.len() > 256
        || request_id.chars().any(char::is_control)
    {
        return Err(OcrEvalError::Invalid(
            "unsupported provider model or request id",
        ));
    }
    if api_key.is_empty() || api_key.len() > 4096 || api_key.chars().any(char::is_control) {
        return Err(OcrEvalError::Invalid("invalid Mistral API credential"));
    }
    let artifact = bind_strict_input(document, MAX_PROVIDER_DOCUMENT_BYTES)?;
    let bytes = artifact.bytes();
    let sha256 = hex_sha256(bytes);
    let document_url = format!("data:application/pdf;base64,{}", base64_encode(bytes));
    let request = MistralRequest {
        model,
        document: MistralDocument {
            kind: "document_url",
            document_url: &document_url,
        },
        include_image_base64,
        include_blocks: false,
    };
    let body = serde_json::to_vec(&request)?;
    if body.len() > MAX_PROVIDER_REQUEST_BYTES {
        return Err(OcrEvalError::RequestTooLarge {
            limit: MAX_PROVIDER_REQUEST_BYTES,
        });
    }
    let agent = provider_agent(limits);
    let mut response = agent
        .post(endpoint)
        .header("Authorization", &format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .header("Accept-Encoding", "identity")
        .send(&body)
        .map_err(|error| match error {
            ureq::Error::Timeout(_) => OcrEvalError::Timeout(limits.global_timeout),
            ureq::Error::Io(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                OcrEvalError::Timeout(limits.global_timeout)
            }
            _ => OcrEvalError::Invalid("Mistral OCR network request failed"),
        })?;
    if !response.status().is_success() {
        return Err(OcrEvalError::ProviderStatus(response.status().as_u16()));
    }
    let content_types: Vec<_> = response.headers().get_all("Content-Type").iter().collect();
    if content_types.len() != 1 {
        return Err(OcrEvalError::Invalid(
            "Mistral OCR response must have one content type",
        ));
    }
    let content_type = content_types[0].to_str().ok().ok_or(OcrEvalError::Invalid(
        "Mistral OCR response has invalid content type",
    ))?;
    if !content_type
        .split(';')
        .next()
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"))
    {
        return Err(OcrEvalError::Invalid("Mistral OCR response is not JSON"));
    }
    let encodings: Vec<_> = response
        .headers()
        .get_all("Content-Encoding")
        .iter()
        .collect();
    if encodings.len() > 1
        || encodings.first().is_some_and(|value| {
            value
                .to_str()
                .ok()
                .is_none_or(|encoding| !encoding.eq_ignore_ascii_case("identity"))
        })
    {
        return Err(OcrEvalError::Invalid(
            "Mistral OCR response uses unsupported content encoding",
        ));
    }
    let lengths: Vec<_> = response
        .headers()
        .get_all("Content-Length")
        .iter()
        .collect();
    if lengths.len() > 1 {
        return Err(OcrEvalError::Invalid(
            "Mistral OCR response has conflicting content length",
        ));
    }
    let declared = match lengths.first() {
        None => None,
        Some(value) => Some(
            value
                .to_str()
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .ok_or(OcrEvalError::Invalid(
                    "Mistral OCR response has invalid content length",
                ))?,
        ),
    };
    if declared.is_some_and(|n| n > limits.max_response_bytes) {
        return Err(OcrEvalError::InputTooLarge {
            limit: limits.max_response_bytes,
        });
    }
    let mut received = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(limits.max_response_bytes.saturating_add(1) as u64)
        .read_to_end(&mut received)
        .map_err(|error| {
            if matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) {
                OcrEvalError::Timeout(limits.global_timeout)
            } else {
                OcrEvalError::Invalid("Mistral OCR response read failed")
            }
        })?;
    if received.len() > limits.max_response_bytes {
        return Err(OcrEvalError::InputTooLarge {
            limit: limits.max_response_bytes,
        });
    }
    if declared.is_some_and(|n| n != received.len()) {
        return Err(OcrEvalError::Invalid(
            "Mistral OCR response content length mismatch",
        ));
    }
    artifact
        .recheck()
        .map_err(|_| OcrEvalError::DocumentBinding("document changed during OCR request"))?;
    normalize_mistral_response(&received, request_id, &sha256, model)
}

fn normalize_mistral_response(
    bytes: &[u8],
    request_id: &str,
    document_sha256: &str,
    model: &str,
) -> Result<OcrResponse, OcrEvalError> {
    #[derive(Deserialize)]
    struct RawPage {
        index: u32,
        markdown: String,
        images: Vec<serde_json::Value>,
    }
    #[derive(Deserialize)]
    struct Raw {
        model: String,
        pages: Vec<RawPage>,
    }
    reject_duplicate_json_keys(bytes)?;
    let raw: Raw = serde_json::from_slice(bytes)?;
    if raw.model != model {
        return Err(OcrEvalError::Invalid("Mistral OCR response model mismatch"));
    }
    if raw.pages.len() > MAX_PAGES
        || raw
            .pages
            .windows(2)
            .any(|pages| pages[0].index >= pages[1].index)
    {
        return Err(OcrEvalError::Invalid(
            "Mistral OCR page indexes must be unique and increasing",
        ));
    }
    let response = OcrResponse {
        schema: RESPONSE_SCHEMA.into(),
        request_id: request_id.into(),
        document_sha256: document_sha256.into(),
        model: model.into(),
        pages: raw
            .pages
            .into_iter()
            .enumerate()
            .map(|(index, page)| OcrPage {
                // Provider examples and retained real responses disagree on
                // whether the first source-page index is 0 or 1. Preserve the
                // provider's ordering invariant, then own one canonical
                // zero-based ordinal in the normalized Kio artifact.
                index: index as u32,
                markdown: page.markdown,
                image_count: page.images.len(),
            })
            .collect(),
    };
    parse_response(&serde_json::to_vec(&response)?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderRequest {
    pub schema: String,
    pub request_id: String,
    pub output_pdf: String,
    pub input_images: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderResponse {
    pub schema: String,
    pub request_id: String,
    pub output_pdf: String,
    pub output_bytes: u64,
    pub output_sha256: String,
    pub page_count: u32,
}

pub const RENDER_REQUEST_SCHEMA: &str = "kio.ocr.fixture-render.request/v1";
pub const RENDER_RESPONSE_SCHEMA: &str = "kio.ocr.fixture-render.response/v1";

/// Explicit interpreter plus explicit adapter script.  The child has no
/// inherited environment; callers must opt in to every variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RendererCommand {
    pub python: PathBuf,
    pub adapter: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentBinding {
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
}

/// Bind an explicitly named PDF through the descriptor-retained artifact
/// boundary. The direct Rust provider receives only the resulting byte snapshot.
pub fn bind_document(path: &Path) -> Result<DocumentBinding, OcrEvalError> {
    let artifact = bind_strict_input(path, MAX_DOCUMENT_BYTES)?;
    let bytes = artifact.bytes();
    artifact
        .recheck()
        .map_err(|_| OcrEvalError::DocumentBinding("document changed while being bound"))?;
    Ok(DocumentBinding {
        path: path.to_owned(),
        bytes: bytes.len() as u64,
        sha256: hex_sha256(bytes),
    })
}

/// Create a canonical evaluation report once. Existing artifact archives are
/// never overwritten or treated as current evidence.
pub fn write_report_create_only(
    path: &Path,
    report: &EvaluationReport,
) -> Result<(), OcrEvalError> {
    if !path.is_absolute() {
        return Err(OcrEvalError::AdapterPath(path.to_owned()));
    }
    let bytes =
        serde_jcs::to_vec(report).map_err(|error| OcrEvalError::Artifact(error.to_string()))?;
    let mut canonical = bytes;
    canonical.push(b'\n');
    match crate::strict_artifact::publish_create_only(path, &canonical, MAX_JSON_BYTES) {
        Ok(_) => Ok(()),
        Err(crate::strict_artifact::StrictArtifactError::AlreadyExists(_)) => {
            Err(OcrEvalError::ReportAlreadyExists(path.to_owned()))
        }
        Err(error) => Err(OcrEvalError::Artifact(error.to_string())),
    }
}

/// Publish the normalized closed OCR response once.  Provider payloads are
/// intentionally never archived as authority; only this Rust-owned schema is.
pub fn write_response_create_only(path: &Path, response: &OcrResponse) -> Result<(), OcrEvalError> {
    if !path.is_absolute() {
        return Err(OcrEvalError::AdapterPath(path.to_owned()));
    }
    let parsed = parse_response(&serde_json::to_vec(response)?)?;
    let mut canonical =
        serde_jcs::to_vec(&parsed).map_err(|error| OcrEvalError::Artifact(error.to_string()))?;
    canonical.push(b'\n');
    match crate::strict_artifact::publish_create_only(path, &canonical, MAX_JSON_BYTES) {
        Ok(_) => Ok(()),
        Err(crate::strict_artifact::StrictArtifactError::AlreadyExists(_)) => {
            Err(OcrEvalError::ReportAlreadyExists(path.to_owned()))
        }
        Err(error) => Err(OcrEvalError::Artifact(error.to_string())),
    }
}

#[derive(Debug, Clone)]
pub struct RendererLimits {
    pub timeout: Duration,
    pub max_stdin_bytes: usize,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
}
impl Default for RendererLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(60),
            max_stdin_bytes: MAX_PROVIDER_REQUEST_BYTES,
            max_stdout_bytes: MAX_JSON_BYTES,
            max_stderr_bytes: 64 * 1024,
        }
    }
}

/// Run the narrow renderer with explicit absolute inputs and a create-only
/// output. The response is verified against the inode-independent bytes Rust
/// observes after the child exits.
pub fn invoke_renderer(
    command: &RendererCommand,
    request: &RenderRequest,
    limits: &RendererLimits,
) -> Result<RenderResponse, OcrEvalError> {
    validate_render_request(request)?;
    let output_path = PathBuf::from(&request.output_pdf);
    let mut aggregate_bytes = 0_u64;
    let mut aggregate_pixels = 0_u64;
    let mut input_bindings = Vec::with_capacity(request.input_images.len());
    for value in &request.input_images {
        let binding = bind_strict_input(Path::new(value), MAX_RENDER_IMAGE_BYTES)?;
        let bytes = binding.bytes();
        aggregate_bytes = aggregate_bytes
            .checked_add(bytes.len() as u64)
            .ok_or(OcrEvalError::Invalid("renderer input byte count overflow"))?;
        if aggregate_bytes > MAX_RENDER_TOTAL_BYTES {
            return Err(OcrEvalError::DocumentBinding(
                "renderer inputs exceed aggregate byte bound",
            ));
        }
        let pixels = png_pixels(bytes)?;
        if pixels > MAX_RENDER_IMAGE_PIXELS {
            return Err(OcrEvalError::DocumentBinding(
                "renderer input exceeds per-image pixel bound",
            ));
        }
        aggregate_pixels = aggregate_pixels
            .checked_add(pixels)
            .ok_or(OcrEvalError::Invalid("renderer input pixel count overflow"))?;
        if aggregate_pixels > MAX_RENDER_TOTAL_PIXELS {
            return Err(OcrEvalError::DocumentBinding(
                "renderer inputs exceed aggregate pixel bound",
            ));
        }
        input_bindings.push(binding);
    }
    // The Python-native renderer receives only private immutable snapshots.
    // It never reopens the caller-controlled input names after Rust validates
    // them, closing the stat/open and mid-render replacement seams.
    let snapshots = tempfile::tempdir()?;
    let mut adapter_request = request.clone();
    adapter_request.input_images.clear();
    for (index, binding) in input_bindings.iter().enumerate() {
        let snapshot = snapshots.path().join(format!("input-{index:05}.png"));
        fs::write(&snapshot, binding.bytes())?;
        adapter_request
            .input_images
            .push(snapshot.to_string_lossy().into_owned());
    }
    let output = invoke_renderer_command(command, &adapter_request, limits)?;
    for binding in &input_bindings {
        binding.recheck().map_err(|_| {
            OcrEvalError::DocumentBinding("renderer input changed during adapter execution")
        })?;
    }
    let response = parse_render_line(&output.0, request)?;
    let final_binding = bind_strict_input(&output_path, MAX_RENDER_OUTPUT_BYTES)?;
    let output_bytes = final_binding.bytes();
    let output_sha256 = hex_sha256(output_bytes);
    if output_bytes.is_empty()
        || output_bytes.len() as u64 != response.output_bytes
        || output_sha256 != response.output_sha256
    {
        return Err(OcrEvalError::DocumentBinding(
            "renderer output identity mismatch",
        ));
    }
    if pdf_page_count(output_bytes)? != response.page_count {
        return Err(OcrEvalError::DocumentBinding(
            "renderer output bytes or page count changed during verification",
        ));
    }
    final_binding.recheck().map_err(|_| {
        OcrEvalError::DocumentBinding("renderer output changed during verification")
    })?;
    Ok(response)
}

/// PNG headers are inexpensive to inspect before spawning Pillow. Other image
/// formats remain bounded by raw bytes and are checked by Pillow in the
/// adapter; an unknown signature fails closed rather than selecting a decoder.
fn png_pixels(bytes: &[u8]) -> Result<u64, OcrEvalError> {
    let header = bytes.get(..24).ok_or(OcrEvalError::Invalid(
        "renderer input lacks a PNG IHDR header",
    ))?;
    if &header[..8] != b"\x89PNG\r\n\x1a\n" || &header[12..16] != b"IHDR" {
        return Err(OcrEvalError::Invalid("renderer only accepts PNG inputs"));
    }
    let width = u32::from_be_bytes(header[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(header[20..24].try_into().unwrap());
    if width == 0 || height == 0 {
        return Err(OcrEvalError::Invalid(
            "renderer PNG dimensions must be nonzero",
        ));
    }
    u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or(OcrEvalError::Invalid("renderer PNG pixel count overflow"))
}

fn pdf_page_count(bytes: &[u8]) -> Result<u32, OcrEvalError> {
    if !bytes.starts_with(b"%PDF-") {
        return Err(OcrEvalError::Invalid("renderer output is not a PDF"));
    }
    let needle = b"/Type /Page";
    if bytes.len() < needle.len() {
        return Ok(0);
    }
    // `/Type /Pages` must not be counted as a page object. The bytes following
    // the singular token are a delimiter in canonical reportlab output.
    let mut pages = 0_u32;
    for offset in 0..=bytes.len() - needle.len() {
        if &bytes[offset..offset + needle.len()] == needle
            && bytes
                .get(offset + needle.len())
                .is_none_or(|byte| *byte != b's')
        {
            pages = pages
                .checked_add(1)
                .ok_or(OcrEvalError::Invalid("PDF page count overflow"))?;
        }
    }
    Ok(pages)
}

fn bind_strict_input(
    path: &Path,
    maximum: u64,
) -> Result<crate::strict_artifact::StrictArtifact, OcrEvalError> {
    if !path.is_absolute() {
        return Err(OcrEvalError::AdapterPath(path.to_owned()));
    }
    let maximum = usize::try_from(maximum)
        .map_err(|_| OcrEvalError::DocumentBinding("input byte bound is unsupported"))?;
    crate::strict_artifact::bind_strict(path, maximum).map_err(|_| {
        OcrEvalError::DocumentBinding("input is not a stable bounded single-link regular file")
    })
}
fn validate_render_request(request: &RenderRequest) -> Result<(), OcrEvalError> {
    if request.schema != RENDER_REQUEST_SCHEMA
        || request.request_id.is_empty()
        || request.input_images.is_empty()
        || request.input_images.len() > MAX_RENDER_INPUT_IMAGES
        || !Path::new(&request.output_pdf).is_absolute()
    {
        return Err(OcrEvalError::Invalid("invalid renderer request"));
    }
    let mut unique = std::collections::BTreeSet::new();
    for image in &request.input_images {
        if !Path::new(image).is_absolute() || !unique.insert(image) {
            return Err(OcrEvalError::Invalid(
                "renderer inputs must be unique absolute paths",
            ));
        }
    }
    let output = Path::new(&request.output_pdf);
    let parent = output
        .parent()
        .ok_or(OcrEvalError::Invalid("renderer output has no parent"))?;
    if !fs::symlink_metadata(parent)?.file_type().is_dir() {
        return Err(OcrEvalError::Invalid(
            "renderer output parent must be an existing directory",
        ));
    }
    if fs::symlink_metadata(&request.output_pdf).is_ok() {
        return Err(OcrEvalError::ReportAlreadyExists(PathBuf::from(
            &request.output_pdf,
        )));
    }
    Ok(())
}

fn invoke_renderer_command<T: Serialize>(
    command: &RendererCommand,
    request: &T,
    limits: &RendererLimits,
) -> Result<(Vec<u8>, Vec<u8>), OcrEvalError> {
    validate_renderer_command(command)?;
    let mut input = serde_json::to_vec(request)?;
    input.push(b'\n');
    if input.len() > limits.max_stdin_bytes {
        return Err(OcrEvalError::RequestTooLarge {
            limit: limits.max_stdin_bytes,
        });
    }
    let mut process = Command::new(&command.python);
    process
        .arg(&command.adapter)
        .env_clear()
        .env("PYTHONHASHSEED", "0")
        .env("PYTHONNOUSERSITE", "1");
    let output = run_bounded_command(
        &mut process,
        BoundedProcessOptions {
            timeout: limits.timeout,
            max_stdout_bytes: limits.max_stdout_bytes,
            max_stderr_bytes: limits.max_stderr_bytes,
        },
        Some(BoundedStdin::new(input, limits.max_stdin_bytes)),
    )
    .map_err(|error| match error {
        BoundedProcessError::Timeout { .. } => OcrEvalError::Timeout(limits.timeout),
        BoundedProcessError::OutputLimit { stream, limit } => {
            OcrEvalError::OutputTooLarge { stream, limit }
        }
        BoundedProcessError::Write(source) => OcrEvalError::Io(source),
        other => OcrEvalError::Io(io::Error::other(other.to_string())),
    })?;
    if !output.status.success() {
        return Err(OcrEvalError::AdapterFailed {
            status: output.status.to_string(),
            stderr: output.stderr,
        });
    }
    Ok((output.stdout.into_bytes(), output.stderr.into_bytes()))
}
fn parse_render_line(
    bytes: &[u8],
    request: &RenderRequest,
) -> Result<RenderResponse, OcrEvalError> {
    if bytes.is_empty()
        || bytes.ends_with(b"\n\n")
        || bytes[..bytes.len().saturating_sub(1)].contains(&b'\n')
    {
        return Err(OcrEvalError::Invalid(
            "adapter must emit exactly one JSONL response",
        ));
    }
    let response: RenderResponse =
        serde_json::from_slice(bytes.strip_suffix(b"\n").unwrap_or(bytes))?;
    if response.schema != RENDER_RESPONSE_SCHEMA
        || response.request_id != request.request_id
        || response.output_pdf != request.output_pdf
        || response.page_count != request.input_images.len() as u32
        || !valid_sha256(&response.output_sha256)
    {
        return Err(OcrEvalError::Invalid(
            "renderer response schema or identity mismatch",
        ));
    }
    Ok(response)
}

fn validate_renderer_command(command: &RendererCommand) -> Result<(), OcrEvalError> {
    for path in [&command.python, &command.adapter] {
        if !path.is_absolute() {
            return Err(OcrEvalError::AdapterPath(path.to_owned()));
        }
        if fs::canonicalize(path)? != *path {
            return Err(OcrEvalError::Invalid(
                "adapter command paths must be canonical absolute paths",
            ));
        }
        let meta = fs::symlink_metadata(path)?;
        if !meta.file_type().is_file() {
            return Err(OcrEvalError::Invalid(
                "adapter command must use regular non-symlink files",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if path == &command.python && meta.permissions().mode() & 0o111 == 0 {
                return Err(OcrEvalError::Invalid("python executable is not executable"));
            }
        }
    }
    Ok(())
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        output.push(TABLE[((value >> 18) & 63) as usize] as char);
        output.push(TABLE[((value >> 12) & 63) as usize] as char);
        output.push(if chunk.len() > 1 {
            TABLE[((value >> 6) & 63) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[(value & 63) as usize] as char
        } else {
            '='
        });
    }
    output
}
fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Keep test loopback servers from replying before a segmented client request
    // has delivered its complete body. This is intentionally test-only: the
    // production client owns its own bounded HTTP request implementation.
    const MAX_LOOPBACK_REQUEST_HEADER_BYTES: usize = 64 * 1024;

    fn loopback_request_content_length(headers: &[u8]) -> usize {
        let header_text = std::str::from_utf8(headers).expect("loopback request headers are UTF-8");
        let mut content_length = None;
        for line in header_text.split("\r\n").skip(1) {
            if line.is_empty() {
                break;
            }
            let (name, value) = line
                .split_once(':')
                .expect("loopback request header has a colon");
            if name.eq_ignore_ascii_case("content-length") {
                assert!(content_length.is_none(), "duplicate Content-Length");
                let value = value.trim_matches(|byte: char| byte.is_ascii_whitespace());
                assert!(
                    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()),
                    "Content-Length is decimal"
                );
                content_length = Some(value.parse::<usize>().expect("Content-Length fits usize"));
            }
        }
        content_length.expect("exactly one Content-Length")
    }

    fn read_loopback_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
        use std::io::Read as _;

        let total_limit = MAX_LOOPBACK_REQUEST_HEADER_BYTES
            .checked_add(MAX_PROVIDER_REQUEST_BYTES)
            .expect("loopback request total limit fits usize");
        let mut request = Vec::new();
        let mut declared_total = None;
        let mut chunk = [0_u8; 4096];

        loop {
            if let Some(total) = declared_total
                && request.len() == total
            {
                return request;
            }
            assert!(
                request.len() < total_limit,
                "loopback request exceeds bound"
            );
            let remaining = declared_total.unwrap_or(total_limit) - request.len();
            let read_cap = remaining.min(chunk.len());
            let read = stream
                .read(&mut chunk[..read_cap])
                .expect("loopback reads request");
            assert_ne!(read, 0, "loopback request ended before its declared body");
            request.extend_from_slice(&chunk[..read]);

            if declared_total.is_none()
                && let Some(header_end) = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|position| position + 4)
            {
                assert!(
                    header_end <= MAX_LOOPBACK_REQUEST_HEADER_BYTES,
                    "loopback request headers exceed bound"
                );
                let content_length = loopback_request_content_length(&request[..header_end]);
                let total = header_end
                    .checked_add(content_length)
                    .expect("loopback request length fits usize");
                assert!(total <= total_limit, "loopback request body exceeds bound");
                assert!(request.len() <= total, "loopback read past declared body");
                declared_total = Some(total);
            } else if declared_total.is_none() {
                assert!(
                    request.len() < MAX_LOOPBACK_REQUEST_HEADER_BYTES,
                    "loopback request headers exceed bound"
                );
            }
        }
    }

    fn loopback_response(
        status: &str,
        headers: Vec<(&'static str, &'static str)>,
        body: &'static str,
    ) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        use std::{io::Write as _, net::TcpListener};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let status = status.to_owned();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_loopback_request(&mut stream);
            let mut response = format!("HTTP/1.1 {status}\r\nConnection: close\r\n");
            for (name, value) in headers {
                response.push_str(name);
                response.push_str(": ");
                response.push_str(value);
                response.push_str("\r\n");
            }
            response.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
            stream.write_all(response.as_bytes()).unwrap();
            request
        });
        (endpoint, handle)
    }

    fn loopback_raw_response(
        response: Vec<u8>,
        delay: Duration,
        replacement: Option<(PathBuf, Vec<u8>)>,
    ) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        use std::{io::Write as _, net::TcpListener};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_loopback_request(&mut stream);
            if let Some((path, bytes)) = replacement {
                fs::write(path, bytes).unwrap();
            }
            std::thread::sleep(delay);
            let _ = stream.write_all(&response);
            request
        });
        (endpoint, handle)
    }

    fn provider_document() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let document = directory.path().join("fixture.pdf");
        fs::write(&document, b"fixture-pdf").unwrap();
        (directory, document)
    }

    #[cfg(unix)]
    fn adapter_test_program(body: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("program");
        let adapter = directory.path().join("adapter.py");
        fs::write(&program, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(&adapter, b"# explicit adapter identity\n").unwrap();
        (
            directory,
            fs::canonicalize(program).unwrap(),
            fs::canonicalize(adapter).unwrap(),
        )
    }

    fn truth() -> GroundTruth {
        parse_ground_truth(r#"{"schema":"kio.ocr.ground-truth/v1","table":{"page_index":0,"expected_cell_texts":["Alpha","Beta"]},"japanese":{"page_index":1,"full_text":"日本 語"},"images":{"page_index":2,"expected_count":2},"formula":{"page_index":3,"expected_tokens":["E=mc^2"]}}"#.as_bytes()).unwrap()
    }
    fn response() -> OcrResponse {
        parse_response(r#"{"schema":"kio.ocr.response/v2","request_id":"test","document_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","model":"mistral-ocr-4-1","pages":[{"index":0,"markdown":"| alpha | BETA |","image_count":0},{"index":1,"markdown":"xx日本語yy","image_count":0},{"index":2,"markdown":"","image_count":2},{"index":3,"markdown":"E = mc^2","image_count":0}]}"#.as_bytes()).unwrap()
    }
    #[test]
    fn vectors_cover_metrics_and_verdict() {
        let report = evaluate(&truth(), &response()).unwrap();
        assert_eq!(report.verdict, Verdict::Passed);
        assert_eq!(report.table.recall, 1.0);
        assert_eq!(report.japanese.cer, 0.0);
        assert_eq!(
            report.formula.classification,
            FormulaClassification::Textized
        );
    }
    #[test]
    fn image_fallback_is_classified_without_changing_core_verdict() {
        let mut response = response();
        response.pages[3].markdown.clear();
        response.pages[3].image_count = 1;
        let report = evaluate(&truth(), &response).unwrap();
        assert_eq!(
            report.formula.classification,
            FormulaClassification::ImageFallback
        );
        assert_eq!(report.verdict, Verdict::Passed);
    }
    #[test]
    fn malformed_and_duplicate_pages_are_rejected() {
        assert!(
            parse_response(
                br#"{"schema":"kio.ocr.response/v1","pages":[{"index":0},{"index":0}]}"#
            )
            .is_err()
        );
        assert!(parse_ground_truth(br#"{"schema":"v0"}"#).is_err());
    }
    #[test]
    fn old_schema_and_mutable_model_are_rejected() {
        assert!(parse_response(br#"{"schema":"kio.ocr.response/v1","pages":[]}"#).is_err());
        assert!(
            normalize_mistral_response(
                br#"{"model":"mistral-ocr-latest","pages":[]}"#,
                "r",
                &"a".repeat(64),
                MISTRAL_OCR_MODEL
            )
            .is_err()
        );
    }
    #[test]
    fn provider_response_normalizes_increasing_source_page_indexes() {
        let hash = "a".repeat(64);
        assert!(
            normalize_mistral_response(
                br#"{"model":"mistral-ocr-4-1","model":"mistral-ocr-4-1","pages":[]}"#,
                "r",
                &hash,
                MISTRAL_OCR_MODEL,
            )
            .is_err()
        );
        let normalized = normalize_mistral_response(
            br#"{"model":"mistral-ocr-4-1","pages":[{"index":1,"markdown":"one","images":[]},{"index":2,"markdown":"two","images":[]}]}"#,
            "r",
            &hash,
            MISTRAL_OCR_MODEL,
        )
        .unwrap();
        assert_eq!(
            normalized
                .pages
                .iter()
                .map(|page| page.index)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert!(
            normalize_mistral_response(
                br#"{"model":"mistral-ocr-4-1","pages":[{"index":2,"markdown":"","images":[]},{"index":1,"markdown":"","images":[]}]}"#,
                "r",
                &hash,
                MISTRAL_OCR_MODEL,
            )
            .is_err()
        );
    }

    #[test]
    fn provider_http_binds_the_exact_request_and_discards_image_payloads() {
        let (_directory, document) = provider_document();
        let body = r#"{"model":"mistral-ocr-4-1","pages":[{"index":0,"markdown":"ok","images":[{"image_base64":"never-persist"}]}]}"#;
        let (endpoint, received) =
            loopback_response("200 OK", vec![("Content-Type", "application/json")], body);
        let response = request_mistral_ocr_at(
            &endpoint,
            "request-1",
            MISTRAL_OCR_MODEL,
            &document,
            false,
            "credential-that-must-not-leak",
        )
        .unwrap();
        let request = String::from_utf8(received.join().unwrap()).unwrap();
        let request_lower = request.to_ascii_lowercase();
        assert!(request_lower.contains("authorization: bearer credential-that-must-not-leak"));
        assert!(request_lower.contains("accept-encoding: identity"));
        assert!(request.contains("\"include_image_base64\":false"));
        assert!(request.contains("\"include_blocks\":false"));
        assert_eq!(response.pages[0].image_count, 1);
        let persisted = serde_json::to_string(&response).unwrap();
        assert!(!persisted.contains("never-persist"));
        assert!(!persisted.contains("credential-that-must-not-leak"));
        assert_eq!(response.document_sha256, hex_sha256(b"fixture-pdf"));
    }

    #[test]
    fn provider_agent_disables_proxy_redirects_and_status_shortcuts() {
        let limits = ProviderLimits::default();
        let agent = provider_agent(limits);
        assert!(agent.config().proxy().is_none());
        assert_eq!(agent.config().max_redirects(), 0);
        assert!(!agent.config().http_status_as_error());
        assert_eq!(
            agent.config().timeouts().connect,
            Some(limits.connect_timeout)
        );
        assert_eq!(
            agent.config().timeouts().global,
            Some(limits.global_timeout)
        );
    }

    #[test]
    fn provider_http_timeout_is_bounded_and_secret_free() {
        let (_directory, document) = provider_document();
        let response = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 40\r\nConnection: close\r\n\r\n"
            .to_vec();
        let (endpoint, received) =
            loopback_raw_response(response, Duration::from_millis(150), None);
        let limits = ProviderLimits {
            connect_timeout: Duration::from_millis(30),
            global_timeout: Duration::from_millis(30),
            max_response_bytes: 1024,
        };
        let secret = "credential-that-must-not-leak";
        let error = request_mistral_ocr_at_with_limits(
            &endpoint,
            "request-1",
            MISTRAL_OCR_MODEL,
            &document,
            false,
            secret,
            limits,
        )
        .unwrap_err();
        assert!(matches!(error, OcrEvalError::Timeout(_)));
        assert!(!error.to_string().contains(secret));
        received.join().unwrap();
    }

    #[test]
    fn provider_http_rejects_declared_and_chunked_oversize_responses() {
        let (_directory, document) = provider_document();
        let limits = ProviderLimits {
            connect_timeout: Duration::from_secs(1),
            global_timeout: Duration::from_secs(1),
            max_response_bytes: 128,
        };
        let declared = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 129\r\nConnection: close\r\n\r\n"
            .to_vec();
        let oversized = vec![b'x'; 256];
        let mut chunked = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:X}\r\n",
            oversized.len()
        )
        .into_bytes();
        chunked.extend_from_slice(&oversized);
        chunked.extend_from_slice(b"\r\n0\r\n\r\n");

        for raw in [declared, chunked] {
            let (endpoint, received) = loopback_raw_response(raw, Duration::ZERO, None);
            assert!(matches!(
                request_mistral_ocr_at_with_limits(
                    &endpoint,
                    "request-1",
                    MISTRAL_OCR_MODEL,
                    &document,
                    false,
                    "credential",
                    limits,
                ),
                Err(OcrEvalError::InputTooLarge { limit: 128 })
            ));
            received.join().unwrap();
        }
    }

    #[test]
    fn provider_http_rejects_malformed_json_and_content_length_anomalies() {
        let (_directory, document) = provider_document();
        let cases = [
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1\r\nContent-Length: 1\r\nConnection: close\r\n\r\n{".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: nope\r\nConnection: close\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1\r\nConnection: close\r\n\r\n{}".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1\r\nConnection: close\r\n\r\n{".to_vec(),
        ];
        for raw in cases {
            let (endpoint, received) = loopback_raw_response(raw, Duration::ZERO, None);
            assert!(
                request_mistral_ocr_at(
                    &endpoint,
                    "request-1",
                    MISTRAL_OCR_MODEL,
                    &document,
                    false,
                    "credential",
                )
                .is_err()
            );
            received.join().unwrap();
        }
    }

    #[test]
    fn provider_http_rechecks_document_identity_after_the_response() {
        let (_directory, document) = provider_document();
        let body = r#"{"model":"mistral-ocr-4-1","pages":[]}"#;
        let raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes();
        let (endpoint, received) = loopback_raw_response(
            raw,
            Duration::ZERO,
            Some((document.clone(), b"changed-fixture-pdf".to_vec())),
        );
        assert!(matches!(
            request_mistral_ocr_at(
                &endpoint,
                "request-1",
                MISTRAL_OCR_MODEL,
                &document,
                false,
                "credential",
            ),
            Err(OcrEvalError::DocumentBinding(_))
        ));
        received.join().unwrap();
    }

    #[test]
    fn provider_http_rejects_status_headers_duplicates_models_and_pages() {
        let (_directory, document) = provider_document();
        for (status, headers, body) in [
            (
                "401 Unauthorized",
                vec![("Content-Type", "application/json")],
                "{}",
            ),
            (
                "500 Internal Server Error",
                vec![("Content-Type", "application/json")],
                "{}",
            ),
            (
                "302 Found",
                vec![("Location", "/next"), ("Content-Type", "application/json")],
                "{}",
            ),
            ("200 OK", vec![("Content-Type", "text/plain")], "{}"),
            (
                "200 OK",
                vec![
                    ("Content-Type", "application/json"),
                    ("Content-Encoding", "gzip"),
                ],
                "{}",
            ),
            (
                "200 OK",
                vec![("Content-Type", "application/json")],
                r#"{"model":"mistral-ocr-4-1","model":"mistral-ocr-4-1","pages":[]}"#,
            ),
            (
                "200 OK",
                vec![("Content-Type", "application/json")],
                r#"{"model":"mistral-ocr-latest","pages":[]}"#,
            ),
            (
                "200 OK",
                vec![("Content-Type", "application/json")],
                r#"{"model":"mistral-ocr-4-1","pages":[{"index":1,"markdown":"","images":[]},{"index":1,"markdown":"","images":[]}]}"#,
            ),
        ] {
            let (endpoint, received) = loopback_response(status, headers, body);
            assert!(
                request_mistral_ocr_at(
                    &endpoint,
                    "request-1",
                    MISTRAL_OCR_MODEL,
                    &document,
                    false,
                    "credential",
                )
                .is_err()
            );
            received.join().unwrap();
        }
    }

    #[test]
    fn provider_response_duplicate_and_old_normalized_artifacts_fail_closed() {
        assert!(
            normalize_mistral_response(
                br#"{"model":"mistral-ocr-4-1","pages":[],"pages":[]}"#,
                "r",
                &"a".repeat(64),
                MISTRAL_OCR_MODEL,
            )
            .is_err()
        );
        assert!(parse_response(
            br#"{"schema":"kio.ocr.response/v2","request_id":"r","document_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","model":"mistral-ocr-4-1","pages":[],"pages":[]}"#
        )
        .is_err());
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn explicit_document_binding_and_create_only_report_are_enforced() {
        let directory = tempfile::tempdir().unwrap();
        let document = directory.path().join("fixture.pdf");
        fs::write(&document, b"fixture-pdf").unwrap();
        let binding = bind_document(&document).unwrap();
        assert_eq!(binding.bytes, 11);
        let report = evaluate(&truth(), &response()).unwrap();
        let report_path = directory.path().join("report.json");
        write_report_create_only(&report_path, &report).unwrap();
        assert!(matches!(
            write_report_create_only(&report_path, &report),
            Err(OcrEvalError::ReportAlreadyExists(_))
        ));
        let response_path = directory.path().join("response.json");
        write_response_create_only(&response_path, &response()).unwrap();
        let response_bytes = fs::read(&response_path).unwrap();
        assert!(response_bytes.ends_with(b"\n"));
        assert_eq!(
            parse_response(&response_bytes).unwrap().schema,
            RESPONSE_SCHEMA
        );
    }

    #[cfg(unix)]
    #[test]
    fn document_binding_rejects_symlink_and_hardlink_aliases() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let document = directory.path().join("fixture.pdf");
        fs::write(&document, b"fixture-pdf").unwrap();
        let symlink_path = directory.path().join("symlink.pdf");
        symlink(&document, &symlink_path).unwrap();
        assert!(bind_document(&symlink_path).is_err());

        let hardlink_path = directory.path().join("hardlink.pdf");
        fs::hard_link(&document, &hardlink_path).unwrap();
        assert!(bind_document(&document).is_err());
        assert!(bind_document(&hardlink_path).is_err());
    }

    #[test]
    fn renderer_response_is_identity_bound_and_create_only() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("out.pdf");
        let image = directory.path().join("one.png");
        fs::write(&image, b"image").unwrap();
        let request = RenderRequest {
            schema: RENDER_REQUEST_SCHEMA.into(),
            request_id: "render-1".into(),
            output_pdf: output.to_string_lossy().into_owned(),
            input_images: vec![image.to_string_lossy().into_owned()],
        };
        validate_render_request(&request).unwrap();
        let response = format!(
            "{}\n",
            serde_json::json!({
                "schema": RENDER_RESPONSE_SCHEMA,
                "request_id": "render-1",
                "output_pdf": output.to_string_lossy(),
                "output_bytes": 1,
                "output_sha256": "a".repeat(64),
                "page_count": 1,
            })
        );
        assert_eq!(
            parse_render_line(response.as_bytes(), &request)
                .unwrap()
                .page_count,
            1
        );
        fs::write(&output, b"existing").unwrap();
        assert!(validate_render_request(&request).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn renderer_child_inherits_no_provider_or_parent_credentials() {
        use std::sync::{Mutex, OnceLock};

        static ENVIRONMENT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _guard = ENVIRONMENT_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        let name = "KIO_OCR_RENDERER_SECRET_TEST";
        let old = std::env::var_os(name);
        // SAFETY: this test owns the unique sentinel name and restores it while
        // holding the test-local lock before returning.
        unsafe { std::env::set_var(name, "parent-secret") };
        let script = format!(
            "read _; if test -z \"${name}\" && test -z \"$MISTRAL_API_KEY\"; then printf '{{}}\\n'; else exit 9; fi"
        );
        let (_directory, python, adapter) = adapter_test_program(&script);
        let output = invoke_renderer_command(
            &RendererCommand { python, adapter },
            &serde_json::json!({"request": "renderer-environment"}),
            &RendererLimits::default(),
        );
        // SAFETY: paired with the mutation above while holding the same lock.
        unsafe {
            if let Some(value) = old {
                std::env::set_var(name, value);
            } else {
                std::env::remove_var(name);
            }
        }
        assert_eq!(String::from_utf8(output.unwrap().0).unwrap(), "{}\n");
    }

    #[test]
    fn oversized_png_header_is_rejected_without_a_decoder() {
        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("bomb.png");
        let mut header = Vec::from(&b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"[..]);
        header.extend_from_slice(&100_000_u32.to_be_bytes());
        header.extend_from_slice(&100_000_u32.to_be_bytes());
        fs::write(&image, header).unwrap();
        assert!(png_pixels(&fs::read(&image).unwrap()).unwrap() > MAX_RENDER_IMAGE_PIXELS);
    }
}
