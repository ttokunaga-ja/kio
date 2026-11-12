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
    let directory = crate::cas::private_log_parent(path)?;
    let leaf = path
        .file_name()
        .ok_or_else(|| KioError::io("log path has no leaf", path.display().to_string()))?;
    append_jsonl_rotating_at(
        session,
        control,
        &directory,
        Path::new(leaf),
        value,
        retention_days,
    )
}

fn append_jsonl_rotating_at(
    session: &LockSession,
    control: &crate::control::ControlHandle,
    directory: &crate::store_dir::StoreDirectory,
    leaf: &Path,
    value: &Value,
    retention_days: u32,
) -> Result<()> {
    crate::private_fs::verify_private_directory_handle(directory)?;
    if directory.contains_entry(leaf)? {
        directory.ensure_owner_private(leaf)?;
    }
    // Locks and all log operations share the originally admitted capability.
    // An external ancestor rename cannot redirect any part of this lease.
    let _scrub_lock = scrub_lock_path(leaf)
        .map(|name| StoreLock::acquire_at_in_session(session, directory, &name, control))
        .transpose()?;
    let today = control
        .now_utc_seconds()
        .get(..10)
        .unwrap_or_default()
        .to_owned();
    if rotation_due_at(directory, leaf, &today)
        && let Ok(_rotate_lock) =
            StoreLock::acquire_at_in_session(session, directory, &rotate_lock_path(leaf), control)
    {
        let _ = rotate_stale_log_at(directory, leaf, &today);
    }
    let _ = prune_rotated_logs_at(directory, leaf, &today, retention_days);
    crate::cas::append_jsonl_at(directory, leaf, value)
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
fn private_log_metadata(
    directory: &crate::store_dir::StoreDirectory,
    leaf: &Path,
) -> Result<std::fs::Metadata> {
    let file = directory.open_regular_read(leaf, u64::MAX)?;
    crate::private_fs::verify_private_regular_file_handle(&file)?;
    file.metadata().kio_io(&directory.path().join(leaf))
}

fn rotation_due_at(directory: &crate::store_dir::StoreDirectory, leaf: &Path, today: &str) -> bool {
    let Ok(metadata) = private_log_metadata(directory, leaf) else {
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

#[cfg(test)]
pub(super) fn rotate_stale_log(path: &Path, today: &str) -> Result<()> {
    let directory = crate::cas::private_log_parent(path)?;
    let leaf = path
        .file_name()
        .ok_or_else(|| KioError::locked("log path has no leaf"))?;
    rotate_stale_log_at(&directory, Path::new(leaf), today)
}

fn rotate_stale_log_at(
    directory: &crate::store_dir::StoreDirectory,
    leaf: &Path,
    today: &str,
) -> Result<()> {
    let Ok(metadata) = private_log_metadata(directory, leaf) else {
        return Ok(());
    };
    let modified = metadata.modified().kio_io(&directory.path().join(leaf))?;
    let file_date = date_of_system_time(modified);
    if file_date.is_empty() || file_date.as_str() >= today {
        return Ok(());
    }
    let dated = dated_log_path(leaf, &file_date);
    if !directory.contains_entry(&dated)? {
        directory.rename_regular_between_raw(
            leaf,
            directory,
            &dated,
            crate::store_dir::Publication::CreateOnly,
        )?;
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn prune_rotated_logs(path: &Path, today: &str, retention_days: u32) -> Result<()> {
    let directory = crate::cas::private_log_parent(path)?;
    let leaf = path
        .file_name()
        .ok_or_else(|| KioError::locked("log path has no leaf"))?;
    prune_rotated_logs_at(&directory, Path::new(leaf), today, retention_days)
}

fn prune_rotated_logs_at(
    directory: &crate::store_dir::StoreDirectory,
    leaf: &Path,
    today: &str,
    retention_days: u32,
) -> Result<()> {
    let (Some(stem), Some(ext)) = (
        leaf.file_stem().and_then(|s| s.to_str()),
        leaf.extension().and_then(|s| s.to_str()),
    ) else {
        return Ok(());
    };
    let Some(today_days) = date_to_days(today) else {
        return Ok(());
    };
    let prefix = format!("{stem}-");
    let suffix = format!(".{ext}");
    for entry in directory.entries(Path::new(""))? {
        let Some(name) = entry.name.to_str() else {
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
        if today_days - file_days > i64::from(retention_days)
            && directory
                .ensure_owner_private(Path::new(&entry.name))
                .is_ok()
        {
            let _ = directory.remove_file(Path::new(&entry.name));
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

#[cfg(test)]
thread_local! {
    static LOGS_ADMITTED_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
fn after_logs_admitted() {
    let hook = LOGS_ADMITTED_HOOK.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
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
    let external_path = data_home(control);
    let owned_path = external_path.join("kio");
    if !owned_path.is_absolute() {
        return Ok(());
    }
    // XDG ancestors are external namespace only. The explicit Kio-owned root
    // must be private before any log child, rotation, or lock is created.
    let external = crate::private_fs::retain_or_create_external_directory_with_control(
        &external_path,
        control,
    )?;
    let owned = StoreDirectory::from_retained_with_control(
        external.create_directory_all(Path::new("kio"))?,
        owned_path,
        control.clone(),
    )?;
    crate::private_fs::verify_private_directory_handle(&owned)?;
    verify_device_log_root(&owned)?;
    let log_dir = owned.path().join("logs");
    let logs = StoreDirectory::from_retained(
        owned.create_directory_all(Path::new("logs"))?,
        log_dir.clone(),
    )?;
    crate::private_fs::verify_private_directory_handle(&logs)?;
    verify_device_log_root(&owned)?;
    #[cfg(test)]
    after_logs_admitted();
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
    // R13-3: the device-global logs (events/errors/metrics) are governed by the
    // device-level `[observability] retention_days` (default 30). Rotation/prune is
    // best-effort inside `append_jsonl_rotating`.
    let retention = read_logs_retention_days(&config_home(control).join("kio/config.toml"))
        .unwrap_or(DEFAULT_LOG_RETENTION_DAYS);
    append_jsonl_rotating_at(
        session,
        control,
        &logs,
        Path::new(file_name),
        &json!({
            "ts": control.now_utc_seconds(),
            "level": level,
            "code": code,
            "component": "kio-cli",
            "message": message,
            "context": context,
        }),
        retention,
    )?;
    verify_device_log_root(&owned)
}

fn verify_device_log_root(owned: &StoreDirectory) -> Result<()> {
    crate::private_fs::verify_private_directory_handle(owned)?;
    let named = crate::platform::real_directory_identity(owned.path()).kio_io(owned.path())?;
    if named != crate::platform::directory_handle_identity(&owned.root_handle()) {
        return Err(KioError::io(
            "device log root changed during append",
            owned.path().display().to_string(),
        ));
    }
    Ok(())
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

#[cfg(all(test, unix))]
mod owned_root_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn device_logging_creates_missing_data_home_beneath_external_alias() {
        use std::os::unix::fs::symlink;

        let _lock = crate::test_control::test_env_lock().lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().canonicalize().unwrap().join("external-parent");
        fs::create_dir(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o777)).unwrap();
        let alias = temp.path().join("parent-alias");
        symlink(&real, &alias).unwrap();
        let data_home = alias.join("new-data");
        let _data = crate::test_control::TestEnvGuard::set("XDG_DATA_HOME", &data_home);
        let _config =
            crate::test_control::TestEnvGuard::set("XDG_CONFIG_HOME", real.join("config"));
        assert!(!real.join("new-data").exists());

        let control = crate::control::ControlHandle::default();
        control
            .append_event_log("TEST", "under external alias", serde_json::json!({}))
            .unwrap();
        let event = fs::read_to_string(real.join("new-data/kio/logs/events.jsonl")).unwrap();
        assert!(event.contains("under external alias"));
        assert_eq!(
            fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o777
        );
        for owned in ["new-data", "new-data/kio", "new-data/kio/logs"] {
            assert_eq!(
                fs::metadata(real.join(owned)).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        assert!(
            fs::symlink_metadata(&alias)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn device_logging_refuses_unsafe_owned_root_without_adopting_external_ancestor() {
        let _lock = crate::test_control::test_env_lock().lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let external = temp.path().canonicalize().unwrap();
        fs::set_permissions(&external, fs::Permissions::from_mode(0o777)).unwrap();
        let _data = crate::test_control::TestEnvGuard::set("XDG_DATA_HOME", &external);
        let _config =
            crate::test_control::TestEnvGuard::set("XDG_CONFIG_HOME", external.join("config"));
        let control = crate::control::ControlHandle::default();
        control
            .append_event_log("TEST", "first", serde_json::json!({}))
            .unwrap();
        let root = external.join("kio");
        let log = root.join("logs/events.jsonl");
        let bytes = fs::read(&log).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            control
                .append_event_log("TEST", "second", serde_json::json!({}))
                .is_err()
        );
        assert_eq!(fs::read(&log).unwrap(), bytes);
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(external).unwrap().permissions().mode() & 0o777,
            0o777
        );
    }
}

#[cfg(test)]
mod retained_log_root_tests {
    use super::*;
    use std::{cell::Cell, collections::BTreeMap, rc::Rc};

    fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        fn visit(root: &Path, relative: &Path, entries: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
            for entry in fs::read_dir(root.join(relative)).unwrap() {
                let entry = entry.unwrap();
                let path = relative.join(entry.file_name());
                let kind = entry.file_type().unwrap();
                if kind.is_dir() {
                    entries.insert(path.clone(), None);
                    visit(root, &path, entries);
                } else {
                    assert!(kind.is_file(), "fixture contains an unexpected link");
                    entries.insert(path, Some(fs::read(entry.path()).unwrap()));
                }
            }
        }
        let mut entries = BTreeMap::new();
        visit(root, Path::new(""), &mut entries);
        entries
    }

    #[test]
    fn device_logging_does_not_mutate_private_replacement_after_admission() {
        let _lock = crate::test_control::test_env_lock().lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let external = temp.path().canonicalize().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&external, fs::Permissions::from_mode(0o777)).unwrap();
        }
        let _data = crate::test_control::TestEnvGuard::set("XDG_DATA_HOME", &external);
        let _config =
            crate::test_control::TestEnvGuard::set("XDG_CONFIG_HOME", external.join("config"));
        let control = crate::control::ControlHandle::default();
        control
            .append_event_log("TREE-A", "first", json!({}))
            .unwrap();
        let original = external.join("kio");
        let replacement = external.join("replacement-kio");
        let parked = external.join("admitted-kio");
        let outer = StoreDirectory::open(&external).unwrap();
        let other = StoreDirectory::from_retained(
            outer
                .create_directory(Path::new("replacement-kio"))
                .unwrap(),
            replacement.clone(),
        )
        .unwrap();
        other.create_directory_all(Path::new("logs")).unwrap();
        for (name, bytes) in [
            ("logs/events.jsonl", b"{\"tree\":\"B\"}\n".as_slice()),
            (
                "logs/events-2000-01-01.jsonl",
                b"old private B record\n".as_slice(),
            ),
        ] {
            other
                .write_atomic(
                    Path::new(name),
                    bytes,
                    crate::store_dir::Publication::CreateOnly,
                )
                .unwrap();
        }
        crate::private_fs::verify_private_directory_handle(&other).unwrap();
        let before = snapshot(&replacement);
        drop(other);
        drop(outer);
        let attempted = Rc::new(Cell::new(false));
        let swapped = Rc::new(Cell::new(false));
        let attempted_hook = attempted.clone();
        let swapped_hook = swapped.clone();
        let original_hook = original.clone();
        let replacement_hook = replacement.clone();
        let parked_hook = parked.clone();
        LOGS_ADMITTED_HOOK.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(Box::new(move || {
                attempted_hook.set(true);
                if let Err(error) = fs::rename(&original_hook, &parked_hook) {
                    #[cfg(not(windows))]
                    panic!("cannot exercise root replacement: {error}");
                    #[cfg(windows)]
                    assert!(
                        matches!(error.raw_os_error(), Some(5 | 32 | 33)),
                        "unexpected rename refusal: {error}"
                    );
                    #[cfg(windows)]
                    return;
                }
                if let Err(error) = fs::rename(&replacement_hook, &original_hook) {
                    fs::rename(&parked_hook, &original_hook).unwrap();
                    #[cfg(not(windows))]
                    panic!("cannot publish replacement fixture: {error}");
                    #[cfg(windows)]
                    assert!(
                        matches!(error.raw_os_error(), Some(5 | 32 | 33)),
                        "unexpected rename refusal: {error}"
                    );
                    #[cfg(windows)]
                    return;
                }
                swapped_hook.set(true);
            }));
        });
        // A safe refusal or retained-A append is acceptable. A path-only reopen
        // must never rotate, prune, lock, or append inside private replacement B.
        let _result = control.append_event_log("TREE-A", "after admission", json!({}));
        assert!(
            attempted.get(),
            "the deterministic admission seam did not execute"
        );
        LOGS_ADMITTED_HOOK.with(|slot| assert!(slot.borrow().is_none()));
        let current_b = if swapped.get() {
            &original
        } else {
            &replacement
        };
        assert_eq!(
            snapshot(current_b),
            before,
            "private replacement B was mutated after A was admitted"
        );
        if swapped.get() {
            assert!(parked.is_dir());
            assert!(
                fs::read_to_string(parked.join("logs/events.jsonl"))
                    .unwrap()
                    .contains("first")
            );
        }
    }
}
