//! A08 combines authenticated GPU OCR with independent runner-local EG2 evidence.

use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    acceptance::{
        AcceptanceCase, AcceptanceError, AcceptanceLane, AcceptanceReceipt, AcceptanceRuntime,
        AcceptanceSubcase, ExpectedReceipt, MAX_BINARY_BYTES, MAX_FIXTURE_BYTES,
        ObservedToolIdentity, current_evaluator_sha256_matches, read_service_identity,
        runtime_from_scope, sha256_bytes, sha256_regular_file, validate_expected, validate_runtime,
        write_receipt_create_only,
    },
    acceptance_environment::IsolatedChildEnvironment,
    runner::{BoundedProcessOptions, run_bounded_command},
};

const OCR_TOOL: &str = "paddleocr_vl_local";
const EMBEDDING_TOOL: &str = kio_core::eg2::TOOL_ID;
/// Mirrors kio-adapter's documented local OCR model alias.
const OCR_MODEL: &str = "PaddleOCR-VL-0.9B";
const OCR_PDF: &str = "ocr.pdf";
const IMAGE_PNG: &str = "image.png";
#[cfg(not(windows))]
const CANDIDATE_COPY: &str = "device/candidate/kio";
#[cfg(windows)]
const CANDIDATE_COPY: &str = "device/candidate/kio.exe";
const CA_COPY: &str = "device/trust/ca.pem";
const IMAGE_QUERY_DIAGNOSTICS: &str = "diagnostics";
const MAX_OUTPUT: usize = 1024 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(300);
const EG2_TEXT: &str = "Kio acceptance checks local embedding with a public text fixture.";

#[derive(Debug, Clone)]
pub struct AuthenticatedLocalOptions {
    pub binary: PathBuf,
    /// Public directory with the reviewed OCR PDF and standalone PNG input.
    pub fixture: PathBuf,
    pub expected: ExpectedReceipt,
    pub work_dir: PathBuf,
    pub receipt: PathBuf,
    pub ocr_endpoint: String,
    pub ca_pem: PathBuf,
    pub service_identity: PathBuf,
    pub eg2_evidence: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Eg2Evidence {
    binary_sha256: String,
    fixture_sha256: String,
    profile_hash: String,
}

impl Eg2Evidence {
    fn verify(&self, binary_sha256: &str, fixture_sha256: &str) -> Result<(), AcceptanceError> {
        if self.binary_sha256 != binary_sha256
            || self.fixture_sha256 != fixture_sha256
            || self.profile_hash != kio_core::eg2::profile_hash()
        {
            return Err(invalid(
                "EG2 evidence differs from the expected candidate, fixture, or profile",
            ));
        }
        Ok(())
    }
}

struct CapturedFixture {
    pdf: Vec<u8>,
    image: Vec<u8>,
    sha256: String,
}

/// The image queries are retained before their assertions so a failed live
/// provider run preserves the product's exact JSON response.  These fixed
/// names also make each observation unambiguous without recording endpoints,
/// grants, or any other private transport configuration.
#[derive(Clone, Copy)]
enum ImageQueryStage {
    BeforeRepair,
    AfterRepair,
}

impl ImageQueryStage {
    fn diagnostic_name(self) -> &'static str {
        match self {
            Self::BeforeRepair => "image-query-before-repair.json",
            Self::AfterRepair => "image-query-after-repair.json",
        }
    }
}

