use kio_core::purge::{PurgePhase, PurgeReason, TombstoneMode};
use kio_core::scope::{ManagedRestoreChange, ManagedRestoreOutcome};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
// Restore and repair share the same recovery projection and error records.
pub use super::repair::{
    ManagedRestoreProjection as RestoreProjection,
    ManagedRestoreProjectionError as RestoreProjectionError,
};

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ExportResponse {
    Exported(ExportSuccess),
    Partial(ExportPartial),
}
#[derive(Debug, Serialize)]
pub struct ExportSuccess {
    pub status: String,
    pub source_kind: String,
    pub source_commit: String,
    pub destination: PathBuf,
    pub exported_count: usize,
    pub overwritten_count: u64,
    pub files: Vec<ExportedFile>,
}
#[derive(Debug, Serialize)]
pub struct ExportPartial {
    #[serde(flatten)]
    pub report: ExportSuccess,
    pub error_code: String,
    pub failed: Vec<ExportFailure>,
}
#[derive(Debug, Serialize)]
pub struct ExportedFile {
    pub path: PathBuf,
    pub path_at_commit: String,
    pub raw_hash: String,
    pub overwritten: bool,
}
#[derive(Debug, Serialize)]
pub struct ExportFailure {
    pub path: PathBuf,
    pub path_at_commit: String,
    pub raw_hash: String,
    pub error_code: String,
    pub message: String,
}
#[derive(Debug, Serialize)]
pub struct RestoreResponse {
    pub operation: String,
    pub status: String,
    pub source_commit: String,
    pub expected_head: String,
    pub new_commit_hash: Option<String>,
    pub new_head: Option<String>,
    pub new_tree_hash: String,
    pub changes: Vec<ManagedRestoreChange>,
    pub provenance: RestoreProvenance,
    pub apply: Option<ManagedRestoreOutcome>,
    pub egress: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projection_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projection: Option<RestoreProjection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projection_error: Option<RestoreProjectionError>,
}
#[derive(Debug, Serialize)]
pub struct RestoreProvenance {
    pub source_commit: String,
    pub parent_commit: String,
    pub commit_type: String,
}
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum PurgeResponse {
    Purged(PurgeSuccess),
    Incomplete(PurgeIncomplete),
}
#[derive(Debug, Serialize)]
pub struct PurgeSuccess {
    pub status: String,
    pub purged_in_commit: Option<String>,
    pub reason: PurgeReason,
    pub target_raw_count: usize,
    pub deleted_counts: DeletedCounts,
    pub shared_artifacts_preserved: SharedArtifactsPreserved,
    pub tombstone_mode: TombstoneMode,
    pub tombstone_count: u64,
    pub erase_receipt_count: u64,
    pub logs_scrubbed: bool,
    pub log_files_scrubbed: u64,
    pub log_rows_removed: u64,
    pub log_fields_masked: u64,
    pub guarantee: String,
    pub not_covered: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_tree_warning: Option<WorkingTreeWarning>,
}
#[derive(Debug, Serialize)]
pub struct PurgeIncomplete {
    #[serde(flatten)]
    pub report: PurgeSuccess,
    pub error_code: String,
    pub message: String,
    pub failure_phase: PurgePhase,
    pub cause_error_code: String,
}
#[derive(Debug, Serialize)]
pub struct WorkingTreeWarning {
    pub live_alias_count: u64,
    pub message: String,
}
// Move these existing persisted-report records from purge.rs; retain Deserialize
// and all serde defaults. Do not keep duplicate canonical definitions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeletedCounts {
    pub raw_objects: u64,
    pub chunk_objects: u64,
    pub chunk_ledger_rows: u64,
    pub sqlite_chunks: u64,
    pub sqlite_associations: u64,
    pub sqlite_vectors: u64,
    pub sqlite_orphan_embeddings: u64,
    pub normalized_instances: u64,
    pub normalized_views: u64,
    pub prepared_objects: u64,
    pub image_objects: u64,
    pub tasks: u64,
    pub reservations: u64,
    pub manifest_rows: u64,
    pub unsupported_rows: u64,
    pub quarantine_rows: u64,
    pub cache_directories: u64,
    #[serde(default)]
    pub staging_descriptors: u64,
    #[serde(default)]
    pub manifest_objects: u64,
    #[serde(default)]
    pub normalized_unit_objects: u64,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SharedArtifactsPreserved {
    pub prepared_objects: u64,
    pub image_objects: u64,
    #[serde(default)]
    pub normalized_unit_objects: u64,
}
