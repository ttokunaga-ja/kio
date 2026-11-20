use super::input::{DOCUMENT_PROMPT, MAX_BODY_CHARS, QUERY_PROMPT};
use super::{DIMENSIONS, TOKEN_CAP, native, window};
use crate::{embedding::image_input::PNG_TRANSCODE_RECIPE, model};
use kio_adapter::identity::{prompt_template_hash, tool_profile_hash};
use kio_core::litert;
use serde_json::{Value, json};
use std::sync::LazyLock;

/// The semantic identity is shared by every supported producer. The archive
/// digest binds the complete set of platform artifacts.
#[must_use]
pub fn profile_value() -> Value {
    json!({
        "adapter_kind": "embedding",
        "adapter_role": "multimodal",
        "model_or_tool_family": model::MODEL,
        "model_version_pin": format!("sha256:{}", model::MODEL_SHA256),
        "prompt_template_id": "eg2_retrieval_v1",
        "prompt_template_hash": prompt_template_hash(&format!("{QUERY_PROMPT}\n{DOCUMENT_PROMPT}")),
        "input_construction": "chunk_filename_context_v1",
        "dimensions": DIMENSIONS,
        "distance": "cosine",
        "modality": "multimodal",
        "runtime_kind": "local",
        "spec_version": 1,
        "embedding_semantics": {
            "model_revision": model::REVISION,
            "runtime": {
                "name": "litert-lm",
                "version": litert::VERSION,
                "c_api_version": native::C_API_VERSION,
                "artifact_set_sha256": litert::ARCHIVE_SHA256,
            },
            // The literal strings in the JCS identity preserve trailing spaces.
            "query_prompt": QUERY_PROMPT,
            "document_prompt": DOCUMENT_PROMPT,
            "windowing": {
                "token_cap": TOKEN_CAP,
                "overflow": "ERROR",
                "insert_special_tokens": native::INSERT_SPECIAL_TOKENS,
                "max_chars": MAX_BODY_CHARS,
                "max_depth": window::MAX_DEPTH,
                "split": "central_half_paragraph_sentence_scalar_earlier_tie_v1",
                "weight": "body_unicode_scalars",
            },
            "native_normalize": native::NORMALIZE,
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
    static HASH: LazyLock<String> = LazyLock::new(|| {
        tool_profile_hash(&profile_value()).expect("EG2 profile is a valid literal")
    });
    HASH.as_str()
}
