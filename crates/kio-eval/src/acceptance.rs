//! Exact-candidate acceptance receipts and the deliberately narrow A01 runner.
//!
//! A receipt is evidence for one required case on one native target.  It is
//! never a release decision by itself: [`AcceptancePlan::verify`] rejects a
//! missing, duplicate, foreign, or differently-bound receipt.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Barrier},
    thread,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    acceptance_environment::IsolatedChildEnvironment,
    runner::{BoundedProcessOptions, BoundedProcessOutput, run_bounded_command},
};

const SCHEMA: &str = "kio.acceptance.receipt/v4";
pub const ACCEPTANCE_MANIFEST_SCHEMA: &str = "kio.acceptance.expected/v3";
pub const ACCEPTANCE_BINDING_SCHEMA: &str = "kio.acceptance.binding/v4";
pub const FIXTURE_BUNDLE_SCHEMA: &str = "kio.acceptance.fixture/v1";
const FIXTURE_BUNDLE_MANIFEST: &str = "acceptance-fixture.json";
pub(crate) const MAX_FIXTURE_BYTES: u64 = 1024 * 1024;
pub(crate) const MAX_BINARY_BYTES: u64 = 512 * 1024 * 1024;
const MAX_EXPECTED_CASE_BYTES: u64 = 64 * 1024;
const MAX_RECEIPT_BYTES: u64 = 64 * 1024;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AcceptanceError {
    #[error("acceptance input is invalid: {0}")]
    Invalid(String),
    #[error("acceptance receipt is invalid: {0}")]
    Receipt(String),
    #[error("required acceptance evidence is missing: {0}")]
    Missing(String),
    #[error("acceptance command failed: {0}")]
    Command(String),
    #[error("I/O: {0}")]
    Io(String),
    #[error("JSON: {0}")]
    Json(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AcceptanceCase {
    A01,
    A02,
    A03,
    A04,
    A05,
    A06,
    A07,
    A08,
    A09,
    A10,
    A11,
    A12,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceLane {
    NativeContract,
    OfficeReal,
    ServiceNative,
    ProviderLive,
    Distribution,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceSubcase {
    Core,
    LocalTrust,
    MockFailure,
    Mistral,
    Gemini,
    AuthenticatedLocal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeOs {
    Linux,
    Macos,
    Windows,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateBinding {
    pub candidate_sha: String,
    pub target: String,
    pub archive_sha256: String,
    pub binary_sha256: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureBinding {
    pub fixture_id: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowBinding {
    pub workflow_path: String,
    pub workflow_sha256: String,
    pub workflow_commit: String,
    pub run_id: u64,
    pub attempt: u32,
    pub architecture: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceRequirement {
    pub case: AcceptanceCase,
    pub lane: AcceptanceLane,
    pub subcase: AcceptanceSubcase,
    pub os: NativeOs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedReceipt {
    #[serde(flatten)]
    pub requirement: AcceptanceRequirement,
    pub candidate: CandidateBinding,
    pub fixture: FixtureBinding,
    pub workflow: WorkflowBinding,
    pub evaluator_sha256: String,
    pub service_identity_sha256: Option<String>,
    /// SHA-256 of the separately instrumented contract binary for the two
    /// native fault-contract requirements. All other requirements must omit it.
    pub contract_binary_sha256: Option<String>,
}

/// Immutable input prepared by the candidate workflow before any case runs.
/// It contains expected bindings, never self-reported execution results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceManifest {
    pub schema: String,
    pub expected: Vec<ExpectedReceipt>,
}

/// Reviewed bindings used to generate the complete expected matrix. It has no
/// receipt or result fields, so a passing case can never influence what
/// evidence is expected for another target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceBindingInput {
    pub schema: String,
    pub candidate_sha: String,
    pub targets: Vec<AcceptanceTargetBinding>,
    pub executions: Vec<AcceptanceExecutionBinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceTargetBinding {
    pub os: NativeOs,
    pub target: String,
    pub archive_sha256: String,
    pub binary_sha256: String,
    pub version: String,
}

/// The independently reviewed inputs for one case execution.  This binding is
/// deliberately separate from the packaged target: an execution workflow and
/// fixture vary by requirement, while an archive hash always identifies the
/// candidate package rather than the workflow which ran the case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceExecutionBinding {
    #[serde(flatten)]
    pub requirement: AcceptanceRequirement,
    pub fixture: FixtureBinding,
    pub workflow: WorkflowBinding,
    pub evaluator_sha256: String,
    pub service_identity_sha256: Option<String>,
    pub contract_binary_sha256: Option<String>,
}

/// Bounded, content-addressed input for cases that need more than one text
/// file. The manifest is canonical and every listed leaf is re-hashed before
/// an executor uses it; unlisted files are not copied into a test scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureBundleManifest {
    pub schema: String,
    pub scopes: Vec<String>,
    pub files: Vec<FixtureBundleFile>,
    pub queries: Vec<FixtureBundleQuery>,
    pub mutations: Vec<FixtureBundleMutation>,
    pub requirements: FixtureBundleRequirements,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureBundleRequirements {
    pub require_image: bool,
    pub require_vector: bool,
    pub deterministic_embedding_runtime: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureBundleFile {
    pub scope: String,
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureBundleQuery {
    pub scope: String,
    pub query: String,
    pub expect_path: String,
    pub expect_image: Option<FixtureImageExpectation>,
}

/// A standalone image's immutable identity, independent of the text hit
/// expected from the same query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureImageExpectation {
    pub scope: String,
    pub path: String,
    pub sha256: String,
}

/// A bounded post-index text mutation. Binary and image inputs remain
/// immutable fixture leaves; a history assertion mutates only a listed text
/// path and creates a new Kio commit through the release binary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureBundleMutation {
    pub scope: String,
    pub path: String,
    pub replacement_utf8: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedToolIdentity {
    pub tool_id: String,
    pub tool_profile_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedConverterIdentity {
    pub sha256: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceRuntime {
    pub tools: Vec<ObservedToolIdentity>,
    pub converter: Option<ObservedConverterIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceIdentity {
    pub schema: String,
    pub run_nonce: String,
    pub ocr_endpoint: String,
    pub embedding_endpoint: String,
    pub ca_sha256: String,
    pub leaf_cert_sha256: String,
    pub controller_sha256: String,
    pub proxy_sha256: String,
    pub entrypoint_sha256: String,
    pub ocr_compose_sha256: String,
    pub embedding_compose_sha256: String,
    pub backend_config_sha256: String,
    pub ocr_image: String,
    pub ocr_revision: String,
    pub ocr_weight_sha256: String,
    pub embedding_image: String,
    pub embedding_revision: String,
    pub embedding_weight_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceReceipt {
    pub schema: String,
    #[serde(flatten)]
    pub requirement: AcceptanceRequirement,
    pub candidate: CandidateBinding,
    pub fixture: FixtureBinding,
    pub workflow: WorkflowBinding,
    pub evaluator_sha256: String,
    pub service_identity_sha256: Option<String>,
    pub contract_binary_sha256: Option<String>,
    pub runtime: AcceptanceRuntime,
    /// A receipt is emitted only after the case has passed.  There is no
    /// skipped or unprepared outcome that an aggregator could mistake for a
    /// success.
    pub passed: bool,
}

pub(crate) fn read_service_identity(
    path: &Path,
) -> Result<(ServiceIdentity, String), AcceptanceError> {
    let bytes = bounded_regular_bytes(path, MAX_EXPECTED_CASE_BYTES)?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| AcceptanceError::Json(e.to_string()))?;
    let canonical = kio_core::cas::canonical_json_bytes(&value)
        .map_err(|e| AcceptanceError::Json(e.to_string()))?;
    if canonical != bytes {
        return Err(AcceptanceError::Invalid(
            "service identity is not canonical JSON".into(),
        ));
    }
    let identity: ServiceIdentity =
        serde_json::from_value(value).map_err(|e| AcceptanceError::Json(e.to_string()))?;
    validate_service_identity(&identity)?;
    Ok((identity, sha256_bytes(&canonical)))
}

pub(crate) fn validate_service_identity(identity: &ServiceIdentity) -> Result<(), AcceptanceError> {
    if identity.schema != "kio.local-gpu.identity/v1"
        || identity.run_nonce.is_empty()
        || identity.run_nonce.len() > 256
        || !identity.run_nonce.is_ascii()
    {
        return Err(AcceptanceError::Invalid(
            "service identity schema or nonce is invalid".into(),
        ));
    }
    for endpoint in [&identity.ocr_endpoint, &identity.embedding_endpoint] {
        validate_loopback_https(endpoint)?;
    }
    for hash in [
        &identity.ca_sha256,
        &identity.leaf_cert_sha256,
        &identity.controller_sha256,
        &identity.proxy_sha256,
        &identity.entrypoint_sha256,
        &identity.ocr_compose_sha256,
        &identity.embedding_compose_sha256,
        &identity.backend_config_sha256,
        &identity.ocr_weight_sha256,
        &identity.embedding_weight_sha256,
    ] {
        if !hash.starts_with("sha256:") || !is_lower_hex(&hash[7..], 64) {
            return Err(AcceptanceError::Invalid(
                "service identity hash is malformed".into(),
            ));
        }
    }
    for text in [
        &identity.ocr_image,
        &identity.ocr_revision,
        &identity.embedding_image,
        &identity.embedding_revision,
    ] {
        if text.is_empty() || text.len() > 512 || !text.is_ascii() {
            return Err(AcceptanceError::Invalid(
                "service identity text is malformed".into(),
            ));
        }
    }
    if identity.ocr_image
        != "ccr-2vdh3abv-pub.cnc.bj.baidubce.com/paddlepaddle/paddleocr-vl@sha256:6c735bdf9e758ffdd58ccc067db0c2d84e37e5e6a2cbd47156069d4d7ea5d709"
        || identity.ocr_revision != "PaddleOCR-VL-1.6"
        || identity.ocr_weight_sha256
            != "sha256:85a479d506a11e724e7285d395c551be69f41dbc16b6342d3cacfb189aed71db"
        || identity.embedding_image
            != "vllm/vllm-openai@sha256:770fe65b2c73ee74a5c42165cf3433de4048cc2cd9c57a937ca4e35aba5aa87b"
        || identity.embedding_revision != "9f2f7e710d6d81056aa5c0a4f04764fec6bb7bda"
        || identity.embedding_weight_sha256
            != "sha256:c73fa9caeddeb3ff831d46c085a7a5708343248ca777e90f2d486964464509c1"
    {
        return Err(AcceptanceError::Invalid(
            "service identity does not match measured GPU pins".into(),
        ));
    }
    Ok(())
}

fn validate_loopback_https(endpoint: &str) -> Result<(), AcceptanceError> {
    let rest = endpoint
        .strip_prefix("https://")
        .ok_or_else(|| AcceptanceError::Invalid("service endpoint must use https".into()))?;
    if rest.contains('@') || rest.contains('?') || rest.contains('#') {
        return Err(AcceptanceError::Invalid(
            "service endpoint contains forbidden URL syntax".into(),
        ));
    }
    let (authority, path) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
    if !path.is_empty() || authority.is_empty() {
        return Err(AcceptanceError::Invalid(
            "service endpoint path must be /".into(),
        ));
    }
    let valid = if let Some(port) = authority.strip_prefix("[::1]:") {
        port
    } else if let Some(port) = authority.strip_prefix("127.0.0.1:") {
        port
    } else {
        return Err(AcceptanceError::Invalid(
            "service endpoint must target literal loopback".into(),
        ));
    };
    let port: u16 = valid
        .parse()
        .map_err(|_| AcceptanceError::Invalid("service endpoint port is invalid".into()))?;
    if port == 0 {
        return Err(AcceptanceError::Invalid(
            "service endpoint port is invalid".into(),
        ));
    }
    Ok(())
}

pub(crate) fn empty_runtime() -> AcceptanceRuntime {
    AcceptanceRuntime {
        tools: Vec::new(),
        converter: None,
    }
}

pub(crate) fn runtime_from_scope(root: &Path) -> Result<AcceptanceRuntime, AcceptanceError> {
    let path = root.join(".kio").join("tool-lock.json");
    if !path.is_file() {
        return Ok(empty_runtime());
    }
    let bytes = bounded_regular_bytes(&path, MAX_MANIFEST_BYTES)?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| AcceptanceError::Json(error.to_string()))?;
    let object = value
        .as_object()
        .ok_or_else(|| AcceptanceError::Invalid("tool-lock must be an object".into()))?;
    let mut tools = Vec::new();
    for (role, entry) in object {
        let Some(profile) = entry
            .get("profile_hash")
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        let tool_id = entry
            .get("tool_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                AcceptanceError::Invalid(format!("tool-lock role {role} has no tool_id"))
            })?;
        tools.push(ObservedToolIdentity {
            tool_id: tool_id.to_owned(),
            tool_profile_hash: profile.to_owned(),
        });
    }
    tools.sort_by(|a, b| a.tool_id.cmp(&b.tool_id));
    let runtime = AcceptanceRuntime {
        tools,
        converter: None,
    };
    validate_runtime(&runtime)?;
    Ok(runtime)
}

pub(crate) fn validate_runtime(runtime: &AcceptanceRuntime) -> Result<(), AcceptanceError> {
    if runtime.tools.len() > 32 {
        return Err(AcceptanceError::Invalid("too many observed tools".into()));
    }
    let mut previous: Option<&str> = None;
    for tool in &runtime.tools {
        if tool.tool_id.is_empty() || tool.tool_id.len() > 128 || !tool.tool_id.is_ascii() {
            return Err(AcceptanceError::Invalid(
                "observed tool id is malformed".into(),
            ));
        }
        if previous.is_some_and(|value| value >= tool.tool_id.as_str()) {
            return Err(AcceptanceError::Invalid(
                "observed tools are not sorted and unique".into(),
            ));
        }
        previous = Some(&tool.tool_id);
        if !tool.tool_profile_hash.starts_with("sha256:")
            || !is_lower_hex(&tool.tool_profile_hash[7..], 64)
        {
            return Err(AcceptanceError::Invalid(
                "observed tool profile hash is malformed".into(),
            ));
        }
    }
    if let Some(converter) = &runtime.converter
        && (!is_lower_hex(&converter.sha256, 64)
            || converter.version.is_empty()
            || converter.version.len() > 16 * 1024
            || !converter.version.is_ascii())
    {
        return Err(AcceptanceError::Invalid(
            "observed converter identity is malformed".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct AcceptancePlan {
    expected: Vec<ExpectedReceipt>,
}

#[derive(Debug, Clone)]
pub struct A01Options {
    pub binary: PathBuf,
    pub fixture: PathBuf,
    pub expected: ExpectedReceipt,
    pub work_dir: PathBuf,
    pub receipt: PathBuf,
}

#[derive(Debug, Clone)]
pub struct A09Options {
    pub binary: PathBuf,
    pub fixture: PathBuf,
    pub expected: ExpectedReceipt,
    pub work_dir: PathBuf,
    pub receipt: PathBuf,
}

#[derive(Debug, Clone)]
pub struct A05Options {
    pub binary: PathBuf,
    /// A versioned multi-scope fixture bundle directory, not a single file.
    pub fixture: PathBuf,
    pub expected: ExpectedReceipt,
    pub work_dir: PathBuf,
    pub receipt: PathBuf,
}

#[derive(Debug, Clone)]
pub struct A06Options {
    pub binary: PathBuf,
    pub fixture: PathBuf,
    pub expected: ExpectedReceipt,
    pub work_dir: PathBuf,
    pub receipt: PathBuf,
}

impl AcceptancePlan {
    /// Construct a fully bound v1 plan.  The expected set is fixed by the
    /// product contract; hashes and run bindings are supplied by the immutable
    /// candidate workflow, not inferred from received artifacts.
    pub fn new(expected: Vec<ExpectedReceipt>) -> Result<Self, AcceptanceError> {
        let actual = expected
            .iter()
            .map(|entry| entry.requirement.clone())
            .collect::<BTreeSet<_>>();
        if actual.len() != expected.len() {
            return Err(AcceptanceError::Invalid(
                "expected receipt requirements contain a duplicate".into(),
            ));
        }
        if actual != fixed_v1_requirements() {
            return Err(AcceptanceError::Invalid(
                "expected receipt requirements differ from the fixed v1 matrix".into(),
            ));
        }
        for entry in &expected {
            validate_expected(entry)?;
        }
        validate_expected_set_bindings(&expected)?;
        Ok(Self { expected })
    }

    pub fn verify(&self, receipts: &[AcceptanceReceipt]) -> Result<(), AcceptanceError> {
        let mut remaining = self
            .expected
            .iter()
            .map(|entry| (entry.requirement.clone(), entry))
            .collect::<std::collections::BTreeMap<_, _>>();
        for receipt in receipts {
            validate_receipt(receipt)?;
            let expected = remaining.remove(&receipt.requirement).ok_or_else(|| {
                AcceptanceError::Receipt(format!(
                    "unexpected or duplicate {}",
                    requirement_label(&receipt.requirement)
                ))
            })?;
            if receipt.candidate != expected.candidate
                || receipt.fixture != expected.fixture
                || receipt.workflow != expected.workflow
                || receipt.evaluator_sha256 != expected.evaluator_sha256
                || receipt.service_identity_sha256 != expected.service_identity_sha256
                || receipt.contract_binary_sha256 != expected.contract_binary_sha256
            {
                return Err(AcceptanceError::Receipt(format!(
                    "binding differs for {}",
                    requirement_label(&receipt.requirement)
                )));
            }
        }
        if let Some((requirement, _)) = remaining.into_iter().next() {
            return Err(AcceptanceError::Missing(requirement_label(&requirement)));
        }
        Ok(())
    }

    #[must_use]
    pub fn expected(&self) -> &[ExpectedReceipt] {
        &self.expected
    }

    pub fn expected_for(
        &self,
        requirement: &AcceptanceRequirement,
    ) -> Result<&ExpectedReceipt, AcceptanceError> {
        self.expected
            .iter()
            .find(|entry| &entry.requirement == requirement)
            .ok_or_else(|| AcceptanceError::Missing(requirement_label(requirement)))
    }
}

fn validate_expected_set_bindings(expected: &[ExpectedReceipt]) -> Result<(), AcceptanceError> {
    let first = expected.first().ok_or_else(|| {
        AcceptanceError::Invalid("expected receipt requirements cannot be empty".into())
    })?;
    let candidate_sha = &first.candidate.candidate_sha;
    let version = &first.candidate.version;
    let mut candidate_by_os = BTreeMap::new();
    let mut contract_binary_by_os = BTreeMap::new();
    for entry in expected {
        if entry.candidate.candidate_sha != *candidate_sha || entry.candidate.version != *version {
            return Err(AcceptanceError::Invalid(
                "expected receipts must bind one candidate SHA and version".into(),
            ));
        }
        match candidate_by_os.entry(entry.requirement.os) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(&entry.candidate);
            }
            std::collections::btree_map::Entry::Occupied(slot)
                if *slot.get() != &entry.candidate =>
            {
                return Err(AcceptanceError::Invalid(
                    "expected receipts must bind one exact candidate per OS".into(),
                ));
            }
            std::collections::btree_map::Entry::Occupied(_) => {}
        }
        if contract_binary_required(&entry.requirement) {
            let contract_binary = entry
                .contract_binary_sha256
                .as_deref()
                .expect("individual expected receipt validation requires this hash");
            match contract_binary_by_os.entry(entry.requirement.os) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(contract_binary);
                }
                std::collections::btree_map::Entry::Occupied(slot)
                    if *slot.get() != contract_binary =>
                {
                    return Err(AcceptanceError::Invalid(
                        "native fault-contract expectations must bind one instrumented binary per OS"
                            .into(),
                    ));
                }
                std::collections::btree_map::Entry::Occupied(_) => {}
            }
        }
    }
    Ok(())
}

impl AcceptanceManifest {
    pub fn parse_canonical(bytes: &[u8]) -> Result<Self, AcceptanceError> {
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(AcceptanceError::Invalid(
                "expected manifest exceeds its bounded size".into(),
            ));
        }
        let value: serde_json::Value = serde_json::from_slice(bytes)
            .map_err(|error| AcceptanceError::Json(error.to_string()))?;
        let canonical = kio_core::cas::canonical_json_bytes(&value)
            .map_err(|error| AcceptanceError::Json(error.to_string()))?;
        if canonical != bytes {
            return Err(AcceptanceError::Invalid(
                "expected manifest JSON is not canonical".into(),
            ));
        }
        let manifest = serde_json::from_value::<Self>(value)
            .map_err(|error| AcceptanceError::Json(error.to_string()))?;
        if manifest.schema != ACCEPTANCE_MANIFEST_SCHEMA {
            return Err(AcceptanceError::Invalid(
                "expected manifest schema is invalid".into(),
            ));
        }
        AcceptancePlan::new(manifest.expected.clone())?;
        Ok(manifest)
    }

    pub fn plan(&self) -> Result<AcceptancePlan, AcceptanceError> {
        AcceptancePlan::new(self.expected.clone())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, AcceptanceError> {
        if self.schema != ACCEPTANCE_MANIFEST_SCHEMA {
            return Err(AcceptanceError::Invalid(
                "expected manifest schema is invalid".into(),
            ));
        }
        self.plan()?;
        let value =
            serde_json::to_value(self).map_err(|error| AcceptanceError::Json(error.to_string()))?;
        let bytes = kio_core::cas::canonical_json_bytes(&value)
            .map_err(|error| AcceptanceError::Json(error.to_string()))?;
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(AcceptanceError::Invalid(
                "expected manifest exceeds its bounded size".into(),
            ));
        }
        Ok(bytes)
    }
}

/// Assemble the final fixed matrix from predeclared, case-specific
/// expectations. Receipt results are deliberately not an input to this API.
pub fn assemble_expected_cases(
    candidate_sha: &str,
    expected: Vec<ExpectedReceipt>,
) -> Result<AcceptanceManifest, AcceptanceError> {
    if !is_lower_hex(candidate_sha, 40) {
        return Err(AcceptanceError::Invalid(
            "assembled expected-case candidate SHA is invalid".into(),
        ));
    }
    let plan = AcceptancePlan::new(expected)?;
    if plan
        .expected()
        .iter()
        .any(|entry| entry.candidate.candidate_sha != candidate_sha)
    {
        return Err(AcceptanceError::Invalid(
            "assembled expected cases do not bind the declared candidate SHA".into(),
        ));
    }
    Ok(AcceptanceManifest {
        schema: ACCEPTANCE_MANIFEST_SCHEMA.into(),
        expected: plan.expected,
    })
}

impl AcceptanceBindingInput {
    pub fn parse_canonical(bytes: &[u8]) -> Result<Self, AcceptanceError> {
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(AcceptanceError::Invalid(
                "acceptance binding input exceeds its bounded size".into(),
            ));
        }
        let value: serde_json::Value = serde_json::from_slice(bytes)
            .map_err(|error| AcceptanceError::Json(error.to_string()))?;
        let canonical = kio_core::cas::canonical_json_bytes(&value)
            .map_err(|error| AcceptanceError::Json(error.to_string()))?;
        if canonical != bytes {
            return Err(AcceptanceError::Invalid(
                "acceptance binding input JSON is not canonical".into(),
            ));
        }
        let binding = serde_json::from_value::<Self>(value)
            .map_err(|error| AcceptanceError::Json(error.to_string()))?;
        binding.validate()?;
        Ok(binding)
    }

    pub fn expected_manifest(&self) -> Result<AcceptanceManifest, AcceptanceError> {
        self.validate()?;
        let target_by_os = self
            .targets
            .iter()
            .map(|target| (target.os, target))
            .collect::<BTreeMap<_, _>>();
        let execution_by_requirement = self
            .executions
            .iter()
            .map(|execution| (execution.requirement.clone(), execution))
            .collect::<BTreeMap<_, _>>();
        let expected = fixed_v1_requirements()
            .into_iter()
            .map(|requirement| {
                let target = target_by_os
                    .get(&requirement.os)
                    .expect("validated every native target");
                let execution = execution_by_requirement
                    .get(&requirement)
                    .expect("validated every fixed requirement");
                ExpectedReceipt {
                    requirement,
                    candidate: CandidateBinding {
                        candidate_sha: self.candidate_sha.clone(),
                        target: target.target.clone(),
                        archive_sha256: target.archive_sha256.clone(),
                        binary_sha256: target.binary_sha256.clone(),
                        version: target.version.clone(),
                    },
                    fixture: execution.fixture.clone(),
                    workflow: execution.workflow.clone(),
                    evaluator_sha256: execution.evaluator_sha256.clone(),
                    service_identity_sha256: execution.service_identity_sha256.clone(),
                    contract_binary_sha256: execution.contract_binary_sha256.clone(),
                }
            })
            .collect();
        let manifest = AcceptanceManifest {
            schema: ACCEPTANCE_MANIFEST_SCHEMA.into(),
            expected,
        };
        manifest.plan()?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<(), AcceptanceError> {
        if self.schema != ACCEPTANCE_BINDING_SCHEMA || !is_lower_hex(&self.candidate_sha, 40) {
            return Err(AcceptanceError::Invalid(
                "acceptance binding schema or candidate SHA is invalid".into(),
            ));
        }
        if self.targets.len() != 3 {
            return Err(AcceptanceError::Invalid(
                "acceptance binding must include exactly three native targets".into(),
            ));
        }
        let target_os = self
            .targets
            .iter()
            .map(|target| target.os)
            .collect::<BTreeSet<_>>();
        if target_os != BTreeSet::from([NativeOs::Linux, NativeOs::Macos, NativeOs::Windows]) {
            return Err(AcceptanceError::Invalid(
                "acceptance binding targets must be Linux, Macos, and Windows exactly once".into(),
            ));
        }
        for target in &self.targets {
            let candidate = CandidateBinding {
                candidate_sha: self.candidate_sha.clone(),
                target: target.target.clone(),
                archive_sha256: target.archive_sha256.clone(),
                binary_sha256: target.binary_sha256.clone(),
                version: target.version.clone(),
            };
            validate_candidate(&candidate)?;
            if !target_matches_os(&target.target, target.os) {
                return Err(AcceptanceError::Invalid(
                    "acceptance target triple does not match its declared OS".into(),
                ));
            }
        }
        let requirements = self
            .executions
            .iter()
            .map(|execution| execution.requirement.clone())
            .collect::<BTreeSet<_>>();
        if requirements.len() != self.executions.len() {
            return Err(AcceptanceError::Invalid(
                "acceptance execution bindings contain a duplicate requirement".into(),
            ));
        }
        if requirements != fixed_v1_requirements() {
            return Err(AcceptanceError::Invalid(
                "acceptance execution bindings must contain every fixed v1 requirement exactly once"
                    .into(),
            ));
        }
        for execution in &self.executions {
            validate_requirement(&execution.requirement)?;
            validate_fixture(&execution.fixture)?;
            validate_workflow(&execution.workflow, &self.candidate_sha)?;
            validate_evaluator_sha256(&execution.evaluator_sha256)?;
            validate_service_identity_hash(
                &execution.requirement,
                execution.service_identity_sha256.as_deref(),
            )?;
            validate_contract_binary(
                &execution.requirement,
                execution.contract_binary_sha256.as_deref(),
            )?;
        }
        Ok(())
    }
}

pub fn read_acceptance_binding(path: &Path) -> Result<AcceptanceBindingInput, AcceptanceError> {
    let bytes = bounded_regular_bytes(path, MAX_MANIFEST_BYTES)
        .map_err(|error| AcceptanceError::Invalid(error.to_string()))?;
    AcceptanceBindingInput::parse_canonical(&bytes)
}

pub fn write_expected_manifest_create_only(
    path: &Path,
    manifest: &AcceptanceManifest,
) -> Result<(), AcceptanceError> {
    let bytes = manifest.canonical_bytes()?;
    if path.exists() {
        return Err(AcceptanceError::Invalid(
            "expected manifest output must be create-only".into(),
        ));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(io_error)?;
    file.write_all(&bytes).map_err(io_error)?;
    file.sync_all().map_err(io_error)
}

/// Write one canonical, create-only expected receipt for a case workflow.
/// Unlike an [`AcceptanceManifest`], this does not require bindings for the
/// other workflows that have not run yet.
pub fn write_expected_receipt_create_only(
    path: &Path,
    expected: &ExpectedReceipt,
) -> Result<(), AcceptanceError> {
    validate_expected(expected)?;
    let value =
        serde_json::to_value(expected).map_err(|error| AcceptanceError::Json(error.to_string()))?;
    let bytes = kio_core::cas::canonical_json_bytes(&value)
        .map_err(|error| AcceptanceError::Json(error.to_string()))?;
    if bytes.len() as u64 > MAX_EXPECTED_CASE_BYTES {
        return Err(AcceptanceError::Invalid(
            "expected receipt exceeds its bounded size".into(),
        ));
    }
    if path.exists() {
        return Err(AcceptanceError::Invalid(
            "expected receipt output must be create-only".into(),
        ));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(io_error)?;
    file.write_all(&bytes).map_err(io_error)?;
    file.sync_all().map_err(io_error)
}

pub fn read_expected_manifest(path: &Path) -> Result<AcceptanceManifest, AcceptanceError> {
    let bytes = bounded_regular_bytes(path, MAX_MANIFEST_BYTES)
        .map_err(|error| AcceptanceError::Invalid(error.to_string()))?;
    AcceptanceManifest::parse_canonical(&bytes)
}

/// Read one canonical expected receipt prepared for a single case workflow.
pub fn read_expected_receipt(path: &Path) -> Result<ExpectedReceipt, AcceptanceError> {
    let bytes = bounded_regular_bytes(path, MAX_EXPECTED_CASE_BYTES)
        .map_err(|error| AcceptanceError::Invalid(error.to_string()))?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| AcceptanceError::Json(error.to_string()))?;
    let canonical = kio_core::cas::canonical_json_bytes(&value)
        .map_err(|error| AcceptanceError::Json(error.to_string()))?;
    if canonical != bytes {
        return Err(AcceptanceError::Invalid(
            "expected receipt JSON is not canonical".into(),
        ));
    }
    let expected = serde_json::from_value::<ExpectedReceipt>(value)
        .map_err(|error| AcceptanceError::Json(error.to_string()))?;
    validate_expected(&expected)?;
    Ok(expected)
}

pub fn read_fixture_bundle(root: &Path) -> Result<FixtureBundleManifest, AcceptanceError> {
    let metadata = fs::symlink_metadata(root).map_err(io_error)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AcceptanceError::Invalid(
            "fixture bundle must be a real directory".into(),
        ));
    }
    let manifest_path = root.join(FIXTURE_BUNDLE_MANIFEST);
    let bytes = bounded_regular_bytes(&manifest_path, MAX_RECEIPT_BYTES)?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| AcceptanceError::Json(error.to_string()))?;
    if kio_core::cas::canonical_json_bytes(&value)
        .map_err(|error| AcceptanceError::Json(error.to_string()))?
        != bytes
    {
        return Err(AcceptanceError::Invalid(
            "fixture bundle manifest is not canonical".into(),
        ));
    }
    let manifest = serde_json::from_value::<FixtureBundleManifest>(value)
        .map_err(|error| AcceptanceError::Json(error.to_string()))?;
    if manifest.schema != FIXTURE_BUNDLE_SCHEMA
        || manifest.scopes.len() < 2
        || manifest.scopes.len() > 8
        || manifest.files.is_empty()
        || manifest.files.len() > 64
        || manifest.queries.is_empty()
        || manifest.queries.len() > 64
    {
        return Err(AcceptanceError::Invalid(
            "fixture bundle manifest has invalid bounds".into(),
        ));
    }
    let scopes = manifest.scopes.iter().collect::<BTreeSet<_>>();
    if scopes.len() != manifest.scopes.len()
        || manifest
            .scopes
            .iter()
            .any(|scope| !is_fixture_relative_path(scope))
        || (manifest.requirements.require_vector
            && manifest
                .requirements
                .deterministic_embedding_runtime
                .as_deref()
                .filter(|value| !value.is_empty())
                .is_none())
    {
        return Err(AcceptanceError::Invalid(
            "fixture bundle scope/runtime is invalid".into(),
        ));
    }
    let mut paths = BTreeSet::new();
    let mut previous_file = None;
    for file in &manifest.files {
        let order = (file.scope.as_str(), file.path.as_str());
        if previous_file.is_some_and(|previous| previous >= order) {
            return Err(AcceptanceError::Invalid(
                "fixture bundle file entries must be strictly sorted by scope and path".into(),
            ));
        }
        previous_file = Some(order);
        let key = format!("{}/{}", file.scope, file.path);
        if !scopes.contains(&file.scope)
            || !is_fixture_relative_path(&file.path)
            || !paths.insert(key.clone())
            || !is_lower_hex(&file.sha256, 64)
        {
            return Err(AcceptanceError::Invalid(
                "fixture bundle file entry is invalid".into(),
            ));
        }
        if sha256_regular_file(&root.join(&key), MAX_FIXTURE_BYTES)? != file.sha256 {
            return Err(AcceptanceError::Invalid(
                "fixture bundle file digest differs".into(),
            ));
        }
    }
    let mut previous_query = None;
    for query in &manifest.queries {
        let order = (
            query.scope.as_str(),
            query.query.as_str(),
            query.expect_path.as_str(),
        );
        if previous_query.is_some_and(|previous| previous >= order) {
            return Err(AcceptanceError::Invalid(
                "fixture bundle query entries must be strictly sorted".into(),
            ));
        }
        previous_query = Some(order);
        if query.query.is_empty()
            || !scopes.contains(&query.scope)
            || !paths.contains(&format!("{}/{}", query.scope, query.expect_path))
        {
            return Err(AcceptanceError::Invalid(
                "fixture bundle query is invalid".into(),
            ));
        }
        if let Some(image) = &query.expect_image
            && (!is_image_fixture(&image.path)
                || !manifest.files.iter().any(|file| {
                    file.scope == image.scope
                        && file.path == image.path
                        && file.sha256 == image.sha256
                }))
        {
            return Err(AcceptanceError::Invalid(
                "fixture image expectation must match a listed image scope, path, and digest"
                    .into(),
            ));
        }
    }
    let mut previous_mutation = None;
    for mutation in &manifest.mutations {
        let order = (mutation.scope.as_str(), mutation.path.as_str());
        if previous_mutation.is_some_and(|previous| previous >= order)
            || !scopes.contains(&mutation.scope)
            || !paths.contains(&format!("{}/{}", mutation.scope, mutation.path))
            || mutation.replacement_utf8.is_empty()
        {
            return Err(AcceptanceError::Invalid(
                "fixture bundle mutation is invalid".into(),
            ));
        }
        previous_mutation = Some(order);
    }
    Ok(manifest)
}

pub fn fixture_bundle_digest(root: &Path) -> Result<String, AcceptanceError> {
    let manifest = read_fixture_bundle(root)?;
    let mut digest = Sha256::new();
    let bytes = serde_json::to_value(&manifest)
        .map_err(|error| AcceptanceError::Json(error.to_string()))?;
    digest.update(
        kio_core::cas::canonical_json_bytes(&bytes)
            .map_err(|error| AcceptanceError::Json(error.to_string()))?,
    );
    for file in &manifest.files {
        digest.update(file.scope.as_bytes());
        digest.update([0]);
        digest.update(file.path.as_bytes());
        digest.update([0]);
        digest.update(bounded_regular_bytes(
            &root.join(&file.scope).join(&file.path),
            MAX_FIXTURE_BYTES,
        )?);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Copy only manifest-listed, already-digested inputs into a create-only
/// isolated root. The returned paths are the scope roots for CLI execution.
pub fn materialize_fixture_bundle(
    source: &Path,
    destination: &Path,
) -> Result<(FixtureBundleManifest, BTreeMap<String, PathBuf>), AcceptanceError> {
    let manifest = read_fixture_bundle(source)?;
    if destination.exists() {
        return Err(AcceptanceError::Invalid(
            "fixture destination must be create-only".into(),
        ));
    }
    fs::create_dir_all(destination).map_err(io_error)?;
    let mut scopes = BTreeMap::new();
    for scope in &manifest.scopes {
        let target = destination.join(scope);
        fs::create_dir_all(&target).map_err(io_error)?;
        scopes.insert(scope.clone(), target);
    }
    for file in &manifest.files {
        let source_file = source.join(&file.scope).join(&file.path);
        let scope_root = scopes.get(&file.scope).ok_or_else(|| {
            AcceptanceError::Invalid("fixture scope disappeared during materialization".into())
        })?;
        let target = scope_root.join(&file.path);
        let parent = target
            .parent()
            .ok_or_else(|| AcceptanceError::Invalid("fixture target has no parent".into()))?;
        fs::create_dir_all(parent).map_err(io_error)?;
        let bytes = bounded_regular_bytes(&source_file, MAX_FIXTURE_BYTES)?;
        if sha256_bytes(&bytes) != file.sha256 {
            return Err(AcceptanceError::Invalid(
                "fixture changed before materialization".into(),
            ));
        }
        fs::write(target, bytes).map_err(io_error)?;
    }
    Ok((manifest, scopes))
}

/// Exercise the extracted binary's basic index/search/open path. Opening the
/// resulting evidence must preserve both scope and private device state.
pub fn run_a01(options: &A01Options) -> Result<AcceptanceReceipt, AcceptanceError> {
    let requirement = &options.expected.requirement;
    if requirement.case != AcceptanceCase::A01
        || requirement.lane != AcceptanceLane::NativeContract
        || requirement.subcase != AcceptanceSubcase::Core
    {
        return Err(AcceptanceError::Invalid(
            "A01 runner requires the A01/native_contract/core requirement".into(),
        ));
    }
    validate_expected(&options.expected)?;
    current_evaluator_sha256_matches(&options.expected.evaluator_sha256)?;
    let binary_hash = sha256_regular_file(&options.binary, MAX_BINARY_BYTES)?;
    if binary_hash != options.expected.candidate.binary_sha256 {
        return Err(AcceptanceError::Invalid(
            "binary hash differs from candidate binding".into(),
        ));
    }
    let fixture_bytes = bounded_regular_bytes(&options.fixture, MAX_FIXTURE_BYTES)?;
    let fixture_hash = sha256_bytes(&fixture_bytes);
    if fixture_hash != options.expected.fixture.sha256 {
        return Err(AcceptanceError::Invalid(
            "fixture hash differs from expected binding".into(),
        ));
    }
    if options.work_dir.exists() || options.receipt.exists() {
        return Err(AcceptanceError::Invalid(
            "A01 work directory and receipt must be create-only".into(),
        ));
    }
    fs::create_dir_all(&options.work_dir).map_err(io_error)?;
    let isolated = options.work_dir.join("isolated");
    let scope = isolated.join("scope");
    for directory in [
        &scope,
        &isolated.join("xdg-config"),
        &isolated.join("xdg-data"),
        &isolated.join("xdg-cache"),
        &isolated.join("tmp"),
    ] {
        fs::create_dir_all(directory).map_err(io_error)?;
    }
    fs::write(scope.join("fixture.txt"), &fixture_bytes).map_err(io_error)?;
    let version = run_a01_command(&options.binary, &isolated, None, &["--version"])?;
    if !String::from_utf8_lossy(&version).contains(&options.expected.candidate.version) {
        return Err(AcceptanceError::Command(
            "A01 binary version differs from candidate binding".into(),
        ));
    }
    let scope_text = scope
        .to_str()
        .ok_or_else(|| AcceptanceError::Invalid("A01 scope is not UTF-8".into()))?;
    run_a01_command(
        &options.binary,
        &isolated,
        None,
        &["--json", "init", scope_text],
    )?;
    let indexed = run_a01_command(
        &options.binary,
        &isolated,
        Some(&scope),
        &["--json", "index", "--offline"],
    )?;
    let indexed: serde_json::Value = serde_json::from_slice(&indexed)
        .map_err(|error| AcceptanceError::Json(error.to_string()))?;
    let head = required_json_string(&indexed, "/commit_hash", "A01 offline index")?;
    let search = run_a01_command(
        &options.binary,
        &isolated,
        Some(&scope),
        &["--json", "search", "fixture", "--mode", "text", "--offline"],
    )?;
    let value: serde_json::Value = serde_json::from_slice(&search)
        .map_err(|error| AcceptanceError::Json(error.to_string()))?;
    let pointer = value
        .pointer("/results/0/evidence_pointer")
        .or_else(|| value.pointer("/result/0/evidence_pointer"))
        .ok_or_else(|| {
            AcceptanceError::Command("A01 search returned no evidence pointer".into())
        })?;
    let pointer =
        serde_json::to_string(pointer).map_err(|error| AcceptanceError::Json(error.to_string()))?;
    let before_authority = readonly_authority_fingerprint(&scope, &isolated)?;
    let before_locks = lock_paths(&scope, &isolated)?;
    let ledger = ledger_artifacts_fingerprint(&isolated.join("xdg-data/kio"))?;
    if ledger.values().any(|value| value != "absent") {
        return Err(AcceptanceError::Command(
            "A01 fixture unexpectedly initialized a ledger".into(),
        ));
    }
    for args in [
        vec!["--json", "status"],
        vec!["--json", "adapter", "status"],
        vec!["--json", "search", "fixture", "--mode", "text", "--offline"],
        vec!["--json", "restore", &head, "--preview"],
    ] {
        run_a01_command(&options.binary, &isolated, Some(&scope), &args)?;
    }
    run_a01_command(
        &options.binary,
        &isolated,
        Some(&scope),
        &["--json", "open", &pointer],
    )?;
    if readonly_authority_fingerprint(&scope, &isolated)? != before_authority
        || lock_paths(&scope, &isolated)? != before_locks
        || ledger_artifacts_fingerprint(&isolated.join("xdg-data/kio"))? != ledger
    {
        return Err(AcceptanceError::Command(
            "A01 read-only matrix changed authority, knowledge, or ledger state".into(),
        ));
    }
    if sha256_regular_file(&options.binary, MAX_BINARY_BYTES)?
        != options.expected.candidate.binary_sha256
    {
        return Err(AcceptanceError::Invalid(
            "binary changed while A01 was executing".into(),
        ));
    }

    let receipt = AcceptanceReceipt {
        schema: SCHEMA.into(),
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

fn lock_paths(scope: &Path, isolated: &Path) -> Result<BTreeSet<PathBuf>, AcceptanceError> {
    let mut locks = BTreeSet::new();
    for root in [scope.join(".kio"), isolated.join("xdg-data/kio")] {
        for (path, _) in directory_fingerprint(&root)? {
            if path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains("lock"))
            {
                locks.insert(root.join(path));
            }
        }
    }
    Ok(locks)
}

/// Exercise text, vector, hybrid, cursor, history, and image retrieval against
/// the release binary's non-network deterministic evaluator. This is native
/// contract evidence, never provider or model-quality evidence.
pub fn run_a05(options: &A05Options) -> Result<AcceptanceReceipt, AcceptanceError> {
    let requirement = &options.expected.requirement;
    if requirement.case != AcceptanceCase::A05
        || requirement.lane != AcceptanceLane::NativeContract
        || requirement.subcase != AcceptanceSubcase::Core
    {
        return Err(AcceptanceError::Invalid(
            "A05 runner requires the A05/native_contract/core requirement".into(),
        ));
    }
    validate_expected(&options.expected)?;
    current_evaluator_sha256_matches(&options.expected.evaluator_sha256)?;
    if sha256_regular_file(&options.binary, MAX_BINARY_BYTES)?
        != options.expected.candidate.binary_sha256
    {
        return Err(AcceptanceError::Invalid(
            "binary hash differs from candidate binding".into(),
        ));
    }
    let fixture_digest = fixture_bundle_digest(&options.fixture)?;
    if fixture_digest != options.expected.fixture.sha256 {
        return Err(AcceptanceError::Invalid(
            "fixture bundle digest differs from expected binding".into(),
        ));
    }
    let fixture = read_fixture_bundle(&options.fixture)?;
    let deterministic_runtime = fixture
        .requirements
        .deterministic_embedding_runtime
        .as_deref()
        .filter(|runtime| !runtime.is_empty())
        .ok_or_else(|| {
            AcceptanceError::Invalid(
                "A05 fixture requires a deterministic embedding runtime declaration".into(),
            )
        })?;
    if deterministic_runtime != "scale-v3" {
        return Err(AcceptanceError::Invalid(
            "A05 fixture must select the shipped scale-v3 deterministic runtime".into(),
        ));
    }
    if !fixture.requirements.require_image
        || !fixture.requirements.require_vector
        || !fixture
            .files
            .iter()
            .any(|file| is_image_fixture(&file.path))
    {
        return Err(AcceptanceError::Invalid(
            "A05 fixture must require vector and image search and list an image input".into(),
        ));
    }
    if fixture.mutations.is_empty() {
        return Err(AcceptanceError::Invalid(
            "A05 fixture must declare a history mutation".into(),
        ));
    }
    if options.work_dir.exists() || options.receipt.exists() {
        return Err(AcceptanceError::Invalid(
            "A05 work directory and receipt must be create-only".into(),
        ));
    }
    fs::create_dir_all(&options.work_dir).map_err(io_error)?;
    let isolated = options.work_dir.join("isolated");
    for directory in [
        &isolated.join("xdg-config"),
        &isolated.join("xdg-data"),
        &isolated.join("xdg-cache"),
        &isolated.join("tmp"),
    ] {
        fs::create_dir_all(directory).map_err(io_error)?;
    }
    let (fixture, scopes) = materialize_fixture_bundle(&options.fixture, &isolated.join("scopes"))?;
    let mut commits = BTreeMap::new();
    for (name, scope) in &scopes {
        let scope_text = scope
            .to_str()
            .ok_or_else(|| AcceptanceError::Invalid("A05 scope is not UTF-8".into()))?;
        run_a05_command(
            &options.binary,
            &isolated,
            None,
            &["--json", "init", scope_text],
            deterministic_runtime,
        )?;
        let indexed = command_json_with_runtime(
            &options.binary,
            &isolated,
            scope,
            &["--json", "index", "--offline"],
            deterministic_runtime,
        )?;
        commits.insert(
            name.clone(),
            required_json_string(&indexed, "/commit_hash", "A05 initial index")?,
        );
    }
    let first_scope = scopes
        .values()
        .next()
        .ok_or_else(|| AcceptanceError::Invalid("A05 fixture has no materialized scopes".into()))?;
    for query in &fixture.queries {
        let response = command_json_with_runtime(
            &options.binary,
            &isolated,
            first_scope,
            &[
                "--json",
                "search",
                &query.query,
                "--all-scopes",
                "--mode",
                "text",
                "--offline",
            ],
            deterministic_runtime,
        )?;
        ensure_search_path(&response, &query.expect_path, "A05 multi-scope text search")?;
        for mode in ["vector", "hybrid"] {
            let response = command_json_with_runtime(
                &options.binary,
                &isolated,
                first_scope,
                &[
                    "--json",
                    "search",
                    &query.query,
                    "--all-scopes",
                    "--mode",
                    mode,
                    "--offline",
                ],
                deterministic_runtime,
            )?;
            if response
                .pointer("/resolved_mode")
                .and_then(serde_json::Value::as_str)
                != Some(mode)
            {
                return Err(AcceptanceError::Command(format!(
                    "A05 {mode} search did not resolve to its requested mode"
                )));
            }
            ensure_search_path(&response, &query.expect_path, &format!("A05 {mode} search"))?;
        }
    }
    let cursor_query = fixture.queries.first().expect("validated nonempty queries");
    let page_one = command_json_with_runtime(
        &options.binary,
        &isolated,
        first_scope,
        &[
            "--json",
            "search",
            &cursor_query.query,
            "--all-scopes",
            "--mode",
            "text",
            "--offline",
            "--limit",
            "1",
        ],
        deterministic_runtime,
    )?;
    let cursor = required_json_string(&page_one, "/paging/next_cursor", "A05 cursor page one")?;
    let page_two = command_json_with_runtime(
        &options.binary,
        &isolated,
        first_scope,
        &[
            "--json",
            "search",
            &cursor_query.query,
            "--all-scopes",
            "--mode",
            "text",
            "--offline",
            "--limit",
            "1",
            "--cursor",
            &cursor,
        ],
        deterministic_runtime,
    )?;
    if page_one.pointer("/results") == page_two.pointer("/results") {
        return Err(AcceptanceError::Command(
            "A05 cursor page two did not advance".into(),
        ));
    }
    let mutation = fixture
        .mutations
        .first()
        .expect("validated nonempty mutations");
    let scope = scopes
        .get(&mutation.scope)
        .ok_or_else(|| AcceptanceError::Invalid("A05 mutation scope is unavailable".into()))?;
    fs::write(
        scope.join(&mutation.path),
        mutation.replacement_utf8.as_bytes(),
    )
    .map_err(io_error)?;
    command_json_with_runtime(
        &options.binary,
        &isolated,
        scope,
        &["--json", "index", "--offline"],
        deterministic_runtime,
    )?;
    let historical_query = fixture
        .queries
        .iter()
        .find(|query| query.scope == mutation.scope && query.expect_path == mutation.path)
        .ok_or_else(|| {
            AcceptanceError::Invalid(
                "A05 mutation must be paired with a query for its original path".into(),
            )
        })?;
    let prior = commits.get(&mutation.scope).ok_or_else(|| {
        AcceptanceError::Invalid("A05 initial mutation commit is unavailable".into())
    })?;
    let history = command_json_with_runtime(
        &options.binary,
        &isolated,
        scope,
        &[
            "--json",
            "search",
            &historical_query.query,
            "--mode",
            "text",
            "--offline",
            "--scope",
            scope
                .to_str()
                .ok_or_else(|| AcceptanceError::Invalid("A05 history scope is not UTF-8".into()))?,
            "--at",
            prior,
        ],
        deterministic_runtime,
    )?;
    ensure_search_path(
        &history,
        &historical_query.expect_path,
        "A05 historical text search",
    )?;
    if sha256_regular_file(&options.binary, MAX_BINARY_BYTES)?
        != options.expected.candidate.binary_sha256
    {
        return Err(AcceptanceError::Invalid(
            "binary changed while A05 was executing".into(),
        ));
    }

    let image_query = fixture
        .queries
        .iter()
        .find(|query| query.expect_image.is_some())
        .ok_or_else(|| {
            AcceptanceError::Invalid(
                "A05 fixture must declare a query that expects an image result".into(),
            )
        })?;
    let image = command_json_with_runtime(
        &options.binary,
        &isolated,
        first_scope,
        &[
            "--json",
            "search",
            &image_query.query,
            "--all-scopes",
            "--mode",
            "hybrid",
            "--offline",
            "--limit",
            "20",
        ],
        deterministic_runtime,
    )?;
    let expected_image = image_query
        .expect_image
        .as_ref()
        .expect("selected image query");
    ensure_image_result(
        &image,
        expected_image,
        &scopes[&expected_image.scope],
        &options.binary,
        &isolated,
        deterministic_runtime,
        "A05 image retrieval",
    )?;
    let receipt = AcceptanceReceipt {
        schema: SCHEMA.into(),
        requirement: options.expected.requirement.clone(),
        candidate: options.expected.candidate.clone(),
        fixture: options.expected.fixture.clone(),
        workflow: options.expected.workflow.clone(),
        evaluator_sha256: current_evaluator_sha256_matches(&options.expected.evaluator_sha256)?,
        service_identity_sha256: options.expected.service_identity_sha256.clone(),
        contract_binary_sha256: options.expected.contract_binary_sha256.clone(),
        runtime: runtime_from_scope(first_scope)?,
        passed: true,
    };
    write_receipt_create_only(&options.receipt, &receipt)?;
    Ok(receipt)
}

/// Rebuild every scoped SQLite projection from the same immutable fixture and
/// require stable text/vector/hybrid result rows before issuing A06 evidence.
pub fn run_a06(options: &A06Options) -> Result<AcceptanceReceipt, AcceptanceError> {
    let requirement = &options.expected.requirement;
    if requirement.case != AcceptanceCase::A06
        || requirement.lane != AcceptanceLane::NativeContract
        || requirement.subcase != AcceptanceSubcase::Core
    {
        return Err(AcceptanceError::Invalid(
            "A06 runner requires the A06/native_contract/core requirement".into(),
        ));
    }
    validate_expected(&options.expected)?;
    current_evaluator_sha256_matches(&options.expected.evaluator_sha256)?;
    if sha256_regular_file(&options.binary, MAX_BINARY_BYTES)?
        != options.expected.candidate.binary_sha256
        || fixture_bundle_digest(&options.fixture)? != options.expected.fixture.sha256
    {
        return Err(AcceptanceError::Invalid(
            "A06 candidate binary or fixture binding differs".into(),
        ));
    }
    let source = read_fixture_bundle(&options.fixture)?;
    let runtime = source
        .requirements
        .deterministic_embedding_runtime
        .as_deref()
        .filter(|value| *value == "scale-v3")
        .ok_or_else(|| AcceptanceError::Invalid("A06 requires scale-v3 runtime".into()))?;
    if !source.requirements.require_vector
        || !source.requirements.require_image
        || !source
            .queries
            .iter()
            .any(|query| query.expect_image.is_some())
    {
        return Err(AcceptanceError::Invalid(
            "A06 fixture must require vector/image retrieval and an image query".into(),
        ));
    }
    if options.work_dir.exists() || options.receipt.exists() {
        return Err(AcceptanceError::Invalid(
            "A06 work directory and receipt must be create-only".into(),
        ));
    }
    fs::create_dir_all(&options.work_dir).map_err(io_error)?;
    let isolated = options.work_dir.join("isolated");
    for directory in [
        &isolated.join("xdg-config"),
        &isolated.join("xdg-data"),
        &isolated.join("xdg-cache"),
        &isolated.join("tmp"),
    ] {
        fs::create_dir_all(directory).map_err(io_error)?;
    }
    let (fixture, scopes) = materialize_fixture_bundle(&options.fixture, &isolated.join("scopes"))?;
    let mut initial_commits = BTreeMap::new();
    for (name, scope) in &scopes {
        let scope_text = scope
            .to_str()
            .ok_or_else(|| AcceptanceError::Invalid("A06 scope is not UTF-8".into()))?;
        run_a05_command(
            &options.binary,
            &isolated,
            None,
            &["--json", "init", scope_text],
            runtime,
        )?;
        let indexed = command_json_with_runtime(
            &options.binary,
            &isolated,
            scope,
            &["--json", "index", "--offline"],
            runtime,
        )?;
        initial_commits.insert(
            name.clone(),
            required_json_string(&indexed, "/commit_hash", "A06 initial index")?,
        );
    }
    let mutation = fixture.mutations.first().ok_or_else(|| {
        AcceptanceError::Invalid("A06 fixture must declare a history mutation".into())
    })?;
    let mutation_scope = scopes
        .get(&mutation.scope)
        .ok_or_else(|| AcceptanceError::Invalid("A06 mutation scope is unavailable".into()))?;
    fs::write(
        mutation_scope.join(&mutation.path),
        mutation.replacement_utf8.as_bytes(),
    )
    .map_err(io_error)?;
    command_json_with_runtime(
        &options.binary,
        &isolated,
        mutation_scope,
        &["--json", "index", "--offline"],
        runtime,
    )?;
    let first_scope = scopes
        .values()
        .next()
        .ok_or_else(|| AcceptanceError::Invalid("A06 fixture has no materialized scopes".into()))?;
    let mut before = BTreeMap::new();
    for query in &fixture.queries {
        for mode in ["text", "vector", "hybrid"] {
            let response = command_json_with_runtime(
                &options.binary,
                &isolated,
                first_scope,
                &[
                    "--json",
                    "search",
                    &query.query,
                    "--all-scopes",
                    "--mode",
                    mode,
                    "--offline",
                ],
                runtime,
            )?;
            ensure_search_path(&response, &query.expect_path, "A06 pre-rebuild search")?;
            if mode == "hybrid"
                && let Some(image) = &query.expect_image
            {
                ensure_image_result(
                    &response,
                    image,
                    &scopes[&image.scope],
                    &options.binary,
                    &isolated,
                    runtime,
                    "A06 pre-rebuild image retrieval",
                )?;
            }
            before.insert(
                (query.scope.clone(), query.query.clone(), mode.to_owned()),
                stable_result_rows(&response)?,
            );
        }
    }
    let historical_query = fixture
        .queries
        .iter()
        .find(|query| query.scope == mutation.scope && query.expect_path == mutation.path)
        .ok_or_else(|| AcceptanceError::Invalid("A06 mutation has no matching query".into()))?;
    let historical_commit = initial_commits.get(&mutation.scope).ok_or_else(|| {
        AcceptanceError::Invalid("A06 initial mutation commit is unavailable".into())
    })?;
    let historical_before = command_json_with_runtime(
        &options.binary,
        &isolated,
        mutation_scope,
        &[
            "--json",
            "search",
            &historical_query.query,
            "--mode",
            "text",
            "--offline",
            "--scope",
            mutation_scope
                .to_str()
                .ok_or_else(|| AcceptanceError::Invalid("A06 history scope is not UTF-8".into()))?,
            "--at",
            historical_commit,
        ],
        runtime,
    )?;
    ensure_search_path(
        &historical_before,
        &historical_query.expect_path,
        "A06 pre-rebuild historical search",
    )?;
    let historical_rows = stable_result_rows(&historical_before)?;
    for (index, scope) in scopes.values().enumerate() {
        let sqlite = scope.join(".kio/index/sqlite.db");
        if index == 0 {
            fs::remove_file(&sqlite).map_err(io_error)?;
        } else {
            fs::write(&sqlite, b"A06 corrupted SQLite projection\n").map_err(io_error)?;
        }
    }
    let aggregator = isolated.join("xdg-data/cache/kio/aggregator.sqlite");
    if aggregator.exists() {
        fs::remove_file(&aggregator).map_err(io_error)?;
    }
    for scope in scopes.values() {
        command_json_with_runtime(
            &options.binary,
            &isolated,
            scope,
            &["--json", "repair", "rebuild-db", "--offline"],
            runtime,
        )?;
    }
    command_json_with_runtime(
        &options.binary,
        &isolated,
        first_scope,
        &["--json", "repair", "replica"],
        runtime,
    )?;
    for query in &fixture.queries {
        for mode in ["text", "vector", "hybrid"] {
            let response = command_json_with_runtime(
                &options.binary,
                &isolated,
                first_scope,
                &[
                    "--json",
                    "search",
                    &query.query,
                    "--all-scopes",
                    "--mode",
                    mode,
                    "--offline",
                ],
                runtime,
            )?;
            let after = stable_result_rows(&response)?;
            let key = (query.scope.clone(), query.query.clone(), mode.to_owned());
            if before.get(&key) != Some(&after) {
                return Err(AcceptanceError::Command(format!(
                    "A06 {mode} search results changed after rebuild"
                )));
            }
            if mode == "hybrid"
                && let Some(image) = &query.expect_image
            {
                ensure_image_result(
                    &response,
                    image,
                    &scopes[&image.scope],
                    &options.binary,
                    &isolated,
                    runtime,
                    "A06 rebuilt image retrieval",
                )?;
            }
        }
    }
    let historical_after = command_json_with_runtime(
        &options.binary,
        &isolated,
        mutation_scope,
        &[
            "--json",
            "search",
            &historical_query.query,
            "--mode",
            "text",
            "--offline",
            "--scope",
            mutation_scope
                .to_str()
                .ok_or_else(|| AcceptanceError::Invalid("A06 history scope is not UTF-8".into()))?,
            "--at",
            historical_commit,
        ],
        runtime,
    )?;
    if stable_result_rows(&historical_after)? != historical_rows {
        return Err(AcceptanceError::Command(
            "A06 historical search results changed after rebuild".into(),
        ));
    }
    ensure_search_path(
        &historical_after,
        &historical_query.expect_path,
        "A06 rebuilt historical search",
    )?;
    assert_a06_concurrent_cli_outcomes(
        &options.binary,
        &isolated,
        first_scope,
        &fixture.queries[0].query,
        runtime,
    )?;
    if sha256_regular_file(&options.binary, MAX_BINARY_BYTES)?
        != options.expected.candidate.binary_sha256
    {
        return Err(AcceptanceError::Invalid(
            "binary changed while A06 was executing".into(),
        ));
    }
    let receipt = AcceptanceReceipt {
        schema: SCHEMA.into(),
        requirement: options.expected.requirement.clone(),
        candidate: options.expected.candidate.clone(),
        fixture: options.expected.fixture.clone(),
        workflow: options.expected.workflow.clone(),
        evaluator_sha256: current_evaluator_sha256_matches(&options.expected.evaluator_sha256)?,
        service_identity_sha256: options.expected.service_identity_sha256.clone(),
        contract_binary_sha256: options.expected.contract_binary_sha256.clone(),
        runtime: runtime_from_scope(first_scope)?,
        passed: true,
    };
    write_receipt_create_only(&options.receipt, &receipt)?;
    Ok(receipt)
}

fn stable_result_rows(response: &serde_json::Value) -> Result<serde_json::Value, AcceptanceError> {
    response
        .pointer("/results")
        .filter(|value| value.is_array())
        .cloned()
        .ok_or_else(|| AcceptanceError::Command("search returned no result rows".into()))
}

fn ensure_search_path(
    response: &serde_json::Value,
    expected_path: &str,
    context: &str,
) -> Result<(), AcceptanceError> {
    let found = response
        .pointer("/results")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|results| {
            results.iter().any(|result| {
                result
                    .pointer("/evidence_pointer/path_at_commit")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|path| path.ends_with(expected_path))
            })
        });
    if found {
        Ok(())
    } else {
        Err(AcceptanceError::Command(format!(
            "{context} returned no result for {expected_path}"
        )))
    }
}

fn ensure_image_result(
    response: &serde_json::Value,
    expected: &FixtureImageExpectation,
    scope: &Path,
    binary: &Path,
    isolated: &Path,
    runtime: &str,
    context: &str,
) -> Result<(), AcceptanceError> {
    let scope_id = kio_core::scope::Repository::open_read_only(scope)
        .and_then(|repo| repo.scope_identity())
        .map_err(|error| AcceptanceError::Command(format!("{context}: {error}")))?
        .scope_id;
    let uri = fixture_image_uri(response, expected, &scope_id, context)?;
    let opened =
        command_json_with_runtime(binary, isolated, scope, &["--json", "open", &uri], runtime)?;
    assert_opened_fixture_image(&opened, expected, context)
}

fn fixture_image_uri(
    response: &serde_json::Value,
    expected: &FixtureImageExpectation,
    scope_id: &str,
    context: &str,
) -> Result<String, AcceptanceError> {
    // Standalone ingestion preserves the original image bytes in image CAS;
    // its payload hash therefore equals the fixture raw hash. The image hit's
    // evidence pointer must cite that same standalone input, not another
    // document which happens to reference an image.
    let raw_hash = format!("sha256:{}", expected.sha256);
    let uri = format!("kio://{scope_id}/object/image/{raw_hash}");
    let found = response
        .get("results")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|results| {
            results.iter().any(|result| {
                result
                    .get("result_type")
                    .and_then(serde_json::Value::as_str)
                    == Some("image")
                    && result
                        .get("payload_uri")
                        .and_then(serde_json::Value::as_str)
                        == Some(uri.as_str())
                    && result
                        .pointer("/evidence_pointer/scope_id")
                        .and_then(serde_json::Value::as_str)
                        == Some(scope_id)
                    && result
                        .pointer("/evidence_pointer/path_at_commit")
                        .and_then(serde_json::Value::as_str)
                        == Some(expected.path.as_str())
                    && result
                        .pointer("/evidence_pointer/raw_hash")
                        .and_then(serde_json::Value::as_str)
                        == Some(raw_hash.as_str())
            })
        });
    if !found {
        return Err(AcceptanceError::Command(format!(
            "{context} returned no image bound to {}/{} and its immutable bytes",
            expected.scope, expected.path,
        )));
    }
    Ok(uri)
}

fn assert_opened_fixture_image(
    opened: &serde_json::Value,
    expected: &FixtureImageExpectation,
    context: &str,
) -> Result<(), AcceptanceError> {
    let path = opened
        .get("path")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            AcceptanceError::Command(format!("{context}: image open returned no path"))
        })?;
    if sha256_regular_file(Path::new(path), MAX_FIXTURE_BYTES)? != expected.sha256 {
        return Err(AcceptanceError::Command(format!(
            "{context}: opened image bytes differ from the immutable fixture",
        )));
    }
    Ok(())
}

fn is_image_fixture(path: &str) -> bool {
    path.rsplit_once('.')
        .is_some_and(|(_, extension)| matches!(extension, "png" | "jpg" | "jpeg" | "webp"))
}

/// Exercise an actual managed restore: a historical fixture is indexed, the
/// working file is changed and indexed again, then `restore --path` must make
/// a new child of that current HEAD and restore the original bytes.
pub fn run_a09(options: &A09Options) -> Result<AcceptanceReceipt, AcceptanceError> {
    let requirement = &options.expected.requirement;
    if requirement.case != AcceptanceCase::A09
        || requirement.lane != AcceptanceLane::NativeContract
        || requirement.subcase != AcceptanceSubcase::Core
    {
        return Err(AcceptanceError::Invalid(
            "A09 runner requires the A09/native_contract/core requirement".into(),
        ));
    }
    validate_expected(&options.expected)?;
    current_evaluator_sha256_matches(&options.expected.evaluator_sha256)?;
    if sha256_regular_file(&options.binary, MAX_BINARY_BYTES)?
        != options.expected.candidate.binary_sha256
    {
        return Err(AcceptanceError::Invalid(
            "binary hash differs from candidate binding".into(),
        ));
    }
    let fixture_bytes = bounded_regular_bytes(&options.fixture, MAX_FIXTURE_BYTES)?;
    if sha256_bytes(&fixture_bytes) != options.expected.fixture.sha256 {
        return Err(AcceptanceError::Invalid(
            "fixture hash differs from expected binding".into(),
        ));
    }
    if options.work_dir.exists() || options.receipt.exists() {
        return Err(AcceptanceError::Invalid(
            "A09 work directory and receipt must be create-only".into(),
        ));
    }
    fs::create_dir_all(&options.work_dir).map_err(io_error)?;
    let isolated = options.work_dir.join("isolated");
    let scope = isolated.join("scope");
    for directory in [
        &scope,
        &isolated.join("xdg-config"),
        &isolated.join("xdg-data"),
        &isolated.join("xdg-cache"),
        &isolated.join("tmp"),
    ] {
        fs::create_dir_all(directory).map_err(io_error)?;
    }
    let file = scope.join("fixture.txt");
    let selected = scope.join("selected.txt");
    let added = scope.join("added.txt");
    let unmanaged = scope.join("unmanaged.bin");
    fs::write(isolated.join(".kioignore"), b"scope/unmanaged.bin\n").map_err(io_error)?;
    fs::write(&file, &fixture_bytes).map_err(io_error)?;
    fs::write(&selected, b"acceptance selected original\n").map_err(io_error)?;
    fs::write(&unmanaged, b"unmanaged acceptance bytes\0\xff").map_err(io_error)?;
    let scope_text = scope
        .to_str()
        .ok_or_else(|| AcceptanceError::Invalid("A09 scope is not UTF-8".into()))?;
    run_a01_command(
        &options.binary,
        &isolated,
        None,
        &["--json", "init", scope_text],
    )?;
    let first = command_json(
        &options.binary,
        &isolated,
        &scope,
        &["--json", "index", "--offline"],
    )?;
    let source = required_json_string(&first, "/commit_hash", "first index")?;
    fs::write(&file, b"acceptance changed bytes\n").map_err(io_error)?;
    fs::write(&selected, b"acceptance selected changed\n").map_err(io_error)?;
    fs::write(&added, b"acceptance added bytes\n").map_err(io_error)?;
    let second = command_json(
        &options.binary,
        &isolated,
        &scope,
        &["--json", "index", "--offline"],
    )?;
    let current = required_json_string(&second, "/commit_hash", "second index")?;
    if source == current {
        return Err(AcceptanceError::Command(
            "A09 second index did not advance HEAD".into(),
        ));
    }
    let initialized_ledger = command_json(
        &options.binary,
        &isolated,
        &scope,
        &["--json", "ledger", "init"],
    )?;
    if initialized_ledger
        .get("status")
        .and_then(serde_json::Value::as_str)
        != Some("initialized")
    {
        return Err(AcceptanceError::Command(
            "A09 ledger init did not create a ready device ledger".into(),
        ));
    }
    let config_before = fs::read(scope.join(".kio/config.toml")).map_err(io_error)?;
    let approval_before = fs::read(scope.join(".kio/scope.json")).map_err(io_error)?;
    let ledger_before = ledger_artifacts_fingerprint(&isolated.join("xdg-data/kio"))?;
    require_present_ledger_artifacts(&ledger_before)?;
    let unmanaged_before = fs::read(&unmanaged).map_err(io_error)?;
    let path_injection = command_json_failure(
        &options.binary,
        &isolated,
        &scope,
        &["--json", "restore", &source, "--path", ".kioignore"],
    )?;
    require_error_code(
        &path_injection,
        "KIO-E-MANAGED-RESTORE-POLICY-001",
        "A09 restore control-path injection",
    )?;
    let stale = command_json_failure(
        &options.binary,
        &isolated,
        &scope,
        &["--json", "restore", &source, "--expected-head", &source],
    )?;
    require_error_code(
        &stale,
        "KIO-E-MANAGED-RESTORE-CONFLICT-001",
        "A09 stale HEAD",
    )?;
    fs::write(&file, b"A09 dirty selected bytes\n").map_err(io_error)?;
    let dirty = command_json_failure(
        &options.binary,
        &isolated,
        &scope,
        &["--json", "restore", &source, "--path", "fixture.txt"],
    )?;
    require_error_code(
        &dirty,
        "KIO-E-MANAGED-RESTORE-CONFLICT-001",
        "A09 dirty restore",
    )?;
    if fs::read(&file).map_err(io_error)? != b"A09 dirty selected bytes\n" {
        return Err(AcceptanceError::Command(
            "A09 dirty refusal changed working bytes".into(),
        ));
    }
    fs::write(&file, b"acceptance changed bytes\n").map_err(io_error)?;
    let selective = command_json(
        &options.binary,
        &isolated,
        &scope,
        &[
            "--json",
            "restore",
            &source,
            "--path",
            "fixture.txt",
            "--expected-head",
            &current,
        ],
    )?;
    let selective_commit =
        required_json_string(&selective, "/apply/commit_hash", "selective restore")?;
    if selective_commit == current || fs::read(&file).map_err(io_error)? != fixture_bytes {
        return Err(AcceptanceError::Command(
            "A09 restore did not restore the selected historical bytes".into(),
        ));
    }
    if fs::read(&selected).map_err(io_error)? != b"acceptance selected changed\n"
        || fs::read(&added).map_err(io_error)? != b"acceptance added bytes\n"
    {
        return Err(AcceptanceError::Command(
            "A09 selective restore changed an unselected managed path".into(),
        ));
    }
    assert_restore_child(
        &options.binary,
        &isolated,
        &scope,
        &selective_commit,
        &current,
        &source,
    )?;
    let full = command_json(
        &options.binary,
        &isolated,
        &scope,
        &[
            "--json",
            "restore",
            &source,
            "--delete-missing",
            "added.txt",
            "--expected-head",
            &selective_commit,
        ],
    )?;
    let restored_commit = required_json_string(&full, "/apply/commit_hash", "full restore")?;
    if fs::read(&file).map_err(io_error)? != fixture_bytes
        || fs::read(&selected).map_err(io_error)? != b"acceptance selected original\n"
        || added.exists()
    {
        return Err(AcceptanceError::Command(
            "A09 full restore did not restore all source paths and delete the explicit missing path".into(),
        ));
    }
    assert_restore_child(
        &options.binary,
        &isolated,
        &scope,
        &restored_commit,
        &selective_commit,
        &source,
    )?;
    if fs::read(scope.join(".kio/config.toml")).map_err(io_error)? != config_before
        || fs::read(scope.join(".kio/scope.json")).map_err(io_error)? != approval_before
        || ledger_artifacts_fingerprint(&isolated.join("xdg-data/kio"))? != ledger_before
        || fs::read(&unmanaged).map_err(io_error)? != unmanaged_before
    {
        return Err(AcceptanceError::Command(
            "A09 restore changed config, approvals, ledger, or unmanaged bytes".into(),
        ));
    }
    assert_a09_concurrent_restore_and_index(&options.binary, &isolated)?;
    let purged = command_json(
        &options.binary,
        &isolated,
        &scope,
        &[
            "--json",
            "purge",
            "fixture.txt",
            "--reason",
            "legal",
            "--yes",
        ],
    )?;
    if purged.get("status").and_then(serde_json::Value::as_str) != Some("purged") {
        return Err(AcceptanceError::Command(
            "A09 fixture purge did not complete".into(),
        ));
    }
    let purged_refusal = command_json_failure(
        &options.binary,
        &isolated,
        &scope,
        &["--json", "restore", &source, "--path", "fixture.txt"],
    )?;
    require_error_code(
        &purged_refusal,
        "KIO-E-MANAGED-RESTORE-RECOVERY-REQUIRED-001",
        "A09 purged source restore",
    )?;
    if sha256_regular_file(&options.binary, MAX_BINARY_BYTES)?
        != options.expected.candidate.binary_sha256
    {
        return Err(AcceptanceError::Invalid(
            "binary changed while A09 was executing".into(),
        ));
    }
    let receipt = AcceptanceReceipt {
        schema: SCHEMA.into(),
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

fn command_json(
    binary: &Path,
    isolated: &Path,
    scope: &Path,
    args: &[&str],
) -> Result<serde_json::Value, AcceptanceError> {
    let output = run_a01_command(binary, isolated, Some(scope), args)?;
    serde_json::from_slice(&output).map_err(|error| AcceptanceError::Json(error.to_string()))
}

fn command_json_failure(
    binary: &Path,
    isolated: &Path,
    scope: &Path,
    args: &[&str],
) -> Result<serde_json::Value, AcceptanceError> {
    let mut command = Command::new(binary);
    IsolatedChildEnvironment::new(
        isolated,
        isolated.join("xdg-config"),
        isolated.join("xdg-data"),
        isolated.join("xdg-cache"),
        isolated.join("tmp"),
    )
    .apply(&mut command)?;
    command.current_dir(scope).args(args);
    let output = run_bounded_command(&mut command, BoundedProcessOptions::default(), None)
        .map_err(|error| {
            AcceptanceError::Command(format!("bounded command did not complete: {error}"))
        })?;
    if output.status.success() {
        return Err(AcceptanceError::Command(format!(
            "{} unexpectedly succeeded",
            args.join(" ")
        )));
    }
    serde_json::from_str(&output.stderr).map_err(|error| {
        AcceptanceError::Command(format!(
            "{} failed without JSON error output: {error}",
            args.join(" ")
        ))
    })
}

fn require_error_code(
    value: &serde_json::Value,
    expected: &str,
    context: &str,
) -> Result<(), AcceptanceError> {
    if value.get("error_code").and_then(serde_json::Value::as_str) == Some(expected) {
        Ok(())
    } else {
        Err(AcceptanceError::Command(format!(
            "{context} returned an unexpected error: {value}"
        )))
    }
}

fn assert_restore_child(
    binary: &Path,
    isolated: &Path,
    scope: &Path,
    child: &str,
    parent: &str,
    source: &str,
) -> Result<(), AcceptanceError> {
    let commit = command_json(binary, isolated, scope, &["--json", "inspect", child])?;
    if !is_linear_restored_child(&commit, parent, source) {
        return Err(AcceptanceError::Command(
            "A09 restored commit is not a linear restored child of its prior HEAD".into(),
        ));
    }
    Ok(())
}

/// Launch an ordinary restore and a distinct-file index from the same current
/// HEAD.  A barrier aligns only evaluator-side process launch; it deliberately
/// does not claim an internal writer-lock interleaving or use a product test
/// seam.  The observed winner must still leave one linear history.
fn assert_a09_concurrent_restore_and_index(
    binary: &Path,
    isolated: &Path,
) -> Result<(), AcceptanceError> {
    let scope = isolated.join("concurrent-restore-index");
    for directory in [&scope, &isolated.join("tmp")] {
        fs::create_dir_all(directory).map_err(io_error)?;
    }
    let restored_path = scope.join("restored.txt");
    let mutation_path = scope.join("mutation.txt");
    let untouched_path = scope.join("untouched.txt");
    fs::write(&restored_path, b"A09 race original bytes\n").map_err(io_error)?;
    fs::write(&mutation_path, b"A09 race baseline bytes\n").map_err(io_error)?;
    fs::write(&untouched_path, b"A09 race untouched bytes\n").map_err(io_error)?;
    let scope_text = scope
        .to_str()
        .ok_or_else(|| AcceptanceError::Invalid("A09 concurrent scope is not UTF-8".into()))?;
    run_a01_command(binary, isolated, None, &["--json", "init", scope_text])?;
    let source = command_json(binary, isolated, &scope, &["--json", "index", "--offline"])?;
    let source = required_json_string(&source, "/commit_hash", "A09 concurrent source index")?;
    fs::write(&restored_path, b"A09 race current bytes\n").map_err(io_error)?;
    let expected = command_json(binary, isolated, &scope, &["--json", "index", "--offline"])?;
    let expected = required_json_string(&expected, "/commit_hash", "A09 concurrent current index")?;
    if source == expected {
        return Err(AcceptanceError::Command(
            "A09 concurrent setup did not advance HEAD".into(),
        ));
    }
    let config_before = fs::read(scope.join(".kio/config.toml")).map_err(io_error)?;
    let approval_before = fs::read(scope.join(".kio/scope.json")).map_err(io_error)?;
    let ledger_before = ledger_artifacts_fingerprint(&isolated.join("xdg-data/kio"))?;
    let untouched_before = fs::read(&untouched_path).map_err(io_error)?;
    fs::write(&mutation_path, b"A09 race concurrent mutation bytes\n").map_err(io_error)?;

    let launch = Arc::new(Barrier::new(3));
    let restore = concurrent_a09_child(
        binary,
        isolated,
        &scope,
        vec![
            "--json".into(),
            "restore".into(),
            source.clone(),
            "--path".into(),
            "restored.txt".into(),
            "--expected-head".into(),
            expected.clone(),
        ],
        Arc::clone(&launch),
    );
    let index = concurrent_a09_child(
        binary,
        isolated,
        &scope,
        vec!["--json".into(), "index".into(), "--offline".into()],
        Arc::clone(&launch),
    );
    launch.wait();
    // Always reap both bounded children before surfacing either outcome. This
    // prevents an early evaluator-side error from detaching a live writer.
    let restore = restore.join();
    let index = index.join();
    let restore = restore
        .map_err(|_| AcceptanceError::Command("A09 concurrent restore worker panicked".into()))??;
    let index = index
        .map_err(|_| AcceptanceError::Command("A09 concurrent index worker panicked".into()))??;
    let restore = concurrent_a09_restore_outcome(&restore)?;
    let index = concurrent_a09_index_outcome(&index)?;
    let (restore_commit, repair_projection) = match restore {
        A09ConcurrentRestoreOutcome::Published {
            commit,
            projection_needs_repair,
        } => (Some(commit), projection_needs_repair),
        A09ConcurrentRestoreOutcome::Refused => (None, false),
    };
    if repair_projection {
        command_json(
            binary,
            isolated,
            &scope,
            &["--json", "repair", "rebuild-db", "--offline"],
        )?;
    }
    let index = match index {
        Some(index) => index,
        None => {
            // The initial concurrent launch is retained as the contention
            // proof. After both children have exited, one offline retry
            // establishes the final projection without treating a fail-fast
            // outer writer lock as a test failure.
            command_json(binary, isolated, &scope, &["--json", "index", "--offline"])?
        }
    };
    let index_commit = required_json_string(&index, "/commit_hash", "A09 concurrent index")?;
    if index_commit == expected {
        return Err(AcceptanceError::Command(
            "A09 concurrent index did not publish the prepared mutation".into(),
        ));
    }

    if let Some(restore_commit) = restore_commit {
        assert_restore_child(
            binary,
            isolated,
            &scope,
            &restore_commit,
            &expected,
            &source,
        )?;
        assert_commit_parent(
            binary,
            isolated,
            &scope,
            &index_commit,
            &restore_commit,
            "A09 concurrent index after restore",
        )?;
        if fs::read(&restored_path).map_err(io_error)? != b"A09 race original bytes\n" {
            return Err(AcceptanceError::Command(
                "A09 concurrent restore succeeded without restoring selected bytes".into(),
            ));
        }
    } else {
        assert_commit_parent(
            binary,
            isolated,
            &scope,
            &index_commit,
            &expected,
            "A09 concurrent index before restore refusal",
        )?;
        if fs::read(&restored_path).map_err(io_error)? != b"A09 race current bytes\n" {
            return Err(AcceptanceError::Command(
                "A09 concurrent restore refusal changed selected bytes".into(),
            ));
        }
    }
    if fs::read(&untouched_path).map_err(io_error)? != untouched_before
        || fs::read(&mutation_path).map_err(io_error)? != b"A09 race concurrent mutation bytes\n"
        || fs::read(scope.join(".kio/config.toml")).map_err(io_error)? != config_before
        || fs::read(scope.join(".kio/scope.json")).map_err(io_error)? != approval_before
        || ledger_artifacts_fingerprint(&isolated.join("xdg-data/kio"))? != ledger_before
    {
        return Err(AcceptanceError::Command(
            "A09 concurrent restore changed unrelated, policy, or ledger state".into(),
        ));
    }
    let head = bounded_regular_bytes(&scope.join(".kio/HEAD"), 256)?;
    if std::str::from_utf8(&head)
        .map_err(|_| AcceptanceError::Command("A09 concurrent HEAD is not UTF-8".into()))?
        .trim()
        != index_commit
    {
        return Err(AcceptanceError::Command(
            "A09 concurrent final HEAD does not name the validated latest commit".into(),
        ));
    }
    Ok(())
}

enum A09ConcurrentRestoreOutcome {
    Published {
        commit: String,
        projection_needs_repair: bool,
    },
    Refused,
}

fn concurrent_a09_restore_outcome(
    output: &BoundedProcessOutput,
) -> Result<A09ConcurrentRestoreOutcome, AcceptanceError> {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&output.stdout)
        && let Some(commit) = value
            .pointer("/apply/commit_hash")
            .and_then(serde_json::Value::as_str)
    {
        if output.status.success() {
            return Ok(A09ConcurrentRestoreOutcome::Published {
                commit: commit.to_owned(),
                projection_needs_repair: false,
            });
        }
        if is_applied_projection_failure(&value, output.status.code()) {
            return Ok(A09ConcurrentRestoreOutcome::Published {
                commit: commit.to_owned(),
                projection_needs_repair: true,
            });
        }
        return Err(AcceptanceError::Command(format!(
            "A09 concurrent restore published a commit with unexpected exit/status: {}",
            output.status
        )));
    }
    if output.status.success() {
        return Err(AcceptanceError::Command(
            "A09 concurrent restore succeeded without an apply commit".into(),
        ));
    }
    let refusal = concurrent_a09_error_json(output, "restore")?;
    let code = refusal
        .get("error_code")
        .and_then(serde_json::Value::as_str);
    if matches!(
        code,
        Some("KIO-E-MANAGED-RESTORE-CONFLICT-001" | "KIO-E-STORE-LOCKED-001")
    ) {
        Ok(A09ConcurrentRestoreOutcome::Refused)
    } else {
        Err(AcceptanceError::Command(format!(
            "A09 concurrent restore returned an unexpected error: {refusal}"
        )))
    }
}

fn concurrent_a09_index_outcome(
    output: &BoundedProcessOutput,
) -> Result<Option<serde_json::Value>, AcceptanceError> {
    if output.status.success()
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&output.stdout)
        && value
            .pointer("/commit_hash")
            .and_then(serde_json::Value::as_str)
            .is_some()
    {
        return Ok(Some(value));
    }
    if output.status.success() {
        return Err(AcceptanceError::Command(
            "A09 concurrent index succeeded without a commit".into(),
        ));
    }
    let refusal = concurrent_a09_error_json(output, "index")?;
    if refusal
        .get("error_code")
        .and_then(serde_json::Value::as_str)
        == Some("KIO-E-STORE-LOCKED-001")
    {
        Ok(None)
    } else {
        Err(AcceptanceError::Command(format!(
            "A09 concurrent index returned an unexpected error: {refusal}"
        )))
    }
}

fn is_applied_projection_failure(value: &serde_json::Value, exit_code: Option<i32>) -> bool {
    exit_code == Some(3)
        && value.get("status").and_then(serde_json::Value::as_str)
            == Some("applied_projection_failed")
        && value
            .get("projection_status")
            .and_then(serde_json::Value::as_str)
            == Some("failed")
}

fn concurrent_a09_error_json(
    output: &BoundedProcessOutput,
    operation: &str,
) -> Result<serde_json::Value, AcceptanceError> {
    for stream in [&output.stdout, &output.stderr] {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(stream)
            && value.get("error_code").is_some()
        {
            return Ok(value);
        }
    }
    Err(AcceptanceError::Command(format!(
        "A09 concurrent {operation} failed without a recognized JSON result: {}",
        output.status
    )))
}

fn concurrent_a09_child(
    binary: &Path,
    isolated: &Path,
    scope: &Path,
    args: Vec<String>,
    launch: Arc<Barrier>,
) -> thread::JoinHandle<Result<BoundedProcessOutput, AcceptanceError>> {
    let binary = binary.to_owned();
    let isolated = isolated.to_owned();
    let scope = scope.to_owned();
    thread::spawn(move || {
        launch.wait();
        let mut command = Command::new(binary);
        IsolatedChildEnvironment::new(
            &isolated,
            isolated.join("xdg-config"),
            isolated.join("xdg-data"),
            isolated.join("xdg-cache"),
            isolated.join("tmp"),
        )
        .apply(&mut command)?;
        command.current_dir(scope).args(args);
        run_bounded_command(
            &mut command,
            BoundedProcessOptions {
                timeout: std::time::Duration::from_secs(90),
                max_stdout_bytes: 1024 * 1024,
                max_stderr_bytes: 256 * 1024,
            },
            None,
        )
        .map_err(|error| AcceptanceError::Command(format!("A09 concurrent child: {error}")))
    })
}

fn assert_commit_parent(
    binary: &Path,
    isolated: &Path,
    scope: &Path,
    child: &str,
    parent: &str,
    context: &str,
) -> Result<(), AcceptanceError> {
    let commit = command_json(binary, isolated, scope, &["--json", "inspect", child])?;
    if is_single_parent_child(&commit, parent) {
        Ok(())
    } else {
        Err(AcceptanceError::Command(format!(
            "{context} did not remain a single-parent child"
        )))
    }
}

fn is_single_parent_child(commit: &serde_json::Value, parent: &str) -> bool {
    commit
        .pointer("/parent")
        .and_then(serde_json::Value::as_str)
        == Some(parent)
        && commit.get("parents").is_none()
}

fn is_linear_restored_child(commit: &serde_json::Value, parent: &str, source: &str) -> bool {
    is_single_parent_child(commit, parent)
        && commit
            .pointer("/commit_type")
            .and_then(serde_json::Value::as_str)
            == Some("restored")
        && commit
            .pointer("/restore_provenance/source_commit")
            .and_then(serde_json::Value::as_str)
            == Some(source)
}

fn directory_fingerprint(root: &Path) -> Result<BTreeMap<PathBuf, String>, AcceptanceError> {
    let mut fingerprint = BTreeMap::new();
    match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.is_dir() => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(fingerprint),
        Err(error) => return Err(io_error(error)),
        Ok(_) => {
            return Err(AcceptanceError::Invalid(
                "fingerprint root is not a directory".into(),
            ));
        }
    }
    let mut pending = vec![root.to_owned()];
    let mut entries = 0_usize;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).map_err(io_error)? {
            entries += 1;
            if entries > 10_000 {
                return Err(AcceptanceError::Invalid(
                    "fingerprint exceeds fixture entry limit".into(),
                ));
            }
            let entry = entry.map_err(io_error)?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(io_error)?;
            let relative = path
                .strip_prefix(root)
                .map_err(|error| AcceptanceError::Io(format!("fixture fingerprint path: {error}")))?
                .to_owned();
            if file_type.is_dir() {
                fingerprint.insert(relative, "directory".into());
                pending.push(path);
            } else if file_type.is_file() {
                fingerprint.insert(relative, sha256_regular_file(&path, MAX_BINARY_BYTES)?);
            } else {
                return Err(AcceptanceError::Invalid(
                    "fingerprint contains a non-regular entry".into(),
                ));
            }
        }
    }
    Ok(fingerprint)
}

/// Read-only acceptance deliberately excludes device cursor/signing caches and
/// search telemetry: those are documented read-side implementation state. It
/// instead binds authority, immutable knowledge objects, and ledger presence;
/// no selected command may create or repair any of them.
fn readonly_authority_fingerprint(
    scope: &Path,
    isolated: &Path,
) -> Result<BTreeMap<PathBuf, String>, AcceptanceError> {
    let mut result = BTreeMap::new();
    for (label, path) in [
        ("scope-config", scope.join(".kio/config.toml")),
        ("scope-head", scope.join(".kio/HEAD")),
        ("scope-manifest", scope.join(".kio/manifest.json")),
        ("scope-objects", scope.join(".kio/objects")),
        ("scope-refs", scope.join(".kio/refs")),
        (
            "device-ledger",
            isolated.join("xdg-data/kio/cost-ledger.sqlite"),
        ),
        (
            "device-ledger-wal",
            isolated.join("xdg-data/kio/cost-ledger.sqlite-wal"),
        ),
        (
            "device-ledger-shm",
            isolated.join("xdg-data/kio/cost-ledger.sqlite-shm"),
        ),
    ] {
        if path.is_file() {
            result.insert(
                PathBuf::from(label),
                sha256_regular_file(&path, MAX_BINARY_BYTES)?,
            );
        } else {
            for (relative, digest) in directory_fingerprint(&path)? {
                result.insert(PathBuf::from(label).join(relative), digest);
            }
        }
    }
    for (relative, digest) in ledger_artifacts_fingerprint(&isolated.join("xdg-data/kio"))? {
        result.insert(
            PathBuf::from("device-ledger-lifecycle").join(relative),
            digest,
        );
    }
    Ok(result)
}

/// The registry and diagnostic JSONL files share the data directory with the
/// ledger, but are derived device-local caches/logs.  A managed restore may
/// legitimately refresh them; the cost ledger and each lifecycle companion
/// remain immutable acceptance inputs.
fn ledger_artifacts_fingerprint(root: &Path) -> Result<BTreeMap<PathBuf, String>, AcceptanceError> {
    const LEDGER_LEAVES: [&str; 8] = [
        "cost-ledger.sqlite",
        "cost-ledger.sqlite-wal",
        "cost-ledger.sqlite-shm",
        "cost-ledger.sqlite.authority.json",
        "cost-ledger.sqlite.checkpoint.json",
        "cost-ledger.sqlite.init.pending",
        "cost-ledger.sqlite.restore.pending",
        "ledger.lifecycle.lock",
    ];
    let mut fingerprint = BTreeMap::new();
    for leaf in LEDGER_LEAVES {
        let path = root.join(leaf);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => {
                fingerprint.insert(
                    PathBuf::from(leaf),
                    sha256_regular_file(&path, MAX_BINARY_BYTES)?,
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fingerprint.insert(PathBuf::from(leaf), "absent".into());
            }
            Ok(_) => {
                return Err(AcceptanceError::Invalid(format!(
                    "A09 ledger artifact is not a regular file: {leaf}"
                )));
            }
            Err(error) => return Err(io_error(error)),
        }
    }
    Ok(fingerprint)
}

fn require_present_ledger_artifacts(
    fingerprint: &BTreeMap<PathBuf, String>,
) -> Result<(), AcceptanceError> {
    for leaf in [
        "cost-ledger.sqlite",
        "cost-ledger.sqlite.authority.json",
        "cost-ledger.sqlite.checkpoint.json",
    ] {
        if fingerprint
            .get(Path::new(leaf))
            .is_none_or(|digest| digest == "absent")
        {
            return Err(AcceptanceError::Command(format!(
                "A09 ledger init did not create required artifact {leaf}"
            )));
        }
    }
    Ok(())
}

fn assert_a06_concurrent_cli_outcomes(
    binary: &Path,
    isolated: &Path,
    scope: &Path,
    query: &str,
    runtime: &str,
) -> Result<(), AcceptanceError> {
    let mut children = Vec::new();
    for args in [
        vec!["--json", "index", "--offline"],
        vec![
            "--json",
            "search",
            query,
            "--all-scopes",
            "--mode",
            "hybrid",
            "--offline",
        ],
    ] {
        let binary = binary.to_owned();
        let isolated = isolated.to_owned();
        let scope = scope.to_owned();
        let runtime = runtime.to_owned();
        let args = args.into_iter().map(ToOwned::to_owned).collect::<Vec<_>>();
        children.push(std::thread::spawn(move || {
            let mut command = Command::new(binary);
            IsolatedChildEnvironment::new(
                &isolated,
                isolated.join("xdg-config"),
                isolated.join("xdg-data"),
                isolated.join("xdg-cache"),
                isolated.join("tmp"),
            )
            .apply(&mut command)?;
            command
                .env("KIO_EVAL_DETERMINISTIC_EMBED", runtime)
                .current_dir(scope)
                .args(&args);
            let result = run_bounded_command(&mut command, BoundedProcessOptions::default(), None)
                .map_err(|error| {
                    AcceptanceError::Command(format!(
                        "A06 concurrent child did not complete: {error}"
                    ))
                })?;
            Ok::<_, AcceptanceError>((args, result.status.success(), result.stderr))
        }));
    }
    for child in children {
        let (args, succeeded, stderr) = child
            .join()
            .map_err(|_| AcceptanceError::Command("A06 concurrent CLI worker panicked".into()))??;
        if succeeded {
            continue;
        }
        let error: serde_json::Value = serde_json::from_str(&stderr).map_err(|parse| {
            AcceptanceError::Command(format!(
                "A06 concurrent {} failed without JSON error output: {parse}",
                args.join(" ")
            ))
        })?;
        require_error_code(&error, "KIO-E-STORE-LOCKED-001", "A06 concurrent CLI")?;
    }
    Ok(())
}

fn command_json_with_runtime(
    binary: &Path,
    isolated: &Path,
    scope: &Path,
    args: &[&str],
    runtime: &str,
) -> Result<serde_json::Value, AcceptanceError> {
    let output = run_a05_command(binary, isolated, Some(scope), args, runtime)?;
    serde_json::from_slice(&output).map_err(|error| AcceptanceError::Json(error.to_string()))
}

fn required_json_string(
    value: &serde_json::Value,
    pointer: &str,
    command: &str,
) -> Result<String, AcceptanceError> {
    value
        .pointer(pointer)
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            AcceptanceError::Command(format!("{command} returned no canonical commit hash"))
        })
}

