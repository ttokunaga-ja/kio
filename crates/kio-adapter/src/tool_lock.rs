//! `tool-lock.json` identity and validation contracts.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::identity::jcs_hash;
use crate::types::ExecutionMode;
use crate::{AdapterError, Result};

const TOOL_LOCK_ROLES: &[&str] = &["prepare", "markdown", "embedding"];
/// `tools.toml` is device-local configuration. Keep its credential-bearing
/// form within the same bounded-read limit as other small trust records.
pub const MAX_TOOLS_TOML_BYTES: u64 = 1024 * 1024;
/// Stable, non-secret operator-facing error for a `plain:` credential that
/// cannot be re-read through the owner-private trust-file boundary.
pub const TOOLS_PRIVATE_CREDENTIAL_ERROR: &str = "KIO-E-ADAPTER-TOOLS-PRIVATE-001";
const PREPARE_LOCK_FIELDS: &[&str] = &["tool_id", "profile_hash", "kind"];
const MARKDOWN_LOCK_FIELDS: &[&str] = &["tool_id", "profile_hash", "kind", "capabilities"];
const EMBEDDING_LOCK_FIELDS: &[&str] = &[
    "tool_id",
    "dimensions",
    "distance",
    "modality",
    "profile_hash",
    "kind",
    "mode",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolLock {
    pub spec_version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepare: Option<PrepareToolLockEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown: Option<MarkdownToolLockEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<EmbeddingToolLockEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareToolLockEntry {
    pub tool_id: String,
    pub profile_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ExecutionMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarkdownToolLockEntry {
    pub tool_id: String,
    pub profile_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ExecutionMode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingToolLockEntry {
    pub tool_id: String,
    pub dimensions: u32,
    pub distance: String,
    pub modality: String,
    pub profile_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ExecutionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

pub fn load_tool_lock(bytes: &[u8]) -> Result<ToolLock> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|err| AdapterError::ConfigSchema(err.to_string()))?;
    validate_tool_lock_value(&value)?;
    serde_json::from_value(value).map_err(|err| AdapterError::ConfigSchema(err.to_string()))
}

pub fn tool_lock_hash_from_bytes(bytes: &[u8]) -> Result<String> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|err| AdapterError::ConfigSchema(err.to_string()))?;
    tool_lock_hash(&value)
}

pub fn tool_lock_hash(value: &Value) -> Result<String> {
    validate_tool_lock_value(value)?;
    jcs_hash(&canonical_tool_lock_value(value)?)
}

pub fn canonical_tool_lock_value(value: &Value) -> Result<Value> {
    validate_tool_lock_value(value)?;
    let object = value
        .as_object()
        .ok_or_else(|| AdapterError::ConfigSchema("tool-lock.json must be an object".to_owned()))?;
    let mut canonical = Map::new();
    let spec_version = object
        .get("spec_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            AdapterError::ConfigSchema("tool-lock.json missing spec_version".to_owned())
        })?;
    if spec_version != 1 {
        return Err(AdapterError::ConfigSchema(format!(
            "unsupported tool-lock spec_version: {spec_version}"
        )));
    }
    canonical.insert("spec_version".to_owned(), Value::from(spec_version));
    for key in ["prepare", "markdown"] {
        if let Some(entry) = canonical_simple_entry(object, key)? {
            canonical.insert(key.to_owned(), entry);
        }
    }
    if let Some(entry) = canonical_embedding_entry(object)? {
        canonical.insert("embedding".to_owned(), entry);
    }
    Ok(Value::Object(canonical))
}

/// Parse and validate device adapter settings without copying source text into
/// diagnostics. TOML's normal Display includes the failing line, which may be
/// a plaintext credential; only the byte offset is safe to log.
pub fn parse_tools_toml(bytes: &[u8]) -> Result<toml::Value> {
    if bytes.len() as u64 > MAX_TOOLS_TOML_BYTES {
        return Err(AdapterError::ConfigSchema(
            "tools.toml exceeds its size limit".to_owned(),
        ));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| AdapterError::ConfigSchema("tools.toml is not valid UTF-8".to_owned()))?;
    let value: toml::Value = toml::from_str(text).map_err(|error: toml::de::Error| {
        let location = error
            .span()
            .map(|span| format!(" at byte {}", span.start))
            .unwrap_or_default();
        AdapterError::ConfigSchema(format!("tools.toml contains invalid TOML{location}"))
    })?;
    validate_tools_toml_value(&value)?;
    Ok(value)
}

pub fn validate_tools_toml(bytes: &[u8]) -> Result<()> {
    parse_tools_toml(bytes).map(|_| ())
}

/// Whether a syntactically valid `tools.toml` declares any literal API key.
/// Only `auth` fields count: an unrelated argument string beginning with
/// `plain:` must not turn a non-secret configuration file into a credential
/// source.
pub fn tools_toml_uses_plain_auth(bytes: &[u8]) -> Result<bool> {
    Ok(toml_value_uses_plain_auth(&parse_tools_toml(bytes)?))
}

/// Read and validate a credential-bearing `tools.toml` through core's
/// owner-private, regular-file, no-follow trust boundary. The returned bytes
/// are the only bytes from which a literal key may be derived.
pub fn read_private_tools_toml(path: &Path) -> Result<Vec<u8>> {
    let bytes =
        kio_core::private_fs::read_private_file(path, MAX_TOOLS_TOML_BYTES).map_err(|_| {
            AdapterError::ConfigSchemaCoded {
                code: TOOLS_PRIVATE_CREDENTIAL_ERROR,
                message:
                    "tools.toml containing a plain credential must be an owner-private regular file"
                        .to_owned(),
            }
        })?;
    validate_tools_toml(&bytes)?;
    Ok(bytes)
}

fn toml_value_uses_plain_auth(value: &toml::Value) -> bool {
    match value {
        toml::Value::Table(table) => table.iter().any(|(key, value)| {
            (key == "auth"
                && value
                    .as_str()
                    .is_some_and(|auth| auth.starts_with("plain:")))
                || toml_value_uses_plain_auth(value)
        }),
        toml::Value::Array(items) => items.iter().any(toml_value_uses_plain_auth),
        _ => false,
    }
}

/// R13-2: the adapter-role sections `tools.toml` may declare (docs/03 §11 +
/// docs/07 §1/§6). Symmetric with `tool-lock.json` and `config.toml`, an unknown
/// top-level section is rejected (exit 2, `KIO-E-CONFIG-SCHEMA-001`).
const TOOLS_ADAPTER_ROLES: &[&str] = &["prepare", "markdown", "embedding"];

/// R13-2: the documented per-adapter fields (docs/03 §11 + docs/07 §1/§6). Any
/// other field in an adapter entry is a typo/unknown key → rejected. `dimensions`
/// / `distance` / `modality` / `mode` are embedding fields (docs/07 §6).
/// `pricing` is QA19 (step4b-contract-tests-p3a.md §F, 03 §11 L832-837 /
/// 07 §4 L298-303): the `[<role>.<tool_id>.pricing]` nested price table.
const TOOLS_ENTRY_FIELDS: &[&str] = &[
    "kind",
    "cmd",
    "args",
    "url",
    "model",
    "auth",
    "profile_hash",
    "capabilities",
    "dimensions",
    "distance",
    "modality",
    "mode",
    "pricing",
];

/// QA19 (step4b-contract-tests-p3a.md §F, 10 §11.3 L952): the closed
/// `billable_units[].kind` enum a `[pricing]` table's keys must be drawn from
/// — identical to `kio_pipeline::ledger::ops::BILLABLE_UNIT_KINDS` and
/// `types::BillableUnitKind`'s serialized variants (kio-adapter cannot depend
/// on kio-pipeline, so this list is independently maintained here; the test
/// `pricing_kind_enum_matches_billable_unit_kind` cross-checks it against
/// `types::BillableUnitKind`).
const PRICING_KIND_ENUM: &[&str] = &["pages", "tokens_in", "tokens_out"];

/// R13-2: typed validation of `tools.toml`. docs/06 §10 and docs/10 §11.3 promise
/// a schema-driven check that never existed — the previous validator only walked
/// `auth` prefixes. Before this, a `tools.toml` `[markdown] totally_bogus_key="x"`
/// plus `cmd = 12345` (type-mismatched) were accepted at exit 0 while `config.toml`
/// rejected the same shapes at exit 2. Now unknown sections/fields and wrong types
/// are `KIO-E-CONFIG-SCHEMA-001` (exit 2). Fields are type-checked first, then
/// target-rich declarations are rejected when this build has no matching
/// dispatcher. The `auth`-prefix check is applied only to the `auth` field.
pub fn validate_tools_toml_value(value: &toml::Value) -> Result<()> {
    let table = value
        .as_table()
        .ok_or_else(|| AdapterError::ConfigSchema("tools.toml must be a table".to_owned()))?;
    for (role, section) in table {
        if !TOOLS_ADAPTER_ROLES.contains(&role.as_str()) {
            return Err(AdapterError::ConfigSchema(format!(
                "unknown tools.toml section `{role}` (expected one of: {})",
                TOOLS_ADAPTER_ROLES.join(", ")
            )));
        }
        validate_tools_role_section(role, section)?;
    }
    Ok(())
}

fn validate_tools_role_section(role: &str, section: &toml::Value) -> Result<()> {
    let table = section.as_table().ok_or_else(|| {
        AdapterError::ConfigSchema(format!("tools.toml `{role}` must be a table"))
    })?;
    for (key, val) in table {
        if TOOLS_ENTRY_FIELDS.contains(&key.as_str()) {
            // Single-adapter-per-role form: fields directly under `[role]`.
            validate_tools_entry_field(role, key, val)?;
        } else if val.is_table() {
            // `[role.tool_id]` form: the key is a tool_id naming a nested entry.
            validate_tools_adapter_entry(role, key, val)?;
        } else {
            return Err(AdapterError::ConfigSchema(format!(
                "unknown key `{role}.{key}` in tools.toml \
                 (not a known adapter field, and not a `[{role}.<tool_id>]` table)"
            )));
        }
    }
    let has_direct_fields = table
        .keys()
        .any(|key| TOOLS_ENTRY_FIELDS.contains(&key.as_str()));
    if has_direct_fields {
        if table
            .keys()
            .any(|key| !TOOLS_ENTRY_FIELDS.contains(&key.as_str()))
        {
            return Err(AdapterError::ConfigSchema(format!(
                "tools.toml `{role}` cannot mix direct adapter fields with nested adapter entries"
            )));
        }
        validate_supported_runtime_target(role, None, table)?;
    }
    Ok(())
}

fn validate_tools_adapter_entry(role: &str, tool_id: &str, entry: &toml::Value) -> Result<()> {
    let table = entry.as_table().ok_or_else(|| {
        AdapterError::ConfigSchema(format!("tools.toml `{role}.{tool_id}` must be a table"))
    })?;
    let context = format!("{role}.{tool_id}");
    for (key, val) in table {
        if !TOOLS_ENTRY_FIELDS.contains(&key.as_str()) {
            return Err(AdapterError::ConfigSchema(format!(
                "unknown field `{key}` in tools.toml `{context}`"
            )));
        }
        validate_tools_entry_field(&context, key, val)?;
    }
    validate_supported_runtime_target(role, Some(tool_id), table)?;
    Ok(())
}

