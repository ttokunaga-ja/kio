use super::skipped_units::{SkippedDisclosure, SkippedUnit};
use crate::verify_objects::{PruneOrphansReport, RegistryPruneReport, VerifyObjectsReport};
use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum RepairResponse {
    RebuildDb(RepairRebuildResponse),
    VerifyObjects(RepairVerifyResponse),
    PruneBlocked(RepairPruneBlockedResponse),
    RegistryPrune(RegistryPruneReport),
    CancelChildInitialization(CancelChildInitializationResponse),
    RecoverRestore(ManagedRestoreRecoveryResponse),
    Device(DeviceRepairResponse),
}
#[derive(Debug, Serialize)]
pub struct RepairRebuildResponse {
    pub status: String,
    pub rebuilt_chunks: u64,
    pub rebuilt_tree_entries: u64,
    pub sqlite_db: PathBuf,
    pub embedding_tasks_executed: usize,
    pub embedding_tasks_failed: usize,
    pub paused_tasks: usize,
    #[serde(flatten)]
    pub skipped: SkippedDisclosure,
}
#[derive(Debug, Serialize)]
pub struct RepairVerifyResponse {
    #[serde(flatten)]
    pub report: VerifyObjectsReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prune_orphans: Option<PruneOrphansReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct RepairPruneBlockedResponse {
    pub prune_orphans: PruneOrphansReport,
    pub error_code: String,
}
#[derive(Debug, Serialize)]
pub struct DeviceRepairResponse {
    pub status: String,
    pub operation: String,
    pub repaired_scopes: Vec<RepairedScope>,
    pub failed_scopes: Vec<FailedScope>,
    pub summary: RepairSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct RepairSummary {
    pub repaired: usize,
    pub failed: usize,
}
#[derive(Debug, Serialize)]
pub struct RepairedScope {
    pub scope_id: String,
    pub path: String,
    pub status: String,
    pub result: ScopeRepairResult,
}
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ScopeRepairResult {
    All(ScopeRepairAll),
    Replica(ScopeRepairReplica),
}
#[derive(Debug, Serialize)]
pub struct ScopeRepairAll {
    #[serde(flatten)]
    pub rebuild: RepairRebuildResponse,
    pub replica: String,
}
#[derive(Debug, Serialize)]
pub struct ScopeRepairReplica {
    pub status: String,
}
#[derive(Debug, Serialize)]
pub struct FailedScope {
    pub scope_id: String,
    pub path: String,
    pub stage: String,
    pub reason: String,
    pub error_code: String,
    pub message: String,
    pub context: Value,
    pub exit_code: i32,
}
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum CancelChildInitializationResponse {
    Preview(CancelChildPreview),
    Applied(CancelChildApplied),
}
#[derive(Debug, Serialize)]
pub struct CancelChildPreview {
    pub preview: bool,
    pub operations: Vec<CancelChildRow>,
    pub pending_cancellation: Option<PendingCancellation>,
}
#[derive(Debug, Serialize)]
pub struct CancelChildRow {
    pub operation: String,
    pub path: PathBuf,
    pub stage: PathBuf,
    pub cancelable: bool,
    pub reason: Option<String>,
    pub current_policy_allows: Option<bool>,
}
#[derive(Debug, Serialize)]
pub struct PendingCancellation {
    pub operation: String,
    pub phase: String,
}
#[derive(Debug, Serialize)]
pub struct CancelChildApplied {
    pub operation: String,
    pub cancelled: bool,
    pub resumed: bool,
}
// These DTOs also cover the recovery branch consumed by C3e Restore.
// Keep one canonical definition when that candidate is installed.
#[derive(Debug, Serialize)]
pub struct ManagedRestoreRecoveryResponse {
    pub operation: String,
    pub status: String,
    pub recovered: bool,
    pub egress: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projection_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projection: Option<ManagedRestoreProjection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projection_error: Option<ManagedRestoreProjectionError>,
}
#[derive(Debug, Serialize)]
pub struct ManagedRestoreProjection {
    pub status: String,
    pub rebuilt_chunks: u64,
    pub rebuilt_tree_entries: u64,
    pub skipped_units: Vec<SkippedUnit>,
    pub egress: bool,
}
#[derive(Debug, Serialize)]
pub struct ManagedRestoreProjectionError {
    pub error_code: String,
    pub message: String,
}
