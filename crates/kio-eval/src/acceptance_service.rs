//! Native per-user watch-scheduler acceptance.
//!
//! This executor deliberately invokes the packaged CLI and the operating
//! system scheduler it registers.  It is unavailable outside an explicitly
//! opted-in GitHub Actions runner: a contract test or an in-process scheduler
//! substitute is not acceptance evidence for A03/A11.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use kio_core::{cas::hash_bytes, dag::TreeEntry, scope::Repository};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    acceptance::{
        AcceptanceCase, AcceptanceError, AcceptanceLane, AcceptanceReceipt, AcceptanceSubcase,
        ExpectedReceipt, MAX_BINARY_BYTES, MAX_FIXTURE_BYTES, NativeOs,
        current_evaluator_sha256_matches, empty_runtime, sha256_regular_file, validate_expected,
        write_receipt_create_only,
    },
    acceptance_environment::IsolatedChildEnvironment,
    runner::{BoundedProcessOptions, run_bounded_command},
};

const TIMEOUT: Duration = Duration::from_secs(45);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
// A single Windows lifecycle command can invoke several individually bounded
// PowerShell workers. Leave room to get the product's operation-specific error.
const WINDOWS_SERVICE_COMMAND_TIMEOUT: Duration = Duration::from_secs(300);

fn command_timeout(windows: bool, args: &[&str]) -> Duration {
    if windows && matches!(args, ["watch", "--root", _, "service", ..]) {
        WINDOWS_SERVICE_COMMAND_TIMEOUT
    } else {
        COMMAND_TIMEOUT
    }
}
const MAX_OUTPUT: usize = 128 * 1024;

mod platform;

/// Inputs for a real native scheduler case.  `required_allow_native_service`
/// is intentionally not inferred from environment: the CLI boundary must
/// receive an explicit affirmative option from its workflow wiring.
#[derive(Debug, Clone)]
pub struct ServiceOptions {
    pub binary: PathBuf,
    pub fixture: PathBuf,
    pub expected: ExpectedReceipt,
    pub work_dir: PathBuf,
    pub receipt: PathBuf,
    pub preflight_report: PathBuf,
    pub required_allow_native_service: bool,
}

#[derive(Debug, Deserialize)]
struct PreflightReport {
    schema: String,
    runner_os: NativeOs,
    host_os: String,
    status: String,
    #[serde(default)]
    failures: Vec<String>,
}

/// Execute A11/service-native/core.  This covers scheduler registration,
/// initial and live convergence, conflict handling, clean stop, and native
/// unregistration.
pub fn run_a11(options: &ServiceOptions) -> Result<AcceptanceReceipt, AcceptanceError> {
    run(options, AcceptanceCase::A11)
}

/// Execute A03/service-native/core.  Its receipt is separate from A11 even
/// though it uses the same native scheduler fixture; aggregation binds the
/// evidence to the independently expected A03 requirement.
pub fn run_a03(options: &ServiceOptions) -> Result<AcceptanceReceipt, AcceptanceError> {
    run(options, AcceptanceCase::A03)
}

