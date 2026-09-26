//! Bounded A08 execution against the production Mistral and Gemini endpoints.
//!
//! This runner deliberately drives only the released `kio` CLI.  It neither
//! reimplements adapter requests nor accepts a mock seam as evidence.  A
//! reservation is read before the first child starts and remains unknown if a
//! child times out or returns an indeterminate provider result.

use std::{
    env,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;

use crate::{
    acceptance::{
        AcceptanceCase, AcceptanceError, AcceptanceLane, AcceptanceReceipt, AcceptanceSubcase,
        ExpectedReceipt, MAX_BINARY_BYTES, current_evaluator_sha256_matches, runtime_from_scope,
        sha256_bytes, sha256_regular_file, validate_expected, write_receipt_create_only,
    },
    acceptance_environment::IsolatedChildEnvironment,
    provider_budget::validate_exact_reservation,
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(90);
const POLL_LIMIT: usize = 8;
const MAX_CHILD_OUTPUT: u64 = 256 * 1024;
const FIXTURE_MAX_BYTES: u64 = 64 * 1024;

/// The provider selected by the guarded workflow.  It is intentionally not a
/// CLI string accepted by the product binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Mistral,
    Gemini,
}

impl Provider {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mistral => "mistral",
            Self::Gemini => "gemini",
        }
    }

    fn credential_name(self) -> &'static str {
        match self {
            Self::Mistral => "MISTRAL_API_KEY",
            Self::Gemini => "GEMINI_API_KEY",
        }
    }

    fn subcase(self) -> AcceptanceSubcase {
        match self {
            Self::Mistral => AcceptanceSubcase::Mistral,
            Self::Gemini => AcceptanceSubcase::Gemini,
        }
    }
}

#[derive(Debug, Clone)]
pub struct A08ProviderOptions {
    pub binary: PathBuf,
    pub fixture: PathBuf,
    pub expected: ExpectedReceipt,
    pub work_dir: PathBuf,
    pub receipt: PathBuf,
    pub provider: Provider,
    /// Previously reconstructed campaign state.  This executor never creates
    /// a campaign or a reservation, because that could reset a failed run.
    pub reservation_state_dir: PathBuf,
    pub campaign_id: String,
    pub allocation_id: String,
}

/// Run exactly one provider's A08 subcase.  The Mistral subcase creates and
/// completes one OCR batch.  The Gemini subcase covers a realtime document and
/// query/image embedding, then one batch document embedding and bounded polls.
pub fn run_a08_provider(
    options: &A08ProviderOptions,
) -> Result<AcceptanceReceipt, AcceptanceError> {
    validate_provider_options(options)?;
    reject_mock_environment()?;
    let key = require_production_credential(options.provider)?;
    verify_reservation(options)?;
    let fixture = load_fixture(&options.fixture)?;
    let isolated = prepare_isolated_root(&options.work_dir)?;
    let scope = isolated.join("scope");
    fs::create_dir(&scope).map_err(io_error)?;
    write_tools_config(&isolated, options.provider)?;
    materialize_fixture(&scope, options.provider, &fixture)?;

    run_json(
        &options.binary,
        &isolated,
        &scope,
        &["--json", "init"],
        options.provider,
        &key,
    )?;
    run_json(
        &options.binary,
        &isolated,
        &scope,
        &["--json", "ledger", "init"],
        options.provider,
        &key,
    )?;
    match options.provider {
        Provider::Mistral => run_mistral(options, &isolated, &scope, &key)?,
        Provider::Gemini => run_gemini(options, &isolated, &scope, &key)?,
    }
    let mut runtime = runtime_from_scope(&scope)?;
    let adapter_kind = match options.provider {
        Provider::Mistral => "markdownize",
        Provider::Gemini => "embedding",
    };
    let ledger_profile = observed_batch_profile(&isolated, adapter_kind)?;
    if !runtime
        .tools
        .iter()
        .any(|tool| tool.tool_profile_hash == ledger_profile)
    {
        return Err(invalid(
            "provider ledger profile does not match persisted tool identity",
        ));
    }
    runtime.tools.retain(|tool| {
        tool.tool_profile_hash == ledger_profile
            || (options.provider == Provider::Mistral && tool.tool_id == "mistral_ocr_markdownize")
            || (options.provider == Provider::Gemini && tool.tool_id == "gemini_embedding_2")
    });

    let receipt = AcceptanceReceipt {
        schema: "kio.acceptance.receipt/v4".to_owned(),
        requirement: options.expected.requirement.clone(),
        candidate: options.expected.candidate.clone(),
        fixture: options.expected.fixture.clone(),
        workflow: options.expected.workflow.clone(),
        evaluator_sha256: current_evaluator_sha256_matches(&options.expected.evaluator_sha256)?,
        service_identity_sha256: options.expected.service_identity_sha256.clone(),
        contract_binary_sha256: options.expected.contract_binary_sha256.clone(),
        runtime,
        passed: true,
    };
    write_receipt_create_only(&options.receipt, &receipt)?;
    Ok(receipt)
}

