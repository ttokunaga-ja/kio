//! Existing Evidence response records and return assembly.

use crate::task_execution::set_exit_override;
use kio_core::{ExitCode, KioError, Result};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum EvidenceStatus {
    Alive,
    Tombstoned,
    NotFound,
    ScopeUnreachable,
    Unverifiable,
    RegistryDuplicate,
}

impl EvidenceStatus {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Alive => "alive",
            Self::Tombstoned => "tombstoned",
            Self::NotFound => "not_found",
            Self::ScopeUnreachable => "scope_unreachable",
            Self::Unverifiable => "unverifiable",
            Self::RegistryDuplicate => "registry_duplicate",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct EvidenceVerifyResult {
    pub(super) status: EvidenceStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) error_code: Option<String>,
    pub(super) details: Value,
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

    fn value(&self) -> Result<Value> {
        serde_json::to_value(&self.result).map_err(|error| KioError::schema(error.to_string()))
    }
}

pub(super) fn verification_output(verification: EvidenceVerification) -> Result<Value> {
    let mut output = verification.value()?;
    if verification.exit != ExitCode::Success {
        set_exit_override(&mut output, verification.exit);
    }
    Ok(output)
}
