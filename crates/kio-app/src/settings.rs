use crate::adapter_to_kio;
use crate::batch_recovery_key;
use crate::cursor_key;
use crate::declared_embedding_profile;
use crate::search::query::toml_f64;
use crate::search::query::toml_u64;
use kio_adapter::catalog::EmbeddingExecution;
use kio_adapter::tool_lock::MAX_TOOLS_TOML_BYTES;
use kio_adapter::tool_lock::load_tool_lock;
use kio_adapter::tool_lock::parse_tools_toml;
use kio_adapter::tool_lock::read_private_tools_toml_with_control;
use kio_adapter::tool_lock::tools_toml_uses_plain_auth;
use kio_adapter::tool_lock::validate_tools_toml;
use kio_adapter::types::ExecutionMode;
use kio_core::KioError;
use kio_core::Result;
use kio_core::cas::read_bounded_regular_file;
use kio_core::control::ControlHandle;
use kio_core::schema::SchemaKind;
use kio_core::schema::validate_json_schema;
use kio_core::scope::Repository;
use kio_index::embedding_store;
use kio_pipeline::ledger::LedgerDb;
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

/// Documented default for `adapter.policy.max_input_bytes` (07 §7): 100 MB.
const DEFAULT_MAX_INPUT_BYTES: u64 = 104_857_600;

/// R12-2: effective `adapter.policy.max_input_bytes` — scope config wins over user
/// config, default 100 MB (07 §7). Enforced as an input gate in `run_index_pipeline`.
pub(crate) fn effective_max_input_bytes(repo: &Repository) -> u64 {
    read_max_input_bytes_config(&repo.kio_dir().join("config.toml"))
        .or_else(|| read_max_input_bytes_config(&user_config_toml_path(repo.control())))
        .unwrap_or(DEFAULT_MAX_INPUT_BYTES)
}

fn read_max_input_bytes_config(path: &Path) -> Option<u64> {
    let text = fs::read_to_string(path).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    value
        .get("adapter")
        .and_then(|adapter| adapter.get("policy"))
        .and_then(|policy| policy.get("max_input_bytes"))
        .and_then(toml::Value::as_integer)
        .and_then(|bytes| u64::try_from(bytes).ok())
}

/// Effective Step 4 bbox-annotation policy. Scope config overrides user config;
/// absence at both levels is the frozen secure/searchable default `true`.
pub(crate) fn bbox_annotation_enabled(repo: &Repository) -> Result<bool> {
    effective_bbox_annotation_policy(
        &repo.kio_dir().join("config.toml"),
        &user_config_toml_path(repo.control()),
    )
}

pub(crate) fn effective_bbox_annotation_policy(
    scope_config: &Path,
    user_config: &Path,
) -> Result<bool> {
    if let Some(enabled) = read_bbox_annotation_config(scope_config)? {
        return Ok(enabled);
    }
    Ok(read_bbox_annotation_config(user_config)?.unwrap_or(true))
}

pub(crate) const BBOX_ANNOTATION_CONFIG_MAX_BYTES: u64 = 1024 * 1024;

pub(crate) fn read_bbox_annotation_config(path: &Path) -> Result<Option<bool>> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(KioError::io(error.to_string(), path.display().to_string()));
        }
    }
    let bytes = read_bounded_regular_file(path, BBOX_ANNOTATION_CONFIG_MAX_BYTES)?;
    let text = std::str::from_utf8(&bytes).map_err(|error| {
        KioError::schema(format!(
            "bbox annotation config is not valid UTF-8 at {}: {error}",
            path.display()
        ))
    })?;
    let value: toml::Value = toml::from_str(text).map_err(|error| {
        KioError::schema(format!(
            "invalid bbox annotation config at {}: {error}",
            path.display()
        ))
    })?;
    // QA33 (step4b-contract-tests-p3a.md §J, arbitration #4): the spec's
    // literal TOML example is a flat `[markdownize] bbox_annotation = true`
    // key (07 §5.2), not a nested `[markdownize.bbox_annotation] enabled =
    // true` table — the schema now matches (config.schema.json).
    Ok(value
        .get("markdownize")
        .and_then(|markdownize| markdownize.get("bbox_annotation"))
        .and_then(toml::Value::as_bool))
}

