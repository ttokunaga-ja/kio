use crate::ledger::lifecycle_fs::capture_leaf;
use crate::ledger::lifecycle_fs::{
    FsSession, LIFECYCLE_LOCK, LOCK_BYTES, MAX_LOCK_BYTES, SqliteFiles, boundary_changed, contract,
    core_private,
};
use crate::{PipelineError, Result};
use kio_core::store_dir::Publication;
use kio_core::store_dir::StoreDirectory;
use rusqlite::OpenFlags;
use std::{
    fs::File,
    path::{Component, Path, PathBuf},
};
pub(crate) type DirectoryIdentity = kio_core::platform::DirectoryHandleIdentity;
pub(crate) type RegularFileIdentity = kio_core::platform::RegularFileHandleIdentity;

pub(crate) fn directory_identity(file: &File, path: &Path) -> Result<DirectoryIdentity> {
    kio_core::platform::directory_handle_identity(file).ok_or_else(|| {
        contract(
            "KIO-E-LEDGER-PRIVATE-001",
            format!(
                "ledger parent is reparse or non-directory: {}",
                path.display()
            ),
        )
    })
}

pub(crate) fn regular_file_identity(file: &File, path: &Path) -> Result<RegularFileIdentity> {
    kio_core::platform::regular_file_handle_identity(file).ok_or_else(|| {
        contract(
            "KIO-E-LEDGER-PRIVATE-001",
            format!(
                "ledger leaf is reparse, nonregular, or hardlinked: {}",
                path.display()
            ),
        )
    })
}

pub(crate) fn verify_private_parent_handle(file: &File, path: &Path) -> Result<()> {
    directory_identity(file, path)?;
    kio_core::platform::verify_owner_private_handle(file).map_err(core_private)
}

fn windows_change_time(file: &File, path: &Path) -> Result<i64> {
    kio_core::platform::file_change_time(file).map_err(|error| PipelineError::Io {
        path: path.display().to_string(),
        message: error.to_string(),
    })
}

pub(crate) fn verify_existing_ancestor_path(
    path: &Path,
    control: &kio_core::control::ControlHandle,
) -> Result<()> {
    // StoreDirectory already opened every component without traversing a
    // reparse point. The newly created final parent receives the protected ACL
    // and is checked through its retained handle before lock publication.
    StoreDirectory::open_with_control(path, control.clone())
        .map(|_| ())
        .map_err(core_private)
}

