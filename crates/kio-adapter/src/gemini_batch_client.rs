//! Gemini embedding Batch lane client (07 §5.3 の 2026-07-24 訂正 / §5.7).
//!
//! Mirrors [`crate::batch_client`] (the Mistral OCR Batch lane) with one
//! structural difference the spec calls out: the Gemini embedding batch takes
//! its input **inline** (`batch.inputConfig.requests.requests[]`), so there is
//! **no upload phase (相 2a)**. Nothing is ever written to the provider's File
//! API, so no upload residue can exist and this trait deliberately omits
//! `list_uploads` / `delete_upload`. `provider_scope_id` is recorded by the
//! caller immediately before job creation instead.
//!
//! Wire contract (2026-07-24 documentation record):
//!
//! - create: `POST {base}/v1beta/{model}:asyncBatchEmbedContent`
//! - poll:   `GET  {base}/v1beta/{name}` where `name` = `batches/{id}`
//! - list:   `GET  {base}/v1beta/batches?pageSize=N[&pageToken=…]`
//! - cancel: `POST {base}/v1beta/{name}:cancel`
//! - results: inline in the poll response, under BOTH
//!   `metadata.output.inlinedResponses.inlinedResponses[]` and
//!   `response.inlinedResponses.inlinedResponses[]`
//!
//! Every response is a long-running-operation envelope: the batch record lives
//! under `metadata` (not at the top level), the listing returns its page under
//! `operations` (not `batches`), and the result lines sit one level deeper than
//! the key name suggests — the outer `inlinedResponses` is the output
//! destination, the inner one is the repeated field inside it. See
//! [`batch_object`], [`parse_job_listing`] and [`inlined_response_lines`]; the
//! captured shapes are pinned by the `real_*` tests at the bottom of this file.
//!
//! Job-level attribution: the Gemini batch object exposes only `displayName`
//! as a free-form job-level string (unlike Mistral's `metadata` object), so the
//! intent_token rides there under the same convention the Mistral lane uses for
//! its upload filename — see [`batch_display_name`]. That name is the recovery
//! walk's 発見キー (04 §5.8 / 10 §7.5.2).
//!
//! Like the sibling embedding adapter, the live HTTP path is not exercised in
//! the hermetic suite (`docs/07-adapter-spec.md` §5.3); the contract is covered through the mock
//! seam ([`TEST_GEMINI_BATCH_ENV`]).

#[cfg(debug_assertions)]
use std::io::Write as _;

#[cfg(debug_assertions)]
use serde::Deserialize;
use serde_json::{Value, json};

use crate::batch_recovery::{BatchRecoveryContext, PinnedBatchIdentity, configured_qualifier};
use crate::http_policy::{
    HttpPolicy, HttpResponse, authenticated_agent, read_json_bounded, require_success,
};
use crate::{AdapterError, Result};

/// Hermetic test seam: an inline JSON [`MockGeminiBatchScript`]. When set, the
/// resolver returns the mock client and no network call is ever made.
#[cfg(debug_assertions)]
pub const TEST_GEMINI_BATCH_ENV: &str = "KIO_TEST_GEMINI_BATCH";

/// Optional project qualifier, captured once as one input to recovery HMAC.
pub const GEMINI_PROJECT_ID_ENV: &str = "KIO_GEMINI_PROJECT_ID";

const GEMINI_API_ORIGIN: &str = "https://generativelanguage.googleapis.com";

/// A listing page IS metadata-class: its entries carry `state`/`batchStats` and
/// never an `output`, so 100 of them stay well under this.
const BATCH_METADATA_MAX_BYTES: usize = 1024 * 1024;

/// A **poll** response is not metadata-class, however tempting the name is.
/// `GET /v1beta/{name}` is the only batch endpoint there is, so the moment a job
/// succeeds that same document carries every inline result — and carries them
/// TWICE, under `metadata.output` and again under `response`.
///
/// Measured against the live API at 768 dimensions: 47,905 response bytes per
/// member. At the caller's 512-member ceiling
/// (`EMBEDDING_BATCH_JOB_MAX_MEMBERS`) that is ~24.5 MB, so this is sized to
/// cover it with margin and is used for **both** the record and the results —
/// they are one HTTP response and must not be bounded differently.
///
/// Reading the record as metadata (1 MB) is what stranded a real 33-member job:
/// `get_job` failed the size check, the poll site's hold-and-retry rule
/// (04 §5.8 unknown) swallowed the error, and the row sat in flight with no
/// diagnostic while the provider had all 33 vectors ready. The bound scaled
/// with job size, so it passed on every small scope and failed on the big one.
const BATCH_POLL_MAX_BYTES: usize = 48 * 1024 * 1024;

/// Listing pagination page size (`pageSize` query parameter).
const BATCH_LIST_PAGE_SIZE: usize = 100;

/// Hard bound on the pagination walk. A successful inventory must be complete:
/// recovery and cleanup cannot safely infer absence from a truncated listing.
const BATCH_LIST_MAX_PAGES: usize = 50;

/// Provider cap on an inline batch is 20 MB. Bound the serialized request array
/// below that with margin, so a job is rejected locally (a contract error the
/// caller can split on) rather than by the provider after a round trip.
pub const MAX_INLINE_REQUEST_BYTES: usize = 16 * 1024 * 1024;

/// Secondary bound on one job's request count. 07 §5.3 requires the caller to
/// keep a job bounded; this is the ceiling the client itself enforces.
pub const MAX_INLINE_REQUESTS: usize = 2_048;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// `BatchState` enum of the Gemini batch object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeminiBatchState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Expired,
    Other(String),
}

impl GeminiBatchState {
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_uppercase().as_str() {
            "BATCH_STATE_PENDING" | "JOB_STATE_PENDING" | "PENDING" => Self::Pending,
            "BATCH_STATE_RUNNING" | "JOB_STATE_RUNNING" | "RUNNING" => Self::Running,
            "BATCH_STATE_SUCCEEDED" | "JOB_STATE_SUCCEEDED" | "SUCCEEDED" => Self::Succeeded,
            "BATCH_STATE_FAILED" | "JOB_STATE_FAILED" | "FAILED" => Self::Failed,
            "BATCH_STATE_CANCELLED" | "JOB_STATE_CANCELLED" | "CANCELLED" => Self::Cancelled,
            "BATCH_STATE_EXPIRED" | "JOB_STATE_EXPIRED" | "EXPIRED" => Self::Expired,
            other => Self::Other(other.to_owned()),
        }
    }

    /// Terminal states — the poll lane stops re-scheduling on these.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Expired
        )
    }
}

/// One Gemini batch job as the provider reports it.
#[derive(Debug, Clone)]
pub struct GeminiBatchJobRecord {
    /// Resource name, `batches/{id}` — the value stored in `batch_job_id`.
    pub name: String,
    pub state: GeminiBatchState,
    /// The job-level free-form label the intent_token rides in
    /// ([`batch_display_name`]).
    pub display_name: String,
    /// Set when the provider chose the file-output form instead of inline.
    /// The embedding lane always submits inline input, but a provider that
    /// answers with `responsesFile` is surfaced rather than silently treated
    /// as an empty result.
    pub responses_file: Option<String>,
}

/// The result of one [`GeminiBatchClient::poll_job`] call.
#[derive(Debug, Clone)]
pub struct GeminiBatchPoll {
    pub record: GeminiBatchJobRecord,
    /// `Some` when the polled document carried inline results — i.e. the job
    /// finished and its output rode along in the same response. `None` while
    /// it is still queued or running.
    ///
    /// A record whose state is `Succeeded` but whose results are `None` is an
    /// anomaly, not a normal in-between: the caller must hold the row and say
    /// it could not read it, never settle it as complete.
    pub results: Option<Vec<GeminiBatchEmbedOutput>>,
}

