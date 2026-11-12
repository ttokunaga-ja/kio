use std::fs::{self, File};
use std::path::Path;

use cap_primitives::fs as cap_fs;
use kio_core::platform::{directory_handle_identity, real_directory_identity};
use kio_core::store_dir::StoreDirectory;
use kio_core::{ExitCode, KioError, Result};

/// Open the repository root through its parent and retain the directory authority.
pub(crate) fn open_chunk_ledger_root(kio_dir: &Path) -> Result<std::fs::File> {
    let before = real_directory_identity(kio_dir)
        .map_err(|error| KioError::io(error.to_string(), kio_dir.display().to_string()))?
        .ok_or_else(|| {
            KioError::io(
                "repository .kio root must be a real directory, not a reparse point",
                kio_dir.display().to_string(),
            )
        })?;
    // A descriptor-bound child has already `fchdir`ed into the retained `.kio`
    // directory, so `.` is both its only operational store spelling and an
    // inode-stable authority. Do not recover a parent/leaf path in this case:
    // doing so would either reject `.` syntactically or re-enter a replaceable
    // public `.kio` pathname.
    if kio_dir == Path::new(".") {
        return cap_fs::open_ambient_dir(Path::new("."), cap_primitives::ambient_authority())
            .map_err(|error| KioError::io(error.to_string(), "."));
    }
    let outer = kio_dir
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let leaf = kio_dir.file_name().ok_or_else(|| {
        KioError::io(
            "repository .kio root has no final path component",
            kio_dir.display().to_string(),
        )
    })?;
    let outer_handle = cap_fs::open_ambient_dir(outer, cap_primitives::ambient_authority())
        .map_err(|error| KioError::io(error.to_string(), outer.display().to_string()))?;
    let root = cap_fs::open_dir_nofollow(&outer_handle, Path::new(leaf))
        .map_err(|error| KioError::io(error.to_string(), kio_dir.display().to_string()))?;
    let unchanged = directory_handle_identity(&root) == Some(before);
    if !unchanged {
        return Err(KioError::io(
            "repository .kio root changed while opening",
            kio_dir.display().to_string(),
        ));
    }
    Ok(root)
}

pub(crate) fn configure_cap_no_follow(options: &mut cap_fs::OpenOptions) {
    use cap_fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
}

pub(crate) fn configure_private_cap_directory(_options: &mut cap_fs::DirOptions) {}

pub(crate) fn configure_chunk_ledger_open(options: &mut cap_fs::OpenOptions) {
    use cap_fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    configure_cap_no_follow(options);
    // Keep a no-delete-share handle for the entire ledger operation. A
    // public-path replacement cannot win between this nofollow open and
    // parsing/appending the durable record.
    options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
}

pub(crate) fn validate_chunk_ledger_links(_metadata: &fs::Metadata) -> std::io::Result<()> {
    Ok(())
}
pub(crate) fn validate_chunk_ledger_existing_links(
    _metadata: &cap_fs::Metadata,
    _path: &Path,
) -> Result<()> {
    Ok(())
}
pub(crate) fn validate_chunk_ledger_temp_links(
    _metadata: &fs::Metadata,
    _path: &Path,
) -> Result<()> {
    Ok(())
}

// Windows cannot flush a directory handle. Retain the real, non-reparse
// directory check and rely on the preceding file-level sync without claiming
// POSIX directory durability.
pub(crate) fn sync_chunk_ledger_directory(directory: &File, path: &Path) -> Result<()> {
    directory_handle_identity(directory).ok_or_else(|| {
        KioError::io(
            "bound directory must be a real directory, not a reparse point",
            path.display().to_string(),
        )
    })?;
    Ok(())
}

pub(crate) fn validate_cached_cas_links(_metadata: &fs::Metadata, _path: &Path) -> Result<()> {
    Ok(())
}

/// Require the final name to still identify the retained created directory.
pub(crate) fn verify_private_state_named_identity(
    directory: &StoreDirectory,
    path: &Path,
) -> Result<()> {
    let named = kio_core::platform::real_directory_identity(path)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?
        .ok_or_else(|| KioError::invalid_usage("private state path is not a real directory"))?;
    let retained = kio_core::platform::directory_handle_identity(&directory.root_handle())
        .ok_or_else(|| KioError::invalid_usage("retained private state is not a real directory"))?;
    if named != retained {
        return Err(KioError::new(
            "KIO-E-PRIVATE-STATE-RACED-001",
            "private state directory changed while being created",
            serde_json::json!({}),
            ExitCode::PermanentFailure,
        ));
    }
    Ok(())
}
