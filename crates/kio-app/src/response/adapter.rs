use crate::grants::{GrantBinding, GrantRecord};
use crate::trust::TrustRecord;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum AdapterResponse {
    ApprovalPreview(ApprovalPreviewResponse),
    Approved(ApprovedResponse),
    Revoked(ApprovalRevokeResponse),
    Status(ApprovalStatusResponse),
    Trust(TrustResponse),
}
#[derive(Debug, Serialize)]
pub struct ApprovalPreviewResponse {
    pub status: String,
    pub scope_id: String,
    pub adapters: Vec<GrantBinding>,
    pub send_secrets: bool,
    pub resume: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct ApprovedResponse {
    pub status: String,
    pub approvals: Vec<ApprovalOutcome>,
}
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ApprovalOutcome {
    AlreadyApproved(AlreadyApproved),
    ResumedAlreadyApproved(ResumedAlreadyApproved),
    Approved(ApprovedApproval),
}
#[derive(Debug, Serialize)]
pub struct AlreadyApproved {
    pub tool_id: String,
    pub status: String,
}
#[derive(Debug, Serialize)]
pub struct ResumedAlreadyApproved {
    pub tool_id: String,
    pub status: String,
    pub grant_id: String,
}
#[derive(Debug, Serialize)]
pub struct ApprovedApproval {
    pub tool_id: String,
    pub status: String,
    pub grant_id: String,
    pub secrets_grant_id: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct ApprovalRevokeResponse {
    pub status: String,
    pub tool_id: Option<String>,
    pub all: bool,
    pub revoked_tool_ids: Vec<String>,
    pub revoked_grant_ids: Vec<String>,
    pub pending_removed: bool,
}
#[derive(Debug, Serialize)]
pub struct ApprovalStatusResponse {
    pub scope_id: String,
    pub grants: Vec<GrantRecord>,
    pub effective: Vec<EffectiveApproval>,
    // Existing persisted JSON is deliberately returned verbatim.
    pub scope_approvals: Vec<Value>,
    pub pending: Option<Value>,
}
#[derive(Debug, Serialize)]
pub struct EffectiveApproval {
    pub grant_id: String,
    pub permitted: bool,
    pub reason: Option<String>,
}
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum TrustResponse {
    Record(TrustRecordResponse),
    Unregistered(TrustUnregisteredResponse),
    Pending(TrustPendingResponse),
}
#[derive(Debug, Serialize)]
pub struct TrustRecordResponse {
    pub operation: String,
    pub trust: TrustRecord,
}
#[derive(Debug, Serialize)]
pub struct TrustUnregisteredResponse {
    pub operation: String,
    pub trust: String,
}
#[derive(Debug, Serialize)]
pub struct TrustPendingResponse {
    pub operation: String,
    pub trust: String,
    pub registration_id: String,
}
