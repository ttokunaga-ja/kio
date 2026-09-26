//! The optional Rerank adapter (07 §5.4) — a local cross-encoder reached over
//! loopback, same posture as [`crate::local_embedding`].
//!
//! # Written against a measured wire, not a guessed one
//!
//! Every shape below was recorded on real hardware on 2026-08-10 and lives in
//! `tasks/gpu-reranker-verification.md` §5. That brief exists because the last
//! local adapter was written against an endpoint whose form was *known*, and
//! the first real connection still found the response schema wrong in three
//! places. Four things it measured are load-bearing here:
//!
//! 1. **`truncate_prompt_tokens` is mandatory.** A single (query, document)
//!    pair over `max_model_len` fails the **whole batch** with HTTP 400 — the
//!    server does not silently truncate. Sending 05 §1.3's `candidate_depth` =
//!    200 chunks without it means one long chunk costs the entire rerank.
//! 2. **The response echoes each document's text.** Without `top_n` a
//!    200-candidate rerank returns 200 bodies the caller already holds.
//! 3. **`index` is the input subscript, and `results` come back descending.**
//!    This code reads `index` and never the position, so a server that returns
//!    them in another order cannot silently permute the ranking.
//! 4. **Failures arrive as HTTP status, not as a 200 carrying an error.** The
//!    local-OCR adapter was burned by an `errorCode` riding a 200, so the
//!    brief demanded this be checked; six deliberate breakages returned 400 or
//!    404, never 200. Status is therefore a sufficient gate here — a fact,
//!    recorded, not an assumption inherited.
//!
//! # Why no model is baked in
//!
//! [`crate::local_embedding`] pins its model, weights hash and prompt template
//! as constants because that identity decides whether stored vectors are
//! comparable (03 §7). **A reranker stores nothing.** It reorders a list and
//! the order is not persisted, so its profile is provenance rather than a
//! compatibility gate.
//!
//! That is the smaller reason. The larger one is that the measurement did not
//! settle which model to use, and pretending otherwise here would bake the
//! wrong kind of evidence into code. `bge-reranker-v2-m3` led on the repo's own
//! 24 golden queries (21/24 top-1) but `japanese-reranker-base-v2` was one
//! question behind at **half the VRAM and half the latency** (191ms vs 408ms at
//! N=200). One question at n=24 does not separate two models, and this
//! codebase has already shipped one retrieval change that four green tests
//! endorsed and measurement rejected. So [`RerankModel`] is a parameter, and
//! the choice belongs to a `kio-eval` differential.

use serde_json::{Value, json};

use crate::http_policy::{
    HttpPolicy, HttpResponse, RERANK_RESPONSE_MAX_BYTES, authenticated_local_agent,
    read_json_bounded, require_success,
};
use crate::identity::tool_profile_hash;
use crate::local_peer::{AuthenticatedLocalEndpoint, is_local_peer_tls_error};
use crate::traits::RerankAdapter;
use crate::types::{
    AdapterKind, AdapterProfile, ExecutionMode, RerankRequest, RerankResponse, RerankedCandidate,
};
use crate::{AdapterError, Result};

/// The `tool_id` a local reranker is declared under in `tools.toml`.
pub const LOCAL_RERANK_ADAPTER_ID: &str = "cross_encoder_rerank_local";

/// Sent on every request. `-1` means "clamp to this model's `max_model_len`",
/// which is the only setting under which a long candidate degrades instead of
/// failing its 199 neighbours (§5.3).
const TRUNCATE_TO_MODEL_LIMIT: i64 = -1;

/// Which backend an adapter instance holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalRerankExecution {
    Mock,
    Real,
}

