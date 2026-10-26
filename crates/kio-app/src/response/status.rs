use super::batch::StalledBatchStatus;
use kio_core::gc::GcInProgressMarker;
use kio_core::purge::PurgeJournal;
use kio_core::scope::FileStatus;
use kio_pipeline::budget::BudgetCapKind;
use kio_pipeline::task::TaskDescriptor;
use kio_pipeline::unsupported::UnsupportedInputDisposition;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub scope_path: PathBuf,
    pub files: Vec<FileStatus>,
    pub head_shallow: bool,
    pub paused_by_hold_reason: PausedTasksByHoldReason,
    pub tasks: Vec<TaskDescriptor>,
    // The status reader returns every valid JSON row verbatim.
    pub quarantine: Vec<Value>,
    pub unsupported_inputs: Vec<UnsupportedInputDisposition>,
    pub unsupported_inputs_complete: bool,
    pub budget: BudgetStatusResponse,
    pub stalled_batch: Vec<StalledBatchStatus>,
    pub active_purge_journal: Option<PurgeJournal>,
    pub active_gc_sweep: Option<GcInProgressMarker>,
}

#[derive(Debug, Serialize)]
pub struct PausedTasksByHoldReason {
    pub budget: u64,
    pub auth: u64,
    pub tier_b_approval: u64,
    pub ledger_initialization_required: u64,
}

#[derive(Debug, Serialize)]
pub struct BudgetStatusResponse {
    pub budget_status: String,
    pub budget_reason: Option<String>,
    pub month: String,
    pub device_monthly_usd_cap: f64,
    pub folder_monthly_usd_cap: Option<f64>,
    pub device_spent_usd: Option<f64>,
    pub folder_spent_usd: Option<f64>,
    pub device_remaining_usd: Option<f64>,
    pub folder_remaining_usd: Option<f64>,
    pub cap_kind: Option<BudgetCapKind>,
    pub device_per_adapter: BTreeMap<String, f64>,
    pub hard_stop: bool,
    pub warn_at_percent: u8,
    pub warned: bool,
    pub warning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ledger_status: Option<String>,
}
