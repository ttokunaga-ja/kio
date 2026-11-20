use super::*;
use std::path::PathBuf;

use kio_core::litert::{Bundle, sha256};

#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
pub(super) const TARGET: &str = "x86_64-unknown-linux-gnu";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(super) const TARGET: &str = "aarch64-apple-darwin";
#[cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]
pub(super) const TARGET: &str = "x86_64-pc-windows-msvc";
#[cfg(not(any(
    all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"),
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"),
)))]
pub(super) const TARGET: &str = "";

#[derive(Debug)]
pub(super) struct VerifiedLibrary {
    pub path: PathBuf,
    #[cfg_attr(windows, allow(dead_code))] // Retains the deny-write/delete sharing mode.
    pub file: File,
}

pub(super) fn verify(executable: &Path) -> Result<VerifiedLibrary, WorkerError> {
    let bundle = Bundle::for_target(TARGET).ok_or(WorkerError::RuntimeUnsupported)?;
    let root = executable
        .parent()
        .and_then(Path::parent)
        .filter(|path| path.is_absolute())
        .ok_or(WorkerError::RuntimeUnavailable)?;
    let path = root.join(bundle.directory()).join(bundle.library.name());
    let mut options = File::options();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
        };
        options
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .share_mode(FILE_SHARE_READ);
    }
    let mut file = options.open(&path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => WorkerError::RuntimeMissing,
        _ => WorkerError::RuntimeUnavailable,
    })?;
    let metadata = file
        .metadata()
        .map_err(|_| WorkerError::RuntimeUnavailable)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(WorkerError::RuntimeUnavailable);
    }

    const MAX_LIBRARY_BYTES: u64 = 256 * 1024 * 1024;
    if metadata.len() > MAX_LIBRARY_BYTES
        || sha256(&mut (&mut file).take(MAX_LIBRARY_BYTES + 1))
            .map_err(|_| WorkerError::RuntimeUnavailable)?
            != bundle.library.sha256
    {
        return Err(WorkerError::RuntimeHashMismatch);
    }
    Ok(VerifiedLibrary { path, file })
}
