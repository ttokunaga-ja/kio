use super::Completion;
use kio_core::KioError;
use kio_core::gc::{
    GcInProgressMarker, GcIndexState, GcPlan, GcSweepPhase, UnreachableInventoryReport,
};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum GcResponse {
    Plan(GcPlan),
    Inventory(UnreachableInventoryReport),
    MarkerExchangePending(GcMarkerExchangePending),
    ExistingMarkerPending(GcExistingMarkerPending),
    Execution(GcExecutionReport),
}
#[derive(Debug, Serialize)]
pub struct GcMarkerExchangePending {
    pub status: String,
    pub reason: String,
}
#[derive(Debug, Serialize)]
pub struct GcExistingMarkerPending {
    pub status: String,
    pub recovery_pending: bool,
    pub marker: GcInProgressMarker,
    pub candidate_count: usize,
    pub candidate_tree_count: usize,
    pub estimated_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum GcExecutionReport {
    NoCandidates(GcNoCandidatesReport),
    Completed(GcCompletedReport),
    RuntimeDeferred(GcRuntimeDeferredReport),
}
#[derive(Debug, Clone, Serialize)]
pub struct GcNoCandidatesReport {
    pub status: String,
    pub reason: String,
    pub candidate_count: usize,
    pub candidate_tree_count: usize,
    pub estimated_bytes: u64,
}
#[derive(Debug, Clone, Serialize)]
pub struct GcCompletedReport {
    pub status: String,
    pub sweep_id: String,
    pub candidate_count: usize,
    pub candidate_tree_count: usize,
    pub estimated_bytes: u64,
    pub index_initial: GcIndexState,
    pub index_pre_sweep: Option<GcIndexState>,
    pub index_final: Option<GcIndexState>,
}
#[derive(Debug, Clone, Serialize)]
pub struct GcRuntimeDeferredReport {
    pub status: String,
    pub reason: String,
    pub recovery_pending: bool,
    pub sweep_id: String,
    pub phase: GcSweepPhase,
    pub candidate_count: usize,
    pub candidate_tree_count: usize,
    pub estimated_bytes: u64,
    pub index_initial: GcIndexState,
    pub index_pre_sweep: Option<GcIndexState>,
    pub index_final: Option<GcIndexState>,
}
#[derive(Debug, Clone, Serialize)]
pub struct AutomaticGcReport {
    #[serde(flatten)]
    pub execution: GcExecutionReport,
    pub trigger: String,
    pub mode: String,
    pub max_runtime_seconds: u64,
    pub recovered_before_publication: bool,
}
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum GcReport {
    AutomaticExecution(AutomaticGcReport),
    AutomationPolicy(GcAutomationPolicyReport),
    AttachmentFailure(GcAttachmentFailure),
    OnIdleSkip(GcOnIdleSkip),
    OnIdleFailure(GcOnIdleFailure),
    PreflightRecovered(GcPreflightRecovered),
}
impl GcReport {
    pub(crate) fn is_runtime_deferred(&self) -> bool {
        matches!(
            self,
            Self::AutomaticExecution(AutomaticGcReport {
                execution: GcExecutionReport::RuntimeDeferred(_),
                ..
            })
        )
    }
    pub(crate) fn has_no_candidates(&self) -> bool {
        matches!(
            self,
            Self::AutomaticExecution(AutomaticGcReport {
                execution: GcExecutionReport::NoCandidates(_),
                ..
            })
        )
    }
    pub(crate) fn recovery_pending(&self) -> Option<bool> {
        match self {
            Self::AutomaticExecution(AutomaticGcReport {
                execution: GcExecutionReport::RuntimeDeferred(report),
                ..
            }) => Some(report.recovery_pending),
            Self::OnIdleFailure(report) => Some(report.recovery_pending),
            _ => None,
        }
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct GcAutomationPolicyReport {
    pub status: String,
    pub reason: String,
    pub trigger: String,
    pub mode: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct GcAttachmentFailure {
    pub status: String,
    pub trigger: String,
    pub error: GcErrorDetail,
}
#[derive(Debug, Clone, Serialize)]
pub struct GcOnIdleSkip {
    pub status: String,
    pub reason: String,
    pub mode: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct GcOnIdleFailure {
    pub status: String,
    pub reason: String,
    pub mode: String,
    pub recovery_pending: bool,
    pub error: GcErrorDetail,
}
#[derive(Debug, Clone, Serialize)]
pub struct GcPreflightRecovered {
    pub status: String,
    pub trigger: String,
    pub mode: String,
    pub recovered_before_publication: bool,
}
#[derive(Debug, Clone, Serialize)]
pub struct GcErrorDetail {
    pub error_code: String,
    pub message: String,
    pub context: Value,
}
impl From<&KioError> for GcErrorDetail {
    fn from(error: &KioError) -> Self {
        Self {
            error_code: error.error_code().to_owned(),
            message: error.message().to_owned(),
            context: error.context().clone(),
        }
    }
}
#[derive(Debug, Serialize)]
pub struct PrepublicationDeferredResponse {
    pub status: String,
    pub publication_status: String,
    pub error_code: String,
    pub message: String,
    pub trigger: String,
    pub gc: GcReport,
}

pub(crate) struct GcAttachment {
    pub(crate) report: GcReport,
    pub(crate) publication_status: Option<String>,
    pub(crate) error_code: Option<String>,
    pub(crate) completion_override: Option<Completion>,
}