fn validate_provider_options(options: &A08ProviderOptions) -> Result<(), AcceptanceError> {
    validate_expected(&options.expected)?;
    current_evaluator_sha256_matches(&options.expected.evaluator_sha256)?;
    let requirement = &options.expected.requirement;
    if requirement.case != AcceptanceCase::A08
        || requirement.lane != AcceptanceLane::ProviderLive
        || requirement.subcase != options.provider.subcase()
    {
        return Err(invalid(
            "A08 provider requirement does not match selected provider",
        ));
    }
    if sha256_regular_file(&options.binary, MAX_BINARY_BYTES)?
        != options.expected.candidate.binary_sha256
    {
        return Err(invalid(
            "candidate binary differs from the expected binding",
        ));
    }
    if !options.fixture.is_dir() || options.work_dir.exists() || options.receipt.exists() {
        return Err(invalid(
            "provider executor requires new fixture work and receipt paths",
        ));
    }
    if options.campaign_id.is_empty() || options.allocation_id.is_empty() {
        return Err(invalid(
            "provider campaign and allocation identifiers are required",
        ));
    }
    Ok(())
}

fn reject_mock_environment() -> Result<(), AcceptanceError> {
    if env::vars_os().any(|(name, _)| {
        let name = name.to_string_lossy();
        name.starts_with("KIO_TEST_") || name.starts_with("KIO_MOCK_")
    }) {
        return Err(invalid(
            "provider acceptance refuses mock-control environment variables",
        ));
    }
    Ok(())
}

fn require_production_credential(provider: Provider) -> Result<String, AcceptanceError> {
    let key = env::var(provider.credential_name())
        .map_err(|_| invalid("selected provider credential is missing"))?;
    if key.trim().is_empty() || key == "KIO_PLACEHOLDER_NOT_CONFIGURED" {
        return Err(invalid(
            "selected provider credential is missing or a placeholder",
        ));
    }
    Ok(key)
}

fn verify_reservation(options: &A08ProviderOptions) -> Result<(), AcceptanceError> {
    validate_exact_reservation(
        &options.reservation_state_dir,
        &options.campaign_id,
        &options.expected.candidate.candidate_sha,
        options.provider.as_str(),
        &options.allocation_id,
    )
    .map(|_| ())
    .map_err(|_| invalid("provider reservation does not exactly bind this request group"))
}

fn prepare_isolated_root(path: &Path) -> Result<PathBuf, AcceptanceError> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("provider work path has no parent"))?;
    if !parent.is_dir() {
        return Err(invalid("provider work parent must already exist"));
    }
    fs::create_dir(path).map_err(io_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(io_error)?;
    }
    let isolated = path.join("a08-private");
    fs::create_dir(&isolated).map_err(io_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&isolated, fs::Permissions::from_mode(0o700)).map_err(io_error)?;
    }
    for child in ["xdg-config", "xdg-data", "xdg-cache", "tmp"] {
        fs::create_dir(isolated.join(child)).map_err(io_error)?;
    }
    Ok(isolated)
}

