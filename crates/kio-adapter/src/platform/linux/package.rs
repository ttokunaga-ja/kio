use crate::{AdapterError, Result};
use std::path::{Path, PathBuf};
pub(crate) fn linux_tdf_bundle_candidate(program: &Path) -> Option<PathBuf> {
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

pub(crate) fn trusted_linux_package_mode(owner: u32, mode: u32, directory: bool) -> bool {
    owner == 0 && mode & 0o022 == 0 && (directory || mode & 0o6000 == 0)
}

pub(crate) fn verify_linux_package_path(path: &Path, directory: bool) -> Result<()> {
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

pub(crate) fn linux_tdf_runtime_roots(program: &Path) -> Result<Vec<PathBuf>> {
    let Some(bundle) = linux_tdf_bundle_candidate(program) else {
        return Ok(Vec::new());
    };
    let program_directory = bundle.join("program");
    let share_directory = bundle.join("share");
    // TDF initializes each private user profile by copying this package leaf.
    let presets_directory = bundle.join("presets");
    for directory in [
        Path::new("/"),
        Path::new("/opt"),
        &bundle,
        &program_directory,
        &share_directory,
        &presets_directory,
    ] {
        verify_linux_package_path(directory, true)?;
    }
    verify_linux_package_path(program, false)?;
    Ok(vec![program_directory, share_directory, presets_directory])
}