/// One embedding implementation this build can execute (07 §5.3).
///
/// Before 2026-07-26 these values were inlined as Gemini's, which is why the
/// `offline_api` half of `ExecutionMode` was unreachable for the embedding role.
struct EmbeddingRuntimeTarget {
    tool_id: &'static str,
    /// `execution_mode` as spelled in `tools.toml`.
    kind: &'static str,
    mode: &'static str,
    model: &'static str,
    dimensions: i64,
    distance: &'static str,
    modality: &'static str,
}

pub(crate) const LOCAL_EMBEDDING_TOOL_ID: &str = "qwen3_vl_embedding_local";

/// Dimensions stay 768 across both targets: `chunk_vec`'s sqlite-vec width is
/// fixed at `kio_index::fts::CHUNK_VEC_DIMENSIONS` (04 §4.3), so a target
/// declaring anything else would produce vectors vector search cannot read.
const EMBEDDING_RUNTIME_TARGETS: &[EmbeddingRuntimeTarget] = &[
    EmbeddingRuntimeTarget {
        tool_id: "gemini_embedding_2",
        kind: "online_api",
        mode: "online",
        model: "gemini-embedding-2",
        dimensions: 768,
        distance: "cosine",
        modality: "multimodal",
    },
    EmbeddingRuntimeTarget {
        tool_id: LOCAL_EMBEDDING_TOOL_ID,
        kind: "offline_api",
        mode: "offline",
        model: "Qwen/Qwen3-VL-Embedding-2B",
        dimensions: 768,
        distance: "cosine",
        modality: "multimodal",
    },
];

/// One markdownize implementation this build can execute (07 §5.2).
///
/// The same generalization the embedding role got, and for the same reason: the
/// markdown arm hard-coded Mistral's name and `online_api`, which left
/// `offline_api` unreachable for this role no matter what `tools.toml` said.
struct MarkdownRuntimeTarget {
    tool_id: &'static str,
    /// `execution_mode` as spelled in `tools.toml`.
    kind: &'static str,
    /// A declared `model` must start with this. A prefix rather than an exact
    /// string because both vendors version the name in place —
    /// `mistral-ocr-2505`, `PaddleOCR-VL-1.6-0.9B` — and 03 §5.1 pins the
    /// weights by digest, so the name is a routing hint, not the identity.
    model_prefix: &'static str,
}

pub(crate) const LOCAL_OCR_TOOL_ID: &str = "paddleocr_vl_local";

const MARKDOWN_RUNTIME_TARGETS: &[MarkdownRuntimeTarget] = &[
    MarkdownRuntimeTarget {
        tool_id: "mistral_ocr_markdownize",
        kind: "online_api",
        model_prefix: "mistral-ocr-",
    },
    MarkdownRuntimeTarget {
        tool_id: LOCAL_OCR_TOOL_ID,
        kind: "offline_api",
        model_prefix: "PaddleOCR-VL",
    },
];

/// Which markdown target a declaration means.
///
/// Mirrors [`embedding_runtime_target`], including its caveat: resolving the
/// flat `[markdown]` form by `kind` is unambiguous only while one target exists
/// per kind. A second offline markdownize implementation — which is what
/// Sarashina2.2-OCR would be, once it can be served at all — must make
/// `tool_id` mandatory rather than let this pick one.
fn markdown_runtime_target(
    tool_id: Option<&str>,
    table: &toml::value::Table,
) -> Result<&'static MarkdownRuntimeTarget> {
    if let Some(tool_id) = tool_id {
        return MARKDOWN_RUNTIME_TARGETS
            .iter()
            .find(|target| target.tool_id == tool_id)
            .ok_or_else(|| {
                AdapterError::ConfigSchema(format!(
                    "unsupported markdown adapter `{tool_id}`; this build executes only {}",
                    supported_markdown_tool_ids()
                ))
            });
    }
    let kind = table
        .get("kind")
        .and_then(toml::Value::as_str)
        .unwrap_or("online_api");
    MARKDOWN_RUNTIME_TARGETS
        .iter()
        .find(|target| target.kind == kind)
        .ok_or_else(|| {
            AdapterError::ConfigSchema(format!(
                "tools.toml `markdown.kind` must be `online_api` or `offline_api` (got `{kind}`)"
            ))
        })
}

fn supported_markdown_tool_ids() -> String {
    MARKDOWN_RUNTIME_TARGETS
        .iter()
        .map(|target| format!("`{}`", target.tool_id))
        .collect::<Vec<_>>()
        .join(" and ")
}

/// The execution-time twin of [`markdown_runtime_target`].
fn declared_markdown_runtime_target(
    declared: &DeclaredAdapter,
) -> Result<&'static MarkdownRuntimeTarget> {
    if let Some(tool_id) = declared.tool_id.as_deref() {
        return MARKDOWN_RUNTIME_TARGETS
            .iter()
            .find(|target| target.tool_id == tool_id)
            .ok_or_else(|| {
                AdapterError::ConfigSchema(format!(
                    "declared markdown adapter `{tool_id}` does not match any effective runtime \
                     ({})",
                    supported_markdown_tool_ids()
                ))
            });
    }
    let kind = declared.kind.as_deref().unwrap_or("online_api");
    MARKDOWN_RUNTIME_TARGETS
        .iter()
        .find(|target| target.kind == kind)
        .ok_or_else(|| {
            AdapterError::ConfigSchema(format!(
                "declared markdown kind `{kind}` does not match any effective runtime"
            ))
        })
}

/// Which target a declaration means.
///
/// A `[embedding.<tool_id>]` section names it outright. The flat `[embedding]`
/// form cannot, so it is resolved by declared `kind`, defaulting to the online
/// target — which is what the flat form has always meant. That resolution is
/// unambiguous only while one target exists per kind; adding a second offline
/// embedding implementation must make `tool_id` mandatory rather than pick one.
fn embedding_runtime_target(
    tool_id: Option<&str>,
    table: &toml::value::Table,
) -> Result<&'static EmbeddingRuntimeTarget> {
    if let Some(tool_id) = tool_id {
        return EMBEDDING_RUNTIME_TARGETS
            .iter()
            .find(|target| target.tool_id == tool_id)
            .ok_or_else(|| {
                AdapterError::ConfigSchema(format!(
                    "unsupported embedding adapter `{tool_id}`; this build executes only {}",
                    supported_embedding_tool_ids()
                ))
            });
    }
    let kind = table
        .get("kind")
        .and_then(toml::Value::as_str)
        .unwrap_or("online_api");
    EMBEDDING_RUNTIME_TARGETS
        .iter()
        .find(|target| target.kind == kind)
        .ok_or_else(|| {
            AdapterError::ConfigSchema(format!(
                "tools.toml `embedding.kind` must be `online_api` or `offline_api` (got `{kind}`)"
            ))
        })
}

/// The execution-time twin of [`embedding_runtime_target`], resolving from a
/// registered [`DeclaredAdapter`] instead of the raw TOML table. Same rule:
/// `tool_id` when it names one, otherwise the declared `kind`.
fn declared_embedding_runtime_target(
    declared: &DeclaredAdapter,
) -> Result<&'static EmbeddingRuntimeTarget> {
    if let Some(tool_id) = declared.tool_id.as_deref() {
        return EMBEDDING_RUNTIME_TARGETS
            .iter()
            .find(|target| target.tool_id == tool_id)
            .ok_or_else(|| {
                AdapterError::ConfigSchema(format!(
                    "declared embedding adapter `{tool_id}` does not match effective runtime {}",
                    supported_embedding_tool_ids()
                ))
            });
    }
    let kind = declared.kind.as_deref().unwrap_or("online_api");
    EMBEDDING_RUNTIME_TARGETS
        .iter()
        .find(|target| target.kind == kind)
        .ok_or_else(|| {
            AdapterError::ConfigSchema(format!(
                "declared embedding kind `{kind}` does not match any built-in runtime"
            ))
        })
}

fn supported_embedding_tool_ids() -> String {
    EMBEDDING_RUNTIME_TARGETS
        .iter()
        .map(|target| format!("`{}`", target.tool_id))
        .collect::<Vec<_>>()
        .join(" / ")
}

/// D1 (07 §3): an `offline_api` target may only address the local machine.
///
/// The check is on the literal host and never on a resolved address. A name
/// that resolves to loopback at validation time can resolve anywhere at request
/// time, so accepting "it resolves to 127.0.0.1" would reopen exactly the hole
/// this closes. `offline_api` still requires an exact network grant for every
/// local HTTP exchange; loopback validation does not bypass consent.
fn validate_offline_url(role: &str, url: &str) -> Result<()> {
    crate::local_peer::validate_local_https_url(url)
        .map(|_| ())
        .map_err(|error| match error {
            AdapterError::LocalPeerConfig { code, message } => AdapterError::LocalPeerConfig {
                code,
                message: format!(
                    "tools.toml `{role}.url` = `{url}` is not an authenticated local peer: {message}"
                ),
            },
            other => other,
        })
}

