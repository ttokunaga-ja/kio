use crate::{AdapterError, Result};
use kio_process::confinement::{
    ConfinementError, RenderResourceLimits, RenderSandbox, RenderSources,
};
use std::{ffi::OsString, path::Path};

pub(crate) const OFFICE_LAUNCH_PROFILE: &[u8] = b"";
pub(crate) const OFFICE_REDIRECT_SHARED_CACHE: bool = false;
pub(crate) const OFFICE_QUERY_VERSION: bool = true;

pub(crate) fn office_render_sandbox(
    sources: &RenderSources,
    scratch: &Path,
    limits: RenderResourceLimits,
) -> std::result::Result<RenderSandbox, ConfinementError> {
    RenderSandbox::from_sources(sources, scratch, limits)
}

pub(crate) fn office_conversion_paths(outdir: &Path, input: &Path) -> (OsString, OsString) {
    (outdir.as_os_str().to_owned(), input.as_os_str().to_owned())
}

pub(crate) fn private_office_scratch(
    directory: std::io::Result<tempfile::TempDir>,
) -> Result<tempfile::TempDir> {
    let directory = directory.map_err(|error| {
        AdapterError::ContractViolation(format!(
            "failed to create private office scratch directory: {error}"
        ))
    })?;
    restrict_scratch(directory.path())?;
    Ok(directory)
}

pub(crate) fn restrict_scratch(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|error| {
        AdapterError::ContractViolation(format!(
            "failed to restrict office scratch directory permissions: {error}"
        ))
    })?;
    crate::office_convert::bind_private_scratch(path).map_err(|error| {
        AdapterError::ContractViolation(format!("unsafe Office scratch directory: {error}"))
    })?;
    Ok(())
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
pub(crate) fn environment() -> Vec<(OsString, OsString)> {
    let path = std::env::join_paths([Path::new("/usr/bin"), Path::new("/bin")])
        .expect("fixed Unix runtime paths contain no separators");
    vec![(OsString::from("PATH"), path)]
}
pub(crate) fn add_home(environment: &mut Vec<(OsString, OsString)>, home: Option<&Path>) {
    if let Some(home) = home {
        environment.push((OsString::from("HOME"), home.as_os_str().to_owned()));
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
}
pub(crate) fn file_url(path: &Path) -> Result<String> {
    let text = path.to_str().ok_or_else(|| {
        AdapterError::ContractViolation("office scratch path is not valid Unicode".to_owned())
    })?;
    Ok(format!(
        "file://{}",
        crate::office_convert::encode_file_url_path(text)
    ))
}

pub(crate) fn scratch_recovery_note(scratch: tempfile::TempDir) -> String {
    drop(scratch);
    String::new()
}
pub(crate) fn finish_scratch(scratch: tempfile::TempDir) -> Result<()> {
    scratch.close().map_err(|error| {
        AdapterError::ContractViolation(format!("failed to remove private Office scratch: {error}"))
    })
}

pub(crate) const DEFAULT_OFFICE_CONVERTER_PROGRAM: &str = "soffice";

pub(crate) fn office_package_launch_roots(_: &[std::path::PathBuf]) -> &[std::path::PathBuf] {
    &[]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finishing_office_scratch_reports_removal_failure() {
        let outer = tempfile::tempdir().unwrap();
        let parent = outer.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        let scratch = tempfile::tempdir_in(&parent).unwrap();
        std::fs::rename(&parent, outer.path().join("moved")).unwrap();
        std::fs::write(&parent, b"replacement").unwrap();

        assert!(finish_scratch(scratch).is_err());
    }
}
