//! Retained owner-private device-state directories shared by mutable stores.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use kio_core::control::ControlHandle;
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

pub(crate) fn open_readonly(
    path: &Path,
    external_base: &Path,
    control: &ControlHandle,
) -> Result<Option<StoreDirectory>> {
    let Some((root, suffix)) = private_boundary(path, external_base, false, control)? else {
        return Ok(None);
    };
    retained_private_directory(root, &suffix, false)
        .map(|result| result.map(|(directory, _)| directory))
}

/// Open and lock an existing private state directory without creating or
/// repairing any filesystem entry.
pub(crate) fn open_existing(
    path: &Path,
    external_base: &Path,
    lock_bytes: &[u8],
    control: &ControlHandle,
) -> Result<Option<PrivateState>> {
    let Some(directory) = open_readonly(path, external_base, control)? else {
        return Ok(None);
    };
    let lock = acquire_existing_lock(&directory, lock_bytes)?;
    Ok(Some(PrivateState { directory, lock }))
}

pub(crate) fn open_or_create(
    path: &Path,
    external_base: &Path,
    lock_bytes: &[u8],
    control: &ControlHandle,
) -> Result<PrivateState> {
    let (root, suffix) =
        private_boundary(path, external_base, true, control)?.ok_or_else(|| {
            KioError::invalid_usage("private state external directory is unavailable")
        })?;
    let (directory, created) = retained_private_directory(root, &suffix, true)?
        .ok_or_else(|| KioError::invalid_usage("private state creation returned no directory"))?;
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

/// Consume the retained external root once. Its diagnostic path is never
/// resolved again while traversing or creating the Kio-owned suffix.
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
        let child = StoreDirectory::from_retained_with_control(
            handle,
            directory.path().join(leaf),
            directory.control().clone(),
        )?;
        kio_core::private_fs::verify_private_directory_handle(&child)?;
        directory = child;
    }
    kio_core::private_fs::verify_private_directory_handle(&directory)?;
    Ok(Some((directory, final_created)))
}

fn owned_suffix(path: &Path, external_base: &Path) -> Result<Vec<std::ffi::OsString>> {
    if !path.is_absolute() || !external_base.is_absolute() {
        return Err(KioError::invalid_usage(
            "private state and external base must be absolute",
        ));
    }
    let relative = path.strip_prefix(external_base).map_err(|_| {
        KioError::invalid_usage("private state is outside its declared external base")
    })?;
    let mut suffix = Vec::new();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(KioError::invalid_usage(
                "private state suffix must contain only normal components",
            ));
        };
        suffix.push(name.to_os_string());
    }
    if suffix.is_empty() {
        return Err(KioError::invalid_usage("private state suffix is empty"));
    }
    Ok(suffix)
}

/// The inherited descriptor spelling has a closed raw grammar. Resolve the
/// complete spelling once and carry the duplicated descriptor into the suffix
/// walk; parsing only the base would let Path::components normalize `./` or
/// repeated separators in an inherited owned path.
fn private_boundary(
    path: &Path,
    external_base: &Path,
    create: bool,
    control: &ControlHandle,
) -> Result<Option<(StoreDirectory, Vec<std::ffi::OsString>)>> {
    if let Some((root, suffix)) =
        kio_core::private_fs::resolve_inherited_private_root_with_control(path, control)?
    {
        if root.path().as_os_str().as_encoded_bytes()
            != external_base.as_os_str().as_encoded_bytes()
            || suffix.is_empty()
        {
            return Err(KioError::invalid_usage(
                "private state inherited boundary differs from declared external base",
            ));
        }
        return Ok(Some((root, suffix)));
    }
    let suffix = owned_suffix(path, external_base)?;
    Ok(external_boundary(external_base, create, control)?.map(|root| (root, suffix)))
}

/// A configured XDG base is external when it exists. If it does not yet exist,
/// retain its nearest existing ancestor and create only absent components with
/// Kio-private protection. An existing component is never adopted or repaired.
fn external_boundary(
    base: &Path,
    create: bool,
    control: &ControlHandle,
) -> Result<Option<StoreDirectory>> {
    if create {
        return kio_core::private_fs::retain_or_create_external_directory_with_control(
            base, control,
        )
        .map(Some);
    }
    match std::fs::symlink_metadata(base) {
        Ok(_) => {
            kio_core::private_fs::retain_external_directory_with_control(base, control).map(Some)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(KioError::io(error.to_string(), base.display().to_string())),
    }
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
    let mut file = directory.open_regular_read(leaf, 256)?;
    kio_core::private_fs::verify_private_regular_file_handle(&file)?;
    // Windows locks deny reads through a contender's separate handle. Acquire
    // ownership before inspecting the marker, then read this same handle.
    file.try_lock()
        .map_err(|error| lock_acquisition_error(error, &directory.path().join(leaf)))?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(257)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            KioError::io(
                error.to_string(),
                directory.path().join(leaf).display().to_string(),
            )
        })?;
    if bytes != lock_bytes {
        return Err(KioError::new(
            "KIO-E-PRIVATE-STATE-LOCK-001",
            "private state lock has unexpected contents",
            serde_json::json!({}),
            ExitCode::PermanentFailure,
        ));
    }
    Ok(file)
}