/// Exercise authenticated OCR and combine it with completed runner-local EG2.
pub fn run_ocr(options: &AuthenticatedLocalOptions) -> Result<AcceptanceReceipt, AcceptanceError> {
    validate_base(options)?;
    let eg2: Eg2Evidence = serde_json::from_slice(&bounded_file(&options.eg2_evidence)?)
        .map_err(|_| invalid("runner-local EG2 evidence is malformed"))?;
    eg2.verify(
        &options.expected.candidate.binary_sha256,
        &options.expected.fixture.sha256,
    )?;
    if options.work_dir.exists() || options.receipt.exists() {
        return Err(invalid(
            "authenticated-local OCR work directory and receipt must be create-only",
        ));
    }
    let candidate = bounded_file_with_limit(&options.binary, MAX_BINARY_BYTES)?;
    let fixture = capture_fixture(&options.fixture)?;
    let ca = bounded_file(&options.ca_pem)?;
    let (service_identity, service_identity_sha256) =
        read_service_identity(&options.service_identity)?;
    if options.expected.service_identity_sha256.as_deref() != Some(&service_identity_sha256) {
        return Err(invalid("service identity differs from expected binding"));
    }
    validate_captured_service_binding(&service_identity, &ca, &options.ocr_endpoint)?;
    if ca.is_empty() {
        return Err(invalid("authenticated-local CA is empty"));
    }
    if sha256_bytes(&candidate) != options.expected.candidate.binary_sha256
        || fixture.sha256 != options.expected.fixture.sha256
    {
        return Err(invalid(
            "candidate binary or fixture differs from expected binding",
        ));
    }
    fs::create_dir(&options.work_dir).map_err(io)?;
    private_dir(&options.work_dir)?;
    let root = fs::canonicalize(&options.work_dir).map_err(io)?;
    let private = prepare_private_root(&root)?;
    write_private_candidate(&root, &candidate)?;
    let staged_ca = write_private_ca(&root, &ca)?;
    let scope = root.join("scope");
    fs::create_dir(&scope).map_err(io)?;
    private_dir(&scope)?;
    copy_fixture(&fixture, &scope)?;
    if sha256_regular_file(&root.join(CANDIDATE_COPY), MAX_BINARY_BYTES)?
        != options.expected.candidate.binary_sha256
        || fixture_digest_from_scope(&scope)? != fixture.sha256
        || sha256_regular_file(&staged_ca, MAX_FIXTURE_BYTES)? != sha256_bytes(&ca)
    {
        return Err(invalid(
            "private candidate, fixture, or CA copy changed before execution",
        ));
    }
    write_tools(&private, &options.ocr_endpoint)?;
    let binary = root.join(CANDIDATE_COPY);

    json(&binary, &private, &scope, &["init"])?;
    // The empty device ledger is an observable assertion, never a synthetic
    // ledger replacement.  `ledger init` is allowed only for that assertion.
    json(&binary, &private, &scope, &["ledger", "init"])?;
    ensure_ledger_zero(&private)?;
    let ca = utf8_path(&staged_ca, "staged CA")?;
    json(
        &binary,
        &private,
        &scope,
        &["adapter", "trust", "register", "--ca-pem", ca, "--yes"],
    )?;
    json(
        &binary,
        &private,
        &scope,
        &["adapter", "approve", OCR_TOOL, "--yes"],
    )?;
    let indexed = json(&binary, &private, &scope, &["index", "--online"])?;
    if indexed.pointer("/status").and_then(Value::as_str) != Some("indexed") {
        return Err(invalid("OCR phase did not complete an index operation"));
    }
    let searchable = json(
        &binary,
        &private,
        &scope,
        &[
            "search",
            "Kio provider OCR fixture",
            "--mode",
            "text",
            "--offline",
        ],
    )?;
    require_results(&searchable, "OCR text search")?;

    let normalized_ocr = verify_ocr_units(&scope)?;
    let revoked = json(&binary, &private, &scope, &["adapter", "revoke", OCR_TOOL])?;
    if revoked.get("status").and_then(Value::as_str) != Some("revoked") {
        return Err(invalid("OCR grant revocation did not take effect"));
    }
    if verify_ocr_units(&scope)? != normalized_ocr {
        return Err(invalid("OCR grant revocation changed normalized objects"));
    }
    ensure_ledger_zero(&private)?;
    if sha256_regular_file(&binary, MAX_BINARY_BYTES)? != options.expected.candidate.binary_sha256
        || fixture_digest_from_scope(&scope)? != options.expected.fixture.sha256
    {
        return Err(invalid("candidate binary or fixture changed during OCR"));
    }
    let runtime = observed_runtime(&scope, eg2.profile_hash)?;
    let receipt = AcceptanceReceipt {
        schema: "kio.acceptance.receipt/v4".into(),
        requirement: options.expected.requirement.clone(),
        candidate: options.expected.candidate.clone(),
        fixture: options.expected.fixture.clone(),
        workflow: options.expected.workflow.clone(),
        evaluator_sha256: current_evaluator_sha256_matches(&options.expected.evaluator_sha256)?,
        service_identity_sha256: Some(service_identity_sha256),
        contract_binary_sha256: options.expected.contract_binary_sha256.clone(),
        runtime,
        passed: true,
    };
    write_receipt_create_only(&options.receipt, &receipt)?;
    Ok(receipt)
}

