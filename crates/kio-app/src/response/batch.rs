use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum BatchResponse {
    Resume(BatchResumeResponse),
    Retry(BatchRetryResponse),
    Abandon(BatchSelectionResponse),
    ResetViolations(BatchSelectionResponse),
    ResendUnknown(BatchResendResponse),
}
#[derive(Debug, Serialize)]
pub struct BatchCounts {
    pub tasks_updated: usize,
    pub tasks_executed: usize,
    pub tasks_attempted: usize,
    pub tasks_failed: usize,
    pub tasks_paused: usize,
    pub tasks_inflight: usize,
    pub tasks_inflight_unreadable: usize,
}
#[derive(Debug, Serialize)]
pub struct BatchResumeResponse {
    pub status: String,
    pub recheck_budget: bool,
    pub override_budget: bool,
    #[serde(flatten)]
    pub counts: BatchCounts,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct BatchRetryResponse {
    pub status: String,
    #[serde(flatten)]
    pub counts: BatchCounts,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum BatchSelectionResponse {
    NoTarget(BatchNoTargetResponse),
    Selected(BatchSelectedResponse),
}
#[derive(Debug, Serialize)]
pub struct BatchNoTargetResponse {
    pub status: String,
}
#[derive(Debug, Serialize)]
pub struct BatchSelectedResponse {
    pub status: String,
    pub scope_id: String,
    pub adapter_kind: String,
    pub input_hash: String,
}
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum BatchResendResponse {
    NoTarget(BatchNoTargetResponse),
    Authorized(BatchResendAuthorizedResponse),
}
#[derive(Debug, Serialize)]
pub struct BatchResendAuthorizedResponse {
    pub status: String,
    pub authorization_created: bool,
    pub tasks_queued: usize,
    pub scope_id: String,
    pub adapter_kind: String,
    pub input_hash: String,
    pub next_step: String,
    pub warning: String,
}
#[derive(Debug, Serialize)]
pub struct StalledBatchStatus {
    pub scope_id: String,
    pub adapter_kind: String,
    pub input_hash: String,
    pub tool_profile_hash: String,
    pub intent_token: Option<String>,
    pub error: Option<String>,
    pub estimated_usd: f64,
    pub issued_at_ms: Option<i64>,
}
