use super::unix_cache::BoundCache;
use super::unix_cache::{
    BoundCacheParent, bind_cache_parent_unchecked, cache_file_identity,
    open_or_create_ambient_cache_parent,
};
use super::{BoundSqliteConnection, linux_bound_sqlite};
use crate::Result;
use rusqlite::OpenFlags;
use std::path::{Path, PathBuf};
fn validate_linux_cache_path_lexical(path: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let raw = path.as_os_str().as_bytes();
    if raw.windows(2).any(|window| window == b"//")
        || (raw.len() > 1 && raw.ends_with(b"/"))
        || raw
            .split(|byte| *byte == b'/')
            .any(|component| component == b"." || component == b"..")
    {
        return Err(crate::IndexError::Schema(format!(
            "aggregator cache path is not canonical (lexical): {}",
            path.display()
        )));
    }
    Ok(())
}
pub(super) fn bind_cache_parent(
    path: &Path,
    create_parent: bool,
    control: &kio_core::control::ControlHandle,
) -> Result<BoundCacheParent> {
    validate_linux_cache_path_lexical(path)?;
    bind_cache_parent_unchecked(path, create_parent, control)
}
pub(super) fn open_or_create_cache_parent(
    path: &Path,
    create_missing: bool,
    control: &kio_core::control::ControlHandle,
) -> Result<(std::fs::File, PathBuf)> {
    if let Some(bound) = open_or_create_inherited_cache_parent(path, create_missing, control)? {
        return Ok(bound);
    }
    open_or_create_ambient_cache_parent(path, create_missing, control)
}
fn open_or_create_inherited_cache_parent(
    path: &Path,
    create_missing: bool,
    control: &kio_core::control::ControlHandle,
) -> Result<Option<(std::fs::File, std::path::PathBuf)>> {
    let Some((mut current, suffix)) =
        kio_core::private_fs::resolve_inherited_private_root_with_control(path, control).map_err(
            |error| {
                crate::IndexError::Schema(format!(
                    "resolve inherited aggregator cache capability: {error}"
                ))
            },
        )?
    else {
        return Ok(None);
    };
    let mut logical = current.path().to_path_buf();
    for component in suffix {
        let leaf = Path::new(&component);
        logical.push(&component);
        let next = if current.contains_entry(leaf).map_err(|error| {
            crate::IndexError::Schema(format!(
                "inspect inherited aggregator cache directory {}: {error}",
                logical.display()
            ))
        })? {
            current.open_directory(leaf).map_err(|error| {
                crate::IndexError::Schema(format!(
                    "open inherited aggregator cache directory {} without following links: {error}",
                    logical.display()
                ))
            })?
        } else if create_missing {
            current.create_directory(leaf).map_err(|error| {
                crate::IndexError::Schema(format!(
                    "create inherited private aggregator cache directory {}: {error}",
                    logical.display()
                ))
            })?
        } else {
            return Err(crate::IndexError::Schema(format!(
                "inherited aggregator cache directory is missing: {}",
                logical.display()
            )));
        };
        current = kio_core::store_dir::StoreDirectory::from_retained_with_control(
            next,
            logical.clone(),
            control.clone(),
        )
        .map_err(|error| {
            crate::IndexError::Schema(format!(
                "retain inherited aggregator cache directory {}: {error}",
                logical.display()
            ))
        })?;
        verify_private_cache_directory(current.root_handle().as_ref(), &logical)?;
    }
    let retained = current.root_handle().try_clone().map_err(|error| {
        crate::IndexError::Schema(format!(
            "retain inherited aggregator cache parent {}: {error}",
            path.display()
        ))
    })?;
    Ok(Some((retained, path.to_path_buf())))
}
fn verify_private_cache_directory(file: &std::fs::File, path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata().map_err(|error| {
        crate::IndexError::Schema(format!(
            "inspect inherited aggregator cache directory {}: {error}",
            path.display()
        ))
    })?;
    // SAFETY: geteuid has no arguments and no memory-safety preconditions.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(crate::IndexError::Schema(format!(
            "inherited aggregator cache directory is not owner-private: {}",
            path.display()
        )));
    }
    Ok(())
}
pub(crate) fn open_bound_cache_connection(
    cache: &BoundCache,
    flags: OpenFlags,
) -> Result<BoundSqliteConnection> {
    let leaf = cache
        .public_path
        .file_name()
        .expect("bound cache has a file name");
    linux_bound_sqlite::connection(
        &cache._parent,
        leaf,
        cache_file_identity(&cache.file)?,
        flags,
    )
}