fn run_a01_command(
    binary: &Path,
    isolated: &Path,
    scope: Option<&Path>,
    args: &[&str],
) -> Result<Vec<u8>, AcceptanceError> {
    let mut command = Command::new(binary);
    IsolatedChildEnvironment::new(
        isolated,
        isolated.join("xdg-config"),
        isolated.join("xdg-data"),
        isolated.join("xdg-cache"),
        isolated.join("tmp"),
    )
    .apply(&mut command)?;
    command.args(args);
    if let Some(scope) = scope {
        command.current_dir(scope);
    }
    let output = run_bounded_command(
        &mut command,
        BoundedProcessOptions {
            timeout: std::time::Duration::from_secs(90),
            max_stdout_bytes: 1024 * 1024,
            max_stderr_bytes: 256 * 1024,
        },
        None,
    )
    .map_err(|error| AcceptanceError::Command(error.to_string()))?;
    if !output.status.success() {
        return Err(AcceptanceError::Command(format!(
            "{} failed with {}: {}",
            args.join(" "),
            output.status,
            output.stderr.trim()
        )));
    }
    Ok(output.stdout.into_bytes())
}

/// Execute the shipped deterministic evaluator adapter. The exact selector is
/// part of the fixture's immutable digest and goes through the normal adapter
/// catalog, not any debug-only test control.
fn run_a05_command(
    binary: &Path,
    isolated: &Path,
    scope: Option<&Path>,
    args: &[&str],
    runtime: &str,
) -> Result<Vec<u8>, AcceptanceError> {
    let mut command = Command::new(binary);
    IsolatedChildEnvironment::new(
        isolated,
        isolated.join("xdg-config"),
        isolated.join("xdg-data"),
        isolated.join("xdg-cache"),
        isolated.join("tmp"),
    )
    .apply(&mut command)?;
    command
        .env("KIO_EVAL_DETERMINISTIC_EMBED", runtime)
        .args(args);
    if let Some(scope) = scope {
        command.current_dir(scope);
    }
    let output = run_bounded_command(
        &mut command,
        BoundedProcessOptions {
            timeout: std::time::Duration::from_secs(90),
            max_stdout_bytes: 1024 * 1024,
            max_stderr_bytes: 256 * 1024,
        },
        None,
    )
    .map_err(|error| AcceptanceError::Command(error.to_string()))?;
    if !output.status.success() {
        return Err(AcceptanceError::Command(format!(
            "{} failed with {}: {}",
            args.join(" "),
            output.status,
            output.stderr.trim()
        )));
    }
    Ok(output.stdout.into_bytes())
}