/// Smallest share of its page's largest figure that a `related_images[]` entry
/// must occupy to be worth an Agent's `kio open`.
///
/// **Measured, not chosen.** Across the four `/layout-parsing` captures in
/// `kio-adapter/tests/fixtures/layout-parsing/`, decoration tops out at 10.7%
/// of the page's largest figure while every real figure sits at 83.6% or above
/// — a 7.8x gap this lands in the middle of. Four pages is a thin basis, which
/// is why the value is a knob and why nothing is discarded at index time: raise
/// it, lower it, or set it to 0 to keep every reference, and the next search
/// obeys without a re-index.
///
/// Relative to the page's own largest figure rather than to the page area, so
/// it survives a resolution change. Re-rendering the same infographic through
/// the PDF path resamples it to 96% and moves every absolute area with it.
const RELATED_IMAGES_MIN_AREA_RATIO_DEFAULT: f64 = 0.25;

/// Effective `[search] related_images_min_area_ratio`, under the same PC49/PC50
/// layering as every other `[search]` key ([`effective_search_config`]): a
/// folder value counts only for a single, non-`--descendants` `--scope`, and a
/// multi-scope search reads the user layer alone. One response cannot hold two
/// floors, and taking the CWD's would let a scope that merely happens to be
/// nearby decide how another scope's figures are presented.
pub(crate) fn related_images_min_area_ratio(
    single_scope_kio_dir: Option<&Path>,
    control: &ControlHandle,
) -> Result<f64> {
    if let Some(kio_dir) = single_scope_kio_dir
        && let Some(ratio) = read_related_images_min_area_ratio(&kio_dir.join("config.toml"))?
    {
        return Ok(ratio);
    }
    Ok(
        read_related_images_min_area_ratio(&user_config_toml_path(control))?
            .unwrap_or(RELATED_IMAGES_MIN_AREA_RATIO_DEFAULT),
    )
}

fn read_related_images_min_area_ratio(path: &Path) -> Result<Option<f64>> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(KioError::io(error.to_string(), path.display().to_string()));
        }
    }
    let bytes = read_bounded_regular_file(path, BBOX_ANNOTATION_CONFIG_MAX_BYTES)?;
    let text = std::str::from_utf8(&bytes).map_err(|error| {
        KioError::schema(format!(
            "search config is not valid UTF-8 at {}: {error}",
            path.display()
        ))
    })?;
    let value: toml::Value = toml::from_str(text).map_err(|error| {
        KioError::schema(format!(
            "invalid search config at {}: {error}",
            path.display()
        ))
    })?;
    let Some(ratio) = value
        .get("search")
        .and_then(|search| search.get("related_images_min_area_ratio"))
    else {
        return Ok(None);
    };
    // An integer `0` or `1` is what a person writes for "keep everything" or
    // "keep only the biggest", so accept both spellings rather than making the
    // decimal point load-bearing.
    let ratio = ratio
        .as_float()
        .or_else(|| ratio.as_integer().map(|value| value as f64))
        .filter(|value| (0.0..=1.0).contains(value))
        .ok_or_else(|| {
            KioError::schema(format!(
                "related_images_min_area_ratio must be a number in 0.0..=1.0 at {}",
                path.display()
            ))
        })?;
    Ok(Some(ratio))
}

/// R12-1: effective `[markdownize.incremental]` (docs/10:537, docs/03:595). `enabled`
/// / `threshold` / `max_consecutive` were documented `.kio/config.toml` overrides but
/// hardcoded (0.30 / 5) in the mode decision. `include_neighbors` is enforced
/// separately (no implementation concept → non-default rejected in
/// `enforce_config_semantics`).
pub(crate) struct IncrementalConfig {
    pub(crate) enabled: bool,
    pub(crate) threshold: f64,
    pub(crate) max_consecutive: u32,
}

pub(crate) fn effective_incremental_config(repo: &Repository) -> Result<IncrementalConfig> {
    let scope = read_incremental_tuning(&repo.kio_dir().join("config.toml"))?;
    let user = read_incremental_tuning(&user_config_toml_path(repo.control()))?;
    Ok(IncrementalConfig {
        enabled: scope.0.or(user.0).unwrap_or(true),
        threshold: scope.1.or(user.1).unwrap_or(0.30),
        max_consecutive: scope.2.or(user.2).unwrap_or(5),
    })
}

