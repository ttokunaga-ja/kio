use kio_core::dag::{CommitObject, CommitStats};
use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum SnapshotResponse {
    Create(SnapshotCreateResponse),
}

#[derive(Debug, Serialize)]
pub struct SnapshotCreateResponse {
    pub status: String,
    pub message: String,
    pub tree_hash: String,
    pub commit_hash: Option<String>,
    pub commit: Option<CommitObject>,
    pub stats: CommitStats,
}