pub fn fixed_v1_requirements() -> BTreeSet<AcceptanceRequirement> {
    let mut requirements = BTreeSet::new();
    for os in [NativeOs::Linux, NativeOs::Macos, NativeOs::Windows] {
        for case in [
            AcceptanceCase::A01,
            AcceptanceCase::A02,
            AcceptanceCase::A03,
            AcceptanceCase::A04,
            AcceptanceCase::A05,
            AcceptanceCase::A06,
            AcceptanceCase::A09,
            AcceptanceCase::A10,
        ] {
            requirements.insert(requirement(
                case,
                AcceptanceLane::NativeContract,
                AcceptanceSubcase::Core,
                os,
            ));
        }
        requirements.insert(requirement(
            AcceptanceCase::A04,
            AcceptanceLane::NativeContract,
            AcceptanceSubcase::LocalTrust,
            os,
        ));
        requirements.insert(requirement(
            AcceptanceCase::A03,
            AcceptanceLane::ServiceNative,
            AcceptanceSubcase::Core,
            os,
        ));
        requirements.insert(requirement(
            AcceptanceCase::A11,
            AcceptanceLane::ServiceNative,
            AcceptanceSubcase::Core,
            os,
        ));
        requirements.insert(requirement(
            AcceptanceCase::A07,
            AcceptanceLane::OfficeReal,
            AcceptanceSubcase::Core,
            os,
        ));
        requirements.insert(requirement(
            AcceptanceCase::A08,
            AcceptanceLane::NativeContract,
            AcceptanceSubcase::MockFailure,
            os,
        ));
        for subcase in [
            AcceptanceSubcase::Mistral,
            AcceptanceSubcase::Gemini,
            AcceptanceSubcase::AuthenticatedLocal,
        ] {
            requirements.insert(requirement(
                AcceptanceCase::A08,
                AcceptanceLane::ProviderLive,
                subcase,
                os,
            ));
        }
        requirements.insert(requirement(
            AcceptanceCase::A12,
            AcceptanceLane::Distribution,
            AcceptanceSubcase::Core,
            os,
        ));
    }
    requirements
}