pub(crate) fn revalidate_lock_contents(
    parent: &StoreDirectory,
    lock: &File,
    lock_identity: &RegularFileIdentity,
    parent_path: &Path,
) -> Result<()> {
    use std::os::windows::fs::FileExt;

    revalidate_retained_lock_binding(parent, lock_identity, parent_path)?;
    let path = parent_path.join(LIFECYCLE_LOCK);
    let before_identity = regular_file_identity(lock, &path)?;
    kio_core::platform::verify_owner_private_handle(lock).map_err(core_private)?;
    let before = lock.metadata().map_err(|error| PipelineError::Io {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    let before_change_time = windows_change_time(lock, &path)?;
    if before_identity != *lock_identity || before.len() != LOCK_BYTES.len() as u64 {
        return Err(boundary_changed("ledger lifecycle lock contents changed"));
    }

    // Never reopen or duplicate the handle and never release its lock.
    // Explicit offsets make repeated checks independent of the file cursor;
    // the extra byte detects growth without an unbounded allocation/read.
    let mut body = [0_u8; LOCK_BYTES.len() + 1];
    let mut read = 0;
    while read < body.len() {
        match lock.seek_read(&mut body[read..], read as u64) {
            Ok(0) => break,
            Ok(count) => read += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(PipelineError::Io {
                    path: path.display().to_string(),
                    message: error.to_string(),
                });
            }
        }
    }

    let after_identity = regular_file_identity(lock, &path)?;
    let after = lock.metadata().map_err(|error| PipelineError::Io {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    let after_change_time = windows_change_time(lock, &path)?;
    kio_core::platform::verify_owner_private_handle(lock).map_err(core_private)?;
    revalidate_retained_lock_binding(parent, lock_identity, parent_path)?;
    if before_identity != after_identity
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
        || before_change_time != after_change_time
        || after.len() != read as u64
    {
        return Err(boundary_changed(
            "ledger lifecycle lock changed while it was read",
        ));
    }
    if &body[..read] != LOCK_BYTES {
        return Err(boundary_changed("ledger lifecycle lock contents changed"));
    }
    Ok(())
}

fn revalidate_retained_lock_binding(
    parent: &StoreDirectory,
    lock_identity: &RegularFileIdentity,
    parent_path: &Path,
) -> Result<()> {
    verify_private_parent_handle(parent.root_handle().as_ref(), parent_path)?;
    // Open only for metadata and ACL checks, relative to the retained
    // parent. Data reads must continue through the handle owning the lock.
    let named_lock = parent
        .open_regular_read(Path::new(LIFECYCLE_LOCK), MAX_LOCK_BYTES)
        .map_err(core_private)?;
    let identity = regular_file_identity(&named_lock, &parent_path.join(LIFECYCLE_LOCK))?;
    kio_core::platform::verify_owner_private_handle(&named_lock).map_err(core_private)?;
    if identity != *lock_identity {
        return Err(boundary_changed(
            "retained parent lifecycle name no longer names the retained lock",
        ));
    }
    Ok(())
}
pub(crate) fn prepare_sqlite_sidecars(
    session: &FsSession,
    files: &mut SqliteFiles,
    flags: OpenFlags,
) -> Result<()> {
    if !flags.contains(OpenFlags::SQLITE_OPEN_READ_WRITE) {
        return Ok(());
    }
    // All existing SQLite leaves were checked before reaching this point.
    // SQLite's Windows VFS otherwise creates these files with the token's
    // default security descriptor. OPEN_ALWAYS reuses our private files.
    // Publication is confined to absent coordination files, never repairs
    // an existing leaf, and leaves their bindings pinned before SQLite opens.
    for (leaf, retained) in [
        (&session.names().wal, &mut files.wal),
        (&session.names().shm, &mut files.shm),
    ] {
        if retained.is_none() {
            session.revalidate_boundary()?;
            session
                .parent()
                .write_atomic(leaf, b"", Publication::CreateOnly)
                .map_err(core_private)?;
            *retained =
                Some(capture_leaf(session, leaf)?.ok_or_else(|| {
                    boundary_changed("newly published SQLite sidecar is missing")
                })?);
            session.revalidate_boundary()?;
        }
    }
    Ok(())
}

pub(crate) fn normalized_absolute_db_path(path: &Path) -> Result<PathBuf> {
    Ok(path.to_path_buf())
}
pub(crate) fn sqlite_path_from_retained_parent(
    _parent: &StoreDirectory,
    _leaf: &Path,
) -> Result<PathBuf> {
    Err(contract(
        "KIO-E-LEDGER-PRIVATE-001",
        "inherited ledger roots are unsupported on this platform",
    ))
}

pub(crate) fn try_inherited_parent(
    _path: &Path,
    _allow_create: bool,
    _control: kio_core::control::ControlHandle,
) -> Result<Option<StoreDirectory>> {
    Ok(None)
}
pub(crate) fn path_component_is_unsafe(component: Component<'_>) -> bool {
    matches!(component, Component::CurDir | Component::ParentDir)
}
pub(crate) fn validate_lock_contents_before_acquire(_parent: &StoreDirectory) -> Result<()> {
    Ok(())
}
pub(crate) fn validate_open_lock_before_acquire(lock: &File) -> Result<()> {
    kio_core::platform::verify_owner_private_handle(lock).map_err(core_private)
}
