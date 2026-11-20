//! Loopback embedding transport for configured endpoints.

use serde_json::{Value, json};

use crate::http_policy::{
    EMBEDDING_RESPONSE_MAX_BYTES, HttpPolicy, HttpResponse, local_peer_agent, read_json_bounded,
    require_success,
};
use crate::identity::tool_profile_hash;
use crate::local_peer::{LocalEndpoint, is_local_peer_tls_error};
use crate::tool_lock::DeclaredAdapter;
use crate::traits::EmbeddingAdapter;
use crate::types::{
    AdapterKind, AdapterProfile, EmbeddingCompleted, EmbeddingContent, EmbeddingItem,
    EmbeddingOutcome, EmbeddingRequest, EmbeddingUsage, EmbeddingVector, ExecutionMode,
    validate_cosine_vector, validate_embedding_request_bytes,
};
use crate::{AdapterError, Result};

/// Tool ID of the configurable loopback embedding target.
pub const GENERIC_LOCAL_EMBEDDING_TOOL_ID: &str = "local_embedding";

pub const LOCAL_EMBEDDING_DIMENSIONS: u32 = 768;

/// Declared by an embedding adapter that genuinely embeds supplied image
/// OBJECT bytes rather than only text.
///
/// `modality: "multimodal"` does not answer this. It describes the vector
/// space (03 §7 fixes one space for every modality), not what a given
/// implementation can be handed. The adopted online adapter declares multimodal
/// and reads only `text`, so it must not be given image items until that
/// changes — at which point adding this flag is the whole of turning it on.
pub const IMAGE_OBJECT_CAPABILITY: &str = "image_object";

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum WireFormat {
    Input,
    Messages,
}

#[derive(Clone)]
pub(crate) struct LocalEmbeddingSpec {
    wire_format: WireFormat,
    pub(crate) model: String,
    model_version_pin: String,
    chat_template_hash: String,
    instruction: String,
    truncate_dimensions: bool,
}

impl LocalEmbeddingSpec {
    pub(crate) fn from_declaration(declared: &DeclaredAdapter) -> Result<Self> {
        fn required(key: &str) -> AdapterError {
            AdapterError::ConfigSchema(format!("embedding.local_embedding requires `{key}`"))
        }

        fn sha256_pin(value: Option<&str>, key: &str) -> Result<String> {
            let value = value.ok_or_else(|| required(key))?;
            if !kio_core::cas::is_hash(value) {
                return Err(AdapterError::ConfigSchema(format!(
                    "embedding.local_embedding `{key}` must be a lowercase sha256 digest"
                )));
            }
            Ok(value.to_owned())
        }

        if declared.kind.as_deref() != Some("offline_api") {
            return Err(AdapterError::ConfigSchema(
                "embedding.local_embedding requires kind = 'offline_api'".to_owned(),
            ));
        }
        if declared.url.is_none() {
            return Err(required("url"));
        }
        if declared.auth.is_some() {
            return Err(AdapterError::ConfigSchema(
                "embedding.local_embedding does not accept `auth`".to_owned(),
            ));
        }
        let options = declared
            .local_embedding
            .as_ref()
            .ok_or_else(|| required("wire_format"))?;
        let wire_format = match options.wire_format.as_deref() {
            Some("input") => WireFormat::Input,
            Some("messages") => WireFormat::Messages,
            Some(_) => {
                return Err(AdapterError::ConfigSchema(
                    "embedding.local_embedding `wire_format` must be `input` or `messages`"
                        .to_owned(),
                ));
            }
            None => return Err(required("wire_format")),
        };
        Ok(Self {
            wire_format,
            model: declared
                .model
                .as_ref()
                .filter(|model| !model.trim().is_empty())
                .ok_or_else(|| required("model"))?
                .clone(),
            model_version_pin: sha256_pin(
                options.model_version_pin.as_deref(),
                "model_version_pin",
            )?,
            chat_template_hash: sha256_pin(
                options.chat_template_hash.as_deref(),
                "chat_template_hash",
            )?,
            instruction: options
                .instruction
                .clone()
                .ok_or_else(|| required("instruction"))?,
            truncate_dimensions: options.truncate_dimensions,
        })
    }

