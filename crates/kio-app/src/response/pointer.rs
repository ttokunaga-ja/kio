use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum OpenResponse {
    Evidence(EvidenceOpenResponse),
    Raw(RawPointerResponse),
    ObjectChunk(ObjectChunkResponse),
    ObjectFile(ObjectFileResponse),
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ViewResponse {
    Evidence(EvidenceViewResponse),
    Raw(RawPointerResponse),
    ObjectChunk(ObjectChunkResponse),
    ObjectFile(ObjectFileResponse),
}

#[derive(Debug, Serialize)]
pub struct EvidenceOpenResponse {
    pub status: String,
    pub path: Option<PathBuf>,
    pub raw_hash: String,
    pub chunk_hash: String,
    pub temporary: bool,
    pub commit_shallow: bool,
    pub manifest_missing: bool,
}

#[derive(Debug, Serialize)]
pub struct EvidenceViewResponse {
    pub status: String,
    pub raw_hash: String,
    pub chunk_hash: String,
    pub view_path: Option<PathBuf>,
    pub view_byte_start: Option<u64>,
    pub view_byte_end: Option<u64>,
    pub path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temporary: Option<bool>,
    pub commit_shallow: bool,
    pub manifest_missing: bool,
}

#[derive(Debug, Serialize)]
pub struct RawPointerResponse {
    pub status: String,
    pub object_type: String,
    pub raw_hash: String,
    pub path: PathBuf,
    pub temporary: bool,
}

#[derive(Debug, Serialize)]
pub struct ObjectChunkResponse {
    pub status: String,
    pub object_type: String,
    pub hash: String,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct ObjectFileResponse {
    pub status: String,
    pub object_type: String,
    pub hash: String,
    pub path: PathBuf,
    pub temporary: bool,
}