/// One embedding result line, keyed back to the caller's request key.
#[derive(Debug, Clone)]
pub struct GeminiBatchEmbedOutput {
    /// The caller's key, echoed through the per-request `metadata.key`.
    pub key: String,
    /// `Some` on success; `None` when this line carries an error instead.
    pub values: Option<Vec<f32>>,
    /// Provider error object for this line, when present.
    pub error: Option<Value>,
    /// `response.usageMetadata.promptTokenCount` for this line.
    ///
    /// The endpoint DOES report token counts — both here and on the
    /// synchronous `embedContent`. The adapter used to assert the opposite in
    /// a comment and degrade `usage` to `None`, which made every embedding
    /// charge settle at the reservation estimate instead of what was actually
    /// billed (07 §5.3, 2026-07-25 correction).
    pub prompt_tokens: Option<u64>,
}

/// One inline request element: the caller's key plus the text to embed.
#[derive(Debug, Clone)]
pub struct GeminiBatchEmbedInput {
    pub key: String,
    pub text: String,
}

/// Job-level attribution. The intent_token embedding is the ONLY attribution a
/// Gemini batch job carries (04 §5.8 発見キー / 10 §7.5.2 — a job whose display
/// name cannot be parsed is unknown, report-only).
#[must_use]
pub fn batch_display_name(intent_token: &str) -> String {
    format!("kio-{intent_token}")
}

/// Inverse of [`batch_display_name`].
#[must_use]
pub fn display_name_intent_token(display_name: &str) -> Option<&str> {
    let token = display_name.strip_prefix("kio-")?;
    (!token.is_empty()).then_some(token)
}

/// Build the `batch` create body for an inline embedding job. Separated from
/// the client so the shape is unit-testable without any network seam.
///
/// `model` is the pinned model id (e.g. `gemini-embedding-2`); it is sent both
/// on the batch and on every inner request, which is what the REST reference's
/// `EmbedContentRequest` requires.
pub fn inline_embed_batch_body(
    model: &str,
    display_name: &str,
    dimensions: u32,
    inputs: &[GeminiBatchEmbedInput],
) -> Result<Value> {
    if inputs.is_empty() {
        return Err(AdapterError::ContractViolation(
            "Gemini embedding batch requires at least one request".to_owned(),
        ));
    }
    if inputs.len() > MAX_INLINE_REQUESTS {
        return Err(AdapterError::ContractViolation(format!(
            "Gemini embedding batch has {} requests, over the {MAX_INLINE_REQUESTS} inline bound",
            inputs.len()
        )));
    }
    let model_name = qualified_model(model);
    let requests = inputs
        .iter()
        .map(|input| {
            json!({
                "request": {
                    "model": model_name,
                    "content": { "parts": [{ "text": input.text }] },
                    "outputDimensionality": dimensions,
                },
                "metadata": { "key": input.key },
            })
        })
        .collect::<Vec<_>>();
    let body = json!({
        "batch": {
            "displayName": display_name,
            "inputConfig": { "requests": { "requests": requests } },
        }
    });
    let encoded = serde_json::to_vec(&body).map_err(|error| {
        AdapterError::ContractViolation(format!("Gemini embedding batch body: {error}"))
    })?;
    if encoded.len() > MAX_INLINE_REQUEST_BYTES {
        return Err(AdapterError::ContractViolation(format!(
            "Gemini embedding batch body is {} bytes, over the {MAX_INLINE_REQUEST_BYTES} inline bound",
            encoded.len()
        )));
    }
    Ok(body)
}

/// `models/x` passes through; a bare `x` is qualified.
fn qualified_model(model: &str) -> String {
    if model.starts_with("models/") {
        model.to_owned()
    } else {
        format!("models/{model}")
    }
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// 07 §5.5 client operations for the embedding Batch lane. One provider job
/// carries ONE task's input (1 job = 1 task), the same v1 posture the OCR lane
/// takes, so the per-task-key `batch_job_id` column keys the recovery walk.
///
/// No upload operations: the input is inline, so 相 2a does not exist and the
/// provider holds no residue to sweep.
pub trait GeminiBatchClient {
    fn provider_scope_id(&self) -> Result<String>;

    /// Create an inline embedding batch job; returns the created job record.
    fn create_embedding_job(
        &self,
        model: &str,
        dimensions: u32,
        display_name: &str,
        inputs: &[GeminiBatchEmbedInput],
    ) -> Result<GeminiBatchJobRecord>;

    /// One poll: the job record and, once the job has finished, its results.
    ///
    /// This is deliberately ONE call. `GET /v1beta/{name}` is the only batch
    /// endpoint, so the state and the output live in the same document —
    /// asking twice downloaded a succeeded job's payload twice (1.5 MB, for a
    /// 33-member job) and, worse, let the two reads acquire DIFFERENT byte
    /// bounds. One of them was sized for "metadata", which silently stranded
    /// every job past ~20 members. A single fetch makes that divergence
    /// unrepresentable rather than merely fixed.
    fn poll_job(&self, name: &str) -> Result<GeminiBatchPoll>;

    fn list_jobs(&self) -> Result<Vec<GeminiBatchJobRecord>>;
}

// ---------------------------------------------------------------------------
// Parsing (shared by the real client and the mock)
// ---------------------------------------------------------------------------

/// Unwrap the long-running-operation envelope. The batch record itself lives
/// under `metadata`; once the job is `done` a copy of the output also appears
/// under `response` (which carries no `state`/`name`, so it never wins here).
/// A bare, unwrapped object is accepted too.
fn batch_object(value: &Value) -> &Value {
    for key in ["metadata", "response"] {
        if let Some(inner) = value.get(key)
            && (inner.get("state").is_some() || inner.get("name").is_some())
        {
            return inner;
        }
    }
    value
}

pub(crate) fn parse_job_record(value: &Value) -> Result<GeminiBatchJobRecord> {
    let object = batch_object(value);
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        // A listing entry carries `name` on the envelope; only the poll
        // response repeats it inside `metadata`. Falling back keeps an entry
        // that omits the inner copy from failing the whole page.
        .or_else(|| {
            value
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
        })
        .ok_or_else(|| {
            AdapterError::ContractViolation("Gemini batch response missing name".to_owned())
        })?
        .to_owned();
    let state = object
        .get("state")
        .and_then(Value::as_str)
        .map(GeminiBatchState::parse)
        .ok_or_else(|| {
            AdapterError::ContractViolation("Gemini batch response missing state".to_owned())
        })?;
    let display_name = object
        .get("displayName")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let responses_file = object
        .get("dest")
        .and_then(|dest| dest.get("responsesFile"))
        .or_else(|| object.get("responsesFile"))
        .and_then(Value::as_str)
        .filter(|file| !file.is_empty())
        .map(str::to_owned);
    Ok(GeminiBatchJobRecord {
        name,
        state,
        display_name,
        responses_file,
    })
}

/// Extract the entries of a `batches.list` page. The provider returns them
/// under `operations` (it is an operations listing); `batches` is the name used
/// by the REST reference. Reading only `batches` yielded an empty page against
/// the real API — silently, since an empty page is also how the walk learns
/// there is nothing to recover.
pub(crate) fn parse_job_listing(value: &Value) -> Result<Vec<GeminiBatchJobRecord>> {
    let Some(entries) = value
        .get("operations")
        .or_else(|| value.get("batches"))
        .and_then(Value::as_array)
    else {
        return Ok(Vec::new());
    };
    entries.iter().map(parse_job_record).collect()
}

/// Locate the result lines inside a poll response.
///
/// The array sits one level below the key that names it: the outer
/// `inlinedResponses` is the output destination, the inner one is the repeated
/// field inside it. The provider exposes two copies — `metadata.output.…` and
/// `response.…` — and the REST reference documents a third, singly-nested
/// `dest.inlinedResponses[]`. Resolving only the singly-nested form is what
/// made every real job unreadable: `fetch_inlined_results` raised a contract
/// violation, the poll site held the row as "still in flight" (04 §5.8
/// unknown), and the job stayed uncollected forever with no diagnostic.
fn inlined_response_lines(value: &Value) -> Option<&Vec<Value>> {
    /// Peel however many `inlinedResponses` wrappers stand between the key and
    /// the array, so both the nested and the flat shape resolve.
    fn lines_at(node: &Value) -> Option<&Vec<Value>> {
        match node {
            Value::Array(lines) => Some(lines),
            _ => node.get("inlinedResponses").and_then(lines_at),
        }
    }
    let object = batch_object(value);
    [
        object.get("output"),
        object.get("dest"),
        Some(object),
        value.get("response"),
    ]
    .into_iter()
    .flatten()
    .find_map(|root| root.get("inlinedResponses").and_then(lines_at))
}

/// Extract `inlinedResponses[]` into keyed outputs. A line without a usable
/// `metadata.key` is a contract violation: silently dropping it would leave the
/// caller's chunk permanently unembedded while the job counted as complete.
/// The API returns counts as JSON numbers here and as quoted strings in
/// `batchStats`; accept both rather than silently dropping a token count.
fn json_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}