fn requirement(
    case: AcceptanceCase,
    lane: AcceptanceLane,
    subcase: AcceptanceSubcase,
    os: NativeOs,
) -> AcceptanceRequirement {
    AcceptanceRequirement {
        case,
        lane,
        subcase,
        os,
    }
}

pub(crate) fn validate_expected(expected: &ExpectedReceipt) -> Result<(), AcceptanceError> {
    validate_requirement(&expected.requirement)?;
    validate_candidate(&expected.candidate)?;
    if !target_matches_os(&expected.candidate.target, expected.requirement.os) {
        return Err(AcceptanceError::Invalid(
            "candidate target does not match receipt OS".into(),
        ));
    }
    validate_fixture(&expected.fixture)?;
    validate_workflow(&expected.workflow, &expected.candidate.candidate_sha)?;
    validate_evaluator_sha256(&expected.evaluator_sha256)?;
    validate_contract_binary(
        &expected.requirement,
        expected.contract_binary_sha256.as_deref(),
    )?;
    validate_service_identity_hash(
        &expected.requirement,
        expected.service_identity_sha256.as_deref(),
    )
}

fn validate_service_identity_hash(
    requirement: &AcceptanceRequirement,
    hash: Option<&str>,
) -> Result<(), AcceptanceError> {
    let required = matches!(
        (requirement.case, requirement.lane, requirement.subcase),
        (
            AcceptanceCase::A08,
            AcceptanceLane::ProviderLive,
            AcceptanceSubcase::AuthenticatedLocal
        )
    );
    match (required, hash) {
        (true, Some(hash)) if is_lower_hex(hash, 64) => Ok(()),
        (false, None) => Ok(()),
        (true, _) => Err(AcceptanceError::Invalid(
            "authenticated-local service identity hash is required".into(),
        )),
        (false, Some(_)) => Err(AcceptanceError::Invalid(
            "service identity hash is forbidden for this case".into(),
        )),
    }
}

