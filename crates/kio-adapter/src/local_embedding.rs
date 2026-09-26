//! The `offline_api` embedding adapter — a local multimodal embedding server
//! reached over loopback (07 §5.3, 07 §3's D1 url restriction).
//!
//! # The wire, and why it is the shape it is
//!
//! 07 §5.3 (2) mandates chat-form `messages` for every call, text-only ones
//! included, because a server's plain-`input` path tokenizes the same
//! characters differently and would split one declared profile across two
//! vector spaces that [03 §7]'s compatibility gate cannot tell apart. V4
//! measured that split rather than assuming it: the same string through
//! `input[]` and through `messages` lands at cosine 0.474, while two
//! *unrelated* sentences both through `messages` sit at 0.597. Changing the
//! wire format moves a vector further than changing the text does. The price
//! is one item per request — `messages` takes a single conversation — and that
//! is what the price buys.
//!
//! No system message is sent. That is not an omission: the model's own chat
//! template injects `"Represent the user's input."` when none is present, so
//! the string belongs to the template, which `prompt_template_hash` already
//! covers. Sending one while the profile records `instruction: ""` would make
//! the identity describe a token stream the adapter does not produce, which is
//! precisely the failure 07 §5.3 exists to prevent. See the dated ruling there.
//!
//! The sole backend talks to the measured local model. Evaluator-only
//! deterministic embeddings live in their own adapter module, so this runtime
//! can never silently mint evaluator vectors under a local-model identity.

use serde_json::{Value, json};

use crate::http_policy::{
    EMBEDDING_RESPONSE_MAX_BYTES, HttpPolicy, HttpResponse, authenticated_local_agent,
    read_json_bounded, require_success,
};
use crate::identity::tool_profile_hash;
use crate::local_peer::{AuthenticatedLocalEndpoint, is_local_peer_tls_error};
use crate::traits::EmbeddingAdapter;
use crate::types::{
    AdapterKind, AdapterProfile, EmbeddingContent, EmbeddingItem, EmbeddingRequest,
    EmbeddingResponse, EmbeddingVector, ExecutionMode, validate_cosine_vector,
    validate_embedding_request_bytes,
};
use crate::{AdapterError, Result};

/// The `tool_id` the local embedding target is declared under in `tools.toml`
/// (see `tool_lock::EMBEDDING_RUNTIME_TARGETS`).
pub const LOCAL_EMBEDDING_ADAPTER_ID: &str = "qwen3_vl_embedding_local";
pub const LOCAL_EMBEDDING_MODEL_FAMILY: &str = "qwen3-vl-embedding";

/// 04 §4.3: `chunk_vec` is `float[768]`, so a local model is MRL-truncated to
/// the same width. A different width would produce vectors vector search
/// cannot read.
///
/// V4 measured the server returning **2048** natively, so this is a genuine
/// truncation and not a no-op. Whether that costs retrieval quality was V3's
/// question, and **V3 answered it on 2026-08-01: it does not.** Over the 24
/// golden queries against the OCR'd fixture, recall@10 was 0.5417 at native
/// 2048 and 0.5833 at 768 — the truncation did not show up as a loss, so this
/// value is **settled**, not provisional, and with it the `tool_profile_hash`
/// it feeds. Read the +1 query as noise at n=24 rather than as 768 being
/// better; the claim is only that no cost was measurable, which is enough,
/// because widening to 2048 buys a lower measured recall at the price of a
/// `chunk_vec` DDL revision and re-embedding everything (03 §7).
/// `eval/v3/results/README.md` has the numbers and the reasoning.
///
/// No equality check against 2048 lives in the code: a later model in the same
/// family may be wider, and truncating a wider vector is still MRL. What must
/// never happen is truncating *up*, which [`truncate_and_renormalize`] rejects.
pub const LOCAL_EMBEDDING_DIMENSIONS: u32 = 768;

/// The model id sent on the wire when `tools.toml` names none. Matches
/// `tool_lock::EMBEDDING_RUNTIME_TARGETS`' entry for this adapter.
pub const LOCAL_EMBEDDING_DEFAULT_MODEL: &str = "Qwen/Qwen3-VL-Embedding-2B";

/// 03 §5.1: a weight-bearing local adapter pins the sha256 of the weights, not
/// a tag — quantization variants share tag names. Measured in V4 over the sole
/// `model.safetensors` of `Qwen/Qwen3-VL-Embedding-2B` rev `9f2f7e71`; the
/// value matches the published blob hash, so it doubles as a download check.
pub const LOCAL_EMBEDDING_MODEL_VERSION_PIN: &str =
    "sha256:c73fa9caeddeb3ff831d46c085a7a5708343248ca777e90f2d486964464509c1";