fn write_tools_config(root: &Path, provider: Provider) -> Result<(), AcceptanceError> {
    let config = root.join("xdg-config").join("kio");
    fs::create_dir(&config).map_err(io_error)?;
    let body = match provider {
        Provider::Mistral => concat!(
            "[markdown.mistral_ocr_markdownize]\n",
            "kind = \"online_api\"\nmodel = \"mistral-ocr-4-0\"\n",
            "auth = \"env:MISTRAL_API_KEY\"\n",
            "[markdown.mistral_ocr_markdownize.pricing]\npages = 0.004\n"
        ),
        Provider::Gemini => concat!(
            "[embedding.gemini_embedding_2]\n",
            "kind = \"online_api\"\nmodel = \"gemini-embedding-2\"\n",
            "auth = \"env:GEMINI_API_KEY\"\n",
            "[embedding.gemini_embedding_2.pricing]\ntokens_in = 0.00000045\n"
        ),
    };
    fs::write(config.join("tools.toml"), body).map_err(io_error)
}

fn load_fixture(root: &Path) -> Result<Vec<(String, Vec<u8>)>, AcceptanceError> {
    let mut files = Vec::new();
    for name in ["document.md", "ocr.pdf"] {
        let path = root.join(name);
        let metadata = fs::symlink_metadata(&path).map_err(io_error)?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() > FIXTURE_MAX_BYTES
        {
            return Err(invalid("provider fixture contains an invalid bounded leaf"));
        }
        files.push((name.to_owned(), fs::read(path).map_err(io_error)?));
    }
    let digest = sha256_bytes(
        &files
            .iter()
            .flat_map(|(name, bytes)| [name.as_bytes(), bytes.as_slice()])
            .flatten()
            .copied()
            .collect::<Vec<_>>(),
    );
    if digest != expected_fixture_digest() {
        return Err(invalid("provider public fixture digest differs"));
    }
    Ok(files)
}

fn expected_fixture_digest() -> &'static str {
    // Updated together with the two public fixture leaves.  Keeping the value
    // here makes an unreviewed fixture edit fail before a paid request.
    "fc6ac89c29025e71da895f61b45d0d7a449989f098f3de1ac01f3f7dd1f02961"
}

fn materialize_fixture(
    scope: &Path,
    provider: Provider,
    files: &[(String, Vec<u8>)],
) -> Result<(), AcceptanceError> {
    for (name, bytes) in files {
        if (provider == Provider::Mistral && name == "ocr.pdf")
            || (provider == Provider::Gemini && name == "document.md")
        {
            fs::write(scope.join(name), bytes).map_err(io_error)?;
        }
    }
    if provider == Provider::Gemini {
        fs::write(scope.join("image.png"), tiny_png()).map_err(io_error)?;
    }
    Ok(())
}

fn run_mistral(
    options: &A08ProviderOptions,
    root: &Path,
    scope: &Path,
    key: &str,
) -> Result<(), AcceptanceError> {
    run_json(
        &options.binary,
        root,
        scope,
        &[
            "--json",
            "adapter",
            "approve",
            "mistral_ocr_markdownize",
            "--yes",
        ],
        Provider::Mistral,
        key,
    )?;
    run_json(
        &options.binary,
        root,
        scope,
        &["--json", "index", "--online", "--realtime"],
        Provider::Mistral,
        key,
    )?;
    fs::copy(scope.join("ocr.pdf"), scope.join("ocr-batch.pdf")).map_err(io_error)?;
    run_json(
        &options.binary,
        root,
        scope,
        &["--json", "index", "--online", "--batch"],
        Provider::Mistral,
        key,
    )?;
    poll_until_complete(&options.binary, root, scope, Provider::Mistral, key)?;
    assert_provenance(scope, "markdown")?;
    assert_settled_usage(root, "markdownize")
}

fn run_gemini(
    options: &A08ProviderOptions,
    root: &Path,
    scope: &Path,
    key: &str,
) -> Result<(), AcceptanceError> {
    run_json(
        &options.binary,
        root,
        scope,
        &[
            "--json",
            "adapter",
            "approve",
            "gemini_embedding_2",
            "--yes",
        ],
        Provider::Gemini,
        key,
    )?;
    let realtime = run_json(
        &options.binary,
        root,
        scope,
        &["--json", "index", "--online", "--realtime"],
        Provider::Gemini,
        key,
    )?;
    require_positive(&realtime, "/embedding_tasks_executed")?;
    let search = run_json(
        &options.binary,
        root,
        scope,
        &[
            "--json",
            "search",
            "public provider fixture",
            "--mode",
            "vector",
        ],
        Provider::Gemini,
        key,
    )?;
    if search
        .pointer("/results")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
    {
        return Err(invalid("Gemini realtime query returned no vector result"));
    }
    fs::write(
        scope.join("batch.md"),
        b"Kio A08 bounded batch embedding fixture.\n",
    )
    .map_err(io_error)?;
    run_json(
        &options.binary,
        root,
        scope,
        &["--json", "index", "--online", "--batch"],
        Provider::Gemini,
        key,
    )?;
    poll_until_complete(&options.binary, root, scope, Provider::Gemini, key)?;
    assert_provenance(scope, "embedding")?;
    assert_settled_usage(root, "embedding")
}

