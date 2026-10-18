use super::*;

/// `07 §3`'s single execution mode every currently-gate-relevant online
/// adapter (the built-in `mistral_ocr_markdownize` / `gemini_embedding_2`
/// targets, `07 §1`) declares. Kept as a named constant (rather than
/// threading `kio_adapter::types::ExecutionMode` into this crate) because
/// `approvals[]` rows store it as the plain string `AdapterProfile` already
/// serializes it to.
pub const NETWORK_APPROVAL_EXECUTION_MODE: &str = "online_api";

pub(super) fn read_scope_json_value(kio_dir: &Path) -> Result<Value> {
    let path = kio_dir.join("scope.json");
    let value = serde_json::from_str(&fs::read_to_string(&path).kio_io(&path)?)
        .map_err(|err| KioError::schema(err.to_string()))?;
    validate_scope_json_value(&value)?;
    Ok(value)
}

/// Validate a persisted scope before any public reader exposes its approval
/// state. Exact-version rejection intentionally precedes current-schema
/// validation so every non-current version has the stable store-version error
/// rather than being interpreted through unknown keys.
pub(super) fn validate_scope_json_value(value: &Value) -> Result<()> {
    let version = match value.get("kio_format_version") {
        Some(Value::String(version)) => version.as_str(),
        Some(_) => return Err(KioError::incompatible_format("<non-string>")),
        None => return Err(KioError::incompatible_format("<missing>")),
    };
    validate_format_version(version)?;
    validate_json_schema(SchemaKind::Scope, value)
}

pub(super) fn validate_stable_store_gate(directory: &StoreDirectory) -> Result<()> {
    crate::platform::validate_store_gate(directory)
}

pub(super) fn validate_store_lock_authority(directory: &StoreDirectory) -> Result<()> {
    // Format must reject old binaries/stores before checking a new required
    // leaf, and before creating or reclaiming any transient lock.
    read_bound_scope_json_value(directory)?;
    validate_stable_store_gate(directory)
}

pub(crate) fn validate_bound_store_lock_authority(
    kio: &File,
    control: &crate::control::ControlHandle,
) -> Result<()> {
    let directory = StoreDirectory::from_retained_with_control(
        kio.try_clone()
            .map_err(|e| KioError::io(e.to_string(), ".kio"))?,
        PathBuf::from(".kio"),
        control.clone(),
    )?;
    validate_store_lock_authority(&directory)
}

/// Bind an approval mutation to a retained `.kio` directory and serialize its
/// complete read/compare/write sequence with every other store writer.  The
/// public path is used only to acquire the initial no-follow directory
/// capability; the closure never resolves it again.
///
/// Standalone approval writers own fresh lock authority. Nested repository
/// approval mutations use the repository's explicit operation session.
fn with_bound_scope_approval_write<T>(
    kio_dir: &Path,
    operation: impl FnOnce(&StoreDirectory) -> Result<T>,
) -> Result<T> {
    let directory = StoreDirectory::open(kio_dir)?;
    let retained = directory
        .root_handle()
        .as_ref()
        .try_clone()
        .map_err(|error| KioError::io(error.to_string(), ".kio"))?;
    let _lock = acquire_retained_store_lock(&retained)?;
    operation(&directory)
}

pub(super) fn read_bound_scope_json_value(directory: &StoreDirectory) -> Result<Value> {
    let bytes = directory
        .read_optional(Path::new("scope.json"), MAX_BOUND_CONFIG_BYTES)?
        .ok_or_else(|| KioError::schema("scope.json is missing"))?;
    let value = serde_json::from_slice(&bytes).map_err(|err| KioError::schema(err.to_string()))?;
    validate_scope_json_value(&value)?;
    Ok(value)
}

fn overwrite_bound_scope_json_value(directory: &StoreDirectory, value: &Value) -> Result<()> {
    let bytes =
        serde_json::to_string_pretty(value).map_err(|err| KioError::schema(err.to_string()))?;
    directory.write_atomic(
        Path::new("scope.json"),
        bytes.as_bytes(),
        crate::store_dir::Publication::Replace,
    )
}

pub(super) fn read_bound_config_document(directory: &StoreDirectory) -> Result<String> {
    let bytes = directory
        .read_optional(Path::new("config.toml"), MAX_BOUND_CONFIG_BYTES)?
        .ok_or_else(|| KioError::schema("config.toml is missing"))?;
    let text =
        String::from_utf8(bytes).map_err(|_| KioError::schema("config.toml must be UTF-8"))?;
    validate_scope_config_document(&text)?;
    Ok(text)
}

