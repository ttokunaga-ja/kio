//! Gemini multimodal Embedding Adapter (07 §5.3 / §6).
//!
//! Mirrors the Mistral OCR adapter structure: a `GeminiEmbeddingClient` trait
//! (network boundary) with an `Env`-backed HTTP implementation and a generic
//! `GeminiEmbeddingAdapter<C>` that implements [`EmbeddingAdapter`]. The adopted
//! profile is the single multimodal profile fixed on 2026-07-03: `gemini-embedding-2`
//! (GA, pinned at startup) / 768 dims (MRL) / cosine / `modality="multimodal"`.
//!
//! The live HTTP path is not exercised in the hermetic test suite (`docs/07-adapter-spec.md` §5.3);
//! the wire format below is documentation-accurate best effort and the contract is
//! covered by the CLI mock seam. `profile()` never performs network I/O.

use crate::http_policy::{
    EMBEDDING_RESPONSE_MAX_BYTES, HttpPolicy, HttpResponse, MODEL_CATALOG_MAX_BYTES,
    authenticated_agent, read_json_bounded, require_success,
};
use crate::identity::{is_mutable_model_alias, tool_profile_hash};
use crate::traits::{EmbeddingAdapter, PreferredRequestKind};
use crate::types::{
    AdapterKind, AdapterProfile, AdapterUsage, BillableUnit, BillableUnitKind, EmbeddingContent,
    EmbeddingItem, EmbeddingRequest, EmbeddingResponse, EmbeddingVector, ExecutionMode,
    validate_cosine_vector, validate_embedding_request_bytes,
};
use crate::{AdapterError, Result};
use serde_json::{Value, json};

/// Adopted embedding profile constants. Non-adapter crates access this profile
/// through `crate::catalog`, not by naming this adapter directly.
pub const ADOPTED_MODEL_FAMILY: &str = "gemini-embedding";
pub const ADOPTED_MODEL_PIN: &str = "gemini-embedding-2";
pub const ADOPTED_DIMENSIONS: u32 = 768;
const GEMINI_API_ORIGIN: &str = "https://generativelanguage.googleapis.com";

/// Network boundary for the Gemini embedding backend.
pub trait GeminiEmbeddingClient: Clone {
    /// Resolve a possibly-mutable configured model alias to an immutable GA
    /// version at startup (07 §6: mutable aliases must never be pinned).
    fn resolve_model_pin(&self, configured_model: &str) -> Result<String>;

    /// Embed the batch of items, returning one vector per item (order
    /// preserved). QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880):
    /// `idempotency_header` is the ADAPTER-resolved `(header name, token)`
    /// pair (see `crate::http_policy::resolve_idempotency_header`) to attach
    /// to the outgoing HTTP request when `Some` — `None` when the profile
    /// declares `ProviderIdempotency::NotProvided` (the real built-in
    /// adapter's permanent posture; see [`GeminiEmbeddingAdapter::profile`]).
    fn embed(
        &self,
        items: &[EmbeddingItem],
        model_pin: &str,
        dimensions: u32,
        idempotency_header: Option<(&str, &str)>,
    ) -> Result<EmbedBatchOutput>;
}

/// One `:batchEmbedContents` response: the vectors, plus the token count the
/// provider reported for the call.
#[derive(Debug, Clone, Default)]
pub struct EmbedBatchOutput {
    pub vectors: Vec<EmbeddingVector>,
    /// `usageMetadata.promptTokenCount` — a total for the whole call, which is
    /// also the granularity the provider bills at.
    pub prompt_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct EnvGeminiEmbeddingClient {
    base_url: Option<String>,
    http_policy: HttpPolicy,
}

impl EnvGeminiEmbeddingClient {
    #[must_use]
    pub fn new() -> Self {
        Self {
            base_url: None,
            http_policy: HttpPolicy::default(),
        }
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: Some(base_url.into()),
            http_policy: HttpPolicy::default(),
        }
    }

    fn base_url(&self) -> String {
        self.base_url
            .clone()
            .unwrap_or_else(|| GEMINI_API_ORIGIN.to_owned())
            .trim_end_matches('/')
            .to_owned()
    }

