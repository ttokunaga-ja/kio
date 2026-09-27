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

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

use kio_process::{
    BoundedProcessOptions,
    confinement::{RenderResourceLimits, RenderSandbox},
};
use sha2::{Digest, Sha256};
use tempfile::{Builder, TempDir};

#[cfg(target_os = "macos")]
use quick_xml::{Reader, events::Event};

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
/// The PATH-resolved program name probed as the last resolution step.
const DEFAULT_OFFICE_CONVERTER_PROGRAM: &str = "soffice";
const OFFICE_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const OFFICE_CONVERT_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_OFFICE_INPUT_BYTES: usize = 100 * 1024 * 1024;
const MAX_OFFICE_PDF_BYTES: usize = 250 * 1024 * 1024;
const MAX_OFFICE_LOG_BYTES: usize = 64 * 1024;
const OFFICE_NORMALIZATION_PROFILE: &str = "pdf-volatile-fields-v1";
const MACOS_UNO_FILTER_VERSION: &str = "macos-spell-component-v1";

// This thread-local switch is intentionally test-only. It permits bounded
// renderer stderr to appear in the assertion from the explicit synthetic
// native acceptance lane, while production errors never forward renderer
// output into application logs or user-visible diagnostics.
#[cfg(test)]
thread_local! {
    static NATIVE_ACCEPTANCE_DIAGNOSTICS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
struct NativeAcceptanceDiagnosticGuard;

#[cfg(test)]
impl NativeAcceptanceDiagnosticGuard {
    fn enable() -> Self {
        NATIVE_ACCEPTANCE_DIAGNOSTICS.with(|enabled| enabled.set(true));
        Self
    }
}

#[cfg(test)]
impl Drop for NativeAcceptanceDiagnosticGuard {
    fn drop(&mut self) {
        NATIVE_ACCEPTANCE_DIAGNOSTICS.with(|enabled| enabled.set(false));
    }
}

#[cfg(test)]
fn native_acceptance_diagnostics_enabled() -> bool {
    NATIVE_ACCEPTANCE_DIAGNOSTICS.with(std::cell::Cell::get)
}

const DOCX_MEDIA_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const PPTX_MEDIA_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.presentationml.presentation";

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
    Seam { fixture_path: PathBuf },
    /// A real `soffice`-compatible binary invoked via [`Command`].
    Real {
        program: PathBuf,
        program_digest: String,
        uno_catalog: Option<PrivateUnoCatalog>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PrivateUnoCatalog {
    #[cfg(target_os = "macos")]
    source: PathBuf,
    #[cfg(target_os = "macos")]
    bundle: PathBuf,
    source_digest: String,
}

/// A resolved Office → PDF converter (07 §5.1). Obtain one via
/// [`resolve_office_converter`].
#[derive(Debug, Clone)]
pub struct OfficeConverter {
    backend: ConverterBackend,
    version: String,
}

impl OfficeConverter {
    /// The renderer's self-reported version (provenance-only — never a hash
    /// input, 07 §5.1). `"test-converter"` for the test seam.
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
            ConverterBackend::Real {
                program,
                program_digest,
                uno_catalog,
            } => {
                digest.update(program.as_os_str().as_encoded_bytes());
                digest.update(b"\0");
                digest.update(program_digest.as_bytes());
                digest.update(b"\0");
                digest.update(
                    uno_catalog
                        .as_ref()
                        .map_or("none", |catalog| &catalog.source_digest)
                        .as_bytes(),
                );
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
            ConverterBackend::Real {
                program,
                program_digest,
                uno_catalog,
            } => {
                crate::ooxml_package::validate_real_office_package(input, media_type)?;
                verify_renderer_fingerprint(program, program_digest, "before conversion")?;
                let pdf =
                    convert_with_real_binary(program, uno_catalog.as_ref(), input, media_type)?;
                verify_renderer_fingerprint(program, program_digest, "after conversion")?;
                Ok(normalize_converted_pdf(&pdf))
            }
        }
    }
}

/// Resolve an Office converter (07 §5.1). Resolution order:
/// [`TEST_OFFICE_CONVERT_ENV`] (test seam) → [`OFFICE_CONVERTER_ENV`]
/// (explicit binary path) → `soffice` on PATH — the first of these that is
/// SET wins outright (a set-but-broken explicit override does not fall
/// through to PATH; that would silently substitute a different converter
/// than the one named). `None` means unavailable — a probe/version failure
/// counts as unavailable, NEVER an `Err` (07 §5.1: "renderer が環境に存在しない
/// 場合...doomed task を作らない" — the caller must be able to silently skip
/// enqueueing rather than crash).
#[must_use]
pub fn resolve_office_converter() -> Option<OfficeConverter> {
    #[cfg(debug_assertions)]
    if let Some(fixture_path) = crate::debug_test_control().adapters.office_convert {
        if fixture_path.is_empty() {
            return None;
        }
        return Some(OfficeConverter {
            #[cfg(debug_assertions)]
            backend: ConverterBackend::Seam { fixture_path },
            version: "test-converter".to_owned(),
        });
    }
    if let Ok(explicit) = std::env::var(OFFICE_CONVERTER_ENV) {
        if explicit.is_empty() {
            return None;
        }
        return resolve_program(PathBuf::from(explicit)).and_then(probe_real_converter);
    }
    resolve_program(PathBuf::from(DEFAULT_OFFICE_CONVERTER_PROGRAM)).and_then(probe_real_converter)
}

/// Resolve the executable before dropping the ambient environment.  The
/// spawned renderer receives a fixed runtime PATH, while this lookup preserves
/// the documented `soffice`-on-PATH resolution at the trusted CLI boundary.
fn resolve_program(program: PathBuf) -> Option<PathBuf> {
    if program.is_absolute() {
        return canonical_renderer_program(program);
    }
    if program.components().count() > 1 {
        return canonical_renderer_program(program);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(&program))
        .find_map(canonical_renderer_program)
}

fn canonical_renderer_program(candidate: PathBuf) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(&candidate).ok()?;
    if !canonical.is_file() {
        return None;
    }
    // Recognize only the known passthrough wrapper by both location and bytes.
    // A command installed at the Homebrew name may select another application
    // or add arguments; its name alone cannot authorize substituting this app.
    #[cfg(target_os = "macos")]
    {
        let known_location = ["/opt/homebrew/bin/soffice", "/usr/local/bin/soffice"]
            .into_iter()
            .filter_map(|path| std::fs::canonicalize(path).ok())
            .any(|path| path == canonical);
        let known_passthrough = known_location
            && kio_core::cas::read_bounded_regular_file(&canonical, 4096)
                .is_ok_and(|bytes| is_macos_office_passthrough_wrapper(&bytes));
        if known_passthrough {
            return std::fs::canonicalize("/Applications/LibreOffice.app/Contents/MacOS/soffice")
                .ok()
                .filter(|program| program.is_file());
        }
    }
    Some(canonical)
}

#[cfg(target_os = "macos")]
fn is_macos_office_passthrough_wrapper(bytes: &[u8]) -> bool {
    bytes == b"#!/bin/bash\nexec \"/Applications/LibreOffice.app/Contents/MacOS/soffice\"  \"$@\"\n"
}

/// Probe a candidate converter binary via `--version`. ANY failure (spawn
/// error / missing binary, non-zero exit, empty stdout) resolves to `None`
/// — never an `Err`, per [`resolve_office_converter`]'s contract.
fn probe_real_converter(program: PathBuf) -> Option<OfficeConverter> {
    probe_real_converter_diagnostic(program).ok()
}

/// Diagnostic variant for native acceptance tests. Production availability
/// remains optional, but a required real-renderer lane can expose a bounded,
/// non-secret failure reason rather than silently treating a broken sandbox as
/// an absent binary.
fn probe_real_converter_diagnostic(program: PathBuf) -> Result<OfficeConverter> {
    let program_digest = fingerprint_renderer_program(&program)?;
    let uno_catalog = private_uno_catalog_for_program(&program)?;
    let scratch = private_temp_dir()?;
    let profile_dir = create_private_renderer_state(scratch.path(), "probe")?;
    let profile_url = file_url(&profile_dir)?;
    let sandbox = RenderSandbox::new(
        &program,
        scratch.path(),
        renderer_runtime_roots(&program, uno_catalog.as_ref())?,
        RenderResourceLimits::default(),
    )
    .map_err(|error| {
        AdapterError::ContractViolation(format!(
            "office converter confinement setup failed: {error}"
        ))
    })?;
    let mut environment = renderer_environment(&program, Some(&profile_dir));
    if let Some(catalog) = uno_catalog.as_ref() {
        let private_catalog = write_private_uno_catalog(catalog, scratch.path())?;
        environment.push((
            OsString::from("URE_MORE_SERVICES"),
            file_url(&private_catalog)?.into(),
        ));
    }
    let output = sandbox
        .run(
            [
                OsString::from("--headless"),
                OsString::from("--norestore"),
                OsString::from(format!("-env:UserInstallation={profile_url}")),
                OsString::from("--version"),
            ],
            &environment,
            BoundedProcessOptions {
                timeout: OFFICE_PROBE_TIMEOUT,
                max_stdout_bytes: MAX_OFFICE_LOG_BYTES,
                max_stderr_bytes: MAX_OFFICE_LOG_BYTES,
            },
        )
        .map_err(|error| {
            AdapterError::ContractViolation(format!(
                "office converter bounded probe failed: {error}"
            ))
        })?;
    if !output.status.success() {
        return Err(AdapterError::ContractViolation(format!(
            "office converter probe exited with {}",
            output.status
        )));
    }
    // The supported official macOS 26.8 build includes the SVP headless
    // backend. The tested 26.2.5 build lacks it: its version probe succeeds,
    // but document conversion enters Cocoa and aborts under confinement.
    #[cfg(target_os = "macos")]
    if uno_catalog.is_some() {
        validate_macos_office_version(&output.stdout)?;
    }
    let version = output
        .stdout
        .lines()
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AdapterError::ContractViolation(
                "office converter probe returned no version line".to_owned(),
            )
        })?;
    Ok(OfficeConverter {
        backend: ConverterBackend::Real {
            program,
            program_digest,
            uno_catalog,
        },
        version: version.to_owned(),
    })
}