/// The model a real backend is serving, as far as identity is concerned.
///
/// `version_pin` follows 03 §5.1 — pin the weights, not a tag, because
/// quantization variants share tag names. The GPU measurement did **not**
/// record weight hashes for any candidate, so whoever adopts a model supplies
/// the measured hash here. There is deliberately no default: a fabricated pin
/// is worse than an absent one, and `sha256:` of nothing is not a value this
/// code will invent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RerankModel {
    /// e.g. `"bge-reranker-v2-m3"`. **Must not name a serving backend.**
    /// 07 §5.3 (3) keeps the runtime out of adapter identity so the same
    /// weights served two ways stay one profile; the reasoning is about
    /// embeddings but the naming rule is general, and `"…-vllm"` here would
    /// make a backend swap look like a different reranker.
    pub family: String,
    /// The measured `sha256:` of the weights.
    pub version_pin: String,
}

/// One query against one candidate pool.
///
/// No batch method: the pool *is* the batch. Splitting it would ask a
/// cross-encoder to compare candidates it never saw together.
pub trait LocalRerankClient: Clone {
    /// Returns `(input index, relevance score)` pairs. Order is not relied on
    /// by the caller — see the module note on `index`.
    fn rerank_documents(
        &self,
        query: &str,
        documents: &[String],
        top_n: usize,
    ) -> Result<Vec<(usize, f64)>>;
}

/// Talks Jina/Cohere-compatible `/v1/rerank` to a loopback server.
///
/// Measured against vLLM 0.26.0, but named for the wire and not the server:
/// TEI advertises the same route shape, and the brief could not test it only
/// because that machine can run neither Docker nor `cargo install` (§5.1). A
/// move to TEI should not need this type renamed.
#[derive(Debug, Clone)]
pub struct EnvLocalRerankClient {
    endpoint: AuthenticatedLocalEndpoint,
    model: String,
    http_policy: HttpPolicy,
}

impl EnvLocalRerankClient {
    /// `model` is the server's `--served-model-name`, not the HuggingFace id.
    /// Getting it wrong is a **404**, measured (§5.3) — which is a
    /// configuration error surfacing as a network error, so it is worth
    /// stating that the two are the same field.
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

impl LocalRerankClient for EnvLocalRerankClient {
    fn rerank_documents(
        &self,
        query: &str,
        documents: &[String],
        top_n: usize,
    ) -> Result<Vec<(usize, f64)>> {
        let url = format!("{}/v1/rerank", self.endpoint.base_url());
        let response = authenticated_local_agent(&self.endpoint, self.http_policy)?
            .post(&url)
            .send_json(json!({
                "model": self.model,
                "query": query,
                "documents": documents,
                "top_n": top_n,
                "truncate_prompt_tokens": TRUNCATE_TO_MODEL_LIMIT,
            }))
            .map_err(rerank_http_error)
            .and_then(|response| require_success(response, rerank_http_status_error))?;
        let body = read_json_bounded(response, RERANK_RESPONSE_MAX_BYTES, "rerank response")?;
        parse_rerank_results(&body, documents.len())
    }
}

/// A local server has no credential and no invoice, so `Auth` and
/// `QuotaExceeded` cannot arise. A full queue is a retry; nothing else is.
fn rerank_http_error(error: ureq::Error) -> AdapterError {
    if is_local_peer_tls_error(&error) {
        return AdapterError::LocalPeerAuth(format!(
            "rerank server TLS authentication failed: {error}"
        ));
    }
    AdapterError::Network(format!("rerank server unreachable: {error}"))
}

fn rerank_http_status_error(response: &HttpResponse) -> AdapterError {
    match response.status().as_u16() {
        429 => {
            let retry_after_ms = response
                .headers()
                .get("Retry-After")
                .and_then(|value| value.to_str().ok())
                .and_then(crate::http_policy::parse_retry_after_ms);
            AdapterError::RateLimit {
                message: format!("rerank server queue is full ({})", response.status()),
                retry_after_ms,
            }
        }
        // 400 is the measured shape for a malformed request AND for a
        // candidate over `max_model_len` when truncation was not requested.
        // Both are permanent for this request, so neither is retried.
        code => AdapterError::Network(format!("rerank server returned HTTP {code}")),
    }
}

/// Reads `results[]`, keyed by `index` rather than by position.
///
/// An index outside the request, or a repeated one, is a contract violation:
/// either would let a server drop a candidate from the ranking while appearing
/// to return a full one.
fn parse_rerank_results(body: &Value, candidate_count: usize) -> Result<Vec<(usize, f64)>> {
    let results = body
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| violation("rerank response has no `results` array"))?;
    let mut seen = vec![false; candidate_count];
    let mut scored = Vec::with_capacity(results.len());
    for entry in results {
        let index = entry
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
            .ok_or_else(|| violation("rerank result has no integer `index`"))?;
        let slot = seen
            .get_mut(index)
            .ok_or_else(|| violation(format!("rerank result index {index} is not a candidate")))?;
        if *slot {
            return Err(violation(format!("rerank returned index {index} twice")));
        }
        *slot = true;
        let score = entry
            .get("relevance_score")
            .and_then(Value::as_f64)
            .ok_or_else(|| violation("rerank result has no numeric `relevance_score`"))?;
        scored.push((index, score));
    }
    Ok(scored)
}

