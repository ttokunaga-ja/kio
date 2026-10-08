use crate::{AdapterError, Result};
use std::{ffi::OsString, path::Path};
pub(crate) fn restrict_scratch(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|error| {
        AdapterError::ContractViolation(format!(
            "failed to restrict office scratch directory permissions: {error}"
        ))
    })?;
    Ok(())
}
pub(crate) fn restrict_renderer_state_directory(
    path: &Path,
    purpose: &str,
    label: &str,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|error| {
        AdapterError::ContractViolation(format!(
            "failed to restrict private {purpose} {label}: {error}"
        ))
    })?;
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