/// Exercise EG2 in a separate runner-local scope without any OCR tool or route.
pub fn run_eg2(
    candidate_bin: &Path,
    fixture_dir: &Path,
    target: &str,
    expected_binary_sha256: &str,
    work_dir: &Path,
    evidence: &Path,
) -> Result<(), AcceptanceError> {
    reject_test_controls()?;
    let candidate = bounded_file_with_limit(candidate_bin, MAX_BINARY_BYTES)?;
    let binary_sha256 = sha256_bytes(&candidate);
    if binary_sha256 != expected_binary_sha256 {
        return Err(invalid(
            "EG2 candidate binary differs from the verified package",
        ));
    }
    let fixture = capture_fixture(fixture_dir)?;
    fs::create_dir(work_dir).map_err(io)?;
    private_dir(work_dir)?;
    let root = fs::canonicalize(work_dir).map_err(io)?;
    let private = prepare_private_root(&root)?;
    write_private_candidate(&root, &candidate)?;
    stage_native_library(candidate_bin, target, &root)?;
    let scope = root.join("scope");
    fs::create_dir(&scope).map_err(io)?;
    private_dir(&scope)?;
    fs::write(scope.join(IMAGE_PNG), &fixture.image).map_err(io)?;
    fs::write(scope.join("embedding.md"), EG2_TEXT).map_err(io)?;
    let binary = root.join(CANDIDATE_COPY);
    json(&binary, &private, &scope, &["init"])?;
    json(&binary, &private, &scope, &["ledger", "init"])?;
    json(
        &binary,
        &private,
        &scope,
        &["model", "install", "embeddinggemma-2", "--yes"],
    )?;
    let readiness = json(&binary, &private, &scope, &["model", "status"])?;
    if readiness.get("readiness").and_then(Value::as_str) != Some("ready") {
        return Err(invalid("EG2 model and runtime are not ready"));
    }
    let indexed = json(&binary, &private, &scope, &["index", "--offline"])?;
    if !matches!(
        indexed.get("status").and_then(Value::as_str),
        Some("indexed" | "noop")
    ) {
        return Err(invalid(
            "embedding phase did not complete an index operation",
        ));
    }
    let text = json(
        &binary,
        &private,
        &scope,
        &["search", EG2_TEXT, "--mode", "vector", "--offline"],
    )?;
    require_results(&text, "text vector query")?;
    let hybrid = json(
        &binary,
        &private,
        &scope,
        &["search", EG2_TEXT, "--mode", "hybrid", "--offline"],
    )?;
    require_results(&hybrid, "hybrid text query")?;
    let image = captured_image_query(
        &binary,
        &private,
        &scope,
        ImageQueryStage::BeforeRepair,
        &[
            "search",
            "standalone acceptance image",
            "--mode",
            "image",
            "--offline",
        ],
    )?;
    let fixture_image_sha256 = sha256_bytes(&bounded_file(&scope.join(IMAGE_PNG))?);
    let image_uri = require_image_result(&image, &fixture_image_sha256)?;
    let image_rows_before_repair = stable_result_rows(&image)?;
    let opened = json(&binary, &private, &scope, &["open", &image_uri])?;
    assert_opened_image(&opened, &scope.join(IMAGE_PNG))?;
    assert_vectors(&scope)?;
    let stored_vectors = vector_projection_digest(&scope)?;
    let stored_text = json(
        &binary,
        &private,
        &scope,
        &["search", EG2_TEXT, "--mode", "text", "--offline"],
    )?;
    require_results(&stored_text, "stored text query")?;
    let stored_text_rows = stable_result_rows(&stored_text)?;
    // Rebuild the exact persisted vectors from the scope's CAS.
    json(
        &binary,
        &private,
        &scope,
        &["repair", "rebuild-db", "--offline"],
    )?;
    json(&binary, &private, &scope, &["repair", "replica"])?;
    assert_vectors(&scope)?;
    if vector_projection_digest(&scope)? != stored_vectors {
        return Err(invalid(
            "offline repair changed persisted text or image vector projections",
        ));
    }
    let rebuilt_text = json(
        &binary,
        &private,
        &scope,
        &["search", EG2_TEXT, "--mode", "text", "--offline"],
    )?;
    let rebuilt_image = captured_image_query(
        &binary,
        &private,
        &scope,
        ImageQueryStage::AfterRepair,
        &[
            "search",
            "standalone acceptance image",
            "--mode",
            "image",
            "--offline",
        ],
    )?;
    require_results(&rebuilt_text, "rebuilt text query")?;
    require_results(&rebuilt_image, "rebuilt hybrid query")?;
    require_image_result(&rebuilt_image, &fixture_image_sha256)?;
    if stable_result_rows(&rebuilt_text)? != stored_text_rows
        || stable_result_rows(&rebuilt_image)? != image_rows_before_repair
    {
        return Err(invalid(
            "offline repair changed stored text or image search results",
        ));
    }

    ensure_ledger_zero(&private)?;
    if sha256_regular_file(&binary, MAX_BINARY_BYTES)? != binary_sha256
        || bounded_file(&scope.join(IMAGE_PNG))? != fixture.image
        || bounded_file(&scope.join("embedding.md"))? != EG2_TEXT.as_bytes()
    {
        return Err(invalid("candidate binary or fixture changed during EG2"));
    }
    let proof = Eg2Evidence {
        binary_sha256,
        fixture_sha256: fixture.sha256,
        profile_hash: tool_profile(
            &scope.join(".kio/tool-lock.json"),
            "embedding",
            EMBEDDING_TOOL,
        )?,
    };
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(evidence)
        .map_err(io)?;
    file.write_all(&canonical(&proof)?).map_err(io)?;
    file.sync_all().map_err(io)
}

fn validate_base(options: &AuthenticatedLocalOptions) -> Result<(), AcceptanceError> {
    reject_test_controls()?;
    validate_expected(&options.expected)?;
    let requirement = &options.expected.requirement;
    if requirement.case != AcceptanceCase::A08
        || requirement.lane != AcceptanceLane::ProviderLive
        || requirement.subcase != AcceptanceSubcase::AuthenticatedLocal
    {
        return Err(invalid(
            "authenticated-local executor received a different requirement",
        ));
    }
    current_evaluator_sha256_matches(&options.expected.evaluator_sha256)?;
    Ok(())
}

fn validate_captured_service_binding(
    identity: &crate::acceptance::ServiceIdentity,
    ca: &[u8],
    ocr_endpoint: &str,
) -> Result<(), AcceptanceError> {
    if identity.ocr_endpoint != ocr_endpoint
        || identity.ca_sha256 != format!("sha256:{}", sha256_bytes(ca))
    {
        return Err(invalid(
            "captured CA or endpoint differs from service identity",
        ));
    }
    Ok(())
}
fn prepare_private_root(root: &Path) -> Result<PathBuf, AcceptanceError> {
    let private = root.join("device");
    for part in ["", "xdg-config", "xdg-data", "xdg-cache", "tmp"] {
        let path = if part.is_empty() {
            private.clone()
        } else {
            private.join(part)
        };
        fs::create_dir(&path).map_err(io)?;
        private_dir(&path)?;
    }
    Ok(private)
}

fn stage_native_library(binary: &Path, target: &str, root: &Path) -> Result<(), AcceptanceError> {
    let bundle = kio_core::litert::Bundle::for_target(target)
        .ok_or_else(|| invalid("unsupported EG2 runtime target"))?;
    let package = binary
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| invalid("candidate binary has no distribution root"))?;
    let directory = PathBuf::from(bundle.directory());
    let library = bundle.library.name();
    let bytes =
        bounded_file_with_limit(&package.join(&directory).join(library), 256 * 1024 * 1024)?;
    if sha256_bytes(&bytes) != bundle.library.sha256 {
        return Err(invalid(
            "candidate native runtime differs from the pinned artifact",
        ));
    }
    let destination = root.join("device").join(directory);
    fs::create_dir_all(&destination).map_err(io)?;
    let path = destination.join(library);
    fs::write(&path, bytes).map_err(io)?;
    private_file(&path)
}