fn violation(message: impl Into<String>) -> AdapterError {
    AdapterError::ContractViolation(message.into())
}

#[derive(Debug, Clone)]
enum Backend<C> {
    Mock,
    Real(C),
}

/// The Rerank adapter (07 §5.4).
#[derive(Debug, Clone)]
pub struct LocalRerankAdapter<C = EnvLocalRerankClient> {
    backend: Backend<C>,
    model: Option<RerankModel>,
}

impl LocalRerankAdapter<EnvLocalRerankClient> {
    /// The CI-only backend, with its own profile identity.
    ///
    /// CI has no GPU and never will, which is the same reason
    /// [`crate::local_embedding`] carries a mock. Its ordering is an arbitrary
    /// but stable permutation of the input — enough to prove the plumbing
    /// reorders, and deliberately not an approximation of relevance, so a
    /// mock-ranked result can never be mistaken for a measured one.
    #[must_use]
    pub fn mock() -> Self {
        Self {
            backend: Backend::Mock,
            model: None,
        }
    }
}

impl<C: LocalRerankClient> LocalRerankAdapter<C> {
    /// The measured backend. `model` is what the profile will declare.
    #[must_use]
    pub fn with_client(client: C, model: RerankModel) -> Self {
        Self {
            backend: Backend::Real(client),
            model: Some(model),
        }
    }
}

impl<C> LocalRerankAdapter<C> {
    #[must_use]
    fn execution(&self) -> LocalRerankExecution {
        match self.backend {
            Backend::Mock => LocalRerankExecution::Mock,
            Backend::Real(_) => LocalRerankExecution::Real,
        }
    }
}

/// A stable, arbitrary score for the mock. Not relevance — see
/// [`LocalRerankAdapter::mock`].
fn mock_score(query: &str, text: &str) -> f64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in query
        .bytes()
        .chain(b"\x00".iter().copied())
        .chain(text.bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    // Map into (0,1) so the mock cannot be mistaken for the unbounded scores
    // some real models emit, and so ordering is total.
    (hash >> 11) as f64 / (1u64 << 53) as f64
}

/// The profile JSON `tool_profile_hash` is taken over (03 §5.1).
///
/// Carries no serving backend, per 07 §5.3 (3)'s naming rule.
#[must_use]
pub fn profile_value_for(execution: LocalRerankExecution, model: Option<&RerankModel>) -> Value {
    let mut profile = json!({
        "adapter_kind": "rerank",
        "adapter_role": "cross_encoder",
        "runtime_kind": "local",
        "spec_version": 1
    });
    let fields = profile
        .as_object_mut()
        .expect("profile literal is an object");
    match execution {
        LocalRerankExecution::Mock => {
            fields.insert(
                "model_or_tool_family".to_owned(),
                json!("kio-local-rerank-mock"),
            );
            fields.insert(
                "model_version_pin".to_owned(),
                json!("kio-local-rerank-mock-1.0.0"),
            );
        }
        LocalRerankExecution::Real => {
            let model = model.expect("a real rerank backend always carries its model");
            fields.insert("model_or_tool_family".to_owned(), json!(model.family));
            fields.insert("model_version_pin".to_owned(), json!(model.version_pin));
        }
    }
    profile
}

