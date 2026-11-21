//! Adapter identity hash helpers from `docs/03-data-model.md` §5.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use crate::{AdapterError, Result};

const PROFILE_FIELDS: &[&str] = &[
    "adapter_kind",
    "adapter_role",
    "model_or_tool_family",
    "model_version_pin",
    "prompt_template_id",
    "prompt_template_hash",
    "sampling",
    "output_schema",
    // QA34 (docs/03-data-model.md §5.1): prepare-only —
    // {renderer_name, renderer_version, dpi, color_space, output_format}, all
    // rendering settings that affect byte-level determinism (04 §2.1). No
    // current built-in Prepare Adapter renders page images (the bundled one
    // is PDF text-layer extraction only), so this key is absent from every
    // existing profile input and does not perturb the frozen hash vectors
    // below — it is schema/calc-convention groundwork ahead of a rendering
    // Prepare Adapter (04 §2.1's "採用要件", confirmed with no grace period).
    "render_params",
    "dimensions",
    "distance",
    "modality",
    // 2026-07-24 (07 §5.3 contextual-embedding addendum): embedding-only — how
    // Kio constructs the text handed to the Embedding Adapter (currently
    // `chunk_filename_context_v1`: humanized filename prepended to the chunk
    // body). It changes what a vector MEANS, so it belongs to the vector-space
    // identity the profile hash pins (03 §7). Absent from every non-embedding
    // profile (markdownize/OCR/prepare), so — like `render_params` above — it
    // leaves their frozen hash vectors untouched and perturbs only the
    // embedding profile hash.
    "input_construction",
    "embedding_semantics",
    "runtime_kind",
    "spec_version",
];

#[must_use]
pub fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{}", lower_hex(&digest))
}

pub fn jcs_hash(value: &Value) -> Result<String> {
    let bytes =
        serde_jcs::to_vec(value).map_err(|err| AdapterError::ConfigSchema(err.to_string()))?;
    Ok(hash_bytes(&bytes))
}

pub fn jcs_bytes(value: &Value) -> Result<Vec<u8>> {
    serde_jcs::to_vec(value).map_err(|err| AdapterError::ConfigSchema(err.to_string()))
}

pub fn canonical_profile_value(profile: &Value) -> Result<Value> {
    let object = profile
        .as_object()
        .ok_or_else(|| AdapterError::ConfigSchema("profile must be an object".to_owned()))?;
    let mut canonical = Map::new();
    for field in PROFILE_FIELDS {
        if let Some(value) = object.get(*field)
            && !value.is_null()
        {
            canonical.insert((*field).to_owned(), value.clone());
        }
    }
    if let Some(pin) = canonical.get("model_version_pin").and_then(Value::as_str)
        && is_mutable_model_alias(pin)
    {
        return Err(AdapterError::ConfigSchema(
            "model_version_pin must be an immutable version, not a mutable alias".to_owned(),
        ));
    }
    Ok(Value::Object(canonical))
}

pub fn tool_profile_hash(profile: &Value) -> Result<String> {
    jcs_hash(&canonical_profile_value(profile)?)
}

pub fn normalize_prompt_template(raw: &str) -> String {
    let normalized_newlines = raw.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines: Vec<String> = normalized_newlines
        .split('\n')
        .map(|line| line.trim_end_matches([' ', '\t']).to_owned())
        .collect();
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines.join("\n").nfc().collect()
}

pub fn prompt_template_hash(raw: &str) -> String {
    hash_bytes(normalize_prompt_template(raw).as_bytes())
}