fn run(
    options: &ServiceOptions,
    case: AcceptanceCase,
) -> Result<AcceptanceReceipt, AcceptanceError> {
    validate_options(options, case)?;
    if options.work_dir.exists() || options.receipt.exists() {
        return Err(invalid(
            "service work directory and receipt must be create-only",
        ));
    }
    fs::create_dir(&options.work_dir).map_err(io)?;
    private_dir(&options.work_dir)?;
    let work = fs::canonicalize(&options.work_dir).map_err(io)?;
    let private = work.join("private");
    let root = work.join("root");
    for path in [
        &private,
        &root,
        &private.join("xdg-config"),
        &private.join("xdg-data"),
        &private.join("xdg-cache"),
        &private.join("tmp"),
    ] {
        fs::create_dir(path).map_err(io)?;
        private_dir(path)?;
    }
    let private = fs::canonicalize(&private).map_err(io)?;
    let root = fs::canonicalize(&root).map_err(io)?;

    // The expected fixture is the first indexed content.  All further leaves
    // are generated under the isolated root, never read from the developer's
    // checkout or HOME.
    let initial = root.join("initial.txt");
    fs::copy(&options.fixture, &initial).map_err(io)?;
    let mut cleanup = Cleanup::new(options.binary.clone(), private.clone(), root.clone());

    cli_json(&options.binary, &private, None, &["init", path_arg(&root)?])?;
    let install = cli_json(
        &options.binary,
        &private,
        Some(&root),
        &[
            "watch",
            "--root",
            path_arg(&root)?,
            "service",
            "install",
            "--reconcile-interval-seconds",
            "1",
        ],
    )?;
    let id = install
        .get("service_id")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| invalid("service install did not report a service_id"))?
        .to_owned();
    cleanup.created_id = Some(id.clone());
    assert_status(&options.binary, &private, &root, true, false, "stopped")?;
    let conflict = cli(
        &options.binary,
        &private,
        Some(&root),
        &[
            "watch",
            "--root",
            path_arg(&root)?,
            "service",
            "install",
            "--reconcile-interval-seconds",
            "2",
        ],
    )?;
    check_install_conflict(conflict.status.success(), &conflict.stderr)?;
    let repeated = cli_json(
        &options.binary,
        &private,
        Some(&root),
        &[
            "watch",
            "--root",
            path_arg(&root)?,
            "service",
            "install",
            "--reconcile-interval-seconds",
            "1",
        ],
    )?;
    check_unchanged_install(&repeated, &id)?;
    assert_status(&options.binary, &private, &root, true, false, "stopped")?;

    cli_json(
        &options.binary,
        &private,
        Some(&root),
        &["watch", "--root", path_arg(&root)?, "service", "start"],
    )?;
    let first = wait_running(&options.binary, &private, &root, None)?;
    let first_pid = first.0;
    let native_definition = platform::registered_definition(&private, &id)?;
    wait_search(
        &options.binary,
        &private,
        &root,
        &fixture_marker(&options.fixture)?,
    )?;
    let live = "KIO_NATIVE_SERVICE_LIVE_CHANGE";
    fs::write(root.join("live-change.txt"), live).map_err(io)?;
    wait_search(&options.binary, &private, &root, live)?;
    // A running instance must reject an additional scheduler start rather
    // than producing a second watcher.
    expect_failure(
        &options.binary,
        &private,
        Some(&root),
        &["watch", "--root", path_arg(&root)?, "service", "start"],
    )?;

    cli_json(
        &options.binary,
        &private,
        Some(&root),
        &["watch", "--root", path_arg(&root)?, "service", "stop"],
    )?;
    wait_stopped(&options.binary, &private, &root, &id)?;
    platform::assert_pid_absent(first_pid)?;
    let stopped = "KIO_NATIVE_SERVICE_STOPPED_CHANGE";
    let stopped_path = "stopped-change.txt";
    fs::write(root.join(stopped_path), stopped).map_err(io)?;
    assert_not_committed(&root, stopped_path)?;

    cli_json(
        &options.binary,
        &private,
        Some(&root),
        &["watch", "--root", path_arg(&root)?, "service", "start"],
    )?;
    let (second_pid, _) = wait_running(&options.binary, &private, &root, Some(&first))?;
    let expected_raw = hash_bytes(stopped.as_bytes());
    wait_until(|| {
        let entry = committed_entry(&root, stopped_path)?;
        if !entry.is_some_and(|entry| entry.raw_hash == expected_raw && entry.normalize.is_some()) {
            return Err(invalid(
                "restarted watcher has not published the normalized stopped change",
            ));
        }
        if !search_has(&options.binary, &private, &root, stopped)? {
            return Err(invalid(
                "search has not published the stopped change after restart",
            ));
        }
        Ok(())
    })?;
    cli_json(
        &options.binary,
        &private,
        Some(&root),
        &["watch", "--root", path_arg(&root)?, "service", "stop"],
    )?;
    wait_stopped(&options.binary, &private, &root, &id)?;
    platform::assert_pid_absent(second_pid)?;
    cli_json(
        &options.binary,
        &private,
        Some(&root),
        &["watch", "--root", path_arg(&root)?, "service", "uninstall"],
    )?;
    cleanup.created_id = None;
    expect_failure(
        &options.binary,
        &private,
        Some(&root),
        &["watch", "--root", path_arg(&root)?, "service", "status"],
    )?;
    match native_definition.symlink_metadata() {
        Ok(_) => return Err(invalid("native service definition remains after uninstall")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io(error)),
    }
    platform::assert_native_unregistered(&id)?;

    if sha256_regular_file(&options.binary, MAX_BINARY_BYTES)?
        != options.expected.candidate.binary_sha256
        || sha256_regular_file(&options.fixture, MAX_FIXTURE_BYTES)?
            != options.expected.fixture.sha256
    {
        return Err(invalid(
            "candidate binary or fixture changed during native service acceptance",
        ));
    }

    let receipt = AcceptanceReceipt {
        schema: "kio.acceptance.receipt/v4".into(),
        requirement: options.expected.requirement.clone(),
        candidate: options.expected.candidate.clone(),
        fixture: options.expected.fixture.clone(),
        workflow: options.expected.workflow.clone(),
        evaluator_sha256: current_evaluator_sha256_matches(&options.expected.evaluator_sha256)?,
        service_identity_sha256: options.expected.service_identity_sha256.clone(),
        contract_binary_sha256: options.expected.contract_binary_sha256.clone(),
        runtime: empty_runtime(),
        passed: true,
    };
    write_receipt_create_only(&options.receipt, &receipt)?;
    Ok(receipt)
}