fn write_private_candidate(root: &Path, bytes: &[u8]) -> Result<(), AcceptanceError> {
    let directory = root.join("device/candidate");
    fs::create_dir(&directory).map_err(io)?;
    private_dir(&directory)?;
    let path = root.join(CANDIDATE_COPY);
    fs::write(&path, bytes).map_err(io)?;
    private_executable(&path)
}

fn write_private_ca(root: &Path, bytes: &[u8]) -> Result<PathBuf, AcceptanceError> {
    use kio_core::{control::ControlHandle, private_fs};

    let path = root.join(CA_COPY);
    let directory = private_fs::retain_or_create_external_directory_with_control(
        &root.join("device/trust"),
        &ControlHandle::default(),
    )
    .map_err(|error| AcceptanceError::Io(error.to_string()))?;
    directory
        .create_private_file(Path::new("ca.pem"))
        .map_err(|error| AcceptanceError::Io(error.to_string()))?
        .write_all(bytes)
        .map_err(io)?;
    Ok(path)
}

fn write_tools(private: &Path, ocr: &str) -> Result<(), AcceptanceError> {
    let body = format!(
        "[markdown.{OCR_TOOL}]\nkind = \"offline_api\"\nurl = {}\nmodel = {}\n",
        toml_string(ocr)?,
        toml_string(OCR_MODEL)?
    );
    let config = private.join("xdg-config/kio");
    fs::create_dir(&config).map_err(io)?;
    private_dir(&config)?;
    let tools = config.join("tools.toml");
    fs::write(&tools, body).map_err(io)?;
    private_file(&tools)
}
fn copy_fixture(fixture: &CapturedFixture, scope: &Path) -> Result<(), AcceptanceError> {
    fs::write(scope.join(OCR_PDF), &fixture.pdf).map_err(io)?;
    fs::write(scope.join(IMAGE_PNG), &fixture.image).map_err(io)?;
    Ok(())
}
fn capture_fixture(root: &Path) -> Result<CapturedFixture, AcceptanceError> {
    if !root.is_dir() {
        return Err(invalid("authenticated-local fixture must be a directory"));
    }
    let pdf = bounded_file(&root.join(OCR_PDF))?;
    let image = bounded_file(&root.join(IMAGE_PNG))?;
    let mut bytes = Vec::new();
    for (name, data) in [(OCR_PDF, &pdf), (IMAGE_PNG, &image)] {
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(data);
        bytes.push(0);
    }
    Ok(CapturedFixture {
        pdf,
        image,
        sha256: sha256_bytes(&bytes),
    })
}
fn fixture_digest_from_scope(scope: &Path) -> Result<String, AcceptanceError> {
    capture_fixture(scope).map(|fixture| fixture.sha256)
}
fn bounded_file(path: &Path) -> Result<Vec<u8>, AcceptanceError> {
    bounded_file_with_limit(path, MAX_FIXTURE_BYTES)
}
fn bounded_file_with_limit(path: &Path, maximum: u64) -> Result<Vec<u8>, AcceptanceError> {
    let file = kio_core::cas::open_regular_nofollow(path)
        .map_err(|_| invalid("acceptance input is not a stable regular file"))?;
    let meta = file.metadata().map_err(io)?;
    if !meta.is_file() || meta.len() > maximum {
        return Err(invalid("fixture leaf must be a bounded regular file"));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    file.take(maximum + 1).read_to_end(&mut bytes).map_err(io)?;
    if bytes.len() as u64 > maximum {
        return Err(invalid("acceptance input exceeds its bound"));
    }
    Ok(bytes)
}

fn json(
    binary: &Path,
    private: &Path,
    scope: &Path,
    args: &[&str],
) -> Result<Value, AcceptanceError> {
    parse_command_json(json_stdout(binary, private, scope, args)?)
}

/// Execute an image query and retain its bounded, exact command stdout before
/// the result-shape assertion.  A failed assertion must not erase the response
/// that explains it; a pre-existing diagnostic leaf rejects a retry rather than
/// overwriting evidence from an earlier attempt.
fn captured_image_query(
    binary: &Path,
    private: &Path,
    scope: &Path,
    stage: ImageQueryStage,
    args: &[&str],
) -> Result<Value, AcceptanceError> {
    let stdout = json_stdout(binary, private, scope, args)?;
    write_image_query_diagnostic(private, stage, stdout.as_bytes())?;
    parse_command_json(stdout)
}

fn json_stdout(
    binary: &Path,
    private: &Path,
    scope: &Path,
    args: &[&str],
) -> Result<String, AcceptanceError> {
    let mut command = Command::new(binary);
    IsolatedChildEnvironment::new(
        private,
        private.join("xdg-config"),
        private.join("xdg-data"),
        private.join("xdg-cache"),
        private.join("tmp"),
    )
    .apply(&mut command)?;
    command.current_dir(scope).arg("--json").args(args);
    let output = run_bounded_command(
        &mut command,
        BoundedProcessOptions {
            timeout: COMMAND_TIMEOUT,
            max_stdout_bytes: MAX_OUTPUT,
            max_stderr_bytes: MAX_OUTPUT / 4,
        },
        None,
    )
    .map_err(|error| {
        AcceptanceError::Command(format!("authenticated-local command failed: {error}"))
    })?;
    if !output.status.success() {
        return Err(AcceptanceError::Command(format!(
            "{} failed: {}",
            args.join(" "),
            output.stderr.trim()
        )));
    }
    Ok(output.stdout)
}

fn parse_command_json(stdout: String) -> Result<Value, AcceptanceError> {
    serde_json::from_str(&stdout)
        .map_err(|_| invalid("authenticated-local command emitted invalid JSON"))
}

fn write_image_query_diagnostic(
    private: &Path,
    stage: ImageQueryStage,
    stdout: &[u8],
) -> Result<(), AcceptanceError> {
    if stdout.len() > MAX_OUTPUT {
        return Err(invalid("image query diagnostic exceeds its bound"));
    }
    let directory = private.join(IMAGE_QUERY_DIAGNOSTICS);
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            private_dir(&directory)?;
        }
        Ok(_) => return Err(invalid("image query diagnostic parent is unsafe")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(&directory).map_err(io)?;
            private_dir(&directory)?;
        }
        Err(error) => return Err(io(error)),
    }
    let path = directory.join(stage.diagnostic_name());
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(io)?;
    file.write_all(stdout).map_err(io)?;
    file.sync_all().map_err(io)?;
    drop(file);
    private_file(&path)
}

