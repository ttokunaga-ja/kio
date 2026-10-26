use crate::{AdapterError, Result};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};
use tempfile::Builder;
pub(crate) fn canonical_renderer_program(candidate: PathBuf) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(&candidate).ok()?;
    if !canonical.is_file() {
        return None;
    }
    Some(canonical)
}
pub(crate) struct PrivateOfficeScratch {
    original: std::sync::Arc<std::fs::File>,
    pin: Option<std::fs::File>,
    parent: kio_core::store_dir::StoreDirectory,
    leaf: PathBuf,
    path: PathBuf,
    cleanup: bool,
}
impl PrivateOfficeScratch {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
    pub(crate) fn keep(mut self) -> PathBuf {
        self.cleanup = false;
        self.path.clone()
    }
    fn remove_owned(&mut self) -> Result<()> {
        use kio_core::store_dir::{PlannedDirectoryRemoval, RemovalBudget};
        let original = kio_core::store_dir::StoreDirectory::from_retained(
            self.original.try_clone().map_err(|error| {
                AdapterError::ContractViolation(format!(
                    "cannot retain scratch cleanup authority: {error}"
                ))
            })?,
            self.path.clone(),
        )
        .map_err(|error| {
            AdapterError::ContractViolation(format!(
                "cannot validate scratch cleanup authority: {error}"
            ))
        })?;
        // An active or unretired invocation still has its SID ACE. Refuse
        // destruction on unwind as well as normal errors until owner-only.
        kio_core::private_fs::verify_private_directory_handle(&original).map_err(|error| {
            AdapterError::ContractViolation(format!(
                "scratch remains under invocation/recovery authority: {error}"
            ))
        })?;
        let current = self.parent.open_directory(&self.leaf).map_err(|error| {
            AdapterError::ContractViolation(format!("cannot bind Office scratch cleanup: {error}"))
        })?;
        let expected = kio_core::platform::directory_handle_identity(&self.original);
        if expected.is_none()
            || kio_core::platform::directory_handle_identity(&current) != expected
            || self
                .pin
                .as_ref()
                .and_then(kio_core::platform::directory_handle_identity)
                != expected
        {
            return Err(AdapterError::ContractViolation(
                "Office scratch changed identity before cleanup".to_owned(),
            ));
        }
        // The creation-time strong pin keeps the original name fixed while the
        // core captures its exact bounded tree. No absolute recursive deletion.
        let plan =
            PlannedDirectoryRemoval::capture(&self.parent, &self.leaf, &mut RemovalBudget::new())
                .map_err(|error| {
                    AdapterError::ContractViolation(format!(
                        "cannot capture Office scratch cleanup: {error}"
                    ))
                })?
                .ok_or_else(|| {
                    AdapterError::ContractViolation(
                        "Office scratch disappeared before cleanup".to_owned(),
                    )
                })?;
        drop(current);
        drop(original);
        // DELETE mutator opens must not overlap our no-delete-sharing pin.
        drop(self.pin.take());
        if !plan.remove().map_err(|error| {
            AdapterError::ContractViolation(format!("Office scratch cleanup refused: {error}"))
        })? {
            return Err(AdapterError::ContractViolation(
                "Office scratch identity changed before retirement".to_owned(),
            ));
        }
        Ok(())
    }
    pub(crate) fn close(mut self) -> Result<()> {
        self.cleanup = false;
        self.remove_owned().map_err(|error| AdapterError::ContractViolation(format!(
            "{error}; cleanup incomplete for original Office scratch {}; any core quarantine remains for recovery", self.path.display())))
    }
}
impl Drop for PrivateOfficeScratch {
    fn drop(&mut self) {
        if self.cleanup {
            let _ = self.remove_owned();
        }
    }
}

pub(crate) fn private_temp_dir() -> Result<PrivateOfficeScratch> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY,
        FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL,
    };
    let temp = std::fs::canonicalize(std::env::temp_dir()).map_err(|error| {
        AdapterError::ContractViolation(format!("cannot bind Office temporary parent: {error}"))
    })?;
    let parent = kio_core::private_fs::verify_private_creation_parent(&temp).map_err(|error| {
        AdapterError::ContractViolation(format!("cannot retain Office temporary parent: {error}"))
    })?;
    let allocation = Builder::new()
        .prefix("kio-")
        .disable_cleanup(true)
        .make_in(&temp, |path| {
            let leaf = path
                .file_name()
                .ok_or_else(|| std::io::Error::other("invalid Office scratch leaf"))?;
            parent
                .create_directory(Path::new(leaf))
                .map_err(std::io::Error::other)
        })
        .map_err(|error| {
            AdapterError::ContractViolation(format!(
                "cannot create private Office scratch: {error}"
            ))
        })?;
    let (created, temporary_path) = allocation.into_parts();
    let path = temporary_path.to_path_buf();
    // File-ID normalization preserves the exclusively created object and drops
    // creation-only directory write rights before read-only monitor pins open.
    let directory = kio_core::store_dir::StoreDirectory::from_retained(created, path.clone())
        .map_err(|error| {
            AdapterError::ContractViolation(format!(
                "cannot retain created Office scratch: {error}"
            ))
        })?;
    kio_core::private_fs::verify_private_directory_handle(&directory).map_err(|error| {
        AdapterError::ContractViolation(format!("created Office scratch is unsafe: {error}"))
    })?;
    let original = directory.root_handle();
    let leaf = PathBuf::from(path.file_name().ok_or_else(|| {
        AdapterError::ContractViolation("invalid Office scratch name".to_owned())
    })?);
    let pin = std::fs::OpenOptions::new()
        .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | READ_CONTROL)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&path)
        .map_err(|error| {
            AdapterError::ContractViolation(format!("cannot pin private Office scratch: {error}"))
        })?;
    let expected = kio_core::platform::directory_handle_identity(&original);
    let current = parent.open_directory(&leaf).map_err(|error| {
        AdapterError::ContractViolation(format!("cannot bind created Office scratch: {error}"))
    })?;
    if expected.is_none()
        || kio_core::platform::directory_handle_identity(&pin) != expected
        || kio_core::platform::directory_handle_identity(&current) != expected
    {
        return Err(AdapterError::ContractViolation(
            "created Office scratch changed identity".to_owned(),
        ));
    }
    drop(current);
    Ok(PrivateOfficeScratch {
        original,
        pin: Some(pin),
        parent,
        leaf,
        path,
        cleanup: true,
    })
}

pub(crate) fn restrict_renderer_state_directory(
    scratch: &kio_core::store_dir::StoreDirectory,
    child: &str,
    purpose: &str,
    label: &str,
) -> Result<()> {
    kio_core::private_fs::verify_private_directory_at(scratch, Path::new(child)).map_err(
        |error| {
            AdapterError::ContractViolation(format!("unsafe private {purpose} {label}: {error}"))
        },
    )?;
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

pub(crate) fn scratch_recovery_note(scratch: PrivateOfficeScratch) -> String {
    format!(
        "; recovery scratch retained at {}",
        scratch.keep().display()
    )
}
pub(crate) fn finish_scratch(scratch: PrivateOfficeScratch) -> Result<()> {
    scratch.close()
}
