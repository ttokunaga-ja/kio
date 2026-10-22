//! Embedding execution metadata, separate from semantic profile identity.
use crate::types::{
    AdapterProfile, AdapterUsage, BillableUnitKind, EmbeddingVector, ExecutionMode,
    MAX_EMBEDDING_ITEM_BYTES, MAX_EMBEDDING_REQUEST_BYTES,
};
use crate::{AdapterError, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EmbeddingLogicalJobId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EmbeddingRemoteHandle(pub String);

impl EmbeddingRemoteHandle {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for EmbeddingRemoteHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EmbeddingUsage {
    Metered { usage: AdapterUsage },
    Estimated,
    Unmetered,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EmbeddingMemberResult {
    Vector { vector: Vec<f32> },
    Failed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingMemberOutcome {
    pub id: String,
    pub result: EmbeddingMemberResult,
}

impl From<EmbeddingVector> for EmbeddingMemberOutcome {
    fn from(vector: EmbeddingVector) -> Self {
        Self {
            id: vector.id,
            result: EmbeddingMemberResult::Vector {
                vector: vector.vector,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingCompleted {
    pub members: Vec<EmbeddingMemberOutcome>,
    pub dimensions: u32,
    pub distance: String,
    pub modality: String,
    pub embedding_profile_hash: String,
    pub usage: EmbeddingUsage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EmbeddingOutcome {
    Completed(EmbeddingCompleted),
    Pending {
        remote_handle: EmbeddingRemoteHandle,
    },
}

impl EmbeddingOutcome {
    /// Require completion at a synchronous-only call boundary.
    pub fn into_completed(self) -> Result<EmbeddingCompleted> {
        match self {
            Self::Completed(completed) => Ok(completed),
            Self::Pending { .. } => Err(AdapterError::ContractViolation(
                "synchronous embedding returned a pending outcome".to_owned(),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingEffects {
    pub egress: bool,
    pub paid_authorization: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingCapability {
    pub text: bool,
    pub image_mime_types: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingLimits {
    pub max_item_bytes: usize,
    pub max_request_bytes: usize,
    pub max_encoded_bytes: Option<usize>,
    pub max_tokens_per_member: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingCompletion {
    pub sync: bool,
    pub batch: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingPrice {
    pub kind: BillableUnitKind,
    pub standard_usd_per_unit: f64,
    pub batch_usd_per_unit: Option<f64>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingDescriptor {
    pub effects: EmbeddingEffects,
    pub capability: EmbeddingCapability,
    pub limits: EmbeddingLimits,
    pub completion: EmbeddingCompletion,
    pub pricing: Vec<EmbeddingPrice>,
}
impl EmbeddingDescriptor {
    /// Conservative declaration for custom implementations; built-ins refine it.
    #[must_use]
    pub fn from_profile(profile: &AdapterProfile) -> Self {
        Self {
            effects: EmbeddingEffects {
                egress: matches!(
                    profile.execution_mode,
                    ExecutionMode::OnlineApi | ExecutionMode::OfflineApi
                ),
                paid_authorization: !profile.billable_kinds.is_empty(),
            },
            capability: EmbeddingCapability {
                text: profile.capability_flags.iter().any(|flag| flag == "text"),
                image_mime_types: Vec::new(),
            },
            limits: EmbeddingLimits {
                max_item_bytes: MAX_EMBEDDING_ITEM_BYTES,
                max_request_bytes: MAX_EMBEDDING_REQUEST_BYTES,
                max_encoded_bytes: None,
                max_tokens_per_member: None,
            },
            completion: EmbeddingCompletion {
                sync: true,
                batch: false,
            },
            pricing: Vec::new(),
        }
    }
}

#[cfg(test)]
impl EmbeddingCompleted {
    pub(crate) fn vectors(&self) -> Vec<EmbeddingVector> {
        self.members
            .iter()
            .map(|member| match &member.result {
                EmbeddingMemberResult::Vector { vector } => EmbeddingVector {
                    id: member.id.clone(),
                    vector: vector.clone(),
                },
                EmbeddingMemberResult::Failed { reason } => {
                    panic!("expected successful vector: {reason}")
                }
            })
            .collect()
    }
}
