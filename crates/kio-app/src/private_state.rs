//! Retained owner-private device-state directories shared by mutable stores.

use std::fs::File;
use std::path::{Path, PathBuf};

use kio_core::store_dir::{Publication, StoreDirectory};
use kio_core::{ExitCode, KioError, Result};

#[derive(Debug)]
pub(crate) struct PrivateState {
    directory: StoreDirectory,
    lock: File,
}

impl Drop for PrivateState {
    fn drop(&mut self) {
        let _ = self.lock.unlock();
    }
}

impl PrivateState {
    pub(crate) fn directory(&self) -> &StoreDirectory {
        &self.directory
    }
}

pub(crate) fn open_readonly(path: &Path) -> Result<Option<StoreDirectory>> {
    if let Some((root, suffix)) = kio_core::private_fs::resolve_inherited_private_root(path)? {
        return retained_private_directory(root, &suffix, false)
            .map(|result| result.map(|(directory, _)| directory));
    }
    match kio_core::private_fs::verify_private_directory(path) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) => {
            // Only an actually absent component beneath a verified creation
            // parent is absence.  A broken or unsafe path must remain an
            // error; `Path::exists` would collapse both cases into `false`.
            let (_, ancestor, _) = nearest_existing_ancestor(path)?;
            if ancestor == path {
                Err(error)
            } else {
                Ok(None)
            }
        }
    }
}

/// Open and lock an existing private state directory without creating or
/// repairing any filesystem entry.
pub(crate) fn open_existing(path: &Path, lock_bytes: &[u8]) -> Result<Option<PrivateState>> {
    let Some(directory) = open_readonly(path)? else {
        return Ok(None);
    };
    let lock = acquire_existing_lock(&directory, lock_bytes)?;
    Ok(Some(PrivateState { directory, lock }))
}

pub(crate) fn open_or_create(path: &Path, lock_bytes: &[u8]) -> Result<PrivateState> {
    let (directory, created) =
        if let Some((root, suffix)) = kio_core::private_fs::resolve_inherited_private_root(path)? {
            retained_private_directory(root, &suffix, true)?.ok_or_else(|| {
                KioError::invalid_usage("private state creation returned no directory")
            })?
        } else {
            match kio_core::private_fs::verify_private_directory(path) {
                Ok(directory) => (directory, false),
                Err(error) => {
                    let (ancestor_directory, ancestor, missing) = nearest_existing_ancestor(path)?;
                    if ancestor == path {
                        return Err(error);
                    }
                    (
                        create_private_directory(ancestor_directory, ancestor, missing, path)?,
                        true,
                    )
                }
            }
        };
    // The lock leaf is initialized only with a newly created store. An
    // existing store with a missing/unsafe lock is corrupt rather than a
    // permission-repair opportunity.
    let lock = if created {
        acquire_lock(&directory, lock_bytes)?
    } else {
        acquire_existing_lock(&directory, lock_bytes)?
    };
    Ok(PrivateState { directory, lock })
}

/// Consume an inherited root once. Its numeric descriptor and diagnostic path
/// are never resolved again while traversing or creating the suffix.
fn retained_private_directory(
    mut directory: StoreDirectory,
    suffix: &[std::ffi::OsString],
    create: bool,
) -> Result<Option<(StoreDirectory, bool)>> {
    let mut final_created = false;
    for leaf in suffix {
        let relative = Path::new(leaf);
        let exists = directory.contains_entry(relative)?;
        if !exists && !create {
            return Ok(None);
        }
        final_created = false;
        let handle = if exists {
            directory.open_directory(relative)?
        } else {
            match directory.create_directory(relative) {
                Ok(handle) => {
                    final_created = true;
                    handle
                }
                Err(create_error) => directory
                    .open_directory(relative)
                    .map_err(|_| create_error)?,
            }
        };
        let child = StoreDirectory::from_retained(handle, directory.path().join(leaf))?;
        kio_core::private_fs::verify_private_directory_handle(&child)?;
        directory = child;
    }
    Ok(Some((directory, final_created)))
}

