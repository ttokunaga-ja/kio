//! Office (DOCX/PPTX) → converted-PDF intermediate
//! ([07-adapter-spec.md §5.1](../../../docs/07-adapter-spec.md), "Office
//! intermediate の変換機構" — 2026-07-23 addendum).
//!
//! DOCX/PPTX unit-ize through an external renderer (LibreOffice headless
//! `soffice --convert-to pdf`) rather than being parsed locally: DOCX pages
//! become `page:N`, PPTX slides become `slide:N` (1 slide = 1 converted
//! page). The renderer's raw PDF output embeds several genuinely volatile
//! fields (wall-clock timestamps, a document ID, LibreOffice's own content
//! checksum) that differ across otherwise-identical conversions;
//! [`normalize_converted_pdf`] rewrites them to fixed, SAME-LENGTH values so
//! byte length — and every xref offset — survives, keeping `prepared_hash`
//! stable within one renderer version (03 §2.1's prepare-profile/renderer
//! driven gen+1 path absorbs an actual renderer version bump). The
//! renderer's own name/version (`/Producer`) is deliberately left untouched
//! — it is provenance, not identity, and SHOULD vary when the renderer
//! changes (07 §5.1: "renderer の名称・版は provenance として記録し、hash 入力には
//! しない").

use kio_core::control::ControlHandle;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use kio_process::{
    BoundedProcessError, BoundedProcessOptions, BoundedProcessOutput,
    confinement::{ConfinementError, RenderResourceLimits, RenderSources},
};
use sha2::{Digest, Sha256};

use crate::office_runtime_identity::{
    ExecutableFingerprint, RuntimeSnapshot, check_verification_deadline,
};
use crate::platform::{
    DEFAULT_OFFICE_CONVERTER_PROGRAM, OFFICE_LAUNCH_PROFILE, OfficeRuntime, file_url,
    finish_scratch, office_conversion_paths, office_render_sandbox, private_temp_dir,
    restrict_renderer_state_directory, scratch_recovery_note,
};
use crate::{AdapterError, Result};

/// Test seam env var: its value is a path to a fixture PDF returned
/// VERBATIM for any input — already deterministic, so
/// [`OfficeConverter::convert_to_pdf`] skips normalization for this backend.
/// `version()` reports `"test-converter"`. Checked before
/// [`OFFICE_CONVERTER_ENV`].
#[cfg(debug_assertions)]
pub const TEST_OFFICE_CONVERT_ENV: &str = "KIO_TEST_OFFICE_CONVERT";
/// Explicit converter binary path, checked before the `soffice` PATH lookup.
pub const OFFICE_CONVERTER_ENV: &str = "KIO_OFFICE_CONVERTER";
pub(crate) const MAX_OFFICE_PROBE_PDF_BYTES: usize = 2 * 1024 * 1024;
const MAX_OFFICE_PROBE_SCRATCH_BYTES: u64 = 8 * 1024 * 1024;
// One budget for runtime verification and both fixture conversions on cold CI hosts.
const OFFICE_PROBE_TIMEOUT: Duration = Duration::from_secs(60);
const OFFICE_CONVERT_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_OFFICE_INPUT_BYTES: usize = 100 * 1024 * 1024;
const MAX_OFFICE_PDF_BYTES: usize = 250 * 1024 * 1024;
pub(crate) const MAX_OFFICE_LOG_BYTES: usize = 64 * 1024;
const OFFICE_NORMALIZATION_PROFILE: &str = "pdf-volatile-fields-v1";
const MACOS_UNO_FILTER_VERSION: &str = "macos-spell-component-v1";

const DOCX_MEDIA_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const PPTX_MEDIA_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.presentationml.presentation";

const DOCX_PROBE_FIXTURE: &[u8] = include_bytes!("../fixtures/office-probe.docx");
const PPTX_PROBE_FIXTURE: &[u8] = include_bytes!("../fixtures/office-probe.pptx");

const DOCX_PROBE_TEXT: &str = "KCS office convert test document";
const PPTX_PROBE_TEXT: &str = "KCS office convert test presentation";

/// True exactly for the DOCX and PPTX OOXML mimes (07 §5.1's 2026-07-23
/// addendum). XLSX is deliberately excluded — its conversion machinery is
/// "本追記の対象外 (未定義のまま — 将来ラウンド)".
#[must_use]
pub fn is_office_media(media_type: &str) -> bool {
    matches!(media_type, DOCX_MEDIA_TYPE | PPTX_MEDIA_TYPE)
}

#[derive(Debug, Clone)]
enum ConverterBackend {
    /// Test seam: [`OfficeConverter::convert_to_pdf`] returns this file's
    /// bytes verbatim for ANY input.
    #[cfg(debug_assertions)]
    Seam {
        fixture_path: PathBuf,
    },
    Real(RealRenderer),
}

#[derive(Debug, Clone)]
struct RealRenderer {
    sources: RenderSources,
    program_fingerprint: ExecutableFingerprint,
    runtime_snapshot: Box<RuntimeSnapshot>,
    runtime: OfficeRuntime,
    control: ControlHandle,
    #[cfg(test)]
    native_acceptance_diagnostics: bool,
}

/// A resolved Office → PDF converter (07 §5.1). Obtain one via
/// [`resolve_office_converter`].
#[derive(Debug, Clone)]
pub struct OfficeConverter {
    backend: ConverterBackend,
    version: String,
}

impl OfficeConverter {
    #[cfg(all(test, debug_assertions))]
    fn with_native_acceptance_diagnostics(mut self) -> Self {
        if let ConverterBackend::Real(renderer) = &mut self.backend {
            renderer.native_acceptance_diagnostics = true;
        }
        self
    }

    /// The renderer's self-reported version (provenance-only — never a hash
    /// input, 07 §5.1). `"unknown"` if no usable version was reported, or
    /// `"test-converter"` for the test seam.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Stable preparation-profile input for this renderer. It binds the
    /// selected executable and all conversion-affecting Kio/UNO filter
    /// definitions without treating the renderer's display version as content.
    #[must_use]
    pub fn profile_identity(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"kio-office-preparation-profile-v1\0");
        digest.update(std::env::consts::OS.as_bytes());
        digest.update(b"\0");
        digest.update(OFFICE_NORMALIZATION_PROFILE.as_bytes());
        digest.update(b"\0");
        digest.update(MACOS_UNO_FILTER_VERSION.as_bytes());
        digest.update(b"\0");
        match &self.backend {
            #[cfg(debug_assertions)]
            ConverterBackend::Seam { fixture_path } => {
                digest.update(fixture_path.as_os_str().as_encoded_bytes())
            }
            ConverterBackend::Real(renderer) => {
                digest.update(OFFICE_LAUNCH_PROFILE);
                let RealRenderer {
                    sources,
                    program_fingerprint,
                    runtime,
                    ..
                } = renderer;
                digest.update(sources.program().as_os_str().as_encoded_bytes());
                digest.update(b"\0");
                digest.update(program_fingerprint.digest.as_bytes());
                digest.update(b"\0");
                digest.update(runtime.catalog_digest().unwrap_or("none").as_bytes());
            }
        }
        hex_digest(&digest.finalize())
    }

    /// Convert `input` (DOCX or PPTX bytes, per `media_type`) to
    /// deterministically-normalized PDF bytes. The seam backend returns its
    /// fixture file verbatim (already deterministic, so normalization is
    /// skipped) regardless of `input`/`media_type`. The real backend invokes
    /// the external renderer and then normalizes its output — see
    /// [`normalize_converted_pdf`].
    ///
    /// # Errors
    /// `AdapterError::ContractViolation` on any conversion failure (missing
    /// binary at call time, non-zero renderer exit, missing/invalid output,
    /// or an unreadable seam fixture) — 07 §5.1: a runtime conversion
    /// failure "joins" contract_violation semantics at the pipeline layer
    /// (04 §5.3: retried once for the same input).
    pub fn convert_to_pdf(&self, input: &[u8], media_type: &str) -> Result<Vec<u8>> {
        match &self.backend {
            #[cfg(debug_assertions)]
            ConverterBackend::Seam { fixture_path } => std::fs::read(fixture_path).map_err(|err| {
                AdapterError::ContractViolation(format!(
                    "office converter test seam fixture unreadable at {}: {err}",
                    fixture_path.display()
                ))
            }),
            ConverterBackend::Real(renderer) => {
                crate::ooxml_package::validate_real_office_package(input, media_type)?;
                let deadline = Instant::now() + OFFICE_CONVERT_TIMEOUT;
                renderer
                    .verify_runtime("before conversion", deadline)
                    .map_err(verification_error)?;
                let pdf = renderer.render_pdf(input, media_type, MAX_OFFICE_PDF_BYTES, deadline)?;
                Ok(normalize_converted_pdf(&pdf))
            }
        }
    }
}

/// An unavailable renderer is either absent or detected but unusable.
#[derive(Debug)]
pub struct OfficeUnavailable {
    pub state: &'static str,
    pub reason: String,
}

impl std::fmt::Display for OfficeUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.state, self.reason)
    }
}

pub fn resolve_office_converter() -> std::result::Result<OfficeConverter, OfficeUnavailable> {
    resolve_office_converter_with_control(&ControlHandle::default())
}

pub fn resolve_office_converter_with_control(
    control: &ControlHandle,
) -> std::result::Result<OfficeConverter, OfficeUnavailable> {
    let deadline = Instant::now() + OFFICE_PROBE_TIMEOUT;
    let unusable = |error: AdapterError| OfficeUnavailable {
        state: "unusable",
        reason: error.to_string(),
    };
    #[cfg(debug_assertions)]
    if let Some(fixture_path) = &control.debug().adapters.office_convert {
        if fixture_path.is_empty() {
            return Err(OfficeUnavailable {
                state: "missing",
                reason: "Office renderer is not configured".into(),
            });
        }
        return Ok(OfficeConverter {
            backend: ConverterBackend::Seam {
                fixture_path: fixture_path.clone(),
            },
            version: "test-converter".to_owned(),
        });
    }
    let requested = std::env::var_os(OFFICE_CONVERTER_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_OFFICE_CONVERTER_PROGRAM));
    if requested.as_os_str().is_empty() {
        return Err(OfficeUnavailable {
            state: "missing",
            reason: "Office renderer is not configured".into(),
        });
    }
    let Some(program) = resolve_program(&requested, deadline).map_err(unusable)? else {
        let detected = renderer_candidate_exists(&requested, deadline).map_err(unusable)?;
        return Err(OfficeUnavailable {
            state: if detected { "unusable" } else { "missing" },
            reason: format!(
                "Office renderer {} could not be resolved",
                requested.display()
            ),
        });
    };
    #[cfg(debug_assertions)]
    if let Some(error) = control
        .debug()
        .adapters
        .office_isolation_failure
        .known()
        .and_then(|selector| crate::platform::office_isolation_fault(selector))
    {
        return Err(OfficeUnavailable {
            state: "unusable",
            reason: error.to_string(),
        });
    }
    probe_real_converter_diagnostic(program, control, deadline).map_err(unusable)
}

fn renderer_candidate_exists(program: &Path, deadline: Instant) -> Result<bool> {
    Ok(find_program_candidate(program, deadline, |path| {
        (!matches!(std::fs::symlink_metadata(&path), Err(error) if error.kind() == std::io::ErrorKind::NotFound))
            .then_some(())
    })?.is_some())
}

fn resolve_program(program: &Path, deadline: Instant) -> Result<Option<PathBuf>> {
    find_program_candidate(program, deadline, |candidate| {
        let canonical = std::fs::canonicalize(candidate).ok()?;
        canonical.is_file().then_some(canonical)
    })
}

