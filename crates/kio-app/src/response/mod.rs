use serde::Serialize;

pub mod adapter;
pub mod batch;
pub mod device;
pub mod evidence;
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
pub mod status;
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
    Status(Box<status::StatusResponse>),
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
    Evidence(evidence::EvidenceResponse),
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

#[cfg(test)]
mod status_tests;

#[cfg(test)]
mod typed_api_tests;
