//! A04 native policy acceptance for a supplied release binary.
//!
//! It has no mock adapter, credential, or provider control. It proves only
//! offline policy and device-grant state transitions.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::Value;

use crate::{
    acceptance::{
        AcceptanceCase, AcceptanceError, AcceptanceLane, AcceptanceReceipt, AcceptanceSubcase,
        ExpectedReceipt, MAX_BINARY_BYTES, MAX_FIXTURE_BYTES, current_evaluator_sha256_matches,
        empty_runtime, sha256_bytes, sha256_regular_file, validate_expected,
        write_receipt_create_only,
    },
    acceptance_environment::IsolatedChildEnvironment,
    runner::{BoundedProcessOptions, run_bounded_command},
};

const TOOL_ID: &str = "mistral_ocr_markdownize";
const MANAGEMENT_AUTHORITY_ERROR: &str = "KIO-E-MANAGEMENT-AUTHORITY-001";
const MAX_CLONE_FILES: usize = 4096;
const MAX_CLONE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct PolicyOptions {
    pub binary: PathBuf,
    pub fixture: PathBuf,
    pub expected: ExpectedReceipt,
    pub work_dir: PathBuf,
    pub receipt: PathBuf,
}

/// Run A04/native-contract/core against exactly the bound binary and fixture.
/// The receipt is created only after every assertion succeeds.
pub fn run_a04(options: &PolicyOptions) -> Result<AcceptanceReceipt, AcceptanceError> {
    let requirement = &options.expected.requirement;
    if requirement.case != AcceptanceCase::A04
        || requirement.lane != AcceptanceLane::NativeContract
        || requirement.subcase != AcceptanceSubcase::Core
    {
        return Err(invalid("A04 requires native_contract/core"));
    }
    validate_expected(&options.expected)?;
    current_evaluator_sha256_matches(&options.expected.evaluator_sha256)?;
    verify_inputs(options)?;
    if options.work_dir.exists() || options.receipt.exists() {
        return Err(invalid(
            "A04 work directory and receipt must be create-only",
        ));
    }

    fs::create_dir_all(&options.work_dir).map_err(io)?;
    let work = options.work_dir.canonicalize().map_err(io)?;
    private_dir(&work)?;
    let device = work.join("isolated-0700-home");
    for leaf in ["home", "xdg-config", "xdg-data", "xdg-cache", "tmp"] {
        let path = device.join(leaf);
        fs::create_dir_all(&path).map_err(io)?;
        private_dir(&path)?;
    }
    private_dir(&device)?;
    let project = device.join("project");
    let child = project.join("child");
    fs::create_dir_all(&child).map_err(io)?;
    private_dir(&project)?;
    private_dir(&child)?;
    fs::write(
        child.join("policy-fixture.md"),
        bounded_file(&options.fixture)?,
    )
    .map_err(io)?;
    let sentinel = work.join("outside-sentinel");
    fs::write(&sentinel, b"outside-policy-sentinel\n").map_err(io)?;
    let sentinel_hash = sha256_regular_file(&sentinel, MAX_FIXTURE_BYTES)?;

    // The parent is the sole explicit root. Indexing it must enroll the child.
    command_ok(&options.binary, &device, &project, &["--json", "init"])?;
    command_json(
        &options.binary,
        &device,
        &project,
        &["--json", "index", "--offline"],
    )?;
    if !child.join(".kio/management.json").is_file() {
        return Err(failure(
            "parent index did not automatically enroll child scope",
        ));
    }

    assert_visible(
        &command_json(&options.binary, &device, &child, &search_args(false))?,
        "baseline current",
    )?;
    assert_visible(
        &command_json(&options.binary, &device, &child, &search_args(true))?,
        "baseline historical",
    )?;

    // Apply a parent rule after indexing. No reindex is allowed between these
    // assertions: current policy must filter both projections live.
    fs::write(project.join(".kioignore"), "child/policy-fixture.md\n").map_err(io)?;
    assert_hidden(
        &command_json(&options.binary, &device, &child, &search_args(false))?,
        "current after parent ignore",
    )?;
    assert_hidden(
        &command_json(&options.binary, &device, &child, &search_args(true))?,
        "historical after parent ignore",
    )?;
    fs::remove_file(project.join(".kioignore")).map_err(io)?;
    assert_visible(
        &command_json(&options.binary, &device, &child, &search_args(false))?,
        "current after ignore removal",
    )?;
    assert_visible(
        &command_json(&options.binary, &device, &child, &search_args(true))?,
        "historical after ignore removal",
    )?;

    let initial = adapter_status(&options.binary, &device, &child)?;
    require_no_active(&initial, "initial adapter status")?;
    let approved = command_json(
        &options.binary,
        &device,
        &child,
        &["--json", "adapter", "approve", TOOL_ID, "--yes"],
    )?;
    require_status(&approved, "approved", "network approve")?;
    require_permitted(
        &adapter_status(&options.binary, &device, &child)?,
        "network approval status",
    )?;

    let secret_approved = command_json(
        &options.binary,
        &device,
        &child,
        &[
            "--json",
            "adapter",
            "approve",
            TOOL_ID,
            "--yes",
            "--send-secrets",
        ],
    )?;
    require_status(&secret_approved, "approved", "secret approval")?;
    require_paired_grants(&adapter_status(&options.binary, &device, &child)?)?;
    // This lane holds no secret material and makes no network/provider call;
    // paired grant metadata is the maximum secret assertion it can establish.

    // Copy all portable scope state while active. The clone and a renamed
    // original must fail their explicit approval attempt at the exact native
    // authority boundary; errors are never swallowed as a pass.
    let clone = device.join("clone");
    fs::create_dir_all(&clone).map_err(io)?;
    fs::copy(
        child.join("policy-fixture.md"),
        clone.join("policy-fixture.md"),
    )
    .map_err(io)?;
    copy_tree(&child.join(".kio"), &clone.join(".kio"))?;
    command_error_code(
        &options.binary,
        &device,
        &clone,
        &["--json", "adapter", "approve", TOOL_ID, "--yes"],
        MANAGEMENT_AUTHORITY_ERROR,
        "cloned scope approval",
    )?;
    let moved = device.join("moved-child");
    fs::rename(&child, &moved).map_err(io)?;
    command_error_code(
        &options.binary,
        &device,
        &moved,
        &["--json", "adapter", "approve", TOOL_ID, "--yes"],
        MANAGEMENT_AUTHORITY_ERROR,
        "moved scope approval",
    )?;
    fs::rename(&moved, &child).map_err(io)?;

    let revoked = command_json(
        &options.binary,
        &device,
        &child,
        &["--json", "adapter", "revoke", TOOL_ID],
    )?;
    require_status(&revoked, "revoked", "adapter revoke")?;
    require_no_active(
        &adapter_status(&options.binary, &device, &child)?,
        "revoked adapter status",
    )?;
    if sha256_regular_file(&sentinel, MAX_FIXTURE_BYTES)? != sentinel_hash {
        return Err(failure("outside sentinel changed during policy acceptance"));
    }
    verify_inputs(options)?;

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

fn verify_inputs(options: &PolicyOptions) -> Result<(), AcceptanceError> {
    if sha256_regular_file(&options.binary, MAX_BINARY_BYTES)?
        != options.expected.candidate.binary_sha256
    {
        return Err(invalid("binary hash differs from candidate binding"));
    }
    if sha256_bytes(&bounded_file(&options.fixture)?) != options.expected.fixture.sha256 {
        return Err(invalid("fixture hash differs from expected binding"));
    }
    Ok(())
}

fn search_args(history: bool) -> Vec<&'static str> {
    let mut args = vec![
        "--json",
        "search",
        "policy",
        "--scope",
        ".",
        "--mode",
        "text",
        "--offline",
    ];
    if history {
        args.push("--all-history");
    }
    args
}