pub(crate) fn parse_inlined_results(value: &Value) -> Result<Vec<GeminiBatchEmbedOutput>> {
    let lines = inlined_response_lines(value).ok_or_else(|| {
        AdapterError::ContractViolation("Gemini batch result missing inlinedResponses".to_owned())
    })?;
    let mut outputs = Vec::with_capacity(lines.len());
    for line in lines {
        let key = line
            .get("metadata")
            .and_then(|metadata| metadata.get("key"))
            .and_then(Value::as_str)
            .filter(|key| !key.is_empty())
            .ok_or_else(|| {
                AdapterError::ContractViolation(
                    "Gemini batch result line missing metadata.key".to_owned(),
                )
            })?
            .to_owned();
        let output = line.get("output").unwrap_or(line);
        // `response.usageMetadata.promptTokenCount` — what the provider
        // actually billed for this line.
        let prompt_tokens = output
            .get("response")
            .and_then(|response| response.get("usageMetadata"))
            .and_then(|usage| usage.get("promptTokenCount"))
            .and_then(json_u64);
        if let Some(error) = output.get("error").filter(|error| !error.is_null()) {
            outputs.push(GeminiBatchEmbedOutput {
                key,
                values: None,
                error: Some(error.clone()),
                prompt_tokens,
            });
            continue;
        }
        let raw = output
            .get("response")
            .and_then(|response| response.get("embedding"))
            .and_then(|embedding| embedding.get("values"))
            .and_then(Value::as_array)
            .ok_or_else(|| {
                AdapterError::ContractViolation(format!(
                    "Gemini batch result line {key} missing embedding values"
                ))
            })?;
        let mut values = Vec::with_capacity(raw.len());
        for element in raw {
            let number = element.as_f64().ok_or_else(|| {
                AdapterError::ContractViolation(format!(
                    "Gemini batch result line {key} has a non-numeric embedding element"
                ))
            })?;
            values.push(number as f32);
        }
        outputs.push(GeminiBatchEmbedOutput {
            key,
            values: Some(values),
            error: None,
            prompt_tokens,
        });
    }
    Ok(outputs)
}

// ---------------------------------------------------------------------------
// Real client
// ---------------------------------------------------------------------------

/// Real Gemini Batch REST client. Auth and base-url posture mirror the
/// embedding adapter exactly: `tools.toml [embedding] auth` is the credential
/// authority and the production origin is fixed by the built-in target.
#[derive(Debug, Clone)]
pub struct EnvGeminiBatchClient {
    identity: PinnedBatchIdentity,
    http_policy: HttpPolicy,
}

impl EnvGeminiBatchClient {
    pub fn new(context: &BatchRecoveryContext) -> Result<Self> {
        let credential = crate::tool_lock::resolve_role_api_key("embedding")?.ok_or_else(|| {
            AdapterError::Auth("declared gemini batch credential is unavailable".into())
        })?;
        Self::from_captured(context, GEMINI_API_ORIGIN, credential)
    }

    fn from_captured(
        context: &BatchRecoveryContext,
        origin: &str,
        credential: String,
    ) -> Result<Self> {
        let qualifier = configured_qualifier(GEMINI_PROJECT_ID_ENV)?;
        Ok(Self {
            identity: context.capture("gemini", origin, qualifier.as_deref(), credential)?,
            http_policy: HttpPolicy::default(),
        })
    }

    #[cfg(test)]
    fn with_base_url(base_url: impl Into<String>) -> Self {
        crate::tool_lock::with_test_role_auth("embedding", || {
            let credential = crate::tool_lock::resolve_role_api_key("embedding")
                .unwrap()
                .unwrap();
            Self::from_captured(
                &BatchRecoveryContext::from_key([7; 32]),
                &base_url.into(),
                credential,
            )
            .unwrap()
        })
    }

    fn base_url(&self) -> &str {
        self.identity.origin()
    }

    fn get_json(&self, url: &str, max_bytes: usize, context: &str) -> Result<Value> {
        let api_key = self.identity.credential();
        let response = authenticated_agent(self.http_policy)
            .get(url)
            .header("x-goog-api-key", api_key)
            .header("Accept-Encoding", "identity")
            .call()
            .map_err(http_error)
            .and_then(|response| require_success(response, http_status_error))?;
        read_json_bounded(response, max_bytes, context)
    }

    /// `batches.list` uses the request builder's query API so opaque provider
    /// page tokens cannot alter the query string or be sent unescaped.
    fn get_batch_listing(&self, page_token: Option<&str>) -> Result<Value> {
        let api_key = self.identity.credential();
        let url = format!("{}/v1beta/batches", self.base_url());
        let request = authenticated_agent(self.http_policy)
            .get(&url)
            .query("pageSize", BATCH_LIST_PAGE_SIZE.to_string());
        let request = match page_token {
            Some(token) => request.query("pageToken", token),
            None => request,
        };
        let response = request
            .header("x-goog-api-key", api_key)
            .header("Accept-Encoding", "identity")
            .call()
            .map_err(http_error)
            .and_then(|response| require_success(response, http_status_error))?;
        read_json_bounded(response, BATCH_METADATA_MAX_BYTES, "Gemini batch listing")
    }
}

impl GeminiBatchClient for EnvGeminiBatchClient {
    fn provider_scope_id(&self) -> Result<String> {
        Ok(self.identity.scope().to_owned())
    }

    fn create_embedding_job(
        &self,
        model: &str,
        dimensions: u32,
        display_name: &str,
        inputs: &[GeminiBatchEmbedInput],
    ) -> Result<GeminiBatchJobRecord> {
        let body = inline_embed_batch_body(model, display_name, dimensions, inputs)?;
        let api_key = self.identity.credential();
        let url = format!(
            "{}/v1beta/{}:asyncBatchEmbedContent",
            self.base_url(),
            qualified_model(model)
        );
        let response = authenticated_agent(self.http_policy)
            .post(&url)
            .header("x-goog-api-key", api_key)
            .header("Accept-Encoding", "identity")
            .send_json(body)
            .map_err(http_error)
            .and_then(|response| require_success(response, http_status_error))?;
        let value = read_json_bounded(
            response,
            BATCH_METADATA_MAX_BYTES,
            "Gemini batch create response",
        )?;
        parse_job_record(&value)
    }

