use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::{IoResultExt, PipelineError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StorePathKind {
    Directory,
    RegularFile,
}

/// Resolve an existing store-relative path without accepting symlinks at any
/// level below the canonical Kio directory. Missing suffixes are reported as
/// `None` so callers can validate references before their output is created.
pub(crate) fn resolve_existing_store_path(
    kio_dir: &Path,
    relative: &Path,
    final_kind: StorePathKind,
) -> Result<Option<PathBuf>> {
    let components = relative.components().collect::<Vec<_>>();
    if components.is_empty()
        || components
            .iter()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(unsafe_store_path(
            relative,
            "store path is not a non-empty relative path",
        ));
    }

    let root_metadata = fs::symlink_metadata(kio_dir).pipeline_io(kio_dir)?;
    if root_metadata.file_type().is_symlink() || !root_metadata.file_type().is_dir() {
        return Err(unsafe_store_path(
            kio_dir,
            "Kio store root is not a real directory",
        ));
    }
    let canonical_root = kio_dir.canonicalize().pipeline_io(kio_dir)?;

    let mut current = kio_dir.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).pipeline_io(&current),
        };
        let is_last = index + 1 == components.len();
        let expected_type = if is_last {
            match final_kind {
                StorePathKind::Directory => metadata.file_type().is_dir(),
                StorePathKind::RegularFile => metadata.file_type().is_file(),
            }
        } else {
            metadata.file_type().is_dir()
        };
        if metadata.file_type().is_symlink() || !expected_type {
            return Err(unsafe_store_path(
                &current,
                if is_last {
                    "store object has an unexpected filesystem type"
                } else {
                    "store object ancestor is not a real directory"
                },
            ));
        }

        let canonical = current.canonicalize().pipeline_io(&current)?;
        if !canonical.starts_with(&canonical_root) {
            return Err(unsafe_store_path(
                &current,
                "store path resolves outside the canonical Kio directory",
            ));
        }
        if is_last {
            return Ok(Some(canonical));
        }
    }

    unreachable!("non-empty component list returns from the loop")
}

/// Create a store-relative directory one component at a time, rejecting every
/// pre-existing symlink or non-directory before descending through it.
fn unsafe_store_path(path: &Path, message: impl Into<String>) -> PipelineError {
    PipelineError::corrupt(path.display().to_string(), message)
}