/// 07 §5.3 (1): `sha256(JCS({"chat_template": P(T), "instruction": P(I)}))`
/// over the model's shipped `chat_template.jinja` with `instruction = ""`.
/// The accepted inputs and digest are frozen in `eval/v4/results/v4-profile.json`
/// and checked by the Rust profile identity vectors below.  The former Python
/// producer was a completed non-authorizing experiment and has been removed.
pub const LOCAL_EMBEDDING_PROMPT_TEMPLATE_HASH: &str =
    "sha256:7b7f47224b2e5c3fee914cb56bf6c701202dfe2693e4b1160291a81a44389e8b";

/// Byte identity of the archived accepted V4 profile.  The executable
/// experiment is retired; this frozen Rust witness is the reproduction
/// boundary for the adopted profile.
#[cfg(all(test, debug_assertions))]
const LOCAL_EMBEDDING_V4_RESULT_DIGEST: &str =
    "sha256:b5ff0d6fa325c48a4e6143d4e975b96380dd602d5b63e1700e2a14821cb4bb8a";

/// Kio's own name for that template, per 03 §5.1's `prompt_template_id`.
pub const LOCAL_EMBEDDING_PROMPT_TEMPLATE_ID: &str = "kio-local-embedding-v1";

/// Declared by an embedding adapter that genuinely embeds supplied image
/// OBJECT bytes rather than only text.
///
/// `modality: "multimodal"` does not answer this. It describes the vector
/// space (03 §7 fixes one space for every modality), not what a given
/// implementation can be handed. The adopted online adapter declares multimodal
/// and reads only `text`, so it must not be given image items until that
/// changes — at which point adding this flag is the whole of turning it on.
pub const IMAGE_OBJECT_CAPABILITY: &str = "image_object";

/// Selects which local implementation runs.
///
/// A unit enum on purpose: it stays `Copy` and travels inside
/// [`crate::catalog::EmbeddingExecution`], while the declared url and model —
/// which only `Real` needs — are read where the adapter is built, exactly as
/// the online `Real` arm reads its configured model there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalEmbeddingExecution {
    Real,
}

/// One request, one vector. There is no batch method here and that is the
/// point: 07 §5.3 (2) rules out the `input[]` form that would allow batching,
/// so a batching signature would only invite someone to reintroduce it.
pub trait LocalEmbeddingClient: Clone {
    /// `messages` is the full array, already built by [`user_messages`]. The
    /// returned vector is the server's **native** width; MRL truncation is the
    /// adapter's job, not the transport's.
    fn embed_messages(&self, messages: Value) -> Result<Vec<f32>>;
}

/// Talks OpenAI-compatible `/v1/embeddings` to a loopback server.
///
/// Not named after a serving backend, and it must not be: 07 §5.3 (3) keeps
/// the backend out of the identity so that the same weights served two ways
/// stay one vector space. V4 ran this shape against vLLM 0.26.0, where
/// `/v1/embeddings` accepted `messages` directly and no `/pooling` fallback
/// was needed, but nothing here depends on that being vLLM.
#[derive(Debug, Clone)]
pub struct EnvLocalEmbeddingClient {
    endpoint: AuthenticatedLocalEndpoint,
    model: String,
    http_policy: HttpPolicy,
}

impl EnvLocalEmbeddingClient {
    /// `timeout_seconds` is D7's `[adapter.policy.offline_api].timeout_seconds`
    /// (07 §7). `None` keeps the shared default, which is exactly what an absent
    /// sub-table means -- 07 §7 says unspecified keys inherit the parent, and
    /// the parent's documented 300 is what `HttpPolicy::default` already
    /// carries. So `None` is the pre-D7 behaviour, not a special case.
    ///
    /// No built-in `offline_api` default is invented here. 07 §7 defers that
    /// value to Stage 3's local-OCR measurement.
    #[must_use]
    pub fn new(
        endpoint: AuthenticatedLocalEndpoint,
        model: impl Into<String>,
        timeout_seconds: Option<u64>,
    ) -> Self {
        Self {
            endpoint,
            model: model.into(),
            http_policy: timeout_seconds
                .map_or_else(HttpPolicy::default, HttpPolicy::with_timeout_seconds),
        }
    }
}

impl LocalEmbeddingClient for EnvLocalEmbeddingClient {
    fn embed_messages(&self, messages: Value) -> Result<Vec<f32>> {
        let url = format!("{}/v1/embeddings", self.endpoint.base_url());
        let response = authenticated_local_agent(&self.endpoint, self.http_policy)?
            .post(&url)
            .send_json(json!({
                "model": self.model,
                "encoding_format": "float",
                "messages": messages,
            }))
            .map_err(local_http_error)
            .and_then(|response| require_success(response, local_http_status_error))?;
        let body = read_json_bounded(
            response,
            EMBEDDING_RESPONSE_MAX_BYTES,
            "local embedding response",
        )?;
        parse_single_embedding(&body)
    }
}