fn validate_supported_runtime_target(
    role: &str,
    tool_id: Option<&str>,
    table: &toml::value::Table,
) -> Result<()> {
    // No shared `cmd || args || url` precheck any more: `url` is now legitimate
    // on both roles when the resolved target is `offline_api`, so lumping it in
    // with `cmd`/`args` would refuse the local adapters before their own arm
    // could apply D1's loopback check.
    match role {
        "markdown" => {
            let target = markdown_runtime_target(tool_id, table)?;
            // `cmd`/`args` stay refused for both kinds, exactly as on the
            // embedding role: Kio never spawns a process (05 §5), and the
            // external dispatcher that would is still future work (07 §7).
            // `url` is what an offline_api target needs, and it is admissible
            // only after the D1 loopback check.
            if table.contains_key("cmd") || table.contains_key("args") {
                return Err(AdapterError::ConfigSchema(format!(
                    "markdown cmd/args targets are unsupported by the built-in runtime (`{}`)",
                    target.tool_id
                )));
            }
            match table.get("url").and_then(toml::Value::as_str) {
                Some(url) if target.kind == "offline_api" => {
                    validate_offline_url("markdown", url)?;
                }
                Some(_) => {
                    return Err(AdapterError::ConfigSchema(format!(
                        "markdown url targets are unsupported for `{}`; only an \
                         `offline_api` adapter may declare one",
                        target.tool_id
                    )));
                }
                None => {}
            }
            require_declared_kind("markdown", table, target.kind)?;
            if let Some(model) = table.get("model").and_then(toml::Value::as_str)
                && !model.starts_with(target.model_prefix)
            {
                return Err(AdapterError::ConfigSchema(format!(
                    "unsupported markdown model `{model}`; expected a `{}` model for `{}`",
                    target.model_prefix, target.tool_id
                )));
            }
        }
        "embedding" => {
            let target = embedding_runtime_target(tool_id, table)?;
            // `cmd`/`args` stay refused for both kinds: Kio never spawns a
            // process (05 §5), and the external dispatcher that would is still
            // future work (07 §7). `url` is what an offline_api target needs,
            // and it is admissible only after the D1 loopback check.
            if table.contains_key("cmd") || table.contains_key("args") {
                return Err(AdapterError::ConfigSchema(format!(
                    "embedding cmd/args targets are unsupported by the built-in runtime \
                     (`{}`)",
                    target.tool_id
                )));
            }
            match table.get("url").and_then(toml::Value::as_str) {
                Some(url) if target.kind == "offline_api" => {
                    validate_offline_url("embedding", url)?;
                }
                Some(_) => {
                    return Err(AdapterError::ConfigSchema(format!(
                        "embedding url targets are unsupported for `{}`; only an \
                         `offline_api` adapter may declare one",
                        target.tool_id
                    )));
                }
                None => {}
            }
            require_declared_kind("embedding", table, target.kind)?;
            if let Some(model) = table.get("model").and_then(toml::Value::as_str)
                && model != target.model
            {
                return Err(AdapterError::ConfigSchema(format!(
                    "unsupported embedding model `{model}`; expected `{}` for `{}`",
                    target.model, target.tool_id
                )));
            }
            if table
                .get("dimensions")
                .and_then(toml::Value::as_integer)
                .is_some_and(|dimensions| dimensions != target.dimensions)
            {
                return Err(AdapterError::ConfigSchema(format!(
                    "embedding dimensions must be {} for `{}`",
                    target.dimensions, target.tool_id
                )));
            }
            if table
                .get("distance")
                .and_then(toml::Value::as_str)
                .is_some_and(|distance| distance != target.distance)
            {
                return Err(AdapterError::ConfigSchema(format!(
                    "embedding distance must be `{}` for `{}`",
                    target.distance, target.tool_id
                )));
            }
            if table
                .get("modality")
                .and_then(toml::Value::as_str)
                .is_some_and(|modality| modality != target.modality)
            {
                return Err(AdapterError::ConfigSchema(format!(
                    "embedding modality must be `{}` for `{}`",
                    target.modality, target.tool_id
                )));
            }
            if table
                .get("mode")
                .and_then(toml::Value::as_str)
                .is_some_and(|mode| mode != target.mode)
            {
                return Err(AdapterError::ConfigSchema(format!(
                    "embedding mode must be `{}` for `{}`",
                    target.mode, target.tool_id
                )));
            }
        }
        _ => {}
    }
    Ok(())
}

/// `expected` always comes from a resolved runtime target now. The former
/// `require_online_kind` wrapper is gone with its last caller: hard-coding
/// `online_api` is exactly the assumption that kept `offline_api` unreachable
/// for the markdown role.
fn require_declared_kind(role: &str, table: &toml::value::Table, expected: &str) -> Result<()> {
    if table
        .get("kind")
        .and_then(toml::Value::as_str)
        .is_some_and(|kind| kind != expected)
    {
        return Err(AdapterError::ConfigSchema(format!(
            "tools.toml `{role}.kind` must be `{expected}` for the built-in runtime"
        )));
    }
    Ok(())
}

fn validate_tools_entry_field(context: &str, key: &str, val: &toml::Value) -> Result<()> {
    match key {
        "kind" | "cmd" | "url" | "model" | "profile_hash" | "distance" | "modality" | "mode" => {
            if !val.is_str() {
                return Err(field_type_error(context, key, "a string"));
            }
        }
        "auth" => {
            // The `env:`/`plain:` prefix check is scoped to the
            // Credential reference prefixes are meaningful only on `auth`.
            let Some(auth) = val.as_str() else {
                return Err(field_type_error(context, key, "a string"));
            };
            if !valid_auth_value(auth) {
                return Err(AdapterError::ConfigSchema(
                    "auth must start with env: or plain:".to_owned(),
                ));
            }
        }
        "args" | "capabilities" => {
            let all_strings = val
                .as_array()
                .is_some_and(|items| items.iter().all(toml::Value::is_str));
            if !all_strings {
                return Err(field_type_error(context, key, "an array of strings"));
            }
        }
        "dimensions" => {
            if !val.is_integer() {
                return Err(field_type_error(context, key, "an integer"));
            }
        }
        // QA19: `[<context>.pricing]` — key = billable_units kind closed enum,
        // value = finite non-negative USD unit price (10 §11.3 L952).
        "pricing" => validate_pricing_table(context, val)?,
        _ => unreachable!("caller restricted `key` to TOOLS_ENTRY_FIELDS"),
    }
    Ok(())
}

/// QA19: validate a `[<role>.<tool_id>.pricing]` table (03 §11 L832-837 /
/// 10 §11.3 L952). Every key must be a member of the closed
/// `billable_units[].kind` enum (unknown key = schema error, matching the
/// crate-wide strict-schema posture — R13-2); every value must be a finite,
/// non-negative REAL (an integer TOML value is accepted too — `0` and `4`
/// are valid TOML integers a user may reasonably write for a whole-dollar
/// price).
fn validate_pricing_table(context: &str, val: &toml::Value) -> Result<()> {
    let table = val
        .as_table()
        .ok_or_else(|| field_type_error(context, "pricing", "a table"))?;
    for (kind, price) in table {
        if !PRICING_KIND_ENUM.contains(&kind.as_str()) {
            return Err(AdapterError::ConfigSchema(format!(
                "unknown pricing kind `{kind}` in tools.toml `{context}.pricing` \
                 (expected one of: {})",
                PRICING_KIND_ENUM.join(", ")
            )));
        }
        let numeric = price
            .as_float()
            .or_else(|| price.as_integer().map(|value| value as f64));
        let Some(numeric) = numeric else {
            return Err(field_type_error(
                context,
                &format!("pricing.{kind}"),
                "a finite, non-negative number",
            ));
        };
        if !numeric.is_finite() || numeric < 0.0 {
            return Err(AdapterError::ConfigSchema(format!(
                "tools.toml `{context}.pricing.{kind}` must be a finite, non-negative number \
                 (got {numeric})"
            )));
        }
    }
    Ok(())
}

fn field_type_error(context: &str, key: &str, expected: &str) -> AdapterError {
    AdapterError::ConfigSchema(format!("tools.toml `{context}.{key}` must be {expected}"))
}

fn validate_tool_lock_value(value: &Value) -> Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| AdapterError::ConfigSchema("tool-lock.json must be an object".to_owned()))?;
    let Some(spec_version) = object.get("spec_version").and_then(Value::as_u64) else {
        return Err(AdapterError::ConfigSchema(
            "tool-lock.json spec_version must be an integer".to_owned(),
        ));
    };
    if spec_version != 1 {
        return Err(AdapterError::ConfigSchema(format!(
            "unsupported tool-lock spec_version: {spec_version}"
        )));
    }
    for key in object.keys() {
        if key != "spec_version" && !TOOL_LOCK_ROLES.contains(&key.as_str()) {
            return Err(AdapterError::ConfigSchema(format!(
                "unknown tool-lock.json role `{key}` (expected one of: {})",
                TOOL_LOCK_ROLES.join(", ")
            )));
        }
    }
    for key in ["prepare", "markdown"] {
        if let Some(entry) = object.get(key) {
            validate_simple_entry(key, entry)?;
        }
    }
    if let Some(entry) = object.get("embedding") {
        validate_embedding_entry(entry)?;
    }
    Ok(())
}

fn canonical_simple_entry(object: &Map<String, Value>, key: &str) -> Result<Option<Value>> {
    let Some(entry) = object.get(key) else {
        return Ok(None);
    };
    if entry.is_null() {
        return Ok(None);
    }
    let entry = entry
        .as_object()
        .ok_or_else(|| AdapterError::ConfigSchema(format!("{key} entry must be an object")))?;
    let mut canonical = Map::new();
    canonical.insert(
        "tool_id".to_owned(),
        required_string(entry, key, "tool_id")?,
    );
    canonical.insert(
        "profile_hash".to_owned(),
        required_string(entry, key, "profile_hash")?,
    );
    Ok(Some(Value::Object(canonical)))
}

fn canonical_embedding_entry(object: &Map<String, Value>) -> Result<Option<Value>> {
    let Some(entry) = object.get("embedding") else {
        return Ok(None);
    };
    if entry.is_null() {
        return Ok(None);
    }
    let entry = entry.as_object().ok_or_else(|| {
        AdapterError::ConfigSchema("embedding entry must be an object".to_owned())
    })?;
    let mut canonical = Map::new();
    canonical.insert(
        "dimensions".to_owned(),
        required_u64(entry, "embedding", "dimensions")?,
    );
    canonical.insert(
        "distance".to_owned(),
        required_string(entry, "embedding", "distance")?,
    );
    canonical.insert(
        "modality".to_owned(),
        required_string(entry, "embedding", "modality")?,
    );
    canonical.insert(
        "profile_hash".to_owned(),
        required_string(entry, "embedding", "profile_hash")?,
    );
    canonical.insert(
        "tool_id".to_owned(),
        required_string(entry, "embedding", "tool_id")?,
    );
    Ok(Some(Value::Object(canonical)))
}

fn validate_simple_entry(key: &str, value: &Value) -> Result<()> {
    if value.is_null() {
        return Ok(());
    }
    let object = value
        .as_object()
        .ok_or_else(|| AdapterError::ConfigSchema(format!("{key} entry must be an object")))?;
    let allowed_fields = match key {
        "prepare" => PREPARE_LOCK_FIELDS,
        "markdown" => MARKDOWN_LOCK_FIELDS,
        _ => unreachable!("simple tool-lock entry is prepare or markdown"),
    };
    validate_lock_entry_fields(key, object, allowed_fields)?;
    required_string(object, key, "tool_id")?;
    required_string(object, key, "profile_hash")?;
    Ok(())
}

fn validate_embedding_entry(value: &Value) -> Result<()> {
    if value.is_null() {
        return Ok(());
    }
    let object = value.as_object().ok_or_else(|| {
        AdapterError::ConfigSchema("embedding entry must be an object".to_owned())
    })?;
    validate_lock_entry_fields("embedding", object, EMBEDDING_LOCK_FIELDS)?;
    required_string(object, "embedding", "tool_id")?;
    required_string(object, "embedding", "profile_hash")?;
    required_u64(object, "embedding", "dimensions")?;
    required_string(object, "embedding", "distance")?;
    let modality = required_string(object, "embedding", "modality")?;
    // 03 §7: modality は "multimodal" に固定。別ベクトル空間 (text 専用等) の
    // embedding profile は tool-lock materialize の時点で採用拒否する。
    if modality.as_str() != Some("multimodal") {
        return Err(AdapterError::ConfigSchemaCoded {
            code: "KIO-E-EMBED-MODALITY-001",
            message: format!(
                "embedding.modality must be \"multimodal\" (got {modality}); \
                 non-multimodal embedding profiles are not adoptable"
            ),
        });
    }
    Ok(())
}

fn validate_lock_entry_fields(
    entry: &str,
    object: &Map<String, Value>,
    allowed_fields: &[&str],
) -> Result<()> {
    for field in object.keys() {
        if !allowed_fields.contains(&field.as_str()) {
            return Err(AdapterError::ConfigSchema(format!(
                "unknown field `{entry}.{field}` in tool-lock.json"
            )));
        }
    }
    Ok(())
}

