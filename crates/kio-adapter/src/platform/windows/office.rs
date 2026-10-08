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
    super::protect_owner_private_scratch(directory.path()).map_err(|error| {
        AdapterError::ContractViolation(format!(
            "failed to establish owner-private office scratch directory: {error}"
        ))
    })?;
    Ok(directory)
}
pub(crate) fn restrict_renderer_state_directory(
    _path: &Path,
    _purpose: &str,
    _label: &str,
) -> Result<()> {
    Ok(())
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
        program: &Path,
        home: Option<&Path>,
    ) -> Vec<(OsString, OsString)> {
        renderer_environment(program, home)
    }
    pub(crate) fn runtime_roots(&self, _program: &Path) -> Result<Vec<PathBuf>> {
        let mut roots = Vec::new();
        if let Some(root) = std::env::var_os("SystemRoot") {
            roots.push(PathBuf::from(root));
        }
        Ok(roots)
    }
}
pub(crate) fn file_url(path: &Path) -> Result<String> {
    let text = path.to_str().ok_or_else(|| {
        AdapterError::ContractViolation("office scratch path is not valid Unicode".to_owned())
    })?;
    Ok(format!(
        "file:///{}",
        crate::office_convert::encode_file_url_path(&text.replace('\\', "/"))
    ))
}

pub(crate) fn renderer_environment(
    _program: &Path,
    home: Option<&Path>,
) -> Vec<(OsString, OsString)> {
    let mut environment = Vec::new();
    for name in ["SystemRoot", "WINDIR", "COMSPEC", "PATHEXT"] {
        if let Some(value) = std::env::var_os(name) {
            environment.push((OsString::from(name), value));
        }
    }
    let mut paths = Vec::new();
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        let root = PathBuf::from(system_root);
        paths.push(root.join("System32"));
        paths.push(root);
    }
    if let Ok(path) = std::env::join_paths(paths) {
        environment.push((OsString::from("PATH"), path));
    }
    if let Some(home) = home {
        environment.push((OsString::from("HOME"), home.as_os_str().to_owned()));
        for name in ["USERPROFILE", "LOCALAPPDATA"] {
            environment.push((OsString::from(name), home.as_os_str().to_owned()));
        }
    }
    environment
}
