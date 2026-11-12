//! Retained, descriptor-relative storage directories.
//!
//! `StoreDirectory` deliberately keeps the directory descriptor which was
//! validated at construction time.  The human-readable path is diagnostic
//! data only; no operation below resolves it again.

use std::{
    fs::File,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use serde_json::json;

use crate::{ExitCode, KioError, Result, platform::store as platform};

pub(crate) mod atomic;
pub(crate) mod planned_removal;
pub use atomic::{ATOMIC_WORKSPACE_DIR, AtomicWorkspaceState};
pub(crate) use planned_removal::require_same_filesystem;
pub use planned_removal::{
    CAS_REMOVAL_QUARANTINE_PREFIX, PRUNE_DIRECTORY_QUARANTINE_PREFIX, PlannedDirectoryRemoval,
    PlannedFileRemoval, RemovalBudget,
};

pub(crate) const MAX_DIRECTORY_REMOVAL_DEPTH: usize = 128;
pub(crate) const MAX_DIRECTORY_REMOVAL_ENTRIES: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Publication {
    CreateOnly,
    Replace,
    /// Publish a new leaf when absent, otherwise replace a verified ordinary
    /// leaf.  This avoids a path-based `exists` check at the call site.
    Upsert,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreEntry {
    pub name: std::ffi::OsString,
    pub is_directory: bool,
    pub is_regular_file: bool,
}

#[derive(Debug, Clone)]
pub struct StoreDirectory {
    handle: Arc<File>,
    logical: PathBuf,
    control: crate::control::ControlHandle,
}

impl StoreDirectory {
    /// Open an absolute directory one component at a time without following
    /// links.  The supplied path is retained only for error reporting.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_control(path, crate::control::ControlHandle::default())
    }

    /// Retain existing ordinary directories and create only absent components
    /// privately. Existing external ancestors are never adopted or restricted;
    /// callers must separately admit their final Kio-owned directory.
    pub fn open_or_create_directories(path: &Path) -> Result<Self> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|error| ioerr(path, error))?
                .join(path)
        };
        if let Some((root, suffix)) = crate::private_fs::resolve_inherited_private_root(&absolute)?
        {
            return Self::open_or_create_suffix(root, suffix);
        }
        let absolute = platform::normalize_store_path(&absolute);
        let mut root = std::path::PathBuf::new();
        let mut names = Vec::new();
        for component in absolute.components() {
            match component {
                Component::Prefix(prefix) => root.push(prefix.as_os_str()),
                Component::RootDir => root.push(component.as_os_str()),
                Component::CurDir => {}
                Component::Normal(name) => names.push(name.to_os_string()),
                Component::ParentDir => {
                    return Err(err(path, "directory path contains parent traversal"));
                }
            }
        }
        let root_handle =
            cap_primitives::fs::open_ambient_dir(&root, cap_primitives::ambient_authority())
                .map_err(|error| ioerr(path, error))?;
        let directory = Self::from_retained(root_handle, root)?;
        Self::open_or_create_suffix(directory, names)
    }

    fn open_or_create_suffix(mut directory: Self, names: Vec<std::ffi::OsString>) -> Result<Self> {
        for name in names {
            let leaf = Path::new(&name);
            let handle = if directory.contains_entry(leaf)? {
                directory.open_directory(leaf)?
            } else {
                match directory.create_directory(leaf) {
                    Ok(handle) => handle,
                    Err(error)
                        if error.error_code() == "KIO-E-STORE-IO-001"
                            && error
                                .context()
                                .get("io_error_kind")
                                .and_then(serde_json::Value::as_str)
                                == Some("already_exists") =>
                    {
                        directory.open_directory(leaf)?
                    }
                    Err(error) => return Err(error),
                }
            };
            directory = Self::from_retained(handle, directory.path().join(&name))?;
        }
        Ok(directory)
    }

    pub fn open_with_control(path: &Path, control: crate::control::ControlHandle) -> Result<Self> {
        if let Some((mut directory, suffix)) =
            crate::private_fs::resolve_inherited_private_root_with_control(path, &control)?
        {
            for name in suffix {
                let handle = directory.open_directory(Path::new(&name))?;
                directory = Self::from_retained_with_control(
                    handle,
                    directory.path().join(name),
                    control.clone(),
                )?;
            }
            return Ok(directory);
        }
        let handle = platform::open(path)?;
        let handle = platform::normalize_directory_handle(handle, path)?;
        Ok(Self {
            handle: Arc::new(handle),
            logical: path.to_path_buf(),
            control,
        })
    }

    /// Adopt a directory handle which the caller already retained.  This
    /// validates the handle itself; callers that have a named binding must
    /// compare that binding separately before adopting it.
    pub fn from_retained(file: File, logical: PathBuf) -> Result<Self> {
        Self::from_retained_with_control(file, logical, crate::control::ControlHandle::default())
    }

    pub fn from_retained_with_control(
        file: File,
        logical: PathBuf,
        control: crate::control::ControlHandle,
    ) -> Result<Self> {
        platform::validate_directory(&file, &logical)?;
        let file = platform::normalize_directory_handle(file, &logical)?;
        Ok(Self {
            handle: Arc::new(file),
            logical,
            control,
        })
    }

    #[must_use]
    pub fn control(&self) -> &crate::control::ControlHandle {
        &self.control
    }

    #[must_use]
    pub fn rebind_with_control(&self, control: crate::control::ControlHandle) -> Self {
        Self {
            handle: self.handle.clone(),
            logical: self.logical.clone(),
            control,
        }
    }

    #[must_use]
    pub fn root_handle(&self) -> Arc<File> {
        Arc::clone(&self.handle)
    }
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.logical
    }

    pub fn open_directory(&self, relative: &Path) -> Result<File> {
        platform::open_directory(&self.handle, relative, &self.logical)
    }

    /// Open maintenance ancestry without crossing filesystem or Linux mount
    /// boundaries. Every component is compared through retained handles before
    /// traversing the next one; ordinary directory operations remain unchanged.
    pub fn open_maintenance_directory(&self, relative: &Path) -> Result<File> {
        let components = relative_components(relative, false, &self.logical)?;
        let mut current = self
            .handle
            .try_clone()
            .map_err(|e| ioerr(&self.logical, e))?;
        for component in components {
            let next = platform::open_directory(&current, Path::new(component), &self.logical)?;
            require_same_filesystem(&current, &next, &self.logical.join(relative))?;
            current = next;
        }
        Ok(current)
    }

    /// Validate an already-retained maintenance parent against this boundary.
    /// This compares handles only; it does not assert ancestry or reopen paths.
    pub fn require_same_filesystem(&self, other: &Self) -> Result<()> {
        require_same_filesystem(&self.handle, &other.handle, &other.logical)
    }

    /// Test one direct name without opening any siblings or following links.
    /// An occupied unsafe name is still present; its subsequent open must
    /// reject it. This must not be used as authorization for replacement.
    pub fn contains_entry(&self, leaf: &Path) -> Result<bool> {
        if relative_components(leaf, false, &self.logical)?.len() != 1 {
            return Err(err(&self.logical, "entry lookup requires one direct leaf"));
        }
        match cap_primitives::fs::stat(&self.handle, leaf, cap_primitives::fs::FollowSymlinks::No) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(ioerr(&self.logical.join(leaf), error)),
        }
    }

    pub fn create_directory_all(&self, relative: &Path) -> Result<File> {
        platform::create_directory_all(&self.handle, relative, &self.logical)
    }

    /// Create one directory entry beneath a retained parent.  This never
    /// reopens an existing name, so callers can distinguish first-time store
    /// initialization from an already-present or substituted directory.
    pub fn create_directory(&self, relative: &Path) -> Result<File> {
        platform::create_directory(&self.handle, relative, &self.logical)
    }

    pub fn open_regular_read(&self, relative: &Path, max_bytes: u64) -> Result<File> {
        platform::open_regular_read(&self.handle, relative, max_bytes, &self.logical)
    }

    /// Open a retained regular leaf with the rights required to restrict a
    /// current-user Windows quarantine object on this same handle.
    pub(super) fn open_regular_for_removal(&self, relative: &Path, max_bytes: u64) -> Result<File> {
        platform::open_regular_for_removal(&self.handle, relative, max_bytes, &self.logical)
    }

    pub fn read_optional(&self, relative: &Path, max_bytes: u64) -> Result<Option<Vec<u8>>> {
        platform::read_optional(&self.handle, relative, max_bytes, &self.logical)
    }

    pub fn write_atomic(
        &self,
        relative: &Path,
        bytes: &[u8],
        publication: Publication,
    ) -> Result<()> {
        atomic::write(self, self, relative, bytes, publication)
    }

    /// Publish private bytes only after a caller's final target check succeeds.
    /// The check runs after staging and syncing, immediately before publication,
    /// with the actual retained destination parent and direct leaf name.
    /// A rejected check leaves private staging residue for normal recovery.
    pub fn write_atomic_checked(
        &self,
        relative: &Path,
        bytes: &[u8],
        publication: Publication,
        before_publish: impl FnOnce(&StoreDirectory, &Path) -> Result<()>,
    ) -> Result<()> {
        atomic::write_checked(self, relative, bytes, publication, before_publish)
    }

    /// Atomically publish a private read-only file, refusing an existing name.
    /// Read-only protection is applied through the staging handle before publication.
    pub fn write_atomic_readonly(&self, relative: &Path, bytes: &[u8]) -> Result<()> {
        atomic::write_readonly(self, relative, bytes)
    }

    /// Publish into this retained target while keeping transient metadata in
    /// an explicitly supplied private owner. Working-tree restore uses its
    /// repository's `.kio` owner rather than creating metadata among user files.
    pub fn write_atomic_with_owner(
        &self,
        owner: &StoreDirectory,
        relative: &Path,
        bytes: &[u8],
        publication: Publication,
    ) -> Result<()> {
        atomic::write(owner, self, relative, bytes, publication)
    }

    /// Validate only the reserved workspace; this never creates or recovers it.
    pub fn inspect_atomic(&self) -> Result<AtomicWorkspaceState> {
        atomic::inspect(self)
    }

    /// Recover this owner's interrupted atomic operations only against the
    /// supplied retained targets. Callers hold their higher-level writer lease.
    pub fn recover_atomic(&self, allowed_targets: &[&StoreDirectory]) -> Result<bool> {
        atomic::recover(self, allowed_targets)
    }

    /// Lock a permanent, empty, owner-private gate in this retained directory.
    /// The file is never unlinked: every cooperating process locks one inode.
    pub fn lock_private_gate(&self, leaf: &Path) -> Result<File> {
        let gate = self.open_private_gate_raw(leaf)?;
        gate.lock()
            .map_err(|error| ioerr(&self.logical.join(leaf), error))?;
        if gate
            .metadata()
            .map_err(|error| ioerr(&self.logical.join(leaf), error))?
            .len()
            != 0
        {
            return Err(err(&self.logical.join(leaf), "private gate is not empty"));
        }
        self.ensure_owner_private(leaf)?;
        Ok(gate)
    }

    /// Append through a verified, retained parent.  The caller owns the
    /// serialization lock; this primitive only provides namespace safety.
    pub fn append(&self, relative: &Path, bytes: &[u8]) -> Result<()> {
        platform::append(&self.handle, relative, bytes, &self.logical)
    }

    /// Create an absent private log leaf, or append to an existing private
    /// regular leaf after checking that exact append handle. No permissions
    /// are repaired, and the caller retains its existing serialization lock.
    pub fn append_private(&self, relative: &Path, bytes: &[u8]) -> Result<()> {
        platform::append_private(&self.handle, relative, bytes, &self.logical)
    }

    pub fn remove_file(&self, relative: &Path) -> Result<()> {
        platform::remove_file(&self.handle, relative, &self.logical)
    }

    /// Move an existing directory to an absent sibling without resolving any
    /// absolute path. Both paths must have the same retained parent. This is
    /// create-only publication; it deliberately never replaces a directory.
    /// The platform operation atomically rejects an occupied destination.
    pub fn rename_directory_create_only(&self, source: &Path, destination: &Path) -> Result<()> {
        platform::rename_directory_create_only(&self.handle, source, destination, &self.logical)
    }

    /// Atomically move one direct retained child directory into a direct child
    /// of another retained directory, refusing an occupied destination.
    pub fn rename_directory_between_create_only(
        &self,
        source_leaf: &Path,
        destination_directory: &StoreDirectory,
        destination_leaf: &Path,
    ) -> Result<()> {
        platform::rename_directory_between_create_only(
            &self.handle,
            source_leaf,
            &destination_directory.handle,
            destination_leaf,
            &self.logical,
            &destination_directory.logical,
        )
    }

    /// Move an existing directory to a generated same-parent quarantine name.
    /// The returned relative path remains usable only through this retained
    /// store and may be passed to [`Self::remove_directory_all`].
    pub fn quarantine_directory(&self, relative: &Path) -> Result<PathBuf> {
        platform::quarantine_directory(&self.handle, relative, &self.logical)
    }

    /// Recursively remove a directory through retained no-follow handles.
    /// Symlinks, reparse points, and non-regular leaves are rejected rather
    /// than removed. Callers supply their own serialization.
    pub fn remove_directory_all(&self, relative: &Path) -> Result<()> {
        platform::remove_directory_all(&self.handle, relative, &self.logical)
    }

    /// Verify a retained leaf is owned by the current user and has no group or
    /// world permissions.  StoreDirectory's general file operations do not
    /// silently repair existing permissions.
    pub fn ensure_owner_private(&self, relative: &Path) -> Result<()> {
        platform::ensure_owner_private(&self.handle, relative, &self.logical)
    }

    pub fn entries(&self, relative: &Path) -> Result<Vec<StoreEntry>> {
        platform::entries(&self.handle, relative, &self.logical)
    }

    /// List a directory, treating only a missing leaf as absent.
    pub fn entries_optional(&self, relative: &Path) -> Result<Option<Vec<StoreEntry>>> {
        platform::entries_optional(&self.handle, relative, &self.logical)
    }

    /// Remove a leaf only after moving it to a private same-directory
    /// quarantine after durably recording its exact identity and bounded bytes.
    /// Recovery never removes a quarantine without its matching ready intent.
    pub fn quarantine_then_remove(
        &self,
        relative: &Path,
        expected: &[u8],
        max_bytes: u64,
    ) -> Result<()> {
        atomic::remove(self, self, relative, expected, max_bytes)
    }

    /// Remove an exact target using a durable intent in a separate retained
    /// private owner. No removal artifact is written among working-tree files.
    pub fn quarantine_then_remove_with_owner(
        &self,
        owner: &StoreDirectory,
        relative: &Path,
        expected: &[u8],
        max_bytes: u64,
    ) -> Result<()> {
        atomic::remove(owner, self, relative, expected, max_bytes)
    }

    pub fn sync(&self) -> Result<()> {
        platform::sync_directory(&self.handle, &self.logical)
    }

    /// Resolve a persisted relative leaf through retained no-follow directory
    /// handles, returning that retained parent and its direct child name.
    /// Protocol code owns any narrower component-count policy.
    pub(super) fn retained_parent(&self, relative: &Path) -> Result<(Self, std::ffi::OsString)> {
        let components = relative_components(relative, false, &self.logical)?;
        let leaf = components
            .last()
            .expect("nonempty relative leaf")
            .to_os_string();
        let mut parent_relative = PathBuf::new();
        for component in &components[..components.len() - 1] {
            parent_relative.push(component);
        }
        let parent = self.open_directory(&parent_relative)?;
        Ok((
            Self::from_retained_with_control(
                parent,
                self.logical.join(parent_relative),
                self.control.clone(),
            )?,
            leaf,
        ))
    }

    /// Create one new owner-private regular child through the retained parent.
    /// Existing names are refused; callers own byte writing and durability.
    pub fn create_private_file(&self, leaf: &Path) -> Result<File> {
        platform::create_private_file_raw(&self.handle, leaf, &self.logical)
    }

    /// Retain a direct private gate file suitable for advisory file locking.
    pub(super) fn open_private_gate_raw(&self, leaf: &Path) -> Result<File> {
        platform::open_private_gate_raw(&self.handle, leaf, &self.logical)
    }

    /// Atomically publish a direct single-link regular file into another
    /// retained parent. This primitive never creates staging files or follows
    /// names outside the two retained directory capabilities.
    pub(super) fn rename_regular_between_raw(
        &self,
        source_leaf: &Path,
        destination_parent: &StoreDirectory,
        destination_leaf: &Path,
        publication: Publication,
    ) -> Result<()> {
        platform::rename_regular_between_raw(
            &self.handle,
            source_leaf,
            &destination_parent.handle,
            destination_leaf,
            publication,
            &self.logical,
            &destination_parent.logical,
        )
    }
}

