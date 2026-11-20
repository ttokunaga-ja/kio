use std::path::PathBuf;

use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelReadiness {
    Ready,
    Missing,
    HashMismatch,
}

#[derive(Debug, Serialize)]
pub struct ModelResponse {
    pub model: &'static str,
    pub selected_profile: Option<String>,
    pub profile_hash: Option<String>,
    pub model_revision: &'static str,
    pub model_hash: &'static str,
    pub runtime_version: Option<String>,
    pub readiness: ModelReadiness,
    pub reason: &'static str,
    pub path: PathBuf,
    pub reembedding_count: Option<u64>,
    pub producers: Vec<ModelProducer>,
    pub mixed_producers: bool,
}

#[derive(Debug, Serialize)]
pub struct ModelProducer {
    pub os: String,
    pub arch: String,
    pub library_sha256: String,
    pub runtime_version: String,
    pub count: u64,
}