fn validate_options(options: &ServiceOptions, case: AcceptanceCase) -> Result<(), AcceptanceError> {
    validate_expected(&options.expected)?;
    current_evaluator_sha256_matches(&options.expected.evaluator_sha256)?;
    let requirement = &options.expected.requirement;
    if requirement.case != case
        || requirement.lane != AcceptanceLane::ServiceNative
        || requirement.subcase != AcceptanceSubcase::Core
    {
        return Err(invalid(
            "service runner received a non-service-native core requirement",
        ));
    }
    if !options.required_allow_native_service
        || std::env::var("GITHUB_ACTIONS").ok().as_deref() != Some("true")
    {
        return Err(invalid(
            "native scheduler acceptance requires GITHUB_ACTIONS=true and explicit required_allow_native_service",
        ));
    }
    for path in [
        &options.binary,
        &options.fixture,
        &options.work_dir,
        &options.receipt,
        &options.preflight_report,
    ] {
        if !path.is_absolute() {
            return Err(invalid("service acceptance paths must be absolute"));
        }
    }
    if sha256_regular_file(&options.binary, MAX_BINARY_BYTES)?
        != options.expected.candidate.binary_sha256
    {
        return Err(invalid("candidate binary differs from expected binding"));
    }
    if !options.fixture.is_file()
        || sha256_regular_file(&options.fixture, MAX_FIXTURE_BYTES)?
            != options.expected.fixture.sha256
    {
        return Err(invalid("service fixture differs from expected binding"));
    }
    let report: PreflightReport = read_json(&options.preflight_report)?;
    if report.schema != "kio.v1.native-preflight/v1"
        || report.status != "passed"
        || !report.failures.is_empty()
        || report.runner_os != requirement.os
        || report.host_os != std::env::consts::OS
    {
        return Err(invalid(
            "service acceptance requires a passing matching native preflight report",
        ));
    }
    if !host_matches(requirement.os) {
        return Err(invalid(
            "expected native OS does not match this evaluator host",
        ));
    }
    Ok(())
}

fn cli_json(
    binary: &Path,
    private: &Path,
    root: Option<&Path>,
    args: &[&str],
) -> Result<Value, AcceptanceError> {
    let output = cli(binary, private, root, args)?;
    if !output.status.success() {
        return Err(command_error(args, &output.stderr));
    }
    serde_json::from_str(&output.stdout).map_err(|e| AcceptanceError::Json(e.to_string()))
}
fn expect_failure(
    binary: &Path,
    private: &Path,
    root: Option<&Path>,
    args: &[&str],
) -> Result<(), AcceptanceError> {
    if cli(binary, private, root, args)?.status.success() {
        Err(invalid(format!(
            "native service operation unexpectedly succeeded: {}",
            args.join(" ")
        )))
    } else {
        Ok(())
    }
}

fn check_install_conflict(succeeded: bool, stderr: &str) -> Result<(), AcceptanceError> {
    let error: Value = serde_json::from_str(stderr).unwrap_or(Value::Null);
    if succeeded
        || error.get("error_code").and_then(Value::as_str) != Some("KIO-E-WATCH-SERVICE-001")
        || !error
            .get("message")
            .and_then(Value::as_str)
            .is_some_and(|message| {
                message.starts_with("watch service definition differs; use install --replace")
            })
    {
        return Err(invalid(
            "unapproved watch service definition change was not rejected as a conflict",
        ));
    }
    Ok(())
}

