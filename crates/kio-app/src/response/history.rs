use kio_core::dag::{CommitObject, TreeObject};
use kio_core::scope::{DiffEntry, LogEntry};
use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Serialize)]
pub struct LogResponse {
    pub commits: Vec<LogEntry>,
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct DiffResponse {
    pub changes: Vec<DiffEntry>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum InspectResponse {
    Tree(TreeObject),
    Commit(CommitObject),
    Raw(RawInspection),
}

#[derive(Debug, Serialize)]
pub struct RawInspection {
    pub object_type: String,
    pub raw_hash: String,
    pub size_bytes: u64,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum TagResponse {
    Set(TagSetResponse),
    Delete(TagDeleteResponse),
}

#[derive(Debug, Serialize)]
pub struct TagSetResponse {
    pub tag: String,
    pub commit_hash: String,
    pub path: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct TagDeleteResponse {
    pub operation: String,
    pub status: String,
    pub tag: String,
    pub commit_hash: String,
    pub path: PathBuf,
}
