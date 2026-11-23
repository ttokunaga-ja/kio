use crate::office_convert::{MAX_OFFICE_LOG_BYTES, hex_digest};
use crate::office_runtime_identity::{RuntimeSnapshot, check_verification_deadline};
use crate::{AdapterError, Result};
use kio_process::{BoundedProcessError, BoundedProcessOptions};
use quick_xml::{Reader, events::Event};
use sha2::{Digest, Sha256};
use std::time::Instant;
use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
};
use tempfile::{Builder, TempDir};
pub(crate) fn macos_office_bundle_candidate(program: &Path) -> Option<PathBuf> {
    if !program.is_absolute() {
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

pub(crate) fn verify_macos_office_bundle(
    bundle: &Path,
    program: &Path,
    deadline: Instant,
) -> io::Result<()> {
    // A valid bundle can contain more than one executable. Its sealed
    // Info.plist determines which executable the bundle signature authorizes.
    let main = run_signature_tool(
        std::process::Command::new("/usr/bin/plutil")
            .args([
                "-extract",
                "CFBundleExecutable",
                "raw",
                "-expect",
                "string",
                "-n",
                "-o",
                "-",
            ])
            .arg(bundle.join("Contents/Info.plist")),
        "cannot read the Office bundle's main executable",
        deadline,
    )?;
    if macos_office_bundle_candidate(program).as_deref() != Some(bundle)
        || program.file_name() != Some(std::ffi::OsStr::new(&main))
    {
        return Err(io::Error::other(
            "Office renderer is not the bundle's main executable",
        ));
    }
    run_signature_tool(
        std::process::Command::new("/usr/bin/codesign")
            .args([
                "--verify",
                "--deep",
                "--strict",
                "--all-architectures",
                "-R=anchor apple generic",
            ])
            .arg(bundle),
        "Office bundle has no valid Apple-issued code signature",
        deadline,
    )?;
    Ok(())
}

fn verify_macos_nested_code<'a>(
    bundle: &Path,
    seals: impl IntoIterator<Item = &'a Path>,
    deadline: Instant,
) -> io::Result<()> {
    use std::collections::BTreeSet;

    let mut nested = BTreeSet::new();
    let mut main_seal = false;
    for seal in seals {
        check_verification_deadline(deadline)?;
        if !std::fs::symlink_metadata(seal).is_ok_and(|metadata| metadata.is_file()) {
            return Err(io::Error::other("unsafe macOS code resource seal"));
        }
        let base = seal
            .parent()
            .and_then(Path::parent)
            .expect("resource seal has a base");
        // Standard bundles seal paths relative to Contents; versioned
        // frameworks use the version directory itself as the signing base.
        let code = if base.ends_with("Contents") {
            base.parent().expect("bundle Contents has a parent")
        } else {
            base
        };
        if code == bundle {
            main_seal = true;
        } else {
            nested.insert(code.to_path_buf());
        }
        let files = run_signature_tool_with_limit(
            std::process::Command::new("/usr/bin/plutil")
                .args(["-extract", "files2", "xml1", "-o", "-"])
                .arg(seal),
            "cannot read the Office bundle's code resource seal",
            deadline,
            16 * 1024 * 1024,
        )?;
        let paths = crate::platform::office_macos_seal::nested_code_paths(&files)?;
        nested.extend(paths.into_iter().map(|path| base.join(path)));
    }
    check_verification_deadline(deadline)?;
    if !main_seal {
        return Err(io::Error::other(
            "Office bundle lacks a modern code resource seal",
        ));
    }
    // --deep validated the seal's own nested requirements. Each path here
    // becomes an explicit subject of our Apple anchor requirement.
    let nested: Vec<_> = nested.into_iter().collect();
    for batch in nested.chunks(64) {
        run_signature_tool(
            std::process::Command::new("/usr/bin/codesign")
                .args([
                    "--verify",
                    "--strict",
                    "--all-architectures",
                    "-R=anchor apple generic",
                ])
                .args(batch),
            "Office bundle nested code has no valid Apple-issued code signature",
            deadline,
        )?;
    }
    Ok(())
}

fn run_signature_tool(
    command: &mut std::process::Command,
    failure: &str,
    deadline: Instant,
) -> io::Result<String> {
    run_signature_tool_with_limit(command, failure, deadline, MAX_OFFICE_LOG_BYTES)
}

fn run_signature_tool_with_limit(
    command: &mut std::process::Command,
    failure: &str,
    deadline: Instant,
    max_stdout_bytes: usize,
) -> io::Result<String> {
    command.env_clear().current_dir("/");
    check_verification_deadline(deadline)?;
    let output = kio_process::run_bounded_command(
        command,
        BoundedProcessOptions {
            timeout: deadline.saturating_duration_since(Instant::now()),
            max_stdout_bytes,
            max_stderr_bytes: MAX_OFFICE_LOG_BYTES,
        },
        None,
    );
    if let Err(error @ BoundedProcessError::Timeout { .. }) = output {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("Office verification tool timed out: {error}"),
        ));
    }
    check_verification_deadline(deadline)?;
    match output {
        Ok(output) if output.status.success() => Ok(output.stdout),
        _ => Err(io::Error::other(failure.to_owned())),
    }
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

