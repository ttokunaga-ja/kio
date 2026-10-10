pub(crate) use super::unix_ledger_lifecycle::*;
use crate::Result;
use crate::ledger::lifecycle_fs::{contract, core_private, require_leaf};
use kio_core::store_dir::StoreDirectory;
use std::path::{Path, PathBuf};
pub(crate) fn try_inherited_parent(
    path: &Path,
    allow_create: bool,
    control: kio_core::control::ControlHandle,
) -> Result<Option<StoreDirectory>> {
    if let Some((root, suffix)) =
        kio_core::private_fs::resolve_inherited_private_root_with_control(path, &control)
            .map_err(core_private)?
    {
        return open_or_create_inherited_parent(root, &suffix, path, allow_create).map(Some);
    }
    Ok(None)
}
/// Traverse only the validated suffix of an inherited private root. The root
/// descriptor was duplicated before this function received it; no operation
/// below reimports the original `/dev/fd/N` spelling.
fn open_or_create_inherited_parent(
    mut current: StoreDirectory,
    suffix: &[std::ffi::OsString],
    path: &Path,
    allow_create: bool,
) -> Result<StoreDirectory> {
    verify_private_parent_handle(current.root_handle().as_ref(), current.path())?;
    let mut logical = current.path().to_path_buf();
    for name in suffix {
        let leaf = Path::new(name);
        logical.push(name);
        let retained = if current.contains_entry(leaf).map_err(core_private)? {
            current.open_directory(leaf).map_err(core_private)?
        } else {
            if !allow_create {
                return Err(contract(
                    "KIO-E-LEDGER-UNINITIALIZED-001",
                    format!("ledger parent is missing: {}", logical.display()),
                ));
            }
            current.create_directory(leaf).map_err(core_private)?
        };
        let next = StoreDirectory::from_retained_with_control(
            retained,
            logical.clone(),
            current.control().clone(),
        )
        .map_err(core_private)?;
        verify_private_parent_handle(next.root_handle().as_ref(), &logical)?;
        current = next;
    }
    // `path` is retained only for precise diagnostics: the actual authority is
    // `current`'s duplicated descriptor-relative handle.
    verify_private_parent_handle(current.root_handle().as_ref(), path)?;
    Ok(current)
}

/// SQLite accepts only a path. On Linux inherited roots are retained directory
/// capabilities, so address the main leaf through this process's *duplicated*
/// descriptor. The descriptor remains owned by `FsSession` for the complete
/// SQLite lifetime; a caller replacing its original `/dev/fd/N` cannot retarget
/// this path.
pub(crate) fn sqlite_path_from_retained_parent(
    parent: &StoreDirectory,
    leaf: &Path,
) -> Result<PathBuf> {
    use std::os::fd::AsRawFd;

    require_leaf(leaf)?;
    Ok(PathBuf::from(format!(
        "/proc/self/fd/{}",
        parent.root_handle().as_raw_fd()
    ))
    .join(leaf))
}

pub(crate) fn normalized_absolute_db_path(path: &Path) -> Result<PathBuf> {
    Ok(path.to_path_buf())
}