pub(crate) fn read_incremental_tuning(
    path: &Path,
) -> Result<(Option<bool>, Option<f64>, Option<u32>)> {
    let Ok(text) = fs::read_to_string(path) else {
        return Ok((None, None, None));
    };
    let value: toml::Value =
        toml::from_str(&text).map_err(|err| KioError::schema(err.to_string()))?;
    let incremental = value
        .get("markdownize")
        .and_then(|markdownize| markdownize.get("incremental"));
    let enabled = incremental
        .and_then(|incremental| incremental.get("enabled"))
        .and_then(toml::Value::as_bool);
    let threshold = toml_f64(incremental.and_then(|incremental| incremental.get("threshold")));
    let max_consecutive =
        toml_u64(incremental.and_then(|incremental| incremental.get("max_consecutive")))
            .and_then(|value| u32::try_from(value).ok());
    Ok((enabled, threshold, max_consecutive))
}

/// R13-3: the observability logs (metrics device-global, access scope-local) go
/// through the shared rotating writer so they get the documented daily rotation +
/// retention prune (docs/06 §12 / docs/10 §11.6), same as events/errors. Rotation
/// is best-effort; only the final append can fail (R12-5).
pub(crate) fn append_jsonl_cli(
    session: &kio_core::scope::LockSession,
    path: &Path,
    value: &Value,
    retention_days: u32,
    control: &ControlHandle,
) -> Result<()> {
    control.append_jsonl_rotating_in_session(session, path, value, retention_days)
}

/// `[observability] retention_days` for the device-global logs (metrics), read from
/// the user config, default 30.
pub(crate) fn device_logs_retention_days(control: &ControlHandle) -> u32 {
    kio_core::scope::read_logs_retention_days(&user_config_toml_path(control))
        .unwrap_or(kio_core::scope::DEFAULT_LOG_RETENTION_DAYS)
}

/// `[observability] retention_days` for the scope-local `access.jsonl` — the scope
/// config wins over the user config (the log lives in `.kio/`), then the 30-day
/// default. Mirrors `access_log_redact`'s precedence.
pub(crate) fn scope_logs_retention_days(repo: &Repository) -> u32 {
    kio_core::scope::read_logs_retention_days(&repo.kio_dir().join("config.toml"))
        .or_else(|| {
            kio_core::scope::read_logs_retention_days(&user_config_toml_path(repo.control()))
        })
        .unwrap_or(kio_core::scope::DEFAULT_LOG_RETENTION_DAYS)
}

