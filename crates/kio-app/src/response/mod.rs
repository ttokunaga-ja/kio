use kio_core::{ExitCode, KioError, Result};
use serde::Serialize;
use serde_json::Value;

pub mod history;

#[derive(Debug)]
pub struct CommandResponse {
    pub body: ResponseBody,
    pub completion: Completion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    Complete,
    Partial,
    Permanent,
    AuthRequired,
    BudgetBlocked,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ResponseBody {
    Log(history::LogResponse),
    Diff(history::DiffResponse),
    Inspect(history::InspectResponse),
    Tag(history::TagResponse),
}

// Temporary C3 projection into the existing execution path; removed in C4
// when execute returns CommandResponse and the CLI owns serialization.
pub(crate) fn project_legacy(response: CommandResponse) -> Result<Value> {
    let mut value =
        serde_json::to_value(response.body).map_err(|err| KioError::schema(err.to_string()))?;
    let code = match response.completion {
        Completion::Complete => None,
        Completion::Partial => Some(ExitCode::PartialFailure),
        Completion::Permanent => Some(ExitCode::PermanentFailure),
        Completion::AuthRequired => Some(ExitCode::AuthError),
        Completion::BudgetBlocked => Some(ExitCode::BudgetExceeded),
    };
    if let Some(code) = code {
        crate::task_execution::set_exit_override(&mut value, code);
    }
    Ok(value)
}

#[cfg(test)]
mod history_tests;