pub(super) fn validate_scope_config_document(text: &str) -> Result<()> {
    if text.len() as u64 > MAX_BOUND_CONFIG_BYTES {
        return Err(KioError::schema("config.toml exceeds the byte limit"));
    }
    let toml: toml::Value =
        toml::from_str(text).map_err(|error| KioError::schema(error.to_string()))?;
    let value = serde_json::to_value(&toml).map_err(|error| KioError::schema(error.to_string()))?;
    validate_json_schema(SchemaKind::Config, &value)?;
    enforce_scope_config_semantics(&value)
}

pub(super) fn network_approvals_from_value(value: &Value) -> Result<Vec<Value>> {
    Ok(value
        .get("approvals")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

pub(super) fn network_approvals_initialized_from_value(value: &Value) -> Result<bool> {
    Ok(value
        .get("approvals_initialized")
        .and_then(Value::as_bool)
        .unwrap_or(false))
}

pub(super) fn network_approval_pending_from_value(value: &Value) -> Result<Option<Value>> {
    Ok(value.get("approval_pending").cloned())
}

pub(super) fn network_approval_active_from_value(
    value: &Value,
    tool_id: &str,
    execution_mode: &str,
    tool_profile_hash: &str,
) -> Result<bool> {
    let Some(scope_id) = value.get("scope_id").and_then(Value::as_str) else {
        return Ok(false);
    };
    let Some(approvals) = value.get("approvals").and_then(Value::as_array) else {
        return Ok(false);
    };
    Ok(approvals.iter().any(|row| {
        row.get("scope_id").and_then(Value::as_str) == Some(scope_id)
            && row.get("tool_id").and_then(Value::as_str) == Some(tool_id)
            && row.get("execution_mode").and_then(Value::as_str) == Some(execution_mode)
            && row.get("tool_profile_hash").and_then(Value::as_str) == Some(tool_profile_hash)
            && row.get("status").and_then(Value::as_str) == Some("active")
    }))
}

pub(super) fn network_approval_row_present_from_value(
    value: &Value,
    tool_id: &str,
) -> Result<bool> {
    let Some(scope_id) = value.get("scope_id").and_then(Value::as_str) else {
        return Ok(false);
    };
    let Some(approvals) = value.get("approvals").and_then(Value::as_array) else {
        return Ok(false);
    };
    Ok(approvals.iter().any(|row| {
        row.get("scope_id").and_then(Value::as_str) == Some(scope_id)
            && row.get("tool_id").and_then(Value::as_str) == Some(tool_id)
            && row.get("status").and_then(Value::as_str) == Some("active")
    }))
}

/// Current `.kio/scope.json` `approvals[]` rows. Empty when the key is
/// absent — no adapter has ever been approved for this scope (10 §11.3).
pub fn read_network_approvals(kio_dir: &Path) -> Result<Vec<Value>> {
    network_approvals_from_value(&read_scope_json_value(kio_dir)?)
}

/// Whether this scope has recorded an explicit approval lifecycle. The marker
/// is audit state only; it never authorizes sending or automatic recovery.
pub fn network_approvals_initialized(kio_dir: &Path) -> Result<bool> {
    network_approvals_initialized_from_value(&read_scope_json_value(kio_dir)?)
}

/// Whether an `active` `approvals[]` row exists for `tool_id` whose
/// `scope_id`/`execution_mode`/`tool_profile_hash` match the CURRENT values
/// exactly (07 §3's send-gate AND condition: "現在の execution_mode/
/// tool_profile_hash に一致する status=active 行が存在する" — a profile
/// change invalidates the row until re-approval, QA23).
pub fn network_approval_active(
    kio_dir: &Path,
    tool_id: &str,
    execution_mode: &str,
    tool_profile_hash: &str,
) -> Result<bool> {
    network_approval_active_from_value(
        &read_scope_json_value(kio_dir)?,
        tool_id,
        execution_mode,
        tool_profile_hash,
    )
}

/// Whether ANY `active` `approvals[]` row currently exists for `tool_id` in
/// this scope, regardless of `execution_mode`/`tool_profile_hash` — the
/// coarser presence check `--online`'s one-shot branch uses to "trust an
/// existing row for one more send" even across a profile change (07 §3),
/// distinct from [`network_approval_active`]'s strict steady-state match.
pub fn network_approval_row_present(kio_dir: &Path, tool_id: &str) -> Result<bool> {
    network_approval_row_present_from_value(&read_scope_json_value(kio_dir)?, tool_id)
}

/// The single in-flight `approval_pending` object, or `None` when absent (07
/// §3 L191-205: the write-order's step (0) pending intent — a
/// single object, never an array, since approval operations are serialized
/// under `.kio/.lock`). Persisted scopes are fully version- and schema-validated
/// before this value is returned.
pub fn read_network_approval_pending(kio_dir: &Path) -> Result<Option<Value>> {
    network_approval_pending_from_value(&read_scope_json_value(kio_dir)?)
}

/// 07 §3 step (0) of the approval write-order: durably record the pending
/// approval intent BEFORE `config.toml` is touched, so a crash between
/// steps leaves a recoverable trail and a concurrent `kio adapter revoke`
/// has something to detect/remove (QA26/27).
pub fn write_network_approval_pending(kio_dir: &Path, pending: Value) -> Result<()> {
    with_bound_scope_approval_write(kio_dir, |directory| {
        write_network_approval_pending_in_directory(directory, pending)
    })
}

pub(super) fn write_network_approval_pending_in_directory(
    directory: &StoreDirectory,
    pending: Value,
) -> Result<()> {
    let mut value = read_bound_scope_json_value(directory)?;
    let Some(object) = value.as_object_mut() else {
        return Err(KioError::schema("scope.json must be an object"));
    };
    object.insert("approval_pending".to_owned(), pending);
    validate_json_schema(SchemaKind::Scope, &value)?;
    overwrite_bound_scope_json_value(directory, &value)
}

/// 07 §3 step (2) (QA21/23/26/27): publish the `approvals[]` row — upserted
/// by `(scope_id, tool_id)` (existing rows are updated in place, never
/// deleted, matching "行は削除しない — 監査保全"), set the
/// `approvals_initialized` marker, and remove `approval_pending` in the SAME
/// atomic write.
///
/// `expected_pending` is the exact pending payload the caller durably wrote
/// in step (0) via [`write_network_approval_pending`] (or `None` for an
/// explicit direct publication with no pending intent). This function serializes its complete
/// re-read/compare/write sequence under the retained `.kio/.lock` and requires
/// the CURRENTLY-persisted `approval_pending` to equal `expected_pending`
/// exactly before publishing. The durable replacement is atomic publication,
/// not a filesystem compare-and-swap; the retained lock supplies the compare
/// exclusion. A mismatch means a concurrent `kio adapter revoke` already
/// removed/changed it, and the publish must not resurrect a stale intent. Returns
/// `KIO-E-ADAPTER-APPROVAL-CONFLICT-001` (exit 5, QA26) on mismatch without
/// writing anything.
pub fn publish_network_approval(
    kio_dir: &Path,
    row: Value,
    expected_pending: Option<&Value>,
) -> Result<()> {
    with_bound_scope_approval_write(kio_dir, |directory| {
        publish_network_approval_in_directory(directory, row, expected_pending)
    })
}

pub(super) fn publish_network_approval_in_directory(
    directory: &StoreDirectory,
    row: Value,
    expected_pending: Option<&Value>,
) -> Result<()> {
    let mut value = read_bound_scope_json_value(directory)?;
    let Some(object) = value.as_object_mut() else {
        return Err(KioError::schema("scope.json must be an object"));
    };
    let current_pending = object.get("approval_pending").cloned();
    if current_pending.as_ref() != expected_pending {
        return Err(KioError::new(
            "KIO-E-ADAPTER-APPROVAL-CONFLICT-001",
            "approval_pending changed or was removed by a concurrent kio adapter revoke; re-approval is required",
            json!({}),
            ExitCode::AuthError,
        ));
    }
    let row_scope_id = row
        .get("scope_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let row_tool_id = row
        .get("tool_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut approvals = object
        .get("approvals")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut replaced = false;
    for existing in &mut approvals {
        let existing_scope_id = existing
            .get("scope_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let existing_tool_id = existing
            .get("tool_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if existing_scope_id == row_scope_id && existing_tool_id == row_tool_id {
            *existing = row.clone();
            replaced = true;
            break;
        }
    }
    if !replaced {
        approvals.push(row);
    }
    object.insert("approvals".to_owned(), Value::Array(approvals));
    object.insert("approvals_initialized".to_owned(), Value::Bool(true));
    object.remove("approval_pending");
    validate_json_schema(SchemaKind::Scope, &value)?;
    overwrite_bound_scope_json_value(directory, &value)
}

/// QA25/26/27 (step4b-contract-tests-p3a.md §H, 07 §3 L118-211): `kio adapter
/// revoke (<tool_id> | --all)`'s scope.json mutation. `tool_id = None` means
/// `--all`.
///
/// - Revoke updates the matching `active` `approvals[]` row(s) to
///   `status="revoked"` + `revoked_at` (rows are never deleted).
/// - Pending removal is `(scope_id, tool_id)`-only — `execution_mode`/
///   `tool_profile_hash` are UNQUESTIONED (07 §3 L123-130, QA27): a 4-tuple
///   -exact match would miss a pending left behind by a since-changed
///   profile, letting a later self-heal resurrect the very approval this
///   revoke meant to stop. `--all` (`tool_id = None`) removes any pending
///   regardless of which tool it names.
/// - Idempotent: when nothing actually changes (no matching active row, no
///   matching pending), returns a `changed() == false` outcome and does
///   **not** write the `approvals_initialized` marker (07 §3: "対象なしの
///   冪等成功では書かない" — an unused scope's initial-materialize
///   exception must not be consumed by a no-op revoke).
/// - When something DID change, the marker is set in the SAME atomic write
///   if it was not already (07 §3's initial-materialize-exception
///   consumption, shared with the approval-publish path).
pub fn revoke_network_approval(
    kio_dir: &Path,
    tool_id: Option<&str>,
    revoked_at: &str,
) -> Result<NetworkRevokeOutcome> {
    with_bound_scope_approval_write(kio_dir, |directory| {
        revoke_network_approval_in_directory(directory, tool_id, revoked_at)
    })
}

pub(super) fn revoke_network_approval_in_directory(
    directory: &StoreDirectory,
    tool_id: Option<&str>,
    revoked_at: &str,
) -> Result<NetworkRevokeOutcome> {
    let mut value = read_bound_scope_json_value(directory)?;
    let Some(scope_id) = value
        .get("scope_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return Err(KioError::schema("scope.json missing scope_id"));
    };
    let Some(object) = value.as_object_mut() else {
        return Err(KioError::schema("scope.json must be an object"));
    };
    let mut outcome = NetworkRevokeOutcome::default();
    if let Some(Value::Array(approvals)) = object.get_mut("approvals") {
        for row in approvals.iter_mut() {
            let row_scope_id = row.get("scope_id").and_then(Value::as_str);
            let row_tool_id = row
                .get("tool_id")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let is_active = row.get("status").and_then(Value::as_str) == Some("active");
            let matches_target = match tool_id {
                Some(target) => row_tool_id.as_deref() == Some(target),
                None => true,
            };
            if row_scope_id == Some(scope_id.as_str()) && is_active && matches_target {
                if let Some(row_object) = row.as_object_mut() {
                    row_object.insert("status".to_owned(), json!("revoked"));
                    row_object.insert("revoked_at".to_owned(), json!(revoked_at));
                }
                if let Some(row_tool_id) = row_tool_id {
                    outcome.revoked_tool_ids.push(row_tool_id);
                }
            }
        }
    }
    if let Some(pending) = object.get("approval_pending").cloned() {
        let pending_scope_id = pending.get("scope_id").and_then(Value::as_str);
        let pending_tool_id = pending.get("tool_id").and_then(Value::as_str);
        let matches_target = match tool_id {
            Some(target) => pending_tool_id == Some(target),
            None => true,
        };
        if pending_scope_id == Some(scope_id.as_str()) && matches_target {
            object.remove("approval_pending");
            outcome.pending_removed = true;
        }
    }
    if !outcome.changed() {
        return Ok(outcome);
    }
    let already_initialized = object
        .get("approvals_initialized")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !already_initialized {
        object.insert("approvals_initialized".to_owned(), json!(true));
        outcome.marker_written = true;
    }
    validate_json_schema(SchemaKind::Scope, &value)?;
    overwrite_bound_scope_json_value(directory, &value)?;
    Ok(outcome)
}