/// A local server has no credential and no invoice, so the classifications the
/// online adapters need (`Auth`, `QuotaExceeded`) cannot arise. What can is a
/// full request queue, which is a retry, and everything else, which is not.
fn local_http_error(error: ureq::Error) -> AdapterError {
    if is_local_peer_tls_error(&error) {
        return AdapterError::LocalPeerAuth(format!(
            "local embedding server TLS authentication failed: {error}"
        ));
    }
    AdapterError::Network(format!("local embedding server unreachable: {error}"))
}

fn local_http_status_error(response: &HttpResponse) -> AdapterError {
    match response.status().as_u16() {
        429 => {
            let retry_after_ms = response
                .headers()
                .get("Retry-After")
                .and_then(|value| value.to_str().ok())
                .and_then(crate::http_policy::parse_retry_after_ms);
            AdapterError::RateLimit {
                message: format!(
                    "local embedding server queue is full ({})",
                    response.status()
                ),
                retry_after_ms,
            }
        }
        code => AdapterError::Network(format!("local embedding server returned HTTP {code}")),
    }
}

/// The OpenAI embeddings response shape, restricted to the one-item case the
/// wire rule forces. More than one vector back means the server batched
/// something we did not send, so it is a contract violation rather than a
/// value to pick from.
fn parse_single_embedding(body: &Value) -> Result<Vec<f32>> {
    let data = body
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| violation("local embedding response has no `data` array"))?;
    let [entry] = data.as_slice() else {
        return Err(violation(format!(
            "local embedding response carries {} vectors for a single input",
            data.len()
        )));
    };
    let embedding = entry
        .get("embedding")
        .and_then(Value::as_array)
        .ok_or_else(|| violation("local embedding response entry has no `embedding` array"))?;
    embedding
        .iter()
        .map(|value| {
            value
                .as_f64()
                .filter(|number| number.is_finite())
                .map(|number| number as f32)
                .ok_or_else(|| violation("local embedding response contains a non-numeric value"))
        })
        .collect()
}

/// MRL: keep the leading `dimensions` components and restore unit length.
///
/// The renormalization is not cosmetic. V4 measured the server returning
/// L2 ≈ 1.0 at native width, so a prefix of it is **shorter** than unit — by
/// however much energy the dropped tail carried, which varies per vector.
/// Storing those unnormalized would make cosine similarity depend on how much
/// of each vector's mass happened to live past 768, which is not a property of
/// the text at all.
fn truncate_and_renormalize(raw: &[f32], dimensions: usize) -> Result<Vec<f32>> {
    if raw.len() < dimensions {
        return Err(violation(format!(
            "local embedding server returned {} dimensions, fewer than the declared {dimensions}",
            raw.len()
        )));
    }
    let mut vector = raw[..dimensions].to_vec();
    let norm = vector
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt();
    if !norm.is_finite() || norm <= 0.0 {
        return Err(violation(
            "local embedding truncates to a zero or non-finite vector",
        ));
    }
    for value in &mut vector {
        *value = (f64::from(*value) / norm) as f32;
    }
    Ok(vector)
}

/// The `messages` array for one item — 07 §5.3 (2)'s single wire form.
///
/// Text and image take the same array-of-content shape rather than the string
/// shorthand. The model's template renders both identically for a lone text
/// part, so this costs no tokens, and it means there is one code path whose
/// rendering V4 measured instead of two that merely ought to agree.
///
/// **No system message.** See the module docs.
fn user_messages(item: &EmbeddingItem) -> Result<Value> {
    let content = match &item.content {
        EmbeddingContent::Text { text } => json!([{ "type": "text", "text": text }]),
        EmbeddingContent::Image { bytes, mime } => {
            // Adapter-to-server wire only. 07 §4.4's ban on base64 concerns what
            // goes into a SEARCH RESPONSE, which is a different direction.
            use base64::Engine as _;
            let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
            json!([{ "type": "image_url", "image_url": { "url": format!("data:{mime};base64,{encoded}") } }])
        }
    };
    Ok(json!([{ "role": "user", "content": content }]))
}

fn violation(message: impl Into<String>) -> AdapterError {
    AdapterError::ContractViolation(message.into())
}

/// The measured local backend carried by an adapter instance.
enum Backend<C> {
    Real(C),
}

/// The `offline_api` embedding adapter.
pub struct LocalEmbeddingAdapter<C = EnvLocalEmbeddingClient> {
    backend: Backend<C>,
    dimensions: u32,
}

impl<C: LocalEmbeddingClient> LocalEmbeddingAdapter<C> {
    /// The measured backend, over whichever transport `client` provides.
    #[must_use]
    pub fn with_client(client: C) -> Self {
        Self {
            backend: Backend::Real(client),
            dimensions: LOCAL_EMBEDDING_DIMENSIONS,
        }
    }
}