fn validate_receipt(receipt: &AcceptanceReceipt) -> Result<(), AcceptanceError> {
    if receipt.schema != SCHEMA || !receipt.passed {
        return Err(AcceptanceError::Receipt(
            "schema or passed marker is invalid".into(),
        ));
    }
    validate_expected(&ExpectedReceipt {
        requirement: receipt.requirement.clone(),
        candidate: receipt.candidate.clone(),
        fixture: receipt.fixture.clone(),
        workflow: receipt.workflow.clone(),
        evaluator_sha256: receipt.evaluator_sha256.clone(),
        service_identity_sha256: receipt.service_identity_sha256.clone(),
        contract_binary_sha256: receipt.contract_binary_sha256.clone(),
    })
    .map_err(|error| AcceptanceError::Receipt(error.to_string()))?;
    validate_runtime(&receipt.runtime)
        .map_err(|error| AcceptanceError::Receipt(error.to_string()))?;
    if receipt.requirement.subcase == AcceptanceSubcase::AuthenticatedLocal
        && receipt.runtime.tools.iter().any(|tool| {
            !matches!(
                tool.tool_id.as_str(),
                "prepare_default" | "paddleocr_vl_local" | "qwen3_vl_embedding_local"
            )
        })
    {
        return Err(AcceptanceError::Receipt(
            "authenticated-local runtime contains an unexpected tool".into(),
        ));
    }
    let required = match (receipt.requirement.case, receipt.requirement.subcase) {
        (AcceptanceCase::A05 | AcceptanceCase::A06, AcceptanceSubcase::Core) => {
            Some("kio_eval_deterministic_embedding")
        }
        (AcceptanceCase::A07, AcceptanceSubcase::Core) => None,
        (AcceptanceCase::A08, AcceptanceSubcase::Mistral) => Some("mistral_ocr_markdownize"),
        (AcceptanceCase::A08, AcceptanceSubcase::Gemini) => Some("gemini_embedding_2"),
        (AcceptanceCase::A08, AcceptanceSubcase::AuthenticatedLocal) => {
            Some("qwen3_vl_embedding_local")
        }
        _ => None,
    };
    if let Some(id) = required
        && !receipt.runtime.tools.iter().any(|tool| tool.tool_id == id)
    {
        return Err(AcceptanceError::Receipt(format!(
            "required observed tool is missing: {id}"
        )));
    }
    if receipt.requirement.case == AcceptanceCase::A08
        && receipt.requirement.subcase == AcceptanceSubcase::AuthenticatedLocal
        && !receipt
            .runtime
            .tools
            .iter()
            .any(|tool| tool.tool_id == "paddleocr_vl_local")
    {
        return Err(AcceptanceError::Receipt(
            "required observed tool is missing: paddleocr_vl_local".into(),
        ));
    }
    if receipt.requirement.case == AcceptanceCase::A07 {
        if receipt.runtime.converter.is_none() {
            return Err(AcceptanceError::Receipt(
                "required observed converter is missing".into(),
            ));
        }
        if receipt.runtime.tools.is_empty() {
            return Err(AcceptanceError::Receipt(
                "required observed normalizer tool is missing".into(),
            ));
        }
    }
    Ok(())
}

