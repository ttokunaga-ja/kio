//! A07: real Office conversion through the shipped Kio CLI.
//!
//! This deliberately has no converter seam.  It accepts only the exact
//! absolute renderer path which was checked by native preflight, runs the
//! release candidate with a fresh private HOME/XDG tree, and examines the
//! persisted normalized-unit objects rather than treating converter exit zero
//! as proof.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use serde::Deserialize;
use serde_json::Value;

use crate::{
    acceptance::{
        AcceptanceCase, AcceptanceError, AcceptanceLane, AcceptanceReceipt, AcceptanceRuntime,
        AcceptanceSubcase, ExpectedReceipt, MAX_BINARY_BYTES, MAX_FIXTURE_BYTES,
        ObservedConverterIdentity, current_evaluator_sha256_matches, runtime_from_scope,
        sha256_bytes, sha256_regular_file, validate_expected, write_receipt_create_only,
    },
    acceptance_environment::IsolatedChildEnvironment,
    runner::{BoundedProcessOptions, run_bounded_command},
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_OUTPUT_BYTES: usize = 128 * 1024;
const MAX_CONVERTER_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const FIXTURE_FILES: &[&str] = &[
    "document.docx",
    "presentation.pptx",
    "table.xlsx",
    "malformed.docx",
];
const DOCX_MARKER: &str = "KIO_A07_DOCX_PUBLIC_MARKER";
const PPTX_MARKER: &str = "KIO_A07_PPTX_PUBLIC_MARKER";
const XLSX_MARKER: &str = "KIO_A07_XLSX_PUBLIC_MARKER";
/// Ordered filename/NUL/bytes/NUL digest documented beside the public leaves.
const OFFICE_PUBLIC_FIXTURE_SHA256: &str =
    "79fdea154a9f61138349e560f2a8e3b8e14126b01f384e4951f2c85247216827";

#[derive(Debug, Clone)]
pub struct A07OfficeOptions {
    pub binary: PathBuf,
    /// The absolute, preflight-validated renderer executable.  The CLI child
    /// receives this exact value through KIO_OFFICE_CONVERTER.
    pub office_converter: PathBuf,
    /// The create-only preflight report that binds this renderer's resolved
    /// path and SHA-256.  This is not accepted as a receipt.
    pub preflight_report: PathBuf,
    pub fixture: PathBuf,
    pub expected: ExpectedReceipt,
    pub work_dir: PathBuf,
    pub receipt: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightReport {
    schema: String,
    office_converter: Option<ConverterRecord>,
    status: String,
    // The report has additional bounded diagnostic fields; retaining them as
    // Value prevents its diagnostic layout from becoming an A07 API.
    #[serde(default)]
    checks: Vec<Value>,
    #[serde(default)]
    failures: Vec<String>,
    #[serde(default)]
    runner_os: Value,
    #[serde(default)]
    host_os: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConverterRecord {
    requested: String,
    resolved: String,
    sha256: String,
}

/// Run A07 against real DOCX/PPTX conversion and direct XLSX table handling.
/// A mock-control environment, a changed converter, a malformed public input,
/// or absent persisted normalized units prevents receipt creation.
pub fn run_a07(options: &A07OfficeOptions) -> Result<AcceptanceReceipt, AcceptanceError> {
    validate_options(options)?;
    reject_test_controls()?;
    let (converter, converter_version) = validate_preflight_converter(options)?;
    let fixture_digest = fixture_digest(&options.fixture)?;
    if fixture_digest != OFFICE_PUBLIC_FIXTURE_SHA256
        || fixture_digest != options.expected.fixture.sha256
    {
        return Err(invalid(
            "Office public fixture digest differs from expected binding",
        ));
    }
    if options.work_dir.exists() || options.receipt.exists() {
        return Err(invalid(
            "A07 work directory and receipt must be create-only",
        ));
    }

    fs::create_dir(&options.work_dir).map_err(io)?;
    private_dir(&options.work_dir)?;
    let isolated = options.work_dir.join("private");
    let scope = isolated.join("scope");
    for path in [
        &isolated,
        &scope,
        &isolated.join("xdg-config"),
        &isolated.join("xdg-data"),
        &isolated.join("xdg-cache"),
        &isolated.join("tmp"),
    ] {
        fs::create_dir(path).map_err(io)?;
        private_dir(path)?;
    }
    for name in FIXTURE_FILES
        .iter()
        .copied()
        .filter(|name| *name != "malformed.docx")
    {
        fs::copy(options.fixture.join(name), scope.join(name)).map_err(io)?;
    }

    command(
        &options.binary,
        &isolated,
        None,
        &converter,
        &["--json", "init", path_arg(&scope)?],
    )?;
    command(
        &options.binary,
        &isolated,
        Some(&scope),
        &converter,
        &["--json", "index", "--offline"],
    )?;
    assert_units(&scope)?;
    assert_search(
        &options.binary,
        &isolated,
        &scope,
        &converter,
        DOCX_MARKER,
        "document.docx",
    )?;
    assert_search(
        &options.binary,
        &isolated,
        &scope,
        &converter,
        PPTX_MARKER,
        "presentation.pptx",
    )?;
    assert_search(
        &options.binary,
        &isolated,
        &scope,
        &converter,
        XLSX_MARKER,
        "table.xlsx",
    )?;

    // Conversion of malformed OOXML is required to fail.  This does not use a
    // fake renderer failure: the real selected program receives invalid bytes.
    fs::copy(
        options.fixture.join("malformed.docx"),
        scope.join("malformed.docx"),
    )
    .map_err(io)?;
    let malformed = command_status(
        &options.binary,
        &isolated,
        Some(&scope),
        &converter,
        &["--json", "index", "--offline"],
    )?;
    if malformed.status.success() {
        return Err(invalid(
            "malformed Office fixture did not cause product rejection",
        ));
    }
    if sha256_regular_file(&options.binary, MAX_BINARY_BYTES)?
        != options.expected.candidate.binary_sha256
        || sha256_regular_file(&converter, MAX_CONVERTER_BYTES)?
            != converter_digest_from_preflight(&options.preflight_report)?
    {
        return Err(invalid(
            "candidate binary or Office converter changed during A07",
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
        runtime: AcceptanceRuntime {
            tools: runtime_from_scope(&scope)?.tools,
            converter: Some(ObservedConverterIdentity {
                sha256: sha256_regular_file(&converter, MAX_CONVERTER_BYTES)?,
                version: converter_version,
            }),
        },
        passed: true,
    };
    write_receipt_create_only(&options.receipt, &receipt)?;
    Ok(receipt)
}

fn validate_options(options: &A07OfficeOptions) -> Result<(), AcceptanceError> {
    validate_expected(&options.expected)?;
    current_evaluator_sha256_matches(&options.expected.evaluator_sha256)?;
    let r = &options.expected.requirement;
    if r.case != AcceptanceCase::A07
        || r.lane != AcceptanceLane::OfficeReal
        || r.subcase != AcceptanceSubcase::Core
    {
        return Err(invalid("A07 runner requires A07/office_real/core"));
    }
    if sha256_regular_file(&options.binary, MAX_BINARY_BYTES)?
        != options.expected.candidate.binary_sha256
    {
        return Err(invalid("candidate binary differs from expected binding"));
    }
    if !options.fixture.is_dir() || !options.office_converter.is_absolute() {
        return Err(invalid(
            "A07 requires a fixture directory and absolute Office converter",
        ));
    }
    Ok(())
}

fn reject_test_controls() -> Result<(), AcceptanceError> {
    if std::env::vars_os().any(|(name, _)| {
        let name = name.to_string_lossy();
        name.starts_with("KIO_TEST_") || name.starts_with("KIO_MOCK_")
    }) {
        return Err(invalid(
            "A07 real Office acceptance refuses mock-control environment variables",
        ));
    }
    Ok(())
}

fn validate_preflight_converter(
    options: &A07OfficeOptions,
) -> Result<(PathBuf, String), AcceptanceError> {
    let report: PreflightReport = read_json(&options.preflight_report, 128 * 1024)?;
    if report.schema != "kio.v1.native-preflight/v1"
        || report.status != "passed"
        || !report.failures.is_empty()
        || report.checks.is_empty()
        || report.host_os.is_empty()
        || report.runner_os.is_null()
    {
        return Err(invalid("A07 requires a passing native preflight report"));
    }
    let record = report
        .office_converter
        .ok_or_else(|| invalid("preflight did not bind an Office converter"))?;
    let resolved = fs::canonicalize(&options.office_converter).map_err(io)?;
    if !resolved.is_file()
        || !is_executable(&resolved)?
        || !options.office_converter.is_absolute()
        || record.requested != display(&options.office_converter)?
        || record.resolved != display(&resolved)?
        || record.sha256 != sha256_regular_file(&resolved, MAX_CONVERTER_BYTES)?
    {
        return Err(invalid(
            "Office converter differs from the passing preflight binding",
        ));
    }
    let version = converter_version(&resolved)?;
    if version.trim().is_empty() {
        return Err(invalid("Office converter returned no version"));
    }
    Ok((resolved, version))
}

fn converter_digest_from_preflight(path: &Path) -> Result<String, AcceptanceError> {
    let report: PreflightReport = read_json(path, 128 * 1024)?;
    report
        .office_converter
        .map(|record| record.sha256)
        .ok_or_else(|| invalid("preflight did not bind an Office converter"))
}

fn fixture_digest(root: &Path) -> Result<String, AcceptanceError> {
    let mut bytes = Vec::new();
    for name in FIXTURE_FILES {
        let path = root.join(name);
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&bounded_file(&path, MAX_FIXTURE_BYTES)?);
        bytes.push(0);
    }
    Ok(sha256_bytes(&bytes))
}

fn assert_units(scope: &Path) -> Result<(), AcceptanceError> {
    let mut manifests = Vec::new();
    collect_manifests(&scope.join(".kio/objects/normalized_units"), &mut manifests)?;
    let mut found = BTreeMap::new();
    let mut markers = BTreeMap::new();
    for manifest in manifests {
        let value: Value = read_json(&manifest, 2 * 1024 * 1024)?;
        for unit in value
            .pointer("/units")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let key = unit.get("unit_key").and_then(Value::as_str).unwrap_or("");
            let status = unit.get("status").and_then(Value::as_str).unwrap_or("");
            if status == "done" {
                found.insert(key.to_owned(), true);
                let unit_path = manifest.parent().unwrap_or(scope).join(format!(
                    "{}.json",
                    unit.get("unit_ref").and_then(Value::as_str).unwrap_or("")
                ));
                let object: Value = read_json(&unit_path, 2 * 1024 * 1024)?;
                let markdown = object
                    .get("markdown")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("A07 normalized unit has no markdown"))?;
                validate_normalized_markdown(markdown)?;
                for marker in [DOCX_MARKER, PPTX_MARKER, XLSX_MARKER] {
                    if markdown.contains(marker) {
                        markers.insert(marker, true);
                    }
                }
            }
        }
    }
    for key in ["page:1", "slide:1"] {
        if !found.contains_key(key) {
            return Err(invalid(&format!(
                "A07 did not persist required Office unit {key}"
            )));
        }
    }
    if !found.keys().any(|key| key.starts_with("sheet:")) {
        return Err(invalid(
            "A07 did not persist an XLSX direct-table sheet unit",
        ));
    }
    for marker in [DOCX_MARKER, PPTX_MARKER, XLSX_MARKER] {
        if !markers.contains_key(marker) {
            return Err(invalid(
                "A07 normalized markdown lacks a public fixture marker",
            ));
        }
    }
    Ok(())
}

fn validate_normalized_markdown(markdown: &str) -> Result<(), AcceptanceError> {
    if markdown.is_empty()
        || markdown.contains('\r')
        || !markdown.ends_with('\n')
        || markdown.ends_with("\n\n")
        || markdown
            .lines()
            .any(|line| line.ends_with(' ') || line.ends_with('\t'))
    {
        return Err(invalid(
            "A07 normalized markdown violates the required line-normalization shape",
        ));
    }
    Ok(())
}

fn collect_manifests(root: &Path, out: &mut Vec<PathBuf>) -> Result<(), AcceptanceError> {
    let entries = fs::read_dir(root).map_err(io)?;
    for entry in entries {
        let path = entry.map_err(io)?.path();
        if path.is_dir() {
            collect_manifests(&path, out)?;
        } else if path.file_name().and_then(|n| n.to_str()) == Some("manifest.json") {
            out.push(path);
        }
    }
    Ok(())
}

fn assert_search(
    binary: &Path,
    isolated: &Path,
    scope: &Path,
    converter: &Path,
    query: &str,
    title: &str,
) -> Result<(), AcceptanceError> {
    let output = command(
        binary,
        isolated,
        Some(scope),
        converter,
        &["--json", "search", query, "--mode", "text", "--offline"],
    )?;
    let value: Value =
        serde_json::from_str(&output).map_err(|e| AcceptanceError::Json(e.to_string()))?;
    if !value
        .get("results")
        .and_then(Value::as_array)
        .is_some_and(|r| {
            r.iter()
                .any(|row| row.get("title").and_then(Value::as_str) == Some(title))
        })
    {
        return Err(invalid(&format!("A07 search did not return {title}")));
    }
    Ok(())
}

fn command(
    binary: &Path,
    isolated: &Path,
    scope: Option<&Path>,
    converter: &Path,
    args: &[&str],
) -> Result<String, AcceptanceError> {
    let output = command_status(binary, isolated, scope, converter, args)?;
    if !output.status.success() {
        return Err(AcceptanceError::Command(format!(
            "{} failed: {}",
            args.join(" "),
            output.stderr.trim()
        )));
    }
    Ok(output.stdout)
}

fn command_status(
    binary: &Path,
    isolated: &Path,
    scope: Option<&Path>,
    converter: &Path,
    args: &[&str],
) -> Result<crate::runner::BoundedProcessOutput, AcceptanceError> {
    let mut cmd = Command::new(binary);
    IsolatedChildEnvironment::new(
        isolated,
        isolated.join("xdg-config"),
        isolated.join("xdg-data"),
        isolated.join("xdg-cache"),
        isolated.join("tmp"),
    )
    .apply(&mut cmd)?;
    cmd.env("KIO_OFFICE_CONVERTER", converter).args(args);
    if let Some(scope) = scope {
        cmd.current_dir(scope);
    }
    run_bounded_command(
        &mut cmd,
        BoundedProcessOptions {
            timeout: COMMAND_TIMEOUT,
            max_stdout_bytes: MAX_OUTPUT_BYTES,
            max_stderr_bytes: MAX_OUTPUT_BYTES,
        },
        None,
    )
    .map_err(|error| AcceptanceError::Command(format!("bounded A07 command failed: {error}")))
}

fn converter_version(path: &Path) -> Result<String, AcceptanceError> {
    let mut cmd = Command::new(path);
    cmd.arg("--version");
    let out = run_bounded_command(
        &mut cmd,
        BoundedProcessOptions {
            timeout: Duration::from_secs(15),
            max_stdout_bytes: 16 * 1024,
            max_stderr_bytes: 16 * 1024,
        },
        None,
    )
    .map_err(|error| {
        AcceptanceError::Command(format!("Office converter version failed: {error}"))
    })?;
    if !out.status.success() {
        return Err(invalid("Office converter version command failed"));
    }
    Ok(out.stdout)
}

fn read_json<T: for<'a> Deserialize<'a>>(path: &Path, maximum: u64) -> Result<T, AcceptanceError> {
    serde_json::from_slice(&bounded_file(path, maximum)?)
        .map_err(|_| invalid("A07 JSON input is malformed"))
}
fn bounded_file(path: &Path, maximum: u64) -> Result<Vec<u8>, AcceptanceError> {
    let metadata = fs::symlink_metadata(path).map_err(io)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > maximum {
        return Err(invalid("A07 input must be a bounded regular file"));
    }
    fs::read(path).map_err(io)
}
fn path_arg(path: &Path) -> Result<&str, AcceptanceError> {
    path.to_str()
        .ok_or_else(|| invalid("A07 path is not UTF-8"))
}
fn display(path: &Path) -> Result<String, AcceptanceError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| invalid("A07 path is not UTF-8"))
}
#[cfg(unix)]
fn is_executable(path: &Path) -> Result<bool, AcceptanceError> {
    use std::os::unix::fs::PermissionsExt;
    Ok(fs::metadata(path).map_err(io)?.permissions().mode() & 0o111 != 0)
}
#[cfg(not(unix))]
fn is_executable(_path: &Path) -> Result<bool, AcceptanceError> {
    Ok(true)
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
fn invalid(message: &str) -> AcceptanceError {
    AcceptanceError::Invalid(message.into())
}
fn io(error: std::io::Error) -> AcceptanceError {
    AcceptanceError::Io(error.to_string())
}