/// Restrict a just-created directory before it is used for persisted state.
/// Existing directories must be rejected by their caller instead of repaired.
pub fn restrict_new_private_directory(directory: &File) -> Result<()> {
    platform::restrict_new_private_directory(directory)
}

pub(crate) fn err(path: &Path, message: impl Into<String>) -> KioError {
    KioError::new(
        "KIO-E-STORE-UNSAFE-001",
        message,
        json!({"path": path.display().to_string()}),
        ExitCode::PermanentFailure,
    )
}
pub(crate) fn ioerr(path: &Path, error: std::io::Error) -> KioError {
    let kind = match error.kind() {
        std::io::ErrorKind::AlreadyExists => "already_exists",
        std::io::ErrorKind::NotFound => "not_found",
        std::io::ErrorKind::PermissionDenied => "permission_denied",
        std::io::ErrorKind::InvalidInput => "invalid_input",
        _ => "other",
    };
    KioError::new(
        "KIO-E-STORE-IO-001",
        error.to_string(),
        json!({"path": path.display().to_string(), "io_error_kind": kind}),
        ExitCode::Failure,
    )
}
pub(crate) fn relative_components<'a>(
    relative: &'a Path,
    allow_empty: bool,
    label: &Path,
) -> Result<Vec<&'a std::ffi::OsStr>> {
    let mut out = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(c) => {
                crate::platform::storage_semantics::validate_store_component(c, label)?;
                out.push(c);
            }
            _ => {
                return Err(err(
                    label,
                    "store path must contain only normal relative components",
                ));
            }
        }
    }
    if out.is_empty() && !allow_empty {
        return Err(err(label, "store path requires a leaf"));
    }
    Ok(out)
}