/// Find the closest name that exists without following a final symlink, then
/// validate it as a safe creation parent.  Returning a retained parent avoids
/// authorizing creation from an unchecked path lookup.
fn nearest_existing_ancestor(
    path: &Path,
) -> Result<(StoreDirectory, PathBuf, Vec<std::ffi::OsString>)> {
    if !path.is_absolute() {
        return Err(KioError::invalid_usage(
            "private state directory must be absolute",
        ));
    }
    let mut current = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match std::fs::symlink_metadata(&current) {
            Ok(_) => {
                let directory = kio_core::private_fs::verify_private_creation_parent(&current)?;
                missing.reverse();
                return Ok((directory, current, missing));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let leaf = current
                    .file_name()
                    .ok_or_else(|| KioError::invalid_usage("private state directory has no leaf"))?
                    .to_owned();
                missing.push(leaf);
                current = current
                    .parent()
                    .ok_or_else(|| {
                        KioError::invalid_usage("private state has no existing ancestor")
                    })?
                    .to_path_buf();
            }
            Err(error) => {
                return Err(KioError::io(
                    error.to_string(),
                    current.display().to_string(),
                ));
            }
        }
    }
}

fn create_private_directory(
    mut directory: StoreDirectory,
    ancestor: PathBuf,
    missing: Vec<std::ffi::OsString>,
    requested: &Path,
) -> Result<StoreDirectory> {
    let mut current_path = ancestor;
    for leaf in missing {
        current_path.push(&leaf);
        // `create_directory` is exclusive and restricts only the handle it
        // created.  If another writer won the name, strictly re-verify that
        // exact directory instead of repairing its permissions.
        directory = match directory.create_directory(Path::new(&leaf)) {
            Ok(created) => StoreDirectory::from_retained(created, current_path.clone())?,
            Err(create_error) => {
                match kio_core::private_fs::verify_private_directory(&current_path) {
                    Ok(existing) => existing,
                    Err(_) => return Err(create_error),
                }
            }
        };
    }
    if current_path != requested {
        return Err(KioError::invalid_usage(
            "private state directory creation did not reach requested path",
        ));
    }
    verify_named_identity(&directory, requested)?;
    Ok(directory)
}