fn check_unchanged_install(install: &Value, id: &str) -> Result<(), AcceptanceError> {
    if install.get("service_id").and_then(Value::as_str) != Some(id)
        || install.get("native_registration").and_then(Value::as_str) != Some("unchanged")
    {
        return Err(invalid(
            "identical watch service install did not preserve the registration",
        ));
    }
    Ok(())
}
fn cli(
    binary: &Path,
    private: &Path,
    root: Option<&Path>,
    args: &[&str],
) -> Result<crate::runner::BoundedProcessOutput, AcceptanceError> {
    let mut command = Command::new(binary);
    IsolatedChildEnvironment::new(
        private,
        private.join("xdg-config"),
        private.join("xdg-data"),
        private.join("xdg-cache"),
        private.join("tmp"),
    )
    .apply(&mut command)?;
    command.arg("--json").args(args);
    // The product deliberately carries only these scheduler transport values
    // into native control commands.  They are required to reach a CI user
    // manager, but are neither persisted by this evaluator nor reported.
    for name in ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    if let Some(root) = root {
        command.current_dir(root);
    }
    run_bounded_command(
        &mut command,
        BoundedProcessOptions {
            timeout: command_timeout(cfg!(windows), args),
            max_stdout_bytes: MAX_OUTPUT,
            max_stderr_bytes: MAX_OUTPUT,
        },
        None,
    )
    .map_err(|e| AcceptanceError::Command(format!("bounded service command failed: {e}")))
}

fn assert_status(
    binary: &Path,
    private: &Path,
    root: &Path,
    registered: bool,
    active: bool,
    state: &str,
) -> Result<Value, AcceptanceError> {
    let value = cli_json(
        binary,
        private,
        Some(root),
        &["watch", "--root", path_arg(root)?, "service", "status"],
    )?;
    let observed = value
        .pointer("/native_registration/registered")
        .and_then(Value::as_bool)
        == Some(registered)
        && value
            .pointer("/native_registration/active")
            .and_then(Value::as_bool)
            == Some(active)
        && value.pointer("/instance/status").and_then(Value::as_str) == Some(state);
    if !observed {
        return Err(invalid(
            "service status did not report the required native and instance state",
        ));
    }
    Ok(value)
}
fn wait_running(
    binary: &Path,
    private: &Path,
    root: &Path,
    stopped: Option<&(u32, String)>,
) -> Result<(u32, String), AcceptanceError> {
    wait_until(|| {
        let value = assert_status(binary, private, root, true, true, "running")?;
        running_identity(&value, stopped)
    })
}

fn running_identity(
    value: &Value,
    stopped: Option<&(u32, String)>,
) -> Result<(u32, String), AcceptanceError> {
    let observation = &value["instance"]["last_observation"];
    let pid = observation["pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid > 0)
        .ok_or_else(|| invalid("running service omitted its PID"))?;
    let instance = observation["instance"]
        .as_str()
        .filter(|instance| !instance.is_empty())
        .ok_or_else(|| invalid("running service omitted its instance ID"))?;
    // A new watcher holds the lock before it publishes its own observation.
    if stopped.is_some_and(|(old_pid, old_instance)| pid == *old_pid || instance == old_instance) {
        return Err(invalid(
            "restart has not published a fresh watcher PID and instance ID",
        ));
    }
    Ok((pid, instance.to_owned()))
}
fn wait_stopped(
    binary: &Path,
    private: &Path,
    root: &Path,
    _id: &str,
) -> Result<(), AcceptanceError> {
    wait_until(|| {
        // Linux reports enabling at login through this flag. The unit file
        // remains installed after stop, with UnitFileState=disabled.
        let registered = !cfg!(target_os = "linux");
        assert_status(binary, private, root, registered, false, "stopped")?;
        #[cfg(target_os = "linux")]
        platform::assert_disabled(_id)?;
        Ok(())
    })
}
fn wait_search(
    binary: &Path,
    private: &Path,
    root: &Path,
    marker: &str,
) -> Result<(), AcceptanceError> {
    wait_until(|| {
        if search_has(binary, private, root, marker)? {
            Ok(())
        } else {
            Err(invalid("watcher has not converged yet"))
        }
    })
}
fn wait_until<T>(mut f: impl FnMut() -> Result<T, AcceptanceError>) -> Result<T, AcceptanceError> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match f() {
            Ok(value) => return Ok(value),
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                thread::sleep(Duration::from_secs(1));
            }
            Err(error) => return Err(error),
        }
    }
}
fn search_has(
    binary: &Path,
    private: &Path,
    root: &Path,
    marker: &str,
) -> Result<bool, AcceptanceError> {
    let value = cli_json(
        binary,
        private,
        Some(root),
        &["search", marker, "--mode", "text", "--offline"],
    )?;
    search_contains_marker(&value, marker)
}