impl<C> LocalEmbeddingAdapter<C> {
    /// Which backend this instance holds, without exposing the client.
    #[must_use]
    fn execution(&self) -> LocalEmbeddingExecution {
        match self.backend {
            Backend::Real(_) => LocalEmbeddingExecution::Real,
        }
    }
}

/// The profile JSON `tool_profile_hash` is taken over (03 §5.1).
///
/// `runtime_kind` is `"local"` and carries no serving backend: 07 §5.3 (3)
/// forbids naming one here, so that the same weights served two ways stay one
/// profile and a backend swap costs no re-embed.
///
/// Free-standing because callers that only need the identity — 03 §7's
/// compatibility gate, the tool-lock entry — must not have to conjure a
/// transport to ask for it.
#[must_use]
pub fn profile_value_for(execution: LocalEmbeddingExecution) -> Value {
    let mut profile = json!({
        "adapter_kind": "embedding",
        "adapter_role": "multimodal",
        "dimensions": LOCAL_EMBEDDING_DIMENSIONS,
        "distance": "cosine",
        // Same chunk-input construction as the online adapter: the humanized
        // filename is prepended before embedding, which is part of what a
        // vector MEANS and therefore of this identity.
        "input_construction": "chunk_filename_context_v1",
        "modality": "multimodal",
        "model_or_tool_family": LOCAL_EMBEDDING_MODEL_FAMILY,
        "runtime_kind": "local",
        "spec_version": 1
    });
    let fields = profile
        .as_object_mut()
        .expect("profile literal is an object");
    let _ = execution;
    fields.insert(
        "model_version_pin".to_owned(),
        json!(LOCAL_EMBEDDING_MODEL_VERSION_PIN),
    );
    fields.insert(
        "prompt_template_hash".to_owned(),
        json!(LOCAL_EMBEDDING_PROMPT_TEMPLATE_HASH),
    );
    fields.insert(
        "prompt_template_id".to_owned(),
        json!(LOCAL_EMBEDDING_PROMPT_TEMPLATE_ID),
    );
    profile
}

/// The full declared profile for a backend. Same reason as
/// [`profile_value_for`] for being free-standing.
#[must_use]
pub fn profile_for(execution: LocalEmbeddingExecution) -> AdapterProfile {
    let profile = profile_value_for(execution);
    AdapterProfile {
        adapter_kind: AdapterKind::Embedding,
        adapter_id: LOCAL_EMBEDDING_ADAPTER_ID.to_owned(),
        execution_mode: ExecutionMode::OfflineApi,
        tool_profile_hash: tool_profile_hash(&profile)
            .expect("built-in local embedding profile is valid"),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        // `image_object` is the flag the index path gates image enrichment
        // on. It says this implementation actually reads an item's
        // `path`/`mime`, which is not something `modality: "multimodal"`
        // implies — the adopted online adapter declares multimodal and then
        // reads only `text`, so handing it an image item would embed the
        // empty string and store a confidently wrong vector. Capability
        // flags are outside `PROFILE_FIELDS`, so this does not perturb the
        // profile hash (qa35 pins that `capabilities` are not hashed).
        capability_flags: vec![
            "text".to_owned(),
            "multimodal".to_owned(),
            IMAGE_OBJECT_CAPABILITY.to_owned(),
        ],
        // 07 §3: an offline_api adapter reaches loopback only, which is why
        // it is exempt from the §3 consent gate. This flag is what the
        // exemption keys off, so it must stay false.
        allow_network: false,
        // Nothing to bill: the model runs on hardware the user already has.
        // An empty set is also what stops the ledger from reserving and
        // settling against a price that does not exist.
        billable_kinds: Vec::new(),
        reject_billing: None,
        provider_idempotency: crate::types::ProviderIdempotency::NotProvided,
    }
}

impl<C: LocalEmbeddingClient> EmbeddingAdapter for LocalEmbeddingAdapter<C> {
    fn profile(&self) -> AdapterProfile {
        profile_for(self.execution())
    }