/// Discover candidates and inspect each one against the same probe deadline.
fn find_program_candidate<T>(
    program: &Path,
    deadline: Instant,
    inspect: impl Fn(PathBuf) -> Option<T>,
) -> Result<Option<T>> {
    let inspect = |path| {
        check_renderer_deadline(deadline)?;
        let result = inspect(path);
        check_renderer_deadline(deadline)?;
        Ok(result)
    };
    if program.is_absolute() || program.components().count() > 1 {
        return inspect(program.to_owned());
    }
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            if let Some(result) = inspect(directory.join(program))? {
                return Ok(Some(result));
            }
        }
    }
    check_renderer_deadline(deadline)?;
    Ok(None)
}

fn probe_real_converter_diagnostic(
    program: PathBuf,
    control: &ControlHandle,
    deadline: Instant,
) -> Result<OfficeConverter> {
    let runtime = OfficeRuntime::new(&program, deadline)?;
    let renderer = RealRenderer::new(
        &program,
        runtime.package_roots(&program)?,
        runtime.runtime_roots(&program)?,
        runtime,
        control,
        deadline,
    )?;
    renderer.verify_initial_runtime(deadline)?;
    let version = probe_renderer_capability(
        || renderer.renderer_version(deadline),
        |fixture, media_type| {
            renderer.render_pdf(fixture, media_type, MAX_OFFICE_PROBE_PDF_BYTES, deadline)
        },
        deadline,
    )?;
    Ok(OfficeConverter {
        backend: ConverterBackend::Real(renderer),
        version,
    })
}

impl RealRenderer {
    fn new(
        program: &Path,
        package_roots: Vec<PathBuf>,
        runtime_roots: Vec<PathBuf>,
        runtime: OfficeRuntime,
        control: &ControlHandle,
        deadline: Instant,
    ) -> Result<Self> {
        check_renderer_deadline(deadline)?;
        let launch_roots = runtime_roots.into_iter().chain(
            crate::platform::office_package_launch_roots(&package_roots)
                .iter()
                .cloned(),
        );
        let sources = RenderSources::resolve(program, launch_roots)
            .map_err(|error| AdapterError::ContractViolation(error.to_string()))?;
        let sources = runtime.bind_runtime(sources)?;
        if sources.program() != program {
            return Err(AdapterError::ContractViolation(
                "office converter executable changed before runtime verification".to_owned(),
            ));
        }
        let program_fingerprint =
            ExecutableFingerprint::new(sources.program(), deadline).map_err(verification_error)?;
        let runtime_snapshot = runtime
            .snapshot(package_roots, &sources, deadline)
            .map_err(verification_error)?;
        Ok(Self {
            sources,
            program_fingerprint,
            runtime,
            runtime_snapshot: Box::new(runtime_snapshot),
            control: control.clone(),
            #[cfg(test)]
            native_acceptance_diagnostics: false,
        })
    }

    fn verify_initial_runtime(&self, deadline: Instant) -> Result<()> {
        self.runtime.verify_initial_runtime(
            &self.sources,
            &self.runtime_snapshot,
            &self.program_fingerprint,
            deadline,
        )
    }

    fn renderer_version(&self, deadline: Instant) -> Result<String> {
        if Instant::now() >= deadline {
            return Ok(String::new());
        }
        let scratch = private_temp_dir()?;
        let profile_dir = create_private_renderer_state(scratch.path(), "probe")?;
        let profile_url = file_url(&profile_dir)?;
        let sandbox = office_render_sandbox(
            &self.sources,
            scratch.path(),
            RenderResourceLimits::default(),
        )
        .map_err(|error| {
            AdapterError::ContractViolation(format!(
                "office converter confinement setup failed: {error}"
            ))
        })?;
        let mut environment = self
            .runtime
            .environment(self.sources.program(), Some(&profile_dir));
        self.runtime
            .configure_services(scratch.path(), &mut environment, deadline)?;
        let _runtime_pins = self
            .runtime_snapshot
            .pin_before_launch(deadline)
            .map_err(verification_error)?;
        let result = sandbox.run_checked_until(
            [
                OsString::from("--headless"),
                OsString::from("--norestore"),
                OsString::from(format!("-env:UserInstallation={profile_url}")),
                OsString::from("--version"),
            ],
            &environment,
            renderer_process_options(OFFICE_PROBE_TIMEOUT),
            deadline,
            || self.verify_runtime("before version query", deadline),
        );
        match advisory_version_output(result) {
            Ok(stdout) => {
                finish_scratch(scratch)?;
                Ok(stdout)
            }
            Err(error) => {
                let recovery_note = scratch_recovery_note(scratch);
                Err(AdapterError::ContractViolation(format!(
                    "office converter bounded version query failed: {error}{recovery_note}"
                )))
            }
        }
    }

    fn verify_runtime(&self, stage: &str, deadline: Instant) -> std::io::Result<()> {
        check_verification_deadline(deadline)?;
        self.program_fingerprint
            .verify(self.sources.program(), stage, deadline)?;
        self.runtime.verify_roots(deadline)?;
        self.runtime_snapshot.verify(&self.sources, stage, deadline)
    }
}

fn verification_error(error: std::io::Error) -> AdapterError {
    AdapterError::ContractViolation(error.to_string())
}

fn advisory_version_output(
    result: std::result::Result<BoundedProcessOutput, ConfinementError>,
) -> std::result::Result<String, ConfinementError> {
    match result {
        Ok(output) if output.status.success() => Ok(output.stdout),
        Ok(_)
        | Err(ConfinementError::Process(
            BoundedProcessError::Timeout { .. }
            | BoundedProcessError::OutputLimit { .. }
            | BoundedProcessError::NonUtf8 { .. }
            | BoundedProcessError::Read { .. }
            | BoundedProcessError::Spawn(_),
        )) => Ok(String::new()),
        Err(error) => Err(error),
    }
}

fn probe_renderer_capability(
    query_version: impl FnOnce() -> Result<String>,
    mut convert_fixture: impl FnMut(&[u8], &str) -> Result<Vec<u8>>,
    deadline: Instant,
) -> Result<String> {
    for (extension, media_type, fixture, expected) in [
        ("docx", DOCX_MEDIA_TYPE, DOCX_PROBE_FIXTURE, DOCX_PROBE_TEXT),
        ("pptx", PPTX_MEDIA_TYPE, PPTX_PROBE_FIXTURE, PPTX_PROBE_TEXT),
    ] {
        check_renderer_deadline(deadline)?;
        let pdf = convert_fixture(fixture, media_type).map_err(|error| {
            AdapterError::ContractViolation(format!(
                "Office renderer probe could not convert the {extension} fixture: {error}"
            ))
        })?;
        if pdf.len() > MAX_OFFICE_PROBE_PDF_BYTES {
            return Err(AdapterError::ContractViolation(
                "Office renderer probe PDF exceeds its byte limit".to_owned(),
            ));
        }
        let text = crate::pdf_decode::office_probe_page_text(&pdf, deadline)?;
        check_renderer_deadline(deadline)?;
        if !match text {
            Some(text) => probe_text_matches(&text, expected, deadline)?,
            None => false,
        } {
            return Err(AdapterError::ContractViolation(format!(
                "Office renderer probe PDF did not contain the expected {extension} page"
            )));
        }
    }
    let stdout = query_version()?;
    Ok(stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.chars().any(char::is_control))
        .unwrap_or("unknown")
        .to_owned())
}

/// Match the fixture words in order without making a copy of the output text.
fn probe_text_matches(text: &str, expected: &str, deadline: Instant) -> Result<bool> {
    let mut remaining = expected.split_whitespace();
    let first = remaining.next().expect("fixture text contains words");
    let mut next = Some(first);
    for word in text.split_whitespace() {
        check_renderer_deadline(deadline)?;
        if Some(word) == next {
            next = remaining.next();
            if next.is_none() {
                return Ok(true);
            }
        } else {
            remaining = expected.split_whitespace();
            next = remaining.next();
            if word == first {
                next = remaining.next();
            }
        }
    }
    check_renderer_deadline(deadline)?;
    Ok(false)
}

pub(crate) fn check_renderer_deadline(deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline {
        return Err(AdapterError::ContractViolation(
            "office converter exceeded its time limit".to_owned(),
        ));
    }
    Ok(())
}

fn renderer_process_options(timeout: Duration) -> BoundedProcessOptions {
    BoundedProcessOptions {
        timeout,
        max_stdout_bytes: MAX_OFFICE_LOG_BYTES,
        max_stderr_bytes: MAX_OFFICE_LOG_BYTES,
    }
}

fn office_extension(media_type: &str) -> Option<&'static str> {
    match media_type {
        DOCX_MEDIA_TYPE => Some("docx"),
        PPTX_MEDIA_TYPE => Some("pptx"),
        _ => None,
    }
}

pub(crate) fn bind_private_scratch(
    path: &Path,
) -> kio_core::Result<kio_core::store_dir::StoreDirectory> {
    bind_private_scratch_with_control(path, &kio_core::control::ControlHandle::default())
}

fn bind_private_scratch_with_control(
    path: &Path,
    control: &kio_core::control::ControlHandle,
) -> kio_core::Result<kio_core::store_dir::StoreDirectory> {
    let parent = path
        .parent()
        .ok_or_else(|| kio_core::KioError::invalid_usage("Office scratch path has no parent"))?;
    let leaf = path
        .file_name()
        .ok_or_else(|| kio_core::KioError::invalid_usage("Office scratch path has no leaf"))?;
    let external = kio_core::private_fs::retain_external_directory_with_control(parent, control)?;
    kio_core::private_fs::verify_private_directory_at(&external, Path::new(leaf))
}

/// Construct all renderer-owned mutable state before process creation. These
/// names are derived only from an exclusively-created private scratch parent;
/// each child is created with protected owner-only access before any
/// invocation-specific renderer access is granted.
fn create_private_renderer_state(scratch: &Path, purpose: &str) -> Result<PathBuf> {
    let profile = scratch.join("lo-profile");
    let directory = bind_private_scratch(scratch).map_err(|error| {
        AdapterError::ContractViolation(format!("unsafe Office scratch for {purpose}: {error}"))
    })?;
    for (name, label) in [
        ("lo-profile", "LibreOffice profile"),
        ("cache", "renderer cache"),
        ("config", "renderer configuration"),
    ] {
        directory
            .create_directory(Path::new(name))
            .map_err(|error| {
                AdapterError::ContractViolation(format!(
                    "failed to create private {purpose} {label}: {error}"
                ))
            })?;
        restrict_renderer_state_directory(&directory, name, purpose, label)?;
    }
    Ok(profile)
}

pub(crate) fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