fn validate_evaluator_sha256(evaluator_sha256: &str) -> Result<(), AcceptanceError> {
    if !is_lower_hex(evaluator_sha256, 64) {
        return Err(AcceptanceError::Invalid(
            "evaluator SHA-256 is malformed".into(),
        ));
    }
    Ok(())
}

fn validate_contract_binary(
    requirement: &AcceptanceRequirement,
    contract_binary_sha256: Option<&str>,
) -> Result<(), AcceptanceError> {
    let required = contract_binary_required(requirement);
    match (required, contract_binary_sha256) {
        (true, Some(value)) if is_lower_hex(value, 64) => Ok(()),
        (true, _) => Err(AcceptanceError::Invalid(
            "instrumented contract binary SHA-256 is required for this native requirement".into(),
        )),
        (false, None) => Ok(()),
        (false, Some(_)) => Err(AcceptanceError::Invalid(
            "instrumented contract binary SHA-256 is forbidden for this requirement".into(),
        )),
    }
}

fn contract_binary_required(requirement: &AcceptanceRequirement) -> bool {
    matches!(
        (requirement.case, requirement.lane, requirement.subcase),
        (
            AcceptanceCase::A10,
            AcceptanceLane::NativeContract,
            AcceptanceSubcase::Core
        ) | (
            AcceptanceCase::A08,
            AcceptanceLane::NativeContract,
            AcceptanceSubcase::MockFailure
        )
    )
}