    fn poll_job(&self, name: &str) -> Result<GeminiBatchPoll> {
        let value = self.get_json(
            &format!("{}/v1beta/{name}", self.base_url()),
            BATCH_POLL_MAX_BYTES,
            "Gemini batch poll response",
        )?;
        let record = parse_job_record(&value)?;
        // Results ride in the SAME document once the job finishes. Their
        // absence is how "still running" looks, so it is not an error here —
        // but a `Succeeded` record with no results is, and the caller is the
        // one that can tell those apart.
        let results = match inlined_response_lines(&value) {
            Some(_) => Some(parse_inlined_results(&value)?),
            None => None,
        };
        Ok(GeminiBatchPoll { record, results })
    }

    fn list_jobs(&self) -> Result<Vec<GeminiBatchJobRecord>> {
        let mut seen_page_tokens = std::collections::HashSet::new();
        let mut records = Vec::new();
        let mut page_token: Option<String> = None;
        for _ in 0..BATCH_LIST_MAX_PAGES {
            let value = self.get_batch_listing(page_token.as_deref())?;
            let entries = parse_job_listing(&value)?;
            if entries.len() > BATCH_LIST_PAGE_SIZE {
                return Err(AdapterError::ContractViolation(format!(
                    "Gemini batch listing returned {} entries, over requested page size {BATCH_LIST_PAGE_SIZE}",
                    entries.len()
                )));
            }
            records.extend(entries);
            let next_page_token = match value.get("nextPageToken") {
                None => return Ok(records),
                Some(Value::String(token)) if !token.is_empty() => token.to_owned(),
                Some(_) => {
                    return Err(AdapterError::ContractViolation(
                        "Gemini batch listing nextPageToken must be a non-empty string".to_owned(),
                    ));
                }
            };
            if !seen_page_tokens.insert(next_page_token.clone()) {
                return Err(AdapterError::ContractViolation(
                    "Gemini batch listing nextPageToken cycle".to_owned(),
                ));
            }
            page_token = Some(next_page_token);
        }
        Err(AdapterError::ContractViolation(format!(
            "Gemini batch listing exceeded {BATCH_LIST_MAX_PAGES} pages before completion"
        )))
    }
}

fn http_error(error: ureq::Error) -> AdapterError {
    AdapterError::Network(format!("Gemini batch transport: {error}"))
}

fn http_status_error(response: &HttpResponse) -> AdapterError {
    match response.status().as_u16() {
        401 | 403 => AdapterError::Auth(format!("Gemini batch HTTP auth: {}", response.status())),
        // QA16 posture (mirrors `gemini_embedding::http_error`): capture a real
        // `Retry-After` when the provider sent one, never a fabricated value.
        429 => {
            let retry_after_ms = response
                .headers()
                .get("Retry-After")
                .and_then(|value| value.to_str().ok())
                .and_then(crate::http_policy::parse_retry_after_ms);
            AdapterError::RateLimit {
                message: format!("Gemini batch HTTP 429: {}", response.status()),
                retry_after_ms,
            }
        }
        402 => {
            AdapterError::QuotaExceeded(format!("Gemini batch HTTP quota: {}", response.status()))
        }
        status => {
            AdapterError::Network(format!("Gemini batch HTTP {status}: {}", response.status()))
        }
    }
}

// ---------------------------------------------------------------------------
// Hermetic mock (KIO_TEST_GEMINI_BATCH)
// ---------------------------------------------------------------------------

/// Inline mock script, mirroring [`crate::batch_client::MockBatchScript`]
/// minus the upload phase.
#[derive(Debug, Clone, Deserialize)]
#[cfg(debug_assertions)]
pub struct MockGeminiBatchScript {
    #[serde(default = "default_scope")]
    pub provider_scope_id: String,
    #[serde(default = "default_job")]
    pub job_name: String,
    /// One entry consumed per `get_job` call when `state_path` is set.
    #[serde(default = "default_sequence")]
    pub state_sequence: Vec<String>,
    /// Inline result lines returned verbatim by `fetch_inlined_results`.
    #[serde(default)]
    pub inlined_responses: Vec<Value>,
    /// Fail the NEXT phase after this one completes: "create_job" fails
    /// `get_job`, "get_job" fails `fetch_inlined_results` (crash-window
    /// emulation).
    #[serde(default)]
    pub fail_phase: Option<String>,
    /// G2: fail `get_job`/`fetch_inlined_results` for these job names ONLY.
    /// `fail_phase` is all-or-nothing, which cannot express the case the Batch
    /// poll lane has to survive — one unreachable row among several — so this
    /// selects by job name instead.
    #[serde(default)]
    pub fail_job_names: Vec<String>,
    #[serde(default)]
    pub jobs_listing: Vec<Value>,
    #[serde(default)]
    pub state_path: Option<String>,
    /// When set, every client call appends one JSON line here so contract
    /// tests can assert call order and payloads.
    #[serde(default)]
    pub capture_path: Option<String>,
    /// Explicit poll attribution override, including mismatches. If omitted,
    /// poll uses only attribution retained from a successful mock submission.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Shared provider state for separate CLI invocations. When omitted, a
    /// companion of state_path or capture_path is used when either is set.
    #[serde(default)]
    pub attribution_path: Option<String>,
}

#[cfg(debug_assertions)]
fn default_scope() -> String {
    "mock-gemini-project".to_owned()
}
#[cfg(debug_assertions)]
fn default_job() -> String {
    "batches/mock-embed-job-1".to_owned()
}
#[cfg(debug_assertions)]
fn default_sequence() -> Vec<String> {
    vec!["BATCH_STATE_SUCCEEDED".to_owned()]
}

#[cfg(debug_assertions)]
pub struct MockGeminiBatchClient {
    script: MockGeminiBatchScript,
    submitted_attribution: std::cell::RefCell<std::collections::BTreeMap<String, String>>,
}

#[cfg(debug_assertions)]
impl MockGeminiBatchClient {
    pub fn from_env_value(raw: &str) -> Result<Self> {
        let script: MockGeminiBatchScript = serde_json::from_str(raw).map_err(|error| {
            AdapterError::ConfigSchema(format!("{TEST_GEMINI_BATCH_ENV} script: {error}"))
        })?;
        Ok(Self {
            script,
            submitted_attribution: Default::default(),
        })
    }

    fn attribution_path(&self) -> Option<String> {
        self.script.attribution_path.clone().or_else(|| {
            self.script
                .state_path
                .as_ref()
                .or(self.script.capture_path.as_ref())
                .map(|path| format!("{path}.gemini-jobs.json"))
        })
    }

    fn attribution_key(&self) -> String {
        serde_json::to_string(&(&self.script.provider_scope_id, &self.script.job_name)).unwrap()
    }

