//! Existing Evidence response records and return assembly.

use crate::response::evidence::EvidenceResponse;
use crate::response::{CommandResponse, Completion, ResponseBody, project_legacy};
use kio_core::{ExitCode, Result};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStatus {
    Alive,
    Tombstoned,
    NotFound,
    ScopeUnreachable,
    Unverifiable,
    RegistryDuplicate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EvidenceVerifyResult {
    pub status: EvidenceStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    pub details: Value,
}

#[derive(Debug, Clone)]
pub(super) struct EvidenceVerification {
    pub(super) result: EvidenceVerifyResult,
    pub(super) exit: ExitCode,
}

impl EvidenceVerification {
    pub(super) fn new(
        status: EvidenceStatus,
        error_code: Option<&str>,
        details: Value,
        exit: ExitCode,
    ) -> Self {
        Self {
            result: EvidenceVerifyResult {
                status,
                error_code: error_code.map(str::to_owned),
                details,
            },
            exit,
        }
    }
}

pub(super) fn verification_output(verification: EvidenceVerification) -> Result<Value> {
    let completion = match verification.exit {
        ExitCode::Success => Completion::Complete,
        ExitCode::PartialFailure => Completion::Partial,
        ExitCode::PermanentFailure => Completion::Permanent,
        _ => {
            unreachable!("Evidence verification emits only success, retryable or permanent results")
        }
    };
    project_legacy(CommandResponse {
        body: ResponseBody::Evidence(EvidenceResponse::Verify(verification.result)),
        completion,
    })
}
#[cfg(test)]
mod evidence_response_tests;
