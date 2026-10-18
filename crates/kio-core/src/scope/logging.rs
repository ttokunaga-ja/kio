use super::*;

/// Default log retention when `[observability] retention_days` is unset
/// (docs/06 §12 / docs/10 §11.6: "保持 30 日 (config 上書き可)").
pub const DEFAULT_LOG_RETENTION_DAYS: u32 = 30;

/// Read `[observability] retention_days` (integer 1..=3650) from a
/// `config.toml`.
/// `None` when the file/key is absent or malformed, so the caller applies the
/// 30-day default. The key is schema-validated (`config.schema.json`) at startup,
/// so a bad value would already have been rejected; this read is defensive.
#[must_use]
pub fn read_logs_retention_days(config_toml_path: &Path) -> Option<u32> {
    let text = fs::read_to_string(config_toml_path).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    value
        .get("observability")
        .and_then(|section| section.get("retention_days"))
        .and_then(toml::Value::as_integer)
        .and_then(|days| u32::try_from(days).ok())
        .filter(|days| (1..=3650).contains(days))
}

/// `[adapter] lane` — the online send lane this config prefers, as a raw
/// string (`"batch"` | `"realtime"`; the schema constrains the enum, so this
/// read is defensive). `None` when the key is absent or unreadable, which
/// leaves the caller on its own default.
///
/// One key governs BOTH online adapters by ruling (07 §5.3, 2026-07-24): a
/// run never splits markdownize onto one lane and embedding onto the other,
/// so there is deliberately no per-adapter form of this key.
#[must_use]
pub fn read_adapter_lane(config_toml_path: &Path) -> Option<String> {
    let text = fs::read_to_string(config_toml_path).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    value
        .get("adapter")
        .and_then(|adapter| adapter.get("lane"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
        .filter(|lane| lane == "batch" || lane == "realtime")
}

/// R13-3: append `value` to the fixed-name JSONL at `path`, first performing a
/// best-effort daily rotation + retention prune (docs/06 §12 / docs/10 §11.6:
/// "日次ローテーション、保持 30 日 (config 上書き可)"). logrotate style: when the
/// live file's last-written day differs from today it is renamed to
/// `<stem>-YYYY-MM-DD.jsonl` and a fresh fixed-name file starts, so the documented
/// fixed names (events.jsonl / errors.jsonl / metrics.jsonl / access.jsonl) stay
/// current. Dated files older than `retention_days` are pruned. Rotation/prune
/// failures are non-fatal (R12-5 / R13-3 ruling): only the final append can fail
/// the caller, so a read-only log dir never kills the command it belongs to. A
/// concurrent process's `O_APPEND` handle on the renamed inode loses no lines —
/// rename is atomic and its bytes land in the dated file.
pub fn append_jsonl_rotating(path: &Path, value: &Value, retention_days: u32) -> Result<()> {
    append_jsonl_rotating_with_control(
        &LockSession::default(),
        &crate::control::ControlHandle::default(),
        path,
        value,
        retention_days,
    )
}

pub(crate) fn append_jsonl_rotating_with_control(
    session: &LockSession,
    control: &crate::control::ControlHandle,
    path: &Path,
    value: &Value,
    retention_days: u32,
) -> Result<()> {
    // CT4-PURGE-008: every current-log append is serialized against purge's
    // current+rotated scrub. A contended nonblocking lock makes this best-effort
    // append fail without landing a post-scrub row; existing observability callers
    // already downgrade log failures so user operations remain unaffected.
    let _scrub_lock = scrub_lock_path(path)
        .map(|path| StoreLock::acquire_path_in_session(session, path, control))
        .transpose()?;
    let today = control
        .now_utc_seconds()
        .get(..10)
        .unwrap_or_default()
        .to_owned();
    // R22-8: `rotate_stale_log` is a check-then-rename (`!dated.exists()` then `fs::rename`),
    // which two processes crossing the first append after midnight can both enter. P1 renames
    // yesterday's live file to the dated name and appends a line to the fresh live file; P2
    // then renames THAT file over the same dated name, and since `rename(2)` replaces its
    // destination, yesterday's entire history is unlinked. Hold a per-log lock across the
    // `exists()` check and the rename so they are one critical section. Only taken on the day
    // a rotation is actually due — every other append stays lock-free. `acquire_path` never
    // blocks (a contended lock is an immediate `Err`), so the loser simply skips this
    // rotation and the next append performs it; the lock is best-effort like the rotation
    // itself (R12-5/R13-3: a read-only log dir must never kill the command), and only the
    // final append can fail the caller.
    if rotation_due(path, &today)
        && let Ok(_rotate_lock) =
            StoreLock::acquire_path_in_session(session, rotate_lock_path(path), control)
    {
        let _ = rotate_stale_log(path, &today);
    }
    let _ = prune_rotated_logs(path, &today, retention_days);
    append_jsonl(path, value)
}

fn scrub_lock_path(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    let parent = path.parent()?;
    if matches!(name, "events.jsonl" | "errors.jsonl" | "metrics.jsonl") {
        Some(parent.join("scrub.lock"))
    } else if name == "access.jsonl" {
        Some(parent.join("access.scrub.lock"))
    } else {
        None
    }
}

/// Cheap pre-check for [`append_jsonl_rotating`]: is the live log's last-written day older
/// than today? `rotate_stale_log` re-checks this under the lock, so this is only a hint that
/// keeps the common (same-day) append off the lock path.
fn rotation_due(path: &Path, today: &str) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    let file_date = date_of_system_time(modified);
    !file_date.is_empty() && file_date.as_str() < today
}

/// Per-log rotation lock, e.g. `.../logs/events.jsonl` → `.../logs/events.rotate.lock`.
fn rotate_lock_path(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "log".to_owned());
    path.with_file_name(format!("{stem}.rotate.lock"))
}