pub(crate) fn private_uno_catalog_for_program(
    program: &Path,
    deadline: Instant,
) -> Result<Option<PrivateUnoCatalog>> {
    let Some(bundle) = macos_office_bundle_candidate(program) else {
        return Ok(None);
    };
    let source = bundle.join("Contents/Resources/services/services.rdb");
    let before = office_bundle_path_snapshot(&bundle, program, &source)?;
    let bytes = read_stable_catalog(&source, deadline)?;
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

pub(crate) fn read_stable_catalog(path: &Path, deadline: Instant) -> Result<Vec<u8>> {
    const MAX_CATALOG_BYTES: usize = 1024 * 1024;
    kio_core::cas::read_bounded_regular_file_until(path, MAX_CATALOG_BYTES as u64, deadline)
        .map_err(|error| {
            AdapterError::ContractViolation(format!(
                "cannot safely read Office service catalog: {error}"
            ))
        })
}

pub(crate) fn macos_spell_component_range(bytes: &[u8]) -> Result<Option<(usize, usize)>> {
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
    Ok(match_range)
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

#[derive(Debug, Clone)]
pub(crate) struct PrivateUnoCatalog {
    pub(crate) source: PathBuf,
    pub(crate) bundle: PathBuf,
    pub(crate) source_digest: String,
}
pub(crate) fn write_private_uno_catalog(
    catalog: &PrivateUnoCatalog,
    scratch: &Path,
    deadline: Instant,
) -> Result<Option<PathBuf>> {
    let bytes = read_stable_catalog(&catalog.source, deadline)?;
    if hex_digest(&Sha256::digest(&bytes)) != catalog.source_digest {
        return Err(AdapterError::ContractViolation(
            "Office service catalog changed after converter probe".to_owned(),
        ));
    }
    let Some((start, end)) = macos_spell_component_range(&bytes)? else {
        return Ok(None);
    };
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
    Ok(Some(destination))
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
    super::super::unix::office_fs::private_office_scratch(builder.tempdir_in(socket_root))
}
#[derive(Debug, Clone)]
pub(crate) struct OfficeRuntime {
    catalog: Option<PrivateUnoCatalog>,
}
impl OfficeRuntime {
    pub(crate) fn verify_captured_signature(
        &self,
        program: &Path,
        snapshot: &RuntimeSnapshot,
        deadline: Instant,
    ) -> io::Result<()> {
        self.verify_runtime_signature(program, snapshot, deadline)
    }

    pub(crate) fn new(program: &Path, deadline: Instant) -> Result<Self> {
        Ok(Self {
            catalog: private_uno_catalog_for_program(program, deadline)?,
        })
    }
    pub(crate) fn verify_runtime_signature(
        &self,
        program: &Path,
        snapshot: &crate::office_runtime_identity::RuntimeSnapshot,
        deadline: Instant,
    ) -> io::Result<()> {
        let catalog = self
            .catalog
            .as_ref()
            .ok_or_else(|| io::Error::other("unsupported macOS Office runtime placement"))?;
        verify_macos_office_bundle(&catalog.bundle, program, deadline)?;
        verify_macos_nested_code(&catalog.bundle, snapshot.macos_code_resources(), deadline)
    }
    pub(crate) fn catalog_digest(&self) -> Option<&str> {
        self.catalog
            .as_ref()
            .map(|catalog| catalog.source_digest.as_str())
    }
    pub(crate) fn package_roots(&self, _program: &Path) -> Result<Vec<PathBuf>> {
        Ok(self
            .catalog
            .iter()
            .map(|catalog| catalog.bundle.clone())
            .collect())
    }
    pub(crate) fn configure_services(
        &self,
        scratch: &Path,
        environment: &mut Vec<(OsString, OsString)>,
        deadline: Instant,
    ) -> Result<()> {
        if let Some(catalog) = self.catalog.as_ref()
            && let Some(private_catalog) = write_private_uno_catalog(catalog, scratch, deadline)?
        {
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

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn signature_tool_timeout_reports_the_deadline() {
        let error = run_signature_tool(
            std::process::Command::new("/bin/sleep").arg("10"),
            "invalid signature",
            Instant::now() + std::time::Duration::from_millis(500),
        )
        .unwrap_err();
        let reason = error.to_string();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(reason.contains("timed out"), "{reason}");
        assert!(!reason.contains("invalid signature"), "{reason}");
    }

    #[test]
    fn unsuccessful_signature_tool_reports_the_verification_failure() {
        let error = run_signature_tool(
            &mut std::process::Command::new("/usr/bin/false"),
            "invalid signature",
            Instant::now() + std::time::Duration::from_secs(5),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(error.to_string().contains("invalid signature"), "{error}");
    }

    #[test]
    fn macos_outer_requirement_does_not_authorize_an_ad_hoc_helper() {
        let scratch = tempfile::tempdir().unwrap();
        let bundle = scratch.path().canonicalize().unwrap().join("Fixture.app");
        let binaries = bundle.join("Contents/MacOS");
        let helper = bundle.join("Contents/Frameworks/helper");
        std::fs::create_dir_all(&binaries).unwrap();
        std::fs::create_dir_all(helper.parent().unwrap()).unwrap();
        let main = binaries.join("main");
        std::fs::copy("/usr/bin/true", &main).unwrap();
        std::fs::copy("/usr/bin/true", &helper).unwrap();
        std::fs::write(
            bundle.join("Contents/Info.plist"),
            br#"<?xml version="1.0"?><plist version="1.0"><dict>
            <key>CFBundleIdentifier</key><string>org.kio.test-parent</string>
            <key>CFBundleExecutable</key><string>main</string>
            <key>CFBundlePackageType</key><string>APPL</string>
            </dict></plist>"#,
        )
        .unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        for item in [&helper, &bundle] {
            run_signature_tool(
                std::process::Command::new("/usr/bin/codesign")
                    .args(["--force", "--sign", "-", "--timestamp=none"])
                    .arg(item),
                "cannot sign test fixture",
                deadline,
            )
            .unwrap();
        }
        // CI lacks a distribution identity. Verify an explicit requirement
        // satisfied by the outer fixture, then apply the real Apple anchor to its helper.
        run_signature_tool(
            std::process::Command::new("/usr/bin/codesign")
                .args([
                    "--verify",
                    "--strict",
                    "--deep",
                    "--all-architectures",
                    "-R=identifier \"org.kio.test-parent\"",
                ])
                .arg(&bundle),
            "fixture must pass deep verification with an outer requirement",
            deadline,
        )
        .unwrap();
        let seal = bundle.join("Contents/_CodeSignature/CodeResources");
        let error = verify_macos_nested_code(&bundle, [seal.as_path()], deadline).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("nested code has no valid Apple-issued"),
            "{error}"
        );
    }

    #[test]
    fn snapshot_scope_is_the_verified_bundle() {
        let program = Path::new("/Applications/LibreOffice.app/Contents/MacOS/soffice");
        let bundle = PathBuf::from("/Applications/LibreOffice.app");
        let runtime = OfficeRuntime {
            catalog: Some(PrivateUnoCatalog {
                source: bundle.join("Contents/Resources/services/services.rdb"),
                bundle: bundle.clone(),
                source_digest: String::new(),
            }),
        };
        assert_eq!(runtime.package_roots(program).unwrap(), [bundle]);
        assert!(
            OfficeRuntime { catalog: None }
                .package_roots(Path::new("/usr/local/bin/standalone"))
                .unwrap()
                .is_empty()
        );
    }
}
