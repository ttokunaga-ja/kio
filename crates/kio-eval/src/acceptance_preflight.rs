//! Fail-closed host checks for the native v1 acceptance lanes.
//!
//! This is intentionally an evaluator-side diagnostic, rather than product
//! behavior.  It records only bounded command results and never emits the
//! ambient environment.  A passing preflight is not an acceptance receipt.

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use kio_core::cas::canonical_json_bytes;
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    acceptance::NativeOs,
    runner::{BoundedProcessOptions, run_bounded_command},
};

const MAX_CAPTURE_BYTES: usize = 16 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_CONVERTER_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct PreflightOptions {
    pub runner_os: NativeOs,
    pub office_converter: PathBuf,
    pub out: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreflightOutcome {
    pub passed: bool,
}

#[derive(Debug, Error)]
pub enum PreflightError {
    #[error("invalid native preflight input: {0}")]
    Input(String),
    #[error("could not inspect native preflight input: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not serialize native preflight report: {0}")]
    Serialize(String),
}

#[derive(Debug, Serialize)]
struct PreflightReport {
    schema: &'static str,
    runner_os: NativeOs,
    host_os: &'static str,
    checks: Vec<CommandRecord>,
    failures: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    office_converter: Option<ConverterRecord>,
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct ConverterRecord {
    requested: String,
    resolved: String,
    sha256: String,
}

#[derive(Debug, Serialize)]
struct CommandRecord {
    argv: Vec<String>,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    returncode: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stderr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'static str>,
}

/// Check the runner capabilities needed to exercise real Office conversion and
/// a real per-user native scheduler.  The report is always written when the
/// checks themselves fail, so the workflow can upload diagnostics in `always`.
pub fn run(options: &PreflightOptions) -> Result<PreflightOutcome, PreflightError> {
    let mut report = PreflightReport {
        schema: "kio.v1.native-preflight/v1",
        runner_os: options.runner_os,
        host_os: std::env::consts::OS,
        checks: Vec::new(),
        failures: Vec::new(),
        office_converter: None,
        status: "failed",
    };

    if !host_matches(options.runner_os) {
        report
            .failures
            .push("workflow matrix runner OS does not match the evaluator host".into());
    }
    if std::env::var("KIO_REAL_OFFICE").ok().as_deref() != Some("1") {
        report
            .failures
            .push("KIO_REAL_OFFICE must be exactly 1".into());
    }
    if std::env::var_os("KIO_TEST_OFFICE_CONVERT").is_some() {
        report
            .failures
            .push("KIO_TEST_OFFICE_CONVERT must be absent for real Office acceptance".into());
    }

    match validated_converter(&options.office_converter) {
        Ok((requested, resolved, sha256)) => {
            report.office_converter = Some(ConverterRecord {
                requested,
                resolved: display_path(&resolved)?,
                sha256,
            });
            let check = command_record(&resolved, &["--version"]);
            require(
                &check,
                "Office converter did not run successfully",
                &mut report.failures,
            );
            report.checks.push(check);
        }
        Err(error) => report.failures.push(error.to_string()),
    }

    match options.runner_os {
        NativeOs::Linux => check_linux(&mut report),
        NativeOs::Macos => check_macos(&mut report),
        NativeOs::Windows => check_windows(&mut report),
    }

    let passed = report.failures.is_empty();
    report.status = if passed { "passed" } else { "failed" };
    write_create_only(&options.out, &report)?;
    Ok(PreflightOutcome { passed })
}

fn host_matches(runner_os: NativeOs) -> bool {
    matches!(
        (runner_os, std::env::consts::OS),
        (NativeOs::Linux, "linux") | (NativeOs::Macos, "macos") | (NativeOs::Windows, "windows")
    )
}

fn validated_converter(path: &Path) -> Result<(String, PathBuf, String), PreflightError> {
    if !path.is_absolute() {
        return Err(PreflightError::Input(
            "KIO_OFFICE_CONVERTER must name an executable absolute file".into(),
        ));
    }
    let requested = display_path(path)?;
    let resolved = fs::canonicalize(path).map_err(|_| {
        PreflightError::Input("KIO_OFFICE_CONVERTER must name an executable absolute file".into())
    })?;
    let metadata = fs::metadata(&resolved).map_err(|_| {
        PreflightError::Input("KIO_OFFICE_CONVERTER must name an executable absolute file".into())
    })?;
    if !metadata.is_file() || !is_executable(&metadata) {
        return Err(PreflightError::Input(
            "KIO_OFFICE_CONVERTER must name an executable absolute file".into(),
        ));
    }
    if metadata.len() > MAX_CONVERTER_BYTES {
        return Err(PreflightError::Input(format!(
            "Office converter exceeds the {MAX_CONVERTER_BYTES}-byte preflight hash limit"
        )));
    }
    Ok((requested, resolved.clone(), sha256_file(&resolved)?))
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    // Windows has no executable permission bit.  The bounded `--version`
    // invocation is the authoritative executable check on that platform.
    true
}

fn display_path(path: &Path) -> Result<String, PreflightError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| PreflightError::Input("preflight paths must be valid UTF-8".into()))
}