pub(super) fn rotate_stale_log(path: &Path, today: &str) -> Result<()> {
    let Ok(metadata) = fs::metadata(path) else {
        // No live file yet → nothing to rotate.
        return Ok(());
    };
    let modified = metadata.modified().kio_io(path)?;
    let file_date = date_of_system_time(modified);
    // Same day (or a backwards clock) → keep appending to the live file.
    if file_date.is_empty() || file_date.as_str() >= today {
        return Ok(());
    }
    let dated = dated_log_path(path, &file_date);
    // Never clobber an already-rotated dated file (rename is skipped, the live
    // file keeps growing until the next distinct day — harmless).
    if !dated.exists() {
        fs::rename(path, &dated).kio_io(&dated)?;
    }
    Ok(())
}

pub(super) fn prune_rotated_logs(path: &Path, today: &str, retention_days: u32) -> Result<()> {
    let (Some(stem), Some(ext), Some(parent)) = (
        path.file_stem().and_then(|s| s.to_str()),
        path.extension().and_then(|s| s.to_str()),
        path.parent(),
    ) else {
        return Ok(());
    };
    let Some(today_days) = date_to_days(today) else {
        return Ok(());
    };
    let prefix = format!("{stem}-");
    let suffix = format!(".{ext}");
    for entry in fs::read_dir(parent).kio_io(parent)? {
        let entry = entry.kio_io(parent)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(date) = name
            .strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(&suffix))
        else {
            continue;
        };
        let Some(file_days) = date_to_days(date) else {
            continue;
        };
        if today_days - file_days > i64::from(retention_days) {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(())
}

/// `events.jsonl` + `2026-07-05` → `events-2026-07-05.jsonl`.
fn dated_log_path(path: &Path, date: &str) -> PathBuf {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("log");
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("jsonl");
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!("{stem}-{date}.{ext}"))
}

fn date_of_system_time(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default();
    format_unix_seconds(secs)
        .get(..10)
        .unwrap_or_default()
        .to_owned()
}

/// Parse a `YYYY-MM-DD` date into days since the Unix epoch, or `None` when the
/// shape is malformed. Reuses the civil-date algorithm the timestamp formatter uses.
fn date_to_days(date: &str) -> Option<i64> {
    let bytes = date.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year = date.get(0..4)?.parse::<i64>().ok()?;
    let month = date.get(5..7)?.parse::<i64>().ok()?;
    let day = date.get(8..10)?.parse::<i64>().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(days_from_civil(year, month, day))
}

pub fn append_event_log(code: &str, message: &str, context: Value) -> Result<()> {
    crate::control::ControlHandle::default().append_event_log(code, message, context)
}

pub fn append_error_log(error: &KioError) -> Result<()> {
    crate::control::ControlHandle::default().append_error_log(error)
}

/// Record a `level=warn` observation to `errors.jsonl` (P3, CT2-ADAPTER-010).
/// Non-fatal: callers use it for warnings that must be observable but must not
/// stop startup (e.g. a world-readable plaintext `plain:` API key in tools.toml).
pub fn append_warn_log(code: &str, message: &str, context: Value) -> Result<()> {
    crate::control::ControlHandle::default().append_warn_log(code, message, context)
}