fn search_contains_marker(value: &Value, marker: &str) -> Result<bool, AcceptanceError> {
    let hits = value
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("text search omitted its results array"))?;
    // Text search ORs the query units, so unrelated chunks can hit shared
    // terms such as SERVICE or NATIVE. Verify the full marker in the chunk.
    for hit in hits {
        let snippet = hit
            .get("snippet")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("text search hit omitted its snippet"))?;
        if snippet.contains(marker) {
            return Ok(true);
        }
    }
    Ok(false)
}

// Search is a read-only projection and an interrupted writer can leave it
// rebuilding. Inspect the scope authority directly without triggering repair.
fn committed_entry(root: &Path, path: &str) -> Result<Option<TreeEntry>, AcceptanceError> {
    let inspect = || -> kio_core::Result<Option<TreeEntry>> {
        let repo = Repository::open_read_only(root)?;
        let head = repo
            .head_commit_hash()?
            .ok_or_else(|| kio_core::KioError::schema("managed scope has no committed HEAD"))?;
        let tree = repo.read_tree(&repo.read_commit(&head)?.tree)?;
        let entry = tree.find_entry(path)?;
        if repo.head_commit_hash()?.as_deref() != Some(head.as_str()) {
            return Err(kio_core::KioError::schema("HEAD changed during inspection"));
        }
        Ok(entry)
    };
    inspect().map_err(|error| {
        invalid(format!(
            "inspect committed path {path} in {}: {error}",
            root.display()
        ))
    })
}

fn assert_not_committed(root: &Path, path: &str) -> Result<(), AcceptanceError> {
    if committed_entry(root, path)?.is_some() {
        return Err(invalid(format!(
            "content changed while stopped was committed before restart: {path}"
        )));
    }
    Ok(())
}
fn fixture_marker(path: &Path) -> Result<String, AcceptanceError> {
    let bytes = fs::read(path).map_err(io)?;
    if bytes.len() as u64 > MAX_FIXTURE_BYTES {
        return Err(invalid("service fixture exceeds its byte limit"));
    }
    std::str::from_utf8(&bytes)
        .map_err(|_| invalid("service fixture must be UTF-8"))?
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| invalid("service fixture must contain a searchable marker"))
}
fn read_json(path: &Path) -> Result<PreflightReport, AcceptanceError> {
    serde_json::from_slice(&fs::read(path).map_err(io)?)
        .map_err(|e| AcceptanceError::Json(e.to_string()))
}
fn command_error(args: &[&str], stderr: &str) -> AcceptanceError {
    AcceptanceError::Command(format!("{} failed: {}", args.join(" "), stderr.trim()))
}
fn invalid(message: impl Into<String>) -> AcceptanceError {
    AcceptanceError::Invalid(message.into())
}
fn io(error: std::io::Error) -> AcceptanceError {
    AcceptanceError::Io(error.to_string())
}
fn path_arg(path: &Path) -> Result<&str, AcceptanceError> {
    path.to_str()
        .ok_or_else(|| invalid("service path is not UTF-8"))
}
fn host_matches(os: NativeOs) -> bool {
    matches!(
        (os, std::env::consts::OS),
        (NativeOs::Linux, "linux") | (NativeOs::Macos, "macos") | (NativeOs::Windows, "windows")
    )
}
#[cfg(unix)]
fn private_dir(path: &Path) -> Result<(), AcceptanceError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(io)
}
#[cfg(not(unix))]
fn private_dir(_path: &Path) -> Result<(), AcceptanceError> {
    Ok(())
}