fn lock_acquisition_error(error: std::fs::TryLockError, path: &Path) -> KioError {
    match error {
        std::fs::TryLockError::WouldBlock => KioError::new(
            "KIO-E-PRIVATE-STATE-LOCKED-001",
            std::fs::TryLockError::WouldBlock.to_string(),
            serde_json::json!({}),
            ExitCode::PartialFailure,
        ),
        std::fs::TryLockError::Error(error) => {
            KioError::io(error.to_string(), path.display().to_string())
        }
    }
}

/// Atomically publish a newly initialized private store. The callback writes
/// all mandatory initial leaves through a staging directory before the final
/// name exists; an interruption can therefore expose either no store or a
/// complete, lockable initial store.
pub(crate) fn create_initialized(
    path: &Path,
    external_base: &Path,
    lock_bytes: &[u8],
    initialize: impl FnOnce(&StoreDirectory) -> Result<()>,
    control: &ControlHandle,
) -> Result<PrivateState> {
    let (root, suffix) =
        private_boundary(path, external_base, true, control)?.ok_or_else(|| {
            KioError::invalid_usage("private state external directory is unavailable")
        })?;
    let (leaf, parent_suffix) = suffix
        .split_last()
        .ok_or_else(|| KioError::invalid_usage("private state path has no owned leaf"))?;
    let parent_path = path
        .parent()
        .ok_or_else(|| KioError::invalid_usage("private state path has no parent"))?;
    let parent = if parent_suffix.is_empty() {
        root
    } else {
        retained_private_directory(root, parent_suffix, true)?
            .map(|(directory, _)| directory)
            .ok_or_else(|| KioError::invalid_usage("private state parent is unavailable"))?
    };
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
    let stage = StoreDirectory::from_retained_with_control(
        stage_handle,
        stage_path,
        parent.control().clone(),
    )?;
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
    let directory = StoreDirectory::from_retained_with_control(
        published,
        path.to_path_buf(),
        parent.control().clone(),
    )?;
    kio_core::private_fs::verify_private_directory_handle(&directory)?;
    let lock = acquire_existing_lock(&directory, lock_bytes)?;
    Ok(PrivateState { directory, lock })
}

static STAGE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn ensure_private_directory(
    path: &Path,
    external_base: &Path,
    control: &ControlHandle,
) -> Result<StoreDirectory> {
    let (root, suffix) =
        private_boundary(path, external_base, true, control)?.ok_or_else(|| {
            KioError::invalid_usage("private state external directory is unavailable")
        })?;
    retained_private_directory(root, &suffix, true)?
        .map(|(directory, _)| directory)
        .ok_or_else(|| KioError::invalid_usage("private directory creation returned no directory"))
}

fn initialize_lock(directory: &StoreDirectory, lock_bytes: &[u8]) -> Result<()> {
    directory.write_atomic(Path::new(".lock"), lock_bytes, Publication::CreateOnly)?;
    directory.ensure_owner_private(Path::new(".lock"))?;
    directory.sync()
}

