use kio_search::evidence::EvidencePointer;
use kio_search::object_uri::RelatedImage;
use kio_search::query::{DiversifyStrategy, SearchMode};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub query: String,
    pub requested_mode: SearchMode,
    pub resolved_mode: SearchMode,
    pub fallback: bool,
    pub fallback_reason: Option<String>,
    pub error_code: Option<String>,
    pub warnings: Vec<String>,
    pub diversify: DiversifySummary,
    pub aggregator: AggregatorResponse,
    pub paging: PagingResponse,
    pub searched_scopes: Vec<SearchedScopeResponse>,
    pub excluded_scopes: Vec<ExcludedScopeResponse>,
    pub index_status: IndexStatusResponse,
    pub results: Vec<SearchHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gc_recovery_pending: Option<SearchGcRecoveryPending>,
}

#[derive(Debug, Serialize)]
pub struct DiversifySummary {
    pub strategy: DiversifyStrategy,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mmr_lambda: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct AggregatorResponse {
    pub collection_generation: String,
}

#[derive(Debug, Serialize)]
pub struct PagingResponse {
    pub limit: u64,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SearchGcRecoveryPending {
    pub reason: String,
    pub next_cursor_suppressed: bool,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum SearchHit {
    Chunk(ChunkSearchHit),
    Image(ImageSearchHit),
}

#[derive(Debug, Serialize)]
pub struct ChunkSearchHit {
    pub result_type: String,
    pub chunk_hash: String,
    pub evidence_pointer: EvidencePointer,
    pub evidence_uri: String,
    pub score: f64,
    pub scope_path: PathBuf,
    pub title: String,
    pub snippet: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub related_images: Vec<RelatedImage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_paths: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ImageSearchHit {
    pub result_type: String,
    pub chunk_hash: String,
    pub evidence_pointer: EvidencePointer,
    pub evidence_uri: String,
    pub score: f64,
    pub scope_path: PathBuf,
    pub title: String,
    pub payload_uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_paths: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SearchedScopeResponse {
    pub scope_id: String,
    pub scope_path: PathBuf,
    pub snapshot_at: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub shallow_skipped: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Serialize)]
pub struct ExcludedScopeResponse {
    pub scope_id: String,
    pub scope_path: Option<PathBuf>,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidates: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct IndexStatusResponse {
    pub enriched_ratio: f64,
    pub total_enrichment_tasks: u64,
    pub skipped_enrichment_tasks: u64,
    pub skipped_enrichment_reasons: BTreeMap<String, u64>,
    pub pending_enrichment_tasks: u64,
    pub budget_status: String,
    pub budget_reason: Option<String>,
    pub budget_paused: Option<bool>,
    pub unsupported_inputs: Vec<FileDispositionResponse>,
    pub unsupported_input_errors: Vec<IndexStatusError>,
    pub unsupported_inputs_complete: bool,
    pub task_errors: Vec<IndexStatusError>,
    pub tasks_complete: bool,
    pub office_conversion_unavailable: OfficeConversionUnavailableResponse,
}

#[derive(Debug, Serialize)]
pub struct FileDispositionResponse {
    pub scope_path: PathBuf,
    pub path: String,
    pub raw_hash: String,
    pub media_type: String,
    pub size_bytes: u64,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexStatusError {
    pub scope_path: PathBuf,
    pub error_code: String,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct OfficeConversionUnavailableResponse {
    pub count: usize,
    pub files: Vec<FileDispositionResponse>,
}