struct Cleanup {
    binary: PathBuf,
    private: PathBuf,
    root: PathBuf,
    created_id: Option<String>,
}
impl Cleanup {
    fn new(binary: PathBuf, private: PathBuf, root: PathBuf) -> Self {
        Self {
            binary,
            private,
            root,
            created_id: None,
        }
    }
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        // The CLI performs its own manifest/native-definition ownership check;
        // never issue broad scheduler cleanup from an evaluator failure path.
        if self.created_id.is_some() {
            let root = match self.root.to_str() {
                Some(value) => value,
                None => return,
            };
            let _ = cli(
                &self.binary,
                &self.private,
                Some(&self.root),
                &["watch", "--root", root, "service", "stop"],
            );
            let _ = cli(
                &self.binary,
                &self.private,
                Some(&self.root),
                &["watch", "--root", root, "service", "uninstall"],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_windows_scheduler_commands_get_the_larger_outer_budget() {
        let service = ["watch", "--root", "fixture", "service", "install"];
        assert_eq!(
            command_timeout(true, &service),
            WINDOWS_SERVICE_COMMAND_TIMEOUT
        );
        assert_eq!(command_timeout(false, &service), COMMAND_TIMEOUT);
        assert_eq!(
            command_timeout(true, &["search", "marker"]),
            COMMAND_TIMEOUT
        );
    }

    #[test]
    fn restart_waits_for_a_new_pid_and_instance() {
        let observation = |pid, instance| {
            serde_json::json!({
                "instance": {"last_observation": {"pid": pid, "instance": instance}}
            })
        };
        let stopped = running_identity(&observation(12, "old"), None).unwrap();
        for value in [
            observation(12, "old"),
            observation(12, "new"),
            observation(13, "old"),
        ] {
            assert!(running_identity(&value, Some(&stopped)).is_err());
        }
        assert_eq!(
            running_identity(&observation(13, "new"), Some(&stopped)).unwrap(),
            (13, "new".into())
        );
    }

    use super::*;

    #[test]
    fn stopped_change_is_checked_against_committed_head() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let repo = Repository::init(root).unwrap();
        let path = "stopped-change.txt";
        assert!(
            assert_not_committed(root, path).is_err(),
            "a missing HEAD is not evidence"
        );

        fs::write(root.join("live-change.txt"), "already indexed").unwrap();
        repo.snapshot(None, None).unwrap();
        fs::write(root.join(path), "stopped edit").unwrap();
        assert_not_committed(root, path).unwrap();

        repo.snapshot(None, None).unwrap();
        assert_eq!(
            committed_entry(root, path).unwrap().unwrap().raw_hash,
            hash_bytes(b"stopped edit"),
        );
        assert!(assert_not_committed(root, path).is_err());
    }

    #[test]
    fn overlapping_search_hits_do_not_prove_a_marker_was_indexed() {
        let stopped = "KIO_NATIVE_SERVICE_STOPPED_CHANGE";
        let mut result = serde_json::json!({
            "results": [
                {"snippet": "kio v1 A03 native service fixture"},
                {"snippet": "KIO_NATIVE_SERVICE_LIVE_CHANGE"}
            ]
        });
        assert!(!search_contains_marker(&result, stopped).unwrap());
        result["results"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "snippet": format!("```txt\n{stopped}\n```")
            }));
        assert!(search_contains_marker(&result, stopped).unwrap());
        assert!(search_contains_marker(&Value::Null, stopped).is_err());
    }

    #[test]
    fn identical_install_must_preserve_the_same_registration() {
        let result = serde_json::json!({
            "service_id": "same-id",
            "native_registration": "unchanged",
        });
        assert!(check_unchanged_install(&result, "same-id").is_ok());
        assert!(check_unchanged_install(&result, "different-id").is_err());
        let mut result = result;
        result["native_registration"] = Value::from("enabled_not_started");
        assert!(check_unchanged_install(&result, "same-id").is_err());
    }

    #[test]
    fn changed_install_requires_the_definition_conflict_error() {
        let conflict = r#"{"error_code":"KIO-E-WATCH-SERVICE-001","message":"watch service definition differs; use install --replace to review and confirm the update"}"#;
        assert!(check_install_conflict(false, conflict).is_ok());
        assert!(check_install_conflict(true, conflict).is_err());
        assert!(check_install_conflict(false, "scheduler was not available").is_err());
        assert!(
            check_install_conflict(false, r#"{"error_code":"KIO-E-WATCH-SERVICE-001"}"#).is_err()
        );
    }
}