#[cfg(all(test, unix))]
mod tests {
    use kio_core::control::ControlHandle;
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
    fn st01_private_state_accepts_external_writable_parent_without_repair() {
        let root = private_tempdir();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o777)).unwrap();
        let path = root.path().join("state");
        let control = ControlHandle::default();
        let state = super::open_or_create(&path, root.path(), b"st01-lock", &control).unwrap();
        assert_eq!(
            fs::metadata(root.path()).unwrap().permissions().mode() & 0o777,
            0o777
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(path.join(".lock"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(state);
        assert!(
            open_existing(&path, root.path(), b"st01-lock", &control)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            fs::metadata(root.path()).unwrap().permissions().mode() & 0o777,
            0o777
        );
    }

    #[test]
    fn st01_private_state_rejects_unsafe_own_directory_without_repair() {
        let root = private_tempdir();
        let path = root.path().join("state");
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            super::open_or_create(&path, root.path(), b"st01-lock", &ControlHandle::default())
                .is_err()
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(!path.join(".lock").exists());
    }

    #[test]
    fn external_alias_direct_child_store_is_retained_and_owned_symlink_is_refused() {
        let fixture = private_tempdir();
        let physical = fixture.path().canonicalize().unwrap();
        let external = physical.join("external");
        fs::create_dir(&external).unwrap();
        let alias = physical.join("external-alias");
        symlink(&external, &alias).unwrap();
        let path = alias.join("state");
        let control = ControlHandle::default();
        let state = super::create_initialized(
            &path,
            &alias,
            b"alias-lock",
            |directory| {
                directory.write_atomic(Path::new("record"), b"owned", Publication::CreateOnly)
            },
            &control,
        )
        .unwrap();
        assert_eq!(
            state
                .directory()
                .read_optional(Path::new("record"), 16)
                .unwrap(),
            Some(b"owned".to_vec())
        );
        drop(state);
        assert!(
            super::open_existing(&path, &alias, b"alias-lock", &control)
                .unwrap()
                .is_some()
        );

        let victim = physical.join("victim");
        fs::create_dir(&victim).unwrap();
        let diverted = alias.join("diverted");
        symlink(&victim, &diverted).unwrap();
        assert!(super::open_or_create(&diverted, &alias, b"alias-lock", &control).is_err());
        assert!(fs::read_dir(&victim).unwrap().next().is_none());
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
        assert!(
            open_readonly(
                &inherited,
                Path::new(&format!("/dev/fd/{}", handle.as_raw_fd())),
                &ControlHandle::default()
            )
            .unwrap()
            .is_none()
        );
        assert!(!root.path().join("missing").exists());
    }

    #[test]
    fn inherited_create_initialized_publishes_lock_and_record() {
        let root = private_tempdir();
        let handle = File::open(root.path()).unwrap();
        let inherited = inherited_path(&handle, "trust");
        let state = create_initialized(
            &inherited,
            Path::new(&format!("/dev/fd/{}", handle.as_raw_fd())),
            b"private-state-test-lock",
            |directory| {
                directory.write_atomic(Path::new("record.json"), b"{}", Publication::CreateOnly)
            },
            &ControlHandle::default(),
        )
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
            open_existing(
                &inherited,
                Path::new(&format!("/dev/fd/{}", handle.as_raw_fd())),
                b"private-state-test-lock",
                &ControlHandle::default()
            )
            .unwrap()
            .is_some()
        );
    }

    #[test]
    fn inherited_external_public_root_allows_private_store_creation() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let handle = File::open(root.path()).unwrap();
        let inherited = inherited_path(&handle, "state");

        let state = super::open_or_create(
            &inherited,
            Path::new(&format!("/dev/fd/{}", handle.as_raw_fd())),
            b"lock",
            &ControlHandle::default(),
        )
        .unwrap();
        assert_eq!(
            fs::metadata(root.path()).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(root.path().join("state"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        drop(state);
    }

    #[test]
    fn inherited_descriptor_suffix_alias_is_rejected_before_private_creation() {
        let root = private_tempdir();
        let handle = File::open(root.path()).unwrap();
        let external_base = PathBuf::from(format!("/dev/fd/{}", handle.as_raw_fd()));
        let malformed = PathBuf::from(format!("{}/./state", external_base.display()));
        assert!(
            super::open_or_create(
                &malformed,
                &external_base,
                b"lock",
                &ControlHandle::default(),
            )
            .is_err()
        );
        assert!(!root.path().join("state").exists());
    }
}

#[cfg(test)]
mod lock_marker_tests {
    use super::{create_initialized, open_existing};
    use kio_core::control::ControlHandle;
    use std::fs;

    #[test]
    fn only_lock_contention_is_classified_as_busy() {
        let path = std::path::Path::new("classified-state.lock");
        let busy = super::lock_acquisition_error(std::fs::TryLockError::WouldBlock, path);
        assert_eq!(busy.error_code(), "KIO-E-PRIVATE-STATE-LOCKED-001");
        assert_eq!(busy.exit_code(), kio_core::ExitCode::PartialFailure);
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::Unsupported,
        ] {
            let message = format!("lock error canary {kind:?}");
            let error = super::lock_acquisition_error(
                std::fs::TryLockError::Error(std::io::Error::new(kind, message.clone())),
                path,
            );
            assert_eq!(error.error_code(), "KIO-E-STORE-IO-001");
            assert_eq!(error.exit_code(), kio_core::ExitCode::Failure);
            assert_eq!(error.message(), message);
            assert_eq!(error.context()["path"], path.display().to_string());
        }
    }

    #[test]
    fn idle_bad_lock_markers_fail_without_repair_or_retaining_a_lock() {
        const EXPECTED: &[u8] = b"private-state-marker-test/v1\n";
        for marker in [Vec::new(), b"foreign marker".to_vec(), vec![b'x'; 257]] {
            let temp = crate::test_support::PrivateTempDir::new();
            let path = temp.path().join("state");
            let control = ControlHandle::default();
            let owner =
                create_initialized(&path, temp.path(), EXPECTED, |_| Ok(()), &control).unwrap();
            drop(owner);
            let lock = path.join(".lock");
            // Intentional tamper of this idle fixture's existing lock inode.
            fs::write(&lock, &marker).unwrap();
            for _ in 0..2 {
                let error = open_existing(&path, temp.path(), EXPECTED, &control).unwrap_err();
                assert_ne!(error.error_code(), "KIO-E-PRIVATE-STATE-LOCKED-001");
                if marker.len() <= 256 {
                    assert_eq!(error.error_code(), "KIO-E-PRIVATE-STATE-LOCK-001");
                }
                assert_eq!(
                    fs::read(&lock).unwrap(),
                    marker,
                    "bad marker must not be repaired"
                );
            }
        }
    }
}
