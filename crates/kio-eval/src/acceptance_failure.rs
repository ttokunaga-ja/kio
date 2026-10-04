//! A08 instrumented mock-failure acceptance driver.
//!
//! This is deliberately a debug-contract test, not provider evidence.  Every
//! mutable failure exercise is performed by the separately bound debug binary;
//! the release candidate only initializes and checks durable state.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use rusqlite::Connection;
use serde_json::{Value, json};

use crate::{
    acceptance::{
        AcceptanceCase, AcceptanceError, AcceptanceLane, AcceptanceReceipt, AcceptanceSubcase,
        ExpectedReceipt, MAX_BINARY_BYTES, MAX_FIXTURE_BYTES, current_evaluator_sha256_matches,
        empty_runtime, sha256_bytes, sha256_regular_file, validate_expected,
        write_receipt_create_only,
    },
    acceptance_environment::IsolatedChildEnvironment,
    runner::{BoundedProcessOptions, BoundedProcessOutput, run_bounded_command},
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const FIXED_NOW: &str = "2026-09-08T00:00:00Z";

/// Inputs for A08's instrumented, native mock-failure contract. `binary` is
/// the release candidate and `contract_binary` is the separately hashed debug
/// executable that alone receives `KIO_TEST_*` controls.
#[derive(Debug, Clone)]
pub struct FailureOptions {
    pub binary: PathBuf,
    pub contract_binary: PathBuf,
    pub fixture: PathBuf,
    pub expected: ExpectedReceipt,
    pub work_dir: PathBuf,
    pub receipt: PathBuf,
}

/// Run the four bounded A08/mock-failure scenarios. A receipt is written only
/// after each scenario has observed persisted CLI/ledger evidence.
pub fn run_a08(options: &FailureOptions) -> Result<AcceptanceReceipt, AcceptanceError> {
    validate_options(options)?;
    if options.work_dir.exists() || options.receipt.exists() {
        return Err(invalid(
            "A08 work directory and receipt must be create-only",
        ));
    }
    fs::create_dir(&options.work_dir).map_err(io)?;
    private_dir(&options.work_dir)?;
    let root = options.work_dir.canonicalize().map_err(io)?;

    missing_credential_pre_admission(&root, options)?;
    auth_failure(&root, options)?;
    rate_limit(&root, options)?;
    budget_denial(&root, options)?;
    unknown_accepted_request(&root, options)?;
    unknown_sync_resend(&root, options)?;
    verify_bindings(options)?;

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

fn validate_options(options: &FailureOptions) -> Result<(), AcceptanceError> {
    validate_expected(&options.expected)?;
    current_evaluator_sha256_matches(&options.expected.evaluator_sha256)?;
    let requirement = &options.expected.requirement;
    if requirement.case != AcceptanceCase::A08
        || requirement.lane != AcceptanceLane::NativeContract
        || requirement.subcase != AcceptanceSubcase::MockFailure
    {
        return Err(invalid("A08 mock-failure requirement is invalid"));
    }
    verify_bindings(options)
}

fn verify_bindings(options: &FailureOptions) -> Result<(), AcceptanceError> {
    if sha256_regular_file(&options.binary, MAX_BINARY_BYTES)?
        != options.expected.candidate.binary_sha256
    {
        return Err(invalid(
            "release binary hash differs from candidate binding",
        ));
    }
    let contract = options
        .expected
        .contract_binary_sha256
        .as_deref()
        .ok_or_else(|| {
            invalid("A08 mock-failure receipt lacks the contract-binary hash binding")
        })?;
    if sha256_regular_file(&options.contract_binary, MAX_BINARY_BYTES)? != contract {
        return Err(invalid(
            "contract binary hash differs from expected binding",
        ));
    }
    let fixture = bounded_regular_bytes(&options.fixture, MAX_FIXTURE_BYTES)?;
    if sha256_bytes(&fixture) != options.expected.fixture.sha256 {
        return Err(invalid("A08 fixture differs from expected binding"));
    }
    Ok(())
}

/// This is intentionally distinct from the AuthError scenario below.  The
/// release binary has no credential and therefore must stop before it creates
/// an intent or a charge; no mock adapter or HTTP fixture participates.
fn missing_credential_pre_admission(
    root: &Path,
    options: &FailureOptions,
) -> Result<(), AcceptanceError> {
    let device = Device::new(root, "missing-credential")?;
    // The debug binary establishes the normal approved/enqueued fixture. The
    // actual pre-admission command is release-only and inherits no test control,
    // synthetic key, or ambient credential from `child`'s env_clear boundary.
    setup_online_task(&device, options, None)?;
    let before = ledger_counts(&device)?;
    let output = release(options, &device, &["batch", "resume"])?;
    require_exit(&output, 5, "missing credential pre-admission")?;
    require(
        ledger_counts(&device)? == before,
        "missing credential created a ledger request or charge before admission",
    )?;
    let status = release_json(options, &device, &["status"])?;
    let task = online_task(&status)?;
    require(
        task["status"] == "paused" && task["hold_reason"] == "auth",
        "missing credential did not produce the pre-admission auth hold",
    )
}

fn auth_failure(root: &Path, options: &FailureOptions) -> Result<(), AcceptanceError> {
    let device = Device::new(root, "auth")?;
    setup_online_task(&device, options, None)?;
    let before = auth_ledger_observation(&device)?;
    let output = debug(
        options,
        &device,
        &["batch", "resume"],
        &[("KIO_TEST_MISTRAL_OCR", "auth_error")],
    )?;
    require_exit(&output, 5, "auth failure")?;
    let status = release_json(options, &device, &["status"])?;
    let task = online_task(&status)?;
    require(
        task["status"] == "paused" && task["hold_reason"] == "auth",
        "auth failure did not pause the task",
    )?;
    require(
        task["attempts"] == 0 && task["next_retry_at"].is_null(),
        "auth failure consumed a retry",
    )?;
    let settled = auth_ledger_observation(&device)?;
    assert_known_sync_rejection(&before, &settled, "auth")?;
    let retry = debug(
        options,
        &device,
        &["batch", "retry"],
        &[("KIO_TEST_MISTRAL_OCR", "mock")],
    )?;
    let retry_json = stdout_json(&retry, "auth retry")?;
    require(
        retry_json["tasks_executed"] == 0,
        "auth-paused task was retried by batch retry",
    )?;
    require(
        auth_ledger_observation(&device)? == settled,
        "batch retry changed a settled auth request or charge",
    )
}

fn rate_limit(root: &Path, options: &FailureOptions) -> Result<(), AcceptanceError> {
    let device = Device::new(root, "rate-limit")?;
    setup_online_task(&device, options, None)?;
    let rate = debug(
        options,
        &device,
        &["batch", "resume"],
        &[
            ("KIO_TEST_MISTRAL_OCR", "rate_limit_after"),
            ("KIO_FIXED_NOW", FIXED_NOW),
        ],
    )?;
    require_exit(&rate, 3, "rate limit")?;
    let status = release_json(options, &device, &["status"])?;
    let task = online_task(&status)?;
    require(
        task["status"] == "pending" && task["attempts"] == 0,
        "rate limit was not classified as retryable",
    )?;
    require(
        task["next_retry_at"] == "2026-09-08T00:00:30Z",
        "provider Retry-After was not retained",
    )?;
    assert_known_sync_rejection(
        &AuthLedgerObservation {
            requests: Vec::new(),
            charges: Vec::new(),
        },
        &auth_ledger_observation(&device)?,
        "rate-limit",
    )?;
    let early = debug(
        options,
        &device,
        &["batch", "resume"],
        &[
            ("KIO_TEST_MISTRAL_OCR", "mock"),
            ("KIO_FIXED_NOW", "2026-09-08T00:00:10Z"),
        ],
    )?;
    require(
        stdout_json(&early, "early rate retry")?["tasks_executed"] == 0,
        "rate-limited task resent before Retry-After",
    )?;
    let late = debug(
        options,
        &device,
        &["batch", "resume"],
        &[
            ("KIO_TEST_MISTRAL_OCR", "mock"),
            ("KIO_FIXED_NOW", "2026-09-08T00:00:31Z"),
        ],
    )?;
    require(
        stdout_json(&late, "late rate retry")?["tasks_executed"]
            .as_u64()
            .unwrap_or(0)
            >= 1,
        "rate-limited task did not recover after Retry-After",
    )
}

fn budget_denial(root: &Path, options: &FailureOptions) -> Result<(), AcceptanceError> {
    let device = Device::new(root, "budget")?;
    // Indexing with the cap already installed consumes this scenario: it
    // pauses the newly enqueued task before `batch resume` runs, so resume
    // truthfully reports no work and exits zero. Install the device cap only
    // after setup has established a pending online task. This makes the
    // bounded command itself prove the admission denial.
    setup_online_task(&device, options, None)?;
    write_device_config(&device, "[budget]\nmonthly_usd_cap = 0.0\n")?;
    let capture = device.private.join("sent-media.jsonl");
    let before = ledger_counts(&device)?;
    let output = debug(
        options,
        &device,
        &["batch", "resume"],
        &[
            ("KIO_TEST_MISTRAL_OCR", "mock"),
            (
                "KIO_TEST_CAPTURE_SENT_MEDIA",
                capture
                    .to_str()
                    .ok_or_else(|| invalid("non-UTF-8 capture path"))?,
            ),
        ],
    )?;
    require_exit(&output, 6, "budget denial")?;
    require(
        stdout_json(&output, "budget denial")?["tasks_paused"]
            .as_u64()
            .unwrap_or(0)
            >= 1,
        "budget denial did not disclose the task it paused",
    )?;
    let status = release_json(options, &device, &["status"])?;
    let task = online_task(&status)?;
    require(
        task["status"] == "paused" && task["hold_reason"] == "budget",
        "budget denial did not occur before send",
    )?;
    require(
        !capture.exists() || fs::metadata(&capture).map_err(io)?.len() == 0,
        "budget-denied task reached the adapter",
    )?;
    require(
        ledger_counts(&device)? == before,
        "budget-denied task created a ledger request or charge",
    )
}

fn unknown_accepted_request(root: &Path, options: &FailureOptions) -> Result<(), AcceptanceError> {
    let device = Device::new(root, "unknown-accepted")?;
    setup_batch_task(&device, options)?;
    let capture = device.private.join("batch-calls.jsonl");
    let failed_script = json!({"fail_phase":"create_job", "capture_path": capture});
    let failed = debug(
        options,
        &device,
        &["batch", "resume"],
        &[
            ("KIO_TEST_MISTRAL_OCR", "mock"),
            ("KIO_TEST_MISTRAL_BATCH", failed_script.to_string().as_str()),
        ],
    )?;
    require_exit(&failed, 3, "unknown accepted request")?;
    let row = batch_row(&device)?;
    let token = row
        .intent_token
        .ok_or_else(|| invalid("unknown accepted request lost its intent token"))?;
    require(
        row.batch_job_id.is_none() && row.job_create_started_at.is_some(),
        "create-job uncertainty was not retained in the ledger",
    )?;
    let scope_id = scope_id(&device)?;
    let input_hash = row.input_hash;
    let tool_profile_hash = row.tool_profile_hash;
    let recovered_script = json!({
        "capture_path": capture.clone(),
        "status_sequence": ["SUCCESS"],
        "jobs_listing": [{"job_id":"batch-mock-job-1", "status":"QUEUED", "metadata": {
            "intent_token": token, "scope_id": scope_id, "adapter_kind":"markdownize",
            "input_hash": input_hash, "tool_profile_hash": tool_profile_hash
        }}],
        "uploads_listing": [{"upload_id":"file-mock-upload-1", "filename": format!("kio-{token}.jsonl")}],
        "output": [{"custom_id": input_hash, "response": {"status_code":200, "body": {"model":"mistral-ocr-2505", "pages":[{"index":0,"markdown":"recovered\n"}]}}}]
    });
    let reconciled = debug(
        options,
        &device,
        &["ledger", "reconcile"],
        &[
            ("KIO_TEST_MISTRAL_OCR", "mock"),
            (
                "KIO_TEST_MISTRAL_BATCH",
                recovered_script.to_string().as_str(),
            ),
        ],
    )?;
    let report = stdout_json(&reconciled, "unknown accepted reconcile")?;
    require(
        report["batch_found"] == 1,
        "reconcile did not retain and recover the accepted request",
    )?;
    let recovered = batch_row(&device)?;
    require(
        recovered.intent_token.as_deref() == Some(token.as_str())
            && recovered.batch_job_id.as_deref() == Some("batch-mock-job-1"),
        "reconcile did not self-describe the accepted request",
    )?;
    let calls = fs::read_to_string(&capture).map_err(io)?;
    require(
        calls
            .lines()
            .filter(|line| line.contains("create_job"))
            .count()
            == 1,
        "unknown accepted request was automatically resent",
    )?;
    release_json(options, &device, &["status"])?;
    Ok(())
}

/// A debug-only transport fault models a sync request whose provider outcome
/// cannot be recovered. Ordinary commands must not resend it; the public
/// authorization command only moves the one-shot ledger fence and queues the
/// existing task. A later mock-backed resume is the sole fresh submission.
fn unknown_sync_resend(root: &Path, options: &FailureOptions) -> Result<(), AcceptanceError> {
    let device = Device::new(root, "unknown-sync-resend")?;
    setup_online_task(&device, options, None)?;
    let capture = device.private.join("sync-media-capture.json");
    let failed = debug(
        options,
        &device,
        &["batch", "resume"],
        &[("KIO_TEST_MISTRAL_OCR", "network_error")],
    )?;
    require_exit(&failed, 4, "unknown sync request")?;
    let unknown_status = release_json(options, &device, &["status"])?;
    let unknown_task = online_task(&unknown_status)?;
    require(
        unknown_task["status"] == "failed"
            && unknown_task["fallback_reason"] == "result_unknown"
            && unknown_task["next_retry_at"].is_null(),
        "unknown sync request was not made non-retryable pending explicit resend authorization",
    )?;
    let settled = auth_ledger_observation(&device)?;
    require(
        settled.requests.len() == 1 && settled.charges.len() == 1,
        "unknown sync request did not produce one fenced request and one conservative charge",
    )?;
    let old_request = &settled.requests[0];
    let old_charge = &settled.charges[0];
    require(
        old_request.state == 3
            && old_request.request_kind == "sync"
            && old_request.intent_token.is_none()
            && old_request.error.as_deref() == Some("unknown_settled")
            && old_charge.outcome == "unknown_settled"
            && old_charge.estimated == 1
            && old_charge.usd.to_bits() == old_request.estimated_usd.to_bits(),
        "unknown sync request was not conservatively settled and fenced",
    )?;
    let selector = format!(
        "{}/markdownize/{}/{}",
        scope_id(&device)?,
        old_request.input_hash,
        old_request.tool_profile_hash
    );

    for (label, args) in [
        ("ordinary batch retry", vec!["batch", "retry"]),
        ("ordinary batch resume", vec!["batch", "resume"]),
        ("ordinary index", vec!["index", "--yes"]),
    ] {
        let _ordinary = debug(
            options,
            &device,
            &args,
            &[
                ("KIO_TEST_MISTRAL_OCR", "mock"),
                (
                    "KIO_TEST_CAPTURE_SENT_MEDIA",
                    capture.to_string_lossy().as_ref(),
                ),
            ],
        )?;
        require(
            auth_ledger_observation(&device)? == settled,
            &format!("{label} changed the unknown-result ledger fence or conservative charge"),
        )?;
    }
    require(
        !capture.exists(),
        "ordinary commands reached the debug mock before unknown-result authorization",
    )?;

    let authorized = debug(
        options,
        &device,
        &["batch", "retry", "--resend-unknown", &selector, "--yes"],
        &[
            ("KIO_TEST_MISTRAL_OCR", "mock"),
            (
                "KIO_TEST_CAPTURE_SENT_MEDIA",
                capture.to_string_lossy().as_ref(),
            ),
        ],
    )?;
    let authorization = stdout_json(&authorized, "unknown sync resend authorization")?;
    require(
        authorized.status.success()
            && authorization["status"] == "resend_authorized"
            && authorization["authorization_created"] == true
            && authorization["tasks_queued"] == 1,
        "explicit resend authorization did not queue exactly one task",
    )?;
    require(
        !capture.exists(),
        "resend authorization contacted the debug mock instead of only changing durable state",
    )?;
    let authorized_ledger = auth_ledger_observation(&device)?;
    require(
        authorized_ledger.charges == settled.charges
            && authorized_ledger.requests.len() == 1
            && authorized_ledger.requests[0].submission_seq == old_request.submission_seq
            && authorized_ledger.requests[0].intent_token.is_none()
            && authorized_ledger.requests[0].error.as_deref() == Some("unknown_resend_authorized"),
        "resend authorization changed accounting or did not retain the one-shot marker",
    )?;

    let fresh = debug(
        options,
        &device,
        &["batch", "resume", "--realtime"],
        &[
            ("KIO_TEST_MISTRAL_OCR", "mock"),
            (
                "KIO_TEST_CAPTURE_SENT_MEDIA",
                capture.to_string_lossy().as_ref(),
            ),
        ],
    )?;
    require_exit(&fresh, 0, "authorized fresh sync request")?;
    let completed = auth_ledger_observation(&device)?;
    require(
        completed.requests.len() == 1
            && completed.requests[0].state == 2
            && completed.requests[0].intent_token.is_none()
            && completed.requests[0].error.is_none()
            && completed.requests[0].submission_seq > old_request.submission_seq
            && completed.charges.len() == 2
            && &completed.charges[0] == old_charge
            && completed.charges[1].submission_seq > old_charge.submission_seq,
        "authorized fresh attempt did not retain the old charge and record a new submission",
    )?;
    require(
        capture.exists(),
        "authorized fresh attempt did not reach the debug mock",
    )?;

    let repeated = debug(
        options,
        &device,
        &["batch", "retry", "--resend-unknown", &selector, "--yes"],
        &[("KIO_TEST_MISTRAL_OCR", "mock")],
    )?;
    require(
        !repeated.status.success() && auth_ledger_observation(&device)? == completed,
        "a consumed resend authorization was accepted or changed ledger accounting",
    )
}

struct Device {
    private: PathBuf,
    scope: PathBuf,
}
impl Device {
    fn new(root: &Path, name: &str) -> Result<Self, AcceptanceError> {
        let scenario = root.join(name);
        fs::create_dir_all(&scenario).map_err(io)?;
        private_dir(&scenario)?;
        let private = scenario.join("private");
        let scope = scenario.join("scope");
        for path in [
            &private,
            &scope,
            &private.join("xdg-config"),
            &private.join("xdg-data"),
            &private.join("xdg-cache"),
            &private.join("tmp"),
        ] {
            fs::create_dir_all(path).map_err(io)?;
            private_dir(path)?;
        }
        Ok(Self {
            private: private.canonicalize().map_err(io)?,
            scope: scope.canonicalize().map_err(io)?,
        })
    }
}

fn setup_online_task(
    device: &Device,
    options: &FailureOptions,
    budget: Option<&str>,
) -> Result<(), AcceptanceError> {
    copy_fixture(device, options)?;
    release_ok(options, device, &["init"])?;
    release_ok(options, device, &["ledger", "init"])?;
    if let Some(body) = budget {
        write_device_config(device, body)?;
    }
    debug_ok(
        options,
        device,
        &["adapter", "approve", "--all", "--yes"],
        &[("KIO_TEST_MISTRAL_OCR", "mock")],
    )?;
    debug_ok(
        options,
        device,
        &["index", "--yes"],
        &[("KIO_TEST_MISTRAL_OCR", "mock")],
    )
}

fn write_device_config(device: &Device, body: &str) -> Result<(), AcceptanceError> {
    fs::create_dir_all(device.private.join("xdg-config/kio")).map_err(io)?;
    fs::write(device.private.join("xdg-config/kio/config.toml"), body).map_err(io)
}

fn setup_batch_task(device: &Device, options: &FailureOptions) -> Result<(), AcceptanceError> {
    copy_fixture(device, options)?;
    release_ok(options, device, &["init"])?;
    release_ok(options, device, &["ledger", "init"])?;
    fs::write(
        device.scope.join(".kio/config.toml"),
        "[markdownize]\nbbox_annotation = false\n",
    )
    .map_err(io)?;
    debug_ok(
        options,
        device,
        &["adapter", "approve", "--all", "--yes"],
        &[("KIO_TEST_MISTRAL_OCR", "mock")],
    )?;
    debug_ok(
        options,
        device,
        &["index", "--yes"],
        &[("KIO_TEST_MISTRAL_OCR", "mock")],
    )
}

fn copy_fixture(device: &Device, options: &FailureOptions) -> Result<(), AcceptanceError> {
    let bytes = bounded_regular_bytes(&options.fixture, MAX_FIXTURE_BYTES)?;
    fs::write(device.scope.join("fixture.pdf"), bytes).map_err(io)
}

fn release_ok(
    options: &FailureOptions,
    device: &Device,
    args: &[&str],
) -> Result<(), AcceptanceError> {
    let out = release(options, device, args)?;
    if out.status.success() {
        Ok(())
    } else {
        Err(command_error("release", args, &out))
    }
}
fn release_json(
    options: &FailureOptions,
    device: &Device,
    args: &[&str],
) -> Result<Value, AcceptanceError> {
    let out = release(options, device, args)?;
    if !out.status.success() {
        return Err(command_error("release", args, &out));
    }
    stdout_json(&out, "release command")
}
fn debug_ok(
    options: &FailureOptions,
    device: &Device,
    args: &[&str],
    env: &[(&str, &str)],
) -> Result<(), AcceptanceError> {
    let out = debug(options, device, args, env)?;
    if out.status.success() {
        Ok(())
    } else {
        Err(command_error("debug", args, &out))
    }
}
fn release(
    options: &FailureOptions,
    device: &Device,
    args: &[&str],
) -> Result<BoundedProcessOutput, AcceptanceError> {
    child(&options.binary, device, args, &[])
}
fn debug(
    options: &FailureOptions,
    device: &Device,
    args: &[&str],
    env: &[(&str, &str)],
) -> Result<BoundedProcessOutput, AcceptanceError> {
    // A fake credential makes this debug-only child exercise the same declared
    // credential route while env_clear prevents any ambient account key from
    // crossing the process boundary.
    let mut scoped = vec![("MISTRAL_API_KEY", "KIO_A08_SYNTHETIC_CREDENTIAL")];
    scoped.extend_from_slice(env);
    child(&options.contract_binary, device, args, &scoped)
}
fn child(
    binary: &Path,
    device: &Device,
    args: &[&str],
    env: &[(&str, &str)],
) -> Result<BoundedProcessOutput, AcceptanceError> {
    let mut command = Command::new(binary);
    IsolatedChildEnvironment::new(
        &device.private,
        device.private.join("xdg-config"),
        device.private.join("xdg-data"),
        device.private.join("xdg-cache"),
        device.private.join("tmp"),
    )
    .apply(&mut command)?;
    command.arg("--json").args(args).current_dir(&device.scope);
    for (key, value) in env {
        command.env(key, value);
    }
    run_bounded_command(
        &mut command,
        BoundedProcessOptions {
            timeout: COMMAND_TIMEOUT,
            max_stdout_bytes: MAX_OUTPUT_BYTES,
            max_stderr_bytes: MAX_OUTPUT_BYTES,
        },
        None,
    )
    .map_err(|error| AcceptanceError::Command(format!("bounded A08 command failed: {error}")))
}

fn online_task(status: &Value) -> Result<&Value, AcceptanceError> {
    status["tasks"]
        .as_array()
        .and_then(|tasks| {
            tasks.iter().find(|task| {
                task["type"] == "markdownize"
                    && (task["output_ref"]
                        .as_str()
                        .is_some_and(|v| v.starts_with("online:"))
                        || task["fallback_reason"] == "online_adapter_done")
            })
        })
        .ok_or_else(|| invalid("status contains no online markdownize task"))
}
fn ledger_counts(device: &Device) -> Result<(i64, i64), AcceptanceError> {
    let conn = Connection::open_with_flags(
        device.private.join("xdg-data/kio/cost-ledger.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|e| invalid(format!("open ledger: {e}")))?;
    let requests = conn
        .query_row("SELECT COUNT(*) FROM batch_requests", [], |row| row.get(0))
        .map_err(|e| invalid(format!("read batch requests: {e}")))?;
    let charges = conn
        .query_row("SELECT COUNT(*) FROM cost_ledger", [], |row| row.get(0))
        .map_err(|e| invalid(format!("read ledger charges: {e}")))?;
    Ok((requests, charges))
}

/// A structured auth rejection is a known pre-bill provider result. Its
/// terminal record must be a zero-valued confirmed rejection and remain
/// unchanged when ordinary `batch retry` refuses the paused task.
#[derive(Debug, PartialEq)]
struct AuthLedgerObservation {
    requests: Vec<AuthRequestRow>,
    charges: Vec<AuthChargeRow>,
}

#[derive(Debug, PartialEq)]
struct AuthRequestRow {
    input_hash: String,
    tool_profile_hash: String,
    state: i64,
    request_kind: String,
    intent_token: Option<String>,
    batch_job_id: Option<String>,
    estimated_usd: f64,
    error: Option<String>,
    submission_seq: i64,
}

#[derive(Debug, PartialEq)]
struct AuthChargeRow {
    submission_seq: i64,
    usd: f64,
    estimated: i64,
    outcome: String,
}

fn auth_ledger_observation(device: &Device) -> Result<AuthLedgerObservation, AcceptanceError> {
    let conn = Connection::open_with_flags(
        device.private.join("xdg-data/kio/cost-ledger.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|e| invalid(format!("open ledger: {e}")))?;
    let mut request_stmt = conn
        .prepare(
            "SELECT input_hash, tool_profile_hash, state, request_kind, intent_token, batch_job_id, \
                    estimated_usd, error, submission_seq \
             FROM batch_requests ORDER BY created_at, submission_seq",
        )
        .map_err(|e| invalid(format!("read auth requests: {e}")))?;
    let requests = request_stmt
        .query_map([], |row| {
            Ok(AuthRequestRow {
                input_hash: row.get(0)?,
                tool_profile_hash: row.get(1)?,
                state: row.get(2)?,
                request_kind: row.get(3)?,
                intent_token: row.get(4)?,
                batch_job_id: row.get(5)?,
                estimated_usd: row.get(6)?,
                error: row.get(7)?,
                submission_seq: row.get(8)?,
            })
        })
        .map_err(|e| invalid(format!("read auth requests: {e}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| invalid(format!("decode auth requests: {e}")))?;
    let mut charge_stmt = conn
        .prepare(
            "SELECT submission_seq, usd, estimated, outcome FROM cost_ledger \
             ORDER BY submission_seq",
        )
        .map_err(|e| invalid(format!("read auth charges: {e}")))?;
    let charges = charge_stmt
        .query_map([], |row| {
            Ok(AuthChargeRow {
                submission_seq: row.get(0)?,
                usd: row.get(1)?,
                estimated: row.get(2)?,
                outcome: row.get(3)?,
            })
        })
        .map_err(|e| invalid(format!("read auth charges: {e}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| invalid(format!("decode auth charges: {e}")))?;
    Ok(AuthLedgerObservation { requests, charges })
}

fn assert_known_sync_rejection(
    before: &AuthLedgerObservation,
    after: &AuthLedgerObservation,
    label: &str,
) -> Result<(), AcceptanceError> {
    let expected_error = match label {
        "auth" => "sync_auth_rejected",
        "rate-limit" => "sync_rate_limited",
        other => return Err(invalid(format!("unknown known-rejection label: {other}"))),
    };
    require(
        before.requests.is_empty() && before.charges.is_empty(),
        &format!("{label} fixture ledger was not fresh"),
    )?;
    require(
        after.requests.len() == 1,
        &format!("{label} failure did not create exactly one terminal sync request"),
    )?;
    let request = &after.requests[0];
    require(
        request.state == 3
            && request.request_kind == "sync"
            && request.intent_token.is_none()
            && request.batch_job_id.is_none()
            && request.error.as_deref() == Some(expected_error),
        &format!("{label} failure did not retain the exact terminal sync rejection identity"),
    )?;
    require(
        request.estimated_usd.is_finite() && request.estimated_usd >= 0.0,
        &format!("{label} rejection retained an invalid original reservation"),
    )?;
    require(
        after.charges.len() == 1,
        &format!("{label} rejection did not create exactly one terminal charge"),
    )?;
    let charge = &after.charges[0];
    require(
        request.submission_seq > 0
            && charge.submission_seq == request.submission_seq
            && charge.outcome == "submit_rejected"
            && charge.estimated == 0
            && charge.usd == 0.0,
        &format!(
            "{label} rejection was not recorded as a confirmed zero-cost submission rejection"
        ),
    )?;
    Ok(())
}
struct BatchRequestRow {
    intent_token: Option<String>,
    batch_job_id: Option<String>,
    job_create_started_at: Option<i64>,
    input_hash: String,
    tool_profile_hash: String,
}

fn batch_row(device: &Device) -> Result<BatchRequestRow, AcceptanceError> {
    let conn = Connection::open_with_flags(
        device.private.join("xdg-data/kio/cost-ledger.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|e| invalid(format!("open ledger: {e}")))?;
    conn.query_row(
        "SELECT intent_token, batch_job_id, job_create_started_at, input_hash, tool_profile_hash FROM batch_requests ORDER BY created_at DESC LIMIT 1",
        [],
        |row| Ok(BatchRequestRow {
            intent_token: row.get(0)?,
            batch_job_id: row.get(1)?,
            job_create_started_at: row.get(2)?,
            input_hash: row.get(3)?,
            tool_profile_hash: row.get(4)?,
        }),
    ).map_err(|e| invalid(format!("read batch request: {e}")))
}
fn scope_id(device: &Device) -> Result<String, AcceptanceError> {
    let value: Value =
        serde_json::from_slice(&fs::read(device.scope.join(".kio/scope.json")).map_err(io)?)
            .map_err(|e| invalid(format!("parse scope: {e}")))?;
    value["scope_id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid("scope has no id"))
}
fn stdout_json(out: &BoundedProcessOutput, context: &str) -> Result<Value, AcceptanceError> {
    serde_json::from_str(&out.stdout)
        .map_err(|e| invalid(format!("{context} returned invalid JSON: {e}")))
}
fn require_exit(
    out: &BoundedProcessOutput,
    expected: i32,
    context: &str,
) -> Result<(), AcceptanceError> {
    if out.status.code() == Some(expected) {
        Ok(())
    } else {
        Err(AcceptanceError::Command(format!(
            "{context} returned {} instead of exit {expected}: {}",
            out.status,
            out.stderr.trim()
        )))
    }
}
fn command_error(binary: &str, args: &[&str], out: &BoundedProcessOutput) -> AcceptanceError {
    AcceptanceError::Command(format!(
        "{binary} {} failed with {}: {}",
        args.join(" "),
        out.status,
        out.stderr.trim()
    ))
}
fn require(condition: bool, message: &str) -> Result<(), AcceptanceError> {
    if condition {
        Ok(())
    } else {
        Err(AcceptanceError::Command(message.into()))
    }
}
fn bounded_regular_bytes(path: &Path, maximum: u64) -> Result<Vec<u8>, AcceptanceError> {
    let metadata = fs::symlink_metadata(path).map_err(io)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > maximum {
        return Err(invalid("A08 fixture must be a bounded regular file"));
    }
    fs::read(path).map_err(io)
}
fn private_dir(_path: &Path) -> Result<(), AcceptanceError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(_path, fs::Permissions::from_mode(0o700)).map_err(io)?;
    }
    Ok(())
}
fn io(error: std::io::Error) -> AcceptanceError {
    AcceptanceError::Io(error.to_string())
}
fn invalid(message: impl Into<String>) -> AcceptanceError {
    AcceptanceError::Invalid(message.into())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn device_new_restricts_group_writable_scenario_ancestor() {
        // Keep the fixture under the checkout, matching the private_fs contract
        // tests, and explicitly seed 0775 instead of racing on process umask.
        let temporary = tempfile::Builder::new()
            .prefix("kio-a08-private-scenario-")
            .tempdir_in(std::env::current_dir().expect("current directory"))
            .expect("scenario fixture");
        private_dir(temporary.path()).expect("private fixture root");
        let root = temporary.path().canonicalize().expect("canonical root");
        kio_core::private_fs::verify_private_creation_parent(&root)
            .expect("fixture ancestry must already satisfy the product policy");

        for name in [
            "missing-credential",
            "auth",
            "rate-limit",
            "budget",
            "unknown-accepted",
            "unknown-sync-resend",
        ] {
            let scenario = root.join(name);
            fs::create_dir(&scenario).expect("seed scenario directory");
            fs::set_permissions(&scenario, fs::Permissions::from_mode(0o775))
                .expect("seed group-writable intermediate directory");
            assert!(
                kio_core::private_fs::verify_private_creation_parent(&scenario).is_err(),
                "the product must reject the unsafe scenario fixture"
            );

            let device = Device::new(&root, name).expect("prepare A08 device");
            for path in [
                device.private.clone(),
                device.scope.clone(),
                device.private.join("xdg-config"),
                device.private.join("xdg-data"),
                device.private.join("xdg-cache"),
                device.private.join("tmp"),
            ] {
                kio_core::private_fs::verify_private_creation_parent(&path)
                    .unwrap_or_else(|error| panic!("unsafe A08 path {}: {error}", path.display()));
                assert_eq!(
                    fs::metadata(&path)
                        .expect("prepared directory metadata")
                        .permissions()
                        .mode()
                        & 0o777,
                    0o700,
                    "A08 device directory {} must be owner-private",
                    path.display()
                );
            }
            assert_eq!(
                fs::metadata(&scenario)
                    .expect("scenario metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700,
                "A08 scenario ancestor must be owner-private"
            );
        }
    }
}
