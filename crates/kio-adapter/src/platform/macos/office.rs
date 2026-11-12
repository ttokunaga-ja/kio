use crate::office_convert::{MAX_OFFICE_LOG_BYTES, hex_digest};
use crate::{AdapterError, Result};
use kio_process::BoundedProcessOptions;
use quick_xml::{Reader, events::Event};
use sha2::{Digest, Sha256};
use std::time::Duration;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};
use tempfile::{Builder, TempDir};
pub(crate) fn canonical_renderer_program(candidate: PathBuf) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(&candidate).ok()?;
    if !canonical.is_file() {
        return None;
    }
    // Recognize only the known passthrough wrapper by both location and bytes.
    // A command installed at the Homebrew name may select another application
    // or add arguments; its name alone cannot authorize substituting this app.
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
pub(crate) fn is_macos_office_passthrough_wrapper(bytes: &[u8]) -> bool {
    bytes == b"#!/bin/bash\nexec \"/Applications/LibreOffice.app/Contents/MacOS/soffice\"  \"$@\"\n"
}

/// Supported-release policy for verified macOS LibreOffice bundles only.
/// This prevents known incompatible releases; actual native acceptance remains
/// the capability proof for each pinned distribution.
pub(crate) fn validate_macos_office_version(stdout: &str) -> Result<()> {
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

pub(crate) fn macos_office_bundle_candidate(program: &Path) -> Option<PathBuf> {
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

pub(crate) fn verify_macos_office_bundle(bundle: &Path) -> Result<()> {
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

pub(crate) fn office_bundle_path_snapshot(
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

pub(crate) fn private_uno_catalog_for_program(program: &Path) -> Result<Option<PrivateUnoCatalog>> {
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

pub(crate) fn read_stable_catalog(path: &Path) -> Result<Vec<u8>> {
    const MAX_CATALOG_BYTES: usize = 1024 * 1024;
    kio_core::cas::read_bounded_regular_file(path, MAX_CATALOG_BYTES as u64).map_err(|error| {
        AdapterError::ContractViolation(format!(
            "cannot safely read Office service catalog: {error}"
        ))
    })
}

pub(crate) fn macos_spell_component_range(bytes: &[u8]) -> Result<(usize, usize)> {
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

#[derive(PartialEq, Eq)]
pub(crate) struct OfficeBundlePathIdentity {
    device: u64,
    inode: u64,
    mode: u32,
    owner: u32,
    len: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrivateUnoCatalog {
    pub(crate) source: PathBuf,
    pub(crate) bundle: PathBuf,
    pub(crate) source_digest: String,
}
pub(crate) fn write_private_uno_catalog(
    catalog: &PrivateUnoCatalog,
    scratch: &Path,
) -> Result<PathBuf> {
    let bytes = read_stable_catalog(&catalog.source)?;
    if hex_digest(&Sha256::digest(&bytes)) != catalog.source_digest {
        return Err(AdapterError::ContractViolation(
            "Office service catalog changed after converter probe".to_owned(),
        ));
    }
    let (start, end) = macos_spell_component_range(&bytes)?;
    let destination = scratch.join("services-without-macos-spell.rdb");
    let directory = crate::office_convert::bind_private_scratch(scratch).map_err(|error| {
        AdapterError::ContractViolation(format!("unsafe Office service catalog scratch: {error}"))
    })?;
    let mut output = directory
        .create_private_file(Path::new("services-without-macos-spell.rdb"))
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
    Ok(destination)
}
/// Create an exclusively allocated, owner-private scratch directory.
/// Use the short canonical parent required by LibreOffice Unix-domain sockets.
pub(crate) fn private_temp_dir() -> Result<TempDir> {
    let mut builder = Builder::new();
    builder.prefix("kio-");
    let socket_root = std::fs::canonicalize("/private/tmp").map_err(|error| {
        AdapterError::ContractViolation(format!(
            "failed to resolve private Office socket root: {error}"
        ))
    })?;
    let directory = builder.tempdir_in(socket_root).map_err(|error| {
        AdapterError::ContractViolation(format!(
            "failed to create private office scratch directory: {error}"
        ))
    })?;
    super::super::unix::office_fs::restrict_scratch(directory.path())?;
    Ok(directory)
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OfficeRuntime {
    catalog: Option<PrivateUnoCatalog>,
}
impl OfficeRuntime {
    pub(crate) fn new(program: &Path) -> Result<Self> {
        Ok(Self {
            catalog: private_uno_catalog_for_program(program)?,
        })
    }
    pub(crate) fn catalog_digest(&self) -> Option<&str> {
        self.catalog
            .as_ref()
            .map(|catalog| catalog.source_digest.as_str())
    }
    pub(crate) fn validate_probe(&self, stdout: &str) -> Result<()> {
        if self.catalog.is_some() {
            validate_macos_office_version(stdout)?;
        }
        Ok(())
    }
    pub(crate) fn configure_services(
        &self,
        scratch: &Path,
        environment: &mut Vec<(OsString, OsString)>,
    ) -> Result<()> {
        if let Some(catalog) = self.catalog.as_ref() {
            let private_catalog = write_private_uno_catalog(catalog, scratch)?;
            environment.push((
                OsString::from("URE_MORE_SERVICES"),
                super::super::file_url(&private_catalog)?.into(),
            ));
        }
        Ok(())
    }
    pub(crate) fn environment(
        &self,
        program: &Path,
        home: Option<&Path>,
    ) -> Vec<(OsString, OsString)> {
        renderer_environment(program, home)
    }
    pub(crate) fn runtime_roots(&self, program: &Path) -> Result<Vec<PathBuf>> {
        renderer_runtime_roots(program, self.catalog.as_ref())
    }
}
pub(crate) fn renderer_runtime_roots(
    _program: &Path,
    _catalog: Option<&PrivateUnoCatalog>,
) -> Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
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
    if let Some(catalog) = _catalog {
        roots.push(catalog.bundle.clone());
    }
    Ok(roots)
}

pub(crate) fn renderer_environment(
    _program: &Path,
    home: Option<&Path>,
) -> Vec<(OsString, OsString)> {
    let mut environment = super::super::unix::office_fs::environment();
    environment.push((
        OsString::from("PYTHONDONTWRITEBYTECODE"),
        OsString::from("1"),
    ));
    super::super::unix::office_fs::add_home(&mut environment, home);
    environment
}