#[must_use]
pub fn is_mutable_model_alias(pin: &str) -> bool {
    let lower = pin.to_ascii_lowercase();
    lower == "latest" || lower.ends_with("-latest") || lower.ends_with("_latest")
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn profile_hash_vectors_match_step2a() {
        let mistral = json!({
            "adapter_kind": "markdownize",
            "adapter_role": "multimodal",
            "model_or_tool_family": "mistral-ocr",
            "model_version_pin": "mistral-ocr-2505",
            "output_schema": "kio-markdown-v1",
            "runtime_kind": "cloud",
            "spec_version": 1
        });
        assert_eq!(
            jcs_bytes(&canonical_profile_value(&mistral).unwrap()).unwrap(),
            br#"{"adapter_kind":"markdownize","adapter_role":"multimodal","model_or_tool_family":"mistral-ocr","model_version_pin":"mistral-ocr-2505","output_schema":"kio-markdown-v1","runtime_kind":"cloud","spec_version":1}"#
        );
        assert_eq!(
            tool_profile_hash(&mistral).unwrap(),
            "sha256:393d7b062ec1fd573c0a061455bef3f3ee16367378ca4122a0684045178e974c"
        );

        let with_nulls = json!({
            "adapter_kind": "markdownize",
            "adapter_role": "multimodal",
            "model_or_tool_family": "mistral-ocr",
            "model_version_pin": "mistral-ocr-2505",
            "prompt_template_id": null,
            "prompt_template_hash": null,
            "sampling": null,
            "dimensions": null,
            "distance": null,
            "modality": null,
            "output_schema": "kio-markdown-v1",
            "runtime_kind": "cloud",
            "spec_version": 1
        });
        assert_eq!(
            tool_profile_hash(&with_nulls).unwrap(),
            tool_profile_hash(&mistral).unwrap()
        );

        let deterministic = json!({
            "adapter_kind": "markdownize",
            "adapter_role": "text",
            "model_or_tool_family": "kio-deterministic-text",
            "model_version_pin": "1.0.0",
            "output_schema": "kio-markdown-v1",
            "runtime_kind": "local",
            "spec_version": 1
        });
        assert_eq!(
            tool_profile_hash(&deterministic).unwrap(),
            "sha256:c45472a018dc124e92ea24d9e7ce46c9a842f8b640829b83f3e46af386e9bb30"
        );

        let embedding = json!({
            "adapter_kind": "embedding",
            "adapter_role": "multimodal",
            "dimensions": 1536,
            "distance": "cosine",
            "modality": "multimodal",
            "model_or_tool_family": "gemini-multimodal-embedding",
            "model_version_pin": "gemini-embedding-001",
            "runtime_kind": "cloud",
            "spec_version": 1
        });
        assert_eq!(
            tool_profile_hash(&embedding).unwrap(),
            "sha256:c2bda78e217e1f9e12cd17ddac6c46e28a50b8060976f533f76f14193a807226"
        );

        let prepare = json!({
            "adapter_kind": "prepare",
            "adapter_role": "text",
            "model_or_tool_family": "kio-deterministic-prepare",
            "model_version_pin": "1.0.0",
            "runtime_kind": "local",
            "spec_version": 1
        });
        assert_eq!(
            tool_profile_hash(&prepare).unwrap(),
            "sha256:fa2b65e6d60562d85c6402ceca92459a5a69ae203a65ba3f6ee96a5553c995a4"
        );
    }

    #[test]
    fn profile_hash_ignores_execution_and_auth_fields() {
        let base = json!({
            "adapter_kind": "markdownize",
            "adapter_role": "text",
            "model_or_tool_family": "kio-deterministic-text",
            "model_version_pin": "1.0.0",
            "output_schema": "kio-markdown-v1",
            "runtime_kind": "local",
            "spec_version": 1,
            "cmd": "/usr/bin/a",
            "url": "https://one.example",
            "auth": "env:A"
        });
        let changed = json!({
            "adapter_kind": "markdownize",
            "adapter_role": "text",
            "model_or_tool_family": "kio-deterministic-text",
            "model_version_pin": "1.0.0",
            "output_schema": "kio-markdown-v1",
            "runtime_kind": "local",
            "spec_version": 1,
            "cmd": "/usr/bin/b",
            "url": "https://two.example",
            "auth": "plain:not-a-real-key"
        });
        assert_eq!(
            tool_profile_hash(&base).unwrap(),
            tool_profile_hash(&changed).unwrap()
        );
    }

    #[test]
    fn prompt_template_hash_vector_matches_step2a() {
        let raw = "You are a markdownize adapter.  \r\nProcess the cafe\u{301} uncha\u{301}nged unit.\t\t\r\n\r\n";
        let normalized = normalize_prompt_template(raw);
        assert_eq!(
            normalized.as_bytes(),
            b"You are a markdownize adapter.\nProcess the caf\xc3\xa9 unch\xc3\xa1nged unit."
        );
        assert_eq!(
            prompt_template_hash(raw),
            "sha256:3f5200e929d23e1f113f605fb528b1b7b75e183d226064d319f57fb3e467d238"
        );
    }

    #[test]
    fn mutable_model_alias_is_rejected() {
        let profile = json!({
            "adapter_kind": "markdownize",
            "adapter_role": "multimodal",
            "model_or_tool_family": "mistral-ocr",
            "model_version_pin": "mistral-ocr-latest",
            "runtime_kind": "cloud",
            "spec_version": 1
        });
        assert!(tool_profile_hash(&profile).is_err());
    }

    // QA34 (docs/03-data-model.md §5.1): `render_params`
    // is a hash input — a renderer setting change perturbs `tool_profile_hash`
    // (04 §2.1's prepared-Adapter byte-stability requirement rests on this).
    #[test]
    fn qa34_render_params_perturbs_tool_profile_hash() {
        let base = json!({
            "adapter_kind": "prepare",
            "adapter_role": "text",
            "model_or_tool_family": "kio-render-prepare",
            "model_version_pin": "1.0.0",
            "runtime_kind": "local",
            "spec_version": 1,
            "render_params": {
                "renderer_name": "kio-pdfium",
                "renderer_version": "1.0.0",
                "dpi": 300,
                "color_space": "srgb",
                "output_format": "png"
            }
        });
        let higher_dpi = json!({
            "adapter_kind": "prepare",
            "adapter_role": "text",
            "model_or_tool_family": "kio-render-prepare",
            "model_version_pin": "1.0.0",
            "runtime_kind": "local",
            "spec_version": 1,
            "render_params": {
                "renderer_name": "kio-pdfium",
                "renderer_version": "1.0.0",
                "dpi": 600,
                "color_space": "srgb",
                "output_format": "png"
            }
        });
        assert_ne!(
            tool_profile_hash(&base).unwrap(),
            tool_profile_hash(&higher_dpi).unwrap()
        );
        // Absent `render_params` (every existing built-in profile) is
        // unaffected — the frozen `profile_hash_vectors_match_step2a` vectors
        // above prove this by never including the key.
        let without = json!({
            "adapter_kind": "prepare",
            "adapter_role": "text",
            "model_or_tool_family": "kio-render-prepare",
            "model_version_pin": "1.0.0",
            "runtime_kind": "local",
            "spec_version": 1,
            "render_params": null
        });
        assert_eq!(
            tool_profile_hash(&without).unwrap(),
            jcs_hash(&json!({
                "adapter_kind": "prepare",
                "adapter_role": "text",
                "model_or_tool_family": "kio-render-prepare",
                "model_version_pin": "1.0.0",
                "runtime_kind": "local",
                "spec_version": 1
            }))
            .unwrap()
        );
    }
}
