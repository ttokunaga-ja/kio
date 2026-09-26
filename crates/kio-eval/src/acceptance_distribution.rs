//! A12 distribution acceptance over two independently packaged candidate archives.
//!
//! The release verifier owns archive parsing, canonical-layout validation,
//! SBOM/provenance validation, and safe extraction.  This module binds those
//! results to the immutable A12 expectation before writing one receipt.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    process::Command,
    time::Duration,
};

use crate::{
    acceptance::{
        AcceptanceCase, AcceptanceError, AcceptanceLane, AcceptanceReceipt, AcceptanceSubcase,
        ExpectedReceipt, MAX_BINARY_BYTES, current_evaluator_sha256,
        current_evaluator_sha256_matches, empty_runtime, sha256_regular_file, validate_expected,
        write_receipt_create_only,
    },
    acceptance_environment::IsolatedChildEnvironment,
    release::{
        SmokeCandidateOptions, VerifyCandidateOptions, candidate_archive_sha256, smoke_candidate,
        verify_candidate,
    },
    runner::{BoundedProcessOptions, run_bounded_command},
};
use kio_core::cas::canonical_json_bytes;
use serde::{Deserialize, Serialize};

const HELP_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_HELP_BYTES: usize = 64 * 1024;
const MAX_TARGET_BINDING_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone)]
pub struct DistributionOptions {
    pub expected: ExpectedReceipt,
    pub archive: PathBuf,
    pub checksums: PathBuf,
    pub repro_archive: PathBuf,
    pub repro_checksums: PathBuf,
    pub source_repo: PathBuf,
    pub expected_lock_sha256: String,
    pub work_dir: PathBuf,
    pub receipt: PathBuf,
}

/// Immutable per-target archive metadata published by the packaging job before
/// an acceptance binding is assembled.  This contains no receipt or execution
/// result, so later acceptance cannot choose its own expected archive hash.
#[derive(Debug, Clone)]
pub struct TargetBindingOptions {
    pub candidate_sha: String,
    pub os: crate::acceptance::NativeOs,
    pub target: String,
    pub archive_sha256: String,
    pub binary_sha256: String,
    pub version: String,
    pub out: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetBindingArtifact {
    candidate_sha: String,
    os: crate::acceptance::NativeOs,
    target: String,
    archive_sha256: String,
    binary_sha256: String,
    version: String,
}

#[derive(Debug, Clone)]
pub struct ExpectedCaseOptions {
    pub target_binding: PathBuf,
    pub requirement: crate::acceptance::AcceptanceRequirement,
    pub fixture_id: String,
    pub fixture_sha256: String,
    pub workflow_path: String,
    pub workflow_sha256: String,
    pub workflow_commit: String,
    pub run_id: u64,
    pub attempt: u32,
    pub architecture: String,
    pub contract_binary_sha256: Option<String>,
    pub service_identity_sha256: Option<String>,
    pub out: PathBuf,
}

/// Write canonical, create-only target binding metadata for a packaged archive.
pub fn write_target_binding(options: &TargetBindingOptions) -> Result<(), AcceptanceError> {
    if !is_lower_hex(&options.candidate_sha, 40)
        || !is_lower_hex(&options.archive_sha256, 64)
        || !is_lower_hex(&options.binary_sha256, 64)
        || options.version.is_empty()
        || options.version.len() > 128
        || !target_matches_os(&options.target, options.os)
    {
        return Err(AcceptanceError::Invalid(
            "target binding fields are malformed or do not match the native OS".into(),
        ));
    }
    let parent = options.out.parent().ok_or_else(|| {
        AcceptanceError::Invalid("target binding output has no parent directory".into())
    })?;
    let parent_metadata = fs::symlink_metadata(parent).map_err(io)?;
    if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
        return Err(AcceptanceError::Invalid(
            "target binding output parent must be a real directory".into(),
        ));
    }
    let artifact = TargetBindingArtifact {
        candidate_sha: options.candidate_sha.clone(),
        os: options.os,
        target: options.target.clone(),
        archive_sha256: options.archive_sha256.clone(),
        binary_sha256: options.binary_sha256.clone(),
        version: options.version.clone(),
    };
    let bytes = canonical_json_bytes(
        &serde_json::to_value(artifact)
            .map_err(|error| AcceptanceError::Json(error.to_string()))?,
    )
    .map_err(|error| AcceptanceError::Json(error.to_string()))?;
    if bytes.len() > MAX_TARGET_BINDING_BYTES {
        return Err(AcceptanceError::Invalid(
            "target binding serialization exceeds its byte bound".into(),
        ));
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&options.out)
        .map_err(io)?;
    output.write_all(&bytes).map_err(io)?;
    output.sync_all().map_err(io)
}