impl RealRenderer {
    /// Stage one document and render it inside the native sandbox with a
    /// private, per-invocation profile and a retained output directory.
    fn render_pdf(
        &self,
        input: &[u8],
        media_type: &str,
        output_limit: usize,
        deadline: Instant,
    ) -> Result<Vec<u8>> {
        check_renderer_deadline(deadline)?;
        let program = self.sources.program();
        let runtime = &self.runtime;
        let extension = office_extension(media_type).ok_or_else(|| {
            AdapterError::ContractViolation(format!(
                "office converter invoked for a non-office media type: {media_type}"
            ))
        })?;
        if input.len() > MAX_OFFICE_INPUT_BYTES {
            return Err(AdapterError::ContractViolation(format!(
                "office input exceeds the {} byte limit",
                MAX_OFFICE_INPUT_BYTES
            )));
        }
        let scratch = private_temp_dir()?;
        let workdir = scratch.path();

        let scratch_directory =
            bind_private_scratch_with_control(workdir, &self.control).map_err(|error| {
                AdapterError::ContractViolation(format!(
                    "failed to retain private Office scratch: {error}"
                ))
            })?;
        let input_leaf = format!("input.{extension}");
        let input_path = workdir.join(&input_leaf);
        let mut staged_input = scratch_directory
            .create_private_file(Path::new(&input_leaf))
            .map_err(|error| {
                AdapterError::ContractViolation(format!(
                    "failed to create private Office input: {error}"
                ))
            })?;
        {
            use std::io::Write as _;
            staged_input.write_all(input).map_err(|error| {
                AdapterError::ContractViolation(format!(
                    "failed to stage private Office input: {error}"
                ))
            })?;
        }
        drop(staged_input);
        let outdir = workdir.join("out");
        let output_handle = scratch_directory
            .create_directory(Path::new("out"))
            .map_err(|error| {
                AdapterError::ContractViolation(format!(
                    "failed to create private Office output directory: {error}"
                ))
            })?;
        // Keep the original output directory capability across renderer execution.
        // Child path replacements cannot redirect the descriptor-relative PDF read.
        let output_directory = kio_core::store_dir::StoreDirectory::from_retained_with_control(
            output_handle,
            outdir.clone(),
            self.control.clone(),
        )
        .map_err(|error| {
            AdapterError::ContractViolation(format!(
                "failed to retain private Office output directory: {error}"
            ))
        })?;
        let profile_dir = create_private_renderer_state(workdir, "conversion")?;

        let profile_url = file_url(&profile_dir)?;
        let mut limits = RenderResourceLimits::default();
        if output_limit == MAX_OFFICE_PROBE_PDF_BYTES {
            limits.max_file_bytes = MAX_OFFICE_PROBE_SCRATCH_BYTES;
        }
        let sandbox = office_render_sandbox(&self.sources, workdir, limits).map_err(|err| {
            AdapterError::ContractViolation(format!(
                "office converter confinement is unavailable: {err}"
            ))
        })?;
        let (outdir_arg, input_arg) = office_conversion_paths(&outdir, &input_path);
        let arguments = vec![
            OsString::from("--headless"),
            OsString::from(format!("-env:UserInstallation={profile_url}")),
            OsString::from("--convert-to"),
            OsString::from("pdf"),
            OsString::from("--outdir"),
            outdir_arg,
            input_arg,
        ];
        let mut environment = runtime.environment(program, Some(&profile_dir));
        #[cfg(test)]
        if self.native_acceptance_diagnostics {
            // This is a fixed diagnostic switch for checked-in synthetic OOXML
            // fixtures only. It never accepts ambient log configuration.
            environment.push((
                OsString::from("SAL_LOG"),
                OsString::from("+WARN+INFO.sal.osl.pipe"),
            ));
        }
        runtime.configure_services(workdir, &mut environment, deadline)?;
        let _runtime_pins = self
            .runtime_snapshot
            .pin_before_launch(deadline)
            .map_err(verification_error)?;
        let output = match sandbox.run_checked_until(
            arguments,
            &environment,
            renderer_process_options(OFFICE_CONVERT_TIMEOUT),
            deadline,
            || self.verify_runtime("before conversion", deadline),
        ) {
            Ok(output) => output,
            Err(err) => {
                // Platform cleanup retains scratch whenever recovery owns it.
                let recovery_note = scratch_recovery_note(scratch);
                return Err(AdapterError::ContractViolation(format!(
                    "office converter {} failed within confinement: {err}{recovery_note}",
                    program.display()
                )));
            }
        };
        if !output.status.success() {
            #[cfg(test)]
            if self.native_acceptance_diagnostics {
                return Err(AdapterError::ContractViolation(format!(
                    "office converter {} exited with {} after {:?}; bounded stdout for the synthetic native acceptance fixture: {}; bounded stderr: {}",
                    program.display(),
                    output.status,
                    output.duration,
                    output.stdout.trim(),
                    output.stderr.trim(),
                )));
            }
            return Err(AdapterError::ContractViolation(format!(
                "office converter {} exited with {}",
                program.display(),
                output.status
            )));
        }

        check_renderer_deadline(deadline)?;
        kio_core::private_fs::verify_private_directory_handle(&output_directory).map_err(
            |error| {
                AdapterError::ContractViolation(format!(
                    "Office output directory lost private protection: {error}"
                ))
            },
        )?;
        let pdf_bytes = kio_core::private_fs::read_private_file_at_until(
            &output_directory,
            "input.pdf",
            output_limit as u64,
            deadline,
        )
        .map_err(|error| {
            AdapterError::ContractViolation(format!(
                "office converter output is absent, unsafe, or exceeded the {output_limit} byte limit: {error}",
            ))
        })?;
        if !pdf_bytes.starts_with(b"%PDF") {
            return Err(AdapterError::ContractViolation(
                "office converter output is not a PDF (missing %PDF magic)".to_owned(),
            ));
        }
        drop(output_directory);
        drop(scratch_directory);
        finish_scratch(scratch)?;
        self.verify_runtime("after conversion", deadline)
            .map_err(verification_error)?;
        Ok(pdf_bytes)
    }
}

/// Deterministically normalize a converter's raw PDF output (07 §5.1):
/// rewrite every volatile-metadata occurrence found by a conservative
/// literal-pattern scan to a FIXED value of the SAME byte length, so total
/// length — and every xref offset — is unchanged. This is not a PDF parser:
/// it recognizes only the exact textual shapes below and leaves everything
/// else untouched, including the renderer's own `/Producer` string (07
/// §5.1 — that string SHOULD vary across renderer versions, so it is
/// deliberately not among the fields normalized here).
///
/// Covers the two constructs 07 §5.1 names explicitly:
///   - `/CreationDate (…)` / `/ModDate (…)` — Info-dict literal-string dates.
///   - `/ID [<…><…>]` — the trailer's two hex-string document IDs.
///
/// Plus two more volatility sources confirmed empirically against the real
/// `/opt/homebrew/bin/soffice` (LibreOffice 26.2.4.2) binary: converting the
/// SAME input twice is NOT byte-identical after normalizing only the two
/// constructs above.
///   - `/DocChecksum /<hex>` — a LibreOffice PDF-export trailer extension (a
///     bare PDF *name* token, not a literal string or hex string).
///   - the XMP metadata packet's `<xmp:CreateDate>`, `<xmp:ModifyDate>`,
///     `<xmp:MetadataDate>` elements — LibreOffice duplicates the same
///     timestamps as plain (uncompressed) XML inside the PDF's
///     `/Type/Metadata/Subtype/XML` stream; the Info-dict fix alone does not
///     reach this second copy.
///
/// Idempotent: normalizing already-normalized bytes is a no-op (the second
/// pass finds the same fixed-length values already in place and rewrites
/// them to themselves).
fn normalize_converted_pdf(pdf: &[u8]) -> Vec<u8> {
    let mut bytes = pdf.to_vec();
    overwrite_paren_value(&mut bytes, b"/CreationDate");
    overwrite_paren_value(&mut bytes, b"/ModDate");
    overwrite_id_hex_strings(&mut bytes);
    overwrite_name_value(&mut bytes, b"/DocChecksum");
    for tag in ["xmp:CreateDate", "xmp:ModifyDate", "xmp:MetadataDate"] {
        overwrite_xml_element_text(&mut bytes, tag);
    }
    bytes
}

/// Rewrite every occurrence of `key`'s parenthesized VALUE (`key(…)` /
/// `key (…)`) to a fixed value of the SAME byte length. Handles backslash
/// escapes and balanced nested parens (a PDF literal string may itself
/// contain unescaped, balanced parens) via [`find_literal_string_end`], the
/// same conservative-but-correct spirit as `skip_pdf_literal_string` in
/// `pdf_decode.rs`. An unterminated literal (malformed input) stops the
/// scan rather than risk corrupting the buffer.
fn overwrite_paren_value(bytes: &mut [u8], key: &[u8]) {
    let mut search_from = 0usize;
    while let Some(offset) = find_subslice(&bytes[search_from..], key) {
        let key_start = search_from + offset;
        let mut cursor = key_start + key.len();
        while bytes
            .get(cursor)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'(') {
            search_from = key_start + key.len();
            continue;
        }
        let value_start = cursor + 1;
        match find_literal_string_end(bytes, value_start) {
            Some(value_end) => {
                let filler = fixed_length_pdf_date_filler(value_end - value_start);
                bytes[value_start..value_end].copy_from_slice(&filler);
                search_from = value_end + 1;
            }
            None => break,
        }
    }
}

/// The index of the unescaped `)` that closes the literal string starting
/// right after its opening `(` (i.e. `start` is the byte AFTER `(`).
/// Balanced nested parens are tracked so an embedded, unescaped `(…)` pair
/// does not terminate the scan early.
fn find_literal_string_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut index = start;
    let mut depth = 1usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = (index + 2).min(bytes.len()),
            b'(' => {
                depth += 1;
                index += 1;
            }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
                index += 1;
            }
            _ => index += 1,
        }
    }
    None
}

/// A deterministic, same-length filler for a PDF date literal-string VALUE,
/// built from the canonical zero-date `D:19700101000000Z` (17 bytes),
/// truncated or zero-padded to match. No paren/backslash bytes ever occur in
/// it, so it is always safe to splice back into a `(...)` literal.
fn fixed_length_pdf_date_filler(length: usize) -> Vec<u8> {
    const CANONICAL: &[u8] = b"D:19700101000000Z";
    if length <= CANONICAL.len() {
        CANONICAL[..length].to_vec()
    } else {
        let mut filler = CANONICAL.to_vec();
        filler.resize(length, b'0');
        filler
    }
}

/// Rewrite the two hex strings of every `/ID [<…><…>]` occurrence to
/// same-length all-zero hex. Handles whitespace/newline between the two hex
/// strings (LibreOffice wraps the line there) since [`find_hex_string`]
/// simply seeks the next `<`.
fn overwrite_id_hex_strings(bytes: &mut [u8]) {
    let mut search_from = 0usize;
    while let Some(offset) = find_subslice(&bytes[search_from..], b"/ID") {
        let key_start = search_from + offset;
        let mut cursor = key_start + 3;
        while bytes
            .get(cursor)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'[') {
            search_from = key_start + 3;
            continue;
        }
        let Some((first_start, first_end)) = find_hex_string(bytes, cursor + 1) else {
            search_from = key_start + 3;
            continue;
        };
        let Some((second_start, second_end)) = find_hex_string(bytes, first_end + 1) else {
            search_from = key_start + 3;
            continue;
        };
        for byte in &mut bytes[first_start..first_end] {
            *byte = b'0';
        }
        for byte in &mut bytes[second_start..second_end] {
            *byte = b'0';
        }
        search_from = second_end + 1;
    }
}

/// The `(content_start, content_end)` byte range (EXCLUDING the angle
/// brackets) of the next `<…>` string at or after `from`.
fn find_hex_string(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    let open = find_byte(bytes, from, b'<')?;
    let close = find_byte(bytes, open + 1, b'>')?;
    Some((open + 1, close))
}

/// Rewrite the PDF *name* value that follows `key` (`key /VALUE`, e.g.
/// LibreOffice's `/DocChecksum /F9B8AED3…`) to a fixed all-zero value of the
/// SAME byte length. A PDF name token ends at the next delimiter
/// (whitespace or one of `()<>[]{}/%`), mirroring `is_pdf_delimiter` in
/// `pdf_decode.rs`.
fn overwrite_name_value(bytes: &mut [u8], key: &[u8]) {
    let mut search_from = 0usize;
    while let Some(offset) = find_subslice(&bytes[search_from..], key) {
        let key_start = search_from + offset;
        let mut cursor = key_start + key.len();
        while bytes
            .get(cursor)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'/') {
            search_from = key_start + key.len();
            continue;
        }
        let value_start = cursor + 1;
        let mut value_end = value_start;
        while bytes
            .get(value_end)
            .is_some_and(|byte| !is_pdf_name_delimiter(*byte))
        {
            value_end += 1;
        }
        for byte in &mut bytes[value_start..value_end] {
            *byte = b'0';
        }
        search_from = value_end;
    }
}