/// Loads every adapter runtime input from the current device configuration.
/// The returned value is installed only around one dispatch, so a changed or
/// removed configuration cannot leak authority into a later invocation.
pub(crate) fn adapter_runtime_settings_from_user_config(
    control: &ControlHandle,
) -> Result<kio_adapter::tool_lock::AdapterRuntimeSettings> {
    use kio_adapter::tool_lock::{
        AdapterRuntimeSettings, declared_adapter_for_role, declared_pricing_for_role,
    };
    let mut settings = AdapterRuntimeSettings::with_control(control.clone());
    if let Some(path) = user_tools_toml_path(control)
        && path.is_file()
    {
        let bytes = read_bounded_regular_file(&path, MAX_TOOLS_TOML_BYTES)?;
        // A literal key is never carried from this ordinary configuration
        // read into runtime settings. Re-read the same file through core's
        // private no-follow boundary, then derive both declaration and key
        // source from those verified bytes.
        let bytes = if tools_toml_uses_plain_auth(&bytes).map_err(adapter_to_kio)? {
            read_private_tools_toml_with_control(&path, control).map_err(adapter_to_kio)?
        } else {
            bytes
        };
        let value = parse_tools_toml(&bytes).map_err(adapter_to_kio)?;
        for role in ["prepare", "markdown", "embedding"] {
            if let Some(declared) = declared_adapter_for_role(&value, role) {
                settings.declarations.insert(role.to_owned(), declared);
            }
            let pricing = declared_pricing_for_role(&value, role);
            if !pricing.is_empty() {
                settings.pricing.insert(role.to_owned(), pricing);
            }
        }
        settings.tools_toml_path = Some(path);
    }
    let path = user_config_toml_path(control);
    let bytes = match read_bounded_regular_file(&path, 1024 * 1024) {
        Ok(bytes) => Some(bytes),
        Err(_error)
            if !path
                .try_exists()
                .map_err(|e| KioError::io(e.to_string(), path.display().to_string()))? =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    let Some(bytes) = bytes else {
        return Ok(settings);
    };
    let text = std::str::from_utf8(&bytes).map_err(|error| KioError::schema(error.to_string()))?;
    let value: toml::Value =
        toml::from_str(text).map_err(|error| KioError::schema(error.to_string()))?;
    if let Some(seconds) = value
        .get("adapter")
        .and_then(|adapter| adapter.get("policy"))
        .and_then(|policy| policy.get("offline_api"))
        .and_then(|mode| mode.get("timeout_seconds"))
        .and_then(toml::Value::as_integer)
        .and_then(|seconds| u64::try_from(seconds).ok())
    {
        settings
            .execution_timeouts
            .insert("offline_api".to_owned(), seconds);
    }
    if value
        .get("adapter")
        .and_then(|adapter| adapter.get("policy"))
        .and_then(|policy| policy.get("offline_api"))
        .and_then(|mode| mode.get("ca_pem_path"))
        .is_some()
    {
        return Err(KioError::invalid_usage(
            "adapter.policy.offline_api.ca_pem_path is retired; import it with `kio adapter trust register --ca-pem ABSOLUTE_PEM --yes`",
        ));
    }
    Ok(settings)
}

pub(crate) fn validate_user_tools_config(control: &ControlHandle) -> Result<()> {
    let Some(path) = user_tools_toml_path(control) else {
        return Ok(());
    };
    if !path.is_file() {
        return Ok(());
    }
    // This bounded no-follow read only classifies a configuration before it
    // is used. A literal credential is never derived from it: that branch
    // re-reads through `read_private_tools_toml` below. An unsafe file that
    // cannot reach this classifier retains the generic bounded-read error.
    let bytes = read_bounded_regular_file(&path, MAX_TOOLS_TOML_BYTES)?;
    if tools_toml_uses_plain_auth(&bytes).map_err(adapter_to_kio)? {
        // This is a refusal, not an advisory log: native Windows DACL and
        // macOS ACL checks belong to the same shared private-file boundary as
        // the later credential use. Its error is intentionally stable and
        // never contains the credential or file bytes.
        read_private_tools_toml_with_control(&path, control).map_err(adapter_to_kio)?;
    }
    validate_tools_toml(&bytes).map_err(adapter_to_kio)
}

/// M8: validate the user (device) `config.toml` against `config.schema.json`
/// before dispatch (10 §11 / 06 §10). The folder `.kio/config.toml` is already
/// validated on `Repository::open` (scope.rs `validate_config`); the user config
/// took no such path, so a negative budget cap etc. slipped through. Schema
/// failures are `KIO-E-CONFIG-SCHEMA-001` (exit 2).
pub(crate) fn validate_user_config(control: &ControlHandle) -> Result<()> {
    let path = user_config_toml_path(control);
    if !path.is_file() {
        return Ok(());
    }
    let text = fs::read_to_string(&path)
        .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
    let toml_value: toml::Value =
        toml::from_str(&text).map_err(|err| KioError::schema(err.to_string()))?;
    let json_value =
        serde_json::to_value(&toml_value).map_err(|err| KioError::schema(err.to_string()))?;
    validate_json_schema(SchemaKind::Config, &json_value)?;
    // R12-2: apply the same documented-but-unwired value enforcement to the user
    // config (device-global) as scope config gets in `validate_config`, so e.g.
    // `store_request_body = true` there is a loud NOT-IMPLEMENTED, not a silent
    // accept.
    kio_core::scope::enforce_config_semantics(&json_value)
}

pub(crate) fn validate_repo_tool_lock(repo: &Repository) -> Result<()> {
    let path = repo.kio_dir().join("tool-lock.json");
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => return Err(KioError::schema("tool-lock must be a regular file")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(KioError::io(error.to_string(), path.display().to_string())),
    }
    let bytes =
        fs::read(&path).map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
    load_tool_lock(&bytes).map(|_| ()).map_err(adapter_to_kio)
}

/// The persisted current embedding profile that selects the one vector space a
/// `repair rebuild-db` may replay.  This intentionally does not consult the
/// active adapter: recovery must work while its environment is unavailable.
/// A missing lock or a lock without `embedding` means text-only replay.
pub(crate) fn rebuild_embedding_entry_from_tool_lock(
    kio_dir: &Path,
) -> Result<Option<kio_adapter::tool_lock::EmbeddingToolLockEntry>> {
    let path = kio_dir.join("tool-lock.json");
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => return Err(KioError::schema("tool-lock must be a regular file")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(KioError::io(error.to_string(), path.display().to_string())),
    }
    let bytes = fs::read(&path)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
    let lock = load_tool_lock(&bytes).map_err(adapter_to_kio)?;
    Ok(lock.embedding)
}

pub(crate) fn embedding_profile_summary(
    entry: &kio_adapter::tool_lock::EmbeddingToolLockEntry,
) -> embedding_store::EmbeddingProfileSummary {
    embedding_store::EmbeddingProfileSummary {
        dimensions: u64::from(entry.dimensions),
        distance: entry.distance.clone(),
        modality: entry.modality.clone(),
        profile_hash: entry.profile_hash.clone(),
    }
}

/// The optional provenance label is not itself authority: only the exact
/// built-in, in-process identity can avoid a recipient secret-send grant.
pub(crate) fn persisted_embedding_is_deterministic(
    entry: &kio_adapter::tool_lock::EmbeddingToolLockEntry,
) -> bool {
    let builtin = declared_embedding_profile(EmbeddingExecution::DeterministicEvaluator);
    entry.kind == Some(ExecutionMode::DeterministicLibrary)
        && entry.tool_id == builtin.tool_id
        && entry.profile_hash == builtin.profile_hash
        && u64::from(entry.dimensions) == builtin.dimensions
        && entry.distance == builtin.distance
        && entry.modality == builtin.modality
}

pub(crate) fn rebuild_embedding_profile_from_tool_lock(
    kio_dir: &Path,
) -> Result<Option<embedding_store::EmbeddingProfileSummary>> {
    Ok(rebuild_embedding_entry_from_tool_lock(kio_dir)?
        .as_ref()
        .map(embedding_profile_summary))
}

pub(crate) fn require_repo_tool_lock(repo: &Repository) -> Result<()> {
    let path = repo.kio_dir().join("tool-lock.json");
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => validate_repo_tool_lock(repo),
        Ok(_) => Err(KioError::schema("tool-lock must be a regular file")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(KioError::schema(
            "tool-lock is required before executing persisted batch tasks; run `kio index` to regenerate it",
        )),
        Err(error) => Err(KioError::io(error.to_string(), path.display().to_string())),
    }
}

fn user_tools_toml_path(control: &ControlHandle) -> Option<PathBuf> {
    // R12-6 / R13-6: empty/relative `XDG_CONFIG_HOME` or `HOME` are invalid.
    if let Some(path) = kio_core::xdg::xdg_dir("XDG_CONFIG_HOME") {
        return Some(path.join("kio/tools.toml"));
    }
    kio_core::xdg::home_dir_with_control(control).map(|home| home.join(".config/kio/tools.toml"))
}

pub(crate) fn user_config_toml_path(control: &ControlHandle) -> PathBuf {
    // R12-6 / R13-6: empty/relative `XDG_CONFIG_HOME` or `HOME` are invalid. The
    // `ensure_device_dirs_resolvable` startup guard rejects the no-absolute-base
    // case loudly, so the relative last resort is never reached in practice.
    if let Some(path) = kio_core::xdg::xdg_dir("XDG_CONFIG_HOME") {
        return path.join("kio/config.toml");
    }
    kio_core::xdg::home_dir_with_control(control)
        .map(|home| home.join(".config/kio/config.toml"))
        .unwrap_or_else(|| PathBuf::from(".config/kio/config.toml"))
}

pub(crate) fn data_home(control: &ControlHandle) -> PathBuf {
    // R12-6 / R13-6: empty/relative `XDG_DATA_HOME` or `HOME` are invalid — the
    // device data dir (registry, cost ledger, logs, 0600 cursor-key) must never
    // resolve to a CWD-relative `kio/` that the next index could archive. The
    // startup guard rejects the no-absolute-base case before the `"."` last resort.
    if let Some(path) = kio_core::xdg::xdg_dir("XDG_DATA_HOME") {
        return path;
    }
    if let Some(home) = kio_core::xdg::home_dir_with_control(control) {
        return home.join(".local/share");
    }
    PathBuf::from(".")
}

/// `$XDG_CACHE_HOME`, else `$HOME/.cache` (06 §1.1). The device cache root for
/// disposable, regenerable data such as the open/view read-only expansion of CAS
/// objects — deliberately separate from the durable `$XDG_DATA_HOME` (P9).
pub(crate) fn cache_home(control: &ControlHandle) -> PathBuf {
    // R12-6 / R13-6: empty/relative `XDG_CACHE_HOME` or `HOME` are invalid.
    if let Some(path) = kio_core::xdg::xdg_dir("XDG_CACHE_HOME") {
        return path;
    }
    if let Some(home) = kio_core::xdg::home_dir_with_control(control) {
        return home.join(".cache");
    }
    PathBuf::from(".")
}

/// R13-6: fail loudly at startup when neither `XDG_*` nor an absolute `HOME` can
/// anchor the device-global directories, instead of silently scattering
/// device-global state (registry, cost ledger, logs, the 0600 cursor-key) and the
/// device budget cap into a CWD-relative `kio/`. R12-6 fixed the `XDG_*` side; the
/// `HOME` fallback still degraded to `PathBuf::from(".")`. Checked once per command
/// (every command touches at least logs/registry), so `env -u HOME` with no XDG
/// override errors rather than writing under the working directory.
pub(crate) fn ensure_device_dirs_resolvable(control: &ControlHandle) -> Result<()> {
    if kio_core::xdg::home_dir_with_control(control).is_some() {
        return Ok(());
    }
    for var in ["XDG_DATA_HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME"] {
        if kio_core::xdg::xdg_dir(var).is_none() {
            return Err(KioError::invalid_usage(format!(
                "cannot resolve an absolute base directory for device-global state: \
                 set $HOME to an absolute path or export an absolute ${var} \
                 (Kio refuses to write device-global state under the working directory)"
            )));
        }
    }
    Ok(())
}

/// Device-local HMAC key that signs search cursors (O1(b)). Stored at
/// `$XDG_DATA_HOME/kio/cursor-key` (0600), generated from the operating system's
/// cryptographically secure random source on first use. Signing binds a cursor
/// to this device so a caller cannot forge or tamper a token to jump scope or
/// page — `query_hash` alone covers only public inputs.
pub(crate) fn cursor_signing_key(control: &ControlHandle) -> Result<Vec<u8>> {
    cursor_key::load_or_create(&data_home(control).join("kio"), control)
}

/// Invoked lazily only after a real batch credential is resolved. An absent
/// key cannot be replaced while any bound in-flight or residual-cleanup row
/// still relies on it. Legacy constant scopes never match the new clients.
pub(crate) fn batch_recovery_context(
    ledger: &LedgerDb,
) -> kio_adapter::Result<kio_adapter::batch_recovery::BatchRecoveryContext> {
    let rows = kio_pipeline::ledger::ops::recovery_candidates(ledger).map_err(|_| {
        kio_adapter::AdapterError::ConfigSchema(
            "cannot inspect pending batch recovery authority".into(),
        )
    })?;
    let pending = rows.iter().any(|row| {
        row.provider_scope_id
            .as_deref()
            .is_some_and(kio_adapter::batch_recovery::is_bound_recovery_scope)
    });
    batch_recovery_key::load_or_create(
        &data_home(ledger.control()).join("kio"),
        pending,
        ledger.control(),
    )
    .map_err(|error| kio_adapter::AdapterError::ConfigSchema(error.to_string()))
}