fn required_string(object: &Map<String, Value>, entry: &str, field: &str) -> Result<Value> {
    let value = object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| AdapterError::ConfigSchema(format!("{entry}.{field} must be a string")))?;
    Ok(Value::String(value.to_owned()))
}

fn required_u64(object: &Map<String, Value>, entry: &str, field: &str) -> Result<Value> {
    let value = object
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| AdapterError::ConfigSchema(format!("{entry}.{field} must be an integer")))?;
    Ok(Value::from(value))
}

fn valid_auth_value(value: &str) -> bool {
    ["env:", "plain:"]
        .iter()
        .any(|prefix| value.starts_with(prefix) && value.len() > prefix.len())
}

/// R13-2: a declared adapter entry from `tools.toml`, resolved for a single
/// adapter role. `tool_id` is the `[role.tool_id]` key (or the role name for the
/// single-adapter form). Execution-target fields are preserved as well as
/// model/auth so the runtime can prove that its effective recipient matches the
/// accepted declaration instead of silently projecting target identity away.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeclaredAdapter {
    pub tool_id: Option<String>,
    pub kind: Option<String>,
    pub cmd: Option<String>,
    pub args: Vec<String>,
    pub url: Option<String>,
    pub model: Option<String>,
    pub auth: Option<String>,
}

/// R13-2: locate the declared adapter for `role` in an already-parsed `tools.toml`
/// (`markdown` / `embedding` / …). Returns the complete execution identity so
/// the CLI can recheck recipient, credential, and model together. Accepts both documented
/// shapes: fields directly under `[role]` (single adapter) and a single
/// `[role.tool_id]` table. `None` when the role is not declared. Assumes the value
/// already passed [`validate_tools_toml_value`].
#[must_use]
pub fn declared_adapter_for_role(value: &toml::Value, role: &str) -> Option<DeclaredAdapter> {
    let section = value.as_table()?.get(role)?.as_table()?;
    // Single-adapter form: documented fields sit directly under `[role]`.
    let has_direct_fields = section
        .keys()
        .any(|key| TOOLS_ENTRY_FIELDS.contains(&key.as_str()));
    if has_direct_fields {
        return Some(DeclaredAdapter {
            tool_id: None,
            kind: optional_string(section, "kind"),
            cmd: optional_string(section, "cmd"),
            args: optional_string_array(section, "args"),
            url: optional_string(section, "url"),
            model: section
                .get("model")
                .and_then(toml::Value::as_str)
                .map(str::to_owned),
            auth: section
                .get("auth")
                .and_then(toml::Value::as_str)
                .map(str::to_owned),
        });
    }
    // `[role.tool_id]` form: take the first declared tool_id (MVP: one per role).
    let (tool_id, entry) = section.iter().next()?;
    let entry = entry.as_table()?;
    Some(DeclaredAdapter {
        tool_id: Some(tool_id.clone()),
        kind: optional_string(entry, "kind"),
        cmd: optional_string(entry, "cmd"),
        args: optional_string_array(entry, "args"),
        url: optional_string(entry, "url"),
        model: entry
            .get("model")
            .and_then(toml::Value::as_str)
            .map(str::to_owned),
        auth: entry
            .get("auth")
            .and_then(toml::Value::as_str)
            .map(str::to_owned),
    })
}

