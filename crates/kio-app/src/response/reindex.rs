use super::Completion;
use super::skipped_units::SkippedDisclosure;
use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ReindexResponse {
    Head(HeadReindexResponse),
    Historical(HistoricalReindexResponse),
}

pub(crate) struct ReindexExecution {
    pub(crate) response: ReindexResponse,
    pub(crate) completion: Completion,
}

#[derive(Debug, Serialize)]
pub struct HeadReindexResponse {
    pub status: String,
    pub reindexed_files: u64,
    pub commit_hash: Option<String>,
    pub rebuilt_chunks: u64,
    pub embedding_tasks_executed: usize,
    pub embedding_tasks_failed: usize,
    pub paused_tasks: usize,
    #[serde(flatten)]
    pub skipped: SkippedDisclosure,
}

#[derive(Debug, Serialize)]
pub struct HistoricalReindexResponse {
    pub status: String,
    pub snapshot_at: String,
    pub head_commit: String,
    pub reindexed_files: u64,
    pub rebuilt_chunks: u64,
    pub embedding_tasks_executed: usize,
    pub embedding_tasks_failed: usize,
    pub paused_tasks: usize,
    pub blocked_raw_hashes: usize,
    #[serde(flatten)]
    pub skipped: SkippedDisclosure,
}