fn is_pdf_name_delimiter(byte: u8) -> bool {
    byte.is_ascii_whitespace()
        || matches!(
            byte,
            b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
        )
}

/// Rewrite the text content of every `<tag>…</tag>` occurrence to a fixed
/// value of the SAME byte length. `tag` carries no `<`/`>` — both are added
/// internally. Used for the XMP packet's date elements, which LibreOffice
/// emits as plain (uncompressed) XML inside the PDF's `/Metadata` stream.
fn overwrite_xml_element_text(bytes: &mut [u8], tag: &str) {
    let open_tag = format!("<{tag}>").into_bytes();
    let close_tag = format!("</{tag}>").into_bytes();
    let mut search_from = 0usize;
    while let Some(offset) = find_subslice(&bytes[search_from..], &open_tag) {
        let value_start = search_from + offset + open_tag.len();
        match find_subslice(&bytes[value_start..], &close_tag) {
            Some(rel_end) => {
                let value_end = value_start + rel_end;
                let filler = fixed_length_xml_date_filler(value_end - value_start);
                bytes[value_start..value_end].copy_from_slice(&filler);
                search_from = value_end + close_tag.len();
            }
            None => break,
        }
    }
}

/// A deterministic, same-length filler for an XMP date ELEMENT text value,
/// built from the canonical zero-date `1970-01-01T00:00:00+00:00` (25
/// bytes), truncated or zero-padded to match. Contains no `<`/`>`, so it can
/// never be mistaken for XML markup once spliced back in.
fn fixed_length_xml_date_filler(length: usize) -> Vec<u8> {
    const CANONICAL: &[u8] = b"1970-01-01T00:00:00+00:00";
    if length <= CANONICAL.len() {
        CANONICAL[..length].to_vec()
    } else {
        let mut filler = CANONICAL.to_vec();
        filler.resize(length, b'0');
        filler
    }
}