fn poll_until_complete(
    binary: &Path,
    root: &Path,
    scope: &Path,
    provider: Provider,
    key: &str,
) -> Result<Value, AcceptanceError> {
    for _ in 0..POLL_LIMIT {
        let value = run_json(
            binary,
            root,
            scope,
            &["--json", "batch", "resume", "--online"],
            provider,
            key,
        )?;
        if value.pointer("/tasks_inflight").and_then(Value::as_u64) == Some(0) {
            return Ok(value);
        }
    }
    Err(invalid(
        "provider batch remained unknown after the bounded poll deadline",
    ))
}

fn assert_provenance(scope: &Path, role: &str) -> Result<(), AcceptanceError> {
    let lock = fs::read(scope.join(".kio").join("tool-lock.json")).map_err(io_error)?;
    let value: Value =
        serde_json::from_slice(&lock).map_err(|_| invalid("provider provenance is malformed"))?;
    if value.get(role).and_then(Value::as_object).is_none() {
        return Err(invalid(
            "provider provenance lacks the executed adapter role",
        ));
    }
    Ok(())
}

/// A passing HTTP status alone is not A08 evidence.  The product must have
/// recorded a settled provider usage row for the adapter that was exercised.
fn assert_settled_usage(root: &Path, adapter_kind: &str) -> Result<(), AcceptanceError> {
    let database = root.join("xdg-data").join("kio").join("cost-ledger.sqlite");
    let connection =
        rusqlite::Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|_| invalid("provider usage ledger is unavailable"))?;
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM cost_ledger WHERE adapter_kind = ?1 AND outcome = 'succeeded'",
            [adapter_kind],
            |row| row.get(0),
        )
        .map_err(|_| invalid("provider usage ledger cannot prove settled usage"))?;
    if count == 0 {
        return Err(invalid(
            "provider execution produced no settled usage record",
        ));
    }
    Ok(())
}

fn observed_batch_profile(root: &Path, adapter_kind: &str) -> Result<String, AcceptanceError> {
    let database = root.join("xdg-data").join("kio").join("cost-ledger.sqlite");
    let connection =
        rusqlite::Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|_| invalid("provider ledger is unavailable"))?;
    connection.query_row(
        "SELECT tool_profile_hash FROM batch_requests WHERE adapter_kind = ?1 ORDER BY created_at DESC LIMIT 1",
        [adapter_kind],
        |row| row.get(0),
    ).map_err(|_| invalid("provider ledger has no observed tool profile"))
}

fn require_positive(value: &Value, pointer: &str) -> Result<(), AcceptanceError> {
    if value.pointer(pointer).and_then(Value::as_u64).unwrap_or(0) == 0 {
        return Err(invalid("provider command did not report completed work"));
    }
    Ok(())
}

fn run_json(
    binary: &Path,
    root: &Path,
    scope: &Path,
    args: &[&str],
    provider: Provider,
    key: &str,
) -> Result<Value, AcceptanceError> {
    let output = run_child(binary, root, scope, args, provider, key)?;
    serde_json::from_slice(&output).map_err(|_| invalid("provider command emitted invalid JSON"))
}

