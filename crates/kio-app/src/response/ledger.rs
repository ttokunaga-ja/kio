use kio_pipeline::ledger::PendingWriteRecovery;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum LedgerResponse {
    Initialized(LedgerPathResponse),
    Status(LedgerPathResponse),
    Backup(LedgerPathResponse),
    Restore(LedgerPathResponse),
    Recover(LedgerRecoverResponse),
    Reconcile(LedgerReconcileResponse),
}
#[derive(Debug, Serialize)]
pub struct LedgerPathResponse {
    pub status: String,
    pub path: PathBuf,
}
#[derive(Debug, Serialize)]
pub struct LedgerRecoverResponse {
    pub status: String,
    pub recovery: PendingWriteRecovery,
    pub path: PathBuf,
}
#[derive(Debug, Serialize)]
pub struct LedgerReconcileResponse {
    pub status: String,
    pub integrity: String,
    pub sync_settled: u64,
    pub batch_found: u64,
    pub batch_found_keys: Vec<String>,
    pub batch_settled_unknown: u64,
    pub batch_settled_unknown_keys: Vec<String>,
    pub unlistable: u64,
    pub unlistable_keys: Vec<String>,
    pub phase2b_not_started: u64,
    pub phase2b_not_started_keys: Vec<String>,
    pub orphans: Vec<OrphanJob>,
    pub unknown: Vec<UnknownJob>,
    pub unknown_uploads: Vec<UnknownUpload>,
}
#[derive(Debug, Serialize)]
pub struct AttributionTaskKey {
    pub scope_id: String,
    pub adapter_kind: String,
    pub input_hash: String,
    pub tool_profile_hash: String,
}
#[derive(Debug, Serialize)]
pub struct OrphanGuidance {
    pub result_fetch: String,
    pub deletion: String,
}
#[derive(Debug, Serialize)]
pub struct OrphanJob {
    pub job_id: String,
    pub task_key: AttributionTaskKey,
    pub guidance: OrphanGuidance,
}
#[derive(Debug, Serialize)]
pub struct UnknownJob {
    pub job_id: String,
    pub task_key: Option<AttributionTaskKey>,
    pub reason: String,
}
#[derive(Debug, Serialize)]
pub struct UnknownUpload {
    pub upload_id: String,
    pub filename_token: Option<String>,
}