/// Supported-release policy for verified macOS LibreOffice bundles only.
/// This prevents known incompatible releases; actual native acceptance remains
/// the capability proof for each pinned distribution.
#[cfg(target_os = "macos")]
fn validate_macos_office_version(stdout: &str) -> Result<()> {
    let malformed = || {
        AdapterError::ContractViolation(
            "verified macOS LibreOffice returned a malformed or ambiguous version".to_owned(),
        )
    };
    let line = stdout.trim();
    if line.len() > 128 || line.contains(['\r', '\n']) {
        return Err(malformed());
    }
    let mut tokens = line.split_ascii_whitespace();
    if tokens.next() != Some("LibreOffice") {
        return Err(malformed());
    }
    let version = tokens.next().ok_or_else(malformed)?;
    if tokens.next().is_some_and(|build| {
        build.len() != 40 || !build.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return Err(malformed());
    }
    if tokens.next().is_some() {
        return Err(malformed());
    }
    let mut components = Vec::new();
    for component in version.split('.') {
        if !(1..=4).contains(&component.len())
            || !component.bytes().all(|byte| byte.is_ascii_digit())
            || (component.len() > 1 && component.starts_with('0'))
        {
            return Err(malformed());
        }
        components.push(component.parse::<u16>().map_err(|_| malformed())?);
    }
    if !(3..=4).contains(&components.len()) {
        return Err(malformed());
    }
    if (components[0], components[1]) < (26, 8) {
        return Err(AdapterError::ContractViolation(
            "sandboxed macOS Office conversion requires LibreOffice 26.8 or newer; the selected release predates the supported headless runtime".to_owned(),
        ));
    }
    Ok(())
}

fn fingerprint_renderer_program(program: &Path) -> Result<String> {
    const MAX_RENDERER_PROGRAM_BYTES: u64 = 32 * 1024 * 1024;
    let bytes = kio_core::cas::read_bounded_regular_file(program, MAX_RENDERER_PROGRAM_BYTES)
        .map_err(|error| {
            AdapterError::ContractViolation(format!(
                "cannot safely fingerprint Office renderer executable: {error}"
            ))
        })?;
    Ok(hex_digest(&Sha256::digest(bytes)))
}

fn verify_renderer_fingerprint(program: &Path, expected: &str, stage: &str) -> Result<()> {
    let actual = fingerprint_renderer_program(program)?;
    if actual != expected {
        return Err(AdapterError::ContractViolation(format!(
            "office converter executable changed {stage}; resolve a fresh converter before retrying"
        )));
    }
    Ok(())
}

fn office_extension(media_type: &str) -> Option<&'static str> {
    match media_type {
        DOCX_MEDIA_TYPE => Some("docx"),
        PPTX_MEDIA_TYPE => Some("pptx"),
        _ => None,
    }
}