    fn load_attribution(&self) -> Result<std::collections::BTreeMap<String, String>> {
        let Some(path) = self.attribution_path() else {
            return Ok(self.submitted_attribution.borrow().clone());
        };
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                AdapterError::ConfigSchema(format!("mock attribution state {path}: {error}"))
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
            Err(error) => Err(AdapterError::Io {
                path,
                message: error.to_string(),
            }),
        }
    }

    fn remember_attribution(&self, attribution: String) -> Result<()> {
        let mut records = self.load_attribution()?;
        records.insert(self.attribution_key(), attribution);
        if let Some(path) = self.attribution_path() {
            let bytes = serde_json::to_vec(&records).map_err(|error| {
                AdapterError::ConfigSchema(format!("mock attribution serialization: {error}"))
            })?;
            std::fs::write(&path, bytes).map_err(|error| AdapterError::Io {
                path,
                message: error.to_string(),
            })?;
        }
        *self.submitted_attribution.borrow_mut() = records;
        Ok(())
    }

    fn capture(&self, event: Value) {
        let Some(path) = self.script.capture_path.as_deref() else {
            return;
        };
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "{event}");
        }
    }

    fn fail_if_scripted(&self, phase: &str) -> Result<()> {
        if self.script.fail_phase.as_deref() == Some(phase) {
            return Err(AdapterError::Network(format!(
                "scripted {phase} failure (mock crash window)"
            )));
        }
        Ok(())
    }

    /// G2: one named job is unreachable while its siblings are fine.
    fn fail_if_named(&self, name: &str) -> Result<()> {
        if self.script.fail_job_names.iter().any(|job| job == name) {
            return Err(AdapterError::Network(format!(
                "scripted per-job failure for {name} (mock)"
            )));
        }
        Ok(())
    }

    /// Current poll step (0-based), advanced by one per call when a
    /// `state_path` is configured; single-entry sequences are stateless.
    fn poll_step(&self) -> usize {
        let Some(path) = self.script.state_path.as_deref() else {
            return self.script.state_sequence.len().saturating_sub(1);
        };
        let current: usize = std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| raw.trim().parse().ok())
            .unwrap_or(0);
        let next = current.saturating_add(1);
        let _ = std::fs::write(path, next.to_string());
        current.min(self.script.state_sequence.len().saturating_sub(1))
    }

    fn record(&self, state: &str) -> Result<GeminiBatchJobRecord> {
        Ok(GeminiBatchJobRecord {
            name: self.script.job_name.clone(),
            state: GeminiBatchState::parse(state),
            display_name: match &self.script.display_name {
                Some(display_name) => display_name.clone(),
                None => self
                    .load_attribution()?
                    .remove(&self.attribution_key())
                    .unwrap_or_default(),
            },
            responses_file: None,
        })
    }
}

#[cfg(debug_assertions)]
impl GeminiBatchClient for MockGeminiBatchClient {
    fn provider_scope_id(&self) -> Result<String> {
        Ok(self.script.provider_scope_id.clone())
    }

    fn create_embedding_job(
        &self,
        model: &str,
        dimensions: u32,
        display_name: &str,
        inputs: &[GeminiBatchEmbedInput],
    ) -> Result<GeminiBatchJobRecord> {
        // Validate the real body shape even in the mock, so a caller that
        // would exceed the inline bounds fails identically offline.
        inline_embed_batch_body(model, display_name, dimensions, inputs)?;
        self.capture(json!({
            "call": "create_embedding_job",
            "model": model,
            "dimensions": dimensions,
            "display_name": display_name,
            "keys": inputs.iter().map(|input| input.key.clone()).collect::<Vec<_>>(),
        }));
        self.fail_if_scripted("create_job")?;
        self.remember_attribution(display_name.to_owned())?;
        let mut record = self.record(
            self.script
                .state_sequence
                .first()
                .map_or("BATCH_STATE_PENDING", String::as_str),
        )?;
        record.display_name = display_name.to_owned();
        Ok(record)
    }

    fn poll_job(&self, name: &str) -> Result<GeminiBatchPoll> {
        self.capture(json!({ "call": "poll_job", "name": name }));
        self.fail_if_named(name)?;
        self.fail_if_scripted("poll_job")?;
        let step = self.poll_step();
        let state = self
            .script
            .state_sequence
            .get(step)
            .map_or("BATCH_STATE_SUCCEEDED", String::as_str);
        let record = self.record(state)?;
        // Mirror the wire: results appear only once the job is terminal, and
        // they arrive in the SAME response as the record.
        let results = if record.state.is_terminal() {
            // Route the scripted lines through the provider's actual envelope
            // rather than handing the parser its innermost shape — the drift
            // that made 13 contract tests pass over an uncollectable lane.
            Some(parse_inlined_results(&json!({
                "name": self.script.job_name,
                "metadata": {
                    "name": self.script.job_name,
                    "state": "BATCH_STATE_SUCCEEDED",
                    "output": {
                        "inlinedResponses": { "inlinedResponses": self.script.inlined_responses },
                    },
                },
                "done": true,
            }))?)
        } else {
            None
        };
        Ok(GeminiBatchPoll { record, results })
    }

    fn list_jobs(&self) -> Result<Vec<GeminiBatchJobRecord>> {
        self.capture(json!({ "call": "list_jobs" }));
        self.fail_if_scripted("list_jobs")?;
        // Route the scripted entries through the same envelope the provider
        // sends, rather than calling the entry parser directly — see the note
        // on `fetch_inlined_results` below.
        parse_job_listing(&json!({ "operations": self.script.jobs_listing }))
    }
}