    pub(crate) fn profile(&self) -> AdapterProfile {
        let identity = json!({
            "adapter_kind": "embedding",
            "adapter_role": if self.wire_format == WireFormat::Input { "text" } else { "multimodal" },
            "dimensions": LOCAL_EMBEDDING_DIMENSIONS,
            "distance": "cosine",
            "modality": "multimodal",
            "input_construction": "chunk_filename_context_v1",
            "model_or_tool_family": self.model,
            "model_version_pin": self.model_version_pin,
            "embedding_semantics": {
                "wire_format": self.wire_format,
                "chat_template_hash": self.chat_template_hash,
                "instruction": self.instruction,
                "truncate_dimensions": self.truncate_dimensions,
            },
            "runtime_kind": "local",
            "spec_version": 1,
        });
        local_profile(GENERIC_LOCAL_EMBEDDING_TOOL_ID, &identity, self.wire_format)
    }

    fn payload(&self, item: &EmbeddingItem) -> Result<Value> {
        let mut payload = match self.wire_format {
            WireFormat::Input => {
                let EmbeddingContent::Text { text } = &item.content else {
                    return Err(violation("image_capability_unavailable"));
                };
                json!({ "input": format!("{}{text}", self.instruction) })
            }
            WireFormat::Messages => json!({ "messages": user_messages(item, &self.instruction) }),
        };
        payload["dimensions"] = json!(LOCAL_EMBEDDING_DIMENSIONS);
        Ok(payload)
    }

    fn accept_vector(&self, raw: Vec<f32>) -> Result<Vec<f32>> {
        if raw.len() > LOCAL_EMBEDDING_DIMENSIONS as usize && self.truncate_dimensions {
            return truncate_and_renormalize(&raw, LOCAL_EMBEDDING_DIMENSIONS as usize);
        }
        validate_cosine_vector(&raw, LOCAL_EMBEDDING_DIMENSIONS)?;
        Ok(raw)
    }
}

/// One payload and one response vector per embedding item.
pub trait LocalEmbeddingClient {
    fn embed_payload(&self, payload: Value) -> Result<Vec<f32>>;
}