/// Create an exclusively allocated, owner-private scratch directory.  The
/// `tempfile` implementation uses a random name and atomic create; on Unix we
/// also set the mode explicitly instead of inheriting the caller's umask.
fn private_temp_dir() -> Result<TempDir> {
    let directory = {
        let mut builder = Builder::new();
        builder.prefix("kio-");
        #[cfg(target_os = "macos")]
        {
            // LibreOffice's Unix-domain socket names must fit sun_path. A
            // canonical short parent leaves room for its generated suffixes;
            // the directory remains exclusively created and owner-private.
            let socket_root = std::fs::canonicalize("/private/tmp").map_err(|error| {
                AdapterError::ContractViolation(format!(
                    "failed to resolve private Office socket root: {error}"
                ))
            })?;
            builder.tempdir_in(socket_root)
        }
        #[cfg(not(target_os = "macos"))]
        {
            builder.tempdir()
        }
    }
    .map_err(|error| {
        AdapterError::ContractViolation(format!(
            "failed to create private office scratch directory: {error}"
        ))
    })?;
    #[cfg(windows)]
    kio_process::confinement::protect_owner_private_scratch(directory.path()).map_err(|error| {
        AdapterError::ContractViolation(format!(
            "failed to establish owner-private office scratch directory: {error}"
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .map_err(|error| {
                AdapterError::ContractViolation(format!(
                    "failed to restrict office scratch directory permissions: {error}"
                ))
            })?;
    }
    Ok(directory)
}

/// Construct all renderer-owned mutable state before process creation. These
/// names are derived only from an exclusively-created private scratch parent;
/// on Windows they inherit its protected owner DACL before AppContainer
/// access is granted, and on Unix each leaf is explicitly mode 0700.
fn create_private_renderer_state(scratch: &Path, purpose: &str) -> Result<PathBuf> {
    let profile = scratch.join("lo-profile");
    let cache = scratch.join("cache");
    let config = scratch.join("config");
    for (path, label) in [
        (&profile, "LibreOffice profile"),
        (&cache, "renderer cache"),
        (&config, "renderer configuration"),
    ] {
        std::fs::create_dir(path).map_err(|error| {
            AdapterError::ContractViolation(format!(
                "failed to create private {purpose} {label}: {error}"
            ))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(
                |error| {
                    AdapterError::ContractViolation(format!(
                        "failed to restrict private {purpose} {label}: {error}"
                    ))
                },
            )?;
        }
    }
    Ok(profile)
}

// A bundle-shaped path is not authority. Only Apple's verifier may authorize
// the enclosing app as a runtime read root. Custom standalone renderers keep
// their existing executable-only access.
#[cfg(target_os = "macos")]
fn macos_office_bundle_candidate(program: &Path) -> Option<PathBuf> {
    if !program.is_absolute() || program.file_name()? != "soffice" {
        return None;
    }
    let macos = program.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    (macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle.extension()? == "app")
        .then(|| bundle.to_path_buf())
}

#[cfg(target_os = "macos")]
fn verify_macos_office_bundle(bundle: &Path) -> Result<()> {
    // -R= is the codesign inline requirement syntax; no shell or ambient
    // executable search is involved. Deep verification covers nested code,
    // and strict verification rejects unsealed/escaping symbolic links.
    const REQUIREMENT: &str = "-R=anchor apple generic and certificate leaf[subject.OU] = \"7P5S3ZLCN7\" and identifier \"org.libreoffice.script\"";
    let mut command = std::process::Command::new("/usr/bin/codesign");
    command
        .env_clear()
        .current_dir("/")
        .args([
            "--verify",
            "--deep",
            "--strict",
            "--all-architectures",
            REQUIREMENT,
        ])
        .arg(bundle);
    let output = kio_process::run_bounded_command(
        &mut command,
        BoundedProcessOptions {
            timeout: Duration::from_secs(60),
            max_stdout_bytes: MAX_OFFICE_LOG_BYTES,
            max_stderr_bytes: MAX_OFFICE_LOG_BYTES,
        },
        None,
    )
    .map_err(|_| {
        AdapterError::ContractViolation("Office bundle signature verification failed".to_owned())
    })?;
    if !output.status.success() {
        return Err(AdapterError::ContractViolation(
            "Office bundle is not a verified LibreOffice distribution".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
#[derive(PartialEq, Eq)]
struct OfficeBundlePathIdentity {
    device: u64,
    inode: u64,
    mode: u32,
    owner: u32,
    len: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

#[cfg(target_os = "macos")]
fn office_bundle_path_snapshot(
    bundle: &Path,
    program: &Path,
    source: &Path,
) -> Result<Vec<OfficeBundlePathIdentity>> {
    use std::os::unix::fs::MetadataExt;
    // Refuse aliases and escapes for every authority-bearing path. Retain
    // filesystem identity across verification and catalog parsing, including
    // ownership and permissions, to detect replacement during these checks.
    [
        bundle.to_path_buf(),
        bundle.join("Contents"),
        bundle.join("Contents/MacOS"),
        bundle.join("Contents/Resources"),
        bundle.join("Contents/Resources/services"),
        program.to_path_buf(),
        source.to_path_buf(),
    ]
    .iter()
    .map(|path| {
        let fail = || {
            AdapterError::ContractViolation("Office bundle path is unstable or unsafe".to_owned())
        };
        if std::fs::canonicalize(path).map_err(|_| fail())? != *path {
            return Err(fail());
        }
        let metadata = std::fs::symlink_metadata(path).map_err(|_| fail())?;
        if metadata.mode() & 0o022 != 0 {
            return Err(fail());
        }
        Ok(OfficeBundlePathIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.mode(),
            owner: metadata.uid(),
            len: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    })
    .collect()
}

#[cfg(target_os = "macos")]
fn private_uno_catalog_for_program(program: &Path) -> Result<Option<PrivateUnoCatalog>> {
    let Some(bundle) = macos_office_bundle_candidate(program) else {
        return Ok(None);
    };
    let source = bundle.join("Contents/Resources/services/services.rdb");
    let before = office_bundle_path_snapshot(&bundle, program, &source)?;
    verify_macos_office_bundle(&bundle)?;
    let bytes = read_stable_catalog(&source)?;
    let _ = macos_spell_component_range(&bytes)?;
    if before != office_bundle_path_snapshot(&bundle, program, &source)? {
        return Err(AdapterError::ContractViolation(
            "Office bundle changed during verification".to_owned(),
        ));
    }
    let digest = Sha256::digest(&bytes);
    Ok(Some(PrivateUnoCatalog {
        source,
        bundle,
        source_digest: hex_digest(&digest),
    }))
}

#[cfg(not(target_os = "macos"))]
fn private_uno_catalog_for_program(_program: &Path) -> Result<Option<PrivateUnoCatalog>> {
    Ok(None)
}

#[cfg(target_os = "macos")]
fn read_stable_catalog(path: &Path) -> Result<Vec<u8>> {
    const MAX_CATALOG_BYTES: usize = 1024 * 1024;
    kio_core::cas::read_bounded_regular_file(path, MAX_CATALOG_BYTES as u64).map_err(|error| {
        AdapterError::ContractViolation(format!(
            "cannot safely read Office service catalog: {error}"
        ))
    })
}

#[cfg(target_os = "macos")]
fn macos_spell_component_range(bytes: &[u8]) -> Result<(usize, usize)> {
    const IMPLEMENTATION: &[u8] = b"org.openoffice.lingu.MacOSXSpellChecker";
    const CONSTRUCTOR: &[u8] = b"lingucomponent_MacSpellChecker_get_implementation";
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().check_end_names = true;
    let mut buffer = Vec::new();
    let mut previous = 0_usize;
    let mut component: Option<(usize, usize, usize, bool)> = None;
    let mut match_range = None;
    let mut root_started = false;
    let mut root_closed = false;
    let mut root_depth = 0_usize;
    loop {
        let start = previous;
        let event = reader.read_event_into(&mut buffer).map_err(|error| {
            AdapterError::ContractViolation(format!(
                "Office service catalog XML is invalid: {error}"
            ))
        })?;
        previous = reader.buffer_position() as usize;
        match event {
            Event::DocType(_) => {
                return Err(AdapterError::ContractViolation(
                    "Office service catalog must not contain a DOCTYPE".to_owned(),
                ));
            }
            Event::Start(ref tag) | Event::Empty(ref tag) => {
                let empty = matches!(&event, Event::Empty(_));
                let tag_name = tag.name();
                let name = tag_name.as_ref();
                // `quick-xml` reports attribute syntax failures lazily. Walk
                // every element's attributes before interpreting its shape so
                // a malformed unrelated component cannot survive the filtered
                // copy unchanged.
                let mut declared_namespace = None;
                let mut implementation = None;
                let mut constructor = None;
                for attribute in tag.attributes().with_checks(true) {
                    let attribute = attribute.map_err(|error| {
                        AdapterError::ContractViolation(format!(
                            "Office service catalog attribute is invalid: {error}"
                        ))
                    })?;
                    match attribute.key.as_ref() {
                        b"xmlns" => {
                            declared_namespace = Some(
                                attribute.value.as_ref()
                                    == b"http://openoffice.org/2010/uno-components",
                            )
                        }
                        b"name" if name == b"implementation" => {
                            implementation = Some(attribute.value.as_ref() == IMPLEMENTATION)
                        }
                        b"constructor" if name == b"implementation" => {
                            constructor = Some(attribute.value.as_ref() == CONSTRUCTOR)
                        }
                        _ => {}
                    }
                }
                if !root_started {
                    if empty || name != b"components" || declared_namespace != Some(true) {
                        return Err(AdapterError::ContractViolation("Office service catalog must have one components root in the UNO namespace".to_owned()));
                    }
                    root_started = true;
                    root_depth = 1;
                    buffer.clear();
                    continue;
                }
                if root_closed {
                    return Err(AdapterError::ContractViolation(
                        "Office service catalog has content after its root element".to_owned(),
                    ));
                }
                root_depth += 1;
                if name == b"component" && root_depth != 2 {
                    return Err(AdapterError::ContractViolation(
                        "Office service catalog components must be direct children of the root"
                            .to_owned(),
                    ));
                }
                if (name == b"component" || name == b"implementation")
                    && declared_namespace == Some(false)
                {
                    return Err(AdapterError::ContractViolation(
                        "Office service catalog direct component namespace is invalid".to_owned(),
                    ));
                }
                if name == b"component" && component.is_none() {
                    component = Some((start, 1, 0, false));
                } else if let Some((_, depth, _, _)) = component.as_mut() {
                    *depth += 1;
                }
                if name == b"implementation" {
                    if component.as_ref().map(|(_, depth, _, _)| *depth) != Some(2) {
                        return Err(AdapterError::ContractViolation(
                            "Office service catalog implementations must be direct component children"
                                .to_owned(),
                        ));
                    }
                    if implementation == Some(true) {
                        if constructor != Some(true) || match_range.is_some() {
                            return Err(AdapterError::ContractViolation("Office service catalog spell checker is not the expected single implementation".to_owned()));
                        }
                        let Some((_, _, implementations, target)) = component.as_mut() else {
                            return Err(AdapterError::ContractViolation(
                                "Office spell checker is outside a component".to_owned(),
                            ));
                        };
                        *implementations += 1;
                        *target = true;
                    } else if let Some((_, _, implementations, _)) = component.as_mut() {
                        *implementations += 1;
                    }
                }
                if empty {
                    if let Some((component_start, depth, implementations, target)) =
                        component.as_mut()
                    {
                        *depth = depth.saturating_sub(1);
                        if *depth == 0 {
                            if *target {
                                if *implementations != 1 {
                                    return Err(AdapterError::ContractViolation("Office spell checker component contains additional implementations".to_owned()));
                                }
                                match_range = Some((*component_start, previous));
                            }
                            component = None;
                        }
                    }
                    root_depth = root_depth.saturating_sub(1);
                }
            }
            Event::End(tag) => {
                if !root_started || root_closed || root_depth == 0 {
                    return Err(AdapterError::ContractViolation(
                        "Office service catalog has an invalid closing element".to_owned(),
                    ));
                }
                if tag.name().as_ref() == b"component" {
                    if let Some((component_start, depth, implementations, target)) =
                        component.as_mut()
                    {
                        *depth = depth.saturating_sub(1);
                        if *depth == 0 {
                            if *target {
                                if *implementations != 1 {
                                    return Err(AdapterError::ContractViolation("Office spell checker component contains additional implementations".to_owned()));
                                }
                                match_range = Some((*component_start, previous));
                            }
                            component = None;
                        }
                    }
                } else if let Some((_, depth, _, _)) = component.as_mut() {
                    *depth = depth.saturating_sub(1);
                }
                root_depth -= 1;
                if root_depth == 0 {
                    if tag.name().as_ref() != b"components" || component.is_some() {
                        return Err(AdapterError::ContractViolation(
                            "Office service catalog has an invalid components root".to_owned(),
                        ));
                    }
                    root_closed = true;
                }
            }
            Event::Text(text) => {
                let raw: &[u8] = text.as_ref();
                if (!root_started || root_closed) && !raw.iter().all(u8::is_ascii_whitespace) {
                    return Err(AdapterError::ContractViolation(
                        "Office service catalog has non-whitespace text outside its root"
                            .to_owned(),
                    ));
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    if !root_closed || component.is_some() {
        return Err(AdapterError::ContractViolation(
            "Office service catalog is truncated or has no closed root".to_owned(),
        ));
    }
    match_range.ok_or_else(|| {
        AdapterError::ContractViolation(
            "Office service catalog has no expected macOS spell checker component".to_owned(),
        )
    })
}

fn write_private_uno_catalog(catalog: &PrivateUnoCatalog, scratch: &Path) -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let bytes = read_stable_catalog(&catalog.source)?;
        if hex_digest(&Sha256::digest(&bytes)) != catalog.source_digest {
            return Err(AdapterError::ContractViolation(
                "Office service catalog changed after converter probe".to_owned(),
            ));
        }
        let (start, end) = macos_spell_component_range(&bytes)?;
        let destination = scratch.join("services-without-macos-spell.rdb");
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)
            .map_err(|error| {
                AdapterError::ContractViolation(format!(
                    "cannot create private Office service catalog: {error}"
                ))
            })?;
        use std::io::Write as _;
        output
            .write_all(&bytes[..start])
            .and_then(|()| output.write_all(&bytes[end..]))
            .and_then(|()| output.sync_all())
            .map_err(|error| {
                AdapterError::ContractViolation(format!(
                    "cannot write private Office service catalog: {error}"
                ))
            })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o600))
                .map_err(|error| {
                    AdapterError::ContractViolation(format!(
                        "cannot restrict private Office service catalog: {error}"
                    ))
                })?;
        }
        Ok(destination)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (catalog, scratch);
        Err(AdapterError::ContractViolation(
            "private UNO catalogs are unsupported on this platform".to_owned(),
        ))
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A renderer receives no ambient credentials, proxy, loader, or application
/// configuration.  This is intentionally a narrow OS-runtime allowlist; the
/// private profile replaces `HOME` so LibreOffice cannot read user state.
fn renderer_environment(_program: &Path, home: Option<&Path>) -> Vec<(OsString, OsString)> {
    let mut environment = Vec::new();
    #[cfg(unix)]
    {
        let path = std::env::join_paths([Path::new("/usr/bin"), Path::new("/bin")])
            .expect("fixed Unix runtime paths contain no separators");
        environment.push((OsString::from("PATH"), path));
    }
    // Python imported by LibreOffice must not rewrite sealed app resources.
    #[cfg(target_os = "macos")]
    environment.push((
        OsString::from("PYTHONDONTWRITEBYTECODE"),
        OsString::from("1"),
    ));
    #[cfg(windows)]
    for name in ["SystemRoot", "WINDIR", "COMSPEC", "PATHEXT"] {
        if let Some(value) = std::env::var_os(name) {
            environment.push((OsString::from(name), value));
        }
    }
    #[cfg(windows)]
    {
        let mut paths = Vec::new();
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            let root = PathBuf::from(system_root);
            paths.push(root.join("System32"));
            paths.push(root);
        }
        if let Ok(path) = std::env::join_paths(paths) {
            environment.push((OsString::from("PATH"), path));
        }
    }
    if let Some(home) = home {
        environment.push((OsString::from("HOME"), home.as_os_str().to_owned()));
        #[cfg(unix)]
        {
            environment.push((OsString::from("TMPDIR"), home.as_os_str().to_owned()));
            if let Some(scratch) = home.parent() {
                environment.push((
                    OsString::from("OSL_SOCKET_PATH"),
                    scratch.as_os_str().to_owned(),
                ));
                environment.push((
                    OsString::from("XDG_CACHE_HOME"),
                    scratch.join("cache").into_os_string(),
                ));
                environment.push((
                    OsString::from("XDG_CONFIG_HOME"),
                    scratch.join("config").into_os_string(),
                ));
            }
        }
        #[cfg(windows)]
        environment.push((OsString::from("USERPROFILE"), home.as_os_str().to_owned()));
    }
    environment
}

// TDF's system DEBs install outside /usr. This exact root-owned layout is
// the only additional Linux package authority; a caller's executable path
// alone still grants no access to sibling files.
#[cfg(any(target_os = "linux", all(test, target_os = "macos")))]
fn linux_tdf_bundle_candidate(program: &Path) -> Option<PathBuf> {
    let version = program
        .to_str()?
        .strip_prefix("/opt/libreoffice")?
        .strip_suffix("/program/soffice")?;
    let (major, minor) = version.split_once('.')?;
    if ![major, minor]
        .iter()
        .all(|part| (1..=4).contains(&part.len()) && part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return None;
    }
    Some(PathBuf::from(format!("/opt/libreoffice{version}")))
}

#[cfg(any(target_os = "linux", all(test, target_os = "macos")))]
fn trusted_linux_package_mode(owner: u32, mode: u32, directory: bool) -> bool {
    owner == 0 && mode & 0o022 == 0 && (directory || mode & 0o6000 == 0)
}

#[cfg(any(target_os = "linux", all(test, target_os = "macos")))]
fn verify_linux_package_path(path: &Path, directory: bool) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let fail = || {
        AdapterError::ContractViolation(
            "Linux Office package path is unsafe or unavailable".to_owned(),
        )
    };
    let metadata = std::fs::symlink_metadata(path).map_err(|_| fail())?;
    if (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
        || !trusted_linux_package_mode(metadata.uid(), metadata.mode(), directory)
        || std::fs::canonicalize(path).map_err(|_| fail())?.as_os_str() != path.as_os_str()
    {
        return Err(fail());
    }
    Ok(())
}

#[cfg(any(target_os = "linux", all(test, target_os = "macos")))]
fn linux_tdf_runtime_roots(program: &Path) -> Result<Vec<PathBuf>> {
    let Some(bundle) = linux_tdf_bundle_candidate(program) else {
        return Ok(Vec::new());
    };
    let program_directory = bundle.join("program");
    let share_directory = bundle.join("share");
    for directory in [
        Path::new("/"),
        Path::new("/opt"),
        &bundle,
        &program_directory,
        &share_directory,
    ] {
        verify_linux_package_path(directory, true)?;
    }
    verify_linux_package_path(program, false)?;
    Ok(vec![program_directory, share_directory])
}

fn renderer_runtime_roots(
    _program: &Path,
    _catalog: Option<&PrivateUnoCatalog>,
) -> Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    #[cfg(target_os = "macos")]
    for root in [
        "/System",
        "/bin",
        "/usr/bin",
        "/usr/lib",
        "/usr/share",
        "/Library/Fonts",
        "/System/Library/Fonts",
        "/private/var/db/timezone",
    ] {
        if Path::new(root).exists() {
            roots.push(PathBuf::from(root));
        }
    }
    #[cfg(target_os = "macos")]
    if let Some(catalog) = _catalog {
        roots.push(catalog.bundle.clone());
    }
    #[cfg(target_os = "linux")]
    {
        roots.extend(linux_tdf_runtime_roots(_program)?);
        let package_program = Path::new("/usr/lib/libreoffice/program/soffice");
        if std::fs::canonicalize(package_program).ok().as_deref() == Some(_program) {
            let share = PathBuf::from("/usr/lib/libreoffice/share");
            if !share.is_dir() {
                return Err(AdapterError::ContractViolation(
                    "recognized Linux LibreOffice package is missing its share runtime".to_owned(),
                ));
            }
            roots.push(share);
        }
    }
    #[cfg(target_os = "linux")]
    for root in [
        "/bin",
        "/usr/bin",
        "/usr/lib",
        "/usr/share",
        "/lib",
        "/lib64",
        "/etc/fonts",
    ] {
        if Path::new(root).exists() {
            roots.push(PathBuf::from(root));
        }
    }
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        roots.push(PathBuf::from(root));
    }
    Ok(roots)
}

fn file_url(path: &Path) -> Result<String> {
    let text = path.to_str().ok_or_else(|| {
        AdapterError::ContractViolation("office scratch path is not valid Unicode".to_owned())
    })?;
    #[cfg(windows)]
    let path = text.replace('\\', "/");
    #[cfg(not(windows))]
    let path = text.to_owned();
    let mut encoded = String::new();
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b':' | b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    #[cfg(windows)]
    return Ok(format!("file:///{encoded}"));
    #[cfg(not(windows))]
    Ok(format!("file://{encoded}"))
}

/// Run the real renderer: stage `input` under the correct extension, convert
/// via `--headless --convert-to pdf --outdir`, and read back the produced
/// PDF. `-env:UserInstallation` points at a private, per-invocation profile
/// dir — LibreOffice refuses to start a second instance against a shared
/// profile (which would otherwise serialize or deadlock concurrent prepare
/// calls); verified against the real `/opt/homebrew/bin/soffice` 26.2.4.2 on
/// the implementing machine. Any failure (spawn, non-zero exit, missing or
/// non-PDF output) is `AdapterError::ContractViolation`.
fn convert_with_real_binary(
    program: &Path,
    uno_catalog: Option<&PrivateUnoCatalog>,
    input: &[u8],
    media_type: &str,
) -> Result<Vec<u8>> {
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
    let validated_catalog = private_uno_catalog_for_program(program)?;
    if validated_catalog != uno_catalog.cloned() {
        return Err(AdapterError::ContractViolation(
            "Office runtime changed after converter probe".to_owned(),
        ));
    }
    let scratch = private_temp_dir()?;
    let workdir = scratch.path();

    let input_path = workdir.join(format!("input.{extension}"));
    std::fs::write(&input_path, input).map_err(|err| {
        AdapterError::ContractViolation(format!(
            "failed to stage office input at {}: {err}",
            input_path.display()
        ))
    })?;

    let outdir = workdir.join("out");
    std::fs::create_dir(&outdir).map_err(|err| {
        AdapterError::ContractViolation(format!(
            "failed to create office-convert output dir at {}: {err}",
            outdir.display()
        ))
    })?;
    // Retain this directory before launching the renderer. It is the only
    // namespace used to recover output: the renderer can replace path names
    // underneath it, but cannot redirect a descriptor-relative no-follow
    // read to a file outside this retained directory.
    let output_directory = kio_core::store_dir::StoreDirectory::from_retained(
        std::fs::File::open(&outdir).map_err(|err| {
            AdapterError::ContractViolation(format!(
                "failed to retain office-convert output dir at {}: {err}",
                outdir.display()
            ))
        })?,
        outdir.clone(),
    )
    .map_err(|err| {
        AdapterError::ContractViolation(format!(
            "failed to validate office-convert output dir at {}: {err}",
            outdir.display()
        ))
    })?;
    let profile_dir = create_private_renderer_state(workdir, "conversion")?;

    let profile_url = file_url(&profile_dir)?;
    let sandbox = RenderSandbox::new(
        program,
        workdir,
        renderer_runtime_roots(program, validated_catalog.as_ref())?,
        RenderResourceLimits::default(),
    )
    .map_err(|err| {
        AdapterError::ContractViolation(format!(
            "office converter confinement is unavailable: {err}"
        ))
    })?;
    let arguments = vec![
        OsString::from("--headless"),
        OsString::from(format!("-env:UserInstallation={profile_url}")),
        OsString::from("--convert-to"),
        OsString::from("pdf"),
        OsString::from("--outdir"),
        outdir.as_os_str().to_owned(),
        input_path.as_os_str().to_owned(),
    ];
    let mut environment = renderer_environment(program, Some(&profile_dir));
    #[cfg(test)]
    if native_acceptance_diagnostics_enabled() {
        // This is a fixed diagnostic switch for checked-in synthetic OOXML
        // fixtures only. It never accepts ambient log configuration.
        environment.push((
            OsString::from("SAL_LOG"),
            OsString::from("+WARN+INFO.sal.osl.pipe"),
        ));
    }
    if let Some(catalog) = uno_catalog {
        let catalog = write_private_uno_catalog(catalog, workdir)?;
        environment.push((
            OsString::from("URE_MORE_SERVICES"),
            file_url(&catalog)?.into(),
        ));
    }
    let output = sandbox
        .run(
            arguments,
            &environment,
            BoundedProcessOptions {
                timeout: OFFICE_CONVERT_TIMEOUT,
                max_stdout_bytes: MAX_OFFICE_LOG_BYTES,
                max_stderr_bytes: MAX_OFFICE_LOG_BYTES,
            },
        )
        .map_err(|err| {
            AdapterError::ContractViolation(format!(
                "office converter {} failed within confinement: {err}",
                program.display()
            ))
        })?;
    if !output.status.success() {
        #[cfg(test)]
        if native_acceptance_diagnostics_enabled() {
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

    let pdf_bytes = output_directory
        .read_optional(Path::new("input.pdf"), MAX_OFFICE_PDF_BYTES as u64)
        .map_err(|err| {
            AdapterError::ContractViolation(format!(
                "office converter output is absent, unsafe, or exceeded the {} byte limit: {err}",
                MAX_OFFICE_PDF_BYTES
            ))
        })?
        .ok_or_else(|| {
            AdapterError::ContractViolation("office converter did not produce input.pdf".to_owned())
        })?;
    if !pdf_bytes.starts_with(b"%PDF") {
        return Err(AdapterError::ContractViolation(
            "office converter output is not a PDF (missing %PDF magic)".to_owned(),
        ));
    }
    Ok(pdf_bytes)
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
/// `deterministic.rs`. An unterminated literal (malformed input) stops the
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
/// `deterministic.rs`.
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
    use base64::Engine;

    #[cfg(unix)]
    fn executable_script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).expect("write renderer fixture");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .expect("mark renderer fixture executable");
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
    fn homebrew_name_does_not_substitute_an_unrecognized_wrapper() {
        assert!(is_macos_office_passthrough_wrapper(
            b"#!/bin/bash\nexec \"/Applications/LibreOffice.app/Contents/MacOS/soffice\"  \"$@\"\n"
        ));
        assert!(!is_macos_office_passthrough_wrapper(
            b"#!/bin/bash\nexec \"/Applications/OtherOffice.app/Contents/MacOS/soffice\"  \"$@\"\n"
        ));
        assert!(!is_macos_office_passthrough_wrapper(
            b"#!/bin/bash\nexec \"/Applications/LibreOffice.app/Contents/MacOS/soffice\" --changed \"$@\"\n"
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_uno_catalog_requires_well_formed_direct_uno_components() {
        let valid = macos_catalog(macos_spell_component());
        assert!(macos_spell_component_range(&valid).is_ok());

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
        let converter = resolve_office_converter().expect("seam converter resolves");
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
        let converter = resolve_office_converter().expect("seam must win over explicit");
        assert_eq!(converter.version(), "test-converter");
    }

    #[test]
    fn no_converter_available_resolves_to_none() {
        let _lock = kio_core::test_control::test_env_lock().lock().unwrap();
        let _clear_seam = kio_core::test_control::TestEnvGuard::remove(TEST_OFFICE_CONVERT_ENV);
        let _clear_explicit = kio_core::test_control::TestEnvGuard::remove(OFFICE_CONVERTER_ENV);
        let _scrub_path =
            kio_core::test_control::TestEnvGuard::set("PATH", "/nonexistent-kio-test-path");
        assert!(resolve_office_converter().is_none());
    }

    #[test]
    fn explicit_converter_pointing_at_missing_binary_resolves_to_none() {
        let _lock = kio_core::test_control::test_env_lock().lock().unwrap();
        let _clear_seam = kio_core::test_control::TestEnvGuard::remove(TEST_OFFICE_CONVERT_ENV);
        let _explicit = kio_core::test_control::TestEnvGuard::set(
            OFFICE_CONVERTER_ENV,
            "/definitely/not/a/real/kio-office-converter-binary",
        );
        assert!(
            resolve_office_converter().is_none(),
            "a probe failure on an explicit override must resolve to None, not fall through to PATH"
        );
    }

    #[cfg(unix)]
    #[test]
    fn real_renderer_does_not_inherit_secret_environment_and_handles_special_paths() {
        let _lock = kio_core::test_control::test_env_lock().lock().unwrap();
        let _clear_seam = kio_core::test_control::TestEnvGuard::remove(TEST_OFFICE_CONVERT_ENV);
        let directory = tempfile::Builder::new()
            .prefix("kio office ; $ ")
            .tempdir()
            .expect("fixture directory");
        let script = directory.path().join("renderer with spaces");
        executable_script(
            &script,
            "#!/bin/sh\n\
             if [ \"$KIO_CONVERTER_SECRET\" = leak ]; then exit 91; fi\n\
             if [ \"$1\" = --version ] || [ \"$4\" = --version ]; then printf 'fixture renderer 1\\n'; exit 0; fi\n\
             out=\n\
             while [ \"$#\" -gt 0 ]; do\n\
               if [ \"$1\" = --outdir ]; then shift; out=$1; fi\n\
               shift\n\
             done\n\
             printf '%%PDF-1.4\\nfixture\\n%%%%EOF\\n' > \"$out/input.pdf\"\n",
        );
        let _secret = kio_core::test_control::TestEnvGuard::set("KIO_CONVERTER_SECRET", "leak");
        let _explicit =
            kio_core::test_control::TestEnvGuard::set(OFFICE_CONVERTER_ENV, script.as_os_str());
        let converter = probe_real_converter_diagnostic(script.clone())
            .expect("fixture renderer probes with a secret-free bounded environment");
        let pdf = converter
            .convert_to_pdf(&decode_fixture(DOCX_FIXTURE_B64), DOCX_MEDIA_TYPE)
            .expect("secret must not be inherited and special paths must work");
        assert!(pdf.starts_with(b"%PDF"));
    }

    #[cfg(unix)]
    #[test]
    fn real_renderer_rejects_executable_replacement_after_probe() {
        let _lock = kio_core::test_control::test_env_lock().lock().unwrap();
        let _clear_seam = kio_core::test_control::TestEnvGuard::remove(TEST_OFFICE_CONVERT_ENV);
        let directory = tempfile::tempdir().expect("fixture directory");
        let script = directory.path().join("renderer");
        executable_script(&script, "#!/bin/sh\nprintf 'fixture renderer 1\\n'\n");
        let _explicit =
            kio_core::test_control::TestEnvGuard::set(OFFICE_CONVERTER_ENV, script.as_os_str());
        let converter = resolve_office_converter().expect("fixture renderer probes");

        executable_script(&script, "#!/bin/sh\nprintf 'replacement renderer 2\\n'\n");
        let error = converter
            .convert_to_pdf(&decode_fixture(DOCX_FIXTURE_B64), DOCX_MEDIA_TYPE)
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
        let _clear_seam = kio_core::test_control::TestEnvGuard::remove(TEST_OFFICE_CONVERT_ENV);
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
             if (grep { $_ eq '--version' } @ARGV) { print qq(fixture renderer 1\\n); exit 0; }\n\
             my $out; for (my $i=0; $i < @ARGV-1; $i++) { $out=$ARGV[$i+1] if $ARGV[$i] eq '--outdir'; }\n\
             defined($out) or die 'no output directory';\n\
             symlink(qq($0-private.pdf), qq($out/input.pdf)) or die qq(symlink: $!);\n",
        );
        let _explicit =
            kio_core::test_control::TestEnvGuard::set(OFFICE_CONVERTER_ENV, script.as_os_str());
        let converter = resolve_office_converter().expect("fixture renderer probes");
        let error = converter
            .convert_to_pdf(&decode_fixture(DOCX_FIXTURE_B64), DOCX_MEDIA_TYPE)
            .expect_err("a renderer-controlled output symlink must be rejected");
        let message = error.to_string();
        assert!(message.contains("unsafe") || message.contains("absent"));
        assert!(!message.contains("private fixture bytes"));
    }

    #[cfg(unix)]
    #[test]
    fn renderer_cannot_redirect_replaced_output_directory() {
        let _lock = kio_core::test_control::test_env_lock().lock().unwrap();
        let _clear_seam = kio_core::test_control::TestEnvGuard::remove(TEST_OFFICE_CONVERT_ENV);
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
             if (grep { $_ eq '--version' } @ARGV) { print qq(fixture renderer 1\\n); exit 0; }\n\
             my $out; for (my $i=0; $i < @ARGV-1; $i++) { $out=$ARGV[$i+1] if $ARGV[$i] eq '--outdir'; }\n\
             defined($out) or die 'no output directory';\n\
             rmdir($out) or die qq(rmdir: $!);\n\
             symlink(qq($0-private-dir), $out) or die qq(symlink: $!);\n",
        );
        let _explicit =
            kio_core::test_control::TestEnvGuard::set(OFFICE_CONVERTER_ENV, script.as_os_str());
        let converter = resolve_office_converter().expect("fixture renderer probes");
        let error = converter
            .convert_to_pdf(&decode_fixture(DOCX_FIXTURE_B64), DOCX_MEDIA_TYPE)
            .expect_err("a replaced output directory must not redirect the retained read");
        let message = error.to_string();
        assert!(
            message.contains("did not produce")
                || message.contains("absent")
                || message.contains("unsafe"),
            "unexpected safe renderer rejection: {message}"
        );
        assert!(!message.contains("private fixture bytes"));
    }

    #[test]
    fn file_url_percent_encodes_reserved_characters() {
        let path = Path::new("/private/kio office#?%.profile");
        let url = file_url(path).expect("Unicode fixture path");
        #[cfg(not(windows))]
        assert_eq!(url, "file:///private/kio%20office%23%3F%25.profile");
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

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn linux_tdf_layout_is_exact_and_version_components_are_bounded() {
        assert_eq!(
            linux_tdf_bundle_candidate(Path::new("/opt/libreoffice26.2/program/soffice")),
            Some(PathBuf::from("/opt/libreoffice26.2"))
        );
        for path in [
            "/opt/libreoffice/program/soffice",
            "/opt/libreoffice26/program/soffice",
            "/opt/libreoffice26.2.5/program/soffice",
            "/opt/libreoffice26./program/soffice",
            "/opt/libreoffice.2/program/soffice",
            "/opt/libreoffice12345.2/program/soffice",
            "/opt/libreoffice26.beta/program/soffice",
            "/home/user/libreoffice26.2/program/soffice",
            "/opt/libreoffice26.2//program/soffice",
            "/opt/libreoffice26.2/program/../program/soffice",
            "/opt/libreoffice26.2/program/custom",
            "/opt/libreoffice26.2/sibling/soffice",
        ] {
            assert!(
                linux_tdf_bundle_candidate(Path::new(path)).is_none(),
                "{path}"
            );
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn linux_package_authority_requires_root_and_rejects_unsafe_modes() {
        assert!(trusted_linux_package_mode(0, 0o100755, false));
        assert!(trusted_linux_package_mode(0, 0o40755, true));
        for (owner, mode) in [
            (501, 0o100755),
            (0, 0o100775),
            (0, 0o100757),
            (0, 0o104755),
            (0, 0o102755),
        ] {
            assert!(!trusted_linux_package_mode(owner, mode, false));
        }
        assert!(!trusted_linux_package_mode(0, 0o40777, true));
        assert!(
            linux_tdf_runtime_roots(Path::new("/tmp/custom/soffice"))
                .unwrap()
                .is_empty()
        );
        assert!(verify_linux_package_path(Path::new("/"), true).is_ok());
        let fixture = tempfile::tempdir().unwrap();
        let alias = fixture.path().join("alias");
        std::os::unix::fs::symlink("/", &alias).unwrap();
        assert!(verify_linux_package_path(&alias, true).is_err());
        assert!(verify_linux_package_path(&fixture.path().join("missing"), false).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_office_requires_supported_headless_release() {
        let old = validate_macos_office_version(
            "LibreOffice 26.2.5.2 cd7284b4cbbfeb507e630c1aac019f4157393acb\n\n",
        )
        .unwrap_err();
        assert!(
            old.to_string()
                .contains("requires LibreOffice 26.8 or newer")
        );
        for version in [
            "LibreOffice 26.8.0.3",
            "LibreOffice 26.8.0\n\n",
            "LibreOffice 26.8.0.3 cd7284b4cbbfeb507e630c1aac019f4157393acb\n",
            "LibreOffice 27.2.0.1",
        ] {
            assert!(validate_macos_office_version(version).is_ok(), "{version}");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_office_rejects_malformed_or_ambiguous_version() {
        for version in [
            "",
            "26.8.0.3",
            "LibreOffice 26.8",
            "LibreOffice 26.8.0.3.1",
            "LibreOffice 026.8.0.3",
            "LibreOffice 26.08.0.3",
            "LibreOffice 26.8..3",
            "LibreOffice 26.8.0-beta",
            "LibreOffice +26.8.0.3",
            "LibreOffice 99999.8.0.3",
            "LibreOffice 26.8.0.3 forged",
            "LibreOffice 26.8.0.3 extra tokens",
            "LibreOffice 26.8.0.3\nLibreOffice 26.2.5.2",
            "LibreOffice\n26.8.0.3",
            "LibreOffice 26.8.0.3\0",
        ] {
            let error = validate_macos_office_version(version).unwrap_err();
            assert!(
                error.to_string().contains("malformed or ambiguous"),
                "{version}"
            );
        }
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
            macos_office_bundle_candidate(&app.join("Contents/MacOS/soffice")),
            Some(app.to_path_buf())
        );
        for path in [
            "relative.app/Contents/MacOS/soffice",
            "/private/LibreOffice/Contents/MacOS/soffice",
            "/private/LibreOffice.app/MacOS/soffice",
            "/private/LibreOffice.app/Contents/MacOS/other",
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
        let (_directory, bundle, program, _source) = fake_office_bundle();
        std::fs::write(
            bundle.join("Contents/Info.plist"),
            br#"<?xml version="1.0"?><plist version="1.0"><dict>
            <key>CFBundleIdentifier</key><string>org.libreoffice.script</string>
            <key>TeamIdentifier</key><string>7P5S3ZLCN7</string>
            <key>CFBundleExecutable</key><string>soffice</string></dict></plist>"#,
        )
        .unwrap();
        assert!(verify_macos_office_bundle(&bundle).is_err());
        assert!(private_uno_catalog_for_program(&program).is_err());
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
        let scratch = tempfile::tempdir().unwrap();
        let filtered = write_private_uno_catalog(&catalog, scratch.path()).unwrap();
        assert!(
            !std::fs::read_to_string(filtered)
                .unwrap()
                .contains("MacOSXSpellChecker")
        );
        std::fs::write(&source, b"<malformed").unwrap();
        let next_scratch = tempfile::tempdir().unwrap();
        assert!(write_private_uno_catalog(&catalog, next_scratch.path()).is_err());
        let malformed_catalog = PrivateUnoCatalog {
            source_digest: hex_digest(&Sha256::digest(b"<malformed")),
            ..catalog
        };
        assert!(write_private_uno_catalog(&malformed_catalog, next_scratch.path()).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_runtime_roots_admit_only_the_validated_bundle() {
        let (_directory, bundle, program, source) = fake_office_bundle();
        // This constructs the post-verification value only to test root
        // selection; production can obtain it only through codesign above.
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

    // ---- real soffice integration (env-gated) ------------------------------
    //
    // Helper script used ONCE to produce the embedded *_FIXTURE_B64 constants
    // below (python3 stdlib only — `zip` is not a dependency anywhere in this
    // workspace's Cargo.toml files, confirmed before writing this). Fixed ZIP
    // entry timestamps (1980-01-01) make the script's OWN output reproducible
    // run to run. DOCX:
    //
    // ```python
    // import zipfile, base64
    // def w(zf, name, data):
    //     info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
    //     info.compress_type = zipfile.ZIP_STORED
    //     zf.writestr(info, data)
    // CONTENT_TYPES = b'''<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
    // <Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
    // <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
    // <Default Extension="xml" ContentType="application/xml"/>
    // <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
    // </Types>'''
    // RELS = b'''<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
    // <Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
    // <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
    // </Relationships>'''
    // DOCUMENT = b'''<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
    // <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
    // <w:body><w:p><w:r><w:t>Kio office convert test</w:t></w:r></w:p></w:body>
    // </w:document>'''
    // with zipfile.ZipFile("minimal.docx", "w", zipfile.ZIP_STORED) as zf:
    //     w(zf, "[Content_Types].xml", CONTENT_TYPES)
    //     w(zf, "_rels/.rels", RELS)
    //     w(zf, "word/document.xml", DOCUMENT)
    // print(base64.b64encode(open("minimal.docx", "rb").read()).decode())
    // ```
    //
    // PPTX needs the fuller OOXML presentation skeleton (presentation.xml +
    // one slide + one slideLayout + one slideMaster + one theme, each with
    // its own `_rels`) — same `w()` helper, additional parts:
    //
    // ```python
    // PRESENTATION = b'''<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
    // <p:presentation xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
    // <p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst>
    // <p:sldIdLst><p:sldId id="256" r:id="rId2"/></p:sldIdLst>
    // <p:sldSz cx="9144000" cy="6858000"/><p:notesSz cx="6858000" cy="9144000"/>
    // </p:presentation>'''
    // # ... slide1.xml / slideLayout1.xml / slideMaster1.xml / theme1.xml and
    // # their _rels, and [Content_Types].xml Overrides for all five parts —
    // # full text lives in this crate's git history / task notes for
    // # office_convert.rs; omitted here for brevity.
    // ```

    fn decode_fixture(b64: &str) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .expect("embedded fixture base64 decodes")
    }

    const DOCX_FIXTURE_B64: &str = "UEsDBBQAAAAAAAAAIQDMg9OxswEAALMBAAATAAAAW0NvbnRlbnRfVHlwZXNdLnhtbDw/eG1sIHZlcnNpb249IjEuMCIgZW5jb2Rpbmc9IlVURi04IiBzdGFuZGFsb25lPSJ5ZXMiPz4KPFR5cGVzIHhtbG5zPSJodHRwOi8vc2NoZW1hcy5vcGVueG1sZm9ybWF0cy5vcmcvcGFja2FnZS8yMDA2L2NvbnRlbnQtdHlwZXMiPgo8RGVmYXVsdCBFeHRlbnNpb249InJlbHMiIENvbnRlbnRUeXBlPSJhcHBsaWNhdGlvbi92bmQub3BlbnhtbGZvcm1hdHMtcGFja2FnZS5yZWxhdGlvbnNoaXBzK3htbCIvPgo8RGVmYXVsdCBFeHRlbnNpb249InhtbCIgQ29udGVudFR5cGU9ImFwcGxpY2F0aW9uL3htbCIvPgo8T3ZlcnJpZGUgUGFydE5hbWU9Ii93b3JkL2RvY3VtZW50LnhtbCIgQ29udGVudFR5cGU9ImFwcGxpY2F0aW9uL3ZuZC5vcGVueG1sZm9ybWF0cy1vZmZpY2Vkb2N1bWVudC53b3JkcHJvY2Vzc2luZ21sLmRvY3VtZW50Lm1haW4reG1sIi8+CjwvVHlwZXM+ClBLAwQUAAAAAAAAACEAA9VLhC0BAAAtAQAACwAAAF9yZWxzLy5yZWxzPD94bWwgdmVyc2lvbj0iMS4wIiBlbmNvZGluZz0iVVRGLTgiIHN0YW5kYWxvbmU9InllcyI/Pgo8UmVsYXRpb25zaGlwcyB4bWxucz0iaHR0cDovL3NjaGVtYXMub3BlbnhtbGZvcm1hdHMub3JnL3BhY2thZ2UvMjAwNi9yZWxhdGlvbnNoaXBzIj4KPFJlbGF0aW9uc2hpcCBJZD0icklkMSIgVHlwZT0iaHR0cDovL3NjaGVtYXMub3BlbnhtbGZvcm1hdHMub3JnL29mZmljZURvY3VtZW50LzIwMDYvcmVsYXRpb25zaGlwcy9vZmZpY2VEb2N1bWVudCIgVGFyZ2V0PSJ3b3JkL2RvY3VtZW50LnhtbCIvPgo8L1JlbGF0aW9uc2hpcHM+ClBLAwQUAAAAAAAAACEAjUhHZOYAAADmAAAAEQAAAHdvcmQvZG9jdW1lbnQueG1sPD94bWwgdmVyc2lvbj0iMS4wIiBlbmNvZGluZz0iVVRGLTgiIHN0YW5kYWxvbmU9InllcyI/Pgo8dzpkb2N1bWVudCB4bWxuczp3PSJodHRwOi8vc2NoZW1hcy5vcGVueG1sZm9ybWF0cy5vcmcvd29yZHByb2Nlc3NpbmdtbC8yMDA2L21haW4iPgo8dzpib2R5Pgo8dzpwPjx3OnI+PHc6dD5LQ1Mgb2ZmaWNlIGNvbnZlcnQgdGVzdDwvdzp0PjwvdzpyPjwvdzpwPgo8L3c6Ym9keT4KPC93OmRvY3VtZW50PgpQSwECFAMUAAAAAAAAACEAzIPTsbMBAACzAQAAEwAAAAAAAAAAAAAAgAEAAAAAW0NvbnRlbnRfVHlwZXNdLnhtbFBLAQIUAxQAAAAAAAAAIQAD1UuELQEAAC0BAAALAAAAAAAAAAAAAACAAeQBAABfcmVscy8ucmVsc1BLAQIUAxQAAAAAAAAAIQCNSEdk5gAAAOYAAAARAAAAAAAAAAAAAACAAToDAAB3b3JkL2RvY3VtZW50LnhtbFBLBQYAAAAAAwADALkAAABPBAAAAAA=";
    const PPTX_FIXTURE_B64: &str = "UEsDBBQAAAAAAAAAIQCMwBYR2AMAANgDAAATAAAAW0NvbnRlbnRfVHlwZXNdLnhtbDw/eG1sIHZlcnNpb249IjEuMCIgZW5jb2Rpbmc9IlVURi04IiBzdGFuZGFsb25lPSJ5ZXMiPz4KPFR5cGVzIHhtbG5zPSJodHRwOi8vc2NoZW1hcy5vcGVueG1sZm9ybWF0cy5vcmcvcGFja2FnZS8yMDA2L2NvbnRlbnQtdHlwZXMiPgo8RGVmYXVsdCBFeHRlbnNpb249InJlbHMiIENvbnRlbnRUeXBlPSJhcHBsaWNhdGlvbi92bmQub3BlbnhtbGZvcm1hdHMtcGFja2FnZS5yZWxhdGlvbnNoaXBzK3htbCIvPgo8RGVmYXVsdCBFeHRlbnNpb249InhtbCIgQ29udGVudFR5cGU9ImFwcGxpY2F0aW9uL3htbCIvPgo8T3ZlcnJpZGUgUGFydE5hbWU9Ii9wcHQvcHJlc2VudGF0aW9uLnhtbCIgQ29udGVudFR5cGU9ImFwcGxpY2F0aW9uL3ZuZC5vcGVueG1sZm9ybWF0cy1vZmZpY2Vkb2N1bWVudC5wcmVzZW50YXRpb25tbC5wcmVzZW50YXRpb24ubWFpbit4bWwiLz4KPE92ZXJyaWRlIFBhcnROYW1lPSIvcHB0L3NsaWRlcy9zbGlkZTEueG1sIiBDb250ZW50VHlwZT0iYXBwbGljYXRpb24vdm5kLm9wZW54bWxmb3JtYXRzLW9mZmljZWRvY3VtZW50LnByZXNlbnRhdGlvbm1sLnNsaWRlK3htbCIvPgo8T3ZlcnJpZGUgUGFydE5hbWU9Ii9wcHQvc2xpZGVMYXlvdXRzL3NsaWRlTGF5b3V0MS54bWwiIENvbnRlbnRUeXBlPSJhcHBsaWNhdGlvbi92bmQub3BlbnhtbGZvcm1hdHMtb2ZmaWNlZG9jdW1lbnQucHJlc2VudGF0aW9ubWwuc2xpZGVMYXlvdXQreG1sIi8+CjxPdmVycmlkZSBQYXJ0TmFtZT0iL3BwdC9zbGlkZU1hc3RlcnMvc2xpZGVNYXN0ZXIxLnhtbCIgQ29udGVudFR5cGU9ImFwcGxpY2F0aW9uL3ZuZC5vcGVueG1sZm9ybWF0cy1vZmZpY2Vkb2N1bWVudC5wcmVzZW50YXRpb25tbC5zbGlkZU1hc3Rlcit4bWwiLz4KPE92ZXJyaWRlIFBhcnROYW1lPSIvcHB0L3RoZW1lL3RoZW1lMS54bWwiIENvbnRlbnRUeXBlPSJhcHBsaWNhdGlvbi92bmQub3BlbnhtbGZvcm1hdHMtb2ZmaWNlZG9jdW1lbnQudGhlbWUreG1sIi8+CjwvVHlwZXM+ClBLAwQUAAAAAAAAACEACaoHxzABAAAwAQAACwAAAF9yZWxzLy5yZWxzPD94bWwgdmVyc2lvbj0iMS4wIiBlbmNvZGluZz0iVVRGLTgiIHN0YW5kYWxvbmU9InllcyI/Pgo8UmVsYXRpb25zaGlwcyB4bWxucz0iaHR0cDovL3NjaGVtYXMub3BlbnhtbGZvcm1hdHMub3JnL3BhY2thZ2UvMjAwNi9yZWxhdGlvbnNoaXBzIj4KPFJlbGF0aW9uc2hpcCBJZD0icklkMSIgVHlwZT0iaHR0cDovL3NjaGVtYXMub3BlbnhtbGZvcm1hdHMub3JnL29mZmljZURvY3VtZW50LzIwMDYvcmVsYXRpb25zaGlwcy9vZmZpY2VEb2N1bWVudCIgVGFyZ2V0PSJwcHQvcHJlc2VudGF0aW9uLnhtbCIvPgo8L1JlbGF0aW9uc2hpcHM+ClBLAwQUAAAAAAAAACEATomFuAUCAAAFAgAAFAAAAHBwdC9wcmVzZW50YXRpb24ueG1sPD94bWwgdmVyc2lvbj0iMS4wIiBlbmNvZGluZz0iVVRGLTgiIHN0YW5kYWxvbmU9InllcyI/Pgo8cDpwcmVzZW50YXRpb24geG1sbnM6YT0iaHR0cDovL3NjaGVtYXMub3BlbnhtbGZvcm1hdHMub3JnL2RyYXdpbmdtbC8yMDA2L21haW4iIHhtbG5zOnI9Imh0dHA6Ly9zY2hlbWFzLm9wZW54bWxmb3JtYXRzLm9yZy9vZmZpY2VEb2N1bWVudC8yMDA2L3JlbGF0aW9uc2hpcHMiIHhtbG5zOnA9Imh0dHA6Ly9zY2hlbWFzLm9wZW54bWxmb3JtYXRzLm9yZy9wcmVzZW50YXRpb25tbC8yMDA2L21haW4iPgo8cDpzbGRNYXN0ZXJJZExzdD48cDpzbGRNYXN0ZXJJZCBpZD0iMjE0NzQ4MzY0OCIgcjppZD0icklkMSIvPjwvcDpzbGRNYXN0ZXJJZExzdD4KPHA6c2xkSWRMc3Q+PHA6c2xkSWQgaWQ9IjI1NiIgcjppZD0icklkMiIvPjwvcDpzbGRJZExzdD4KPHA6c2xkU3ogY3g9IjkxNDQwMDAiIGN5PSI2ODU4MDAwIi8+CjxwOm5vdGVzU3ogY3g9IjY4NTgwMDAiIGN5PSI5MTQ0MDAwIi8+CjwvcDpwcmVzZW50YXRpb24+ClBLAwQUAAAAAAAAACEAFMCPq7wBAAC8AQAAHwAAAHBwdC9fcmVscy9wcmVzZW50YXRpb24ueG1sLnJlbHM8P3htbCB2ZXJzaW9uPSIxLjAiIGVuY29kaW5nPSJVVEYtOCIgc3RhbmRhbG9uZT0ieWVzIj8+CjxSZWxhdGlvbnNoaXBzIHhtbG5zPSJodHRwOi8vc2NoZW1hcy5vcGVueG1sZm9ybWF0cy5vcmcvcGFja2FnZS8yMDA2L3JlbGF0aW9uc2hpcHMiPgo8UmVsYXRpb25zaGlwIElkPSJySWQxIiBUeXBlPSJodHRwOi8vc2NoZW1hcy5vcGVueG1sZm9ybWF0cy5vcmcvb2ZmaWNlRG9jdW1lbnQvMjAwNi9yZWxhdGlvbnNoaXBzL3NsaWRlTWFzdGVyIiBUYXJnZXQ9InNsaWRlTWFzdGVycy9zbGlkZU1hc3RlcjEueG1sIi8+CjxSZWxhdGlvbnNoaXAgSWQ9InJJZDIiIFR5cGU9Imh0dHA6Ly9zY2hlbWFzLm9wZW54bWxmb3JtYXRzLm9yZy9vZmZpY2VEb2N1bWVudC8yMDA2L3JlbGF0aW9uc2hpcHMvc2xpZGUiIFRhcmdldD0ic2xpZGVzL3NsaWRlMS54bWwiLz4KPC9SZWxhdGlvbnNoaXBzPgpQSwMEFAAAAAAAAAAhAFyz5RdbAgAAWwIAABUAAABwcHQvc2xpZGVzL3NsaWRlMS54bWw8P3htbCB2ZXJzaW9uPSIxLjAiIGVuY29kaW5nPSJVVEYtOCIgc3RhbmRhbG9uZT0ieWVzIj8+CjxwOnNsZCB4bWxuczphPSJodHRwOi8vc2NoZW1hcy5vcGVueG1sZm9ybWF0cy5vcmcvZHJhd2luZ21sLzIwMDYvbWFpbiIgeG1sbnM6cj0iaHR0cDovL3NjaGVtYXMub3BlbnhtbGZvcm1hdHMub3JnL29mZmljZURvY3VtZW50LzIwMDYvcmVsYXRpb25zaGlwcyIgeG1sbnM6cD0iaHR0cDovL3NjaGVtYXMub3BlbnhtbGZvcm1hdHMub3JnL3ByZXNlbnRhdGlvbm1sLzIwMDYvbWFpbiI+CjxwOmNTbGQ+CjxwOnNwVHJlZT4KPHA6bnZHcnBTcFByPjxwOmNOdlByIGlkPSIxIiBuYW1lPSIiLz48cDpjTnZHcnBTcFByLz48cDpudlByLz48L3A6bnZHcnBTcFByPgo8cDpncnBTcFByLz4KPHA6c3A+CjxwOm52U3BQcj48cDpjTnZQciBpZD0iMiIgbmFtZT0iVGl0bGUiLz48cDpjTnZTcFByLz48cDpudlByLz48L3A6bnZTcFByPgo8cDpzcFByLz4KPHA6dHhCb2R5PjxhOmJvZHlQci8+PGE6cD48YTpyPjxhOnQ+S0NTIG9mZmljZSBjb252ZXJ0IHRlc3Q8L2E6dD48L2E6cj48L2E6cD48L3A6dHhCb2R5Pgo8L3A6c3A+CjwvcDpzcFRyZWU+CjwvcDpjU2xkPgo8L3A6c2xkPgpQSwMEFAAAAAAAAAAhADTsLLQ5AQAAOQEAACAAAABwcHQvc2xpZGVzL19yZWxzL3NsaWRlMS54bWwucmVsczw/eG1sIHZlcnNpb249IjEuMCIgZW5jb2Rpbmc9IlVURi04IiBzdGFuZGFsb25lPSJ5ZXMiPz4KPFJlbGF0aW9uc2hpcHMgeG1sbnM9Imh0dHA6Ly9zY2hlbWFzLm9wZW54bWxmb3JtYXRzLm9yZy9wYWNrYWdlLzIwMDYvcmVsYXRpb25zaGlwcyI+CjxSZWxhdGlvbnNoaXAgSWQ9InJJZDEiIFR5cGU9Imh0dHA6Ly9zY2hlbWFzLm9wZW54bWxmb3JtYXRzLm9yZy9vZmZpY2VEb2N1bWVudC8yMDA2L3JlbGF0aW9uc2hpcHMvc2xpZGVMYXlvdXQiIFRhcmdldD0iLi4vc2xpZGVMYXlvdXRzL3NsaWRlTGF5b3V0MS54bWwiLz4KPC9SZWxhdGlvbnNoaXBzPgpQSwMEFAAAAAAAAAAhADYpqHbGAQAAxgEAACEAAABwcHQvc2xpZGVMYXlvdXRzL3NsaWRlTGF5b3V0MS54bWw8P3htbCB2ZXJzaW9uPSIxLjAiIGVuY29kaW5nPSJVVEYtOCIgc3RhbmRhbG9uZT0ieWVzIj8+CjxwOnNsZExheW91dCB4bWxuczphPSJodHRwOi8vc2NoZW1hcy5vcGVueG1sZm9ybWF0cy5vcmcvZHJhd2luZ21sLzIwMDYvbWFpbiIgeG1sbnM6cj0iaHR0cDovL3NjaGVtYXMub3BlbnhtbGZvcm1hdHMub3JnL29mZmljZURvY3VtZW50LzIwMDYvcmVsYXRpb25zaGlwcyIgeG1sbnM6cD0iaHR0cDovL3NjaGVtYXMub3BlbnhtbGZvcm1hdHMub3JnL3ByZXNlbnRhdGlvbm1sLzIwMDYvbWFpbiIgdHlwZT0iYmxhbmsiIHByZXNlcnZlPSIxIj4KPHA6Y1NsZD4KPHA6c3BUcmVlPgo8cDpudkdycFNwUHI+PHA6Y052UHIgaWQ9IjEiIG5hbWU9IiIvPjxwOmNOdkdycFNwUHIvPjxwOm52UHIvPjwvcDpudkdycFNwUHI+CjxwOmdycFNwUHIvPgo8L3A6c3BUcmVlPgo8L3A6Y1NsZD4KPC9wOnNsZExheW91dD4KUEsDBBQAAAAAAAAAIQAmX7qVOQEAADkBAAAsAAAAcHB0L3NsaWRlTGF5b3V0cy9fcmVscy9zbGlkZUxheW91dDEueG1sLnJlbHM8P3htbCB2ZXJzaW9uPSIxLjAiIGVuY29kaW5nPSJVVEYtOCIgc3RhbmRhbG9uZT0ieWVzIj8+CjxSZWxhdGlvbnNoaXBzIHhtbG5zPSJodHRwOi8vc2NoZW1hcy5vcGVueG1sZm9ybWF0cy5vcmcvcGFja2FnZS8yMDA2L3JlbGF0aW9uc2hpcHMiPgo8UmVsYXRpb25zaGlwIElkPSJySWQxIiBUeXBlPSJodHRwOi8vc2NoZW1hcy5vcGVueG1sZm9ybWF0cy5vcmcvb2ZmaWNlRG9jdW1lbnQvMjAwNi9yZWxhdGlvbnNoaXBzL3NsaWRlTWFzdGVyIiBUYXJnZXQ9Ii4uL3NsaWRlTWFzdGVycy9zbGlkZU1hc3RlcjEueG1sIi8+CjwvUmVsYXRpb25zaGlwcz4KUEsDBBQAAAAAAAAAIQBB3XZ8wAIAAMACAAAhAAAAcHB0L3NsaWRlTWFzdGVycy9zbGlkZU1hc3RlcjEueG1sPD94bWwgdmVyc2lvbj0iMS4wIiBlbmNvZGluZz0iVVRGLTgiIHN0YW5kYWxvbmU9InllcyI/Pgo8cDpzbGRNYXN0ZXIgeG1sbnM6YT0iaHR0cDovL3NjaGVtYXMub3BlbnhtbGZvcm1hdHMub3JnL2RyYXdpbmdtbC8yMDA2L21haW4iIHhtbG5zOnI9Imh0dHA6Ly9zY2hlbWFzLm9wZW54bWxmb3JtYXRzLm9yZy9vZmZpY2VEb2N1bWVudC8yMDA2L3JlbGF0aW9uc2hpcHMiIHhtbG5zOnA9Imh0dHA6Ly9zY2hlbWFzLm9wZW54bWxmb3JtYXRzLm9yZy9wcmVzZW50YXRpb25tbC8yMDA2L21haW4iPgo8cDpjU2xkPgo8cDpzcFRyZWU+CjxwOm52R3JwU3BQcj48cDpjTnZQciBpZD0iMSIgbmFtZT0iIi8+PHA6Y052R3JwU3BQci8+PHA6bnZQci8+PC9wOm52R3JwU3BQcj4KPHA6Z3JwU3BQci8+CjwvcDpzcFRyZWU+CjwvcDpjU2xkPgo8cDpjbHJNYXAgYmcxPSJsdDEiIHR4MT0iZGsxIiBiZzI9Imx0MiIgdHgyPSJkazIiIGFjY2VudDE9ImFjY2VudDEiIGFjY2VudDI9ImFjY2VudDIiIGFjY2VudDM9ImFjY2VudDMiIGFjY2VudDQ9ImFjY2VudDQiIGFjY2VudDU9ImFjY2VudDUiIGFjY2VudDY9ImFjY2VudDYiIGhsaW5rPSJobGluayIgZm9sSGxpbms9ImZvbEhsaW5rIi8+CjxwOnNsZExheW91dElkTHN0PjxwOnNsZExheW91dElkIGlkPSIyMTQ3NDgzNjQ5IiByOmlkPSJySWQxIi8+PC9wOnNsZExheW91dElkTHN0Pgo8L3A6c2xkTWFzdGVyPgpQSwMEFAAAAAAAAAAhAFIh0dPBAQAAwQEAACwAAABwcHQvc2xpZGVNYXN0ZXJzL19yZWxzL3NsaWRlTWFzdGVyMS54bWwucmVsczw/eG1sIHZlcnNpb249IjEuMCIgZW5jb2Rpbmc9IlVURi04IiBzdGFuZGFsb25lPSJ5ZXMiPz4KPFJlbGF0aW9uc2hpcHMgeG1sbnM9Imh0dHA6Ly9zY2hlbWFzLm9wZW54bWxmb3JtYXRzLm9yZy9wYWNrYWdlLzIwMDYvcmVsYXRpb25zaGlwcyI+CjxSZWxhdGlvbnNoaXAgSWQ9InJJZDEiIFR5cGU9Imh0dHA6Ly9zY2hlbWFzLm9wZW54bWxmb3JtYXRzLm9yZy9vZmZpY2VEb2N1bWVudC8yMDA2L3JlbGF0aW9uc2hpcHMvc2xpZGVMYXlvdXQiIFRhcmdldD0iLi4vc2xpZGVMYXlvdXRzL3NsaWRlTGF5b3V0MS54bWwiLz4KPFJlbGF0aW9uc2hpcCBJZD0icklkMiIgVHlwZT0iaHR0cDovL3NjaGVtYXMub3BlbnhtbGZvcm1hdHMub3JnL29mZmljZURvY3VtZW50LzIwMDYvcmVsYXRpb25zaGlwcy90aGVtZSIgVGFyZ2V0PSIuLi90aGVtZS90aGVtZTEueG1sIi8+CjwvUmVsYXRpb25zaGlwcz4KUEsDBBQAAAAAAAAAIQANajFPDgcAAA4HAAAUAAAAcHB0L3RoZW1lL3RoZW1lMS54bWw8P3htbCB2ZXJzaW9uPSIxLjAiIGVuY29kaW5nPSJVVEYtOCIgc3RhbmRhbG9uZT0ieWVzIj8+CjxhOnRoZW1lIHhtbG5zOmE9Imh0dHA6Ly9zY2hlbWFzLm9wZW54bWxmb3JtYXRzLm9yZy9kcmF3aW5nbWwvMjAwNi9tYWluIiBuYW1lPSJLQ1MiPgo8YTp0aGVtZUVsZW1lbnRzPgo8YTpjbHJTY2hlbWUgbmFtZT0iS0NTIj4KPGE6ZGsxPjxhOnN5c0NsciB2YWw9IndpbmRvd1RleHQiIGxhc3RDbHI9IjAwMDAwMCIvPjwvYTpkazE+CjxhOmx0MT48YTpzeXNDbHIgdmFsPSJ3aW5kb3ciIGxhc3RDbHI9IkZGRkZGRiIvPjwvYTpsdDE+CjxhOmRrMj48YTpzcmdiQ2xyIHZhbD0iMUY0OTdEIi8+PC9hOmRrMj4KPGE6bHQyPjxhOnNyZ2JDbHIgdmFsPSJFRUVDRTEiLz48L2E6bHQyPgo8YTphY2NlbnQxPjxhOnNyZ2JDbHIgdmFsPSI0RjgxQkQiLz48L2E6YWNjZW50MT4KPGE6YWNjZW50Mj48YTpzcmdiQ2xyIHZhbD0iQzA1MDREIi8+PC9hOmFjY2VudDI+CjxhOmFjY2VudDM+PGE6c3JnYkNsciB2YWw9IjlCQkI1OSIvPjwvYTphY2NlbnQzPgo8YTphY2NlbnQ0PjxhOnNyZ2JDbHIgdmFsPSI4MDY0QTIiLz48L2E6YWNjZW50ND4KPGE6YWNjZW50NT48YTpzcmdiQ2xyIHZhbD0iNEJBQ0M2Ii8+PC9hOmFjY2VudDU+CjxhOmFjY2VudDY+PGE6c3JnYkNsciB2YWw9IkY3OTY0NiIvPjwvYTphY2NlbnQ2Pgo8YTpobGluaz48YTpzcmdiQ2xyIHZhbD0iMDAwMEZGIi8+PC9hOmhsaW5rPgo8YTpmb2xIbGluaz48YTpzcmdiQ2xyIHZhbD0iODAwMDgwIi8+PC9hOmZvbEhsaW5rPgo8L2E6Y2xyU2NoZW1lPgo8YTpmb250U2NoZW1lIG5hbWU9IktDUyI+CjxhOm1ham9yRm9udD48YTpsYXRpbiB0eXBlZmFjZT0iQ2FsaWJyaSIvPjwvYTptYWpvckZvbnQ+CjxhOm1pbm9yRm9udD48YTpsYXRpbiB0eXBlZmFjZT0iQ2FsaWJyaSIvPjwvYTptaW5vckZvbnQ+CjwvYTpmb250U2NoZW1lPgo8YTpmbXRTY2hlbWUgbmFtZT0iS0NTIj4KPGE6ZmlsbFN0eWxlTHN0PjxhOnNvbGlkRmlsbD48YTpzY2hlbWVDbHIgdmFsPSJwaENsciIvPjwvYTpzb2xpZEZpbGw+PGE6c29saWRGaWxsPjxhOnNjaGVtZUNsciB2YWw9InBoQ2xyIi8+PC9hOnNvbGlkRmlsbD48YTpzb2xpZEZpbGw+PGE6c2NoZW1lQ2xyIHZhbD0icGhDbHIiLz48L2E6c29saWRGaWxsPjwvYTpmaWxsU3R5bGVMc3Q+CjxhOmxuU3R5bGVMc3Q+PGE6bG4+PGE6c29saWRGaWxsPjxhOnNjaGVtZUNsciB2YWw9InBoQ2xyIi8+PC9hOnNvbGlkRmlsbD48L2E6bG4+PGE6bG4+PGE6c29saWRGaWxsPjxhOnNjaGVtZUNsciB2YWw9InBoQ2xyIi8+PC9hOnNvbGlkRmlsbD48L2E6bG4+PGE6bG4+PGE6c29saWRGaWxsPjxhOnNjaGVtZUNsciB2YWw9InBoQ2xyIi8+PC9hOnNvbGlkRmlsbD48L2E6bG4+PC9hOmxuU3R5bGVMc3Q+CjxhOmVmZmVjdFN0eWxlTHN0PjxhOmVmZmVjdFN0eWxlPjxhOmVmZmVjdExzdC8+PC9hOmVmZmVjdFN0eWxlPjxhOmVmZmVjdFN0eWxlPjxhOmVmZmVjdExzdC8+PC9hOmVmZmVjdFN0eWxlPjxhOmVmZmVjdFN0eWxlPjxhOmVmZmVjdExzdC8+PC9hOmVmZmVjdFN0eWxlPjwvYTplZmZlY3RTdHlsZUxzdD4KPGE6YmdGaWxsU3R5bGVMc3Q+PGE6c29saWRGaWxsPjxhOnNjaGVtZUNsciB2YWw9InBoQ2xyIi8+PC9hOnNvbGlkRmlsbD48YTpzb2xpZEZpbGw+PGE6c2NoZW1lQ2xyIHZhbD0icGhDbHIiLz48L2E6c29saWRGaWxsPjxhOnNvbGlkRmlsbD48YTpzY2hlbWVDbHIgdmFsPSJwaENsciIvPjwvYTpzb2xpZEZpbGw+PC9hOmJnRmlsbFN0eWxlTHN0Pgo8L2E6Zm10U2NoZW1lPgo8L2E6dGhlbWVFbGVtZW50cz4KPC9hOnRoZW1lPgpQSwECFAMUAAAAAAAAACEAjMAWEdgDAADYAwAAEwAAAAAAAAAAAAAAgAEAAAAAW0NvbnRlbnRfVHlwZXNdLnhtbFBLAQIUAxQAAAAAAAAAIQAJqgfHMAEAADABAAALAAAAAAAAAAAAAACAAQkEAABfcmVscy8ucmVsc1BLAQIUAxQAAAAAAAAAIQBOiYW4BQIAAAUCAAAUAAAAAAAAAAAAAACAAWIFAABwcHQvcHJlc2VudGF0aW9uLnhtbFBLAQIUAxQAAAAAAAAAIQAUwI+rvAEAALwBAAAfAAAAAAAAAAAAAACAAZkHAABwcHQvX3JlbHMvcHJlc2VudGF0aW9uLnhtbC5yZWxzUEsBAhQDFAAAAAAAAAAhAFyz5RdbAgAAWwIAABUAAAAAAAAAAAAAAIABkgkAAHBwdC9zbGlkZXMvc2xpZGUxLnhtbFBLAQIUAxQAAAAAAAAAIQA07Cy0OQEAADkBAAAgAAAAAAAAAAAAAACAASAMAABwcHQvc2xpZGVzL19yZWxzL3NsaWRlMS54bWwucmVsc1BLAQIUAxQAAAAAAAAAIQA2Kah2xgEAAMYBAAAhAAAAAAAAAAAAAACAAZcNAABwcHQvc2xpZGVMYXlvdXRzL3NsaWRlTGF5b3V0MS54bWxQSwECFAMUAAAAAAAAACEAJl+6lTkBAAA5AQAALAAAAAAAAAAAAAAAgAGcDwAAcHB0L3NsaWRlTGF5b3V0cy9fcmVscy9zbGlkZUxheW91dDEueG1sLnJlbHNQSwECFAMUAAAAAAAAACEAQd12fMACAADAAgAAIQAAAAAAAAAAAAAAgAEfEQAAcHB0L3NsaWRlTWFzdGVycy9zbGlkZU1hc3RlcjEueG1sUEsBAhQDFAAAAAAAAAAhAFIh0dPBAQAAwQEAACwAAAAAAAAAAAAAAIABHhQAAHBwdC9zbGlkZU1hc3RlcnMvX3JlbHMvc2xpZGVNYXN0ZXIxLnhtbC5yZWxzUEsBAhQDFAAAAAAAAAAhAA1qMU8OBwAADgcAABQAAAAAAAAAAAAAAIABKRYAAHBwdC90aGVtZS90aGVtZTEueG1sUEsFBgAAAAALAAsALgMAAGkdAAAAAA==";

    #[test]
    fn bundled_office_fixtures_pass_offline_package_preflight() {
        crate::ooxml_package::validate_real_office_package(
            &decode_fixture(DOCX_FIXTURE_B64),
            DOCX_MEDIA_TYPE,
        )
        .expect("bundled DOCX package");
        crate::ooxml_package::validate_real_office_package(
            &decode_fixture(PPTX_FIXTURE_B64),
            PPTX_MEDIA_TYPE,
        )
        .expect("bundled PPTX package");
    }

    /// Native Office acceptance is intentionally opt-in so regular unit tests
    /// never discover or launch an ambient renderer. `KIO_REAL_OFFICE=1`
    /// makes an unavailable or broken explicit/PATH-resolved renderer a test
    /// failure rather than a skip.
    fn required_real_office_converter() -> Option<OfficeConverter> {
        if std::env::var_os("KIO_REAL_OFFICE").as_deref() != Some(std::ffi::OsStr::new("1")) {
            eprintln!(
                "skipping native Office acceptance; set KIO_REAL_OFFICE=1 and \
                 KIO_OFFICE_CONVERTER or PATH to require it"
            );
            return None;
        }
        let _clear_seam = kio_core::test_control::TestEnvGuard::remove(TEST_OFFICE_CONVERT_ENV);
        let selected = std::env::var_os(OFFICE_CONVERTER_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_OFFICE_CONVERTER_PROGRAM));
        let program = resolve_program(selected).expect(
            "KIO_REAL_OFFICE=1 requires KIO_OFFICE_CONVERTER or PATH to resolve a converter",
        );
        let converter = probe_real_converter_diagnostic(program)
            .expect("KIO_REAL_OFFICE=1 requires a working sandboxed Office converter");
        assert_ne!(converter.version(), "test-converter");
        Some(converter)
    }

    #[test]
    fn office_real_soffice_docx_converts_deterministically() {
        let _lock = kio_core::test_control::test_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _diagnostic = NativeAcceptanceDiagnosticGuard::enable();
        let Some(converter) = required_real_office_converter() else {
            return;
        };

        let docx = decode_fixture(DOCX_FIXTURE_B64);
        let first = converter
            .convert_to_pdf(&docx, DOCX_MEDIA_TYPE)
            .expect("first conversion");
        let second = converter
            .convert_to_pdf(&docx, DOCX_MEDIA_TYPE)
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
        let pages = crate::deterministic::extract_pdf_text_pages_bounded(
            &first,
            crate::deterministic::MAX_DETERMINISTIC_PDF_PAGES,
        )
        .expect("extract converted-PDF text layer");
        let joined = pages.join("\n");
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
        let _diagnostic = NativeAcceptanceDiagnosticGuard::enable();
        let Some(converter) = required_real_office_converter() else {
            return;
        };

        let pptx = decode_fixture(PPTX_FIXTURE_B64);
        let first = converter
            .convert_to_pdf(&pptx, PPTX_MEDIA_TYPE)
            .expect("first conversion");
        let second = converter
            .convert_to_pdf(&pptx, PPTX_MEDIA_TYPE)
            .expect("second conversion");
        assert!(first.starts_with(b"%PDF"), "converted output must be a PDF");
        assert_eq!(
            first, second,
            "normalized converted-PDF bytes must be identical across \
             independent conversions of the same input"
        );
    }
}