/// The final name must still identify the retained created directory before it
/// can become a mutable store. This detects a rename-and-replace race without
/// falling back to path-relative I/O after the check.
#[cfg(unix)]
fn verify_named_identity(directory: &StoreDirectory, path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

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

#[cfg(windows)]
fn verify_named_identity(directory: &StoreDirectory, path: &Path) -> Result<()> {
    let named = kio_core::cas::windows_real_directory_identity(path)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?
        .ok_or_else(|| KioError::invalid_usage("private state path is not a real directory"))?;
    let retained = kio_core::cas::windows_directory_handle_identity(&directory.root_handle())
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

#[cfg(not(any(unix, windows)))]
fn verify_named_identity(_directory: &StoreDirectory, _path: &Path) -> Result<()> {
    Err(KioError::invalid_usage(
        "private state directory identity is unsupported on this platform",
    ))
}

fn acquire_existing_lock(directory: &StoreDirectory, lock_bytes: &[u8]) -> Result<File> {
    let leaf = Path::new(".lock");
    // Existing-only callers (runtime admission and lifecycle mutation) never
    // repair a missing lock leaf. A registered store creates it once through
    // `open_or_create`; disappearance is a tamper/corruption error.
    directory.ensure_owner_private(leaf)?;
    lock_checked(directory, lock_bytes)
}

fn acquire_lock(directory: &StoreDirectory, lock_bytes: &[u8]) -> Result<File> {
    let leaf = Path::new(".lock");
    if directory.read_optional(leaf, 256)?.is_none() {
        let _ = directory.write_atomic(leaf, lock_bytes, Publication::CreateOnly);
    }
    directory.ensure_owner_private(leaf)?;
    lock_checked(directory, lock_bytes)
}

fn lock_checked(directory: &StoreDirectory, lock_bytes: &[u8]) -> Result<File> {
    let leaf = Path::new(".lock");
    let bytes = directory
        .read_optional(leaf, 256)?
        .ok_or_else(|| KioError::invalid_usage("private state lock disappeared"))?;
    if bytes != lock_bytes {
        return Err(KioError::new(
            "KIO-E-PRIVATE-STATE-LOCK-001",
            "private state lock has unexpected contents",
            serde_json::json!({}),
            ExitCode::PermanentFailure,
        ));
    }
    let file = directory.open_regular_read(leaf, 256)?;
    file.try_lock().map_err(|error| {
        KioError::new(
            "KIO-E-PRIVATE-STATE-LOCKED-001",
            error.to_string(),
            serde_json::json!({}),
            ExitCode::PartialFailure,
        )
    })?;
    Ok(file)
}

/// Atomically publish a newly initialized private store. The callback writes
/// all mandatory initial leaves through a staging directory before the final
/// name exists; an interruption can therefore expose either no store or a
/// complete, lockable initial store.
pub(crate) fn create_initialized(
    path: &Path,
    lock_bytes: &[u8],
    initialize: impl FnOnce(&StoreDirectory) -> Result<()>,
) -> Result<PrivateState> {
    let parent_path = path
        .parent()
        .ok_or_else(|| KioError::invalid_usage("private state path has no parent"))?;
    let parent = ensure_private_directory(parent_path)?;
    let leaf = path
        .file_name()
        .ok_or_else(|| KioError::invalid_usage("private state path has no leaf"))?;
    if parent.contains_entry(Path::new(leaf))? {
        return Err(KioError::invalid_usage(
            "private state store already exists",
        ));
    }
    let stage_leaf = std::ffi::OsString::from(format!(
        ".{}.staging-{}-{}",
        leaf.to_string_lossy(),
        std::process::id(),
        STAGE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));
    let stage_handle = parent.create_directory(Path::new(&stage_leaf))?;
    let stage_path = parent_path.join(&stage_leaf);
    let stage = StoreDirectory::from_retained(stage_handle, stage_path)?;
    initialize_lock(&stage, lock_bytes)?;
    initialize(&stage)?;
    stage.sync()?;
    parent.rename_directory_create_only(Path::new(&stage_leaf), Path::new(leaf))?;
    // Keep the initialized directory itself after publication. The rename
    // helper checks the published name; reopening it here could select a
    // different directory after that check.
    let published = stage
        .root_handle()
        .try_clone()
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
    let directory = StoreDirectory::from_retained(published, path.to_path_buf())?;
    kio_core::private_fs::verify_private_directory_handle(&directory)?;
    let lock = acquire_existing_lock(&directory, lock_bytes)?;
    Ok(PrivateState { directory, lock })
}

static STAGE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn ensure_private_directory(path: &Path) -> Result<StoreDirectory> {
    if let Some((root, suffix)) = kio_core::private_fs::resolve_inherited_private_root(path)? {
        return retained_private_directory(root, &suffix, true)?
            .map(|(directory, _)| directory)
            .ok_or_else(|| {
                KioError::invalid_usage("private directory creation returned no directory")
            });
    }
    match kio_core::private_fs::verify_private_directory(path) {
        Ok(directory) => Ok(directory),
        Err(error) => {
            let (ancestor_directory, ancestor, missing) = nearest_existing_ancestor(path)?;
            if ancestor == path {
                return Err(error);
            }
            create_private_directory(ancestor_directory, ancestor, missing, path)
        }
    }
}

fn initialize_lock(directory: &StoreDirectory, lock_bytes: &[u8]) -> Result<()> {
    directory.write_atomic(Path::new(".lock"), lock_bytes, Publication::CreateOnly)?;
    directory.ensure_owner_private(Path::new(".lock"))?;
    directory.sync()
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs::{self, File};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};

    use kio_core::store_dir::Publication;

    use super::{create_initialized, open_existing, open_readonly, retained_private_directory};

    fn private_tempdir() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    fn inherited_path(directory: &File, suffix: &str) -> PathBuf {
        PathBuf::from(format!("/dev/fd/{}/{}", directory.as_raw_fd(), suffix))
    }

    #[test]
    fn inherited_root_remains_bound_after_fd_replacement_and_path_rename() {
        let original = private_tempdir();
        let replacement = private_tempdir();
        let moved = original.path().with_extension("retained");
        let original_handle = File::open(original.path()).unwrap();
        let inherited = inherited_path(&original_handle, "state/nested");
        let (root, suffix) = kio_core::private_fs::resolve_inherited_private_root(&inherited)
            .unwrap()
            .expect("canonical inherited path must resolve");

        // The resolver already duplicated `original_handle`.  Both replacing
        // its numeric descriptor and moving its old name must therefore leave
        // the retained capability bound to the original directory.
        fs::rename(original.path(), &moved).unwrap();
        fs::create_dir(original.path()).unwrap();
        fs::set_permissions(original.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let replacement_handle = File::open(replacement.path()).unwrap();
        assert!(
            unsafe { libc::dup2(replacement_handle.as_raw_fd(), original_handle.as_raw_fd()) } >= 0
        );

        let (directory, _) = retained_private_directory(root, &suffix, true)
            .unwrap()
            .expect("creation through retained inherited root");
        directory
            .write_atomic(Path::new("record"), b"original", Publication::CreateOnly)
            .unwrap();

        assert_eq!(
            fs::read(moved.join("state/nested/record")).unwrap(),
            b"original"
        );
        assert!(!original.path().join("state").exists());
        assert!(!replacement.path().join("state").exists());
        drop(directory);
        fs::remove_dir_all(&moved).unwrap();
    }

    #[test]
    fn inherited_symlink_suffix_refuses_without_touching_the_victim() {
        let root = private_tempdir();
        let victim = private_tempdir();
        let handle = File::open(root.path()).unwrap();
        symlink(victim.path(), root.path().join("state")).unwrap();
        let inherited = inherited_path(&handle, "state/child");
        let (retained, suffix) = kio_core::private_fs::resolve_inherited_private_root(&inherited)
            .unwrap()
            .expect("canonical inherited path must resolve");

        assert!(retained_private_directory(retained, &suffix, true).is_err());
        assert!(!victim.path().join("child").exists());
    }

    #[test]
    fn inherited_readonly_missing_suffix_creates_nothing() {
        let root = private_tempdir();
        let handle = File::open(root.path()).unwrap();
        let inherited = inherited_path(&handle, "missing/state");
        let (retained, suffix) = kio_core::private_fs::resolve_inherited_private_root(&inherited)
            .unwrap()
            .expect("canonical inherited path must resolve");

        assert!(
            retained_private_directory(retained, &suffix, false)
                .unwrap()
                .is_none()
        );
        assert!(open_readonly(&inherited).unwrap().is_none());
        assert!(!root.path().join("missing").exists());
    }

    #[test]
    fn inherited_create_initialized_publishes_lock_and_record() {
        let root = private_tempdir();
        let handle = File::open(root.path()).unwrap();
        let inherited = inherited_path(&handle, "trust");
        let state = create_initialized(&inherited, b"private-state-test-lock", |directory| {
            directory.write_atomic(Path::new("record.json"), b"{}", Publication::CreateOnly)
        })
        .unwrap();
        assert_eq!(
            state
                .directory()
                .read_optional(Path::new("record.json"), 1024)
                .unwrap(),
            Some(b"{}".to_vec())
        );
        drop(state);

        assert_eq!(
            fs::read(root.path().join("trust/.lock")).unwrap(),
            b"private-state-test-lock"
        );
        assert_eq!(
            fs::read(root.path().join("trust/record.json")).unwrap(),
            b"{}"
        );
        assert!(
            open_existing(&inherited, b"private-state-test-lock")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn inherited_public_root_is_rejected_before_store_creation() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let handle = File::open(root.path()).unwrap();
        let inherited = inherited_path(&handle, "state");

        assert!(kio_core::private_fs::resolve_inherited_private_root(&inherited).is_err());
        assert!(super::open_or_create(&inherited, b"lock").is_err());
        assert!(!root.path().join("state").exists());
    }
}