/// Shared loopback client for the OpenAI-compatible embedding route.
#[derive(Debug, Clone)]
pub struct EnvLocalEmbeddingClient {
    endpoint: LocalEndpoint,
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
        endpoint: LocalEndpoint,
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
    fn embed_payload(&self, mut payload: Value) -> Result<Vec<f32>> {
        payload["model"] = json!(self.model);
        payload["encoding_format"] = json!("float");
        let url = self.endpoint.endpoint_url("/v1/embeddings");
        let response = local_peer_agent(&self.endpoint, self.http_policy)?
            .post(&url)
            .send_json(payload)
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

/// Classify failures to reach the configured loopback peer.
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
        code @ 400..=499 if code != 408 => AdapterError::InvalidInput(format!(
            "local embedding server rejected the request (HTTP {code})"
        )),
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
    if entry.get("index").and_then(Value::as_u64) != Some(0) {
        return Err(violation(
            "local embedding response must identify the single input with index 0",
        ));
    }
    let embedding = entry
        .get("embedding")
        .and_then(Value::as_array)
        .ok_or_else(|| violation("local embedding response entry has no `embedding` array"))?;
    embedding
        .iter()
        .map(|value| {
            value
                .as_f64()
                .map(|number| number as f32)
                .filter(|number| number.is_finite())
                .ok_or_else(|| violation("local embedding response contains a non-numeric value"))
        })
        .collect()
}

/// MRL: keep the leading `dimensions` components and restore unit length.
///
/// The retained prefix can have a different norm from the full vector.
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

/// One user message, optionally preceded by the configured system instruction.
fn user_messages(item: &EmbeddingItem, instruction: &str) -> Value {
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
    let user = json!({ "role": "user", "content": content });
    if instruction.is_empty() {
        json!([user])
    } else {
        json!([{ "role": "system", "content": instruction }, user])
    }
}

fn violation(message: impl Into<String>) -> AdapterError {
    AdapterError::ContractViolation(message.into())
}

/// The `offline_api` embedding adapter.
pub struct LocalEmbeddingAdapter<C = EnvLocalEmbeddingClient> {
    client: C,
    spec: LocalEmbeddingSpec,
    profile: AdapterProfile,
}

pub(crate) fn descriptor_from_profile(
    profile: &AdapterProfile,
) -> crate::types::EmbeddingDescriptor {
    let mut descriptor = crate::types::EmbeddingDescriptor::from_profile(profile);
    if profile
        .capability_flags
        .iter()
        .any(|flag| flag == IMAGE_OBJECT_CAPABILITY)
    {
        descriptor.capability.image_mime_types =
            vec!["image/png".to_owned(), "image/jpeg".to_owned()];
    }
    descriptor
}

impl<C: LocalEmbeddingClient> LocalEmbeddingAdapter<C> {
    pub(crate) fn with_spec(client: C, spec: LocalEmbeddingSpec) -> Self {
        Self {
            profile: spec.profile(),
            client,
            spec,
        }
    }
}

fn local_profile(tool_id: &str, identity: &Value, wire_format: WireFormat) -> AdapterProfile {
    let mut capability_flags = vec!["text".to_owned()];
    if wire_format == WireFormat::Messages {
        capability_flags.extend(["multimodal".to_owned(), IMAGE_OBJECT_CAPABILITY.to_owned()]);
    }
    AdapterProfile {
        adapter_kind: AdapterKind::Embedding,
        adapter_id: tool_id.to_owned(),
        execution_mode: ExecutionMode::OfflineApi,
        tool_profile_hash: tool_profile_hash(identity).expect("local embedding profile is valid"),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        capability_flags,
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
        self.profile.clone()
    }

    fn descriptor(&self) -> crate::types::EmbeddingDescriptor {
        descriptor_from_profile(&self.profile())
    }

    fn embed(&self, request: EmbeddingRequest) -> Result<EmbeddingOutcome> {
        validate_embedding_request_bytes(&request)?;
        if self.spec.wire_format == WireFormat::Input
            && request
                .items
                .iter()
                .any(|item| matches!(item.content, EmbeddingContent::Image { .. }))
        {
            return Err(violation("image_capability_unavailable"));
        }
        let vectors = request
            .items
            .iter()
            .map(|item| {
                let vector = self
                    .spec
                    .accept_vector(self.client.embed_payload(self.spec.payload(item)?)?)?;
                Ok(EmbeddingVector {
                    id: item.id.clone(),
                    vector,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(EmbeddingOutcome::Completed(EmbeddingCompleted {
            members: vectors.into_iter().map(Into::into).collect(),
            dimensions: LOCAL_EMBEDDING_DIMENSIONS,
            distance: "cosine".to_owned(),
            modality: "multimodal".to_owned(),
            embedding_profile_hash: self.profile.tool_profile_hash.clone(),
            // No token accounting: there is no provider and no invoice. The
            // ledger path reads `billable_kinds` (empty, above) and skips.
            usage: EmbeddingUsage::Unmetered,
        }))
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::types::{
        EmbeddingContent, EmbeddingInputType, EmbeddingItem, MAX_EMBEDDING_ITEM_BYTES,
        MAX_EMBEDDING_REQUEST_BYTES,
    };

    fn messages_spec() -> LocalEmbeddingSpec {
        LocalEmbeddingSpec {
            wire_format: WireFormat::Messages,
            model: "test-embedding".to_owned(),
            model_version_pin: kio_core::cas::hash_bytes(b"weights"),
            chat_template_hash: kio_core::cas::hash_bytes(b"template"),
            instruction: String::new(),
            truncate_dimensions: true,
        }
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
        let profile =
            LocalEmbeddingAdapter::with_spec(RecordingClient::new(), messages_spec()).profile();
        assert_eq!(profile.execution_mode, ExecutionMode::OfflineApi);
        assert_eq!(profile.adapter_id, GENERIC_LOCAL_EMBEDDING_TOOL_ID);
        // The three properties the offline forks key off (07 §3 / 07 §5.5).
        assert!(!profile.allow_network, "offline_api must not claim network");
        assert!(
            profile.billable_kinds.is_empty(),
            "a local model has nothing to bill"
        );
        assert!(profile.reject_billing.is_none());
    }

    /// Both local and online adapters now declare actual PNG/JPEG image support.
    #[test]
    fn declares_image_object_capability_for_local_and_online_png_jpeg() {
        let local =
            LocalEmbeddingAdapter::with_spec(RecordingClient::new(), messages_spec()).profile();
        assert!(
            local
                .capability_flags
                .iter()
                .any(|flag| flag == IMAGE_OBJECT_CAPABILITY)
        );
        let online = crate::catalog::adopted_embedding_profile();
        assert!(
            online
                .capability_flags
                .iter()
                .any(|flag| flag == IMAGE_OBJECT_CAPABILITY)
        );
        let descriptor = crate::catalog::embedding_descriptor(
            &crate::tool_lock::AdapterRuntimeSettings::default(),
            crate::catalog::EmbeddingExecution::Online(
                crate::catalog::AdoptedEmbeddingExecution::Real,
            ),
        )
        .unwrap();
        assert_eq!(
            descriptor
                .capability
                .image_mime_types
                .iter()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            ["image/jpeg", "image/png"].into_iter().collect()
        );
    }

    /// 03 §7 / 07 §5.3: the local profile is a DIFFERENT vector space from the
    /// online one. The compatibility gate must be able to tell them apart, and
    /// it only has the profile hash to do it with.
    #[test]
    fn local_profile_hash_differs_from_the_online_one() {
        let local =
            LocalEmbeddingAdapter::with_spec(RecordingClient::new(), messages_spec()).profile();
        let online = crate::catalog::adopted_embedding_profile();
        assert_ne!(local.tool_profile_hash, online.tool_profile_hash);
        // Same width and metric, though — `chunk_vec` is one table (04 §4.3),
        // so a swap is a re-embed and not a schema migration.
        assert_eq!(LOCAL_EMBEDDING_DIMENSIONS, 768);
    }

    #[test]
    fn embed_returns_one_usable_vector_per_input_in_order() {
        let adapter = LocalEmbeddingAdapter::with_spec(RecordingClient::new(), messages_spec());
        let response = adapter
            .embed(request(&["alpha", "beta", "gamma"]))
            .unwrap()
            .into_completed()
            .unwrap();
        assert_eq!(response.vectors().len(), 3);
        assert_eq!(response.dimensions, LOCAL_EMBEDDING_DIMENSIONS);
        assert_eq!(response.distance, "cosine");
        assert_eq!(response.modality, "multimodal");
        assert_eq!(
            response.embedding_profile_hash,
            adapter.profile().tool_profile_hash
        );
        for (index, vector) in response.vectors().iter().enumerate() {
            assert_eq!(vector.id, format!("item-{index}"));
            validate_cosine_vector(&vector.vector, LOCAL_EMBEDDING_DIMENSIONS).unwrap();
        }
        // Distinct inputs must not collapse onto one vector, or vector search
        // would rank every chunk identically.
        assert_ne!(response.vectors()[0].vector, response.vectors()[1].vector);
    }

    #[test]
    fn embed_reports_no_usage_because_there_is_no_invoice() {
        let response = LocalEmbeddingAdapter::with_spec(RecordingClient::new(), messages_spec())
            .embed(request(&["alpha"]))
            .unwrap()
            .into_completed()
            .unwrap();
        assert_eq!(response.usage, EmbeddingUsage::Unmetered);
    }

    /// 07 §5.5: no batch lane. A local server has no job queue to submit to,
    /// and the half-price rationale that puts Gemini on Batch does not exist.
    #[test]
    fn prefers_the_sync_lane() {
        use crate::traits::PreferredRequestKind;
        assert_eq!(
            LocalEmbeddingAdapter::with_spec(RecordingClient::new(), messages_spec())
                .preferred_request_kind(),
            PreferredRequestKind::Sync
        );
    }

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
        fn embed_payload(&self, payload: Value) -> Result<Vec<f32>> {
            let mut sent = self.sent.lock().unwrap();
            let index = sent.len();
            sent.push(payload);
            let mut native = vec![0.0_f32; NATIVE_DIMENSIONS];
            // 0.6 inside the kept prefix, 0.8 past it: unit at native width,
            // 0.6 long once truncated.
            native[index] = 0.6;
            native[LOCAL_EMBEDDING_DIMENSIONS as usize + index] = 0.8;
            Ok(native)
        }
    }

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
    fn generic_local_embedding_wire_and_dimension_policy_apply_to_all_text_items() {
        for wire_format in [WireFormat::Input, WireFormat::Messages] {
            let mut spec = LocalEmbeddingSpec {
                wire_format,
                model: "test-model".into(),
                model_version_pin: "test-weights".into(),
                chat_template_hash: "test-template".into(),
                instruction: String::new(),
                truncate_dimensions: false,
            };
            let client = RecordingClient::new();
            let call = || EmbeddingRequest {
                input_type: EmbeddingInputType::Query,
                ..request(&["needle"])
            };
            let error = LocalEmbeddingAdapter::with_spec(client, spec.clone())
                .embed(call())
                .unwrap_err();
            assert!(matches!(error, AdapterError::ContractViolation(_)));
            let short = vec![1.0; LOCAL_EMBEDDING_DIMENSIONS as usize - 1];
            assert!(spec.accept_vector(short.clone()).is_err());

            spec.truncate_dimensions = true;
            assert!(spec.accept_vector(short).is_err());
            let client = RecordingClient::new();
            let response = LocalEmbeddingAdapter::with_spec(client.clone(), spec)
                .embed(call())
                .unwrap()
                .into_completed()
                .unwrap();
            assert_eq!(response.dimensions, 768);
            assert_eq!(response.usage, EmbeddingUsage::Unmetered);
            assert!((norm(&response.vectors()[0].vector) - 1.0).abs() < 1e-6);

            let mut payloads = client.sent();
            assert_eq!(payloads.len(), 1);
            let payload = payloads.pop().unwrap();
            assert_eq!(payload["dimensions"], 768);
            match wire_format {
                WireFormat::Input => {
                    assert_eq!(payload["input"], "needle");
                    assert!(payload.get("messages").is_none());
                }
                WireFormat::Messages => {
                    assert_eq!(
                        payload["messages"],
                        json!([
                            {"role": "user", "content": [{"type": "text", "text": "needle"}]}
                        ])
                    );
                    assert!(payload.get("input").is_none());
                }
            }
        }
    }

    #[test]
    fn generic_local_embedding_input_rejects_a_mixed_request_before_sending_text() {
        let spec = LocalEmbeddingSpec {
            wire_format: WireFormat::Input,
            model: "test-model".into(),
            model_version_pin: "test-weights".into(),
            chat_template_hash: "test-template".into(),
            instruction: "prefix: ".into(),
            truncate_dimensions: true,
        };
        let client = RecordingClient::new();
        let adapter = LocalEmbeddingAdapter::with_spec(client.clone(), spec);
        let mut mixed = request(&["public document"]);
        mixed.items.push(EmbeddingItem {
            id: "image".into(),
            content: EmbeddingContent::Image {
                bytes: vec![0],
                mime: "image/png".into(),
            },
        });
        assert!(matches!(
            adapter.embed(mixed),
            Err(AdapterError::ContractViolation(reason)) if reason == "image_capability_unavailable"
        ));
        assert!(client.sent().is_empty());

        adapter.embed(request(&["public document"])).unwrap();
        assert_eq!(client.sent()[0]["input"], "prefix: public document");
    }

    #[test]
    fn generic_local_embedding_messages_send_each_image_in_its_own_user_turn() {
        let spec = LocalEmbeddingSpec {
            wire_format: WireFormat::Messages,
            model: "test-model".into(),
            model_version_pin: "test-weights".into(),
            chat_template_hash: "test-template".into(),
            instruction: "Describe the image.".into(),
            truncate_dimensions: true,
        };
        let client = RecordingClient::new();
        let adapter = LocalEmbeddingAdapter::with_spec(client.clone(), spec);
        let items = [(b"a", "YQ=="), (b"b", "Yg==")];
        let response = adapter
            .embed(EmbeddingRequest {
                input_type: EmbeddingInputType::ImageObject,
                items: items
                    .iter()
                    .map(|(bytes, id)| EmbeddingItem {
                        id: (*id).into(),
                        content: EmbeddingContent::Image {
                            bytes: bytes.to_vec(),
                            mime: "image/png".into(),
                        },
                    })
                    .collect(),
                idempotency_token: None,
            })
            .unwrap()
            .into_completed()
            .unwrap();
        let payloads = client.sent();
        assert_eq!(payloads.len(), 2);
        for ((payload, member), (_, encoded)) in payloads.iter().zip(response.members).zip(items) {
            assert_eq!(member.id, encoded);
            assert_eq!(
                payload,
                &json!({
                    "dimensions": 768,
                    "messages": [
                        {"role": "system", "content": "Describe the image."},
                        {"role": "user", "content": [
                            {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{encoded}")}}
                        ]}
                    ],
                })
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
        LocalEmbeddingAdapter::with_spec(client.clone(), messages_spec())
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
        let content = &sent[0]["messages"][0]["content"][0];
        assert_eq!(
            sent[0]["messages"][0]["role"], "user",
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
        let error = LocalEmbeddingAdapter::with_spec(client, messages_spec())
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
        let error = LocalEmbeddingAdapter::with_spec(client.clone(), messages_spec())
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
        let body = json!({"data": [{"index": 0, "embedding": [1.0, "nope"]}]});
        assert!(parse_single_embedding(&body).is_err());
        assert!(
            parse_single_embedding(&json!({"data": [{"index": 0, "embedding": [1.0, null]}]}))
                .is_err()
        );
        // JSON has no literal for infinity, so the `is_finite` filter is
        // defence against a PARSER that maps an overflowing exponent to one.
        // Guarded because rejecting it outright is equally correct.
        if let Ok(overflowed) =
            serde_json::from_str::<Value>(r#"{"data":[{"index":0,"embedding":[1e400]}]}"#)
        {
            assert!(
                parse_single_embedding(&overflowed).is_err(),
                "a non-finite component must not reach the stored vector"
            );
        }
    }

    #[test]
    fn a_well_formed_response_parses_in_order() {
        let body = json!({"data": [{"index": 0, "embedding": [0.25, -0.5, 0.75]}], "usage": {}});
        assert_eq!(
            parse_single_embedding(&body).unwrap(),
            vec![0.25, -0.5, 0.75]
        );
    }

    /// Plain loopback HTTP used to be accepted here.  It is now rejected before
    /// a client exists, so a listener cannot observe even a request body.
    #[test]
    fn plain_http_cannot_construct_the_real_client() {
        let directory = tempfile::tempdir().unwrap();
        let error = LocalEndpoint::new_https(
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
    fn single_embedding_checks_response_index() {
        for index in [json!(1), json!(-1), json!("0"), json!(0.0), Value::Null] {
            let body = json!({"data": [{"index": index, "embedding": [1.0]}]});
            assert!(
                matches!(
                    parse_single_embedding(&body),
                    Err(AdapterError::ContractViolation(_))
                ),
                "{body}"
            );
        }
        let missing = json!({"data": [{"embedding": [1.0]}]});
        assert!(matches!(
            parse_single_embedding(&missing),
            Err(AdapterError::ContractViolation(_))
        ));
        assert_eq!(
            parse_single_embedding(&json!({"data": [{"index": 0, "embedding": [1.0]}]})).unwrap(),
            [1.0]
        );
    }

    #[test]
    fn local_http_input_rejection_is_not_a_network_error() {
        for code in [400, 404, 413, 422] {
            let response = ureq::http::Response::builder()
                .status(code)
                .body(ureq::Body::builder().data(Vec::new()))
                .unwrap();
            assert_eq!(
                local_http_status_error(&response).error_code(),
                "KIO-E-BATCH-INPUT-001"
            );
        }
        for code in [408, 503] {
            let response = ureq::http::Response::builder()
                .status(code)
                .body(ureq::Body::builder().data(Vec::new()))
                .unwrap();
            assert!(matches!(
                local_http_status_error(&response),
                AdapterError::Network(_)
            ));
        }
    }

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
