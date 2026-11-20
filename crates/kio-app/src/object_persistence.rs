use crate::ledger_access::ledger_snapshot_to_kio;
use kio_core::ExitCode;
use kio_core::KioError;
use kio_core::Result;
use kio_core::cas::ContentObjectKind;
use kio_core::scope::Repository;
use kio_core::scope::new_ulid;
use kio_pipeline::markdownize::NormalizedInstanceManifest;
use kio_pipeline::markdownize::{
    NormalizedInstanceIdentity, persist_resumable_normalized_units,
    read_normalized_instance_manifest, read_validated_normalized_unit,
    visit_validated_normalized_units,
};
use kio_pipeline::task::normalized_task_output_ref;
use serde_json::json;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::Path;

/// Publish mutable Kio-owned metadata through its retained private directory.
/// Every caller targets tool-lock.json or promotion-state.json inside `.kio`.
/// Existing unsafe metadata is refused rather than replaced to repair access.
pub(crate) fn atomic_overwrite_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| KioError::io("path has no parent", path.display().to_string()))?;
    let leaf = path
        .file_name()
        .ok_or_else(|| KioError::io("path has no leaf", path.display().to_string()))?;
    // The store must already exist. Do not recreate a disappeared .kio or
    // traverse a replaced name to initialize new metadata there.
    let directory = kio_core::store_dir::StoreDirectory::open(parent)?;
    kio_core::private_fs::verify_private_directory_handle(&directory)?;
    if directory.contains_entry(Path::new(leaf))? {
        directory.ensure_owner_private(Path::new(leaf))?;
    }
    directory.write_atomic(
        Path::new(leaf),
        bytes,
        kio_core::store_dir::Publication::Upsert,
    )?;
    crate::platform::verify_private_state_named_identity(&directory, parent)
}