fn sha256_file(path: &Path) -> Result<String, PreflightError> {
    let mut input = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn check_linux(report: &mut PreflightReport) {
    let bubblewrap = [
        Path::new("/usr/bin/bwrap"),
        Path::new("/usr/local/bin/bwrap"),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file());
    match bubblewrap {
        Some(path) => {
            let check = command_record(path, &["--version"]);
            require(
                &check,
                "bubblewrap did not run successfully",
                &mut report.failures,
            );
            report.checks.push(check);
        }
        None => report.failures.push("bubblewrap is unavailable".into()),
    }
    let check = command_record(
        Path::new("/usr/bin/systemctl"),
        &["--user", "show", "--property=Version"],
    );
    require(
        &check,
        "systemd user manager is unavailable",
        &mut report.failures,
    );
    report.checks.push(check);
}

fn check_macos(report: &mut PreflightReport) {
    let check = command_record(
        Path::new("/usr/bin/sandbox-exec"),
        &["-p", "(version 1) (allow default)", "/usr/bin/true"],
    );
    require(
        &check,
        "sandbox-exec confinement is unavailable",
        &mut report.failures,
    );
    report.checks.push(check);

    let uid = current_uid();
    let launch_domain = format!("gui/{uid}");
    let check = command_record(
        Path::new("/bin/launchctl"),
        &["print-disabled", launch_domain.as_str()],
    );
    require(
        &check,
        "launchd GUI domain is unavailable",
        &mut report.failures,
    );
    report.checks.push(check);
}

#[cfg(unix)]
fn current_uid() -> u32 {
    // libc is already a direct Unix dependency of this crate.
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

fn check_windows(report: &mut PreflightReport) {
    let system_root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\\Windows"));
    let schtasks = system_root.join("System32").join("schtasks.exe");
    let sc = system_root.join("System32").join("sc.exe");
    if schtasks.is_file() {
        let check = command_record(&schtasks, &["/query", "/fo", "LIST"]);
        require(&check, "Task Scheduler query failed", &mut report.failures);
        report.checks.push(check);
    } else {
        report.failures.push("schtasks.exe is unavailable".into());
    }
    if sc.is_file() {
        let check = command_record(&sc, &["query", "Schedule"]);
        require(
            &check,
            "Task Scheduler service query failed",
            &mut report.failures,
        );
        if check.ok
            && !check
                .stdout
                .as_deref()
                .unwrap_or_default()
                .to_ascii_uppercase()
                .contains("RUNNING")
        {
            report
                .failures
                .push("Task Scheduler service is not running".into());
        }
        report.checks.push(check);
    } else {
        report.failures.push("sc.exe is unavailable".into());
    }
}

fn command_record(program: &Path, args: &[&str]) -> CommandRecord {
    let argv = std::iter::once(program)
        .map(|path| path.to_string_lossy().into_owned())
        .chain(args.iter().map(|arg| (*arg).to_owned()))
        .collect();
    let mut command = Command::new(program);
    command.args(args);
    match run_bounded_command(
        &mut command,
        BoundedProcessOptions {
            timeout: COMMAND_TIMEOUT,
            max_stdout_bytes: MAX_CAPTURE_BYTES,
            max_stderr_bytes: MAX_CAPTURE_BYTES,
        },
        None,
    ) {
        Ok(output) => CommandRecord {
            argv,
            ok: output.status.success(),
            returncode: output.status.code(),
            stdout: Some(output.stdout),
            stderr: Some(output.stderr),
            error: None,
        },
        Err(_) => CommandRecord {
            argv,
            ok: false,
            returncode: None,
            stdout: None,
            stderr: None,
            error: Some("bounded command did not complete"),
        },
    }
}

fn require(check: &CommandRecord, failure: &str, failures: &mut Vec<String>) {
    if !check.ok {
        failures.push(failure.into());
    }
}

fn write_create_only(path: &Path, report: &PreflightReport) -> Result<(), PreflightError> {
    let parent = path.parent().ok_or_else(|| {
        PreflightError::Input("preflight output path must have a parent directory".into())
    })?;
    fs::create_dir_all(parent)?;
    let bytes = canonical_json_bytes(
        &serde_json::to_value(report)
            .map_err(|error| PreflightError::Serialize(error.to_string()))?,
    )
    .map_err(|error| PreflightError::Serialize(error.to_string()))?;
    let mut output = OpenOptions::new().write(true).create_new(true).open(path)?;
    output.write_all(&bytes)?;
    output.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::host_matches;
    use crate::acceptance::NativeOs;

    #[test]
    fn host_mapping_requires_the_selected_native_target() {
        let expected = match std::env::consts::OS {
            "linux" => NativeOs::Linux,
            "macos" => NativeOs::Macos,
            "windows" => NativeOs::Windows,
            _ => return,
        };
        assert!(host_matches(expected));
    }
}