fn run_child(
    binary: &Path,
    root: &Path,
    scope: &Path,
    args: &[&str],
    provider: Provider,
    key: &str,
) -> Result<Vec<u8>, AcceptanceError> {
    // A file avoids the pipe back-pressure deadlock that would otherwise let a
    // noisy child evade the deadline before it can be killed and reaped.
    let stdout_path = root.join("tmp").join("child-output.json");
    let stdout = File::create(&stdout_path).map_err(io_error)?;
    let mut command = Command::new(binary);
    IsolatedChildEnvironment::new(
        root,
        root.join("xdg-config"),
        root.join("xdg-data"),
        root.join("xdg-cache"),
        root.join("tmp"),
    )
    .apply(&mut command)?;
    command
        .env(provider.credential_name(), key)
        .current_dir(scope)
        .args(args)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(io_error)?;
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().map_err(io_error)? {
            if !status.success() {
                return Err(invalid("provider product command failed"));
            }
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(invalid("provider product command timed out and was reaped"));
        }
        thread::sleep(Duration::from_millis(25));
    }
    let mut bytes = Vec::new();
    let mut stdout = File::open(stdout_path).map_err(io_error)?;
    stdout
        .by_ref()
        .take(MAX_CHILD_OUTPUT + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > MAX_CHILD_OUTPUT {
        return Err(invalid("provider product command exceeded bounded output"));
    }
    Ok(bytes)
}

fn tiny_png() -> &'static [u8] {
    &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 11, 73, 68, 65, 84, 120, 156, 99, 248, 15, 4, 0, 9,
        251, 3, 253, 251, 94, 107, 43, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ]
}

fn invalid(message: impl Into<String>) -> AcceptanceError {
    AcceptanceError::Invalid(message.into())
}
fn io_error(error: std::io::Error) -> AcceptanceError {
    AcceptanceError::Io(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::{io::Cursor, path::PathBuf};

    use image::{GenericImageView, ImageReader};

    use super::{
        Provider, expected_fixture_digest, load_fixture, materialize_fixture,
        prepare_isolated_root, tiny_png, write_tools_config,
    };

    #[test]
    fn provider_private_root_supports_offline_preparation_and_refuses_reuse() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("acceptance-fixtures/v1/provider-public");
        let files = load_fixture(&fixture).unwrap();
        let temp = tempfile::tempdir().unwrap();
        for provider in [Provider::Mistral, Provider::Gemini] {
            let work = temp.path().join(provider.as_str());
            let isolated = prepare_isolated_root(&work).unwrap();
            assert_eq!(isolated, work.join("a08-private"));
            for child in ["xdg-config", "xdg-data", "xdg-cache", "tmp"] {
                assert!(isolated.join(child).is_dir());
                assert!(!work.join(child).exists());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                for root in [&work, &isolated] {
                    assert_eq!(
                        std::fs::metadata(root).unwrap().permissions().mode() & 0o777,
                        0o700
                    );
                }
            }
            let scope = isolated.join("scope");
            std::fs::create_dir(&scope).unwrap();
            write_tools_config(&isolated, provider).unwrap();
            materialize_fixture(&scope, provider, &files).unwrap();
            let config = isolated.join("xdg-config/kio/tools.toml");
            let config_before = std::fs::read(&config).unwrap();
            let mut names = std::fs::read_dir(&scope)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect::<Vec<_>>();
            names.sort();
            match provider {
                Provider::Mistral => assert_eq!(names, ["ocr.pdf"]),
                Provider::Gemini => assert_eq!(names, ["document.md", "image.png"]),
            }
            assert!(prepare_isolated_root(&work).is_err());
            assert_eq!(std::fs::read(&config).unwrap(), config_before);
        }
    }

    #[test]
    fn public_provider_fixture_loads_and_matches_workflow_binding() {
        let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let fixture = repository.join("crates/kio-eval/acceptance-fixtures/v1/provider-public");

        let files = load_fixture(&fixture).expect("checked-in provider fixture must load");
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].0, "document.md");
        assert_eq!(files[1].0, "ocr.pdf");

        let workflow = std::fs::read_to_string(
            repository.join(".github/workflows/v1-provider-acceptance.yml"),
        )
        .expect("provider workflow must be readable");
        let expected_argument = format!("--fixture-sha256 {}", expected_fixture_digest());
        assert!(
            workflow.contains(&expected_argument),
            "provider workflow must bind the fixture digest validated by the executor"
        );
    }

    #[test]
    fn generated_gemini_image_is_a_decodable_one_pixel_png() {
        let image = ImageReader::new(Cursor::new(tiny_png()))
            .with_guessed_format()
            .expect("generated image must identify as PNG")
            .decode()
            .expect("generated image must decode with the production image decoder");
        assert_eq!(image.dimensions(), (1, 1));
    }
}