/// Produce one canonical expectation for a case workflow from already-published
/// target metadata.  Unlike the full-matrix manifest, this deliberately does
/// not manufacture bindings for workflows which have not been dispatched.
pub fn write_expected_case(options: &ExpectedCaseOptions) -> Result<(), AcceptanceError> {
    let target = read_target_binding(&options.target_binding)?;
    if target.os != options.requirement.os {
        return Err(AcceptanceError::Invalid(
            "expected-case target binding OS differs from the requested requirement".into(),
        ));
    }
    let expected = ExpectedReceipt {
        requirement: options.requirement.clone(),
        candidate: crate::acceptance::CandidateBinding {
            candidate_sha: target.candidate_sha,
            target: target.target,
            archive_sha256: target.archive_sha256,
            binary_sha256: target.binary_sha256,
            version: target.version,
        },
        fixture: crate::acceptance::FixtureBinding {
            fixture_id: options.fixture_id.clone(),
            sha256: options.fixture_sha256.clone(),
        },
        workflow: crate::acceptance::WorkflowBinding {
            workflow_path: options.workflow_path.clone(),
            workflow_sha256: options.workflow_sha256.clone(),
            workflow_commit: options.workflow_commit.clone(),
            run_id: options.run_id,
            attempt: options.attempt,
            architecture: options.architecture.clone(),
        },
        evaluator_sha256: current_evaluator_sha256()?,
        service_identity_sha256: options.service_identity_sha256.clone(),
        contract_binary_sha256: options.contract_binary_sha256.clone(),
    };
    crate::acceptance::write_expected_receipt_create_only(&options.out, &expected)
}

