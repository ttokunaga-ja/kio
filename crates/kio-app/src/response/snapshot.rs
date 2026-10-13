use super::gc::{GcReport, PrepublicationDeferredResponse};
use kio_core::dag::{CommitObject, CommitStats};
use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum SnapshotResponse {
    Create(SnapshotCreateReport),
    Auto(SnapshotAutoResponse),
    Deferred(PrepublicationDeferredResponse),
}

#[derive(Debug, Serialize)]
pub struct SnapshotCreateResponse {
    pub status: String,
    pub message: String,
    pub tree_hash: String,
    pub commit_hash: Option<String>,
    pub commit: Option<CommitObject>,
    pub stats: CommitStats,
}

#[derive(Debug, Serialize)]
pub struct SnapshotCreateReport {
    #[serde(flatten)]
    pub base: SnapshotCreateResponse,
    pub gc: GcReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publication_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct SnapshotAutoResponse {
    pub operation: String,
    pub status: String,
    pub reason: String,
    pub publication_status: String,
    pub snapshot_status: String,
    pub eligibility_reason: Option<String>,
    pub eligible: bool,
    pub change_count: Option<u64>,
    pub next_eligible_at: Option<String>,
    pub commit_hash: Option<String>,
    pub tree_hash: Option<String>,
    pub stats: Option<CommitStats>,
    pub working_set_digest: Option<String>,
    pub idle_observed_since: Option<String>,
    pub idle_observed_seconds: Option<u64>,
    pub idle_threshold_seconds: Option<u64>,
    pub idle_eligible: bool,
    pub recovered_gc: bool,
    pub recovery_pending: bool,
    pub gc: Option<GcReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}