fn adapter_status(binary: &Path, device: &Path, cwd: &Path) -> Result<Value, AcceptanceError> {
    command_json(
        binary,
        device,
        cwd,
        &["--json", "adapter", "status", TOOL_ID],
    )
}

struct CommandResult {
    stdout: String,
    stderr: String,
    success: bool,
    code: Option<i32>,
}

fn command_json(
    binary: &Path,
    device: &Path,
    cwd: &Path,
    args: &[&str],
) -> Result<Value, AcceptanceError> {
    let output = command_ok(binary, device, cwd, args)?;
    serde_json::from_str(&output.stdout).map_err(|error| AcceptanceError::Json(error.to_string()))
}

fn command_ok(
    binary: &Path,
    device: &Path,
    cwd: &Path,
    args: &[&str],
) -> Result<CommandResult, AcceptanceError> {
    let output = command(binary, device, cwd, args)?;
    if output.success {
        Ok(output)
    } else {
        Err(failure(format!(
            "{} failed with {:?}: {}",
            args.join(" "),
            output.code,
            output.stderr.trim()
        )))
    }
}

fn command_error_code(
    binary: &Path,
    device: &Path,
    cwd: &Path,
    args: &[&str],
    expected: &str,
    context: &str,
) -> Result<(), AcceptanceError> {
    let output = command(binary, device, cwd, args)?;
    if output.success {
        return Err(failure(format!("{context} unexpectedly succeeded")));
    }
    let error: Value = serde_json::from_str(&output.stderr)
        .map_err(|parse| failure(format!("{context} did not emit JSON error: {parse}")))?;
    if error.get("error_code").and_then(Value::as_str) != Some(expected) {
        return Err(failure(format!(
            "{context} returned {:?}, expected {expected}",
            error.get("error_code")
        )));
    }
    Ok(())
}