/// Validate both independently packaged archives, extract and smoke the first
/// one through the release verifier, then issue an A12 receipt.
pub fn run(options: &DistributionOptions) -> Result<AcceptanceReceipt, AcceptanceError> {
    let requirement = &options.expected.requirement;
    if requirement.case != AcceptanceCase::A12
        || requirement.lane != AcceptanceLane::Distribution
        || requirement.subcase != AcceptanceSubcase::Core
    {
        return Err(AcceptanceError::Invalid(
            "distribution executor received a non-A12 requirement".into(),
        ));
    }
    validate_expected(&options.expected)?;
    current_evaluator_sha256_matches(&options.expected.evaluator_sha256)?;
    if options.work_dir.exists() || options.receipt.exists() {
        return Err(AcceptanceError::Invalid(
            "distribution work directory and receipt must be create-only".into(),
        ));
    }
    let primary_path = fs::canonicalize(&options.archive).map_err(io)?;
    let reproducible_path = fs::canonicalize(&options.repro_archive).map_err(io)?;
    if primary_path == reproducible_path {
        return Err(AcceptanceError::Invalid(
            "primary and reproducible archives must be separately produced files".into(),
        ));
    }

    let verify = verify_options(options, &options.archive, &options.checksums);
    let primary = verify_candidate(&verify).map_err(release_error)?;
    assert_binding(
        options,
        &primary.binding.version,
        &primary.binding.commit,
        &primary.binding.target,
    )?;

    let reproducible_hash =
        candidate_archive_sha256(&options.repro_archive).map_err(release_error)?;
    if reproducible_hash != primary.archive_sha256
        || reproducible_hash != options.expected.candidate.archive_sha256
    {
        return Err(AcceptanceError::Invalid(
            "independently packaged archive does not byte-match the expected candidate archive"
                .into(),
        ));
    }
    let repro = verify_candidate(&verify_options(
        options,
        &options.repro_archive,
        &options.repro_checksums,
    ))
    .map_err(release_error)?;
    if repro.binding != primary.binding || repro.archive_sha256 != primary.archive_sha256 {
        return Err(AcceptanceError::Invalid(
            "reproducible archive binding differs from the primary candidate".into(),
        ));
    }

    let smoke = smoke_candidate(&SmokeCandidateOptions {
        verify,
        work_dir: options.work_dir.clone(),
        receipt: None,
    })
    .map_err(release_error)?;
    if smoke.version != options.expected.candidate.version
        || smoke.commit != options.expected.candidate.candidate_sha
        || smoke.target != options.expected.candidate.target
        || smoke.archive_sha256 != options.expected.candidate.archive_sha256
    {
        return Err(AcceptanceError::Invalid(
            "extracted release identity differs from the expected A12 candidate".into(),
        ));
    }
    let binary = options
        .work_dir
        .join(&primary.root)
        .join("bin")
        .join(binary_name(&primary.binding.target)?);
    if sha256_regular_file(&binary, MAX_BINARY_BYTES)? != options.expected.candidate.binary_sha256 {
        return Err(AcceptanceError::Invalid(
            "extracted binary hash differs from the expected candidate".into(),
        ));
    }
    require_help(&binary, &options.work_dir.join("isolated"))?;

    let receipt = AcceptanceReceipt {
        schema: "kio.acceptance.receipt/v4".into(),
        requirement: requirement.clone(),
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

fn verify_options(
    options: &DistributionOptions,
    archive: &std::path::Path,
    checksums: &std::path::Path,
) -> VerifyCandidateOptions {
    VerifyCandidateOptions {
        archive: archive.to_path_buf(),
        checksum: Some(checksums.to_path_buf()),
        expected_archive_sha256: options.expected.candidate.archive_sha256.clone(),
        expected_repo: Some(options.source_repo.clone()),
        expected_commit: Some(options.expected.candidate.candidate_sha.clone()),
        expected_lock_sha256: Some(options.expected_lock_sha256.clone()),
    }
}

fn assert_binding(
    options: &DistributionOptions,
    version: &str,
    commit: &str,
    target: &str,
) -> Result<(), AcceptanceError> {
    let candidate = &options.expected.candidate;
    if version != candidate.version
        || commit != candidate.candidate_sha
        || target != candidate.target
        || !target_matches_os(target, options.expected.requirement.os)
    {
        return Err(AcceptanceError::Invalid(
            "archive binding differs from the expected A12 target".into(),
        ));
    }
    Ok(())
}

fn target_matches_os(target: &str, os: crate::acceptance::NativeOs) -> bool {
    matches!(
        (os, target),
        (
            crate::acceptance::NativeOs::Linux,
            "x86_64-unknown-linux-gnu"
        ) | (crate::acceptance::NativeOs::Macos, "aarch64-apple-darwin")
            | (
                crate::acceptance::NativeOs::Windows,
                "x86_64-pc-windows-msvc"
            )
    )
}

fn binary_name(target: &str) -> Result<&'static str, AcceptanceError> {
    match target {
        "x86_64-unknown-linux-gnu" | "aarch64-apple-darwin" => Ok("kio"),
        "x86_64-pc-windows-msvc" => Ok("kio.exe"),
        _ => Err(AcceptanceError::Invalid(
            "A12 target is not one of the supported native targets".into(),
        )),
    }
}

fn require_help(
    binary: &std::path::Path,
    isolated: &std::path::Path,
) -> Result<(), AcceptanceError> {
    let mut command = Command::new(binary);
    IsolatedChildEnvironment::new(
        isolated,
        isolated.join("xdg-config"),
        isolated.join("xdg-data"),
        isolated.join("xdg-cache"),
        isolated.join("tmp"),
    )
    .apply(&mut command)?;
    command.arg("--help");
    let output = run_bounded_command(
        &mut command,
        BoundedProcessOptions {
            timeout: HELP_TIMEOUT,
            max_stdout_bytes: MAX_HELP_BYTES,
            max_stderr_bytes: MAX_HELP_BYTES,
        },
        None,
    )
    .map_err(|error| AcceptanceError::Command(format!("release --help: {error}")))?;
    if !output.status.success() || output.stdout.trim().is_empty() {
        return Err(AcceptanceError::Command(
            "extracted release binary --help failed".into(),
        ));
    }
    Ok(())
}

fn release_error(error: crate::release::ReleaseError) -> AcceptanceError {
    AcceptanceError::Command(format!("release distribution verification: {error}"))
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn read_target_binding(path: &std::path::Path) -> Result<TargetBindingArtifact, AcceptanceError> {
    let metadata = fs::symlink_metadata(path).map_err(io)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_TARGET_BINDING_BYTES as u64
    {
        return Err(AcceptanceError::Invalid(
            "target binding input must be a bounded regular file".into(),
        ));
    }
    let bytes = fs::read(path).map_err(io)?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| AcceptanceError::Json(error.to_string()))?;
    let canonical =
        canonical_json_bytes(&value).map_err(|error| AcceptanceError::Json(error.to_string()))?;
    if canonical != bytes {
        return Err(AcceptanceError::Invalid(
            "target binding input is not canonical JSON".into(),
        ));
    }
    let target = serde_json::from_value::<TargetBindingArtifact>(value)
        .map_err(|error| AcceptanceError::Json(error.to_string()))?;
    if !is_lower_hex(&target.candidate_sha, 40)
        || !is_lower_hex(&target.archive_sha256, 64)
        || !is_lower_hex(&target.binary_sha256, 64)
        || target.version.is_empty()
        || target.version.len() > 128
        || !target_matches_os(&target.target, target.os)
    {
        return Err(AcceptanceError::Invalid(
            "target binding input is malformed".into(),
        ));
    }
    Ok(target)
}

fn io(error: std::io::Error) -> AcceptanceError {
    AcceptanceError::Io(error.to_string())
}
