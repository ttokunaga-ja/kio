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
const MAX_OUTPUT: usize = 128 * 1024;

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
    expect_failure(
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

    cli_json(
        &options.binary,
        &private,
        Some(&root),
        &["watch", "--root", path_arg(&root)?, "service", "start"],
    )?;
    let first_pid = wait_running(&options.binary, &private, &root)?;
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
    wait_stopped(&options.binary, &private, &root)?;
    assert_pid_absent(first_pid)?;
    let stopped = "KIO_NATIVE_SERVICE_STOPPED_CHANGE";
    fs::write(root.join("stopped-change.txt"), stopped).map_err(io)?;
    assert_not_searchable(&options.binary, &private, &root, stopped)?;

    cli_json(
        &options.binary,
        &private,
        Some(&root),
        &["watch", "--root", path_arg(&root)?, "service", "start"],
    )?;
    let second_pid = wait_running(&options.binary, &private, &root)?;
    if second_pid == first_pid {
        return Err(invalid("restart reused the stopped watcher PID"));
    }
    wait_search(&options.binary, &private, &root, stopped)?;
    cli_json(
        &options.binary,
        &private,
        Some(&root),
        &["watch", "--root", path_arg(&root)?, "service", "stop"],
    )?;
    wait_stopped(&options.binary, &private, &root)?;
    assert_pid_absent(second_pid)?;
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
    assert_native_unregistered(&private, &id)?;

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
        Err(invalid("native service operation unexpectedly succeeded"))
    } else {
        Ok(())
    }
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
            timeout: COMMAND_TIMEOUT,
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
fn wait_running(binary: &Path, private: &Path, root: &Path) -> Result<u32, AcceptanceError> {
    wait_until(|| {
        let value = assert_status(binary, private, root, true, true, "running")?;
        value
            .pointer("/instance/last_observation/pid")
            .and_then(Value::as_u64)
            .filter(|pid| *pid > 0 && *pid <= u32::MAX as u64)
            .map(|pid| pid as u32)
            .ok_or_else(|| invalid("running service omitted its PID"))
    })
}
fn wait_stopped(binary: &Path, private: &Path, root: &Path) -> Result<(), AcceptanceError> {
    wait_until(|| assert_status(binary, private, root, true, false, "stopped").map(|_| ()))
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
    Ok(value
        .get("results")
        .or_else(|| value.get("result"))
        .and_then(Value::as_array)
        .is_some_and(|items| !items.is_empty()))
}
fn assert_not_searchable(
    binary: &Path,
    private: &Path,
    root: &Path,
    marker: &str,
) -> Result<(), AcceptanceError> {
    if search_has(binary, private, root, marker)? {
        Err(invalid(
            "content changed while stopped was indexed before restart",
        ))
    } else {
        Ok(())
    }
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

fn assert_pid_absent(pid: u32) -> Result<(), AcceptanceError> {
    #[cfg(unix)]
    {
        if unsafe { libc::kill(pid as i32, 0) } == 0 {
            return Err(invalid("watcher process remains after stop"));
        }
    }
    #[cfg(windows)]
    {
        let mut command = Command::new("tasklist.exe");
        command.args(["/FI", &format!("PID eq {pid}"), "/NH"]);
        let output = run_bounded_command(
            &mut command,
            BoundedProcessOptions {
                timeout: COMMAND_TIMEOUT,
                max_stdout_bytes: MAX_OUTPUT,
                max_stderr_bytes: MAX_OUTPUT,
            },
            None,
        )
        .map_err(|e| AcceptanceError::Command(e.to_string()))?;
        if output.status.success() && output.stdout.contains(&pid.to_string()) {
            return Err(invalid("watcher process remains after stop"));
        }
    }
    Ok(())
}
fn assert_native_unregistered(private: &Path, id: &str) -> Result<(), AcceptanceError> {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let unit = private
            .join("xdg-config/systemd/user")
            .join(format!("{id}.service"));
        if unit.exists() {
            return Err(invalid("systemd unit remains after uninstall"));
        }
        let mut command = Command::new("/usr/bin/systemctl");
        command.args(["--user", "is-enabled", id]);
        if run_bounded_command(
            &mut command,
            BoundedProcessOptions {
                timeout: COMMAND_TIMEOUT,
                max_stdout_bytes: MAX_OUTPUT,
                max_stderr_bytes: MAX_OUTPUT,
            },
            None,
        )
        .map_err(|e| AcceptanceError::Command(e.to_string()))?
        .status
        .success()
        {
            return Err(invalid("systemd still enables service after uninstall"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        let plist = private
            .join("Library/LaunchAgents")
            .join(format!("{id}.plist"));
        if plist.exists() {
            return Err(invalid("launchd plist remains after uninstall"));
        }
        let target = format!("gui/{}/{}", unsafe { libc::geteuid() }, id);
        let mut command = Command::new("/bin/launchctl");
        command.args(["print", &target]);
        if run_bounded_command(
            &mut command,
            BoundedProcessOptions {
                timeout: COMMAND_TIMEOUT,
                max_stdout_bytes: MAX_OUTPUT,
                max_stderr_bytes: MAX_OUTPUT,
            },
            None,
        )
        .map_err(|e| AcceptanceError::Command(e.to_string()))?
        .status
        .success()
        {
            return Err(invalid("launchd still has service after uninstall"));
        }
    }
    #[cfg(windows)]
    {
        let xml = private
            .join("xdg-data/kio/watch-services-native")
            .join(format!("{id}.xml"));
        if xml.exists() {
            return Err(invalid("Task Scheduler XML remains after uninstall"));
        }
        let mut command = Command::new("schtasks.exe");
        command.args(["/query", "/tn", id]);
        if run_bounded_command(
            &mut command,
            BoundedProcessOptions {
                timeout: COMMAND_TIMEOUT,
                max_stdout_bytes: MAX_OUTPUT,
                max_stderr_bytes: MAX_OUTPUT,
            },
            None,
        )
        .map_err(|e| AcceptanceError::Command(e.to_string()))?
        .status
        .success()
        {
            return Err(invalid("Task Scheduler still has service after uninstall"));
        }
    }
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