fn command(
    binary: &Path,
    device: &Path,
    cwd: &Path,
    args: &[&str],
) -> Result<CommandResult, AcceptanceError> {
    let mut command = Command::new(binary);
    IsolatedChildEnvironment::new(
        device.join("home"),
        device.join("xdg-config"),
        device.join("xdg-data"),
        device.join("xdg-cache"),
        device.join("tmp"),
    )
    .apply(&mut command)?;
    command.current_dir(cwd).args(args);
    let output = run_bounded_command(&mut command, BoundedProcessOptions::default(), None)
        .map_err(|error| AcceptanceError::Command(error.to_string()))?;
    Ok(CommandResult {
        stdout: output.stdout,
        stderr: output.stderr,
        success: output.status.success(),
        code: output.status.code(),
    })
}

fn fixture_present(value: &Value) -> bool {
    value
        .get("results")
        .and_then(Value::as_array)
        .is_some_and(|rows| {
            rows.iter().any(|row| {
                row.pointer("/evidence_pointer/path_at_commit")
                    .and_then(Value::as_str)
                    == Some("policy-fixture.md")
            })
        })
}

fn assert_visible(value: &Value, context: &str) -> Result<(), AcceptanceError> {
    if fixture_present(value) {
        Ok(())
    } else {
        Err(failure(format!("{context} omitted policy fixture")))
    }
}

fn assert_hidden(value: &Value, context: &str) -> Result<(), AcceptanceError> {
    if fixture_present(value) {
        Err(failure(format!("{context} exposed ignored fixture")))
    } else {
        Ok(())
    }
}

fn require_status(value: &Value, expected: &str, context: &str) -> Result<(), AcceptanceError> {
    if value.get("status").and_then(Value::as_str) == Some(expected) {
        Ok(())
    } else {
        Err(failure(format!(
            "{context} did not return status={expected}"
        )))
    }
}