fn require_results(value: &Value, label: &str) -> Result<(), AcceptanceError> {
    if value
        .pointer("/results")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
    {
        return Err(invalid(format!("{label} returned no results")));
    }
    Ok(())
}
fn require_image_result(value: &Value, fixture_sha256: &str) -> Result<String, AcceptanceError> {
    let fixture_path = format!("object/image/sha256:{fixture_sha256}");
    value
        .pointer("/results")
        .and_then(Value::as_array)
        .and_then(|results| {
            results
                .iter()
                .filter(|row| row.get("result_type").and_then(Value::as_str) == Some("image"))
                .filter_map(|row| row.get("payload_uri").and_then(Value::as_str))
                .find(|uri| {
                    uri.strip_prefix("kio://")
                        .and_then(|rest| rest.split_once('/'))
                        .is_some_and(|(scope, path)| !scope.is_empty() && path == fixture_path)
                })
        })
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            invalid("image vector query returned no image object URI matching the fixture")
        })
}
fn assert_opened_image(value: &Value, expected: &Path) -> Result<(), AcceptanceError> {
    let path = value
        .get("path")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| invalid("image open returned no materialized path"))?;
    if bounded_file(&path)? != bounded_file(expected)? {
        return Err(invalid("opened image bytes differ from the public fixture"));
    }
    Ok(())
}
fn ensure_ledger_zero(private: &Path) -> Result<(), AcceptanceError> {
    let path = private.join("xdg-data/kio/cost-ledger.sqlite");
    let db =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|_| invalid("authenticated-local ledger is unavailable"))?;
    let rows: i64 = db
        .query_row("SELECT COUNT(*) FROM cost_ledger", [], |row| row.get(0))
        .map_err(|_| invalid("authenticated-local ledger cannot be read"))?;
    if rows != 0 {
        return Err(invalid(
            "authenticated-local local execution recorded a cost ledger row",
        ));
    }
    Ok(())
}