fn validate_requirement(requirement: &AcceptanceRequirement) -> Result<(), AcceptanceError> {
    if !fixed_v1_requirements().contains(requirement) {
        return Err(AcceptanceError::Invalid(format!(
            "requirement is outside the fixed v1 matrix: {}",
            requirement_label(requirement)
        )));
    }
    Ok(())
}

fn validate_candidate(candidate: &CandidateBinding) -> Result<(), AcceptanceError> {
    if !is_lower_hex(&candidate.candidate_sha, 40)
        || !is_lower_hex(&candidate.archive_sha256, 64)
        || !is_lower_hex(&candidate.binary_sha256, 64)
        || candidate.target.is_empty()
        || candidate.version.is_empty()
    {
        return Err(AcceptanceError::Invalid(
            "candidate binding is malformed".into(),
        ));
    }
    Ok(())
}

fn validate_fixture(fixture: &FixtureBinding) -> Result<(), AcceptanceError> {
    if fixture.fixture_id.is_empty() || !is_lower_hex(&fixture.sha256, 64) {
        return Err(AcceptanceError::Invalid(
            "fixture binding is malformed".into(),
        ));
    }
    Ok(())
}

fn validate_workflow(
    workflow: &WorkflowBinding,
    candidate_sha: &str,
) -> Result<(), AcceptanceError> {
    if workflow.workflow_path.is_empty()
        || !is_lower_hex(&workflow.workflow_sha256, 64)
        || workflow.workflow_commit != candidate_sha
        || workflow.run_id == 0
        || workflow.attempt == 0
        || workflow.architecture.is_empty()
    {
        return Err(AcceptanceError::Invalid(
            "workflow binding is malformed".into(),
        ));
    }
    Ok(())
}

pub(crate) fn sha256_regular_file(path: &Path, maximum: u64) -> Result<String, AcceptanceError> {
    Ok(sha256_bytes(&bounded_regular_bytes(path, maximum)?))
}

pub(crate) fn current_evaluator_sha256() -> Result<String, AcceptanceError> {
    let executable = std::env::current_exe().map_err(io_error)?;
    sha256_regular_file(&executable, MAX_BINARY_BYTES)
}

pub(crate) fn current_evaluator_sha256_matches(
    expected_evaluator_sha256: &str,
) -> Result<String, AcceptanceError> {
    let actual = current_evaluator_sha256()?;
    if actual != expected_evaluator_sha256 {
        return Err(AcceptanceError::Invalid(
            "evaluator executable differs from the expected workflow binding".into(),
        ));
    }
    Ok(actual)
}

fn bounded_regular_bytes(path: &Path, maximum: u64) -> Result<Vec<u8>, AcceptanceError> {
    let metadata = fs::symlink_metadata(path).map_err(io_error)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > maximum {
        return Err(AcceptanceError::Invalid(
            "acceptance input must be a bounded regular file".into(),
        ));
    }
    fs::read(path).map_err(io_error)
}

