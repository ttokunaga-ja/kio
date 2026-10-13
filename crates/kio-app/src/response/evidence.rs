pub use crate::verify_objects::response::{EvidenceStatus, EvidenceVerifyResult};
use kio_search::evidence::EvidencePointer;
use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum EvidenceResponse {
    Verify(EvidenceVerifyResult),
    Batch(EvidenceBatchResponse),
    Retarget(Box<EvidenceRetargetResponse>),
}

#[derive(Debug, Serialize)]
pub struct EvidenceBatchResponse {
    pub schema: String,
    pub schema_version: u64,
    pub input_sha256: String,
    pub strict: bool,
    pub results: Vec<EvidenceBatchRow>,
    pub summary: EvidenceBatchSummary,
    pub verified_count: usize,
}

#[derive(Debug, Serialize)]
pub struct EvidenceBatchRow {
    pub line: usize,
    pub result: EvidenceVerifyResult,
}

#[derive(Debug, Serialize)]
pub struct EvidenceBatchSummary {
    pub total: usize,
    pub status_counts: EvidenceStatusCounts,
}

#[derive(Debug, Default, Serialize)]
pub struct EvidenceStatusCounts {
    pub alive: usize,
    pub tombstoned: usize,
    pub not_found: usize,
    pub scope_unreachable: usize,
    pub unverifiable: usize,
    pub registry_duplicate: usize,
}

impl EvidenceStatusCounts {
    pub(crate) fn record(&mut self, status: EvidenceStatus) {
        match status {
            EvidenceStatus::Alive => self.alive += 1,
            EvidenceStatus::Tombstoned => self.tombstoned += 1,
            EvidenceStatus::NotFound => self.not_found += 1,
            EvidenceStatus::ScopeUnreachable => self.scope_unreachable += 1,
            EvidenceStatus::Unverifiable => self.unverifiable += 1,
            EvidenceStatus::RegistryDuplicate => self.registry_duplicate += 1,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct EvidenceRetargetResponse {
    pub schema: String,
    pub schema_version: u64,
    pub status: String,
    pub target_commit: String,
    pub retargeted_from: EvidencePointer,
    pub new_pointer: EvidencePointer,
    pub match_method: String,
}

pub(crate) fn batch_completion(
    strict: bool,
    permanent: bool,
    retryable: bool,
) -> crate::response::Completion {
    if strict && permanent {
        crate::response::Completion::Permanent
    } else if retryable {
        crate::response::Completion::Partial
    } else {
        crate::response::Completion::Complete
    }
}