/// The active embedding Batch client: the mock seam when
/// [`TEST_GEMINI_BATCH_ENV`] is set, otherwise the real client when an API key
/// is resolvable from declared `tools.toml [embedding] auth`.
/// `None` means the lane is unavailable and the caller must not send.
pub fn resolve_gemini_batch_client(
    recovery_context: impl FnOnce() -> Result<BatchRecoveryContext>,
) -> Result<Option<Box<dyn GeminiBatchClient>>> {
    #[cfg(debug_assertions)]
    {
        use kio_core::test_control::Selector;
        if let Selector::Known(raw) = crate::debug_test_control().adapters.gemini_batch {
            return Ok(Some(Box::new(MockGeminiBatchClient::from_env_value(&raw)?)));
        }
    }
    if let Some(credential) = crate::tool_lock::resolve_role_api_key("embedding")? {
        let context = recovery_context()?;
        return Ok(Some(Box::new(EnvGeminiBatchClient::from_captured(
            &context,
            GEMINI_API_ORIGIN,
            credential,
        )?)));
    }
    Ok(None)
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn credential_rotation_changes_scope_but_existing_instance_remains_pinned() {
        let _guard = crate::batch_client::test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _qualifier =
            kio_core::test_control::TestEnvGuard::set(GEMINI_PROJECT_ID_ENV, "same-qualifier");
        let _credential = kio_core::test_control::TestEnvGuard::set(
            "KIO_TEST_CAPTURED_BATCH_KEY",
            "credential-before",
        );
        let context = BatchRecoveryContext::from_key([9; 32]);
        let old = crate::batch_recovery::with_test_env_credential(
            "embedding",
            "KIO_TEST_CAPTURED_BATCH_KEY",
            || {
                let old = EnvGeminiBatchClient::new(&context).unwrap();
                let _rotated = kio_core::test_control::TestEnvGuard::set(
                    "KIO_TEST_CAPTURED_BATCH_KEY",
                    "credential-after",
                );
                let new = EnvGeminiBatchClient::new(&context).unwrap();
                assert_ne!(
                    old.provider_scope_id().unwrap(),
                    new.provider_scope_id().unwrap()
                );
                assert_eq!(old.identity.credential(), "credential-before");
                assert_eq!(new.identity.credential(), "credential-after");
                old
            },
        );
        // The source tools.toml is gone and no declaration is active now.
        assert_eq!(old.identity.credential(), "credential-before");
        let debug = format!("{old:?}");
        assert!(!debug.contains("credential-before"));
        assert!(!debug.contains(&old.provider_scope_id().unwrap()));
    }

    #[test]
    fn mock_poll_roundtrips_only_recorded_submission_display_name() {
        let dir = tempfile::tempdir().unwrap();
        let script = json!({ "attribution_path": dir.path().join("jobs.json") });
        let client = MockGeminiBatchClient::from_env_value(&script.to_string()).unwrap();
        assert!(
            client
                .poll_job(&default_job())
                .unwrap()
                .record
                .display_name
                .is_empty()
        );
        let inputs = [GeminiBatchEmbedInput {
            key: "input".into(),
            text: "text".into(),
        }];
        client
            .create_embedding_job("model", 768, "kio-submitted-intent", &inputs)
            .unwrap();
        assert_eq!(
            client.poll_job(&default_job()).unwrap().record.display_name,
            "kio-submitted-intent"
        );
        let reopened = MockGeminiBatchClient::from_env_value(&script.to_string()).unwrap();
        assert_eq!(
            reopened
                .poll_job(&default_job())
                .unwrap()
                .record
                .display_name,
            "kio-submitted-intent"
        );
        for field in ["provider_scope_id", "job_name"] {
            let mut other = script.clone();
            other[field] = json!("different");
            let client = MockGeminiBatchClient::from_env_value(&other.to_string()).unwrap();
            assert!(
                client
                    .poll_job("different")
                    .unwrap()
                    .record
                    .display_name
                    .is_empty()
            );
        }
        let mut mismatch = script;
        mismatch["display_name"] = json!("kio-wrong");
        let client = MockGeminiBatchClient::from_env_value(&mismatch.to_string()).unwrap();
        assert_eq!(
            client.poll_job(&default_job()).unwrap().record.display_name,
            "kio-wrong"
        );
        let memory_only = MockGeminiBatchClient::from_env_value("{}").unwrap();
        memory_only
            .create_embedding_job("model", 768, "kio-memory", &inputs)
            .unwrap();
        assert_eq!(
            memory_only
                .poll_job(&default_job())
                .unwrap()
                .record
                .display_name,
            "kio-memory"
        );
    }

    fn with_test_gemini_auth<T>(operation: impl FnOnce() -> T) -> T {
        crate::tool_lock::with_test_role_auth("embedding", operation)
    }

    fn http_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn spawn_listing_server(
        responses: Vec<String>,
    ) -> (
        String,
        std::thread::JoinHandle<Vec<String>>,
        crate::http_policy::test_tls::TrustGuard,
    ) {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let tls = crate::http_policy::test_tls::Fixture::new();
        let trust = tls.trust();
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let mut stream = tls.accept(&listener);
                let mut request = Vec::new();
                let mut chunk = [0_u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let count = stream.read(&mut chunk).unwrap();
                    assert!(count > 0, "Gemini listing request ended before headers");
                    request.extend_from_slice(&chunk[..count]);
                    assert!(
                        request.len() <= 64 * 1024,
                        "Gemini listing request headers too large"
                    );
                }
                stream.write_all(response.as_bytes()).unwrap();
                requests.push(String::from_utf8_lossy(&request).into_owned());
            }
            requests
        });
        (format!("https://{address}"), handle, trust)
    }

    #[test]
    fn state_parses_batch_and_job_prefixes_and_marks_terminals() {
        assert_eq!(
            GeminiBatchState::parse("BATCH_STATE_RUNNING"),
            GeminiBatchState::Running
        );
        assert_eq!(
            GeminiBatchState::parse("JOB_STATE_SUCCEEDED"),
            GeminiBatchState::Succeeded
        );
        assert!(!GeminiBatchState::parse("BATCH_STATE_PENDING").is_terminal());
        assert!(GeminiBatchState::parse("BATCH_STATE_EXPIRED").is_terminal());
        assert!(matches!(
            GeminiBatchState::parse("SOMETHING_NEW"),
            GeminiBatchState::Other(_)
        ));
        // An unknown state is NOT terminal: the poll lane keeps the row alive
        // rather than settling a job it does not understand.
        assert!(!GeminiBatchState::parse("SOMETHING_NEW").is_terminal());
    }

    #[test]
    fn display_name_round_trips_the_intent_token() {
        let name = batch_display_name("01KY-token");
        assert_eq!(name, "kio-01KY-token");
        assert_eq!(display_name_intent_token(&name), Some("01KY-token"));
        assert_eq!(display_name_intent_token("kio-"), None);
        assert_eq!(display_name_intent_token("someone-elses-job"), None);
    }

    #[test]
    fn inline_body_carries_model_dimensions_and_per_request_keys() {
        let inputs = vec![
            GeminiBatchEmbedInput {
                key: "chunk-a".to_owned(),
                text: "hello".to_owned(),
            },
            GeminiBatchEmbedInput {
                key: "chunk-b".to_owned(),
                text: "世界".to_owned(),
            },
        ];
        let body = inline_embed_batch_body("gemini-embedding-2", "kio-tok", 768, &inputs).unwrap();
        assert_eq!(body["batch"]["displayName"], "kio-tok");
        let requests = body["batch"]["inputConfig"]["requests"]["requests"]
            .as_array()
            .unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["request"]["model"], "models/gemini-embedding-2");
        assert_eq!(requests[0]["request"]["outputDimensionality"], 768);
        assert_eq!(
            requests[0]["request"]["content"]["parts"][0]["text"],
            "hello"
        );
        assert_eq!(requests[0]["metadata"]["key"], "chunk-a");
        assert_eq!(requests[1]["metadata"]["key"], "chunk-b");
    }

    #[test]
    fn inline_body_rejects_empty_and_oversized_request_sets() {
        assert!(inline_embed_batch_body("gemini-embedding-2", "kio-tok", 768, &[]).is_err());
        let too_many = (0..MAX_INLINE_REQUESTS + 1)
            .map(|index| GeminiBatchEmbedInput {
                key: format!("chunk-{index}"),
                text: "x".to_owned(),
            })
            .collect::<Vec<_>>();
        assert!(
            inline_embed_batch_body("gemini-embedding-2", "kio-tok", 768, &too_many).is_err(),
            "request-count bound must be enforced locally, not by the provider"
        );
    }

    #[test]
    fn inline_body_rejects_a_payload_over_the_byte_bound() {
        // One request whose text alone exceeds the inline byte bound.
        let huge = GeminiBatchEmbedInput {
            key: "chunk-huge".to_owned(),
            text: "x".repeat(MAX_INLINE_REQUEST_BYTES + 1),
        };
        let error = inline_embed_batch_body("gemini-embedding-2", "kio-tok", 768, &[huge])
            .expect_err("oversized inline payload must be rejected before the round trip");
        assert!(matches!(error, AdapterError::ContractViolation(_)));
    }

    #[test]
    fn qualified_model_is_idempotent() {
        assert_eq!(
            qualified_model("gemini-embedding-2"),
            "models/gemini-embedding-2"
        );
        assert_eq!(
            qualified_model("models/gemini-embedding-2"),
            "models/gemini-embedding-2"
        );
    }

    #[test]
    fn job_record_parses_top_level_and_lro_envelopes() {
        let flat = json!({
            "name": "batches/abc",
            "state": "BATCH_STATE_RUNNING",
            "displayName": "kio-tok",
        });
        let record = parse_job_record(&flat).unwrap();
        assert_eq!(record.name, "batches/abc");
        assert_eq!(record.state, GeminiBatchState::Running);
        assert_eq!(record.display_name, "kio-tok");
        assert!(record.responses_file.is_none());

        let wrapped = json!({
            "name": "operations/xyz",
            "metadata": {
                "name": "batches/abc",
                "state": "BATCH_STATE_PENDING",
                "displayName": "kio-tok",
            },
        });
        let record = parse_job_record(&wrapped).unwrap();
        assert_eq!(record.name, "batches/abc");
        assert_eq!(record.state, GeminiBatchState::Pending);
    }

    #[test]
    fn job_record_surfaces_a_file_output_instead_of_treating_it_as_empty() {
        let value = json!({
            "name": "batches/abc",
            "state": "BATCH_STATE_SUCCEEDED",
            "dest": { "responsesFile": "files/out-1" },
        });
        let record = parse_job_record(&value).unwrap();
        assert_eq!(record.responses_file.as_deref(), Some("files/out-1"));
    }

    #[test]
    fn job_record_rejects_a_response_without_name_or_state() {
        assert!(parse_job_record(&json!({ "state": "BATCH_STATE_RUNNING" })).is_err());
        assert!(parse_job_record(&json!({ "name": "batches/abc" })).is_err());
    }

    #[test]
    fn inlined_results_key_back_to_the_request_and_carry_errors_separately() {
        let value = json!({
            "dest": {
                "inlinedResponses": [
                    {
                        "metadata": { "key": "chunk-a" },
                        "output": { "response": { "embedding": { "values": [0.5, -0.25] } } },
                    },
                    {
                        "metadata": { "key": "chunk-b" },
                        "output": { "error": { "code": 400, "message": "bad" } },
                    },
                ]
            }
        });
        let outputs = parse_inlined_results(&value).unwrap();
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].key, "chunk-a");
        assert_eq!(outputs[0].values.as_deref(), Some(&[0.5f32, -0.25f32][..]));
        assert!(outputs[0].error.is_none());
        assert_eq!(outputs[1].key, "chunk-b");
        assert!(outputs[1].values.is_none());
        assert_eq!(outputs[1].error.as_ref().unwrap()["code"], 400);
    }

    #[test]
    fn a_result_line_without_a_key_is_a_contract_violation_not_a_silent_drop() {
        let value = json!({
            "inlinedResponses": [
                { "output": { "response": { "embedding": { "values": [1.0] } } } }
            ]
        });
        let error = parse_inlined_results(&value)
            .expect_err("an unkeyed line would strand a chunk unembedded");
        assert!(matches!(error, AdapterError::ContractViolation(_)));
    }

    #[test]
    fn a_result_line_with_non_numeric_values_is_rejected() {
        let value = json!({
            "inlinedResponses": [
                {
                    "metadata": { "key": "chunk-a" },
                    "output": { "response": { "embedding": { "values": ["nope"] } } },
                }
            ]
        });
        assert!(parse_inlined_results(&value).is_err());
    }

    #[test]
    fn missing_inlined_responses_is_an_error_not_an_empty_result() {
        let value = json!({ "name": "batches/abc", "state": "BATCH_STATE_SUCCEEDED" });
        assert!(parse_inlined_results(&value).is_err());
    }

    // -----------------------------------------------------------------------
    // Wire shapes captured from the live provider on 2026-07-25.
    //
    // These are verbatim responses (embedding vectors truncated to 3 elements,
    // one entry kept per page) from a real `gemini-embedding-2` batch. They
    // exist because the mock seam had been feeding the parser hand-built shapes
    // the API never sends: `{"inlinedResponses": [...]}` for results and
    // `{"batches": [...]}` for the listing. Every contract test passed, and no
    // real job could be collected or recovered. Assert against the wire.
    // -----------------------------------------------------------------------

    /// `GET /v1beta/batches/{id}` for a succeeded embedding batch.
    const REAL_POLL_RESPONSE: &str = r#"{
      "name": "batches/pq1j6cr5x7usrrlj98tlukhnsav7gcq5lt8x",
      "metadata": {
        "@type": "type.googleapis.com/google.ai.generativelanguage.v1main.EmbedContentBatch",
        "model": "models/gemini-embedding-2",
        "displayName": "kio-019f96a0-c9e2-7687-b146-f586fe4930b8",
        "output": {
          "inlinedResponses": {
            "inlinedResponses": [
              {
                "response": {
                  "embedding": { "values": [-0.01657847, 0.020118287, 0.026177283] },
                  "usageMetadata": { "promptTokenCount": 81 }
                },
                "metadata": { "key": "sha256:aaa" }
              },
              {
                "response": {
                  "embedding": { "values": [0.0013260875, 0.038272932, 0.03331437] },
                  "usageMetadata": { "promptTokenCount": 25 }
                },
                "metadata": { "key": "sha256:bbb" }
              }
            ]
          }
        },
        "createTime": "2026-07-25T00:15:48.588771827Z",
        "endTime": "2026-07-25T00:17:11.340704784Z",
        "batchStats": { "requestCount": "5", "successfulRequestCount": "5" },
        "state": "BATCH_STATE_SUCCEEDED",
        "name": "batches/pq1j6cr5x7usrrlj98tlukhnsav7gcq5lt8x"
      },
      "done": true,
      "response": {
        "@type": "type.googleapis.com/google.ai.generativelanguage.v1main.EmbedContentBatch",
        "inlinedResponses": {
          "inlinedResponses": [
            {
              "response": {
                "embedding": { "values": [-0.01657847, 0.020118287, 0.026177283] },
                "usageMetadata": { "promptTokenCount": 81 }
              },
              "metadata": { "key": "sha256:aaa" }
            },
            {
              "response": {
                "embedding": { "values": [0.0013260875, 0.038272932, 0.03331437] },
                "usageMetadata": { "promptTokenCount": 25 }
              },
              "metadata": { "key": "sha256:bbb" }
            }
          ]
        }
      }
    }"#;

    /// `GET /v1beta/batches?pageSize=N`.
    const REAL_LIST_RESPONSE: &str = r#"{
      "operations": [
        {
          "name": "batches/o828in12yctmkpzraz93m0a2p6nqerf2fpre",
          "metadata": {
            "@type": "type.googleapis.com/google.ai.generativelanguage.v1main.EmbedContentBatch",
            "model": "models/gemini-embedding-2",
            "displayName": "kio-019f96a5-b5a5-7cb2-ba6e-8ddfccafc483",
            "createTime": "2026-07-25T00:21:11.123599637Z",
            "endTime": "2026-07-25T00:22:44.923079258Z",
            "batchStats": { "requestCount": "6", "pendingRequestCount": "6" },
            "state": "BATCH_STATE_SUCCEEDED",
            "name": "batches/o828in12yctmkpzraz93m0a2p6nqerf2fpre"
          },
          "done": true
        }
      ]
    }"#;

    #[test]
    fn real_poll_response_yields_the_embedding_vectors() {
        let value: Value = serde_json::from_str(REAL_POLL_RESPONSE).unwrap();
        let outputs = parse_inlined_results(&value)
            .expect("the live poll shape must resolve, or no job is ever collectable");
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].key, "sha256:aaa");
        assert_eq!(outputs[0].values.as_ref().unwrap().len(), 3);
        assert_eq!(outputs[1].key, "sha256:bbb");
        assert!(outputs.iter().all(|output| output.error.is_none()));
    }

    #[test]
    fn real_poll_response_carries_the_billed_token_counts() {
        // I4: the provider DOES report per-line `promptTokenCount`. Dropping it
        // made every embedding charge settle at the reservation estimate
        // instead of at what was billed.
        let value: Value = serde_json::from_str(REAL_POLL_RESPONSE).unwrap();
        let outputs = parse_inlined_results(&value).unwrap();
        assert_eq!(outputs[0].prompt_tokens, Some(81));
        assert_eq!(outputs[1].prompt_tokens, Some(25));
    }

    #[test]
    fn a_line_without_a_usage_report_yields_no_token_count() {
        // Absence must stay `None` (which routes billing to the conservative
        // estimate), never a fabricated zero — a zero would settle the charge
        // at $0 and let the budget cap release spending already incurred.
        let value = json!({
            "inlinedResponses": [{
                "metadata": { "key": "chunk-a" },
                "output": { "response": { "embedding": { "values": [1.0] } } },
            }]
        });
        let outputs = parse_inlined_results(&value).unwrap();
        assert_eq!(outputs[0].prompt_tokens, None);
    }

    #[test]
    fn real_poll_response_yields_a_terminal_job_record() {
        let value: Value = serde_json::from_str(REAL_POLL_RESPONSE).unwrap();
        let record = parse_job_record(&value).unwrap();
        assert_eq!(record.name, "batches/pq1j6cr5x7usrrlj98tlukhnsav7gcq5lt8x");
        assert_eq!(record.state, GeminiBatchState::Succeeded);
        assert!(record.state.is_terminal());
        // The recovery walk keys off this, so an empty display name would make
        // every real job unattributable.
        assert_eq!(
            display_name_intent_token(&record.display_name),
            Some("019f96a0-c9e2-7687-b146-f586fe4930b8")
        );
    }

    #[test]
    fn real_listing_response_yields_the_page_entries() {
        let value: Value = serde_json::from_str(REAL_LIST_RESPONSE).unwrap();
        let records = parse_job_listing(&value)
            .expect("the live listing shape must resolve, or recovery never finds a job");
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].name,
            "batches/o828in12yctmkpzraz93m0a2p6nqerf2fpre"
        );
        assert_eq!(records[0].state, GeminiBatchState::Succeeded);
        assert_eq!(
            display_name_intent_token(&records[0].display_name),
            Some("019f96a5-b5a5-7cb2-ba6e-8ddfccafc483")
        );
    }

    #[test]
    fn real_listing_continues_after_an_empty_page_and_encodes_the_next_token() {
        let _guard = crate::batch_client::test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (base, server, _trust) = spawn_listing_server(vec![
            http_response(r#"{"operations":[],"nextPageToken":"cursor /?&+%"}"#),
            http_response(
                r#"{"operations":[{"name":"batches/after-empty","state":"BATCH_STATE_RUNNING"}]}"#,
            ),
        ]);
        let client = EnvGeminiBatchClient::with_base_url(base);

        let jobs = with_test_gemini_auth(|| client.list_jobs()).unwrap();

        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "batches/after-empty");
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].starts_with("GET /v1beta/batches?pageSize=100 "));
        assert!(
            requests[1].starts_with(
                "GET /v1beta/batches?pageSize=100&pageToken=cursor%20%2F%3F%26%2B%25 "
            )
        );
    }

    #[test]
    fn real_listing_rejects_a_next_page_token_cycle() {
        let _guard = crate::batch_client::test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (base, server, _trust) = spawn_listing_server(vec![
            http_response(r#"{"operations":[],"nextPageToken":"again"}"#),
            http_response(r#"{"operations":[],"nextPageToken":"again"}"#),
        ]);
        let client = EnvGeminiBatchClient::with_base_url(base);

        let error = with_test_gemini_auth(|| client.list_jobs()).unwrap_err();
        assert!(matches!(error, AdapterError::ContractViolation(_)));
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[test]
    fn real_listing_rejects_malformed_tokens_and_oversized_pages() {
        let _guard = crate::batch_client::test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());

        let (base, server, _trust) = spawn_listing_server(vec![http_response(
            r#"{"operations":[],"nextPageToken":17}"#,
        )]);
        let client = EnvGeminiBatchClient::with_base_url(base);
        let error = with_test_gemini_auth(|| client.list_jobs()).unwrap_err();
        assert!(matches!(error, AdapterError::ContractViolation(_)));
        assert_eq!(server.join().unwrap().len(), 1);

        let operations = (0..BATCH_LIST_PAGE_SIZE + 1)
            .map(|index| {
                json!({
                    "name": format!("batches/oversized-{index}"),
                    "state": "BATCH_STATE_RUNNING",
                })
            })
            .collect::<Vec<_>>();
        let (base, server, _trust) = spawn_listing_server(vec![http_response(
            &json!({ "operations": operations }).to_string(),
        )]);
        let client = EnvGeminiBatchClient::with_base_url(base);
        let error = with_test_gemini_auth(|| client.list_jobs()).unwrap_err();
        assert!(matches!(error, AdapterError::ContractViolation(_)));
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn real_listing_rejects_an_incomplete_walk_at_the_page_cap() {
        let _guard = crate::batch_client::test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let responses = (0..BATCH_LIST_MAX_PAGES)
            .map(|page| {
                http_response(&format!(
                    r#"{{"operations":[],"nextPageToken":"cursor-{page}"}}"#
                ))
            })
            .collect();
        let (base, server, _trust) = spawn_listing_server(responses);
        let client = EnvGeminiBatchClient::with_base_url(base);

        let error = with_test_gemini_auth(|| client.list_jobs()).unwrap_err();
        assert!(matches!(error, AdapterError::ContractViolation(_)));
        assert_eq!(server.join().unwrap().len(), BATCH_LIST_MAX_PAGES);
    }

    #[test]
    fn the_documented_listing_key_is_still_accepted() {
        let value = json!({
            "batches": [{ "name": "batches/abc", "state": "BATCH_STATE_RUNNING" }]
        });
        let records = parse_job_listing(&value).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "batches/abc");
        assert!(parse_job_listing(&json!({})).unwrap().is_empty());
    }

    #[test]
    fn mock_client_progresses_through_the_scripted_states() {
        let dir =
            std::env::temp_dir().join(format!("kio-gemini-batch-mock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state_path = dir.join("poll-state");
        let script = json!({
            "state_sequence": ["BATCH_STATE_PENDING", "BATCH_STATE_RUNNING", "BATCH_STATE_SUCCEEDED"],
            "state_path": state_path.to_string_lossy(),
            "inlined_responses": [
                {
                    "metadata": { "key": "chunk-a" },
                    "output": { "response": { "embedding": { "values": [0.1, 0.2] } } },
                }
            ],
        })
        .to_string();
        let client = MockGeminiBatchClient::from_env_value(&script).unwrap();
        let created = client
            .create_embedding_job(
                "gemini-embedding-2",
                768,
                "kio-tok",
                &[GeminiBatchEmbedInput {
                    key: "chunk-a".to_owned(),
                    text: "hello".to_owned(),
                }],
            )
            .unwrap();
        assert_eq!(created.state, GeminiBatchState::Pending);
        assert_eq!(created.display_name, "kio-tok");
        let pending = client.poll_job(&created.name).unwrap();
        assert_eq!(pending.record.state, GeminiBatchState::Pending);
        // A job that has not finished carries no results, and that absence is
        // normal rather than a read failure.
        assert!(pending.results.is_none());

        let running = client.poll_job(&created.name).unwrap();
        assert_eq!(running.record.state, GeminiBatchState::Running);
        assert!(running.results.is_none());

        // I8: the terminal poll delivers the record AND the results in ONE
        // call, because the provider delivers them in one document.
        let done = client.poll_job(&created.name).unwrap();
        assert_eq!(done.record.state, GeminiBatchState::Succeeded);
        let outputs = done.results.expect("a succeeded job carries its results");
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].key, "chunk-a");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mock_client_emulates_a_scripted_phase_failure() {
        let script = json!({ "fail_phase": "create_job" }).to_string();
        let client = MockGeminiBatchClient::from_env_value(&script).unwrap();
        let error = client
            .create_embedding_job(
                "gemini-embedding-2",
                768,
                "kio-tok",
                &[GeminiBatchEmbedInput {
                    key: "chunk-a".to_owned(),
                    text: "hello".to_owned(),
                }],
            )
            .expect_err("scripted create_job failure");
        assert!(matches!(error, AdapterError::Network(_)));
    }

    #[test]
    fn mock_client_enforces_the_same_inline_bounds_as_the_real_one() {
        let client = MockGeminiBatchClient::from_env_value("{}").unwrap();
        assert!(
            client
                .create_embedding_job("gemini-embedding-2", 768, "kio-tok", &[])
                .is_err()
        );
    }
}