pub(crate) fn sha256_bytes(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn write_receipt_create_only(
    path: &Path,
    receipt: &AcceptanceReceipt,
) -> Result<(), AcceptanceError> {
    validate_receipt(receipt)?;
    let value =
        serde_json::to_value(receipt).map_err(|error| AcceptanceError::Json(error.to_string()))?;
    let bytes = kio_core::cas::canonical_json_bytes(&value)
        .map_err(|error| AcceptanceError::Json(error.to_string()))?;
    if bytes.len() as u64 > MAX_RECEIPT_BYTES {
        return Err(AcceptanceError::Receipt(
            "receipt exceeds its bounded size".into(),
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| AcceptanceError::Invalid("receipt has no parent".into()))?;
    fs::create_dir_all(parent).map_err(io_error)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(io_error)?;
    file.write_all(&bytes).map_err(io_error)?;
    file.sync_all().map_err(io_error)
}

/// Read a create-only receipt without accepting alternative JSON spellings.
/// The aggregator must use this before passing receipts to [`AcceptancePlan`].
pub fn read_receipt(path: &Path) -> Result<AcceptanceReceipt, AcceptanceError> {
    let metadata = fs::symlink_metadata(path).map_err(io_error)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_RECEIPT_BYTES
    {
        return Err(AcceptanceError::Receipt(
            "receipt must be a bounded regular file".into(),
        ));
    }
    let bytes = fs::read(path).map_err(io_error)?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| AcceptanceError::Json(error.to_string()))?;
    let canonical = kio_core::cas::canonical_json_bytes(&value)
        .map_err(|error| AcceptanceError::Json(error.to_string()))?;
    if canonical != bytes {
        return Err(AcceptanceError::Receipt(
            "receipt JSON is not canonical".into(),
        ));
    }
    let receipt = serde_json::from_value::<AcceptanceReceipt>(value)
        .map_err(|error| AcceptanceError::Json(error.to_string()))?;
    validate_receipt(&receipt)?;
    Ok(receipt)
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn is_fixture_relative_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn target_matches_os(target: &str, os: NativeOs) -> bool {
    match os {
        NativeOs::Linux => target.contains("-linux-"),
        NativeOs::Macos => target.contains("-apple-darwin"),
        NativeOs::Windows => target.contains("-windows-"),
    }
}

fn requirement_label(requirement: &AcceptanceRequirement) -> String {
    format!(
        "{:?}/{:?}/{:?}/{:?}",
        requirement.case, requirement.lane, requirement.subcase, requirement.os
    )
}

fn io_error(error: std::io::Error) -> AcceptanceError {
    AcceptanceError::Io(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(value: char, length: usize) -> String {
        std::iter::repeat_n(value, length).collect()
    }

    fn expected(requirement: AcceptanceRequirement) -> ExpectedReceipt {
        let contract_binary_sha256 = contract_binary_required(&requirement).then(|| hex('f', 64));
        let target = match requirement.os {
            NativeOs::Linux => "x86_64-unknown-linux-gnu",
            NativeOs::Macos => "aarch64-apple-darwin",
            NativeOs::Windows => "x86_64-pc-windows-msvc",
        };
        ExpectedReceipt {
            requirement: requirement.clone(),
            candidate: CandidateBinding {
                candidate_sha: hex('a', 40),
                target: target.into(),
                archive_sha256: hex('b', 64),
                binary_sha256: hex('c', 64),
                version: "1.0.0".into(),
            },
            fixture: FixtureBinding {
                fixture_id: "fixture-a".into(),
                sha256: hex('d', 64),
            },
            workflow: WorkflowBinding {
                workflow_path: ".github/workflows/acceptance.yml".into(),
                workflow_sha256: hex('e', 64),
                workflow_commit: hex('a', 40),
                run_id: 1,
                attempt: 1,
                architecture: "x86_64".into(),
            },
            evaluator_sha256: hex('9', 64),
            service_identity_sha256: (requirement.case == AcceptanceCase::A08
                && requirement.subcase == AcceptanceSubcase::AuthenticatedLocal)
                .then(|| hex('7', 64)),
            contract_binary_sha256,
        }
    }

    fn full_plan() -> Vec<ExpectedReceipt> {
        fixed_v1_requirements().into_iter().map(expected).collect()
    }

    fn receipt(expected: &ExpectedReceipt) -> AcceptanceReceipt {
        let hash = format!("sha256:{}", "a".repeat(64));
        let mut tools = Vec::new();
        match (expected.requirement.case, expected.requirement.subcase) {
            (AcceptanceCase::A05 | AcceptanceCase::A06, AcceptanceSubcase::Core) => {
                tools.push(ObservedToolIdentity {
                    tool_id: "kio_eval_deterministic_embedding".into(),
                    tool_profile_hash: hash.clone(),
                })
            }
            (AcceptanceCase::A08, AcceptanceSubcase::Mistral) => tools.push(ObservedToolIdentity {
                tool_id: "mistral_ocr_markdownize".into(),
                tool_profile_hash: hash.clone(),
            }),
            (AcceptanceCase::A08, AcceptanceSubcase::Gemini) => tools.push(ObservedToolIdentity {
                tool_id: "gemini_embedding_2".into(),
                tool_profile_hash: hash.clone(),
            }),
            (AcceptanceCase::A08, AcceptanceSubcase::AuthenticatedLocal) => {
                tools.push(ObservedToolIdentity {
                    tool_id: "paddleocr_vl_local".into(),
                    tool_profile_hash: hash.clone(),
                });
                tools.push(ObservedToolIdentity {
                    tool_id: "qwen3_vl_embedding_local".into(),
                    tool_profile_hash: hash.clone(),
                });
            }
            (AcceptanceCase::A07, AcceptanceSubcase::Core) => tools.push(ObservedToolIdentity {
                tool_id: "prepare_default".into(),
                tool_profile_hash: hash.clone(),
            }),
            _ => {}
        }
        AcceptanceReceipt {
            schema: SCHEMA.into(),
            requirement: expected.requirement.clone(),
            candidate: expected.candidate.clone(),
            fixture: expected.fixture.clone(),
            workflow: expected.workflow.clone(),
            evaluator_sha256: expected.evaluator_sha256.clone(),
            service_identity_sha256: expected.service_identity_sha256.clone(),
            contract_binary_sha256: expected.contract_binary_sha256.clone(),
            runtime: AcceptanceRuntime {
                tools,
                converter: (expected.requirement.case == AcceptanceCase::A07).then(|| {
                    ObservedConverterIdentity {
                        sha256: "b".repeat(64),
                        version: "Office 1.0".into(),
                    }
                }),
            },
            passed: true,
        }
    }

    fn reviewed_binding() -> AcceptanceBindingInput {
        AcceptanceBindingInput {
            schema: ACCEPTANCE_BINDING_SCHEMA.into(),
            candidate_sha: hex('a', 40),
            targets: [
                (NativeOs::Linux, "x86_64-unknown-linux-gnu"),
                (NativeOs::Macos, "aarch64-apple-darwin"),
                (NativeOs::Windows, "x86_64-pc-windows-msvc"),
            ]
            .into_iter()
            .map(|(os, target)| AcceptanceTargetBinding {
                os,
                target: target.into(),
                archive_sha256: hex('b', 64),
                binary_sha256: hex('c', 64),
                version: "1.0.0".into(),
            })
            .collect(),
            executions: fixed_v1_requirements()
                .into_iter()
                .map(|requirement| {
                    let contract_binary_sha256 =
                        contract_binary_required(&requirement).then(|| hex('f', 64));
                    AcceptanceExecutionBinding {
                        fixture: FixtureBinding {
                            fixture_id: format!("fixture-{:?}", requirement.case),
                            sha256: hex('d', 64),
                        },
                        workflow: WorkflowBinding {
                            workflow_path: match requirement.lane {
                                AcceptanceLane::OfficeReal => {
                                    ".github/workflows/v1-acceptance-office.yml"
                                }
                                AcceptanceLane::ProviderLive => {
                                    ".github/workflows/v1-provider-acceptance.yml"
                                }
                                _ => ".github/workflows/v1-acceptance.yml",
                            }
                            .into(),
                            workflow_sha256: hex('e', 64),
                            workflow_commit: hex('a', 40),
                            run_id: u64::from(requirement.case as u8) + 1,
                            attempt: 1,
                            architecture: match requirement.os {
                                NativeOs::Linux | NativeOs::Windows => "x86_64",
                                NativeOs::Macos => "aarch64",
                            }
                            .into(),
                        },
                        evaluator_sha256: hex('9', 64),
                        service_identity_sha256: (requirement.case == AcceptanceCase::A08
                            && requirement.subcase == AcceptanceSubcase::AuthenticatedLocal)
                            .then(|| hex('7', 64)),
                        requirement,
                        contract_binary_sha256,
                    }
                })
                .collect(),
        }
    }

    #[test]
    fn reviewed_bindings_generate_the_complete_fixed_matrix_without_receipts() {
        let binding = reviewed_binding();
        let manifest = binding.expected_manifest().unwrap();
        assert_eq!(manifest.expected.len(), fixed_v1_requirements().len());
        for entry in &manifest.expected {
            assert_eq!(entry.candidate.candidate_sha, binding.candidate_sha);
            assert!(
                binding
                    .targets
                    .iter()
                    .any(|target| target.os == entry.requirement.os
                        && target.binary_sha256 == entry.candidate.binary_sha256)
            );
            assert!(binding.executions.iter().any(|execution| {
                execution.requirement == entry.requirement
                    && execution.fixture == entry.fixture
                    && execution.workflow == entry.workflow
                    && execution.contract_binary_sha256 == entry.contract_binary_sha256
            }));
        }
        assert!(AcceptanceManifest::parse_canonical(&manifest.canonical_bytes().unwrap()).is_ok());
    }

    #[test]
    fn reviewed_bindings_reject_missing_target_or_unreviewed_workflow_commit() {
        let mut missing = reviewed_binding();
        missing.targets.pop();
        assert!(missing.expected_manifest().is_err());
        let mut wrong_commit = reviewed_binding();
        wrong_commit.executions[0].workflow.workflow_commit = hex('f', 40);
        assert!(wrong_commit.expected_manifest().is_err());
    }

    #[test]
    fn reviewed_bindings_reject_the_retired_v1_schema() {
        let mut binding = reviewed_binding();
        binding.schema = "kio.acceptance.binding/v1".into();
        assert!(binding.expected_manifest().is_err());
    }

    #[test]
    fn reviewed_bindings_reject_missing_duplicate_and_extra_execution_requirements() {
        let mut missing = reviewed_binding();
        missing.executions.pop();
        assert!(missing.expected_manifest().is_err());

        let mut duplicate = reviewed_binding();
        duplicate.executions[1].requirement = duplicate.executions[0].requirement.clone();
        assert!(duplicate.expected_manifest().is_err());

        let mut extra = reviewed_binding();
        extra.executions.push(extra.executions[0].clone());
        assert!(extra.expected_manifest().is_err());
    }

    #[test]
    fn contract_binary_hash_is_required_only_for_the_native_fault_contracts() {
        let a10 = requirement(
            AcceptanceCase::A10,
            AcceptanceLane::NativeContract,
            AcceptanceSubcase::Core,
            NativeOs::Linux,
        );
        let mut missing = expected(a10.clone());
        missing.contract_binary_sha256 = None;
        assert!(validate_expected(&missing).is_err());

        let a08 = requirement(
            AcceptanceCase::A08,
            AcceptanceLane::NativeContract,
            AcceptanceSubcase::MockFailure,
            NativeOs::Linux,
        );
        let mut malformed = expected(a08);
        malformed.contract_binary_sha256 = Some("f".repeat(63));
        assert!(validate_expected(&malformed).is_err());

        let mut forbidden = expected(requirement(
            AcceptanceCase::A01,
            AcceptanceLane::NativeContract,
            AcceptanceSubcase::Core,
            NativeOs::Linux,
        ));
        forbidden.contract_binary_sha256 = Some(hex('f', 64));
        assert!(validate_expected(&forbidden).is_err());
    }

    #[test]
    fn reviewed_bindings_can_bind_case_executions_to_distinct_workflows_and_fixtures() {
        let binding = reviewed_binding();
        let manifest = binding.expected_manifest().unwrap();
        let native = manifest
            .expected
            .iter()
            .find(|entry| {
                entry.requirement.case == AcceptanceCase::A01
                    && entry.requirement.os == NativeOs::Linux
            })
            .unwrap();
        let office = manifest
            .expected
            .iter()
            .find(|entry| {
                entry.requirement.case == AcceptanceCase::A07
                    && entry.requirement.lane == AcceptanceLane::OfficeReal
                    && entry.requirement.os == NativeOs::Linux
            })
            .unwrap();
        let provider = manifest
            .expected
            .iter()
            .find(|entry| {
                entry.requirement.case == AcceptanceCase::A08
                    && entry.requirement.lane == AcceptanceLane::ProviderLive
                    && entry.requirement.os == NativeOs::Linux
            })
            .unwrap();
        assert_ne!(native.workflow.workflow_path, office.workflow.workflow_path);
        assert_ne!(
            office.workflow.workflow_path,
            provider.workflow.workflow_path
        );
        assert_ne!(native.fixture.fixture_id, office.fixture.fixture_id);
    }

    #[test]
    fn single_expected_receipt_round_trip_is_canonical_and_does_not_require_full_matrix() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("expected.json");
        let expected = expected(requirement(
            AcceptanceCase::A01,
            AcceptanceLane::NativeContract,
            AcceptanceSubcase::Core,
            NativeOs::Linux,
        ));
        write_expected_receipt_create_only(&path, &expected).unwrap();
        assert_eq!(read_expected_receipt(&path).unwrap(), expected);
        assert!(write_expected_receipt_create_only(&path, &expected).is_err());
    }

    #[test]
    fn expected_receipt_rejects_missing_evaluator_binding() {
        let mut expected = expected(requirement(
            AcceptanceCase::A01,
            AcceptanceLane::NativeContract,
            AcceptanceSubcase::Core,
            NativeOs::Linux,
        ));
        expected.evaluator_sha256.clear();
        assert!(validate_expected(&expected).is_err());
    }

    #[test]
    fn authenticated_local_endpoint_rejects_remote_authority_and_userinfo_prefix() {
        assert!(validate_loopback_https("https://127.0.0.1:18080").is_ok());
        assert!(validate_loopback_https("https://127.0.0.1.evil.example:18080").is_err());
        assert!(validate_loopback_https("https://127.0.0.1@evil.example:18080").is_err());
        assert!(validate_loopback_https("https://[::1]:18080/path").is_err());
    }

    #[test]
    fn receipt_rejects_missing_case_runtime_identity() {
        let mut a05 = receipt(&expected(requirement(
            AcceptanceCase::A05,
            AcceptanceLane::NativeContract,
            AcceptanceSubcase::Core,
            NativeOs::Linux,
        )));
        a05.runtime.tools.clear();
        assert!(validate_receipt(&a05).is_err());

        let mut a07 = receipt(&expected(requirement(
            AcceptanceCase::A07,
            AcceptanceLane::OfficeReal,
            AcceptanceSubcase::Core,
            NativeOs::Linux,
        )));
        a07.runtime.converter = None;
        assert!(validate_receipt(&a07).is_err());
    }

    #[test]
    fn evaluator_mismatch_rejects_a01_before_creating_work_or_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let expected = expected(requirement(
            AcceptanceCase::A01,
            AcceptanceLane::NativeContract,
            AcceptanceSubcase::Core,
            NativeOs::Linux,
        ));
        let work_dir = directory.path().join("work");
        let receipt = directory.path().join("receipt.json");
        let result = run_a01(&A01Options {
            binary: directory.path().join("missing-kio"),
            fixture: directory.path().join("missing-fixture"),
            expected,
            work_dir: work_dir.clone(),
            receipt: receipt.clone(),
        });
        assert!(result.is_err());
        assert!(!work_dir.exists());
        assert!(!receipt.exists());
    }

    #[test]
    fn a09_restore_assertion_requires_restored_single_parent_provenance() {
        let parent = hex('a', 64);
        let source = hex('b', 64);
        let valid = serde_json::json!({
            "parent": parent,
            "commit_type": "restored",
            "restore_provenance": { "source_commit": source },
        });
        assert!(is_linear_restored_child(
            &valid,
            valid["parent"].as_str().unwrap(),
            valid["restore_provenance"]["source_commit"]
                .as_str()
                .unwrap(),
        ));
        for invalid in [
            serde_json::json!({
                "parent": hex('a', 64),
                "commit_type": "auto",
                "restore_provenance": { "source_commit": hex('b', 64) },
            }),
            serde_json::json!({
                "parent": hex('a', 64),
                "commit_type": "restored",
                "restore_provenance": { "source_commit": hex('c', 64) },
            }),
            serde_json::json!({
                "parent": hex('a', 64),
                "parents": [hex('a', 64), hex('c', 64)],
                "commit_type": "restored",
                "restore_provenance": { "source_commit": hex('b', 64) },
            }),
        ] {
            assert!(!is_linear_restored_child(
                &invalid,
                &hex('a', 64),
                &hex('b', 64)
            ));
        }
    }

    #[test]
    fn a09_projection_failure_requires_exact_partial_failure_shape() {
        let published = serde_json::json!({
            "apply": { "commit_hash": hex('a', 64) },
            "status": "applied_projection_failed",
            "projection_status": "failed",
        });
        assert!(is_applied_projection_failure(&published, Some(3)));
        assert!(!is_applied_projection_failure(&published, Some(4)));
        assert!(!is_applied_projection_failure(
            &serde_json::json!({
                "apply": { "commit_hash": hex('a', 64) },
                "status": "applied_projection_failed",
                "projection_status": "ready",
            }),
            Some(3),
        ));
    }

    #[test]
    fn oversized_expected_and_manifest_writers_do_not_create_outputs() {
        let directory = tempfile::tempdir().unwrap();
        let expected_output = directory.path().join("expected.json");
        let mut expected = expected(requirement(
            AcceptanceCase::A01,
            AcceptanceLane::NativeContract,
            AcceptanceSubcase::Core,
            NativeOs::Linux,
        ));
        expected.fixture.fixture_id = "f".repeat((MAX_EXPECTED_CASE_BYTES + 1) as usize);
        assert!(write_expected_receipt_create_only(&expected_output, &expected).is_err());
        assert!(!expected_output.exists());

        let receipt_output = directory.path().join("receipt.json");
        let mut oversized_receipt = receipt(&expected);
        oversized_receipt.fixture.fixture_id = "f".repeat((MAX_RECEIPT_BYTES + 1) as usize);
        assert!(write_receipt_create_only(&receipt_output, &oversized_receipt).is_err());
        assert!(!receipt_output.exists());

        let manifest_output = directory.path().join("manifest.json");
        let mut expected = full_plan();
        for entry in &mut expected {
            entry.fixture.fixture_id = "f".repeat(32 * 1024);
        }
        let manifest = AcceptanceManifest {
            schema: ACCEPTANCE_MANIFEST_SCHEMA.into(),
            expected,
        };
        assert!(write_expected_manifest_create_only(&manifest_output, &manifest).is_err());
        assert!(!manifest_output.exists());
    }

    #[test]
    fn fixed_matrix_requires_every_case_and_separate_security_service_and_provider_evidence() {
        let matrix = fixed_v1_requirements();
        assert!(AcceptanceCase::A01 <= AcceptanceCase::A12);
        for case in [
            AcceptanceCase::A01,
            AcceptanceCase::A02,
            AcceptanceCase::A03,
            AcceptanceCase::A04,
            AcceptanceCase::A05,
            AcceptanceCase::A06,
            AcceptanceCase::A07,
            AcceptanceCase::A08,
            AcceptanceCase::A09,
            AcceptanceCase::A10,
            AcceptanceCase::A11,
            AcceptanceCase::A12,
        ] {
            assert!(matrix.iter().any(|entry| entry.case == case));
        }
        for os in [NativeOs::Linux, NativeOs::Macos, NativeOs::Windows] {
            assert!(matrix.contains(&requirement(
                AcceptanceCase::A04,
                AcceptanceLane::NativeContract,
                AcceptanceSubcase::LocalTrust,
                os
            )));
            assert!(matrix.contains(&requirement(
                AcceptanceCase::A11,
                AcceptanceLane::ServiceNative,
                AcceptanceSubcase::Core,
                os
            )));
            for subcase in [
                AcceptanceSubcase::MockFailure,
                AcceptanceSubcase::Mistral,
                AcceptanceSubcase::Gemini,
                AcceptanceSubcase::AuthenticatedLocal,
            ] {
                assert!(matrix.iter().any(|entry| entry.case == AcceptanceCase::A08
                    && entry.subcase == subcase
                    && entry.os == os));
            }
        }
    }

    #[test]
    fn plan_rejects_missing_duplicate_and_wrong_bound_receipts() {
        let expected = full_plan();
        let plan = AcceptancePlan::new(expected.clone()).unwrap();
        let receipts = expected.iter().map(receipt).collect::<Vec<_>>();
        plan.verify(&receipts).unwrap();
        assert!(plan.verify(&receipts[..receipts.len() - 1]).is_err());
        let mut duplicate = receipts.clone();
        duplicate.push(receipts[0].clone());
        assert!(plan.verify(&duplicate).is_err());
        let mut wrong = receipts;
        wrong[0].candidate.binary_sha256 = hex('f', 64);
        assert!(plan.verify(&wrong).is_err());
        let mut wrong_contract_binary = expected.iter().map(receipt).collect::<Vec<_>>();
        let contract = wrong_contract_binary
            .iter_mut()
            .find(|receipt| receipt.contract_binary_sha256.is_some())
            .unwrap();
        contract.contract_binary_sha256 = Some(hex('e', 64));
        assert!(plan.verify(&wrong_contract_binary).is_err());
        let mut wrong_evaluator = expected.iter().map(receipt).collect::<Vec<_>>();
        wrong_evaluator[0].evaluator_sha256 = hex('e', 64);
        assert!(plan.verify(&wrong_evaluator).is_err());
    }

    #[test]
    fn plan_rejects_mixed_candidate_sha_even_when_each_workflow_matches_it() {
        let mut expected = full_plan();
        let changed = expected
            .iter_mut()
            .find(|entry| entry.requirement.os == NativeOs::Linux)
            .unwrap();
        changed.candidate.candidate_sha = hex('f', 40);
        changed.workflow.workflow_commit = hex('f', 40);
        assert!(AcceptancePlan::new(expected).is_err());
    }

    #[test]
    fn plan_rejects_mixed_candidate_binary_for_one_os() {
        let mut expected = full_plan();
        let mut entries = expected
            .iter_mut()
            .filter(|entry| entry.requirement.os == NativeOs::Linux);
        entries.next().unwrap();
        entries.next().unwrap().candidate.binary_sha256 = hex('f', 64);
        assert!(AcceptancePlan::new(expected).is_err());
    }

    #[test]
    fn plan_rejects_mixed_instrumented_binary_for_one_os() {
        let mut expected = full_plan();
        let changed = expected
            .iter_mut()
            .find(|entry| {
                entry.requirement.case == AcceptanceCase::A08
                    && entry.requirement.lane == AcceptanceLane::NativeContract
                    && entry.requirement.subcase == AcceptanceSubcase::MockFailure
                    && entry.requirement.os == NativeOs::Linux
            })
            .unwrap();
        changed.contract_binary_sha256 = Some(hex('e', 64));
        assert!(AcceptancePlan::new(expected).is_err());
    }

    #[test]
    fn assemble_expected_cases_accepts_only_a_complete_predeclared_candidate_matrix() {
        let expected = full_plan();
        let manifest = assemble_expected_cases(&hex('a', 40), expected.clone()).unwrap();
        assert_eq!(manifest.expected, expected);
        assert!(assemble_expected_cases(&hex('f', 40), expected[..1].to_vec()).is_err());
    }

    #[test]
    fn plan_rejects_non_passing_and_unprepared_style_receipts() {
        let expected = full_plan();
        let plan = AcceptancePlan::new(expected.clone()).unwrap();
        let mut receipts = expected.iter().map(receipt).collect::<Vec<_>>();
        receipts[0].passed = false;
        assert!(plan.verify(&receipts).is_err());
        receipts[0].passed = true;
        receipts[0].workflow.attempt = 0;
        assert!(plan.verify(&receipts).is_err());
    }

    #[test]
    fn receipt_reader_rejects_noncanonical_json() {
        let directory = tempfile::tempdir().unwrap();
        let expected = expected(requirement(
            AcceptanceCase::A01,
            AcceptanceLane::NativeContract,
            AcceptanceSubcase::Core,
            NativeOs::Linux,
        ));
        let receipt = receipt(&expected);
        let path = directory.path().join("receipt.json");
        fs::write(&path, serde_json::to_vec_pretty(&receipt).unwrap()).unwrap();
        assert!(read_receipt(&path).is_err());
    }

    fn public_image_expectation() -> (PathBuf, FixtureImageExpectation) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("acceptance-fixtures/v1/a05-full-image-unavailable");
        let fixture = read_fixture_bundle(&root).unwrap();
        let query = fixture
            .queries
            .iter()
            .find(|query| query.expect_image.is_some())
            .unwrap();
        // Image identity is separate from the beta text hit requested by this query.
        assert_eq!((&*query.scope, &*query.expect_path), ("beta", "beta.md"));
        (root, query.expect_image.clone().unwrap())
    }

    fn public_image_response(expected: &FixtureImageExpectation) -> serde_json::Value {
        serde_json::json!({"results": [{
            "result_type": "image",
            "payload_uri": format!("kio://actual-alpha/object/image/sha256:{}", expected.sha256),
            "evidence_pointer": {
                "scope_id": "actual-alpha",
                "path_at_commit": expected.path,
                "raw_hash": format!("sha256:{}", expected.sha256),
            }
        }]})
    }

    #[test]
    fn image_result_requires_exact_fixture_scope_path_hash_and_payload_uri() {
        let (_, expected) = public_image_expectation();
        let response = public_image_response(&expected);
        assert!(fixture_image_uri(&response, &expected, "actual-alpha", "test").is_ok());
        for (pointer, replacement) in [
            (
                "/results/0/payload_uri",
                serde_json::json!("kio://fabricated"),
            ),
            (
                "/results/0/payload_uri",
                serde_json::json!(format!(
                    "kio://foreign/object/image/sha256:{}",
                    expected.sha256
                )),
            ),
            (
                "/results/0/payload_uri",
                serde_json::json!(format!(
                    "kio://actual-alpha/object/image/sha256:{}",
                    "b".repeat(64)
                )),
            ),
            (
                "/results/0/evidence_pointer/scope_id",
                serde_json::json!("foreign"),
            ),
            (
                "/results/0/evidence_pointer/path_at_commit",
                serde_json::json!("nested/figure.png"),
            ),
            (
                "/results/0/evidence_pointer/raw_hash",
                serde_json::json!(format!("sha256:{}", "b".repeat(64))),
            ),
            ("/results/0/evidence_pointer", serde_json::Value::Null),
        ] {
            let mut invalid = response.clone();
            *invalid.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                fixture_image_uri(&invalid, &expected, "actual-alpha", "test").is_err(),
                "accepted {pointer}"
            );
        }
        let mut missing = response;
        missing["results"][0]
            .as_object_mut()
            .unwrap()
            .remove("evidence_pointer");
        assert!(fixture_image_uri(&missing, &expected, "actual-alpha", "test").is_err());
    }

    #[test]
    fn opened_image_must_match_immutable_public_fixture_bytes() {
        let (root, expected) = public_image_expectation();
        let opened = serde_json::json!({"path": root.join(&expected.scope).join(&expected.path)});
        assert_opened_fixture_image(&opened, &expected, "test").unwrap();
        let temp = tempfile::tempdir().unwrap();
        let wrong = temp.path().join("wrong.png");
        fs::write(&wrong, b"different or stale image bytes").unwrap();
        assert!(
            assert_opened_fixture_image(&serde_json::json!({"path": wrong}), &expected, "test")
                .is_err()
        );
        assert!(assert_opened_fixture_image(&serde_json::json!({}), &expected, "test").is_err());
    }

    #[test]
    fn image_expectation_must_match_a_listed_image_file() {
        let temp = tempfile::tempdir().unwrap();
        write_fixture_bundle(
            temp.path(),
            FixtureBundleRequirements {
                require_image: false,
                require_vector: true,
                deterministic_embedding_runtime: Some("scale-v3".into()),
            },
        );
        let manifest_path = temp.path().join(FIXTURE_BUNDLE_MANIFEST);
        let original: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        for expectation in [
            serde_json::json!({"scope":"alpha","path":"missing.png","sha256":sha256_bytes(b"image")}),
            serde_json::json!({"scope":"alpha","path":"nested/first.md","sha256":sha256_bytes(b"alpha needle")}),
        ] {
            let mut invalid = original.clone();
            invalid["queries"][0]["expect_image"] = expectation;
            fs::write(
                &manifest_path,
                kio_core::cas::canonical_json_bytes(&invalid).unwrap(),
            )
            .unwrap();
            assert!(read_fixture_bundle(temp.path()).is_err());
        }
    }

    fn write_fixture_bundle(root: &Path, requirements: FixtureBundleRequirements) {
        fs::create_dir_all(root.join("alpha/nested")).unwrap();
        fs::create_dir_all(root.join("beta")).unwrap();
        fs::write(root.join("alpha/nested/first.md"), b"alpha needle").unwrap();
        fs::write(root.join("beta/second.md"), b"beta needle").unwrap();
        let manifest = FixtureBundleManifest {
            schema: FIXTURE_BUNDLE_SCHEMA.into(),
            scopes: vec!["alpha".into(), "beta".into()],
            files: vec![
                FixtureBundleFile {
                    scope: "alpha".into(),
                    path: "nested/first.md".into(),
                    sha256: sha256_bytes(b"alpha needle"),
                },
                FixtureBundleFile {
                    scope: "beta".into(),
                    path: "second.md".into(),
                    sha256: sha256_bytes(b"beta needle"),
                },
            ],
            queries: vec![
                FixtureBundleQuery {
                    scope: "alpha".into(),
                    query: "alpha".into(),
                    expect_path: "nested/first.md".into(),
                    expect_image: None,
                },
                FixtureBundleQuery {
                    scope: "beta".into(),
                    query: "beta".into(),
                    expect_path: "second.md".into(),
                    expect_image: None,
                },
            ],
            mutations: vec![FixtureBundleMutation {
                scope: "alpha".into(),
                path: "nested/first.md".into(),
                replacement_utf8: "alpha changed needle".into(),
            }],
            requirements,
        };
        let value = serde_json::to_value(manifest).unwrap();
        let bytes = kio_core::cas::canonical_json_bytes(&value).unwrap();
        fs::write(root.join(FIXTURE_BUNDLE_MANIFEST), bytes).unwrap();
    }

    #[test]
    fn materialized_fixture_bundle_copies_only_bound_scoped_files() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        write_fixture_bundle(
            &source,
            FixtureBundleRequirements {
                require_image: true,
                require_vector: true,
                deterministic_embedding_runtime: Some("test-control/mock/v1".into()),
            },
        );
        fs::write(source.join("alpha/unlisted.md"), b"must not copy").unwrap();
        let destination = directory.path().join("materialized");
        let (manifest, scopes) = materialize_fixture_bundle(&source, &destination).unwrap();
        assert_eq!(manifest.scopes, vec!["alpha".to_owned(), "beta".to_owned()]);
        assert_eq!(
            fs::read(scopes["alpha"].join("nested/first.md")).unwrap(),
            b"alpha needle"
        );
        assert_eq!(
            fs::read(scopes["beta"].join("second.md")).unwrap(),
            b"beta needle"
        );
        assert!(!scopes["alpha"].join("unlisted.md").exists());
        assert!(fixture_bundle_digest(&source).is_ok());
        assert!(materialize_fixture_bundle(&source, &destination).is_err());
    }

    #[test]
    fn fixture_bundle_rejects_vector_without_deterministic_runtime() {
        let directory = tempfile::tempdir().unwrap();
        write_fixture_bundle(
            directory.path(),
            FixtureBundleRequirements {
                require_image: true,
                require_vector: true,
                deterministic_embedding_runtime: None,
            },
        );
        assert!(read_fixture_bundle(directory.path()).is_err());
    }
}