pub(crate) fn append_observation(
    session: &LockSession,
    control: &crate::control::ControlHandle,
    file_name: &str,
    level: &str,
    code: &str,
    message: &str,
    mut context: Value,
) -> Result<()> {
    // R13-6: never write the device-global log under a CWD-relative path. When
    // neither an absolute `XDG_DATA_HOME` nor an absolute `HOME` resolves,
    // `data_home()` degrades to `"."`; the CLI's startup guard rejects such an
    // invocation, but its OWN error would still be logged here first and scatter
    // `./kio/logs/errors.jsonl` under the working directory. Skip silently (logging
    // is best-effort) rather than create it.
    let log_dir = data_home(control).join("kio/logs");
    if !log_dir.is_absolute() {
        return Ok(());
    }
    // N3: honor `redact_logs` (06 §8 / 10 §11.6, default true) before writing. The
    // KioError context routinely carries a `path` (and search/adapter contexts a
    // `query`/`prompt`); writing them verbatim both violates the redaction policy
    // and defeats purge, whose scrubber assumes "path is never recorded". Mask the
    // sensitive keys recursively so nested contexts (e.g. an index partial-failure
    // `output`) are covered too.
    let redact = redact_logs_enabled(control);
    if redact {
        redact_context(&mut context);
    }
    // P4: several error Displays embed an absolute path in their *message*
    // (`io error at {path}`, `corrupt store file at {path}`), which N3's
    // context-only masking missed — the path then landed verbatim in
    // errors.jsonl, breaking the "path is never recorded" premise (10 §11.6) and,
    // combined with a group-readable errors.jsonl, leaking scope paths to other
    // local users. Mask absolute-path tokens in the message too under redact_logs.
    let message: Value = if redact {
        Value::String(redact_message_paths(message))
    } else {
        Value::String(message.to_owned())
    };
    let path = log_dir.join(file_name);
    // R13-3: the device-global logs (events/errors/metrics) are governed by the
    // device-level `[observability] retention_days` (default 30). Rotation/prune is
    // best-effort inside `append_jsonl_rotating`.
    let retention = read_logs_retention_days(&config_home(control).join("kio/config.toml"))
        .unwrap_or(DEFAULT_LOG_RETENTION_DAYS);
    append_jsonl_rotating_with_control(
        session,
        control,
        &path,
        &json!({
            "ts": control.now_utc_seconds(),
            "level": level,
            "code": code,
            "component": "kio-cli",
            "message": message,
            "context": context,
        }),
        retention,
    )
}

/// Replace every absolute-path-looking token (a whitespace-delimited run) in a
/// log message with `[redacted]` (P4). Whitespace is preserved exactly. This is
/// deliberately conservative: relative tokens are left alone; the leak sources
/// all emit absolute paths via `path.display()`.
pub(super) fn redact_message_paths(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut token = String::new();
    for ch in message.chars() {
        if ch.is_whitespace() {
            push_redacted_token(&token, &mut out);
            token.clear();
            out.push(ch);
        } else {
            token.push(ch);
        }
    }
    push_redacted_token(&token, &mut out);
    out
}

fn push_redacted_token(token: &str, out: &mut String) {
    if looks_like_absolute_path_token(token) {
        out.push_str("[redacted]");
    } else {
        out.push_str(token);
    }
}

fn looks_like_absolute_path_token(token: &str) -> bool {
    let bytes = token.as_bytes();
    let windows_drive_absolute = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    let windows_unc_absolute = token.len() > 2 && token.starts_with(r"\\");

    (token.len() > 1 && token.starts_with('/')) || windows_drive_absolute || windows_unc_absolute
}

/// Whether `redact_logs` is in effect (06 §8 default true). Read from the user
/// config's `[adapter.policy]`; the observation logs are device-global so the
/// device-level config governs them. Absent config / key -> the secure default.
fn redact_logs_enabled(control: &crate::control::ControlHandle) -> bool {
    let path = config_home(control).join("kio/config.toml");
    let Ok(text) = fs::read_to_string(&path) else {
        return true;
    };
    let Ok(value) = toml::from_str::<toml::Value>(&text) else {
        return true;
    };
    value
        .get("adapter")
        .and_then(|adapter| adapter.get("policy"))
        .and_then(|policy| policy.get("redact_logs"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(true)
}

/// Recursively replace the values of sensitive keys with `[redacted]` anywhere
/// they appear in a log `context` object/array (N3). The allowlist covers the
/// path-carrying keys other error contexts use: `scope_path`
/// (`purge_not_found_error`), `candidates` (`scope_ambiguous_error`, an array of
/// absolute paths), and `root_path`/`kio_path` (registry/scope contexts) — P4.
pub(super) fn redact_context(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, entry) in map.iter_mut() {
                if matches!(
                    key.as_str(),
                    "path"
                        | "query"
                        | "prompt"
                        | "scope_path"
                        | "candidates"
                        | "root_path"
                        | "kio_path"
                ) {
                    *entry = Value::String("[redacted]".to_owned());
                } else {
                    redact_context(entry);
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                redact_context(item);
            }
        }
        _ => {}
    }
}

fn config_home(control: &crate::control::ControlHandle) -> PathBuf {
    // R12-6 / R13-6: empty/relative `XDG_CONFIG_HOME` AND empty/relative `HOME` are
    // both invalid — fall back to `$HOME/.config` only for an absolute `HOME`,
    // never to a CWD-relative dir. The CLI startup guard rejects the no-absolute-
    // base case loudly before we reach the `"."` last resort.
    crate::xdg::xdg_dir("XDG_CONFIG_HOME")
        .or_else(|| crate::xdg::home_dir_with_control(control).map(|home| home.join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
}
