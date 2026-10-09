use crate::{IoResultExt, Result};
use cap_primitives::fs as cap_fs;
use kio_core::management::DirectoryIdentity;
use std::fs::File;
use std::fs::Metadata;
use std::path::Path;

pub(crate) fn directory_identity(file: &File) -> Result<DirectoryIdentity> {
    use cap_fs::MetadataExt;
    let metadata = cap_fs::Metadata::from_file(file).pipeline_io(Path::new("."))?;
    if !metadata.file_type().is_dir() {
        return Err(crate::PipelineError::contract(
            "KIO-E-CHILD-DISCOVERY-CONTINUATION-STALE-001",
            "discovery frontier is not a directory",
        ));
    }
    Ok(DirectoryIdentity::Unix {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

pub(crate) fn same_cap_directory_volume(left: &File, right: &File) -> Result<bool> {
    let left = cap_fs::Metadata::from_file(left).pipeline_io(Path::new("."))?;
    let right = cap_fs::Metadata::from_file(right).pipeline_io(Path::new("."))?;

    {
        use cap_fs::MetadataExt;
        Ok(left.dev() == right.dev())
    }
}

pub(crate) fn same_cap_directory_identity(
    left: &cap_fs::Metadata,
    right: &cap_fs::Metadata,
) -> bool {
    use cap_fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

pub(crate) fn same_bound_file_identity(left: &cap_fs::Metadata, right: &cap_fs::Metadata) -> bool {
    use cap_fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

pub(crate) fn same_bound_file_state(left: &cap_fs::Metadata, right: &cap_fs::Metadata) -> bool {
    use cap_fs::MetadataExt;
    same_bound_file_identity(left, right)
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

#[derive(Debug)]
pub(crate) struct OpenedFileState {
    metadata: Metadata,
}

impl OpenedFileState {
    pub(crate) fn len(&self) -> u64 {
        self.metadata.len()
    }
}

pub(crate) fn open_verified_regular_file(path: &Path) -> Result<(File, OpenedFileState)> {
    {
        // `symlink_metadata` does not follow the final component. Comparing its identity
        // with the opened handle closes the swap-to-symlink interval without requiring a
        // later pathname read.
        let listed = std::fs::symlink_metadata(path).pipeline_io(path)?;
        if !listed.file_type().is_file() {
            return Err(crate::PipelineError::contract(
                "KIO-E-SCAN-FILE-IDENTITY-001",
                format!("scan candidate is not a regular file: {}", path.display()),
            ));
        }
        let file = File::open(path).pipeline_io(path)?;
        verify_opened_regular_file(path, listed, file)
    }
}

pub(crate) fn verify_opened_regular_file(
    path: &Path,
    listed: Metadata,
    file: File,
) -> Result<(File, OpenedFileState)> {
    let opened = file.metadata().pipeline_io(path)?;
    if !opened.is_file() || !same_file_identity(&listed, &opened) {
        return Err(crate::PipelineError::contract(
            "KIO-E-SCAN-FILE-IDENTITY-001",
            format!(
                "scan candidate changed while it was being opened: {}",
                path.display()
            ),
        ));
    }
    Ok((file, OpenedFileState { metadata: opened }))
}

pub(crate) fn ensure_file_unchanged(
    file: &File,
    before: &OpenedFileState,
    path: &Path,
) -> Result<()> {
    let unchanged = {
        let after = file.metadata().pipeline_io(path)?;
        same_file_state(&before.metadata, &after)
    };
    if !unchanged {
        return Err(crate::PipelineError::contract(
            "KIO-E-SCAN-INPUT-CHANGED-001",
            format!(
                "scan candidate changed while it was being read: {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn same_file_identity(left: &Metadata, right: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

fn same_file_state(left: &Metadata, right: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    same_file_identity(left, right)
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

const MAX_CHILD_SCOPE_DISCOVERY_RELATIVE_BYTES: usize = 128 * 1024;
pub(crate) fn validate_discovery_path_limit(path: &Path) -> Result<()> {
    if path.as_os_str().as_encoded_bytes().len() > MAX_CHILD_SCOPE_DISCOVERY_RELATIVE_BYTES {
        return Err(crate::PipelineError::contract(
            "KIO-E-CHILD-DISCOVERY-CONTINUATION-STALE-001",
            "child discovery path exceeds the supported token limit",
        ));
    }
    Ok(())
}
pub(crate) fn reject_child_directory(_child: &File) -> bool {
    false
}