fn require_no_active(value: &Value, context: &str) -> Result<(), AcceptanceError> {
    let count = value
        .get("grants")
        .and_then(Value::as_array)
        .map_or(0, |rows| {
            rows.iter()
                .filter(|row| row.get("state").and_then(Value::as_str) == Some("active"))
                .count()
        });
    if count == 0 {
        Ok(())
    } else {
        Err(failure(format!("{context} exposed {count} active grants")))
    }
}

fn require_permitted(value: &Value, context: &str) -> Result<(), AcceptanceError> {
    let permitted = value
        .get("effective")
        .and_then(Value::as_array)
        .is_some_and(|rows| {
            rows.iter()
                .any(|row| row.get("permitted").and_then(Value::as_bool) == Some(true))
        });
    if permitted {
        Ok(())
    } else {
        Err(failure(format!("{context} has no permitted grant")))
    }
}

fn require_paired_grants(value: &Value) -> Result<(), AcceptanceError> {
    let grants = value
        .get("grants")
        .and_then(Value::as_array)
        .ok_or_else(|| failure("adapter status has no grants array"))?;
    // A second approval with --send-secrets terminally revokes the preceding
    // network-only record.  Match the active pair by approval_id instead of
    // selecting the first historical network record in status output.
    let paired = grants
        .iter()
        .filter(|network| {
            network.get("state").and_then(Value::as_str) == Some("active")
                && network
                    .pointer("/binding/operation")
                    .and_then(Value::as_str)
                    == Some("network")
        })
        .any(|network| {
            let approval_id = network.get("approval_id");
            grants.iter().any(|secret| {
                secret.get("state").and_then(Value::as_str) == Some("active")
                    && secret.pointer("/binding/operation").and_then(Value::as_str)
                        == Some("send_secrets")
                    && secret.get("approval_id") == approval_id
            })
        });
    if paired {
        Ok(())
    } else {
        Err(failure(
            "status did not prove paired active network and secret grants",
        ))
    }
}

fn bounded_file(path: &Path) -> Result<Vec<u8>, AcceptanceError> {
    let metadata = fs::metadata(path).map_err(io)?;
    if !metadata.is_file() || metadata.len() > MAX_FIXTURE_BYTES {
        return Err(invalid("A04 fixture is not a bounded regular file"));
    }
    fs::read(path).map_err(io)
}

fn private_dir(path: &Path) -> Result<(), AcceptanceError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(io)?;
    }
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), AcceptanceError> {
    fn copy(
        source: &Path,
        destination: &Path,
        files: &mut usize,
        bytes: &mut u64,
    ) -> Result<(), AcceptanceError> {
        let metadata = fs::symlink_metadata(source).map_err(io)?;
        if metadata.file_type().is_symlink() {
            return Err(invalid("A04 clone contains a symlink"));
        }
        if metadata.is_dir() {
            fs::create_dir_all(destination).map_err(io)?;
            for entry in fs::read_dir(source).map_err(io)? {
                let entry = entry.map_err(io)?;
                copy(
                    &entry.path(),
                    &destination.join(entry.file_name()),
                    files,
                    bytes,
                )?;
            }
        } else if metadata.is_file() {
            *files += 1;
            *bytes = bytes.saturating_add(metadata.len());
            if *files > MAX_CLONE_FILES || *bytes > MAX_CLONE_BYTES {
                return Err(invalid("A04 clone exceeds bound"));
            }
            fs::copy(source, destination).map_err(io)?;
        } else {
            return Err(invalid("A04 clone contains non-regular entry"));
        }
        Ok(())
    }
    let mut files = 0;
    let mut bytes = 0;
    copy(source, destination, &mut files, &mut bytes)
}

fn invalid(message: impl Into<String>) -> AcceptanceError {
    AcceptanceError::Invalid(message.into())
}
fn failure(message: impl Into<String>) -> AcceptanceError {
    AcceptanceError::Command(message.into())
}
fn io(error: std::io::Error) -> AcceptanceError {
    AcceptanceError::Io(error.to_string())
}
