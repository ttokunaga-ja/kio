//! Fixed EG2 semantic profile, shared by the worker, adapter and CAS reader.

use crate::{cas, litert};
use serde_json::{Value, json};
use std::sync::LazyLock;

pub const TOOL_ID: &str = "embeddinggemma_2";
pub const DIMENSIONS: usize = 768;
pub const TOKEN_CAP: u32 = 2048;
pub const QUERY_PROMPT: &str = "task: search result | query: ";
pub const DOCUMENT_PROMPT: &str = "title: none | text: ";
pub const MAX_BODY_CHARS: usize = crate::dag::DEFAULT_CHUNKING_MAX_CHARS as usize;
pub const MAX_DEPTH: u8 = 32;
pub const C_API_VERSION: &str = "1.0.0";
pub const NORMALIZE: bool = true;
pub const INSERT_SPECIAL_TOKENS: bool = true;
pub const PNG_TRANSCODE_RECIPE: &str = "kio_image_png_v1";

pub const MODEL: &str = "embeddinggemma-2";
pub const REVISION: &str = "e301f74d5551b0c2641bd5cb4652a76239d5c5f8";
pub const MODEL_SHA256: &str = "92dcbea108899e5d6e30d919b0744f90d9967e80c67a4ab5503ac16d54f62eb0";

/// The semantic identity is shared by every supported producer. The archive
/// digest binds the complete set of platform artifacts.
#[must_use]
pub fn profile_value() -> Value {
    json!({
        "adapter_kind": "embedding",
        "adapter_role": "multimodal",
        "model_or_tool_family": MODEL,
        "model_version_pin": format!("sha256:{}", MODEL_SHA256),
        "prompt_template_id": "eg2_retrieval_v1",
        "prompt_template_hash": cas::hash_bytes(format!("{}\n{}", QUERY_PROMPT.trim_end(), DOCUMENT_PROMPT.trim_end()).as_bytes()),
        "input_construction": "chunk_filename_context_v1",
        "dimensions": DIMENSIONS,
        "distance": "cosine",
        "modality": "multimodal",
        "runtime_kind": "local",
        "spec_version": 1,
        "embedding_semantics": {
            "model_revision": REVISION,
            "runtime": {
                "name": "litert-lm",
                "version": litert::VERSION,
                "c_api_version": C_API_VERSION,
                "artifact_set_sha256": litert::ARCHIVE_SHA256,
            },
            // The literal strings in the JCS identity preserve trailing spaces.
            "query_prompt": QUERY_PROMPT,
            "document_prompt": DOCUMENT_PROMPT,
            "windowing": {
                "token_cap": TOKEN_CAP,
                "overflow": "ERROR",
                "insert_special_tokens": INSERT_SPECIAL_TOKENS,
                "max_chars": MAX_BODY_CHARS,
                "max_depth": MAX_DEPTH,
                "split": "central_half_paragraph_sentence_scalar_earlier_tie_v1",
                "weight": "body_unicode_scalars",
            },
            "native_normalize": NORMALIZE,
            "aggregation": "l2_weighted_mean_l2_f64_to_f32_source_order_v1",
            "image_input": {
                "original": "png_jpeg_passthrough",
                "transcode": PNG_TRANSCODE_RECIPE,
                "receipt": "image_input_receipt_v1",
            },
        }
    })
}

#[must_use]
pub fn profile_hash() -> &'static str {
    static HASH: LazyLock<String> =
        LazyLock::new(|| cas::hash_json(&profile_value()).expect("EG2 profile is a valid literal"));
    HASH.as_str()
}