fn find_byte(bytes: &[u8], from: usize, target: u8) -> Option<usize> {
    bytes
        .get(from..)?
        .iter()
        .position(|&byte| byte == target)
        .map(|offset| from + offset)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::platform::*;

    #[cfg(unix)]
    fn executable_script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).expect("write renderer fixture");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .expect("mark renderer fixture executable");
    }

    fn test_deadline() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }

    fn fixture_converter(script: &Path) -> (OfficeConverter, PathBuf) {
        let program = script.canonicalize().unwrap();
        let package = program.with_extension("runtime");
        std::fs::create_dir(&package).unwrap();
        std::fs::write(package.join("component"), b"original").unwrap();
        let deadline = test_deadline();
        #[cfg(any(target_os = "linux", windows))]
        let runtime = OfficeRuntime::fixture(package.clone());
        #[cfg(target_os = "macos")]
        let runtime = OfficeRuntime::new(&program, deadline).unwrap();
        let roots = runtime.runtime_roots(&program).unwrap();
        #[cfg(not(target_os = "linux"))]
        let roots = roots.into_iter().chain([package.clone()]).collect();
        let renderer = RealRenderer::new(
            &program,
            vec![package.clone()],
            roots,
            runtime,
            &ControlHandle::default(),
            deadline,
        )
        .unwrap();
        (
            OfficeConverter {
                backend: ConverterBackend::Real(renderer),
                version: "unknown".to_owned(),
            },
            package,
        )
    }

    #[cfg(target_os = "macos")]
    fn macos_catalog(component_body: &str) -> Vec<u8> {
        format!(
            r#"<components xmlns="http://openoffice.org/2010/uno-components">{component_body}</components>"#
        )
        .into_bytes()
    }

    #[cfg(target_os = "macos")]
    fn macos_spell_component() -> &'static str {
        r#"<component><implementation name="org.openoffice.lingu.MacOSXSpellChecker" constructor="lingucomponent_MacSpellChecker_get_implementation"/></component>"#
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_path_discovery_does_not_substitute_a_bundle_for_a_script() {
        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("soffice");
        executable_script(&program, "#!/bin/sh\nexit 0\n");
        let canonical = resolve_program(&program, test_deadline()).unwrap().unwrap();
        assert_eq!(canonical, program.canonicalize().unwrap());
        assert!(macos_office_bundle_candidate(&canonical).is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_uno_catalog_requires_well_formed_direct_uno_components() {
        let valid = macos_catalog(macos_spell_component());
        assert!(macos_spell_component_range(&valid).unwrap().is_some());
        assert!(
            macos_spell_component_range(&macos_catalog(""))
                .unwrap()
                .is_none()
        );

        let malformed_root =
            br#"<components xmlns=http://openoffice.org/2010/uno-components></components>"#;
        assert!(macos_spell_component_range(malformed_root).is_err());

        let malformed_nested_attribute = macos_catalog(
            r#"<component broken=><implementation name="org.openoffice.lingu.MacOSXSpellChecker" constructor="lingucomponent_MacSpellChecker_get_implementation"/></component>"#,
        );
        assert!(macos_spell_component_range(&malformed_nested_attribute).is_err());

        let doctype = format!(
            r#"<!DOCTYPE components SYSTEM "untrusted"><components xmlns="http://openoffice.org/2010/uno-components">{}</components>"#,
            macos_spell_component()
        );
        assert!(macos_spell_component_range(doctype.as_bytes()).is_err());

        let wrong_namespace = b"<components xmlns=\"urn:other\">\
            <component><implementation name=\"org.openoffice.lingu.MacOSXSpellChecker\" constructor=\"lingucomponent_MacSpellChecker_get_implementation\"/></component>\
            </components>";
        assert!(macos_spell_component_range(wrong_namespace).is_err());

        let nested_component =
            macos_catalog(&format!("<wrapper>{}</wrapper>", macos_spell_component()));
        assert!(macos_spell_component_range(&nested_component).is_err());

        let nested_implementation = macos_catalog(
            r#"<component><wrapper><implementation name="org.openoffice.lingu.MacOSXSpellChecker" constructor="lingucomponent_MacSpellChecker_get_implementation"/></wrapper></component>"#,
        );
        assert!(macos_spell_component_range(&nested_implementation).is_err());

        let component_namespace_override = macos_catalog(
            r#"<component xmlns="urn:other"><implementation name="org.openoffice.lingu.MacOSXSpellChecker" constructor="lingucomponent_MacSpellChecker_get_implementation"/></component>"#,
        );
        assert!(macos_spell_component_range(&component_namespace_override).is_err());
    }

    // ---- is_office_media -------------------------------------------------

    #[test]
    fn is_office_media_truth_table() {
        assert!(is_office_media(DOCX_MEDIA_TYPE));
        assert!(is_office_media(PPTX_MEDIA_TYPE));
        assert!(!is_office_media(
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        ));
        assert!(!is_office_media("application/pdf"));
        assert!(!is_office_media("text/plain"));
        assert!(!is_office_media("text/markdown"));
        assert!(!is_office_media("application/octet-stream"));
        assert!(!is_office_media("image/png"));
        assert!(!is_office_media(""));
    }

    // ---- resolution / seam -------------------------------------------------

    #[test]
    fn seam_converter_returns_fixture_bytes_verbatim_for_any_input() {
        let _lock = kio_core::test_control::test_env_lock().lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let fixture_path = dir.path().join("fixture.pdf");
        let fixture_bytes = b"%PDF-1.4\nfixture content\n%%EOF";
        std::fs::write(&fixture_path, fixture_bytes).unwrap();

        let _seam = kio_core::test_control::TestEnvGuard::set(
            TEST_OFFICE_CONVERT_ENV,
            fixture_path.as_os_str(),
        );
        let converter = resolve_office_converter_with_control(&ControlHandle::new(
            kio_core::test_control::DebugTestControl::from_env(),
        ))
        .expect("seam converter resolves");
        assert_eq!(converter.version(), "test-converter");

        // "returned verbatim for ANY input" — arbitrary bytes/media type.
        let out = converter
            .convert_to_pdf(b"not actually a docx", DOCX_MEDIA_TYPE)
            .unwrap();
        assert_eq!(out, fixture_bytes);

        let out2 = converter.convert_to_pdf(b"", "text/plain").unwrap();
        assert_eq!(out2, fixture_bytes);
    }

    #[test]
    fn resolution_order_prefers_seam_over_explicit_path() {
        let _lock = kio_core::test_control::test_env_lock().lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let fixture_path = dir.path().join("fixture.pdf");
        std::fs::write(&fixture_path, b"seam-bytes").unwrap();

        let _explicit = kio_core::test_control::TestEnvGuard::set(
            OFFICE_CONVERTER_ENV,
            "/does/not/matter/for/this/case",
        );
        let _seam = kio_core::test_control::TestEnvGuard::set(
            TEST_OFFICE_CONVERT_ENV,
            fixture_path.as_os_str(),
        );
        let converter = resolve_office_converter_with_control(&ControlHandle::new(
            kio_core::test_control::DebugTestControl::from_env(),
        ))
        .expect("seam must win over explicit");
        assert_eq!(converter.version(), "test-converter");
    }

    #[test]
    fn no_converter_available_reports_missing() {
        let _lock = kio_core::test_control::test_env_lock().lock().unwrap();
        let _clear_seam = kio_core::test_control::TestEnvGuard::remove(TEST_OFFICE_CONVERT_ENV);
        let _clear_explicit = kio_core::test_control::TestEnvGuard::remove(OFFICE_CONVERTER_ENV);
        let _scrub_path =
            kio_core::test_control::TestEnvGuard::set("PATH", "/nonexistent-kio-test-path");
        let error = resolve_office_converter_with_control(&ControlHandle::new(
            kio_core::test_control::DebugTestControl::from_env(),
        ))
        .unwrap_err();
        assert_eq!(error.state, "missing");
    }

    #[test]
    fn explicit_converter_pointing_at_missing_binary_reports_missing() {
        let _lock = kio_core::test_control::test_env_lock().lock().unwrap();
        let _clear_seam = kio_core::test_control::TestEnvGuard::remove(TEST_OFFICE_CONVERT_ENV);
        let _explicit = kio_core::test_control::TestEnvGuard::set(
            OFFICE_CONVERTER_ENV,
            "/definitely/not/a/real/kio-office-converter-binary",
        );
        let error = resolve_office_converter_with_control(&ControlHandle::new(
            kio_core::test_control::DebugTestControl::from_env(),
        ))
        .unwrap_err();
        assert_eq!(error.state, "missing");
        assert!(
            error
                .reason
                .contains("/definitely/not/a/real/kio-office-converter-binary"),
            "{error}"
        );
    }

    #[test]
    fn path_candidate_that_exists_but_is_not_a_renderer_is_unusable() {
        let _lock = kio_core::test_control::test_env_lock().lock().unwrap();
        let _clear_seam = kio_core::test_control::TestEnvGuard::remove(TEST_OFFICE_CONVERT_ENV);
        let _clear_explicit = kio_core::test_control::TestEnvGuard::remove(OFFICE_CONVERTER_ENV);
        let directory = tempfile::tempdir().unwrap();
        #[cfg(windows)]
        let name = "soffice.exe";
        #[cfg(not(windows))]
        let name = "soffice";
        std::fs::create_dir(directory.path().join(name)).unwrap();
        let _path = kio_core::test_control::TestEnvGuard::set("PATH", directory.path());
        let error = resolve_office_converter_with_control(&ControlHandle::new(
            kio_core::test_control::DebugTestControl::from_env(),
        ))
        .unwrap_err();
        assert_eq!(error.state, "unusable");
    }

    #[cfg(unix)]
    #[test]
    fn real_renderer_does_not_inherit_secret_environment_and_handles_special_paths() {
        let _lock = kio_core::test_control::test_env_lock().lock().unwrap();
        let directory = tempfile::Builder::new()
            .prefix("kio office ; $ ")
            .tempdir()
            .expect("fixture directory");
        let script = directory.path().join("renderer with spaces");
        executable_script(
            &script,
            "#!/bin/sh\n\
             if [ \"$KIO_CONVERTER_SECRET\" = leak ]; then exit 91; fi\n\
             out=\n\
             while [ \"$#\" -gt 0 ]; do\n\
               if [ \"$1\" = --outdir ]; then shift; out=$1; fi\n\
               shift\n\
             done\n\
             printf '%%PDF-1.4\\nfixture\\n1 0 obj\\n<< /Type /Page >>\\nendobj\\n%%%%EOF\\n' > \"$out/input.pdf\"\n",
        );
        let _secret = kio_core::test_control::TestEnvGuard::set("KIO_CONVERTER_SECRET", "leak");
        let (converter, _) = fixture_converter(&script);
        assert_eq!(converter.version(), "unknown");
        let pdf = converter
            .convert_to_pdf(DOCX_PROBE_FIXTURE, DOCX_MEDIA_TYPE)
            .expect("secret must not be inherited and special paths must work");
        assert!(pdf.starts_with(b"%PDF"));
    }

    #[cfg(unix)]
    #[test]
    fn real_renderer_receives_private_staged_input_and_output_directory() {
        const CHILD: &str = "KIO_OFFICE_PRIVATE_STAGE_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new("/bin/sh")
                .args(["-c", "umask 022; exec \"$@\"", "kio-office-private-stage"])
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "office_convert::tests::real_renderer_receives_private_staged_input_and_output_directory",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success(), "private staging regression child failed");
            return;
        }
        let _lock = kio_core::test_control::test_env_lock().lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("renderer");
        executable_script(
            &script,
            "#!/usr/bin/perl\n\
             my $out; for (my $i=0; $i < @ARGV-1; $i++) { $out=$ARGV[$i+1] if $ARGV[$i] eq '--outdir'; }\n\
             defined($out) or die 'no output directory';\n\
             my $input = $ARGV[-1];\n\
             ((stat($input))[2] & 0777) == 0600 or die 'staged input is not private';\n\
             ((stat($out))[2] & 0777) == 0700 or die 'staged output directory is not private';\n\
             open(my $pdf, '>', qq($out/input.pdf)) or die qq(open output: $!);\n\
             print $pdf qq(%PDF-1.4\\n1 0 obj\\n<< /Type /Page >>\\nendobj\\n%%EOF\\n); close($pdf) or die qq(close output: $!);\n",
        );
        let (converter, _) = fixture_converter(&script);
        let converter = converter.with_native_acceptance_diagnostics();
        let pdf = converter
            .convert_to_pdf(DOCX_PROBE_FIXTURE, DOCX_MEDIA_TYPE)
            .expect("renderer must receive private input and output directory under umask 022");
        assert!(pdf.starts_with(b"%PDF"));
    }

    #[cfg(unix)]
    #[test]
    fn real_renderer_rejects_executable_replacement_after_probe() {
        let _lock = kio_core::test_control::test_env_lock().lock().unwrap();
        let directory = tempfile::tempdir().expect("fixture directory");
        let script = directory.path().join("renderer");

        executable_script(
            &script,
            "#!/bin/sh\n\
             printf '%%PDF-1.4\\n1 0 obj\\n<< /Type /Page >>\\nendobj\\n%%%%EOF\\n' > \"$6/input.pdf\"\n",
        );
        let (converter, _) = fixture_converter(&script);

        executable_script(&script, "#!/bin/sh\nprintf 'replacement renderer 2\\n'\n");
        let error = converter
            .convert_to_pdf(DOCX_PROBE_FIXTURE, DOCX_MEDIA_TYPE)
            .expect_err("a converter replacement must invalidate the resolved identity");
        assert!(
            error
                .to_string()
                .contains("executable changed before conversion")
        );
    }

    #[cfg(unix)]
    #[test]
    fn renderer_output_symlink_is_rejected_without_reading_its_target() {
        let _lock = kio_core::test_control::test_env_lock().lock().unwrap();
        let directory = tempfile::Builder::new()
            .prefix("kio-office-symlink-")
            .tempdir()
            .expect("fixture directory");
        let script = directory.path().join("renderer");
        let private_target = PathBuf::from(format!("{}-private.pdf", script.display()));
        std::fs::write(&private_target, b"%PDF-1.4\nprivate fixture bytes\n%%EOF\n")
            .expect("private target");
        executable_script(
            &script,
            "#!/usr/bin/perl\n\
             my $out; for (my $i=0; $i < @ARGV-1; $i++) { $out=$ARGV[$i+1] if $ARGV[$i] eq '--outdir'; }\n\
             defined($out) or die 'no output directory';\n\
             symlink(qq($0-private.pdf), qq($out/input.pdf)) or die qq(symlink: $!);\n",
        );
        let (converter, _) = fixture_converter(&script);
        let error = converter
            .convert_to_pdf(DOCX_PROBE_FIXTURE, DOCX_MEDIA_TYPE)
            .expect_err("a renderer-controlled output symlink must be rejected");
        let message = error.to_string();
        assert!(
            message.contains("unsafe")
                || message.contains("absent")
                || message.contains("renderer scratch contains a symlink"),
            "unexpected safe renderer rejection: {message}"
        );
        assert!(!message.contains("private fixture bytes"));
    }

    #[cfg(unix)]
    #[test]
    fn renderer_cannot_redirect_replaced_output_directory() {
        let _lock = kio_core::test_control::test_env_lock().lock().unwrap();
        let directory = tempfile::Builder::new()
            .prefix("kio-office-output-dir-")
            .tempdir()
            .expect("fixture directory");
        let script = directory.path().join("renderer");
        let private_target = PathBuf::from(format!("{}-private-dir", script.display()));
        std::fs::create_dir(&private_target).expect("private target directory");
        std::fs::write(
            private_target.join("input.pdf"),
            b"%PDF-1.4\nprivate fixture bytes\n%%EOF\n",
        )
        .expect("private target");
        executable_script(
            &script,
            "#!/usr/bin/perl\n\
             my $out; for (my $i=0; $i < @ARGV-1; $i++) { $out=$ARGV[$i+1] if $ARGV[$i] eq '--outdir'; }\n\
             defined($out) or die 'no output directory';\n\
             rmdir($out) or die qq(rmdir: $!);\n\
             symlink(qq($0-private-dir), $out) or die qq(symlink: $!);\n",
        );
        let (converter, _) = fixture_converter(&script);
        let error = converter
            .convert_to_pdf(DOCX_PROBE_FIXTURE, DOCX_MEDIA_TYPE)
            .expect_err("a replaced output directory must not redirect the retained read");
        let message = error.to_string();
        assert!(
            message.contains("did not produce")
                || message.contains("absent")
                || message.contains("unsafe")
                || message.contains("renderer scratch contains a symlink"),
            "unexpected safe renderer rejection: {message}"
        );
        assert!(!message.contains("private fixture bytes"));
    }

    #[test]
    fn file_url_percent_encodes_reserved_characters() {
        #[cfg(not(windows))]
        let path = Path::new("/private/kio office#?%.profile");
        #[cfg(windows)]
        let path = Path::new(r"C:\private\kio office#?%.profile");
        let url = file_url(path).expect("Unicode fixture path");
        #[cfg(not(windows))]
        assert_eq!(url, "file:///private/kio%20office%23%3F%25.profile");
        #[cfg(windows)]
        assert_eq!(url, "file:///C:/private/kio%20office%23%3F%25.profile");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn renderer_runtime_roots_never_admit_program_directory_or_ancestors() {
        let fixture = tempfile::Builder::new()
            .prefix("kio-office-runtime-")
            .tempdir()
            .expect("fixture directory");
        let home = fixture.path().join("home");
        let program = home.join("bin").join("soffice");
        let roots = renderer_runtime_roots(&program, None).expect("runtime roots");
        assert!(
            !roots.contains(&home.join("bin")) && !roots.contains(&home),
            "a caller-selected converter must not grant its directory or home"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_renderer_environment_uses_only_private_profile_state() {
        let windows = Path::new(r"C:\Windows");
        let home = Path::new(r"C:\kio scratch\private プロファイル");
        let environment = renderer_environment(windows, Some(home));
        for name in ["HOME", "USERPROFILE", "LOCALAPPDATA"] {
            let values: Vec<_> = environment
                .iter()
                .filter(|(key, _)| key == name)
                .map(|(_, value)| value.as_os_str())
                .collect();
            assert_eq!(values, vec![home.as_os_str()], "{name}");
        }
        // An exhaustive key allowlist rejects ambient credentials, proxies,
        // and user state without mutating the process-wide test environment.
        for (key, _) in &environment {
            assert!(
                [
                    "SystemRoot",
                    "WINDIR",
                    "PATH",
                    "HOME",
                    "USERPROFILE",
                    "LOCALAPPDATA",
                ]
                .iter()
                .any(|allowed| key == *allowed),
                "unexpected renderer environment key: {key:?}"
            );
        }
        let without_home = renderer_environment(windows, None);
        assert!(without_home.iter().all(|(key, _)| {
            !["HOME", "USERPROFILE", "LOCALAPPDATA"]
                .iter()
                .any(|name| key == name)
        }));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_renderer_disables_python_bytecode_writes() {
        let environment = renderer_environment(Path::new("/custom/soffice"), None);
        assert!(environment.contains(&(
            OsString::from("PYTHONDONTWRITEBYTECODE"),
            OsString::from("1")
        )));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_bundle_candidate_requires_exact_app_layout() {
        let app = Path::new("/private/task/LibreOffice.app");
        assert_eq!(
            macos_office_bundle_candidate(&app.join("Contents/MacOS/alternate-renderer")),
            Some(app.to_path_buf())
        );
        for path in [
            "relative.app/Contents/MacOS/soffice",
            "/private/LibreOffice/Contents/MacOS/soffice",
            "/private/LibreOffice.app/MacOS/soffice",
            "/private/LibreOffice.app/Contents/bin/soffice",
            "/private/bin/soffice",
        ] {
            assert_eq!(
                macos_office_bundle_candidate(Path::new(path)),
                None,
                "{path}"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_rejects_an_executable_other_than_the_declared_bundle_main() {
        let (_directory, bundle, program, _source) = fake_office_bundle();
        std::fs::write(
            bundle.join("Contents/Info.plist"),
            br#"<?xml version="1.0"?><plist version="1.0"><dict>
            <key>CFBundleExecutable</key><string>alternate-renderer</string></dict></plist>"#,
        )
        .unwrap();
        let error = verify_macos_office_bundle(&bundle, &program, test_deadline()).unwrap_err();
        assert!(
            error.to_string().contains("bundle's main executable"),
            "{error}"
        );
    }

    #[cfg(target_os = "macos")]
    fn fake_office_bundle() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(directory.path()).unwrap();
        let bundle = base.join("LibreOffice.app");
        std::fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
        std::fs::create_dir_all(bundle.join("Contents/Resources/services")).unwrap();
        let program = bundle.join("Contents/MacOS/soffice");
        let source = bundle.join("Contents/Resources/services/services.rdb");
        std::fs::write(&program, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::write(&source, b"<components/>").unwrap();
        (directory, bundle, program, source)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_forged_bundle_identity_does_not_authorize_runtime() {
        let (_directory, bundle, program, source) = fake_office_bundle();
        std::fs::write(source, macos_catalog(macos_spell_component())).unwrap();
        std::fs::write(
            bundle.join("Contents/Info.plist"),
            br#"<?xml version="1.0"?><plist version="1.0"><dict>
            <key>CFBundleIdentifier</key><string>org.libreoffice.script</string>
            <key>TeamIdentifier</key><string>7P5S3ZLCN7</string>
            <key>CFBundleExecutable</key><string>soffice</string></dict></plist>"#,
        )
        .unwrap();
        assert!(verify_macos_office_bundle(&bundle, &program, test_deadline()).is_err());
        let deadline = test_deadline();
        let runtime = OfficeRuntime::new(&program, deadline).unwrap();
        let sources = RenderSources::resolve(&program, [bundle.clone()]).unwrap();
        let snapshot = RuntimeSnapshot::new(vec![bundle], &sources, deadline).unwrap();
        assert!(
            runtime
                .verify_runtime_signature(&program, &snapshot, deadline)
                .is_err()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_bundle_authority_rejects_symlink_escapes_and_writable_paths() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let (directory, bundle, program, source) = fake_office_bundle();
        assert!(office_bundle_path_snapshot(&bundle, &program, &source).is_ok());
        let outside = directory.path().join("outside.rdb");
        std::fs::write(&outside, b"<components/>").unwrap();
        std::fs::remove_file(&source).unwrap();
        symlink(&outside, &source).unwrap();
        assert!(office_bundle_path_snapshot(&bundle, &program, &source).is_err());
        std::fs::remove_file(&source).unwrap();
        std::fs::write(&source, b"<components/>").unwrap();
        std::fs::set_permissions(&bundle, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(office_bundle_path_snapshot(&bundle, &program, &source).is_err());
        std::fs::set_permissions(&bundle, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_private_catalog_remains_bound_to_exact_source_bytes() {
        let (_directory, bundle, _program, source) = fake_office_bundle();
        let bytes = macos_catalog(macos_spell_component());
        std::fs::write(&source, &bytes).unwrap();
        let catalog = PrivateUnoCatalog {
            bundle,
            source: source.clone(),
            source_digest: hex_digest(&Sha256::digest(&bytes)),
        };
        let scratch = private_temp_dir().unwrap();
        let filtered = write_private_uno_catalog(&catalog, scratch.path(), test_deadline())
            .unwrap()
            .unwrap();
        assert!(
            !std::fs::read_to_string(filtered)
                .unwrap()
                .contains("MacOSXSpellChecker")
        );
        std::fs::write(&source, b"<malformed").unwrap();
        let next_scratch = private_temp_dir().unwrap();
        assert!(write_private_uno_catalog(&catalog, next_scratch.path(), test_deadline()).is_err());
        let malformed_catalog = PrivateUnoCatalog {
            source_digest: hex_digest(&Sha256::digest(b"<malformed")),
            ..catalog
        };
        assert!(
            write_private_uno_catalog(&malformed_catalog, next_scratch.path(), test_deadline())
                .is_err()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_runtime_roots_admit_only_the_validated_bundle() {
        let (_directory, bundle, program, source) = fake_office_bundle();
        // This constructs the post-verification value only to test root
        // selection; signature verification is tested separately.
        let catalog = PrivateUnoCatalog {
            bundle: bundle.clone(),
            source,
            source_digest: String::new(),
        };
        let roots = renderer_runtime_roots(&program, Some(&catalog)).unwrap();
        assert!(roots.contains(&bundle));
        assert!(!roots.contains(&bundle.parent().unwrap().to_path_buf()));
        assert!(!roots.contains(&bundle.with_file_name("Sibling.app")));
        assert!(!roots.contains(&program.parent().unwrap().to_path_buf()));
        let roots = renderer_runtime_roots(&program, None).unwrap();
        assert!(!roots.contains(&bundle));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_office_scratch_is_current_user_private_at_creation() {
        let scratch = private_temp_dir().expect(
            "Office scratch must be created for TokenUser even with elevated default owner",
        );
        let path = scratch.path().to_path_buf();
        let directory = kio_core::private_fs::verify_private_directory(&path)
            .expect("fresh Office scratch must satisfy the unchanged strict core verifier");
        drop(
            directory
                .create_private_file(Path::new("owned-child"))
                .expect("create a private child to exercise bound recursive cleanup"),
        );
        drop(directory);
        scratch
            .close()
            .expect("explicit Office scratch cleanup must succeed");
        assert!(
            !path.exists(),
            "successful cleanup must retire the original scratch path"
        );
    }

    #[cfg(unix)]
    #[test]
    fn office_scratch_external_alias_keeps_private_children_nofollow() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().unwrap();
        let real = fixture.path().join("external");
        std::fs::create_dir(&real).unwrap();
        let alias = fixture.path().join("alias");
        symlink(&real, &alias).unwrap();
        let external = kio_core::private_fs::retain_external_directory(&real).unwrap();
        external.create_directory(Path::new("scratch")).unwrap();
        let scratch = alias.join("scratch");

        create_private_renderer_state(&scratch, "test").unwrap();
        let directory = bind_private_scratch(&scratch).unwrap();
        kio_core::private_fs::verify_private_directory_at(&directory, Path::new("lo-profile"))
            .unwrap();

        symlink(real.join("scratch"), real.join("linked-scratch")).unwrap();
        assert!(bind_private_scratch(&alias.join("linked-scratch")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn office_scratch_directory_is_owner_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = private_temp_dir().expect("private scratch directory");
        assert_eq!(
            std::fs::metadata(directory.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn office_scratch_uses_the_short_private_socket_root() {
        let directory = private_temp_dir().expect("private scratch directory");
        let socket_root = std::fs::canonicalize("/private/tmp").expect("private socket root");
        assert!(
            directory.path().starts_with(socket_root),
            "Office scratch must leave room for Unix-domain socket names"
        );
    }

    // ---- normalization -----------------------------------------------------

    fn build_fixture_pdf_with_metadata(
        creation_date: &str,
        mod_date: &str,
        id1: &str,
        id2: &str,
    ) -> Vec<u8> {
        format!(
            "%PDF-1.4\n\
             1 0 obj << /Type /Catalog >> endobj\n\
             2 0 obj << /CreationDate({creation_date}) /ModDate({mod_date}) >> endobj\n\
             trailer\n\
             <</Size 3/Root 1 0 R/Info 2 0 R/ID [ <{id1}>\n<{id2}> ]>>\n\
             startxref\n0\n%%EOF\n"
        )
        .into_bytes()
    }

    #[test]
    fn normalize_rewrites_creationdate_moddate_id_preserving_length_and_is_idempotent() {
        let pdf = build_fixture_pdf_with_metadata(
            "D:20260101120000+09'00'",
            "D:20260102130000+09'00'",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        );
        let normalized = normalize_converted_pdf(&pdf);

        // (b) byte length unchanged.
        assert_eq!(normalized.len(), pdf.len());

        // (a) values rewritten.
        let text = String::from_utf8_lossy(&normalized);
        assert!(!text.contains("20260101120000"));
        assert!(!text.contains("20260102130000"));
        assert!(!text.contains("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        assert!(!text.contains("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"));
        // The fixture's 23-byte date values (with a `'09'00'` tz suffix, matching
        // real LibreOffice output) are longer than the 17-byte canonical filler,
        // so the filler is zero-padded out to the original length.
        assert!(text.contains("/CreationDate(D:19700101000000Z000000)"));
        assert!(text.contains("/ModDate(D:19700101000000Z000000)"));
        let id_needle = "/ID [ <";
        let id_pos = text.find(id_needle).expect("ID present");
        let id1_value = &text[id_pos + id_needle.len()..id_pos + id_needle.len() + 32];
        assert!(id1_value.chars().all(|ch| ch == '0'));

        // (c) idempotent.
        let twice = normalize_converted_pdf(&normalized);
        assert_eq!(twice, normalized);
    }

    #[test]
    fn normalize_handles_multiple_occurrences_docchecksum_and_xmp_dates() {
        let mut pdf = build_fixture_pdf_with_metadata(
            "D:20260101120000+09'00'",
            "D:20260102130000+09'00'",
            "1111111111111111111111111111aa",
            "2222222222222222222222222222bb",
        );
        pdf.extend_from_slice(
            b"\n15 0 obj <</Type/Metadata/Subtype/XML>>\nstream\n\
              <x:xmpmeta><rdf:RDF><rdf:Description xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\">\
              <xmp:CreateDate>2026-01-01T12:00:00+09:00</xmp:CreateDate>\
              <xmp:ModifyDate>2026-01-02T13:00:00+09:00</xmp:ModifyDate>\
              <xmp:MetadataDate>2026-01-02T13:00:00+09:00</xmp:MetadataDate>\
              </rdf:Description></rdf:RDF></x:xmpmeta>\nendstream\nendobj\n\
              16 0 obj << /DocChecksum /F9B8AED31B45B7E5448225558F0F91DC >> endobj\n\
              17 0 obj << /CreationDate(D:20260101120000+09'00') >> endobj\n",
        );

        let normalized = normalize_converted_pdf(&pdf);
        assert_eq!(normalized.len(), pdf.len());

        let text = String::from_utf8_lossy(&normalized);
        assert!(!text.contains("2026-01-01T12:00:00"));
        assert!(!text.contains("2026-01-02T13:00:00"));
        assert!(!text.contains("F9B8AED31B45B7E5448225558F0F91DC"));
        // Both /CreationDate occurrences (multiple-occurrence handling); see
        // the padding note in the test above for why the filler is 23 bytes.
        assert_eq!(
            text.matches("/CreationDate(D:19700101000000Z000000)")
                .count(),
            2
        );
        assert!(text.contains("<xmp:CreateDate>1970-01-01T00:00:00+00:00</xmp:CreateDate>"));
        assert!(text.contains("<xmp:ModifyDate>1970-01-01T00:00:00+00:00</xmp:ModifyDate>"));
        assert!(text.contains("<xmp:MetadataDate>1970-01-01T00:00:00+00:00</xmp:MetadataDate>"));

        let checksum_needle = "/DocChecksum /";
        let pos = text.find(checksum_needle).expect("DocChecksum present");
        let value = &text[pos + checksum_needle.len()..pos + checksum_needle.len() + 32];
        assert!(value.chars().all(|ch| ch == '0'));

        let twice = normalize_converted_pdf(&normalized);
        assert_eq!(twice, normalized, "normalization must be idempotent");
    }

    #[test]
    fn normalize_is_a_no_op_when_no_volatile_fields_are_present() {
        let pdf = b"%PDF-1.4\n1 0 obj << /Type /Catalog >> endobj\n%%EOF\n".to_vec();
        let normalized = normalize_converted_pdf(&pdf);
        assert_eq!(normalized, pdf);
    }

    fn probe_pdf(text: &str, compressed_page: bool) -> Vec<u8> {
        probe_pdf_with_length(text, compressed_page, false)
    }

    fn probe_pdf_with_length(text: &str, compressed_page: bool, indirect_length: bool) -> Vec<u8> {
        let page = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>";
        let content = format!("BT /F1 12 Tf 20 20 Td ({text}) Tj ET");
        let mut pdf = b"%PDF-1.6\n".to_vec();
        let mut offsets = [0; 8];
        let mut object = |id: usize, dict: &str, stream: Option<&[u8]>| {
            offsets[id] = pdf.len();
            pdf.extend(format!("{id} 0 obj\n{dict}\n").as_bytes());
            if let Some(stream) = stream {
                pdf.extend_from_slice(b"stream\n");
                pdf.extend_from_slice(stream);
                pdf.extend_from_slice(b"\nendstream\n");
            }
            pdf.extend_from_slice(b"endobj\n");
        };
        object(1, "<< /Type /Catalog /Pages 2 0 R >>", None);
        object(2, "<< /Type /Pages /Count 1 /Kids [3 0 R] >>", None);
        if compressed_page {
            let packed =
                miniz_oxide::deflate::compress_to_vec_zlib(format!("3 0 {page}").as_bytes(), 6);
            object(
                7,
                &format!(
                    "<< /Type /ObjStm /N 1 /First 4 /Filter /FlateDecode /Length {} >>",
                    packed.len()
                ),
                Some(&packed),
            );
        } else {
            object(3, page, None);
        }
        let length = if indirect_length {
            "7 0 R".to_owned()
        } else {
            content.len().to_string()
        };
        object(
            4,
            &format!("<< /Length {length} >>"),
            Some(content.as_bytes()),
        );
        object(
            5,
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /ToUnicode 6 0 R >>",
            None,
        );
        let cmap = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap\n\
            1 begincodespacerange <20> <7E> endcodespacerange\n\
            1 beginbfrange <20> <7E> <0020> endbfrange\n\
            endcmap CMapName currentdict /CMap defineresource pop end end";
        object(6, &format!("<< /Length {} >>", cmap.len()), Some(cmap));
        if indirect_length {
            assert!(!compressed_page);
            object(7, &content.len().to_string(), None);
        }
        let xref = pdf.len();
        if compressed_page {
            let mut data = vec![0, 0, 0, 0, 0, 255, 255];
            for (id, offset) in offsets
                .iter()
                .copied()
                .chain(std::iter::once(xref))
                .enumerate()
                .skip(1)
            {
                let (kind, offset) = if id == 3 { (2, 7) } else { (1, offset) };
                data.push(kind);
                data.extend_from_slice(&(offset as u32).to_be_bytes());
                data.extend_from_slice(&[0, 0]);
            }
            pdf.extend(format!("8 0 obj\n<< /Type /XRef /Size 9 /Root 1 0 R /W [1 4 2] /Length {} >>\nstream\n", data.len()).as_bytes());
            pdf.extend(&data);
            pdf.extend_from_slice(b"\nendstream\nendobj\n");
        } else {
            let size = if indirect_length { 8 } else { 7 };
            pdf.extend(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
            for offset in &offsets[1..size] {
                pdf.extend(format!("{offset:010} 00000 n \n").as_bytes());
            }
            pdf.extend(format!("trailer\n<< /Size {size} /Root 1 0 R >>\n").as_bytes());
        }
        pdf.extend(format!("startxref\n{xref}\n%%EOF\n").as_bytes());
        pdf
    }

    fn rendered_probe_fixture(media_type: &str) -> Vec<u8> {
        if media_type == DOCX_MEDIA_TYPE {
            probe_pdf(DOCX_PROBE_TEXT, false)
        } else {
            probe_pdf(PPTX_PROBE_TEXT, true)
        }
    }

    #[test]
    fn probe_requires_both_fixture_conversions_without_a_version_line() {
        let mut media_types = Vec::new();
        let version = probe_renderer_capability(
            || Ok("  \r\n".to_owned()),
            |bytes, media_type| {
                crate::ooxml_package::validate_real_office_package(bytes, media_type)?;
                media_types.push(media_type.to_owned());
                Ok(rendered_probe_fixture(media_type))
            },
            test_deadline(),
        )
        .expect("successful fixture conversions establish capability");
        assert_eq!(media_types, [DOCX_MEDIA_TYPE, PPTX_MEDIA_TYPE]);
        assert_eq!(version, "unknown");
    }

    // The supplied soffice capture exercises the Writer TJ text run.
    const LIBREOFFICE_DOCX_PROBE_PDF: &[u8] =
        include_bytes!("../fixtures/office-probe-libreoffice-docx.pdf");
    // LibreOfficeKit export of the probe slide with a text box sized to fit the marker.
    const LIBREOFFICE_PPTX_PROBE_PDF: &[u8] =
        include_bytes!("../fixtures/office-probe-libreoffice-pptx.pdf");
    // The supplied soffice capture wraps one glyph per row and positions the tail off the page.
    const LIBREOFFICE_CLIPPED_PPTX_PROBE_PDF: &[u8] =
        include_bytes!("../fixtures/office-probe-libreoffice-pptx-clipped.pdf");

    fn probe_captured_libreoffice_pdfs(presentation: &[u8]) -> Result<String> {
        probe_renderer_capability(
            || Ok("LibreOffice 24.2".to_owned()),
            |_, media_type| match media_type {
                DOCX_MEDIA_TYPE => Ok(LIBREOFFICE_DOCX_PROBE_PDF.to_vec()),
                PPTX_MEDIA_TYPE => Ok(presentation.to_vec()),
                _ => panic!("unexpected probe media type: {media_type}"),
            },
            test_deadline(),
        )
    }

    #[test]
    fn probe_accepts_captured_libreoffice_docx_and_pptx() {
        let version = probe_captured_libreoffice_pdfs(LIBREOFFICE_PPTX_PROBE_PDF)
            .expect("real LibreOffice PDFs contain both expected probe pages");
        assert_eq!(version, "LibreOffice 24.2");
    }

    #[test]
    fn probe_rejects_captured_libreoffice_pptx_with_clipped_marker() {
        let error = probe_captured_libreoffice_pdfs(LIBREOFFICE_CLIPPED_PPTX_PROBE_PDF)
            .expect_err("a marker that runs off the slide does not establish capability");
        assert!(
            error
                .to_string()
                .contains("did not contain the expected pptx page"),
            "{error}"
        );
    }

    #[test]
    fn probe_rejects_inactive_wrong_generation_and_wrong_offset_xref_entries() {
        let good = String::from_utf8(probe_pdf(DOCX_PROBE_TEXT, false)).unwrap();
        let rows = good
            .split_once("xref\n0 7\n")
            .unwrap()
            .1
            .lines()
            .collect::<Vec<_>>();
        for (id, replacement) in [
            (3, "0000000000 00001 f ".to_owned()),
            (4, rows[4].replace("00000 n ", "00001 n ")),
            (4, rows[3].to_owned()),
        ] {
            let bad = good.replacen(rows[id], &replacement, 1);
            assert!(
                crate::pdf_decode::office_probe_page_text(bad.as_bytes(), test_deadline())
                    .ok()
                    .flatten()
                    .is_none(),
                "accepted broken xref row {id}: {replacement}"
            );
        }
    }

    #[test]
    fn probe_resolves_content_references_by_object_number_and_generation() {
        let pdf = String::from_utf8(probe_pdf(DOCX_PROBE_TEXT, false)).unwrap();
        let content_row = pdf
            .split_once("xref\n0 7\n")
            .unwrap()
            .1
            .lines()
            .nth(4)
            .unwrap();
        let pdf = pdf.replacen("4 0 obj", "4 1 obj", 1).replacen(
            content_row,
            &content_row.replace("00000 n", "00001 n"),
            1,
        );
        let error =
            crate::pdf_decode::office_probe_page_text(pdf.as_bytes(), test_deadline()).unwrap_err();
        assert!(
            error.to_string().contains("content reference is absent"),
            "{error}"
        );

        let pdf = pdf.replace("/Contents 4 0 R", "/Contents 4 1 R");
        let text =
            crate::pdf_decode::office_probe_page_text(pdf.as_bytes(), test_deadline()).unwrap();
        assert_eq!(text.as_deref().map(str::trim), Some(DOCX_PROBE_TEXT));
    }

    #[test]
    fn program_discovery_rejects_a_result_returned_after_the_budget() {
        let deadline = Instant::now() + Duration::from_millis(30);
        let error = find_program_candidate(Path::new("/fixture"), deadline, |path| {
            std::thread::sleep(
                deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(1),
            );
            Some(path)
        })
        .unwrap_err();
        assert!(error.to_string().contains("time limit"));
    }

    #[test]
    fn probe_rejects_wrong_object_stream_member_index() {
        let mut pdf = probe_pdf(PPTX_PROBE_TEXT, true);
        let marker = b"/W [1 4 2]";
        let width = pdf
            .windows(marker.len())
            .position(|bytes| bytes == marker)
            .unwrap();
        let data = width
            + pdf[width..]
                .windows(8)
                .position(|bytes| bytes == b"\nstream\n")
                .unwrap()
            + 8;
        pdf[data + 3 * 7 + 6] = 1;
        assert!(
            crate::pdf_decode::office_probe_page_text(&pdf, test_deadline())
                .ok()
                .flatten()
                .is_none()
        );
    }

    #[test]
    fn probe_resolves_indirect_stream_length_after_the_stream_object() {
        let pdf = probe_pdf_with_length(DOCX_PROBE_TEXT, false, true);
        let text = crate::pdf_decode::office_probe_page_text(&pdf, test_deadline()).unwrap();
        assert_eq!(text.as_deref().map(str::trim), Some(DOCX_PROBE_TEXT));
    }

    #[test]
    fn probe_resolves_flate_xref_stream_with_png_prediction() {
        let mut pdf = probe_pdf(PPTX_PROBE_TEXT, true);
        let marker = b"8 0 obj\n<< /Type /XRef";
        let xref = pdf
            .windows(marker.len())
            .position(|bytes| bytes == marker)
            .unwrap();
        let start = xref
            + pdf[xref..]
                .windows(8)
                .position(|bytes| bytes == b"\nstream\n")
                .unwrap()
            + 8;
        let mut encoded = Vec::new();
        let mut previous = [0; 7];
        for row in pdf[start..start + 9 * 7].as_chunks::<7>().0 {
            encoded.push(2);
            encoded.extend(
                row.iter()
                    .zip(previous)
                    .map(|(byte, above)| byte.wrapping_sub(above)),
            );
            previous = *row;
        }
        let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&encoded, 6);
        pdf.truncate(xref);
        pdf.extend(format!(
            "8 0 obj\n<< /Type /XRef /Size 9 /Root 1 0 R /W [1 4 2] /Filter /FlateDecode /DecodeParms << /Predictor 12 /Columns 7 >> /Length {} >>\nstream\n",
            compressed.len()
        ).as_bytes());
        pdf.extend(compressed);
        pdf.extend(format!("\nendstream\nendobj\nstartxref\n{xref}\n%%EOF\n").as_bytes());
        let text = crate::pdf_decode::office_probe_page_text(&pdf, test_deadline()).unwrap();
        assert_eq!(text.as_deref().map(str::trim), Some(PPTX_PROBE_TEXT));
    }

    #[test]
    fn probe_refuses_large_renderer_output_before_decoding() {
        let result = probe_renderer_capability(
            || panic!("version queried with oversized PDF"),
            |_, media| {
                let mut pdf = rendered_probe_fixture(media);
                pdf.resize(MAX_OFFICE_PROBE_PDF_BYTES + 1, b' ');
                Ok(pdf)
            },
            test_deadline(),
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("probe PDF exceeds")
        );
    }

    #[test]
    fn probe_rejects_unreachable_page_with_pdf_markers() {
        let result = probe_renderer_capability(
            || Ok("test renderer".to_owned()),
            |_, _| Ok(b"%PDF-1.4\n1 0 obj\n<< /Type /Page >>\nendobj\n%%EOF\n".to_vec()),
            test_deadline(),
        );
        assert!(result.is_err());
        let mut unreachable = probe_pdf(DOCX_PROBE_TEXT, false);
        let kids = unreachable
            .windows(12)
            .position(|bytes| bytes == b"/Kids [3 0 R")
            .unwrap();
        unreachable[kids + 7] = b'5';
        assert!(
            crate::pdf_decode::office_probe_page_text(&unreachable, test_deadline())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn probe_rejects_pdf_with_a_root_reference_only_in_a_nested_dictionary() {
        let pdf = String::from_utf8(probe_pdf(DOCX_PROBE_TEXT, false)).unwrap();
        let pdf = pdf.replace(
            "<< /Size 7 /Root 1 0 R >>",
            "<< /Size 7 /Other << /Root 1 0 R >> >>",
        );
        assert!(
            crate::pdf_decode::office_probe_page_text(pdf.as_bytes(), test_deadline())
                .ok()
                .flatten()
                .is_none()
        );
    }

    #[test]
    fn probe_preserves_capability_after_clean_version_timeout_or_output_limit() {
        for error in [
            BoundedProcessError::Timeout { timeout_ms: 10 },
            BoundedProcessError::OutputLimit {
                stream: "stdout",
                limit: MAX_OFFICE_LOG_BYTES,
            },
        ] {
            let version = probe_renderer_capability(
                || {
                    advisory_version_output(Err(error.into()))
                        .map_err(|error| AdapterError::ContractViolation(error.to_string()))
                },
                |_, media_type| Ok(rendered_probe_fixture(media_type)),
                test_deadline(),
            );
            assert_eq!(version.unwrap(), "unknown");
        }
        let failure = BoundedProcessError::RendererCleanup(std::io::Error::other("unverified"));
        assert!(advisory_version_output(Err(failure.into())).is_err());
    }

    #[test]
    fn probe_rejects_pptx_pdf_with_docx_content_even_with_a_version_line() {
        let error = probe_renderer_capability(
            || Ok("LibreOffice 26.8.0.3".to_owned()),
            |_, _| Ok(probe_pdf(DOCX_PROBE_TEXT, false)),
            test_deadline(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("pptx"), "{error}");
    }

    #[test]
    fn deadline_reached_during_version_verification_is_typed() {
        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("renderer");
        std::fs::write(&program, b"fixture").unwrap();
        let (converter, _) = fixture_converter(&program);
        let ConverterBackend::Real(renderer) = converter.backend else {
            unreachable!()
        };
        let error = renderer
            .verify_runtime("before version query", Instant::now())
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn probe_rejects_an_expired_budget_before_rendering() {
        let result = probe_renderer_capability(
            || panic!("version queried after the deadline"),
            |_, _| panic!("renderer invoked after the deadline"),
            Instant::now(),
        );
        assert!(result.unwrap_err().to_string().contains("time limit"));
    }

    #[test]
    fn probe_checks_remaining_budget_again_after_reading_output() {
        let deadline = Instant::now() + Duration::from_millis(50);
        let result = probe_renderer_capability(
            || panic!("version queried after the deadline"),
            |_, _| {
                std::thread::sleep(
                    deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(1),
                );
                Ok(probe_pdf(DOCX_PROBE_TEXT, false))
            },
            deadline,
        );
        assert!(result.unwrap_err().to_string().contains("time limit"));
    }

    #[cfg(unix)]
    #[test]
    fn probe_refuses_a_renderer_without_an_identifiable_runtime_package() {
        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().canonicalize().unwrap().join("standalone");
        std::fs::write(&program, b"fixture").unwrap();
        let error =
            probe_real_converter_diagnostic(program, &ControlHandle::default(), test_deadline())
                .unwrap_err();
        assert!(
            error.to_string().contains("cannot identify its package"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn conversion_verifies_the_resolved_program_even_after_restoring_the_original_name() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let original = root.join("renderer");
        let saved = root.join("saved");
        let substituted = root.join("substituted");
        std::fs::write(&original, b"original").unwrap();
        std::fs::write(&substituted, b"alternate").unwrap();
        let (mut converter, package) = fixture_converter(&original);
        let ConverterBackend::Real(renderer) = &mut converter.backend else {
            unreachable!()
        };
        std::fs::rename(&original, &saved).unwrap();
        symlink(&substituted, &original).unwrap();
        renderer.sources = RenderSources::resolve(&original, [package]).unwrap();
        std::fs::remove_file(&original).unwrap();
        std::fs::rename(saved, &original).unwrap();

        renderer
            .program_fingerprint
            .verify(&original, "control", test_deadline())
            .unwrap();
        let error = converter
            .convert_to_pdf(DOCX_PROBE_FIXTURE, DOCX_MEDIA_TYPE)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("executable changed before conversion"),
            "{error}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_rejects_runtime_replacement_at_conversion_entry() {
        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("soffice.exe");
        std::fs::write(&program, b"fixture").unwrap();
        let (converter, package) = fixture_converter(&program);
        let component = package.join("component");
        let replacement = directory.path().join("replacement");
        std::fs::write(&replacement, b"original").unwrap();
        std::fs::remove_file(&component).unwrap();
        std::fs::rename(replacement, component).unwrap();
        let error = converter
            .convert_to_pdf(DOCX_PROBE_FIXTURE, DOCX_MEDIA_TYPE)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("runtime changed before conversion"),
            "{error}"
        );
    }

    #[test]
    fn executable_identity_rejects_same_bytes_at_a_replaced_path() {
        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("renderer");
        let replacement = directory.path().join("replacement");
        std::fs::write(&program, b"renderer").unwrap();
        std::fs::write(&replacement, b"renderer").unwrap();
        let before = ExecutableFingerprint::new(&program, test_deadline()).unwrap();
        std::fs::remove_file(&program).unwrap();
        std::fs::rename(replacement, &program).unwrap();
        assert!(
            before
                .verify(&program, "before conversion", test_deadline())
                .is_err()
        );
    }

    #[test]
    fn bundled_office_fixtures_pass_offline_package_preflight() {
        crate::ooxml_package::validate_real_office_package(DOCX_PROBE_FIXTURE, DOCX_MEDIA_TYPE)
            .expect("bundled DOCX package");
        crate::ooxml_package::validate_real_office_package(PPTX_PROBE_FIXTURE, PPTX_MEDIA_TYPE)
            .expect("bundled PPTX package");
    }

    /// Native Office acceptance is intentionally opt-in so regular unit tests
    /// never discover or launch an ambient renderer. `KIO_REAL_OFFICE=1`
    /// makes an unavailable or broken explicit/PATH-resolved renderer a test
    /// failure rather than a skip.
    fn required_real_office_program(deadline: Instant) -> Option<PathBuf> {
        if std::env::var_os("KIO_REAL_OFFICE").as_deref() != Some(std::ffi::OsStr::new("1")) {
            eprintln!(
                "skipping native Office acceptance; set KIO_REAL_OFFICE=1 and \
                 KIO_OFFICE_CONVERTER or PATH to require it"
            );
            return None;
        }
        let selected = std::env::var_os(OFFICE_CONVERTER_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_OFFICE_CONVERTER_PROGRAM));
        Some(resolve_program(&selected, deadline).unwrap().expect(
            "KIO_REAL_OFFICE=1 requires KIO_OFFICE_CONVERTER or PATH to resolve a converter",
        ))
    }

    fn required_real_office_converter() -> Option<OfficeConverter> {
        let deadline = Instant::now() + OFFICE_PROBE_TIMEOUT;
        let program = required_real_office_program(deadline)?;
        let _clear_seam = kio_core::test_control::TestEnvGuard::remove(TEST_OFFICE_CONVERT_ENV);
        let converter =
            probe_real_converter_diagnostic(program, &ControlHandle::default(), deadline)
                .expect("KIO_REAL_OFFICE=1 requires a working sandboxed Office converter");
        assert_ne!(converter.version(), "test-converter");
        Some(converter)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_rejects_signed_bundle_runtime_replacement_at_conversion_entry() {
        let _lock = kio_core::test_control::test_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(installed) = required_real_office_program(test_deadline()) else {
            return;
        };
        let bundle =
            macos_office_bundle_candidate(&installed).expect("installed signed Office bundle");
        let temp = tempfile::tempdir().unwrap();
        let private_bundle = temp
            .path()
            .canonicalize()
            .unwrap()
            .join("Renamed Office.app");
        let output = kio_process::run_bounded_command(
            std::process::Command::new("/usr/bin/ditto")
                .arg(&bundle)
                .arg(&private_bundle),
            renderer_process_options(Duration::from_secs(120)),
            None,
        )
        .expect("copy signed fixture bundle");
        assert!(output.status.success(), "{}", output.stderr);
        let program = private_bundle.join("Contents/MacOS/soffice");
        let deadline = Instant::now() + Duration::from_secs(60);
        let runtime =
            OfficeRuntime::new(&program, deadline).expect("verify copied bundle signature");
        let renderer = RealRenderer::new(
            &program,
            runtime.package_roots(&program).unwrap(),
            runtime.runtime_roots(&program).unwrap(),
            runtime,
            &ControlHandle::default(),
            deadline,
        )
        .unwrap();
        renderer
            .verify_initial_runtime(deadline)
            .expect("verify copied bundle signature");
        let converter = OfficeConverter {
            backend: ConverterBackend::Real(renderer),
            version: "unknown".to_owned(),
        };
        let catalog = private_bundle.join("Contents/Resources/services/services.rdb");
        let replacement = temp.path().join("replacement.rdb");
        std::fs::copy(&catalog, &replacement).unwrap();
        std::fs::rename(replacement, catalog).unwrap();
        let error = converter
            .convert_to_pdf(DOCX_PROBE_FIXTURE, DOCX_MEDIA_TYPE)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("runtime changed before conversion"),
            "{error}"
        );
    }

    #[test]
    fn office_real_soffice_docx_converts_deterministically() {
        let _lock = kio_core::test_control::test_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(converter) = required_real_office_converter()
            .map(OfficeConverter::with_native_acceptance_diagnostics)
        else {
            return;
        };

        let docx = DOCX_PROBE_FIXTURE;
        let first = converter
            .convert_to_pdf(docx, DOCX_MEDIA_TYPE)
            .expect("first conversion");
        let second = converter
            .convert_to_pdf(docx, DOCX_MEDIA_TYPE)
            .expect("second conversion");
        assert!(first.starts_with(b"%PDF"), "converted output must be a PDF");
        assert_eq!(
            first, second,
            "normalized converted-PDF bytes must be identical across \
             independent conversions of the same input"
        );

        // FlateDecode round (07 §2.1, 2026-07-23 addendum): a real soffice
        // PDF carries its text as FlateDecode-compressed CID glyph runs.
        // The graph decoder must recover the body text offline — this is
        // the acceptance property the round exists for, and it was NOT
        // covered before (only byte determinism was asserted).
        let mut pages =
            crate::pdf_decode::PdfPages::open(&first, kio_core::error::ResourceTier::Standard)
                .expect("index converted PDF");
        let joined = (0..pages.len())
            .map(|page| {
                pages
                    .read_page(page)
                    .expect("extract converted-PDF text layer")
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("office") && joined.contains("convert"),
            "converted-PDF compressed text layer must decode offline; got {joined:?}"
        );
    }

    /// Same determinism property as the DOCX test above, for PPTX. The
    /// minimal-pptx skeleton (presentation + slide + layout + master +
    /// theme) converts cleanly with the real `soffice` on the implementing
    /// machine, so this is included rather than falling back to docx-only.
    #[test]
    fn office_real_soffice_pptx_converts_deterministically() {
        let _lock = kio_core::test_control::test_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(converter) = required_real_office_converter()
            .map(OfficeConverter::with_native_acceptance_diagnostics)
        else {
            return;
        };

        let pptx = PPTX_PROBE_FIXTURE;
        let first = converter
            .convert_to_pdf(pptx, PPTX_MEDIA_TYPE)
            .expect("first conversion");
        let second = converter
            .convert_to_pdf(pptx, PPTX_MEDIA_TYPE)
            .expect("second conversion");
        assert!(first.starts_with(b"%PDF"), "converted output must be a PDF");
        assert_eq!(
            first, second,
            "normalized converted-PDF bytes must be identical across \
             independent conversions of the same input"
        );
    }
}

pub(crate) fn encode_file_url_path(path: &str) -> String {
    let mut encoded = String::new();
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b':' | b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}
