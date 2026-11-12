use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Serialize)]
pub struct InitResponse {
    pub status: String,
    pub repaired: Vec<String>,
    pub path: PathBuf,
    pub kio_path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub independent: Option<IndependentInitResponse>,
}

#[derive(Debug, Serialize)]
pub struct IndependentInitResponse {
    pub parent: Option<PathBuf>,
    pub parent_relationship: String,
    pub confirmed: bool,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum RootRegisterResponse {
    Preview(RootRegisterPreviewResponse),
    Applied(RootRegisterAppliedResponse),
}

#[derive(Debug, Serialize)]
pub struct RootRegisterPreviewResponse {
    pub operation: String,
    pub operation_id: Option<String>,
    pub path: PathBuf,
    pub scope_id: String,
    pub preview: bool,
    pub requires_yes: bool,
    pub detaches_from_former_parent: bool,
    pub scopes_registered: usize,
    pub child_memberships_retired: usize,
    pub retired_children: Vec<RetiredChild>,
    pub effects: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct RetiredChild {
    pub path: PathBuf,
    pub scope_id: String,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct RootRegisterAppliedResponse {
    pub status: String,
    pub operation_id: String,
    pub scope_id: String,
    pub path: PathBuf,
    pub registration_generation: u64,
    pub scopes_registered: usize,
    pub child_memberships_retired: usize,
    pub grants_revoked: bool,
}
