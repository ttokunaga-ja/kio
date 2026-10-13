use kio_core::{ExitCode, KioError, Result};
use serde::Serialize;
use serde_json::Value;

pub mod adapter;
pub mod batch;
pub mod device;
pub mod gc;
pub mod history;
pub mod index;
pub mod ledger;
pub mod mutation;
pub mod pointer;
pub mod reindex;
pub mod repair;
pub mod search;
pub mod skipped_units;
pub mod snapshot;
pub mod watch;

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
    Export(mutation::ExportResponse),
    Restore(mutation::RestoreResponse),
    Purge(mutation::PurgeResponse),
    Watch(watch::WatchResponse),
    Batch(batch::BatchResponse),
    Adapter(adapter::AdapterResponse),
    Ledger(ledger::LedgerResponse),
    Repair(repair::RepairResponse),
    Index(index::IndexResponse),
    Reindex(reindex::ReindexResponse),
    Gc(gc::GcResponse),
    Init(device::InitResponse),
    RootRegister(device::RootRegisterResponse),
    Snapshot(snapshot::SnapshotResponse),
    Log(history::LogResponse),
    Diff(history::DiffResponse),
    Inspect(history::InspectResponse),
    Tag(history::TagResponse),
    Open(pointer::OpenResponse),
    View(pointer::ViewResponse),
    Search(search::SearchResponse),
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
mod device_snapshot_tests;
#[cfg(test)]
mod history_tests;
#[cfg(test)]
mod pointer_tests;

#[cfg(test)]
mod search_tests;

#[cfg(test)]
mod index_tests;
#[cfg(test)]
mod inventory_tests;
#[cfg(test)]
mod reindex_skipped_tests;
#[cfg(test)]
mod snapshot_gc_tests;

#[cfg(test)]
mod c3d_completion_tests;
#[cfg(test)]
mod c3d_tests;

#[cfg(test)]
mod mutation_watch_tests;