    fn api_key() -> Result<String> {
        crate::tool_lock::resolve_role_api_key("embedding")?.ok_or_else(|| {
            AdapterError::Auth(
                "no Gemini embedding API key: declare tools.toml `[embedding] auth`".to_owned(),
            )
        })
    }
}

impl GeminiEmbeddingClient for EnvGeminiEmbeddingClient {
    fn resolve_model_pin(&self, configured_model: &str) -> Result<String> {
        if !is_mutable_model_alias(configured_model) {
            return Ok(configured_model.to_owned());
        }
        let api_key = Self::api_key()?;
        let response = authenticated_agent(self.http_policy)
            .get(&format!("{}/v1beta/models", self.base_url()))
            .header("x-goog-api-key", &api_key)
            .header("Accept-Encoding", "identity")
            .call()
            .map_err(http_error)
            .and_then(|response| require_success(response, http_status_error))?;
        let value = read_json_bounded(
            response,
            MODEL_CATALOG_MAX_BYTES,
            "Gemini model catalog response",
        )?;
        // Model ids arrive as "models/<family>-NNN"; pick the max stable version
        // of the configured family, rejecting any remaining "-latest" alias.
        let family = configured_model
            .trim_end_matches("-latest")
            .rsplit('/')
            .next()
            .unwrap_or(configured_model);
        let models = value
            .get("models")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                AdapterError::ContractViolation("Gemini model catalog missing models".to_owned())
            })?;
        if models.len() > 10_000 {
            return Err(AdapterError::ContractViolation(
                "Gemini model catalog has too many entries".to_owned(),
            ));
        }
        if models.iter().any(|model| {
            model
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| name.len() > 512)
        }) {
            return Err(AdapterError::ContractViolation(
                "Gemini model identifier exceeds 512 bytes".to_owned(),
            ));
        }
        models
            .iter()
            .filter_map(|model| model.get("name").and_then(Value::as_str))
            .map(|name| name.rsplit('/').next().unwrap_or(name))
            .filter(|id| id.starts_with(family) && !id.ends_with("-latest"))
            .max()
            .map(str::to_owned)
            .ok_or_else(|| {
                AdapterError::ContractViolation(format!(
                    "no versioned model found for {configured_model}"
                ))
            })
    }

    fn embed(
        &self,
        items: &[EmbeddingItem],
        model_pin: &str,
        dimensions: u32,
        idempotency_header: Option<(&str, &str)>,
    ) -> Result<EmbedBatchOutput> {
        let api_key = Self::api_key()?;
        // `:batchEmbedContents` embeds the whole batch in ONE SYNCHRONOUS
        // request — the batching here is client-side, and this is the Sync lane
        // even though the endpoint name contains "batch". The provider's actual
        // Batch lane (half price, async) is `:asyncBatchEmbedContent`, driven by
        // `crate::gemini_batch_client` — see `preferred_request_kind` and the
        // 2026-07-24 correction in 07 §5.3. (The earlier comment here claimed
        // Vertex has no batch inference; this adapter does not call Vertex.)
        let requests = items
            .iter()
            .map(|item| {
                let EmbeddingContent::Text { text } = &item.content else {
                    return Err(AdapterError::ContractViolation(
                        "Gemini embedding adapter does not accept image content".to_owned(),
                    ));
                };
                Ok(json!({
                    "model": format!("models/{model_pin}"),
                    "content": { "parts": [{ "text": text }] },
                    "outputDimensionality": dimensions,
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut http_request = authenticated_agent(self.http_policy)
            .post(&format!(
                "{}/v1beta/models/{model_pin}:batchEmbedContents",
                self.base_url()
            ))
            .header("x-goog-api-key", &api_key)
            .header("Content-Type", "application/json")
            .header("Accept-Encoding", "identity");
        // QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): attach the
        // provider idempotency header only when the profile resolved one —
        // dormant in production (the real Gemini embedding profile always
        // declares `ProviderIdempotency::NotProvided`), reachable via a test
        // profile.
        if let Some((name, value)) = idempotency_header {
            http_request = http_request.header(name, value);
        }
        let response = http_request
            .send_json(json!({ "requests": requests }))
            .map_err(http_error)
            .and_then(|response| require_success(response, http_status_error))?;
        let response = read_json_bounded(
            response,
            EMBEDDING_RESPONSE_MAX_BYTES,
            "Gemini embedding response",
        )?;
        Ok(EmbedBatchOutput {
            vectors: parse_embeddings(&response, items, dimensions)?,
            prompt_tokens: parse_prompt_tokens(&response),
        })
    }
}

/// `usageMetadata.promptTokenCount`, when the response carried one.
///
/// The endpoint reports a total for the CALL rather than per request, which is
/// what the previous comment here observed — but it then concluded there was
/// "no real signal to self-report", and a per-call total is exactly the
/// granularity the provider bills at. Reporting `None` made every synchronous
/// embedding settle at the caller's reservation estimate instead of at cost.
fn parse_prompt_tokens(response: &Value) -> Option<u64> {
    let value = response.get("usageMetadata")?.get("promptTokenCount")?;
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}

fn parse_embeddings(
    response: &Value,
    items: &[EmbeddingItem],
    dimensions: u32,
) -> Result<Vec<EmbeddingVector>> {
    let embeddings = response
        .get("embeddings")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            AdapterError::ContractViolation("embedding response missing embeddings".to_owned())
        })?;
    if embeddings.len() != items.len() {
        return Err(AdapterError::ContractViolation(
            "embedding response count does not match request".to_owned(),
        ));
    }
    // QA47 (step4b-contract-tests-p3a.md §N, arbitration #5): Gemini's
    // `batchEmbedContents` carries no per-item id, so each output id is
    // synthesized positionally from the matching request item below. That
    // positional synthesis is only a true bijection over the input id set
    // when the input ids are themselves unique — otherwise two distinct
    // request items collapse onto one id and the "bijection" is vacuous.
    // Reject the duplicate up front rather than silently accept it.
    let unique_ids: std::collections::BTreeSet<&str> =
        items.iter().map(|item| item.id.as_str()).collect();
    if unique_ids.len() != items.len() {
        return Err(AdapterError::ContractViolation(
            "embedding request ids are not unique; a positionally-synthesized \
             response id cannot form a bijection over a duplicated input id set"
                .to_owned(),
        ));
    }
    items
        .iter()
        .zip(embeddings)
        .map(|(item, embedding)| {
            let values = embedding
                .get("values")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    AdapterError::ContractViolation("embedding missing values".to_owned())
                })?;
            if values.len() != dimensions as usize {
                return Err(AdapterError::ContractViolation(format!(
                    "embedding dimension mismatch: expected {dimensions}, got {}",
                    values.len()
                )));
            }
            let vector = values
                .iter()
                .map(|value| value.as_f64().map(|value| value as f32))
                .collect::<Option<Vec<f32>>>()
                .ok_or_else(|| {
                    AdapterError::ContractViolation("embedding values must be numeric".to_owned())
                })?;
            // F7: the response vector must have exactly the requested
            // `outputDimensionality`. A wrong-width vector would otherwise be
            // persisted with the declared `dimensions` (768) but a mismatched
            // byte length, so `link_chunk_vec` silently drops it from `chunk_vec`
            // (permanent KNN exclusion) even though the chunk is billed and marked
            // done, with no self-repair path. Reject it as a contract violation so
            // the batch fails (retryable/reportable) instead of being charged.
            validate_cosine_vector(&vector, dimensions)?;
            Ok(EmbeddingVector {
                id: item.id.clone(),
                vector,
            })
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct GeminiEmbeddingAdapter<C = EnvGeminiEmbeddingClient> {
    client: C,
    configured_model: String,
    dimensions: u32,
    /// QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): defaults to
    /// `NotProvided` (the real, shipped Gemini `:batchEmbedContents`
    /// endpoint offers no provider idempotency key) — see
    /// [`Self::with_provider_idempotency`].
    provider_idempotency: crate::types::ProviderIdempotency,
}

impl Default for GeminiEmbeddingAdapter<EnvGeminiEmbeddingClient> {
    fn default() -> Self {
        Self::new(
            EnvGeminiEmbeddingClient::new(),
            ADOPTED_MODEL_PIN,
            ADOPTED_DIMENSIONS,
        )
    }
}

impl<C> GeminiEmbeddingAdapter<C> {
    pub fn new(client: C, configured_model: impl Into<String>, dimensions: u32) -> Self {
        Self {
            client,
            configured_model: configured_model.into(),
            dimensions,
            provider_idempotency: crate::types::ProviderIdempotency::NotProvided,
        }
    }

    /// QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): declare the
    /// provider's sync-call idempotency posture — see
    /// [`crate::types::ProviderIdempotency`]. The real, shipped Gemini
    /// embedding adapter never calls this (it stays at the `::new()`
    /// default, `NotProvided`) — only a test-seam profile does.
    #[cfg(debug_assertions)]
    #[must_use]
    pub fn with_provider_idempotency(
        mut self,
        provider_idempotency: crate::types::ProviderIdempotency,
    ) -> Self {
        self.provider_idempotency = provider_idempotency;
        self
    }

    /// The adopted profile JSON value (07 §5.3).
    #[must_use]
    pub fn profile_value(&self) -> Value {
        // Network-free (like Mistral): if an unresolved mutable alias is still
        // held, use a deterministic placeholder rather than resolving over HTTP.
        let model_pin = if is_mutable_model_alias(&self.configured_model) {
            format!(
                "{}-unresolved",
                self.configured_model.trim_end_matches("-latest")
            )
        } else {
            self.configured_model.clone()
        };
        json!({
            "adapter_kind": "embedding",
            "adapter_role": "multimodal",
            "dimensions": self.dimensions,
            "distance": "cosine",
            // 2026-07-24 (07 §5.3 contextual-embedding addendum): Kio prepends a
            // chunk's humanized filename to the embedded text
            // (`embedding_store::contextualized_embedding_input`). That changes
            // what a chunk vector MEANS, so it is part of the vector-space
            // identity the profile hash pins (03 §7 compat gate): a store of
            // pre-addendum, non-contextual vectors must read as INCOMPATIBLE with
            // a contextual query profile, and bumping this field is what makes it
            // so (and re-triggers a full re-embed on the next online index).
            "input_construction": "chunk_filename_context_v1",
            "modality": "multimodal",
            "model_or_tool_family": ADOPTED_MODEL_FAMILY,
            "model_version_pin": model_pin,
            "runtime_kind": "cloud",
            "spec_version": 1
        })
    }
}

impl<C: GeminiEmbeddingClient> EmbeddingAdapter for GeminiEmbeddingAdapter<C> {
    fn profile(&self) -> AdapterProfile {
        let profile = self.profile_value();
        AdapterProfile {
            adapter_kind: AdapterKind::Embedding,
            adapter_id: "gemini_embedding_2".to_owned(),
            execution_mode: ExecutionMode::OnlineApi,
            tool_profile_hash: tool_profile_hash(&profile)
                .expect("built-in Gemini embedding profile is valid"),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            capability_flags: vec!["text".to_owned(), "multimodal".to_owned()],
            allow_network: true,
            // QA18: Gemini embedding bills per input token; there is no
            // output-token leg for an embedding response.
            billable_kinds: vec![crate::types::BillableUnitKind::TokensIn],
            reject_billing: Some(crate::types::BillingDeclaration::Billable),
            // QA13 (04 §5.5 L880): the real Gemini `:batchEmbedContents`
            // endpoint offers no idempotency parameter — "job 作成に
            // idempotency key の無い provider が現実" — so the shipped
            // adapter's `::new()` default, `NotProvided`, flows straight
            // through here. Only a test-seam adapter overrides it via
            // `with_provider_idempotency`.
            provider_idempotency: self.provider_idempotency.clone(),
        }
    }

    /// 07 §5.3 の 2026-07-24 訂正: the Gemini Developer API bills an embedding
    /// batch at half the sync rate, so this adapter's production sends prefer
    /// the Batch lane (`gemini_batch_client`, inline input / no upload phase).
    fn preferred_request_kind(&self) -> PreferredRequestKind {
        PreferredRequestKind::Batch
    }

    fn embed(&self, request: EmbeddingRequest) -> Result<EmbeddingResponse> {
        validate_embedding_request_bytes(&request)?;
        if request
            .items
            .iter()
            .any(|item| matches!(item.content, EmbeddingContent::Image { .. }))
        {
            return Err(AdapterError::ContractViolation(
                "Gemini embedding adapter does not accept image content".to_owned(),
            ));
        }
        // QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): resolve (and
        // fail closed on) the provider idempotency header BEFORE any network
        // call — a `HttpHeader`-declaring provider with no caller-supplied
        // token must never reach the model-pin lookup or the embed call.
        // `NotProvided` (the real, shipped adapter's permanent posture) never
        // inspects `request.idempotency_token` and never errors here.
        let idempotency_header = crate::http_policy::resolve_idempotency_header(
            &self.provider_idempotency,
            request.idempotency_token.as_deref(),
        )?;
        let model_pin = self.client.resolve_model_pin(&self.configured_model)?;
        let batch = self.client.embed(
            &request.items,
            &model_pin,
            self.dimensions,
            idempotency_header
                .as_ref()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        )?;
        let vectors = batch.vectors;
        if vectors.len() != request.items.len() {
            return Err(AdapterError::ContractViolation(
                "embedding response count does not match request".to_owned(),
            ));
        }
        for (item, vector) in request.items.iter().zip(&vectors) {
            if vector.id != item.id {
                return Err(AdapterError::ContractViolation(
                    "embedding response order or identity does not match request".to_owned(),
                ));
            }
            validate_cosine_vector(&vector.vector, self.dimensions)?;
        }
        Ok(EmbeddingResponse {
            vectors,
            dimensions: self.dimensions,
            distance: "cosine".to_owned(),
            modality: "multimodal".to_owned(),
            // QA49: the profile in force for this response, so the consumer
            // can reject a same-dimension vector from an unexpected profile.
            embedding_profile_hash: tool_profile_hash(&self.profile_value()).ok(),
            // I4 (2026-07-25, measured against the live endpoint): the
            // response DOES carry `usageMetadata.promptTokenCount` — as a
            // per-CALL total, not per request, which is the granularity the
            // provider bills at. This used to report `None` on the reasoning
            // that a per-request count was absent, which made every
            // synchronous embedding settle at the caller's reservation
            // estimate rather than at cost. `None` remains the honest answer
            // when the field is missing, and still degrades that way.
            usage: batch
                .prompt_tokens
                .map(|count| AdapterUsage::BillableUnits {
                    billable_units: vec![BillableUnit {
                        kind: BillableUnitKind::TokensIn,
                        count,
                    }],
                }),
        })
    }
}

fn http_error(error: ureq::Error) -> AdapterError {
    AdapterError::Network(error.to_string())
}

fn http_status_error(response: &HttpResponse) -> AdapterError {
    match response.status().as_u16() {
        401 | 403 => {
            AdapterError::Auth(format!("Gemini embedding HTTP auth: {}", response.status()))
        }
        // QA16: capture a real `Retry-After` header when the provider sent
        // one — never a fabricated value (`parse_retry_after_ms` returns
        // `None` for an absent/unparseable header, same as before this
        // field existed).
        429 => {
            let retry_after_ms = response
                .headers()
                .get("Retry-After")
                .and_then(|value| value.to_str().ok())
                .and_then(crate::http_policy::parse_retry_after_ms);
            AdapterError::RateLimit {
                message: format!("Gemini embedding HTTP 429: {}", response.status()),
                retry_after_ms,
            }
        }
        402 => AdapterError::QuotaExceeded(format!(
            "Gemini embedding HTTP quota: {}",
            response.status()
        )),
        code => AdapterError::Network(format!(
            "Gemini embedding HTTP {code}: {}",
            response.status()
        )),
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::types::{EmbeddingContent, EmbeddingInputType};

    #[derive(Clone)]
    struct StubClient;

    impl GeminiEmbeddingClient for StubClient {
        fn resolve_model_pin(&self, _configured_model: &str) -> Result<String> {
            Ok(ADOPTED_MODEL_PIN.to_owned())
        }

        fn embed(
            &self,
            items: &[EmbeddingItem],
            _model_pin: &str,
            dimensions: u32,
            _idempotency_header: Option<(&str, &str)>,
        ) -> Result<EmbedBatchOutput> {
            Ok(EmbedBatchOutput {
                vectors: items
                    .iter()
                    .map(|item| EmbeddingVector {
                        id: item.id.clone(),
                        vector: {
                            let mut vector = vec![0.0; dimensions as usize];
                            vector[0] = 1.0;
                            vector
                        },
                    })
                    .collect(),
                prompt_tokens: Some(7),
            })
        }
    }

    #[test]
    fn the_reported_token_count_becomes_a_billable_unit() {
        // I4: measured against the live `:batchEmbedContents` — the response
        // carries `usageMetadata.promptTokenCount` as a per-CALL total. The
        // adapter used to report `usage: None` on the reasoning that a
        // per-REQUEST count was absent, which is true and beside the point:
        // per-call is the granularity the provider bills at.
        let adapter =
            GeminiEmbeddingAdapter::new(StubClient, ADOPTED_MODEL_PIN, ADOPTED_DIMENSIONS);
        let response = adapter
            .embed(EmbeddingRequest {
                input_type: EmbeddingInputType::MarkdownChunk,
                items: vec![EmbeddingItem::text("a", "hello")],
                idempotency_token: None,
            })
            .expect("embed");
        match response.usage {
            Some(AdapterUsage::BillableUnits { billable_units }) => {
                assert_eq!(billable_units.len(), 1);
                assert_eq!(billable_units[0].kind, BillableUnitKind::TokensIn);
                assert_eq!(billable_units[0].count, 7);
            }
            other => panic!("expected a tokens_in report, got {other:?}"),
        }
    }

    #[test]
    fn a_response_without_usage_metadata_reports_nothing() {
        // `None` still degrades to the caller's reservation estimate, which is
        // the conservative direction; a fabricated 0 would not be.
        assert_eq!(parse_prompt_tokens(&json!({ "embeddings": [] })), None);
        assert_eq!(
            parse_prompt_tokens(&json!({ "usageMetadata": { "promptTokenCount": 42 } })),
            Some(42)
        );
        // The API quotes counts as strings in some fields; accept both rather
        // than dropping a real count.
        assert_eq!(
            parse_prompt_tokens(&json!({ "usageMetadata": { "promptTokenCount": "42" } })),
            Some(42)
        );
    }

    #[test]
    fn adopted_profile_hash_matches_frozen_vector() {
        let adapter =
            GeminiEmbeddingAdapter::new(StubClient, ADOPTED_MODEL_PIN, ADOPTED_DIMENSIONS);
        assert_eq!(
            adapter.profile().tool_profile_hash,
            // 2026-07-24: bumped from `66aff638…` by the contextual-embedding
            // addendum (07 §5.3) — the profile now declares
            // `input_construction=chunk_filename_context_v1`.
            "sha256:09ff078458a30dc1607a66f996cb5261cd16dc8d16c267c9a67245ca5fd66f90"
        );
        assert_eq!(adapter.profile().adapter_kind, AdapterKind::Embedding);
        assert!(adapter.profile().allow_network);
    }

    // QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880), test (d): the
    // real, shipped Gemini embedding adapter declares `NotProvided` — its
    // pinned `:batchEmbedContents` endpoint offers no provider idempotency
    // key.
    #[test]
    fn qa13_default_gemini_profile_declares_not_provided() {
        let adapter =
            GeminiEmbeddingAdapter::new(StubClient, ADOPTED_MODEL_PIN, ADOPTED_DIMENSIONS);
        assert_eq!(
            adapter.profile().provider_idempotency,
            crate::types::ProviderIdempotency::NotProvided
        );
    }

    // QA13: `embed()`'s generic idempotency gate — a `HttpHeader`-declaring
    // profile rejects a request with no token BEFORE the client is ever
    // reached (fail closed, no billed call), and accepts one once the caller
    // supplies a token.
    #[test]
    fn qa13_embed_enforces_provider_idempotency_header_requirement() {
        let adapter =
            GeminiEmbeddingAdapter::new(StubClient, ADOPTED_MODEL_PIN, ADOPTED_DIMENSIONS)
                .with_provider_idempotency(crate::types::ProviderIdempotency::HttpHeader(
                    "Idempotency-Key".to_owned(),
                ));
        let item = EmbeddingItem::text("a", "hello");
        let missing_token_request = EmbeddingRequest {
            input_type: EmbeddingInputType::MarkdownChunk,
            items: vec![item.clone()],
            idempotency_token: None,
        };
        let error = adapter.clone().embed(missing_token_request).unwrap_err();
        assert!(
            matches!(error, AdapterError::ContractViolation(_)),
            "expected ContractViolation, got {error:?}"
        );

        let with_token_request = EmbeddingRequest {
            input_type: EmbeddingInputType::MarkdownChunk,
            items: vec![item],
            idempotency_token: Some("intent-token-xyz".to_owned()),
        };
        let response = adapter.embed(with_token_request).unwrap();
        assert_eq!(response.vectors.len(), 1);
    }

    #[test]
    fn embed_returns_one_vector_per_item() {
        let adapter =
            GeminiEmbeddingAdapter::new(StubClient, ADOPTED_MODEL_PIN, ADOPTED_DIMENSIONS);
        let response = adapter
            .embed(EmbeddingRequest {
                input_type: EmbeddingInputType::MarkdownChunk,
                items: vec![
                    EmbeddingItem::text("a", "hello"),
                    EmbeddingItem::text("b", "world"),
                ],
                idempotency_token: None,
            })
            .unwrap();
        assert_eq!(response.vectors.len(), 2);
        assert_eq!(response.dimensions, 768);
        assert_eq!(response.distance, "cosine");
        assert_eq!(response.modality, "multimodal");
    }

    #[test]
    fn batch_embed_response_is_parsed_in_order() {
        let items = vec![EmbeddingItem::text("x", "a"), EmbeddingItem::text("y", "b")];
        let response = json!({
            "embeddings": [
                { "values": [1.0, 2.0] },
                { "values": [3.0, 4.0] }
            ]
        });
        let vectors = parse_embeddings(&response, &items, 2).unwrap();
        assert_eq!(vectors[0].id, "x");
        assert_eq!(vectors[0].vector, vec![1.0, 2.0]);
        assert_eq!(vectors[1].id, "y");
    }

    // F7: a response whose vector length disagrees with the requested dimension
    // must be rejected as a contract violation, not persisted (which would make
    // the chunk permanently invisible to KNN yet billed and marked done).
    #[test]
    fn embedding_wrong_dimension_is_contract_violation() {
        let items = vec![EmbeddingItem::text("x", "a")];
        // 768 requested, but the backend returns a 5-element vector.
        let response = json!({
            "embeddings": [
                { "values": [0.0, 0.1, 0.2, 0.3, 0.4] }
            ]
        });
        let err = parse_embeddings(&response, &items, ADOPTED_DIMENSIONS).unwrap_err();
        assert!(
            matches!(err, AdapterError::ContractViolation(_)),
            "expected ContractViolation, got {err:?}"
        );
    }

    #[test]
    fn embedding_numeric_domain_is_validated_after_f32_conversion() {
        let items = vec![EmbeddingItem::text("x", "a")];
        let over_range = json!({
            "embeddings": [{ "values": [3.5e38, 1.0] }]
        });
        assert!(parse_embeddings(&over_range, &items, 2).is_err());

        let zero = json!({
            "embeddings": [{ "values": [0.0, 0.0] }]
        });
        assert!(parse_embeddings(&zero, &items, 2).is_err());

        let valid = json!({
            "embeddings": [{ "values": [1.0, 0.0] }]
        });
        assert_eq!(
            parse_embeddings(&valid, &items, 2).unwrap()[0].vector,
            vec![1.0, 0.0]
        );
    }

    #[test]
    fn image_content_is_rejected_before_any_client_call() {
        let adapter =
            GeminiEmbeddingAdapter::new(StubClient, ADOPTED_MODEL_PIN, ADOPTED_DIMENSIONS);
        let error = adapter
            .embed(EmbeddingRequest {
                input_type: EmbeddingInputType::ImageObject,
                items: vec![EmbeddingItem {
                    id: "image".to_owned(),
                    content: EmbeddingContent::Image {
                        bytes: vec![1, 2, 3],
                        mime: "image/png".to_owned(),
                    },
                }],
                idempotency_token: None,
            })
            .unwrap_err();
        assert!(matches!(error, AdapterError::ContractViolation(_)));
    }
}