pub(crate) fn copy_normalized_instance_gen(
    repo: &Repository,
    raw_hash: &str,
    normalize: &kio_core::dag::NormalizeRef,
    new_gen: u64,
) -> Result<String> {
    let source = crate::historical_reindex::pinned_manifest(repo, raw_hash, normalize)?;
    let directory = bound_extraction_store(repo)?;
    let old_dir = kio_pipeline::markdownize::normalized_instance_dir(
        repo.kio_dir(),
        raw_hash,
        &normalize.tool_profile_hash,
        normalize.r#gen,
    );
    if let Ok(relative) = old_dir.strip_prefix(repo.kio_dir())
        && let Ok(handle) = directory.open_directory(relative)
    {
        let old = kio_core::store_dir::StoreDirectory::from_retained_with_control(
            handle,
            old_dir,
            repo.control().clone(),
        )?;
        if let Ok(entries) = old.entries(Path::new("")) {
            for entry in entries {
                if entry.is_regular_file && entry.name.to_str().is_some_and(is_orphan_temp_name) {
                    old.remove_file(Path::new(&entry.name))?;
                }
            }
        }
    }
    let entries = source
        .units
        .iter()
        .map(|entry| (entry.unit_key.as_str(), entry))
        .collect::<BTreeMap<_, _>>();
    let identity = NormalizedInstanceIdentity {
        raw_hash: source.raw_hash.clone(),
        tool_profile_hash: source.tool_profile_hash.clone(),
        r#gen: normalize.r#gen,
    };
    let mut target = source.clone();
    target.parent_gen = Some(normalize.r#gen);
    target.r#gen = new_gen;
    target.run_id = format!("run_{}", new_ulid(repo.kio_dir()));
    target.generated_at = repo.control().now_utc_seconds();
    let store = repo.object_store();
    let stamped = persist_resumable_normalized_units(
        &directory,
        &target,
        &normalize.manifest_hash,
        false,
        |draft, entry| {
            let mut unit = read_validated_normalized_unit(
                &store,
                &identity,
                entries[entry.unit_key.as_str()],
            )?;
            unit.r#gen = new_gen;
            unit.generated_at.clone_from(&draft.generated_at);
            Ok(Cow::Owned(unit))
        },
    )
    .map_err(pipeline_to_kio)?;
    published_manifest_hash(repo, &stamped)
}

/// Whether `name` matches the normalized-unit file naming convention written by
/// `persist_normalized_instance`: a 16-hex-char `unit_ref` digest plus `.json`
/// (e.g. `1a2b3c4d5e6f7089.json`). Anything else in a gen dir is not store truth
/// (R9-5) — `manifest.json`, a crashed writer's `.tmp-*`, a `.DS_Store`.
#[cfg(test)]
pub(crate) fn is_normalized_unit_file(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".json") else {
        return false;
    };
    stem.len() == 16
        && stem
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Whether `name` looks like one of Kio's own atomic-write temp files
/// (`.tmp-<pid>-...` or `.<name>.tmp-<pid>-...`). Used by R9-5 to GC a
/// crash-orphaned temp left inside a gen dir by a killed writer.
pub(crate) fn is_orphan_temp_name(name: &str) -> bool {
    name.starts_with('.') && name.contains(".tmp-")
}

pub(crate) fn index_to_kio(error: kio_index::IndexError) -> KioError {
    KioError::schema(error.to_string())
}

pub(crate) fn search_to_kio(error: kio_search::SearchError) -> KioError {
    match error {
        kio_search::SearchError::Cursor(message) => KioError::new(
            "KIO-E-SEARCH-CURSOR-001",
            message,
            json!({}),
            ExitCode::InvalidUsage,
        ),
        kio_search::SearchError::Evidence(message) => KioError::schema(message),
        kio_search::SearchError::Contract(message) => KioError::schema(message),
    }
}

pub(crate) fn bound_extraction_store(
    repo: &Repository,
) -> Result<kio_core::store_dir::StoreDirectory> {
    let handle = repo.bound_kio_handle().ok_or_else(|| {
        KioError::schema("managed extraction is missing its retained store handle")
    })?;
    kio_core::store_dir::StoreDirectory::from_retained_with_control(
        handle.try_clone().map_err(|error| {
            KioError::io(error.to_string(), repo.kio_dir().display().to_string())
        })?,
        repo.kio_dir().to_owned(),
        repo.control().clone(),
    )
}

pub(crate) fn normalized_output_ref(
    _repo: &Repository,
    raw_hash: &str,
    tool_profile_hash: &str,
    r#gen: u64,
) -> String {
    normalized_task_output_ref(raw_hash, tool_profile_hash, r#gen)
}

/// Resolve the current manifest through authenticated normalized storage.
pub(crate) fn compute_manifest_hash(
    repo: &Repository,
    raw_hash: &str,
    tool_profile_hash: &str,
    r#gen: u64,
) -> Result<String> {
    let (manifest, _) =
        read_normalized_instance_manifest(repo.kio_dir(), raw_hash, tool_profile_hash, r#gen)
            .map_err(pipeline_to_kio)?;
    visit_validated_normalized_units(&repo.object_store(), &manifest, |_| Ok(()))
        .map_err(pipeline_to_kio)?;
    published_manifest_hash(repo, &manifest)
}

/// Verify and sync the immutable manifest already published by normalization.
pub(crate) fn published_manifest_hash(
    repo: &Repository,
    manifest: &NormalizedInstanceManifest,
) -> Result<String> {
    let directory = bound_extraction_store(repo)?;
    kio_pipeline::markdownize::adopt_normalized_instance(
        &directory,
        &manifest.raw_hash,
        &manifest.tool_profile_hash,
        manifest.r#gen,
    )
    .map_err(pipeline_to_kio)?;
    let hash =
        kio_pipeline::markdownize::normalized_manifest_hash(manifest).map_err(pipeline_to_kio)?;
    if !repo.object_store().publish_content_stage(
        ContentObjectKind::Manifest,
        &hash,
        &directory,
        None,
    )? {
        return Err(KioError::not_found(&hash));
    }
    Ok(hash)
}

pub(crate) fn adapter_to_kio(error: kio_adapter::AdapterError) -> KioError {
    match error {
        kio_adapter::AdapterError::ResourceLimit(limit) => {
            limit.into_error("adapter resource exhausted")
        }
        kio_adapter::AdapterError::RuntimeConfiguration { role, error } => {
            let mut context = if error.context().is_object() {
                error.context().clone()
            } else {
                json!({"source_context": error.context()})
            };
            if let Some(context) = context.as_object_mut() {
                context.insert("dependency".into(), json!("adapter_configuration"));
                context.insert("role".into(), json!(role));
            }
            KioError::new(
                error.error_code(),
                error.message(),
                context,
                error.exit_code(),
            )
        }
        kio_adapter::AdapterError::RuntimeTrust(error) => {
            let mut context = if error.context().is_object() {
                error.context().clone()
            } else {
                json!({"source_context": error.context()})
            };
            if let Some(context) = context.as_object_mut() {
                context.insert("dependency".to_owned(), json!("managed_https_trust"));
            }
            KioError::new(
                error.error_code(),
                error.message(),
                context,
                error.exit_code(),
            )
        }

        kio_adapter::AdapterError::LocalPeerAuth(message) => KioError::new(
            "KIO-E-LOCAL-PEER-AUTH-001",
            message,
            json!({}),
            ExitCode::AuthError,
        ),
        kio_adapter::AdapterError::LocalPeerConfig { code, message } => {
            KioError::new(code, message, json!({}), ExitCode::InvalidUsage)
        }
        // A config violation that named its own operator-facing code (06 §8).
        // `AdapterError::error_code()` cannot carry it — that one is pinned to
        // the retry-policy table — so the code travels on the variant and is
        // read structurally here rather than sniffed out of the message text.
        // Every code in this class is exit 2.
        kio_adapter::AdapterError::ConfigSchemaCoded { code, message } => {
            KioError::new(code, message, json!({}), ExitCode::InvalidUsage)
        }
        other => KioError::schema(other.to_string()),
    }
}

pub(crate) fn pipeline_to_kio(error: kio_pipeline::PipelineError) -> KioError {
    match error {
        kio_pipeline::PipelineError::ResourceLimit(limit) => {
            limit.into_error("pipeline resource exhausted")
        }
        // M1(c): a corrupt persisted store file is exit 4 (KIO-E-STORE-CORRUPT-001),
        // not a schema/config error (exit 2). The path is preserved in context.
        kio_pipeline::PipelineError::Corrupt { path, message } => KioError::new(
            "KIO-E-STORE-CORRUPT-001",
            format!("corrupt store file at {path}: {message}"),
            json!({ "path": path }),
            ExitCode::PermanentFailure,
        ),
        // P1: a task whose input_path escapes the scope is KIO-E-STORE-PATH-001
        // (exit 2), the same contract as an out-of-scope tree entry path. The
        // offending path stays in context (redacted in logs), never in the message.
        kio_pipeline::PipelineError::Path { path } => KioError::new(
            "KIO-E-STORE-PATH-001",
            "task input_path must be a scope-local file name (no separators or `..` traversal)",
            json!({ "path": path }),
            ExitCode::InvalidUsage,
        ),
        kio_pipeline::PipelineError::Locked { path } => KioError::locked(path),
        kio_pipeline::PipelineError::Io { path, message } => KioError::io(message, path),
        kio_pipeline::PipelineError::LedgerSnapshot { path, source } => {
            ledger_snapshot_to_kio(&path, source)
        }
        // Lifecycle errors distinguish an explicit setup operation, recoverable
        // incomplete state, and unsafe durable state. A caller must not retry
        // a missing authority as if it were a transient provider failure.
        kio_pipeline::PipelineError::Contract { code, message } => {
            let exit = if matches!(
                code,
                "KIO-E-LEDGER-UNINITIALIZED-001"
                    | "KIO-E-LEDGER-ALREADY-EXISTS-001"
                    | "KIO-E-LEDGER-PATH-001"
            ) {
                ExitCode::InvalidUsage
            } else if matches!(
                code,
                "KIO-E-LEDGER-INIT-PARTIAL-001" | "KIO-E-LEDGER-SEQUENCE-MISMATCH-001"
            ) {
                ExitCode::PartialFailure
            } else if code.starts_with("KIO-E-LEDGER-") || code == "KIO-E-STORE-CONSTRAINT-001" {
                ExitCode::PermanentFailure
            } else {
                ExitCode::Failure
            };
            KioError::new(code, message, json!({}), exit)
        }
        other => KioError::schema(other.to_string()),
    }
}

pub(crate) fn diagnostic_safe_text(input: &str, allow_newlines: bool) -> String {
    let mut output = String::with_capacity(input.len());
    for ch in input.chars() {
        if allow_newlines && ch == '\n' {
            output.push(ch);
            continue;
        }
        let code = ch as u32;
        let terminal_active = matches!(code, 0x00..=0x1f | 0x7f..=0x9f)
            || matches!(code, 0x061c | 0x200e..=0x200f | 0x2028..=0x202e | 0x2066..=0x2069);
        if terminal_active {
            if code <= 0xff {
                output.push_str(&format!("\\x{code:02x}"));
            } else {
                output.push_str(&format!("\\u{{{code:04x}}}"));
            }
        } else {
            output.push(ch);
        }
    }
    output
}