    fn embed(&self, request: EmbeddingRequest) -> Result<EmbeddingResponse> {
        validate_embedding_request_bytes(&request)?;
        let vectors = match &self.backend {
            // One request per item, in order. 07 §5.3 (2) forbids the batching
            // form, and the local server's continuous batching is what absorbs
            // the cost — so this loop is the design, not a naive first draft.
            Backend::Real(client) => request
                .items
                .iter()
                .map(|item| {
                    let native = client.embed_messages(user_messages(item)?)?;
                    Ok(EmbeddingVector {
                        id: item.id.clone(),
                        vector: truncate_and_renormalize(&native, self.dimensions as usize)?,
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        };
        // 07 §5.3 acceptance checks (1)-(3): one vector per input, in order,
        // each of the declared width and usable as a cosine operand. The mock
        // satisfies these by construction, but running them here means the
        // checks are exercised on this path too rather than only on Gemini's.
        if vectors.len() != request.items.len() {
            return Err(AdapterError::ContractViolation(
                "local embedding returned a different number of vectors than inputs".to_owned(),
            ));
        }
        for (vector, item) in vectors.iter().zip(&request.items) {
            if vector.id != item.id {
                return Err(AdapterError::ContractViolation(
                    "local embedding returned vectors out of input order".to_owned(),
                ));
            }
            validate_cosine_vector(&vector.vector, self.dimensions)?;
        }
        Ok(EmbeddingResponse {
            vectors,
            dimensions: self.dimensions,
            distance: "cosine".to_owned(),
            modality: "multimodal".to_owned(),
            embedding_profile_hash: Some(self.profile().tool_profile_hash),
            // No token accounting: there is no provider and no invoice. The
            // ledger path reads `billable_kinds` (empty, above) and skips.
            usage: None,
        })
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::types::{
        EmbeddingContent, EmbeddingInputType, EmbeddingItem, MAX_EMBEDDING_ITEM_BYTES,
        MAX_EMBEDDING_REQUEST_BYTES,
    };
    use sha2::{Digest, Sha256};

    fn sha256(bytes: &[u8]) -> String {
        let digest = Sha256::digest(bytes);
        let hex = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        format!("sha256:{hex}")
    }

    fn request(texts: &[&str]) -> EmbeddingRequest {
        EmbeddingRequest {
            input_type: EmbeddingInputType::MarkdownChunk,
            items: texts
                .iter()
                .enumerate()
                .map(|(index, text)| EmbeddingItem::text(format!("item-{index}"), *text))
                .collect(),
            idempotency_token: None,
        }
    }

    #[test]
    fn profile_declares_the_offline_posture() {
        let profile = LocalEmbeddingAdapter::with_client(RecordingClient::new()).profile();
        assert_eq!(profile.execution_mode, ExecutionMode::OfflineApi);
        assert_eq!(profile.adapter_id, LOCAL_EMBEDDING_ADAPTER_ID);
        // The three properties the offline forks key off (07 §3 / 07 §5.5).
        assert!(!profile.allow_network, "offline_api must not claim network");
        assert!(
            profile.billable_kinds.is_empty(),
            "a local model has nothing to bill"
        );
        assert!(profile.reject_billing.is_none());
    }

    /// The flag that gates image enrichment. The local adapter reads an item's
    /// `path`/`mime`; the adopted online one does not, and declaring
    /// `multimodal` is not the same claim.
    #[test]
    fn declares_the_image_object_capability_that_the_online_adapter_does_not() {
        let local = LocalEmbeddingAdapter::with_client(RecordingClient::new()).profile();
        assert!(
            local
                .capability_flags
                .iter()
                .any(|flag| flag == IMAGE_OBJECT_CAPABILITY)
        );
        let online = crate::catalog::adopted_embedding_profile();
        assert!(
            !online
                .capability_flags
                .iter()
                .any(|flag| flag == IMAGE_OBJECT_CAPABILITY),
            "the online adapter reads only `text`; claiming this flag would have \
             it embed the empty string for every image"
        );
    }

    /// 07 §5.3: the serving backend must not appear anywhere in the hashed
    /// profile, so that the same weights served two ways stay one vector space
    /// and a backend swap does not force a re-embed.
    #[test]
    fn profile_identity_names_no_serving_backend() {
        let value = profile_value_for(LocalEmbeddingExecution::Real);
        let rendered = value.to_string().to_ascii_lowercase();
        for backend in ["vllm", "sglang", "llama.cpp", "llamacpp", "ollama", "mlx"] {
            assert!(
                !rendered.contains(backend),
                "profile identity must not name `{backend}`: {rendered}"
            );
        }
        assert_eq!(value["runtime_kind"], "local");
    }

    /// 03 §7 / 07 §5.3: the local profile is a DIFFERENT vector space from the
    /// online one. The compatibility gate must be able to tell them apart, and
    /// it only has the profile hash to do it with.
    #[test]
    fn local_profile_hash_differs_from_the_online_one() {
        let local = LocalEmbeddingAdapter::with_client(RecordingClient::new()).profile();
        let online = crate::catalog::adopted_embedding_profile();
        assert_ne!(local.tool_profile_hash, online.tool_profile_hash);
        // Same width and metric, though — `chunk_vec` is one table (04 §4.3),
        // so a swap is a re-embed and not a schema migration.
        assert_eq!(LOCAL_EMBEDDING_DIMENSIONS, 768);
    }

    #[test]
    fn embed_returns_one_usable_vector_per_input_in_order() {
        let adapter = LocalEmbeddingAdapter::with_client(RecordingClient::new());
        let response = adapter.embed(request(&["alpha", "beta", "gamma"])).unwrap();
        assert_eq!(response.vectors.len(), 3);
        assert_eq!(response.dimensions, LOCAL_EMBEDDING_DIMENSIONS);
        assert_eq!(response.distance, "cosine");
        assert_eq!(response.modality, "multimodal");
        assert_eq!(
            response.embedding_profile_hash.as_deref(),
            Some(adapter.profile().tool_profile_hash.as_str())
        );
        for (index, vector) in response.vectors.iter().enumerate() {
            assert_eq!(vector.id, format!("item-{index}"));
            validate_cosine_vector(&vector.vector, LOCAL_EMBEDDING_DIMENSIONS).unwrap();
        }
        // Distinct inputs must not collapse onto one vector, or vector search
        // would rank every chunk identically.
        assert_ne!(response.vectors[0].vector, response.vectors[1].vector);
    }

    #[test]
    fn embed_reports_no_usage_because_there_is_no_invoice() {
        let response = LocalEmbeddingAdapter::with_client(RecordingClient::new())
            .embed(request(&["alpha"]))
            .unwrap();
        assert!(response.usage.is_none());
    }

    /// 07 §5.5: no batch lane. A local server has no job queue to submit to,
    /// and the half-price rationale that puts Gemini on Batch does not exist.
    #[test]
    fn prefers_the_sync_lane() {
        use crate::traits::PreferredRequestKind;
        assert_eq!(
            LocalEmbeddingAdapter::with_client(RecordingClient::new()).preferred_request_kind(),
            PreferredRequestKind::Sync
        );
    }

    // -- the measured backend -------------------------------------------------
    //
    // CI has no GPU, so the real server is stood in for by a client that
    // records what it was sent. What these tests pin is everything between the
    // item and the wire, plus everything between the wire and the stored
    // vector — which is where V4's measurements actually land in code.

    /// Answers with a native-width vector whose energy straddles the MRL cut,
    /// so truncation genuinely changes the norm and the renormalization is not
    /// a no-op that would pass either way.
    #[derive(Clone)]
    struct RecordingClient {
        sent: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    }

    impl RecordingClient {
        fn new() -> Self {
            Self {
                sent: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }

        fn sent(&self) -> Vec<Value> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl LocalEmbeddingClient for RecordingClient {
        fn embed_messages(&self, messages: Value) -> Result<Vec<f32>> {
            let mut sent = self.sent.lock().unwrap();
            let index = sent.len();
            sent.push(messages);
            let mut native = vec![0.0_f32; NATIVE_DIMENSIONS];
            // 0.6 inside the kept prefix, 0.8 past it: unit at native width,
            // 0.6 long once truncated.
            native[index] = 0.6;
            native[LOCAL_EMBEDDING_DIMENSIONS as usize + index] = 0.8;
            Ok(native)
        }
    }

    /// What V4 measured the server returning before MRL truncation.
    const NATIVE_DIMENSIONS: usize = 2048;

    fn norm(vector: &[f32]) -> f64 {
        vector
            .iter()
            .map(|value| f64::from(*value).powi(2))
            .sum::<f64>()
            .sqrt()
    }

    /// 07 §5.3 (2) and the module's ruling on the system message, read off the
    /// bytes that would go out rather than off the code that builds them.
    #[test]
    fn the_real_backend_sends_one_user_turn_per_item_and_no_system_message() {
        let client = RecordingClient::new();
        let adapter = LocalEmbeddingAdapter::with_client(client.clone());
        adapter.embed(request(&["alpha", "beta"])).unwrap();

        let sent = client.sent();
        assert_eq!(sent.len(), 2, "one request per item — never a batch");
        for (index, messages) in sent.iter().enumerate() {
            let turns = messages.as_array().expect("messages is an array");
            assert_eq!(
                turns.len(),
                1,
                "a system turn would change the token stream"
            );
            assert_eq!(turns[0]["role"], "user");
            assert_eq!(turns[0]["content"][0]["type"], "text");
            assert_eq!(
                turns[0]["content"][0]["text"],
                ["alpha", "beta"][index],
                "items must go out in order"
            );
        }
    }

    /// The MRL step. A prefix of a unit vector is short by however much energy
    /// the tail carried, and that amount varies per vector — storing it
    /// unnormalized would make cosine depend on where a text's mass happened
    /// to sit rather than on the text.
    #[test]
    fn truncation_restores_unit_length_rather_than_keeping_a_short_prefix() {
        let mut native = vec![0.0_f32; NATIVE_DIMENSIONS];
        native[0] = 0.6;
        native[LOCAL_EMBEDDING_DIMENSIONS as usize] = 0.8;
        assert!(
            (norm(&native) - 1.0).abs() < 1e-6,
            "fixture is unit at native width"
        );

        let stored =
            truncate_and_renormalize(&native, LOCAL_EMBEDDING_DIMENSIONS as usize).unwrap();
        assert_eq!(stored.len(), LOCAL_EMBEDDING_DIMENSIONS as usize);
        assert!(
            (norm(&stored) - 1.0).abs() < 1e-6,
            "truncated vector must be renormalized, got norm {}",
            norm(&stored)
        );
        // The bare prefix would have been 0.6 long, so this cannot pass by
        // accident on a code path that only slices.
        assert!((f64::from(stored[0]) - 1.0).abs() < 1e-6);
    }

    /// Truncating *up* is not MRL, it is inventing components.
    #[test]
    fn a_server_narrower_than_the_stored_width_is_a_contract_violation() {
        let narrow = vec![1.0_f32; LOCAL_EMBEDDING_DIMENSIONS as usize - 1];
        let error =
            truncate_and_renormalize(&narrow, LOCAL_EMBEDDING_DIMENSIONS as usize).unwrap_err();
        assert!(
            matches!(error, AdapterError::ContractViolation(_)),
            "{error:?}"
        );
    }

    #[test]
    fn an_all_zero_prefix_is_rejected_rather_than_divided_by_zero() {
        let native = vec![0.0_f32; NATIVE_DIMENSIONS];
        assert!(truncate_and_renormalize(&native, LOCAL_EMBEDDING_DIMENSIONS as usize).is_err());
    }

    /// 07 §5.3 (2): the image lane rides the same `messages` shape. The base64
    /// here is the ADAPTER-to-server wire, which §4.4's ban does not concern.
    #[test]
    fn an_image_item_travels_as_a_data_uri_in_the_same_message_shape() {
        let bytes: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

        let client = RecordingClient::new();
        LocalEmbeddingAdapter::with_client(client.clone())
            .embed(EmbeddingRequest {
                input_type: crate::types::EmbeddingInputType::ImageObject,
                items: vec![EmbeddingItem {
                    id: "sha256:aaa".to_owned(),
                    content: EmbeddingContent::Image {
                        bytes: bytes.to_vec(),
                        mime: "image/png".to_owned(),
                    },
                }],
                idempotency_token: None,
            })
            .unwrap();

        let sent = client.sent();
        let content = &sent[0][0]["content"][0];
        assert_eq!(
            sent[0][0]["role"], "user",
            "no system turn on the image lane either"
        );
        assert_eq!(content["type"], "image_url");
        use base64::Engine as _;
        let expected = base64::engine::general_purpose::STANDARD.encode(bytes);
        assert_eq!(
            content["image_url"]["url"],
            format!("data:image/png;base64,{expected}")
        );
    }

    #[test]
    fn image_payload_debug_redacts_the_source_bytes() {
        let item = EmbeddingItem {
            id: "sha256:aaa".to_owned(),
            content: EmbeddingContent::Image {
                bytes: b"private image bytes".to_vec(),
                mime: "image/png".to_owned(),
            },
        };
        let rendered = format!("{item:?}");
        assert!(rendered.contains("bytes_len"));
        assert!(!rendered.contains("private image bytes"));
    }

    #[test]
    fn oversized_image_is_rejected_before_the_client_or_base64_wire() {
        let client = RecordingClient::new();
        let error = LocalEmbeddingAdapter::with_client(client)
            .embed(EmbeddingRequest {
                input_type: crate::types::EmbeddingInputType::ImageObject,
                items: vec![EmbeddingItem {
                    id: "sha256:aaa".to_owned(),
                    content: EmbeddingContent::Image {
                        bytes: vec![0; MAX_EMBEDDING_ITEM_BYTES + 1],
                        mime: "image/png".to_owned(),
                    },
                }],
                idempotency_token: None,
            })
            .unwrap_err();
        assert!(
            matches!(error, AdapterError::ContractViolation(_)),
            "{error:?}"
        );
    }

    #[test]
    fn aggregate_payload_cap_is_enforced_before_any_client_call() {
        let client = RecordingClient::new();
        let half = MAX_EMBEDDING_REQUEST_BYTES / 2 + 1;
        let error = LocalEmbeddingAdapter::with_client(client.clone())
            .embed(EmbeddingRequest {
                input_type: crate::types::EmbeddingInputType::ImageObject,
                items: vec![
                    EmbeddingItem {
                        id: "a".to_owned(),
                        content: EmbeddingContent::Image {
                            bytes: vec![0; half],
                            mime: "image/png".to_owned(),
                        },
                    },
                    EmbeddingItem {
                        id: "b".to_owned(),
                        content: EmbeddingContent::Image {
                            bytes: vec![0; half],
                            mime: "image/png".to_owned(),
                        },
                    },
                ],
                idempotency_token: None,
            })
            .unwrap_err();
        assert!(matches!(error, AdapterError::ContractViolation(_)));
        assert!(client.sent().is_empty());
    }

    /// More vectors back than items sent means the server batched something we
    /// did not ask it to, so there is no safe one to pick.
    #[test]
    fn a_response_carrying_more_than_one_vector_is_a_contract_violation() {
        let body = json!({"data": [{"embedding": [1.0, 2.0]}, {"embedding": [3.0, 4.0]}]});
        assert!(matches!(
            parse_single_embedding(&body).unwrap_err(),
            AdapterError::ContractViolation(_)
        ));
    }

    #[test]
    fn a_response_with_a_non_numeric_component_is_a_contract_violation() {
        let body = json!({"data": [{"embedding": [1.0, "nope"]}]});
        assert!(parse_single_embedding(&body).is_err());
        assert!(parse_single_embedding(&json!({"data": [{"embedding": [1.0, null]}]})).is_err());
        // JSON has no literal for infinity, so the `is_finite` filter is
        // defence against a PARSER that maps an overflowing exponent to one.
        // Guarded because rejecting it outright is equally correct.
        if let Ok(overflowed) = serde_json::from_str::<Value>(r#"{"data":[{"embedding":[1e400]}]}"#)
        {
            assert!(
                parse_single_embedding(&overflowed).is_err(),
                "a non-finite component must not reach the stored vector"
            );
        }
    }

    #[test]
    fn a_well_formed_response_parses_in_order() {
        let body = json!({"data": [{"embedding": [0.25, -0.5, 0.75]}], "usage": {}});
        assert_eq!(
            parse_single_embedding(&body).unwrap(),
            vec![0.25, -0.5, 0.75]
        );
    }

    /// V4's measurement, frozen. If a profile field moves, this fails and the
    /// person moving it has to decide consciously whether every vector already
    /// stored under the old identity is being orphaned (03 §7).
    #[test]
    fn the_real_profile_matches_the_identity_v4_settled() {
        assert_eq!(
            sha256(include_bytes!("../../../eval/v4/results/v4-profile.json")),
            LOCAL_EMBEDDING_V4_RESULT_DIGEST
        );
        let profile = profile_value_for(LocalEmbeddingExecution::Real);
        assert_eq!(
            profile["model_version_pin"],
            LOCAL_EMBEDDING_MODEL_VERSION_PIN
        );
        assert_eq!(
            profile["prompt_template_hash"],
            LOCAL_EMBEDDING_PROMPT_TEMPLATE_HASH
        );
        assert_eq!(
            profile["prompt_template_id"],
            LOCAL_EMBEDDING_PROMPT_TEMPLATE_ID
        );
        assert_eq!(
            profile_for(LocalEmbeddingExecution::Real).tool_profile_hash,
            "sha256:f9f610bbe0dde5799630031e312a078ec94c1b71f7bd8ae56f2c5f08d365439a",
            "this is eval/v4/results/v4-profile.json's tool_profile_hash; if it \
             no longer matches, the Rust profile and the measured one have \
             diverged and one of them is describing vectors that do not exist"
        );
    }

    #[test]
    fn local_embedding_identity_always_names_the_measured_template() {
        let profile = profile_value_for(LocalEmbeddingExecution::Real);
        assert!(profile.get("prompt_template_hash").is_some());
        assert!(profile.get("prompt_template_id").is_some());
    }

    /// Plain loopback HTTP used to be accepted here.  It is now rejected before
    /// a client exists, so a listener cannot observe even a request body.
    #[test]
    fn plain_http_cannot_construct_the_real_client() {
        let directory = tempfile::tempdir().unwrap();
        let error = AuthenticatedLocalEndpoint::new(
            "http://127.0.0.1:8000",
            directory.path().join("peer-ca.pem"),
        )
        .unwrap_err();
        assert!(matches!(error, AdapterError::LocalPeerConfig { .. }));
    }

    /// A busy local server is a retry; anything else is not. There is no
    /// credential and no invoice here, so `Auth` and `QuotaExceeded` cannot
    /// arise and must not be invented.
    #[test]
    fn a_busy_server_classifies_as_a_rate_limit_and_a_broken_one_does_not() {
        let busy = ureq::http::Response::builder()
            .status(429)
            .body(ureq::Body::builder().data(Vec::new()))
            .unwrap();
        assert!(matches!(
            local_http_status_error(&busy),
            AdapterError::RateLimit { .. }
        ));
        let broken = ureq::http::Response::builder()
            .status(500)
            .body(ureq::Body::builder().data(Vec::new()))
            .unwrap();
        assert!(matches!(
            local_http_status_error(&broken),
            AdapterError::Network(_)
        ));
    }
}