fn assert_vectors(scope: &Path) -> Result<(), AcceptanceError> {
    kio_index::vec::ensure_registered();
    let db = rusqlite::Connection::open_with_flags(
        scope.join(".kio/index/sqlite.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|_| invalid("embedding index database is unavailable"))?;
    let (text, image): (i64, i64) = db
        .query_row(
            "SELECT (SELECT COUNT(*) FROM chunk_vec), (SELECT COUNT(*) FROM image_vec)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| invalid("embedding vector projections are unavailable"))?;
    if text == 0 || image == 0 {
        return Err(invalid("text or image vector projection is empty"));
    }
    let profile = tool_profile(
        &scope.join(".kio/tool-lock.json"),
        "embedding",
        EMBEDDING_TOOL,
    )?;
    if profile != kio_core::eg2::profile_hash() {
        return Err(invalid(
            "scope does not use the pinned EG2 semantic profile",
        ));
    }
    let store = kio_core::cas::ObjectStore::new(scope.join(".kio"));
    let mut targets = std::collections::BTreeSet::new();
    for hash in store
        .embedding_hashes()
        .map_err(|error| invalid(error.to_string()))?
    {
        let object = store
            .read_embedding(&hash)
            .map_err(|error| invalid(error.to_string()))?;
        if object.profile_hash != profile
            || object.metadata.producer.execution_mode
                != kio_core::cas::ProducerExecutionMode::NativeLibrary
        {
            return Err(invalid("CAS lacks EG2 native producer provenance"));
        }
        targets.insert(object.target_type);
    }
    if !targets.contains("chunk") || !targets.contains("image") {
        return Err(invalid("CAS lacks native text or image embeddings"));
    }
    for (table, id, target) in [
        ("chunk_vec", "chunk_id", "chunk"),
        ("image_vec", "image_id", "image"),
    ] {
        let mut stmt = db
            .prepare(&format!("SELECT {id} FROM {table}"))
            .map_err(|_| invalid("vector projection cannot be read"))?;
        let ids = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|_| invalid("vector projection cannot be enumerated"))?;
        let mut seen = 0_u64;
        for id in ids {
            let id = id.map_err(|_| invalid("vector identity is malformed"))?;
            let vector = if target == "chunk" {
                kio_index::embedding_store::read_chunk_vector(&db, &id)
            } else {
                kio_index::embedding_store::read_image_vector(&db, &id)
            }
            .map_err(|_| invalid("stored local vector cannot be read"))?
            .ok_or_else(|| invalid("vector projection lacks its stored vector"))?;
            assert_unit_vector(&vector)?;
            seen += 1;
        }
        if seen == 0 {
            return Err(invalid("text or image vector projection is empty"));
        }
        let persisted: i64 = db.query_row("SELECT COUNT(*) FROM embeddings WHERE target_type = ?1 AND profile_hash = ?2 AND dimensions = 768", [target, profile.as_str()], |row| row.get(0)).map_err(|_| invalid("stored embedding provenance is unavailable"))?;
        if persisted < seen as i64 {
            return Err(invalid(
                "vector projection lacks the local embedding profile",
            ));
        }
    }
    Ok(())
}

/// Bind the derived sqlite-vec projections to their exact stored values before
/// and after an offline rebuild, without issuing an embedding request.
fn vector_projection_digest(scope: &Path) -> Result<String, AcceptanceError> {
    kio_index::vec::ensure_registered();
    let db = rusqlite::Connection::open_with_flags(
        scope.join(".kio/index/sqlite.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|_| invalid("embedding index database is unavailable"))?;
    let mut evidence = Vec::new();
    for (table, id, target) in [
        ("chunk_vec", "chunk_id", "chunk"),
        ("image_vec", "image_id", "image"),
    ] {
        let mut stmt = db
            .prepare(&format!("SELECT {id} FROM {table} ORDER BY {id}"))
            .map_err(|_| invalid("vector projection cannot be enumerated"))?;
        let ids = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|_| invalid("vector identity is malformed"))?;
        let mut count = 0_u64;
        for id in ids {
            let id = id.map_err(|_| invalid("vector identity is malformed"))?;
            let vector = if target == "chunk" {
                kio_index::embedding_store::read_chunk_vector(&db, &id)
            } else {
                kio_index::embedding_store::read_image_vector(&db, &id)
            }
            .map_err(|_| invalid("stored local vector cannot be read"))?
            .ok_or_else(|| invalid("vector projection lacks its stored vector"))?;
            evidence.extend_from_slice(target.as_bytes());
            evidence.push(0);
            evidence.extend_from_slice(id.as_bytes());
            evidence.push(0);
            for value in vector {
                evidence.extend_from_slice(&value.to_le_bytes());
            }
            count += 1;
        }
        if count == 0 {
            return Err(invalid("text or image vector projection is empty"));
        }
    }
    Ok(sha256_bytes(&evidence))
}

fn stable_result_rows(response: &Value) -> Result<Value, AcceptanceError> {
    response
        .pointer("/results")
        .filter(|value| value.is_array())
        .cloned()
        .ok_or_else(|| invalid("search returned no result rows"))
}

fn assert_unit_vector(vector: &[f32]) -> Result<(), AcceptanceError> {
    if vector.len() != 768 {
        return Err(invalid("embedding vector is not 768-dimensional"));
    }
    let norm = vector
        .iter()
        .map(|value| f64::from(*value))
        .map(|value| value * value)
        .sum::<f64>()
        .sqrt();
    if !norm.is_finite() || (norm - 1.0).abs() > 1e-3 {
        return Err(invalid("embedding vector is not unit normalized"));
    }
    Ok(())
}

fn require_tool(lock: &Path, role: &str, id: &str) -> Result<(), AcceptanceError> {
    let value: Value = serde_json::from_slice(&bounded_file(lock)?)
        .map_err(|_| invalid("tool-lock is malformed"))?;
    if value
        .pointer(&format!("/{role}/tool_id"))
        .and_then(Value::as_str)
        != Some(id)
    {
        return Err(invalid("tool-lock lacks the executed local adapter"));
    }
    Ok(())
}
fn tool_profile(lock: &Path, role: &str, id: &str) -> Result<String, AcceptanceError> {
    require_tool(lock, role, id)?;
    let value: Value = serde_json::from_slice(&bounded_file(lock)?)
        .map_err(|_| invalid("tool-lock is malformed"))?;
    value
        .pointer(&format!("/{role}/profile_hash"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| invalid("tool-lock profile hash is missing"))
}

/// Reopen the immutable normalized manifests through the pipeline's validation
/// boundary.  Counts alone cannot distinguish a local OCR result from a
/// placeholder or a stale projection, so each public input must retain a
/// non-empty local-profile unit.
fn verify_ocr_units(scope: &Path) -> Result<String, AcceptanceError> {
    let profile = real_ocr_profile();
    kio_index::vec::ensure_registered();
    let db = rusqlite::Connection::open_with_flags(
        scope.join(".kio/index/sqlite.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|_| invalid("OCR index database is unavailable"))?;
    let repository = kio_core::scope::Repository::open(scope)
        .map_err(|_| invalid("OCR scope repository is unavailable"))?;
    let head = repository
        .head_commit_hash()
        .map_err(|_| invalid("OCR scope HEAD is unavailable"))?
        .ok_or_else(|| invalid("OCR scope has no published HEAD"))?;
    let mut evidence = Vec::new();
    for path in [OCR_PDF, IMAGE_PNG] {
        let (raw_hash, stored_profile, generation): (String, String, i64) = db.query_row(
            "SELECT raw_hash, tool_profile_hash, gen FROM tree_entries WHERE commit_hash = ?1 AND path = ?2",
            [&head, path], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).map_err(|_| invalid("OCR provenance is missing a public fixture input"))?;
        if generation < 0 || !is_real_ocr_profile(&stored_profile, &profile) {
            return Err(invalid(
                "OCR provenance does not bind the local OCR profile",
            ));
        }
        let instance = kio_pipeline::markdownize::load_validated_normalized_instance(
            scope.join(".kio"),
            &raw_hash,
            &profile,
            generation as u64,
        )
        .map_err(|_| invalid("OCR normalized manifest or units are invalid"))?;
        if instance.units.is_empty()
            || instance.units.iter().any(|unit| {
                unit.tool_profile_hash != profile
                    || unit.markdown.trim().is_empty()
                    || unit.markdown.to_ascii_lowercase().contains("placeholder")
            })
        {
            return Err(invalid(
                "OCR normalized output is empty, placeholder, or foreign",
            ));
        }
        evidence.extend_from_slice(path.as_bytes());
        evidence.push(0);
        evidence.extend_from_slice(raw_hash.as_bytes());
        evidence.push(0);
        evidence.extend_from_slice(profile.as_bytes());
        evidence.push(0);
        evidence.extend_from_slice(&canonical(&instance.manifest)?);
        for unit in instance.units {
            evidence.extend_from_slice(&canonical(&unit)?);
        }
    }
    Ok(sha256_bytes(&evidence))
}
fn observed_runtime(
    scope: &Path,
    eg2_profile: String,
) -> Result<AcceptanceRuntime, AcceptanceError> {
    let locked = runtime_from_scope(scope)?;
    let mut tools: Vec<_> = locked
        .tools
        .into_iter()
        .filter(|tool| tool.tool_id == "prepare_default")
        .collect();
    if tools.is_empty() {
        return Err(invalid("authenticated OCR tool-lock lacks prepare_default"));
    }
    tools.extend([
        ObservedToolIdentity {
            tool_id: OCR_TOOL.to_owned(),
            tool_profile_hash: real_ocr_profile(),
        },
        ObservedToolIdentity {
            tool_id: EMBEDDING_TOOL.to_owned(),
            tool_profile_hash: eg2_profile,
        },
    ]);
    tools.sort_by(|left, right| left.tool_id.cmp(&right.tool_id));
    let runtime = AcceptanceRuntime {
        tools,
        converter: None,
    };
    validate_runtime(&runtime)?;
    Ok(runtime)
}

fn real_ocr_profile() -> String {
    kio_adapter::local_ocr_markdownize::profile_for(
        kio_adapter::local_ocr_markdownize::LocalOcrExecution::Real,
    )
    .tool_profile_hash
}

fn is_real_ocr_profile(stored_profile: &str, expected_profile: &str) -> bool {
    stored_profile == expected_profile
}

fn canonical<T: Serialize>(value: &T) -> Result<Vec<u8>, AcceptanceError> {
    let value = serde_json::to_value(value).map_err(|e| AcceptanceError::Json(e.to_string()))?;
    kio_core::cas::canonical_json_bytes(&value).map_err(|e| AcceptanceError::Json(e.to_string()))
}
fn utf8_path<'a>(path: &'a Path, name: &str) -> Result<&'a str, AcceptanceError> {
    path.to_str()
        .ok_or_else(|| invalid(format!("{name} path is not UTF-8")))
}
fn toml_string(value: &str) -> Result<String, AcceptanceError> {
    Ok(toml::Value::String(value.to_owned()).to_string())
}
fn reject_test_controls() -> Result<(), AcceptanceError> {
    if std::env::vars_os().any(|(key, _)| {
        let key = key.to_string_lossy();
        key.starts_with("KIO_TEST_")
            || key.starts_with("KIO_MOCK_")
            || key == "KIO_EVAL_DETERMINISTIC_EMBED"
    }) {
        return Err(invalid(
            "authenticated-local acceptance refuses test, mock, and deterministic evaluator controls",
        ));
    }
    Ok(())
}
#[cfg(unix)]
fn private_dir(path: &Path) -> Result<(), AcceptanceError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(io)
}
#[cfg(unix)]
fn private_file(path: &Path) -> Result<(), AcceptanceError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(io)
}
#[cfg(unix)]
fn private_executable(path: &Path) -> Result<(), AcceptanceError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(io)
}
#[cfg(not(unix))]
fn private_file(_: &Path) -> Result<(), AcceptanceError> {
    Ok(())
}
#[cfg(not(unix))]
fn private_executable(_: &Path) -> Result<(), AcceptanceError> {
    Ok(())
}
#[cfg(not(unix))]
fn private_dir(_: &Path) -> Result<(), AcceptanceError> {
    Ok(())
}
fn invalid(message: impl Into<String>) -> AcceptanceError {
    AcceptanceError::Invalid(message.into())
}
fn io(error: std::io::Error) -> AcceptanceError {
    AcceptanceError::Io(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_ca_passes_private_file_admission() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        prepare_private_root(&root).unwrap();

        let bytes = b"staged CA";
        let path = write_private_ca(&root, bytes).unwrap();

        assert_eq!(path, root.join(CA_COPY));
        let parent = path.parent().unwrap();
        let retained = kio_core::private_fs::retain_external_directory(parent).unwrap();
        assert_eq!(
            kio_core::private_fs::read_private_file_at_path(
                &retained,
                Path::new(path.file_name().unwrap()),
                MAX_FIXTURE_BYTES,
            )
            .unwrap(),
            bytes
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(parent), 0o700);
        }
    }

    #[test]
    fn tools_configuration_contains_only_declared_roles_and_escapes_endpoint() {
        let temporary = tempfile::tempdir().unwrap();
        let private = prepare_private_root(temporary.path()).unwrap();
        let endpoint = "https://127.0.0.1:9443/ocr?name=\\\"quoted\\\"";

        write_tools(&private, endpoint).unwrap();

        let tools = fs::read_to_string(private.join("xdg-config/kio/tools.toml")).unwrap();
        let parsed: toml::Value = toml::from_str(&tools).unwrap();
        assert_eq!(parsed["markdown"][OCR_TOOL]["url"].as_str(), Some(endpoint));
        assert_eq!(
            parsed["markdown"][OCR_TOOL]["model"].as_str(),
            Some(OCR_MODEL)
        );
        assert!(parsed.get("embedding").is_none());
        assert!(parsed.get("adapter").is_none());
        assert!(!tools.contains("ca_pem_path"));
    }

    #[test]
    fn eg2_evidence_requires_the_same_candidate_fixture_and_profile() {
        let mut evidence = Eg2Evidence {
            binary_sha256: "a".repeat(64),
            fixture_sha256: "b".repeat(64),
            profile_hash: kio_core::eg2::profile_hash().to_owned(),
        };
        let binary = "a".repeat(64);
        let fixture = "b".repeat(64);
        assert!(evidence.verify(&binary, &fixture).is_ok());
        assert!(evidence.verify(&fixture, &fixture).is_err());
        assert!(evidence.verify(&binary, &binary).is_err());
        evidence.profile_hash = "sha256:wrong".into();
        assert!(evidence.verify(&binary, &fixture).is_err());
    }

    #[test]
    fn captured_service_binding_rejects_ca_or_endpoint_mismatch_before_cli() {
        let ca = b"captured-ca";
        let identity = crate::acceptance::ServiceIdentity {
            schema: "kio.local-gpu.ocr-identity/v1".into(),
            run_nonce: "run".into(),
            ocr_endpoint: "https://127.0.0.1:18080".into(),
            ca_sha256: format!("sha256:{}", sha256_bytes(ca)),
            leaf_cert_sha256: String::new(),
            controller_sha256: String::new(),
            proxy_sha256: String::new(),
            entrypoint_sha256: String::new(),
            ocr_compose_sha256: String::new(),
            backend_config_sha256: String::new(),
            ocr_image: String::new(),
            ocr_revision: String::new(),
            ocr_weight_sha256: String::new(),
        };
        assert!(
            validate_captured_service_binding(&identity, ca, "https://127.0.0.1:18080",).is_ok()
        );
        assert!(
            validate_captured_service_binding(&identity, b"other-ca", "https://127.0.0.1:18080",)
                .is_err()
        );
        assert!(
            validate_captured_service_binding(&identity, ca, "https://127.0.0.1:28080",).is_err()
        );
    }

    #[test]
    fn real_ocr_profile_is_retained_when_builtin_is_primary_lock() {
        let temporary = tempfile::tempdir().unwrap();
        let scope = temporary.path();
        fs::create_dir(scope.join(".kio")).unwrap();
        fs::write(
            scope.join(".kio/tool-lock.json"),
            serde_json::json!({
                "markdown": {
                    "tool_id": "deterministic_builtin",
                    "profile_hash": format!("sha256:{}", "a".repeat(64)),
                },
                "prepare": {
                    "tool_id": "prepare_default",
                    "profile_hash": format!("sha256:{}", "b".repeat(64)),
                },
                "embedding": {
                    "tool_id": EMBEDDING_TOOL,
                    "profile_hash": format!("sha256:{}", "c".repeat(64)),
                },
                "spec_version": 1,
            })
            .to_string(),
        )
        .unwrap();

        let real_profile = real_ocr_profile();
        let runtime = observed_runtime(scope, kio_core::eg2::profile_hash().to_owned()).unwrap();

        assert!(is_real_ocr_profile(&real_profile, &real_profile));
        assert_eq!(
            runtime
                .tools
                .iter()
                .map(|tool| tool.tool_id.as_str())
                .collect::<Vec<_>>(),
            vec![EMBEDDING_TOOL, "paddleocr_vl_local", "prepare_default"]
        );
        assert_eq!(
            runtime
                .tools
                .iter()
                .find(|tool| tool.tool_id == OCR_TOOL)
                .map(|tool| tool.tool_profile_hash.as_str()),
            Some(real_profile.as_str())
        );
    }

    #[test]
    fn wrong_per_file_ocr_profile_is_denied() {
        let real_profile = real_ocr_profile();
        assert!(!is_real_ocr_profile(
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            &real_profile,
        ));
    }

    #[test]
    fn image_search_selects_fixture_by_content() {
        let fixture_sha256 = sha256_bytes(b"fixture");
        let other_uri = format!("kio://scope/object/image/sha256:{}", sha256_bytes(b"other"));
        let fixture_uri = format!("kio://scope/object/image/sha256:{fixture_sha256}");
        let mut response = serde_json::json!({"results": [
            {"result_type": "image", "payload_uri": other_uri},
            {"result_type": "image", "payload_uri": fixture_uri},
        ]});
        assert_eq!(
            require_image_result(&response, &fixture_sha256).unwrap(),
            fixture_uri
        );
        response["results"].as_array_mut().unwrap().pop();
        assert!(require_image_result(&response, &fixture_sha256).is_err());
    }

    #[test]
    fn image_query_diagnostic_preserves_a_three_row_response_create_only() {
        let temporary = tempfile::tempdir().unwrap();
        let private = prepare_private_root(temporary.path()).unwrap();
        let response = br#"{"results":[
            {"result_type":"chunk","chunk_hash":"sha256:pdf"},
            {"result_type":"chunk","chunk_hash":"sha256:image-markdown"},
            {"result_type":"chunk","chunk_hash":"sha256:other"}
        ]}"#;

        write_image_query_diagnostic(&private, ImageQueryStage::BeforeRepair, response).unwrap();

        let path = private
            .join(IMAGE_QUERY_DIAGNOSTICS)
            .join(ImageQueryStage::BeforeRepair.diagnostic_name());
        assert_eq!(fs::read(&path).unwrap(), response);
        let captured: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(require_image_result(&captured, &sha256_bytes(b"fixture")).is_err());
        assert!(
            write_image_query_diagnostic(&private, ImageQueryStage::BeforeRepair, response)
                .is_err()
        );

        write_image_query_diagnostic(&private, ImageQueryStage::AfterRepair, response).unwrap();
    }
}