/// QA19 (step4b-contract-tests-p3a.md §F, 03 §11 L832-837): the declared
/// `[pricing]` table for `role` in an already-parsed `tools.toml`, resolved
/// the same way [`declared_adapter_for_role`] resolves the adapter fields —
/// both the flat `[role] pricing = {...}` shape and the nested
/// `[role.tool_id] pricing = {...}` shape. Empty when the role is undeclared
/// or declares no `pricing` table (never an error here — pricing coverage is
/// a *billing-time* concern, [`ledger::ops::resolve_billing_from_billable_units`]
/// in `kio-pipeline` degrades an uncovered kind to `estimated`, it does not
/// reject the config). Assumes the value already passed
/// [`validate_tools_toml_value`] (so every price is already known-finite and
/// non-negative).
#[must_use]
pub fn declared_pricing_for_role(value: &toml::Value, role: &str) -> BTreeMap<String, f64> {
    let Some(section) = value
        .as_table()
        .and_then(|table| table.get(role))
        .and_then(toml::Value::as_table)
    else {
        return BTreeMap::new();
    };
    let has_direct_fields = section
        .keys()
        .any(|key| TOOLS_ENTRY_FIELDS.contains(&key.as_str()));
    let pricing_table = if has_direct_fields {
        // Single-adapter form: `pricing` sits directly under `[role]`.
        section.get("pricing")
    } else {
        // `[role.tool_id]` form: take the first declared tool_id (MVP: one
        // per role, same assumption as `declared_adapter_for_role`).
        section
            .values()
            .next()
            .and_then(toml::Value::as_table)
            .and_then(|entry| entry.get("pricing"))
    };
    pricing_table
        .and_then(toml::Value::as_table)
        .map(|table| {
            table
                .iter()
                .filter_map(|(kind, price)| {
                    let numeric = price
                        .as_float()
                        .or_else(|| price.as_integer().map(|value| value as f64));
                    numeric.map(|numeric| (kind.clone(), numeric))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn optional_string(table: &toml::value::Table, field: &str) -> Option<String> {
    table
        .get(field)
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
}

fn optional_string_array(table: &toml::value::Table, field: &str) -> Vec<String> {
    table
        .get(field)
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// All configuration that affects adapter execution.  The app loads this from
/// the current device configuration and installs it for one command only;
/// profile identities intentionally remain outside this execution context.
#[derive(Debug, Clone, Default)]
pub struct AdapterRuntimeSettings {
    pub declarations: std::collections::HashMap<String, DeclaredAdapter>,
    pub pricing: std::collections::HashMap<String, BTreeMap<String, f64>>,
    pub execution_timeouts: std::collections::HashMap<String, u64>,
    pub local_peer_ca_path: Option<PathBuf>,
    /// Digest captured from the private local CA at command composition. The
    /// transport compares fresh bytes to this value before every TLS request.
    pub local_peer_trust_digest: Option<String>,
    /// Lifecycle-bound non-secret identity for the managed local CA. This is
    /// distinct from the CA digest so A→B→A never revives a grant for A.
    pub local_peer_trust_binding: Option<String>,
    /// Source path used to re-read literal credentials at the moment they are
    /// used. It is deliberately absent from profile identity state.
    pub tools_toml_path: Option<PathBuf>,
}

thread_local! {
    static ADAPTER_RUNTIME_SETTINGS: RefCell<Vec<AdapterRuntimeSettings>> = const { RefCell::new(Vec::new()) };
}

/// Run one command under its complete adapter configuration.  There is no
/// process-global fallback: an absent context has no adapter authority, and a
/// nested context restores its caller even when the operation unwinds.
pub fn with_runtime_settings<T>(
    settings: AdapterRuntimeSettings,
    operation: impl FnOnce() -> T,
) -> T {
    struct TrustGuard;
    impl Drop for TrustGuard {
        fn drop(&mut self) {
            ADAPTER_RUNTIME_SETTINGS.with(|contexts| {
                let popped = contexts.borrow_mut().pop();
                debug_assert!(popped.is_some(), "adapter runtime context stack underflow");
            });
        }
    }
    ADAPTER_RUNTIME_SETTINGS.with(|contexts| contexts.borrow_mut().push(settings));
    let _guard = TrustGuard;
    operation()
}

/// R13-2: the current command's declared adapter for `role`, if any.
#[must_use]
pub fn registered_declared_adapter(role: &str) -> Option<DeclaredAdapter> {
    ADAPTER_RUNTIME_SETTINGS.with(|contexts| {
        contexts
            .borrow()
            .last()
            .and_then(|settings| settings.declarations.get(role))
            .cloned()
    })
}

/// QA19: the current command's pricing table for `role`.  An absent context or
/// declaration has no price and therefore retains the documented `estimated`
/// billing outcome.
#[must_use]
pub fn registered_declared_pricing(role: &str) -> BTreeMap<String, f64> {
    ADAPTER_RUNTIME_SETTINGS.with(|contexts| {
        contexts
            .borrow()
            .last()
            .and_then(|settings| settings.pricing.get(role))
            .cloned()
            .unwrap_or_default()
    })
}

/// D7: the current command's configured execution timeout, if present.
#[must_use]
pub fn registered_execution_timeout(execution_mode: &str) -> Option<u64> {
    ADAPTER_RUNTIME_SETTINGS.with(|contexts| {
        contexts
            .borrow()
            .last()
            .and_then(|settings| settings.execution_timeouts.get(execution_mode))
            .copied()
    })
}

pub fn registered_local_peer_ca_path() -> Result<PathBuf> {
    ADAPTER_RUNTIME_SETTINGS
        .with(|contexts| {
            contexts
                .borrow()
                .last()
                .and_then(|settings| settings.local_peer_ca_path.clone())
        })
        .ok_or_else(|| AdapterError::LocalPeerConfig {
            code: "KIO-E-LOCAL-PEER-CA-PATH-001",
            message:
                "adapter.policy.offline_api.ca_pem_path is required for an offline_api adapter"
                    .to_owned(),
        })
}

pub fn registered_local_peer_trust_digest() -> Result<String> {
    ADAPTER_RUNTIME_SETTINGS
        .with(|contexts| {
            contexts
                .borrow()
                .last()
                .and_then(|settings| settings.local_peer_trust_digest.clone())
        })
        .ok_or_else(|| AdapterError::LocalPeerConfig {
            code: "KIO-E-LOCAL-PEER-TRUST-CAPTURE-001",
            message: "offline_api adapter trust digest was not captured for this runtime"
                .to_owned(),
        })
}

pub fn registered_local_peer_trust_binding() -> Result<String> {
    ADAPTER_RUNTIME_SETTINGS
        .with(|contexts| {
            contexts
                .borrow()
                .last()
                .and_then(|settings| settings.local_peer_trust_binding.clone())
        })
        .ok_or_else(|| AdapterError::LocalPeerConfig {
            code: "KIO-E-LOCAL-PEER-TRUST-BINDING-001",
            message: "offline_api managed trust lifecycle was not bound for this runtime"
                .to_owned(),
        })
}

pub fn authenticated_local_endpoint(
    role: &str,
    url: &str,
) -> Result<crate::local_peer::AuthenticatedLocalEndpoint> {
    let ca_path = registered_local_peer_ca_path()?;
    let trust_digest = registered_local_peer_trust_digest()?;
    crate::local_peer::AuthenticatedLocalEndpoint::new_bound(url, ca_path, trust_digest).map_err(
        |error| match error {
            AdapterError::LocalPeerConfig { code, message } => AdapterError::LocalPeerConfig {
                code,
                message: format!("local peer for `{role}` is unavailable: {message}"),
            },
            other => other,
        },
    )
}

/// R13-2: resolve the API key for an online adapter `role` — the declared
/// `tools.toml` `auth` (env/plain via [`resolve_auth`]). `None` means the role
/// is undeclared, has no auth, or its declared environment variable is unset.
pub fn resolve_role_api_key(role: &str) -> Result<Option<String>> {
    let Some(settings) = registered_runtime_settings() else {
        return Ok(None);
    };
    let Some(declared) = settings.declarations.get(role).cloned() else {
        return Ok(None);
    };
    validate_declared_runtime_target(role, &declared)?;
    let Some(auth) = declared.auth.as_deref() else {
        return Ok(None);
    };
    if auth.starts_with("env:") {
        return resolve_auth(auth);
    }
    if !auth.starts_with("plain:") {
        return resolve_auth(auth);
    }
    let path =
        settings
            .tools_toml_path
            .as_deref()
            .ok_or_else(|| AdapterError::ConfigSchemaCoded {
                code: TOOLS_PRIVATE_CREDENTIAL_ERROR,
                message: "plain credential has no owner-private tools.toml source".to_owned(),
            })?;
    let bytes = read_private_tools_toml(path)?;
    let value = parse_tools_toml(&bytes)?;
    let verified =
        declared_adapter_for_role(&value, role).ok_or_else(|| AdapterError::ConfigSchemaCoded {
            code: TOOLS_PRIVATE_CREDENTIAL_ERROR,
            message: "owner-private tools.toml no longer declares this adapter credential"
                .to_owned(),
        })?;
    if verified != declared {
        return Err(AdapterError::ConfigSchemaCoded {
            code: TOOLS_PRIVATE_CREDENTIAL_ERROR,
            message: "tools.toml changed after adapter configuration was loaded; retry the command"
                .to_owned(),
        });
    }
    let key = verified
        .auth
        .as_deref()
        .and_then(|value| value.strip_prefix("plain:"))
        .ok_or_else(|| AdapterError::ConfigSchemaCoded {
            code: TOOLS_PRIVATE_CREDENTIAL_ERROR,
            message: "owner-private tools.toml no longer declares a plain credential".to_owned(),
        })?;
    Ok(Some(key.to_owned()))
}

/// Real transport fixtures use a private synthetic credential source, just as
/// production does. Nothing in this helper reads the host's provider keys.
#[cfg(test)]
pub(crate) fn with_test_role_auth<T>(role: &str, operation: impl FnOnce() -> T) -> T {
    use kio_core::store_dir::{Publication, StoreDirectory, restrict_new_private_directory};

    assert!(matches!(role, "markdown" | "embedding"));
    let temporary = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temporary.path()).unwrap();
    let parent = StoreDirectory::open(&root).unwrap();
    let directory = parent.create_directory(Path::new("credentials")).unwrap();
    restrict_new_private_directory(&directory).unwrap();
    let root = root.join("credentials");
    let store = StoreDirectory::from_retained(directory, root.clone()).unwrap();
    let source = format!("[{role}]\nauth = \"plain:test-key\"\n");
    store
        .write_atomic(
            Path::new("tools.toml"),
            source.as_bytes(),
            Publication::CreateOnly,
        )
        .unwrap();
    let value: toml::Value = toml::from_str(&source).unwrap();
    let declarations = std::collections::HashMap::from([(
        role.to_owned(),
        declared_adapter_for_role(&value, role).unwrap(),
    )]);
    with_runtime_settings(
        AdapterRuntimeSettings {
            declarations,
            tools_toml_path: Some(root.join("tools.toml")),
            ..AdapterRuntimeSettings::default()
        },
        operation,
    )
}

fn registered_runtime_settings() -> Option<AdapterRuntimeSettings> {
    ADAPTER_RUNTIME_SETTINGS.with(|contexts| contexts.borrow().last().cloned())
}

pub fn validate_declared_runtime_target(role: &str, declared: &DeclaredAdapter) -> Result<()> {
    // The embedding role resolves against the same target table as the
    // config-load gate. Both must agree: this one runs at execution time
    // (`resolve_role_api_key`, `run_adopted_embedding`), so a relaxation
    // applied only to the other one would accept a declaration at startup and
    // then refuse it at the moment of use.
    let embedding_target = if role == "embedding" {
        Some(declared_embedding_runtime_target(declared)?)
    } else {
        None
    };
    // Resolved for the same reason as the embedding target: this gate runs at
    // execution time, so a relaxation applied only to the config-load gate
    // would accept a declaration at startup and refuse it at the moment of use.
    let markdown_target = if role == "markdown" {
        Some(declared_markdown_runtime_target(declared)?)
    } else {
        None
    };
    if matches!(role, "markdown" | "embedding")
        && (declared.cmd.is_some() || !declared.args.is_empty())
    {
        return Err(AdapterError::ConfigSchema(format!(
            "declared {role} target cannot be executed by the built-in runtime"
        )));
    }
    // A `url` is admissible only on an `offline_api` target, and only after the
    // D1 literal-loopback check — for either role.
    let offline_url_role = match (embedding_target, markdown_target) {
        (Some(target), _) if target.kind == "offline_api" => Some("embedding"),
        (_, Some(target)) if target.kind == "offline_api" => Some("markdown"),
        _ => None,
    };
    match (declared.url.as_deref(), offline_url_role) {
        (Some(url), Some(url_role)) => {
            validate_offline_url(url_role, url)?;
        }
        (Some(_), None) if matches!(role, "markdown" | "embedding") => {
            return Err(AdapterError::ConfigSchema(format!(
                "declared {role} target cannot be executed by the built-in runtime"
            )));
        }
        _ => {}
    }
    let expected_kind = embedding_target
        .map(|target| target.kind)
        .or(markdown_target.map(|target| target.kind))
        .unwrap_or("online_api");
    if matches!(role, "markdown" | "embedding")
        && declared
            .kind
            .as_deref()
            .is_some_and(|kind| kind != expected_kind)
    {
        return Err(AdapterError::ConfigSchema(format!(
            "declared {role} kind does not match effective `{expected_kind}` runtime"
        )));
    }
    if let Some(target) = markdown_target
        && declared
            .model
            .as_deref()
            .is_some_and(|model| !model.starts_with(target.model_prefix))
    {
        return Err(AdapterError::ConfigSchema(format!(
            "declared markdown model does not match effective `{}` runtime",
            target.tool_id
        )));
    }
    if let Some(target) = embedding_target
        && declared
            .model
            .as_deref()
            .is_some_and(|model| model != target.model)
    {
        return Err(AdapterError::ConfigSchema(format!(
            "declared embedding model does not match effective `{}` runtime",
            target.model
        )));
    }
    // The Mistral-prefixed markdown model check that used to live here is gone:
    // the `markdown_target` block above does the same job against the resolved
    // target's prefix. Leaving both in place made the local pipeline pass the
    // first check and fail the second with a message naming Mistral, which is
    // precisely the two-gates-disagree failure this function exists to prevent.
    Ok(())
}

/// R13-2: resolve a `tools.toml` `auth` reference to a concrete API key
/// (docs/07 §1). `env:<NAME>` reads `$NAME`. Literal `plain:` credentials are
/// intentionally refused here because only [`resolve_role_api_key`] can bind
/// them to freshly verified owner-private `tools.toml` bytes. Returns `None`
/// only when `env:<NAME>` names an unset variable.
pub fn resolve_auth(auth: &str) -> Result<Option<String>> {
    if !valid_auth_value(auth) {
        return Err(AdapterError::ConfigSchema(
            "auth must start with env: or plain:".to_owned(),
        ));
    }
    if let Some(name) = auth.strip_prefix("env:") {
        return Ok(std::env::var(name).ok());
    }
    if auth.starts_with("plain:") {
        return Err(AdapterError::ConfigSchemaCoded {
            code: TOOLS_PRIVATE_CREDENTIAL_ERROR,
            message: "plain credentials require an owner-private tools.toml source".to_owned(),
        });
    }
    unreachable!("valid_auth_value accepts only env: and plain:")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn malformed_tools_toml_never_discloses_credential_source() {
        const CANARY: &str = "CANARY_PRIVATE_TOML_CREDENTIAL_9831";
        for source in [
            format!("[markdown]\nauth = \"plain:{CANARY}\n"),
            format!("[markdown]\nauth = \"plain:{CANARY}\\q\"\n"),
        ] {
            for error in [
                super::parse_tools_toml(source.as_bytes()).unwrap_err(),
                super::validate_tools_toml(source.as_bytes()).unwrap_err(),
                super::tools_toml_uses_plain_auth(source.as_bytes()).unwrap_err(),
            ] {
                let message = error.to_string();
                assert!(message.contains("tools.toml contains invalid TOML at byte"));
                assert!(!message.contains(CANARY), "source escaped through Display");
                assert!(
                    !format!("{error:?}").contains(CANARY),
                    "source escaped through Debug"
                );
            }
        }
        assert!(
            super::tools_toml_uses_plain_auth(
                format!("[markdown]\nauth = \"plain:{CANARY}\"\n").as_bytes()
            )
            .unwrap()
        );
    }

    #[test]
    fn local_peer_trust_is_per_invocation_and_none_revokes_it() {
        let first = PathBuf::from("/private/kio-ca-one.pem");
        let second = PathBuf::from("/private/kio-ca-two.pem");
        with_runtime_settings(
            AdapterRuntimeSettings {
                local_peer_ca_path: Some(first.clone()),
                local_peer_trust_digest: Some("sha256:first".to_owned()),
                local_peer_trust_binding: Some("local-ca:v1:g1:sha256:first".to_owned()),
                ..AdapterRuntimeSettings::default()
            },
            || {
                assert_eq!(registered_local_peer_ca_path().unwrap(), first);
                assert_eq!(
                    registered_local_peer_trust_digest().unwrap(),
                    "sha256:first"
                );
                assert_eq!(
                    registered_local_peer_trust_binding().unwrap(),
                    "local-ca:v1:g1:sha256:first"
                );
                with_runtime_settings(
                    AdapterRuntimeSettings {
                        local_peer_ca_path: Some(second.clone()),
                        local_peer_trust_digest: Some("sha256:second".to_owned()),
                        local_peer_trust_binding: Some("local-ca:v1:g2:sha256:second".to_owned()),
                        ..AdapterRuntimeSettings::default()
                    },
                    || {
                        assert_eq!(registered_local_peer_ca_path().unwrap(), second);
                        assert_eq!(
                            registered_local_peer_trust_digest().unwrap(),
                            "sha256:second"
                        );
                        assert_eq!(
                            registered_local_peer_trust_binding().unwrap(),
                            "local-ca:v1:g2:sha256:second"
                        );
                    },
                );
                assert_eq!(registered_local_peer_ca_path().unwrap(), first);
                assert_eq!(
                    registered_local_peer_trust_digest().unwrap(),
                    "sha256:first"
                );
                assert_eq!(
                    registered_local_peer_trust_binding().unwrap(),
                    "local-ca:v1:g1:sha256:first"
                );
            },
        );
        with_runtime_settings(AdapterRuntimeSettings::default(), || {
            assert!(registered_local_peer_ca_path().is_err())
        });
        with_runtime_settings(
            AdapterRuntimeSettings {
                local_peer_ca_path: Some(PathBuf::from("/private/missing-capture.pem")),
                ..AdapterRuntimeSettings::default()
            },
            || {
                let error = authenticated_local_endpoint("embedding", "https://127.0.0.1:8443")
                    .expect_err("runtime local peer must require a captured CA digest");
                assert!(
                    error
                        .to_string()
                        .contains("KIO-E-LOCAL-PEER-TRUST-CAPTURE-001")
                );
            },
        );
        assert!(registered_local_peer_ca_path().is_err());
        assert!(registered_local_peer_trust_digest().is_err());
        assert!(registered_local_peer_trust_binding().is_err());
    }

    #[test]
    fn local_peer_trust_isolated_between_threads() {
        let left = std::thread::spawn(|| {
            let path = PathBuf::from("/private/kio-ca-left.pem");
            with_runtime_settings(
                AdapterRuntimeSettings {
                    local_peer_ca_path: Some(path.clone()),
                    ..AdapterRuntimeSettings::default()
                },
                || registered_local_peer_ca_path().unwrap(),
            )
        });
        let right = std::thread::spawn(|| {
            let path = PathBuf::from("/private/kio-ca-right.pem");
            with_runtime_settings(
                AdapterRuntimeSettings {
                    local_peer_ca_path: Some(path.clone()),
                    ..AdapterRuntimeSettings::default()
                },
                || registered_local_peer_ca_path().unwrap(),
            )
        });
        assert_eq!(
            left.join().unwrap(),
            PathBuf::from("/private/kio-ca-left.pem")
        );
        assert_eq!(
            right.join().unwrap(),
            PathBuf::from("/private/kio-ca-right.pem")
        );
    }

    fn runtime_settings(
        url: &str,
        price: f64,
        timeout: u64,
        ca_path: Option<PathBuf>,
    ) -> AdapterRuntimeSettings {
        let local_peer_trust_digest = ca_path
            .as_deref()
            .map(crate::local_peer::capture_private_local_trust)
            .transpose()
            .expect("runtime local-peer fixture must have a private valid CA");
        let mut declarations = std::collections::HashMap::new();
        declarations.insert(
            "embedding".to_owned(),
            DeclaredAdapter {
                url: Some(url.to_owned()),
                ..DeclaredAdapter::default()
            },
        );
        let mut pricing = std::collections::HashMap::new();
        pricing.insert(
            "embedding".to_owned(),
            BTreeMap::from([("tokens_in".to_owned(), price)]),
        );
        AdapterRuntimeSettings {
            declarations,
            pricing,
            execution_timeouts: std::collections::HashMap::from([(
                "offline_api".to_owned(),
                timeout,
            )]),
            local_peer_ca_path: ca_path,
            local_peer_trust_binding: local_peer_trust_digest
                .as_ref()
                .map(|digest| format!("local-ca:v1:g1:{digest}")),
            local_peer_trust_digest,
            tools_toml_path: None,
        }
    }

    #[test]
    fn runtime_settings_are_scoped_and_restore_all_getters_after_nesting() {
        let outer = runtime_settings("https://127.0.0.1:8443", 1.0, 10, None);
        let inner = runtime_settings("https://127.0.0.1:9443", 2.0, 20, None);
        with_runtime_settings(outer, || {
            assert_eq!(
                registered_declared_adapter("embedding")
                    .unwrap()
                    .url
                    .as_deref(),
                Some("https://127.0.0.1:8443")
            );
            assert_eq!(
                registered_declared_pricing("embedding").get("tokens_in"),
                Some(&1.0)
            );
            assert_eq!(registered_execution_timeout("offline_api"), Some(10));
            with_runtime_settings(inner, || {
                assert_eq!(
                    registered_declared_adapter("embedding")
                        .unwrap()
                        .url
                        .as_deref(),
                    Some("https://127.0.0.1:9443")
                );
                assert_eq!(
                    registered_declared_pricing("embedding").get("tokens_in"),
                    Some(&2.0)
                );
                assert_eq!(registered_execution_timeout("offline_api"), Some(20));
                assert!(registered_local_peer_ca_path().is_err());
                assert!(registered_local_peer_trust_digest().is_err());
            });
            assert_eq!(
                registered_declared_adapter("embedding")
                    .unwrap()
                    .url
                    .as_deref(),
                Some("https://127.0.0.1:8443")
            );
            assert_eq!(
                registered_declared_pricing("embedding").get("tokens_in"),
                Some(&1.0)
            );
            assert_eq!(registered_execution_timeout("offline_api"), Some(10));
            assert!(registered_local_peer_ca_path().is_err());
            assert!(registered_local_peer_trust_digest().is_err());
        });
        assert!(registered_declared_adapter("embedding").is_none());
        assert!(registered_declared_pricing("embedding").is_empty());
        assert_eq!(registered_execution_timeout("offline_api"), None);
        assert!(registered_local_peer_ca_path().is_err());
    }

    #[test]
    fn runtime_settings_are_isolated_between_threads() {
        let left = std::thread::spawn(|| {
            with_runtime_settings(
                runtime_settings("https://127.0.0.1:8443", 1.0, 10, None),
                || {
                    (
                        registered_declared_adapter("embedding").unwrap().url,
                        registered_execution_timeout("offline_api"),
                    )
                },
            )
        });
        let right = std::thread::spawn(|| {
            with_runtime_settings(
                runtime_settings("https://127.0.0.1:9443", 2.0, 20, None),
                || {
                    (
                        registered_declared_adapter("embedding").unwrap().url,
                        registered_execution_timeout("offline_api"),
                    )
                },
            )
        });
        assert_eq!(
            left.join().unwrap(),
            (Some("https://127.0.0.1:8443".to_owned()), Some(10))
        );
        assert_eq!(
            right.join().unwrap(),
            (Some("https://127.0.0.1:9443".to_owned()), Some(20))
        );
    }

    #[test]
    fn non_multimodal_embedding_entry_is_rejected() {
        // 03 §7: 別ベクトル空間 (modality != "multimodal") の embedding profile は
        // tool-lock materialize の時点で採用拒否 (KIO-E-EMBED-MODALITY-001)。
        let value = json!({
            "spec_version": 1,
            "embedding": {
                "tool_id": "some_text_embedding",
                "profile_hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
                "dimensions": 768,
                "distance": "cosine",
                "modality": "text"
            }
        });
        let err = validate_tool_lock_value(&value).expect_err("text modality must be rejected");
        assert!(err.to_string().contains("KIO-E-EMBED-MODALITY-001"));

        let ok = json!({
            "spec_version": 1,
            "embedding": {
                "tool_id": "gemini_embedding_2",
                "profile_hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
                "dimensions": 768,
                "distance": "cosine",
                "modality": "multimodal"
            }
        });
        validate_tool_lock_value(&ok).expect("multimodal modality must be accepted");
    }

    #[test]
    fn reserved_tool_lock_roles_are_rejected() {
        for role in ["summary", "classification", "rerank"] {
            let mut value = json!({ "spec_version": 1 });
            value
                .as_object_mut()
                .unwrap()
                .insert(role.to_owned(), Value::Null);
            let error = validate_tool_lock_value(&value).unwrap_err();
            assert!(error.to_string().contains("unknown tool-lock.json role"));
            let error = canonical_tool_lock_value(&value).unwrap_err();
            assert!(error.to_string().contains("unknown tool-lock.json role"));
        }
    }

    #[test]
    fn placeholder_tool_lock_serializes_spec_version() {
        let lock = ToolLock {
            spec_version: 1,
            prepare: None,
            markdown: None,
            embedding: None,
        };

        let value = serde_json::to_value(lock).expect("serialize tool lock");
        assert_eq!(value["spec_version"], 1);
    }

    #[test]
    fn tool_lock_hash_vector_matches_step2a() {
        let value = json!({
            "spec_version": 1,
            "prepare": {
                "tool_id": "prepare_default",
                "profile_hash": "sha256:20b67a9d7e7e2654379f16f20b445d007e95abac7c8f85d6da65beccff7e6b03",
                "kind": "deterministic_library"
            },
            "markdown": {
                "tool_id": "mistral_ocr_markdownize",
                "profile_hash": "sha256:393d7b062ec1fd573c0a061455bef3f3ee16367378ca4122a0684045178e974c",
                "kind": "online_api",
                "capabilities": ["ocr"]
            },
            "embedding": {
                "tool_id": "gemini_multimodal_embedding",
                "profile_hash": "sha256:c2bda78e217e1f9e12cd17ddac6c46e28a50b8060976f533f76f14193a807226",
                "dimensions": 1536,
                "distance": "cosine",
                "modality": "multimodal",
                "kind": "online_api",
                "mode": "ignored"
            }
        });

        assert_eq!(
            crate::identity::jcs_bytes(&canonical_tool_lock_value(&value).unwrap()).unwrap(),
            br#"{"embedding":{"dimensions":1536,"distance":"cosine","modality":"multimodal","profile_hash":"sha256:c2bda78e217e1f9e12cd17ddac6c46e28a50b8060976f533f76f14193a807226","tool_id":"gemini_multimodal_embedding"},"markdown":{"profile_hash":"sha256:393d7b062ec1fd573c0a061455bef3f3ee16367378ca4122a0684045178e974c","tool_id":"mistral_ocr_markdownize"},"prepare":{"profile_hash":"sha256:20b67a9d7e7e2654379f16f20b445d007e95abac7c8f85d6da65beccff7e6b03","tool_id":"prepare_default"},"spec_version":1}"#
        );
        assert_eq!(
            tool_lock_hash(&value).unwrap(),
            "sha256:cec7965071b40a9431bcb6d17b25b9df7aa1863bacfd3df65c4fb4ed9578572a"
        );
    }

    #[test]
    fn tools_toml_auth_prefix_is_validated() {
        // R13-2: auth prefix validation now lives inside the typed loader and is
        // scoped to the `auth` field of a documented adapter role. (The old
        // `[tools.mistral]` grouping is no longer a recognized section — see
        // `r13_2_typed_loader_*` below.)
        validate_tools_toml(
            br#"[markdown.mistral_ocr_markdownize]
auth = "env:MISTRAL_API_KEY"
"#,
        )
        .unwrap();
        assert!(
            validate_tools_toml(
                br#"[markdown.mistral_ocr_markdownize]
auth = "file:/tmp/key"
"#
            )
            .is_err()
        );
    }

    // R13-2(a): unknown sections/fields and type mismatches are exit-2 schema
    // errors, symmetric with config.toml (which rejected the same shapes while
    // tools.toml silently accepted them at exit 0).
    #[test]
    fn r13_2_typed_loader_rejects_unknown_key_and_type_mismatch() {
        // Unknown key directly under a role, with a scalar value (not a tool_id).
        assert!(validate_tools_toml(b"[markdown]\ntotally_bogus_key=\"xyz\"\n").is_err());
        // Type mismatch: cmd must be a string.
        assert!(validate_tools_toml(b"[markdown.x]\ncmd = 12345\n").is_err());
        // Unknown top-level section.
        assert!(validate_tools_toml(b"[tools.mistral]\nauth = \"env:X\"\n").is_err());
        // Unknown field inside a declared entry.
        assert!(validate_tools_toml(b"[embedding.g]\nbogus = \"y\"\n").is_err());
    }

    // Declared execution targets must match the built-in runtime. Target-rich
    // declarations are rejected until a matching dispatcher exists.
    #[test]
    fn declared_targets_must_match_builtin_runtime() {
        assert!(
            validate_tools_toml(
                br#"[markdown.mistral_ocr_markdownize]
kind = "online_api"
cmd = "uvx kio-mistral-ocr-adapter"
model = "mistral-ocr-latest"
profile_hash = "sha256:..."
capabilities = ["ocr", "layout_detection", "table_extraction"]
"#,
            )
            .is_err()
        );
        validate_tools_toml(
            br#"[markdown.mistral_ocr_markdownize]
kind = "online_api"
model = "mistral-ocr-latest"
auth = "env:MISTRAL_API_KEY"
"#,
        )
        .unwrap();
        validate_tools_toml(b"[embedding.gemini_embedding_2]\nauth = \"env:GEMINI_API_KEY\"\n")
            .unwrap();
        validate_tools_toml(b"[markdown]\nauth = \"plain:sk-secret-key\"\n").unwrap();
        assert!(
            validate_tools_toml(
                b"[embedding.custom]\nurl = \"https://example.test\"\nauth = \"plain:key\"\n"
            )
            .is_err()
        );
    }

    /// An `offline_api` embedding target must name an authenticated HTTPS
    /// loopback peer; locality without server authentication is insufficient.
    #[test]
    fn offline_embedding_accepts_only_a_loopback_url() {
        let entry = |url: &str| {
            format!(
                "[embedding.{LOCAL_EMBEDDING_TOOL_ID}]\n\
                 kind = \"offline_api\"\n\
                 url = \"{url}\"\n\
                 model = \"Qwen/Qwen3-VL-Embedding-2B\"\n\
                 dimensions = 768\n\
                 distance = \"cosine\"\n\
                 modality = \"multimodal\"\n\
                 mode = \"offline\"\n"
            )
        };

        for url in [
            "https://127.0.0.1:8000",
            "https://localhost:8000/v1",
            "https://[::1]:8000",
            "https://127.0.0.1:8443",
        ] {
            validate_tools_toml(entry(url).as_bytes())
                .unwrap_or_else(|error| panic!("{url} must be accepted: {error}"));
        }

        for url in [
            // The reason D1 judges literals: each of these can resolve to
            // loopback at validation time and elsewhere at request time.
            "https://localhost.evil.example:8000",
            "https://127.0.0.1.evil.example:8000",
            // Userinfo puts the real host after the `@`.
            "https://127.0.0.1@evil.example:8000/",
            "https://api.example.com",
            "https://10.0.0.5:8000",
            // A scheme is required so the authority is unambiguous.
            "127.0.0.1:8000",
            "unix:/tmp/kio-embed.sock",
            "http://127.0.0.1:8000",
        ] {
            let error = validate_tools_toml(entry(url).as_bytes())
                .expect_err(&format!("{url} must be rejected"));
            assert!(
                error.to_string().contains("KIO-E-LOCAL-PEER-URL-001"),
                "{url} must be refused as a non-loopback target, got: {error}"
            );
        }
    }

    /// The relaxation is scoped to `offline_api`. An online target still cannot
    /// name a `url`, and neither kind may name a `cmd`/`args` (05 §5: Kio never
    /// spawns a process; the external dispatcher is future work, 07 §7).
    #[test]
    fn offline_relaxation_does_not_leak_to_other_targets() {
        assert!(
            validate_tools_toml(
                b"[embedding.gemini_embedding_2]\nurl = \"http://127.0.0.1:8000\"\n"
            )
            .is_err()
        );
        let cmd_entry = format!(
            "[embedding.{LOCAL_EMBEDDING_TOOL_ID}]\n\
             kind = \"offline_api\"\n\
             cmd = \"vllm\"\n"
        );
        assert!(validate_tools_toml(cmd_entry.as_bytes()).is_err());
        // Stage 3 added an offline markdown target, and this must stay failing:
        // the relaxation is per-target, not per-role. Mistral OCR is a cloud
        // API, so declaring it `offline_api` is a claim about the network that
        // is simply untrue.
        assert!(
            validate_tools_toml(b"[markdown.mistral_ocr_markdownize]\nkind = \"offline_api\"\n")
                .is_err()
        );
        // Symmetrically, the local pipeline may not be declared online.
        let online_local = format!("[markdown.{LOCAL_OCR_TOOL_ID}]\nkind = \"online_api\"\n");
        assert!(validate_tools_toml(online_local.as_bytes()).is_err());
        // And an online markdown target still cannot name a url.
        assert!(
            validate_tools_toml(
                b"[markdown.mistral_ocr_markdownize]\nurl = \"http://127.0.0.1:8118\"\n"
            )
            .is_err()
        );
        let cmd_local = format!(
            "[markdown.{LOCAL_OCR_TOOL_ID}]\nkind = \"offline_api\"\ncmd = \"paddleocr\"\n"
        );
        assert!(validate_tools_toml(cmd_local.as_bytes()).is_err());
    }

    /// The markdown role uses the same authenticated-peer URL contract as
    /// embedding, resolved from the same target table.
    #[test]
    fn offline_markdown_accepts_only_a_loopback_url() {
        let entry = |url: &str| {
            format!(
                "[markdown.{LOCAL_OCR_TOOL_ID}]\n\
                 kind = \"offline_api\"\n\
                 url = \"{url}\"\n\
                 model = \"PaddleOCR-VL-0.9B\"\n"
            )
        };

        for url in [
            "https://127.0.0.1:8080",
            "https://localhost:8080",
            "https://[::1]:8080",
        ] {
            validate_tools_toml(entry(url).as_bytes())
                .unwrap_or_else(|error| panic!("{url} must be accepted: {error}"));
        }

        for url in [
            "https://localhost.evil.example:8080",
            "https://api.example.com",
            "https://10.0.0.5:8080",
            "http://127.0.0.1:8080",
            "unix:/tmp/kio-ocr.sock",
        ] {
            let error = validate_tools_toml(entry(url).as_bytes())
                .expect_err(&format!("{url} must be rejected"));
            assert!(
                error.to_string().contains("KIO-E-LOCAL-PEER-URL-001"),
                "{url} must be refused as a non-loopback target, got: {error}"
            );
        }
    }

    /// The model name is checked by prefix because upstream versions it in
    /// place — `PaddleOCR-VL-0.9B` became `PaddleOCR-VL-1.6-0.9B` — while
    /// 03 §5.1 pins the weights by digest. A name from the *other* target is
    /// still refused, which is what stops a copy-pasted Mistral entry from
    /// being accepted as the local one.
    #[test]
    fn markdown_models_are_matched_against_their_own_target() {
        for model in ["PaddleOCR-VL-0.9B", "PaddleOCR-VL-1.6-0.9B"] {
            let entry = format!(
                "[markdown.{LOCAL_OCR_TOOL_ID}]\nkind = \"offline_api\"\nmodel = \"{model}\"\n"
            );
            validate_tools_toml(entry.as_bytes())
                .unwrap_or_else(|error| panic!("{model} must be accepted: {error}"));
        }
        let crossed = format!(
            "[markdown.{LOCAL_OCR_TOOL_ID}]\nkind = \"offline_api\"\nmodel = \"mistral-ocr-2505\"\n"
        );
        assert!(validate_tools_toml(crossed.as_bytes()).is_err());
        assert!(
            validate_tools_toml(
                b"[markdown.mistral_ocr_markdownize]\nmodel = \"PaddleOCR-VL-0.9B\"\n"
            )
            .is_err()
        );
    }

    /// An unknown markdown tool_id must name what this build *can* run, the
    /// way the embedding role's error does — otherwise the operator learns only
    /// that their name was wrong, not what to write instead.
    #[test]
    fn an_unknown_markdown_tool_id_lists_the_supported_ones() {
        let error = validate_tools_toml(b"[markdown.some_other_ocr]\nkind = \"offline_api\"\n")
            .expect_err("unknown tool_id must be rejected");
        let message = error.to_string();
        assert!(message.contains("mistral_ocr_markdownize"), "{message}");
        assert!(message.contains(LOCAL_OCR_TOOL_ID), "{message}");
    }

    /// The execution-time gate must reach the same verdict as the config-load
    /// gate. When it did not, a declaration accepted at startup was refused at
    /// the moment of use.
    #[test]
    fn declared_runtime_target_agrees_with_the_config_gate() {
        let offline = |url: &str| DeclaredAdapter {
            tool_id: Some(LOCAL_EMBEDDING_TOOL_ID.to_owned()),
            kind: Some("offline_api".to_owned()),
            url: Some(url.to_owned()),
            model: Some("Qwen/Qwen3-VL-Embedding-2B".to_owned()),
            ..DeclaredAdapter::default()
        };
        validate_declared_runtime_target("embedding", &offline("https://127.0.0.1:8000")).unwrap();
        let error =
            validate_declared_runtime_target("embedding", &offline("https://api.example.com"))
                .expect_err("a remote url must be refused at execution time too");
        assert!(error.to_string().contains("KIO-E-LOCAL-PEER-URL-001"));

        // The online target is unchanged: still no url, still Gemini's model.
        let online_with_url = DeclaredAdapter {
            tool_id: Some("gemini_embedding_2".to_owned()),
            url: Some("http://127.0.0.1:8000".to_owned()),
            ..DeclaredAdapter::default()
        };
        assert!(validate_declared_runtime_target("embedding", &online_with_url).is_err());
        let wrong_model = DeclaredAdapter {
            tool_id: Some(LOCAL_EMBEDDING_TOOL_ID.to_owned()),
            kind: Some("offline_api".to_owned()),
            model: Some("gemini-embedding-2".to_owned()),
            ..DeclaredAdapter::default()
        };
        assert!(validate_declared_runtime_target("embedding", &wrong_model).is_err());
    }

    /// Stage 3's half of the same agreement. The execution-time gate is the one
    /// that runs at the moment a document is sent, so a markdown relaxation
    /// applied only to the config-load gate would accept the local pipeline at
    /// startup and then refuse it mid-index.
    #[test]
    fn declared_markdown_target_agrees_with_the_config_gate() {
        let offline = |url: &str| DeclaredAdapter {
            tool_id: Some(LOCAL_OCR_TOOL_ID.to_owned()),
            kind: Some("offline_api".to_owned()),
            url: Some(url.to_owned()),
            model: Some("PaddleOCR-VL-0.9B".to_owned()),
            ..DeclaredAdapter::default()
        };
        validate_declared_runtime_target("markdown", &offline("https://127.0.0.1:8080")).unwrap();
        let error =
            validate_declared_runtime_target("markdown", &offline("https://ocr.example.com"))
                .expect_err("a remote url must be refused at execution time too");
        assert!(error.to_string().contains("KIO-E-LOCAL-PEER-URL-001"));

        // The online markdown target is unchanged: still no url.
        let online_with_url = DeclaredAdapter {
            tool_id: Some("mistral_ocr_markdownize".to_owned()),
            url: Some("http://127.0.0.1:8080".to_owned()),
            ..DeclaredAdapter::default()
        };
        assert!(validate_declared_runtime_target("markdown", &online_with_url).is_err());

        let crossed_model = DeclaredAdapter {
            tool_id: Some(LOCAL_OCR_TOOL_ID.to_owned()),
            kind: Some("offline_api".to_owned()),
            model: Some("mistral-ocr-2505".to_owned()),
            ..DeclaredAdapter::default()
        };
        assert!(validate_declared_runtime_target("markdown", &crossed_model).is_err());

        let unknown = DeclaredAdapter {
            tool_id: Some("some_other_ocr".to_owned()),
            ..DeclaredAdapter::default()
        };
        assert!(validate_declared_runtime_target("markdown", &unknown).is_err());
    }

    #[test]
    fn auth_prefix_and_target_fields_are_validated_independently() {
        assert!(
            validate_tools_toml(b"[markdown.mistral_ocr_markdownize]\nurl = \"plain:\"\n").is_err()
        );
        assert!(
            validate_tools_toml(b"[markdown.mistral_ocr_markdownize]\nmodel = \"keychain:\"\n")
                .is_err()
        );
        assert!(
            validate_tools_toml(b"[markdown.mistral_ocr_markdownize]\nauth = \"file:/tmp/key\"\n")
                .is_err()
        );
    }

    // Direct auth resolution supports only environment references. Plaintext
    // values must pass through the private-tools source guard below.
    #[cfg(debug_assertions)]
    #[test]
    fn resolve_auth_accepts_only_env_references() {
        let _lock = kio_core::test_control::test_env_lock().lock().unwrap();
        {
            let _auth =
                kio_core::test_control::TestEnvGuard::set("KIO_TEST_R13_2_AUTH", "resolved-key");
            assert_eq!(
                resolve_auth("env:KIO_TEST_R13_2_AUTH").unwrap(),
                Some("resolved-key".to_owned())
            );
        }
        assert_eq!(resolve_auth("env:KIO_TEST_R13_2_AUTH").unwrap(), None);
        assert!(matches!(
            resolve_auth("plain:abc123"),
            Err(AdapterError::ConfigSchemaCoded {
                code: TOOLS_PRIVATE_CREDENTIAL_ERROR,
                ..
            })
        ));
        assert!(matches!(
            resolve_auth("keychain:login"),
            Err(AdapterError::ConfigSchema(_))
        ));
        assert!(matches!(
            resolve_auth("env:"),
            Err(AdapterError::ConfigSchema(_))
        ));
        assert!(matches!(
            resolve_auth("plain:"),
            Err(AdapterError::ConfigSchema(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn plain_role_key_is_rederived_from_private_tools_bytes_at_use_time() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        // macOS may spell the same temporary directory through `/var` or
        // `/private/var`; keep the test's path spelling aligned with the
        // retained private-file reader and make both relevant objects private.
        let private_directory = std::fs::canonicalize(directory.path()).unwrap();
        std::fs::set_permissions(&private_directory, std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let path = private_directory.join("tools.toml");
        let source = b"[embedding.gemini_embedding_2]\nauth = \"plain:verified-key\"\n";
        std::fs::write(&path, source).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let value: toml::Value = toml::from_str(std::str::from_utf8(source).unwrap()).unwrap();
        let declared = declared_adapter_for_role(&value, "embedding").unwrap();
        let declarations = std::collections::HashMap::from([("embedding".to_owned(), declared)]);
        with_runtime_settings(
            AdapterRuntimeSettings {
                declarations,
                tools_toml_path: Some(path.clone()),
                ..AdapterRuntimeSettings::default()
            },
            || {
                assert_eq!(
                    resolve_role_api_key("embedding").unwrap(),
                    Some("verified-key".to_owned())
                );
                std::fs::write(
                    &path,
                    b"[embedding.gemini_embedding_2]\nauth = \"plain:replacement-key\"\n",
                )
                .unwrap();
                assert!(matches!(
                    resolve_role_api_key("embedding"),
                    Err(AdapterError::ConfigSchemaCoded {
                        code: TOOLS_PRIVATE_CREDENTIAL_ERROR,
                        ..
                    })
                ));
                std::fs::write(&path, source).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
                assert!(matches!(
                    resolve_role_api_key("embedding"),
                    Err(AdapterError::ConfigSchemaCoded {
                        code: TOOLS_PRIVATE_CREDENTIAL_ERROR,
                        ..
                    })
                ));
            },
        );
    }

    #[test]
    fn tool_lock_entry_rejects_unknown_fields_before_canonicalization() {
        for role in ["prepare", "markdown", "embedding"] {
            let mut value = json!({
                "spec_version": 1,
                "prepare": { "tool_id": "prepare", "profile_hash": "sha256:test" },
                "markdown": { "tool_id": "markdown", "profile_hash": "sha256:test" },
                "embedding": {
                    "tool_id": "embedding",
                    "profile_hash": "sha256:test",
                    "dimensions": 1,
                    "distance": "cosine",
                    "modality": "multimodal"
                }
            });
            value[role]["auth"] = json!("plain:shared-secret");
            let error = canonical_tool_lock_value(&value).unwrap_err();
            assert!(error.to_string().contains(&format!("{role}.auth")));
            assert!(load_tool_lock(serde_json::to_vec(&value).unwrap().as_slice()).is_err());
        }
    }

    #[test]
    fn r13_2_declared_adapter_for_role_reads_model_and_auth() {
        let value: toml::Value = toml::from_str(
            "[embedding.gemini_embedding_2]\nmodel = \"gemini-embedding-2\"\nauth = \"env:MY_KEY\"\n",
        )
        .unwrap();
        let declared = declared_adapter_for_role(&value, "embedding").unwrap();
        assert_eq!(declared.tool_id.as_deref(), Some("gemini_embedding_2"));
        assert_eq!(declared.model.as_deref(), Some("gemini-embedding-2"));
        assert_eq!(declared.auth.as_deref(), Some("env:MY_KEY"));
        // Single-adapter form (fields directly under the role).
        let flat: toml::Value = toml::from_str("[markdown]\nauth = \"plain:k\"\n").unwrap();
        let declared = declared_adapter_for_role(&flat, "markdown").unwrap();
        assert_eq!(declared.tool_id, None);
        assert_eq!(declared.auth.as_deref(), Some("plain:k"));
        assert_eq!(declared_adapter_for_role(&flat, "embedding"), None);
    }

    #[test]
    fn declared_adapter_projection_preserves_target_for_runtime_recheck() {
        let value: toml::Value = toml::from_str(
            "[embedding.custom]\nkind = \"online_api\"\nurl = \"https://example.test\"\nmodel = \"custom-v1\"\nauth = \"plain:synthetic\"\n",
        )
        .unwrap();
        let declared = declared_adapter_for_role(&value, "embedding").unwrap();
        assert_eq!(declared.tool_id.as_deref(), Some("custom"));
        assert_eq!(declared.url.as_deref(), Some("https://example.test"));
        assert!(validate_declared_runtime_target("embedding", &declared).is_err());
    }

    // ------------------------------------------------------------------
    // QA19 (step4b-contract-tests-p3a.md §F): tools.toml `[pricing]`.
    // ------------------------------------------------------------------

    /// `PRICING_KIND_ENUM` must stay byte-for-byte identical to
    /// `types::BillableUnitKind`'s serialized variant names — the two lists
    /// are independently maintained (this crate has no reason to derive one
    /// from the other via serde at const-eval time) and this test is the
    /// guard against them drifting apart.
    #[test]
    fn pricing_kind_enum_matches_billable_unit_kind() {
        use crate::types::BillableUnitKind;
        let mut from_enum: Vec<String> = [
            BillableUnitKind::Pages,
            BillableUnitKind::TokensIn,
            BillableUnitKind::TokensOut,
        ]
        .iter()
        .map(|kind| {
            serde_json::to_value(kind)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
        from_enum.sort_unstable();
        let mut pricing_enum: Vec<String> = PRICING_KIND_ENUM
            .iter()
            .map(|kind| (*kind).to_owned())
            .collect();
        pricing_enum.sort_unstable();
        assert_eq!(from_enum, pricing_enum);
    }

    /// QA19: the spec's literal example (03 §11 L832-837:
    /// `[markdown.mistral_ocr_markdownize.pricing] pages = 0.004`) is
    /// accepted — before this fix `pricing` fell through the closed
    /// `TOOLS_ENTRY_FIELDS` list and was rejected as an unknown field.
    #[test]
    fn qa19_spec_pricing_example_is_accepted() {
        validate_tools_toml(
            br#"[markdown.mistral_ocr_markdownize]
kind = "online_api"

[markdown.mistral_ocr_markdownize.pricing]
pages = 0.004
"#,
        )
        .unwrap();
        // The single-adapter flat form too.
        validate_tools_toml(b"[embedding]\n[embedding.pricing]\ntokens_in = 0.00000015\n").unwrap();
    }

    /// QA19: an unknown pricing kind, a wrong-typed price, and a
    /// negative/non-finite price are all `KIO-E-CONFIG-SCHEMA-001` — the
    /// strict-schema posture (R13-2) extends to `[pricing]`, it is not a
    /// silently-ignored/best-effort table.
    #[test]
    fn qa19_pricing_rejects_unknown_kind_and_bad_values() {
        assert!(
            validate_tools_toml(b"[markdown.mistral_ocr_markdownize.pricing]\nbogus_kind = 0.01\n")
                .is_err()
        );
        assert!(
            validate_tools_toml(b"[markdown.mistral_ocr_markdownize.pricing]\npages = \"0.004\"\n")
                .is_err()
        );
        assert!(
            validate_tools_toml(b"[markdown.mistral_ocr_markdownize.pricing]\npages = -0.004\n")
                .is_err()
        );
        assert!(
            validate_tools_toml(b"[markdown.mistral_ocr_markdownize.pricing]\npages = nan\n")
                .is_err()
        );
        // A plain (non-table) `pricing` value is also rejected, not silently
        // dropped.
        assert!(
            validate_tools_toml(b"[markdown.mistral_ocr_markdownize]\npricing = \"cheap\"\n")
                .is_err()
        );
        // A whole-integer TOML price (no decimal point) is valid.
        validate_tools_toml(b"[markdown.mistral_ocr_markdownize.pricing]\npages = 0\n").unwrap();
    }

    #[test]
    fn declared_pricing_for_role_reads_both_toml_shapes() {
        let nested: toml::Value =
            toml::from_str("[markdown.mistral_ocr_markdownize.pricing]\npages = 0.004\n").unwrap();
        let pricing = declared_pricing_for_role(&nested, "markdown");
        assert_eq!(pricing.get("pages"), Some(&0.004));

        let flat: toml::Value =
            toml::from_str("[embedding.pricing]\ntokens_in = 0.00000015\n").unwrap();
        let pricing = declared_pricing_for_role(&flat, "embedding");
        assert_eq!(pricing.get("tokens_in"), Some(&0.00000015));

        // No `pricing` table declared, or role absent entirely -> empty, not
        // an error/panic.
        let none: toml::Value = toml::from_str("[markdown.mistral_ocr_markdownize]\n").unwrap();
        assert!(declared_pricing_for_role(&none, "markdown").is_empty());
        assert!(declared_pricing_for_role(&none, "embedding").is_empty());
    }

    #[test]
    fn declared_pricing_for_role_accepts_integer_prices() {
        let value: toml::Value =
            toml::from_str("[markdown.mistral_ocr_markdownize.pricing]\npages = 0\n").unwrap();
        let pricing = declared_pricing_for_role(&value, "markdown");
        assert_eq!(pricing.get("pages"), Some(&0.0));
    }
}
