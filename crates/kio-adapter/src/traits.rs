//! Adapter trait groups.

use crate::Result;
use crate::types::{
    AdapterProfile, EmbeddingOutcome, EmbeddingRequest, MarkdownizeRequest, MarkdownizeResponse,
    PrepareRequest, PrepareResponse, RerankRequest, RerankResponse,
};

pub trait PrepareAdapter {
    fn profile(&self) -> AdapterProfile;

    fn prepare(&self, request: PrepareRequest) -> Result<PrepareResponse>;
}

pub trait MarkdownizeAdapter {
    fn profile(&self) -> AdapterProfile;

    fn markdownize(&self, request: MarkdownizeRequest) -> Result<MarkdownizeResponse>;

    /// Which request lane this adapter's online sends should take (07 §5.5).
    /// Default = Sync (single request/response round trip). The built-in
    /// Mistral OCR adapter overrides this to Batch: the 2026-07-23 user
    /// ruling permits OCR spending on the Batch lane only ($2/1,000 pages),
    /// so its production sends must never take the sync lane. A trait
    /// default (not an `AdapterProfile` field) keeps the lane OUT of
    /// identity — same posture as `ProviderIdempotency` (QA13).
    fn preferred_request_kind(&self) -> PreferredRequestKind {
        PreferredRequestKind::Sync
    }
}

/// Online send lane selector (07 §5.5; ledger `batch_requests.request_kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreferredRequestKind {
    Sync,
    Batch,
}

pub trait EmbeddingAdapter {
    fn profile(&self) -> AdapterProfile;

    fn descriptor(&self) -> crate::types::EmbeddingDescriptor {
        crate::types::EmbeddingDescriptor::from_profile(&self.profile())
    }

    fn embed(&self, request: EmbeddingRequest) -> Result<EmbeddingOutcome>;

    /// Which request lane this adapter's online sends should take (07 §5.3 の
    /// 2026-07-24 訂正 / §5.7). Default = Sync, same as
    /// [`MarkdownizeAdapter::preferred_request_kind`]. The built-in Gemini
    /// adapter overrides this to Batch: the Gemini Developer API bills an
    /// embedding batch at half the sync rate ($0.10 vs $0.20 per 1M text
    /// tokens), and the earlier "Vertex はバッチ推論非対応" rationale did not
    /// apply to the endpoint this adapter actually calls. Like the markdownize
    /// selector, a trait default (not an `AdapterProfile` field) keeps the lane
    /// OUT of identity — the same vectors come back either way.
    fn preferred_request_kind(&self) -> PreferredRequestKind {
        PreferredRequestKind::Sync
    }
}

/// 07 §5.4 (optional). Reorders search results and nothing else.
///
/// There is no batch method and no lane selector, because the unit of work is
/// already a batch: one query against one candidate pool. Splitting a pool
/// across calls would ask a cross-encoder to compare candidates it never saw
/// together, which is the one thing it exists to do.
pub trait RerankAdapter {
    fn profile(&self) -> AdapterProfile;

    /// Returns the candidates in descending relevance.
    ///
    /// **Reordering only.** 07 §5.4 forbids a reranker from concealing
    /// `searched_scopes` / `fallback_reason`, and the trait keeps that
    /// enforceable by never handing those fields to the adapter: it sees
    /// opaque `result_id`s, so it has nothing to conceal them with. A caller
    /// that cannot map every returned id back to a result it supplied should
    /// treat that as a contract violation rather than reconcile it.
    fn rerank(&self, request: RerankRequest) -> Result<RerankResponse>;
}

#[cfg(test)]
mod tests {
    use crate::deterministic::DeterministicAdapter;
    use crate::types::{AdapterKind, ExecutionMode};

    use super::PrepareAdapter;

    #[test]
    fn placeholder_prepare_trait_exposes_profile() {
        let adapter = DeterministicAdapter;
        let profile = PrepareAdapter::profile(&adapter);

        assert_eq!(profile.adapter_kind, AdapterKind::Prepare);
        assert_eq!(profile.execution_mode, ExecutionMode::DeterministicLibrary);
    }
}
