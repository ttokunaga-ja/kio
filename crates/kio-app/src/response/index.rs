use super::gc::{GcReport, PrepublicationDeferredResponse};
use super::skipped_units::SkippedDisclosure;
use kio_core::dag::CommitObject;
use kio_pipeline::scan::{ChildScopeDiscovery, CostPreview, ScanCandidate};
use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum IndexResponse {
    Preview(IndexPreviewResponse),
    Published(Box<IndexPublishedResponse>),
    Deferred(PrepublicationDeferredResponse),
}

#[derive(Debug, Serialize)]
pub struct IndexPreviewResponse {
    pub status: String,
    pub root: PathBuf,
    pub scope: String,
    pub estimated_file_count: usize,
    pub estimated_size_bytes: u64,
    pub large_files: Vec<LargeFile>,
    pub effective_ignore: String,
    pub excluded_candidates: Vec<String>,
    pub sensitive_candidates: Vec<SensitiveCandidate>,
    pub network_transmission_policy: NetworkTransmissionPolicy,
    pub adapter_execution_mode: AdapterExecutionMode,
    pub estimated_cost: Option<CostPreview>,
    pub budget_cap: Option<f64>,
    pub estimated_completion: String,
    pub candidates: Vec<ScanCandidate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_scopes: Option<Vec<ChildPreviewRow>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_aggregate_file_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_aggregate_size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_aggregate_cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_scope_discovery: Option<PreviewChildDiscovery>,
}

#[derive(Debug, Serialize)]
pub struct LargeFile {
    pub path: String,
    pub size_bytes: u64,
}
#[derive(Debug, Serialize)]
pub struct SensitiveCandidate {
    pub path: String,
    pub reason: String,
    pub ignored: bool,
}
#[derive(Debug, Serialize)]
pub struct NetworkTransmissionPolicy {
    pub default: String,
    pub yes_grants_network: bool,
}
#[derive(Debug, Serialize)]
pub struct AdapterExecutionMode {
    pub markdownize: String,
    pub embedding: String,
}
#[derive(Debug, Serialize)]
pub struct ChildPreviewRow {
    #[serde(flatten)]
    pub discovery: ChildScopeDiscovery,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_file_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
}
#[derive(Debug, Serialize)]
pub struct PreviewChildDiscovery {
    pub page_size: usize,
    pub has_more: bool,
    pub preview: bool,
}
#[derive(Debug, Serialize)]
pub struct PublishedChildDiscovery {
    pub page_size: usize,
    pub has_more: bool,
    pub continuation_committed: bool,
    pub restarted_from_root: bool,
    pub rows_total: u64,
    pub rows_omitted: u64,
}
#[derive(Debug, Serialize)]
pub struct ChildPublishedRow {
    #[serde(flatten)]
    pub discovery: ChildScopeDiscovery,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gc: Option<GcReport>,
}

#[derive(Debug, Serialize)]
pub struct IndexPublishedResponse {
    pub status: String,
    pub approval_method: String,
    pub network_allowed: bool,
    pub network_opt_in: bool,
    pub pending_online_tasks: usize,
    pub paused_tasks: usize,
    pub failed_files: usize,
    pub normalized_files: usize,
    pub pending_files: usize,
    pub skipped_oversized_files: usize,
    pub skipped_unrecognized_binary_files: usize,
    pub embedding_tasks_executed: usize,
    pub embedding_tasks_failed: usize,
    pub tree_hash: String,
    pub commit_hash: Option<String>,
    pub commit: Option<CommitObject>,
    pub budget_warning: Option<String>,
    #[serde(flatten)]
    pub skipped: SkippedDisclosure,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gc: Option<GcReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publication_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_scopes: Option<Vec<ChildPublishedRow>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_scope_discovery: Option<PublishedChildDiscovery>,
}

impl IndexResponse {
    pub(crate) fn gc(&self) -> Option<&GcReport> {
        match self {
            Self::Published(report) => report.gc.as_ref(),
            Self::Deferred(report) => Some(&report.gc),
            Self::Preview(_) => None,
        }
    }
    pub(crate) fn error_code(&self) -> Option<&str> {
        match self {
            Self::Published(report) => report.error_code.as_deref(),
            Self::Deferred(report) => Some(&report.error_code),
            Self::Preview(_) => None,
        }
    }
    pub(crate) fn message(&self) -> Option<&str> {
        match self {
            Self::Deferred(report) => Some(&report.message),
            Self::Published(_) | Self::Preview(_) => None,
        }
    }
}
