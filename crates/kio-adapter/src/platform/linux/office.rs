use crate::{AdapterError, Result};
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
    Some(canonical)
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
pub(crate) struct OfficeRuntime;
impl OfficeRuntime {
    pub(crate) fn new(_program: &Path) -> Result<Self> {
        Ok(Self)
    }
    pub(crate) fn catalog_digest(&self) -> Option<&str> {
        None
    }
    pub(crate) fn validate_probe(&self, _stdout: &str) -> Result<()> {
        Ok(())
    }
    pub(crate) fn configure_services(
        &self,
        _scratch: &Path,
        _environment: &mut Vec<(OsString, OsString)>,
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
    pub(crate) fn runtime_roots(&self, program: &Path) -> Result<Vec<PathBuf>> {
        let mut roots = Vec::new();
        roots.extend(super::super::linux_package::linux_tdf_runtime_roots(
            program,
        )?);
        let package_program = Path::new("/usr/lib/libreoffice/program/soffice");
        if std::fs::canonicalize(package_program).ok().as_deref() == Some(program) {
            let share = PathBuf::from("/usr/lib/libreoffice/share");
            if !share.is_dir() {
                return Err(AdapterError::ContractViolation(
                    "recognized Linux LibreOffice package is missing its share runtime".to_owned(),
                ));
            }
            roots.push(share);
        }
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
        Ok(roots)
    }
}
