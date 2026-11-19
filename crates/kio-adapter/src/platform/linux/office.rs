use super::dpkg::DpkgCatalog;
use crate::{
    AdapterError, Result,
    office_runtime_identity::{RuntimeSnapshot, check_verification_deadline},
};
use kio_process::confinement::RenderSources;
use std::{
    ffi::OsString,
    fs::{self, Metadata},
    io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::Instant,
};
use tempfile::{Builder, TempDir};

pub(crate) fn canonical_renderer_program(candidate: PathBuf) -> Option<PathBuf> {
    let canonical = fs::canonicalize(candidate).ok()?;
    canonical.is_file().then_some(canonical)
}

pub(crate) fn private_temp_dir() -> Result<TempDir> {
    let directory = Builder::new().prefix("kio-").tempdir().map_err(|error| {
        AdapterError::ContractViolation(format!(
            "failed to create private office scratch directory: {error}"
        ))
    })?;
    super::super::unix::office_fs::restrict_scratch(directory.path())?;
    Ok(directory)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OfficeRuntime {
    roots: Vec<PathBuf>,
    system: PathBuf,
    trust: RuntimeTrust,
    distro: bool,
}

impl OfficeRuntime {
    pub(crate) fn new(program: &Path, deadline: Instant) -> Result<Self> {
        // SAFETY: geteuid takes no arguments and has no memory preconditions.
        let uid = unsafe { libc::geteuid() };
        Self::discover(program, Path::new("/"), 0, uid, deadline)
    }

    fn discover(
        program: &Path,
        system: &Path,
        system_uid: u32,
        user_uid: u32,
        deadline: Instant,
    ) -> Result<Self> {
        let distro_root = system.join("usr/lib/libreoffice");
        let soffice = program
            .file_name()
            .is_some_and(|name| name == "soffice" || name == "soffice.bin");
        let distro = soffice && program.parent() == Some(distro_root.join("program").as_path());
        let mut trust = RuntimeTrust {
            system_uid,
            owner: system_uid,
        };
        let roots = if distro {
            let mut roots = vec![distro_root];
            for path in [
                "usr/share/libreoffice",
                "etc/libreoffice",
                "var/lib/libreoffice/share/prereg/bundled",
                "var/spool/libreoffice/uno_packages/cache",
            ] {
                let root = system.join(path);
                if optional_metadata(&root).map_err(contract_error)?.is_some() {
                    roots.push(root);
                }
            }
            roots
        } else {
            let Some(bundle) = program.parent().and_then(Path::parent).filter(|bundle| {
                soffice
                    && program.parent() == Some(bundle.join("program").as_path())
                    && bundle.join("program/soffice.bin").is_file()
                    && bundle.join("program/bootstraprc").is_file()
            }) else {
                return Err(contract_error(format_args!(
                    "unsupported Office runtime placement for {}: cannot identify its package",
                    program.display()
                )));
            };
            trust.owner = fs::symlink_metadata(bundle).map_err(contract_error)?.uid();
            if trust.owner != system_uid && trust.owner != user_uid {
                return Err(contract_error(format_args!(
                    "untrusted Office package owner: {}",
                    bundle.display()
                )));
            }
            let mut roots = vec![bundle.join("program"), bundle.join("share")];
            let presets = bundle.join("presets");
            if optional_metadata(&presets)
                .map_err(contract_error)?
                .is_some()
            {
                roots.push(presets);
            }
            roots
        };
        let runtime = Self {
            roots,
            system: system.into(),
            trust,
            distro,
        };
        runtime.verify_roots(deadline).map_err(contract_error)?;
        Ok(runtime)
    }

    pub(crate) fn verify_roots(&self, deadline: Instant) -> io::Result<()> {
        for root in &self.roots {
            check_verification_deadline(deadline)?;
            self.trust.verify_ancestors(root, &self.system)?;
        }
        Ok(())
    }

    pub(crate) fn catalog_digest(&self) -> Option<&str> {
        None
    }

    pub(crate) fn package_roots(&self, _program: &Path) -> Result<Vec<PathBuf>> {
        Ok(self.roots.clone())
    }

    pub(crate) fn bind_runtime(&self, sources: RenderSources) -> Result<RenderSources> {
        // dpkg's post-install Python bytecode has no package checksum. Cover the
        // generated directory in the existing OS library bind with an empty one.
        let cache = self.system.join("usr/lib/libreoffice/program/__pycache__");
        if self.distro
            && optional_metadata(&cache)
                .map_err(contract_error)?
                .is_some_and(|metadata| metadata.is_dir())
        {
            return sources
                .with_masked_runtime_directory(&cache)
                .map_err(contract_error);
        }
        Ok(sources)
    }

    pub(crate) fn snapshot(
        &self,
        roots: Vec<PathBuf>,
        sources: &RenderSources,
        deadline: Instant,
    ) -> io::Result<RuntimeSnapshot> {
        let catalog = self
            .distro
            .then(|| DpkgCatalog::read(&self.system, self.trust, deadline))
            .transpose()?;
        let check = |path: &Path, metadata: &Metadata| {
            self.trust.verify_entry(path, metadata)?;
            let in_package = self.roots.iter().any(|root| path.starts_with(root));
            if let Some(catalog) = &catalog {
                let relative = path.strip_prefix(&self.system).unwrap_or(path);
                let external = !metadata.is_dir()
                    && (relative.starts_with("usr/share/java")
                        || relative
                            .strip_prefix("usr/share/doc")
                            .ok()
                            .and_then(|suffix| suffix.components().next())
                            .is_some_and(|name| {
                                name.as_os_str()
                                    .as_encoded_bytes()
                                    .starts_with(b"libreoffice-")
                            }));
                if in_package || external {
                    if external {
                        self.trust.verify_ancestors(
                            path.parent().expect("relative runtime path"),
                            &self.system,
                        )?;
                    }
                    return catalog.verify(path, metadata, deadline);
                }
            } else if in_package {
                return Ok(());
            }
            Err(io::Error::other(format!(
                "outside the verified Office runtime: {}",
                path.display()
            )))
        };
        let excluded = sources.masked_runtime_directory().map(Path::to_path_buf);
        RuntimeSnapshot::new_checked(roots, sources, excluded, &check, deadline)
    }

    pub(crate) fn configure_services(
        &self,
        _scratch: &Path,
        _environment: &mut Vec<(OsString, OsString)>,
        _deadline: Instant,
    ) -> Result<()> {
        Ok(())
    }

    pub(crate) fn environment(
        &self,
        _program: &Path,
        home: Option<&Path>,
    ) -> Vec<(OsString, OsString)> {
        let mut environment = super::super::unix::office_fs::environment();
        super::super::unix::office_fs::add_home(&mut environment, home);
        environment
    }

    pub(crate) fn runtime_roots(&self, _program: &Path) -> Result<Vec<PathBuf>> {
        let system_roots: Vec<_> = [
            "bin",
            "usr/bin",
            "usr/lib",
            "usr/share",
            "lib",
            "lib64",
            "etc/fonts",
        ]
        .into_iter()
        .map(|path| self.system.join(path))
        .filter(|path| path.exists())
        .collect();
        let extra = self
            .roots
            .iter()
            .filter(|root| !system_roots.iter().any(|base| root.starts_with(base)));
        Ok(extra.cloned().chain(system_roots.iter().cloned()).collect())
    }

    #[cfg(test)]
    pub(crate) fn fixture(package: PathBuf) -> Self {
        // SAFETY: geteuid takes no arguments and has no memory preconditions.
        let owner = unsafe { libc::geteuid() };
        Self {
            roots: vec![package],
            system: PathBuf::from("/"),
            trust: RuntimeTrust {
                system_uid: fs::metadata("/").unwrap().uid(),
                owner,
            },
            distro: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RuntimeTrust {
    system_uid: u32,
    owner: u32,
}

impl RuntimeTrust {
    pub(super) fn verify_entry(self, path: &Path, metadata: &Metadata) -> io::Result<()> {
        let mode = metadata.mode();
        let safe_mode = metadata.is_symlink()
            || (mode & 0o022 == 0 && (metadata.is_dir() || mode & 0o6000 == 0));
        if ![self.system_uid, self.owner].contains(&metadata.uid()) || !safe_mode {
            return Err(io::Error::other(format!(
                "unsafe ownership or permissions for Office path {}",
                path.display()
            )));
        }
        Ok(())
    }

    pub(super) fn verify_ancestors(self, path: &Path, system: &Path) -> io::Result<()> {
        for directory in path.ancestors().take_while(|dir| dir.starts_with(system)) {
            let metadata = fs::symlink_metadata(directory).map_err(|error| {
                io::Error::other(format!(
                    "cannot verify Office runtime directory {}: {error}",
                    directory.display()
                ))
            })?;
            if !metadata.is_dir() {
                return Err(io::Error::other(format!(
                    "expected a real Office runtime directory: {}",
                    directory.display()
                )));
            }
            // Sticky system directories (e.g. /tmp) cannot replace the trusted child.
            if directory != path
                && metadata.uid() == self.system_uid
                && metadata.mode() & 0o1000 != 0
            {
                continue;
            }
            self.verify_entry(directory, &metadata)?;
        }
        Ok(())
    }
}

pub(super) fn optional_metadata(path: &Path) -> io::Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn contract_error(error: impl std::fmt::Display) -> AdapterError {
    AdapterError::ContractViolation(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kio_process::confinement::RenderSources;
    use std::{
        fs,
        os::unix::fs::{MetadataExt, symlink},
        time::{Duration, Instant},
    };

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }

    fn file(root: &Path, relative: &str, bytes: &[u8]) -> PathBuf {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }

    fn verify(program: &Path, system: &Path) -> Result<Vec<PathBuf>> {
        let uid = fs::metadata(system).unwrap().uid();
        let runtime = OfficeRuntime::discover(program, system, uid, uid, deadline())?;
        let package = runtime.package_roots(program)?;
        let mounts = runtime.runtime_roots(program)?;
        let sources =
            runtime.bind_runtime(RenderSources::resolve(program, mounts.clone()).unwrap())?;
        runtime
            .snapshot(package, &sources, deadline())
            .map_err(|error| AdapterError::ContractViolation(error.to_string()))?;
        Ok(mounts)
    }

    #[test]
    fn linux_closure_admits_package_links_for_tdf_and_portable() {
        let scratch = tempfile::tempdir().unwrap();
        let system = scratch.path().canonicalize().unwrap();
        for name in ["opt/libreoffice26.2", "extracted/renderer"] {
            let bundle = system.join(name);
            let program = file(&bundle, "program/soffice", b"launcher");
            file(&bundle, "program/soffice.bin", b"executable");
            file(&bundle, "program/bootstraprc", b"profile");
            file(&bundle, "share/data", b"runtime");
            fs::create_dir(bundle.join("presets")).unwrap();
            symlink("../share/data", bundle.join("program/resource")).unwrap();
            fs::create_dir(bundle.join("share/subdir")).unwrap();
            symlink("../share/subdir", bundle.join("program/inside")).unwrap();
            symlink("inside/../data", bundle.join("program/via_parent")).unwrap();
            let mounts = verify(&program, &system).unwrap();
            assert!(mounts.contains(&bundle.join("share")));
            assert!(!mounts.contains(&bundle));
            assert!(verify(&bundle.join("program/soffice.bin"), &system).is_ok());

            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(bundle.join("share/data"), fs::Permissions::from_mode(0o666))
                .unwrap();
            let error = verify(&program, &system).unwrap_err().to_string();
            assert!(error.contains("unsafe ownership or permissions"), "{error}");
        }
    }

    #[test]
    fn linux_closure_rejects_link_that_leaves_package_and_reenters() {
        let scratch = tempfile::tempdir().unwrap();
        let system = scratch.path().canonicalize().unwrap();
        let bundle = system.join("extracted");
        let program = file(&bundle, "program/soffice", b"launcher");
        file(&bundle, "program/soffice.bin", b"executable");
        file(&bundle, "program/bootstraprc", b"profile");
        let data = file(&bundle, "share/data", b"runtime");
        symlink(&data, system.join("outside")).unwrap();
        symlink(system.join("outside"), bundle.join("program/resource")).unwrap();
        let error = verify(&program, &system).unwrap_err().to_string();
        assert!(
            error.contains("outside the verified Office runtime"),
            "{error}"
        );
    }

    #[test]
    fn linux_closure_rejects_unidentified_placement_with_reason() {
        let scratch = tempfile::tempdir().unwrap();
        let system = scratch.path().canonicalize().unwrap();
        let program = file(&system, "bin/soffice", b"launcher");
        let error = verify(&program, &system).unwrap_err().to_string();
        assert!(error.contains("cannot identify its package"), "{error}");
    }

    #[test]
    fn linux_closure_verifies_dpkg_membership_and_content() {
        let scratch = tempfile::tempdir().unwrap();
        let system = scratch.path().canonicalize().unwrap();
        let program = file(&system, "usr/lib/libreoffice/program/soffice", b"abc");
        let config = file(&system, "etc/libreoffice/registry/main.xcd", b"abc");
        let conffile = file(&system, "etc/libreoffice/sofficerc", b"abc");
        let jar = file(&system, "usr/share/java/uno-1.jar", b"abc");
        let alias = jar.with_file_name("uno.jar");
        symlink(&jar, &alias).unwrap();
        file(
            &system,
            "usr/lib/libreoffice/program/__pycache__/generated.pyc",
            b"no checksum",
        );
        let share = system.join("usr/lib/libreoffice/share");
        fs::create_dir_all(&share).unwrap();
        symlink(config.parent().unwrap(), share.join("registry")).unwrap();
        symlink(&alias, share.join("jar")).unwrap();
        let sums = b"900150983cd24fb0d6963f7d28e17f72  usr/lib/libreoffice/program/soffice\n\
            900150983cd24fb0d6963f7d28e17f72  usr/lib/libreoffice/share/.registry/main.xcd\n\
            900150983cd24fb0d6963f7d28e17f72  usr/share/java/uno-1.jar\n";
        file(
            &system,
            "usr/lib/libreoffice/share/.registry/main.xcd",
            b"abc",
        );
        file(&system, "var/lib/dpkg/info/libreoffice-core.md5sums", sums);
        file(
            &system,
            "var/lib/dpkg/info/libreoffice-core.list",
            b"/usr/lib/libreoffice\n\
             /usr/lib/libreoffice/program\n\
             /usr/lib/libreoffice/program/soffice\n\
             /usr/lib/libreoffice/share\n\
             /usr/lib/libreoffice/share/.registry\n\
             /usr/lib/libreoffice/share/.registry/main.xcd\n\
             /usr/lib/libreoffice/share/registry\n\
             /usr/lib/libreoffice/share/jar\n\
             /etc/libreoffice\n\
             /etc/libreoffice/registry\n\
             /etc/libreoffice/sofficerc\n\
             /usr/share/java/uno-1.jar\n\
             /usr/share/java/uno.jar\n",
        );
        file(
            &system,
            "var/lib/dpkg/status",
            b"Package: libreoffice-core\n\
             Status: install ok installed\n\
             Architecture: amd64\n\
             Conffiles:\n\
             \x20/etc/libreoffice/sofficerc 900150983cd24fb0d6963f7d28e17f72\n",
        );
        file(
            &system,
            "var/lib/ucf/registry",
            b"libreoffice-core /etc/libreoffice/registry/main.xcd\n",
        );

        let mounts = verify(&program, &system).unwrap();
        assert!(mounts.contains(&system.join("etc/libreoffice")));
        assert!(!mounts.contains(&system.join("etc")));
        fs::write(&conffile, b"changed").unwrap();
        let error = verify(&program, &system).unwrap_err().to_string();
        assert!(
            error.contains("checksum mismatch") && error.contains("sofficerc"),
            "{error}"
        );
        fs::write(&conffile, b"abc").unwrap();
        fs::remove_file(&alias).unwrap();
        symlink(file(&system, "usr/share/unrelated", b"abc"), &alias).unwrap();
        let error = verify(&program, &system).unwrap_err().to_string();
        assert!(
            error.contains("outside the verified Office runtime"),
            "{error}"
        );
        fs::remove_file(&alias).unwrap();
        symlink(&jar, &alias).unwrap();
        fs::write(&config, b"changed").unwrap();
        let error = verify(&program, &system).unwrap_err().to_string();
        assert!(
            error.contains("checksum mismatch") && error.contains("main.xcd"),
            "{error}"
        );
        fs::write(&config, b"abc").unwrap();
        file(&system, "etc/libreoffice/unlisted", b"unverified");
        let error = verify(&program, &system).unwrap_err().to_string();
        assert!(error.contains("unlisted"), "{error}");
    }
}