/// The full declared profile for a backend.
#[must_use]
pub fn profile_for(execution: LocalRerankExecution, model: Option<&RerankModel>) -> AdapterProfile {
    let profile = profile_value_for(execution, model);
    AdapterProfile {
        adapter_kind: AdapterKind::Rerank,
        adapter_id: LOCAL_RERANK_ADAPTER_ID.to_owned(),
        execution_mode: ExecutionMode::OfflineApi,
        tool_profile_hash: tool_profile_hash(&profile)
            .expect("built-in local rerank profile is valid"),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        capability_flags: vec!["text".to_owned()],
        // 07 §3: loopback only, which is what exempts an offline_api adapter
        // from the consent gate. Must stay false.
        allow_network: false,
        // The model runs on hardware the user already has.
        billable_kinds: Vec::new(),
        reject_billing: None,
        provider_idempotency: crate::types::ProviderIdempotency::NotProvided,
    }
}

impl<C: LocalRerankClient> RerankAdapter for LocalRerankAdapter<C> {
    fn profile(&self) -> AdapterProfile {
        profile_for(self.execution(), self.model.as_ref())
    }

    fn rerank(&self, request: RerankRequest) -> Result<RerankResponse> {
        let profile_hash = self.profile().tool_profile_hash;
        if request.candidates.is_empty() {
            return Ok(RerankResponse {
                ranking: Vec::new(),
                rerank_profile_hash: Some(profile_hash),
            });
        }
        let top_n = request
            .top_n
            .unwrap_or(request.candidates.len())
            .min(request.candidates.len());

        let mut scored: Vec<(usize, f64)> = match &self.backend {
            Backend::Mock => request
                .candidates
                .iter()
                .enumerate()
                .map(|(index, candidate)| (index, mock_score(&request.query, &candidate.text)))
                .collect(),
            Backend::Real(client) => {
                let documents: Vec<String> = request
                    .candidates
                    .iter()
                    .map(|candidate| candidate.text.clone())
                    .collect();
                client.rerank_documents(&request.query, &documents, top_n)?
            }
        };

        // Sort here rather than trusting the server's order. The measured
        // server does return descending, but `index` is the authority and a
        // total order that ties on score must still be deterministic — so the
        // input subscript breaks ties.
        scored.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(left.0.cmp(&right.0))
        });
        scored.truncate(top_n);

        let ranking = scored
            .into_iter()
            .map(|(index, score)| RerankedCandidate {
                result_id: request.candidates[index].result_id.clone(),
                score,
            })
            .collect();
        Ok(RerankResponse {
            ranking,
            rerank_profile_hash: Some(profile_hash),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::RerankCandidate;

    fn candidates(texts: &[&str]) -> Vec<RerankCandidate> {
        texts
            .iter()
            .enumerate()
            .map(|(index, text)| RerankCandidate {
                result_id: format!("r{index}"),
                text: (*text).to_owned(),
            })
            .collect()
    }

    #[derive(Clone)]
    struct ScriptedClient {
        results: Vec<(usize, f64)>,
        last_top_n: std::sync::Arc<std::sync::Mutex<Option<usize>>>,
    }

    impl LocalRerankClient for ScriptedClient {
        fn rerank_documents(
            &self,
            _query: &str,
            _documents: &[String],
            top_n: usize,
        ) -> Result<Vec<(usize, f64)>> {
            *self.last_top_n.lock().unwrap() = Some(top_n);
            Ok(self.results.clone())
        }
    }

    fn scripted(
        results: Vec<(usize, f64)>,
    ) -> (LocalRerankAdapter<ScriptedClient>, ScriptedClient) {
        let client = ScriptedClient {
            results,
            last_top_n: std::sync::Arc::new(std::sync::Mutex::new(None)),
        };
        let adapter = LocalRerankAdapter::with_client(
            client.clone(),
            RerankModel {
                family: "bge-reranker-v2-m3".to_owned(),
                version_pin:
                    "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                        .to_owned(),
            },
        );
        (adapter, client)
    }

    /// The measured server returns `index` into the request, and this is the
    /// property that makes reading it (rather than the position) matter: a
    /// response listed in a different order must still produce the ranking the
    /// scores describe.
    #[test]
    fn the_ranking_follows_index_and_score_not_response_order() {
        let (adapter, _) = scripted(vec![(0, 0.1), (2, 0.9), (1, 0.5)]);
        let response = adapter
            .rerank(RerankRequest {
                query: "q".to_owned(),
                candidates: candidates(&["a", "b", "c"]),
                top_n: None,
            })
            .unwrap();
        let ids: Vec<&str> = response
            .ranking
            .iter()
            .map(|entry| entry.result_id.as_str())
            .collect();
        assert_eq!(ids, ["r2", "r1", "r0"]);
    }

    /// §5.2: the server echoes each returned candidate's text, so `top_n` is
    /// what keeps a 200-candidate rerank from returning 200 bodies. It must
    /// reach the wire even when the caller left it implicit.
    #[test]
    fn top_n_reaches_the_client_even_when_the_caller_omits_it() {
        let (adapter, client) = scripted(vec![(0, 0.5)]);
        adapter
            .rerank(RerankRequest {
                query: "q".to_owned(),
                candidates: candidates(&["a", "b", "c"]),
                top_n: None,
            })
            .unwrap();
        assert_eq!(*client.last_top_n.lock().unwrap(), Some(3));
    }

    #[test]
    fn top_n_is_clamped_to_the_candidates_offered() {
        let (adapter, client) = scripted(vec![(0, 0.5)]);
        let response = adapter
            .rerank(RerankRequest {
                query: "q".to_owned(),
                candidates: candidates(&["a"]),
                top_n: Some(200),
            })
            .unwrap();
        assert_eq!(*client.last_top_n.lock().unwrap(), Some(1));
        assert_eq!(response.ranking.len(), 1);
    }

    /// Scores are model-dependent and may be unbounded and negative
    /// (`japanese-reranker-base-v2` measured −11.2 … +4.5). Ordering must not
    /// assume a (0,1) range anywhere.
    #[test]
    fn unbounded_negative_scores_still_order_correctly() {
        let (adapter, _) = scripted(vec![(0, -11.187), (1, -8.079), (2, -12.955)]);
        let response = adapter
            .rerank(RerankRequest {
                query: "q".to_owned(),
                candidates: candidates(&["a", "b", "c"]),
                top_n: None,
            })
            .unwrap();
        let ids: Vec<&str> = response
            .ranking
            .iter()
            .map(|entry| entry.result_id.as_str())
            .collect();
        assert_eq!(ids, ["r1", "r0", "r2"]);
    }

    /// A server that drops a candidate while returning a full-looking list
    /// must not be reconciled into a plausible ranking.
    #[test]
    fn a_repeated_index_is_a_contract_violation() {
        let body = json!({"results": [
            {"index": 0, "relevance_score": 0.9},
            {"index": 0, "relevance_score": 0.1}
        ]});
        let error = parse_rerank_results(&body, 2).unwrap_err();
        assert!(
            matches!(error, AdapterError::ContractViolation(_)),
            "{error:?}"
        );
    }

    #[test]
    fn an_index_outside_the_request_is_a_contract_violation() {
        let body = json!({"results": [{"index": 7, "relevance_score": 0.9}]});
        let error = parse_rerank_results(&body, 2).unwrap_err();
        assert!(
            matches!(error, AdapterError::ContractViolation(_)),
            "{error:?}"
        );
    }

    /// The recorded response from `tasks/gpu-reranker-verification.md` §5.2,
    /// verbatim. If the parser and the measured wire ever disagree, this is
    /// where it shows.
    #[test]
    fn the_recorded_response_parses() {
        let body: Value = serde_json::from_str(
            r#"{"id":"score-b03bc1b9d3684578","model":"reranker","usage":{"prompt_tokens":226,"total_tokens":226},"results":[{"index":2,"document":{"text":"c","multi_modal":null},"relevance_score":1.66491972777294e-05},{"index":1,"document":{"text":"b","multi_modal":null},"relevance_score":1.6425787180196494e-05},{"index":0,"document":{"text":"a","multi_modal":null},"relevance_score":1.621661431272514e-05}]}"#,
        )
        .unwrap();
        let scored = parse_rerank_results(&body, 3).unwrap();
        assert_eq!(scored.len(), 3);
        assert_eq!(scored[0].0, 2);
    }

    /// The mock exists so CI can exercise the plumbing without a GPU, and it
    /// must never be mistakable for the measured backend.
    #[test]
    fn the_mock_declares_a_different_profile_than_a_real_model() {
        let mock = LocalRerankAdapter::mock().profile();
        let real = profile_for(
            LocalRerankExecution::Real,
            Some(&RerankModel {
                family: "bge-reranker-v2-m3".to_owned(),
                version_pin: "sha256:abc".to_owned(),
            }),
        );
        assert_ne!(mock.tool_profile_hash, real.tool_profile_hash);
        assert_eq!(mock.adapter_kind, AdapterKind::Rerank);
        assert!(
            !mock.allow_network,
            "a loopback adapter must not claim network"
        );
        assert!(
            mock.billable_kinds.is_empty(),
            "local inference bills nothing"
        );
    }

    /// 07 §5.3 (3): a backend name in the identity would make swapping the
    /// serving runtime look like a different reranker.
    #[test]
    fn the_profile_identity_names_no_serving_backend() {
        let profile = profile_value_for(
            LocalRerankExecution::Real,
            Some(&RerankModel {
                family: "bge-reranker-v2-m3".to_owned(),
                version_pin: "sha256:abc".to_owned(),
            }),
        );
        let rendered = profile.to_string().to_lowercase();
        for backend in [
            "vllm",
            "tei",
            "text-embeddings-inference",
            "sentence-transformers",
        ] {
            assert!(
                !rendered.contains(backend),
                "{backend} leaked into {rendered}"
            );
        }
    }

    #[test]
    fn an_empty_candidate_pool_is_an_empty_ranking_not_an_error() {
        let response = LocalRerankAdapter::mock()
            .rerank(RerankRequest {
                query: "q".to_owned(),
                candidates: Vec::new(),
                top_n: None,
            })
            .unwrap();
        assert!(response.ranking.is_empty());
        assert!(response.rerank_profile_hash.is_some());
    }

    /// Reordering only: every returned id must be one the caller supplied, so
    /// the caller can still own `searched_scopes` / `fallback_reason`
    /// (07 §5.4).
    #[test]
    fn the_mock_returns_only_ids_it_was_given() {
        let offered = candidates(&["a", "b", "c", "d"]);
        let response = LocalRerankAdapter::mock()
            .rerank(RerankRequest {
                query: "q".to_owned(),
                candidates: offered.clone(),
                top_n: None,
            })
            .unwrap();
        assert_eq!(response.ranking.len(), offered.len());
        for entry in &response.ranking {
            assert!(
                offered.iter().any(|c| c.result_id == entry.result_id),
                "{} was not offered",
                entry.result_id
            );
        }
    }

    #[test]
    fn the_mock_is_a_pure_function_of_its_input() {
        let request = RerankRequest {
            query: "q".to_owned(),
            candidates: candidates(&["a", "b", "c"]),
            top_n: None,
        };
        let first = LocalRerankAdapter::mock().rerank(request.clone()).unwrap();
        let second = LocalRerankAdapter::mock().rerank(request).unwrap();
        assert_eq!(first, second);
    }
}
