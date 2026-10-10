use std::fs::{self, File};
use std::path::Path;

use cap_primitives::fs as cap_fs;
use kio_core::store_dir::StoreDirectory;
use kio_core::{ExitCode, KioError, Result};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

fn same_directory_identity(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    before.dev() == after.dev() && before.ino() == after.ino()
}

/// Open the repository root through its parent and retain the directory authority.
pub(crate) fn open_chunk_ledger_root(kio_dir: &Path) -> Result<std::fs::File> {
    let before = fs::symlink_metadata(kio_dir)
        .map_err(|error| KioError::io(error.to_string(), kio_dir.display().to_string()))?;
    if before.file_type().is_symlink() || !before.is_dir() {
        return Err(KioError::io(
            "repository .kio root must be a real directory, not a symlink",
            kio_dir.display().to_string(),
        ));
    }
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
    let unchanged = {
        let after = root
            .metadata()
            .map_err(|error| KioError::io(error.to_string(), kio_dir.display().to_string()))?;
        same_directory_identity(&before, &after)
    };
    if !unchanged {
        return Err(KioError::io(
            "repository .kio root changed while opening",
            kio_dir.display().to_string(),
        ));
    }
    Ok(root)
}

pub(crate) fn configure_private_cap_directory(options: &mut cap_fs::DirOptions) {
    use cap_fs::DirBuilderExt;
    options.mode(0o700);
}

pub(crate) fn configure_chunk_ledger_open(options: &mut cap_fs::OpenOptions) {
    crate::platform::configure_cap_no_follow(options);
}

pub(crate) fn configure_chunk_ledger_temp(options: &mut cap_fs::OpenOptions) {
    use cap_fs::OpenOptionsExt;
    options.mode(0o600);
    crate::platform::configure_cap_no_follow(options);
}

pub(crate) fn validate_chunk_ledger_links(metadata: &fs::Metadata) -> std::io::Result<()> {
    if metadata.nlink() != 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "chunks.jsonl must have exactly one hard link",
        ));
    }
    Ok(())
}

pub(crate) fn validate_chunk_ledger_existing_links(
    metadata: &cap_fs::Metadata,
    path: &Path,
) -> Result<()> {
    use cap_fs::MetadataExt;
    if metadata.nlink() != 1 {
        return Err(KioError::io(
            "chunks.jsonl must have exactly one hard link",
            path.display().to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_chunk_ledger_temp_links(metadata: &fs::Metadata, path: &Path) -> Result<()> {
    if metadata.nlink() != 1 {
        return Err(KioError::io(
            "temporary chunk ledger must have exactly one hard link",
            path.display().to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn sync_chunk_ledger_directory(directory: &File, path: &Path) -> Result<()> {
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        // `.` is deliberately resolved through the retained directory
        // capability.  Requesting no-follow keeps this reopen on the same
        // no-symlink boundary as the original handle (and avoids a Linux
        // O_PATH-only resolution through the capability wrapper).
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let syncable = cap_fs::open(directory, Path::new("."), &options)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
    let metadata = syncable
        .metadata()
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
    if !metadata.is_dir() {
        return Err(KioError::io(
            "bound directory changed type",
            path.display().to_string(),
        ));
    }
    syncable
        .sync_all()
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))
}

pub(crate) fn configure_owner_only_file(options: &mut fs::OpenOptions) {
    options.mode(0o600);
}

pub(crate) fn harden_open_cache_hit(path: &Path) {
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o400));
}

pub(crate) fn harden_open_cache_subtree(leaf: &Path, root: &Path) {
    if !leaf.starts_with(root) {
        return;
    }
    let mut current = leaf.to_path_buf();
    loop {
        let _ = fs::set_permissions(&current, fs::Permissions::from_mode(0o700));
        if current == root {
            break;
        }
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => break,
        }
    }
}

pub(crate) fn validate_cached_cas_links(metadata: &fs::Metadata, path: &Path) -> Result<()> {
    if metadata.nlink() != 1 {
        return Err(crate::store_corrupt_error(
            path,
            "CAS object has an unexpected hard-link count",
        ));
    }
    Ok(())
}

pub(crate) fn validate_exact_cas_links(metadata: &fs::Metadata, path: &Path) -> Result<()> {
    if metadata.nlink() != 1 {
        return Err(crate::store_corrupt_error(
            path,
            "content-addressed object has an unexpected hard-link count",
        ));
    }
    Ok(())
}

/// Require the final name to still identify the retained created directory.
pub(crate) fn verify_private_state_named_identity(
    directory: &StoreDirectory,
    path: &Path,
) -> Result<()> {
    let named = std::fs::symlink_metadata(path)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
    let retained = directory
        .root_handle()
        .metadata()
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
    if named.file_type().is_symlink()
        || !named.is_dir()
        || named.dev() != retained.dev()
        || named.ino() != retained.ino()
    {
        return Err(KioError::new(
            "KIO-E-PRIVATE-STATE-RACED-001",
            "private state directory changed while being created",
            serde_json::json!({}),
            ExitCode::PermanentFailure,
        ));
    }
    Ok(())
}
