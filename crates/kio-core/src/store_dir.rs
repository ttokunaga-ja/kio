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

use crate::{ExitCode, KioError, Result};

mod atomic;
pub use atomic::{ATOMIC_WORKSPACE_DIR, AtomicWorkspaceState};

const MAX_DIRECTORY_REMOVAL_DEPTH: usize = 128;
const MAX_DIRECTORY_REMOVAL_ENTRIES: usize = 100_000;

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
}

impl StoreDirectory {
    /// Open an absolute directory one component at a time without following
    /// links.  The supplied path is retained only for error reporting.
    pub fn open(path: &Path) -> Result<Self> {
        let handle = platform::open(path)?;
        let handle = platform::normalize_directory_handle(handle, path)?;
        Ok(Self {
            handle: Arc::new(handle),
            logical: path.to_path_buf(),
        })
    }

    /// Adopt a directory handle which the caller already retained.  This
    /// validates the handle itself; callers that have a named binding must
    /// compare that binding separately before adopting it.
    pub fn from_retained(file: File, logical: PathBuf) -> Result<Self> {
        platform::validate_directory(&file, &logical)?;
        let file = platform::normalize_directory_handle(file, &logical)?;
        Ok(Self {
            handle: Arc::new(file),
            logical,
        })
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
            Self::from_retained(parent, self.logical.join(parent_relative))?,
            leaf,
        ))
    }

    /// Create one direct private regular-file child without publishing it.
    pub(super) fn create_private_file_raw(&self, leaf: &Path) -> Result<File> {
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

    /// Move a Windows regular file by its already-validated retained handle.
    /// The source name is never reopened by this primitive.
    #[cfg(windows)]
    pub(super) fn move_verified_regular_handle_to_raw(
        &self,
        source: &File,
        destination_parent: &StoreDirectory,
        destination_leaf: &Path,
    ) -> Result<()> {
        platform::move_verified_regular_handle_to_raw(
            &self.handle,
            source,
            &destination_parent.handle,
            destination_leaf,
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

fn err(path: &Path, message: impl Into<String>) -> KioError {
    KioError::new(
        "KIO-E-STORE-UNSAFE-001",
        message,
        json!({"path": path.display().to_string()}),
        ExitCode::PermanentFailure,
    )
}
fn ioerr(path: &Path, error: std::io::Error) -> KioError {
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
fn relative_components<'a>(
    relative: &'a Path,
    allow_empty: bool,
    label: &Path,
) -> Result<Vec<&'a std::ffi::OsStr>> {
    let mut out = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(c) => {
                #[cfg(windows)]
                {
                    use std::os::windows::ffi::OsStrExt as _;
                    if c.encode_wide().any(|unit| unit == b':' as u16) {
                        return Err(err(
                            label,
                            "Windows store paths cannot contain ADS separators",
                        ));
                    }
                }
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

#[cfg(unix)]
mod platform {
    use super::{
        MAX_DIRECTORY_REMOVAL_DEPTH, MAX_DIRECTORY_REMOVAL_ENTRIES, Publication, Result,
        StoreEntry, err, ioerr, relative_components,
    };
    use cap_primitives::fs::{DirBuilderExt, OpenOptionsExt};
    use cap_primitives::{ambient_authority, fs as cap_fs};
    use std::{
        ffi::CString,
        fs::File,
        io::{Read, Write},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::fs::{MetadataExt, PermissionsExt},
        },
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };
    static TEMP: AtomicU64 = AtomicU64::new(0);

    fn full(label: &Path, relative: &Path) -> std::path::PathBuf {
        label.join(relative)
    }
    pub(super) fn open(path: &Path) -> Result<File> {
        if !path.is_absolute() {
            return Err(err(path, "store directory path must be absolute"));
        }
        let root = cap_fs::open_ambient_dir(Path::new("/"), ambient_authority())
            .map_err(|e| ioerr(path, e))?;
        let mut current = root;
        for component in path.components() {
            match component {
                std::path::Component::RootDir => {}
                std::path::Component::Normal(name) => {
                    current =
                        cap_fs::open_dir_nofollow(&current, Path::new(name)).map_err(|_| {
                            err(path, "store directory has a missing or unsafe component")
                        })?
                }
                _ => {
                    return Err(err(
                        path,
                        "store directory path contains an unsafe component",
                    ));
                }
            }
        }
        validate_directory(&current, path)?;
        Ok(current)
    }
    pub(super) fn validate_directory(file: &File, label: &Path) -> Result<()> {
        let meta = file.metadata().map_err(|e| ioerr(label, e))?;
        if !meta.is_dir() {
            return Err(err(label, "retained store handle is not a directory"));
        }
        Ok(())
    }
    pub(super) fn normalize_directory_handle(file: File, label: &Path) -> Result<File> {
        operation_directory(&file, label)
    }
    fn directory(root: &File, relative: &Path, label: &Path) -> Result<File> {
        let components = relative_components(relative, true, label)?;
        let mut current =
            cap_fs::open_dir_nofollow(root, Path::new(".")).map_err(|e| ioerr(label, e))?;
        for c in components {
            current = cap_fs::open_dir_nofollow(&current, Path::new(c)).map_err(|_| {
                err(
                    &full(label, relative),
                    "store directory component is missing or unsafe",
                )
            })?;
        }
        validate_directory(&current, &full(label, relative))?;
        Ok(current)
    }
    pub(super) fn open_directory(root: &File, relative: &Path, label: &Path) -> Result<File> {
        directory(root, relative, label)
    }
    pub(super) fn create_directory_all(root: &File, relative: &Path, label: &Path) -> Result<File> {
        let components = relative_components(relative, true, label)?;
        let mut current =
            cap_fs::open_dir_nofollow(root, Path::new(".")).map_err(|e| ioerr(label, e))?;
        for c in components {
            match cap_fs::open_dir_nofollow(&current, Path::new(c)) {
                Ok(next) => current = next,
                Err(_) => {
                    let mut options = cap_fs::DirOptions::new();
                    options.mode(0o700);
                    cap_fs::create_dir(&current, Path::new(c), &options)
                        .map_err(|e| ioerr(&full(label, relative), e))?;
                    let next = cap_fs::open_dir_nofollow(&current, Path::new(c)).map_err(|_| {
                        err(&full(label, relative), "created store directory is unsafe")
                    })?;
                    restrict_new_private_directory(&next)?;
                    sync_directory(&current, label)?;
                    current = next;
                }
            }
            validate_directory(&current, &full(label, relative))?;
        }
        Ok(current)
    }
    pub(super) fn create_directory(root: &File, relative: &Path, label: &Path) -> Result<File> {
        let (parent, leaf) = parent(root, relative, label)?;
        let path = full(label, relative);
        let mut options = cap_fs::DirOptions::new();
        options.mode(0o700);
        cap_fs::create_dir(&parent, Path::new(leaf), &options).map_err(|e| ioerr(&path, e))?;
        let created = cap_fs::open_dir_nofollow(&parent, Path::new(leaf))
            .map_err(|_| err(&path, "created store directory is unsafe"))?;
        restrict_new_private_directory(&created)?;
        sync_directory(&parent, label)?;
        Ok(created)
    }
    fn parent<'a>(
        root: &File,
        relative: &'a Path,
        label: &Path,
    ) -> Result<(File, &'a std::ffi::OsStr)> {
        let c = relative_components(relative, false, label)?;
        let leaf = *c.last().expect("nonempty");
        let mut parent =
            cap_fs::open_dir_nofollow(root, Path::new(".")).map_err(|e| ioerr(label, e))?;
        for part in &c[..c.len() - 1] {
            parent = cap_fs::open_dir_nofollow(&parent, Path::new(part))
                .map_err(|_| err(&full(label, relative), "store parent is missing or unsafe"))?;
        }
        Ok((parent, leaf))
    }
    fn open_leaf(
        parent: &File,
        leaf: &std::ffi::OsStr,
        flags: i32,
        mode: i32,
        label: &Path,
    ) -> Result<File> {
        let name = CString::new(leaf.as_encoded_bytes())
            .map_err(|_| err(label, "store leaf contains NUL"))?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                mode,
            )
        };
        if fd < 0 {
            let cause = std::io::Error::last_os_error();
            if matches!(cause.raw_os_error(), Some(libc::ELOOP)) {
                return Err(err(label, "store leaf is a symlink or unsafe link"));
            }
            return Err(ioerr(label, cause));
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    fn open_leaf_optional(
        parent: &File,
        leaf: &std::ffi::OsStr,
        label: &Path,
    ) -> Result<Option<File>> {
        let name = CString::new(leaf.as_encoded_bytes())
            .map_err(|_| err(label, "store leaf contains NUL"))?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            let cause = std::io::Error::last_os_error();
            return if cause.kind() == std::io::ErrorKind::NotFound
                || matches!(cause.raw_os_error(), Some(libc::ENOENT))
            {
                Ok(None)
            } else if matches!(cause.raw_os_error(), Some(libc::ELOOP)) {
                Err(err(label, "store leaf is a symlink or unsafe link"))
            } else {
                Err(ioerr(label, cause))
            };
        }
        Ok(Some(unsafe { File::from_raw_fd(fd) }))
    }
    fn parent_optional<'a>(
        root: &File,
        relative: &'a Path,
        label: &Path,
    ) -> Result<Option<(File, &'a std::ffi::OsStr)>> {
        let c = relative_components(relative, false, label)?;
        let leaf = *c.last().unwrap();
        let mut parent =
            cap_fs::open_dir_nofollow(root, Path::new(".")).map_err(|e| ioerr(label, e))?;
        for part in &c[..c.len() - 1] {
            match cap_fs::open_dir_nofollow(&parent, Path::new(part)) {
                Ok(next) => parent = next,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(_) => return Err(err(&full(label, relative), "store parent is unsafe")),
            }
        }
        Ok(Some((parent, leaf)))
    }
    fn safe_regular(file: &File, label: &Path) -> Result<std::fs::Metadata> {
        let m = file.metadata().map_err(|e| ioerr(label, e))?;
        if !m.is_file() || m.nlink() != 1 {
            return Err(err(label, "store leaf is not a single-link regular file"));
        }
        Ok(m)
    }
    fn same(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.len() == after.len()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
    }
    fn same_directory_identity(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
        before.is_dir()
            && after.is_dir()
            && before.dev() == after.dev()
            && before.ino() == after.ino()
    }
    pub(super) fn open_regular_read(
        root: &File,
        relative: &Path,
        max: u64,
        label: &Path,
    ) -> Result<File> {
        let (parent, leaf) = parent(root, relative, label)?;
        let p = full(label, relative);
        let file = open_leaf(&parent, leaf, libc::O_RDONLY, 0, &p)?;
        let m = safe_regular(&file, &p)?;
        if m.len() > max {
            return Err(err(&p, "store file exceeds byte limit"));
        }
        Ok(file)
    }
    pub(super) fn open_regular_for_removal(
        root: &File,
        relative: &Path,
        max: u64,
        label: &Path,
    ) -> Result<File> {
        open_regular_read(root, relative, max, label)
    }
    pub(super) fn read_optional(
        root: &File,
        relative: &Path,
        max: u64,
        label: &Path,
    ) -> Result<Option<Vec<u8>>> {
        let Some((parent, leaf)) = parent_optional(root, relative, label)? else {
            return Ok(None);
        };
        let p = full(label, relative);
        let Some(mut file) = open_leaf_optional(&parent, leaf, &p)? else {
            return Ok(None);
        };
        let before = safe_regular(&file, &p)?;
        if before.len() > max {
            return Err(err(&p, "store file exceeds byte limit"));
        }
        let mut data = Vec::with_capacity(before.len() as usize);
        Read::by_ref(&mut file)
            .take(max.saturating_add(1))
            .read_to_end(&mut data)
            .map_err(|e| ioerr(&p, e))?;
        let after = safe_regular(&file, &p)?;
        if data.len() as u64 > max || !same(&before, &after) || after.len() != data.len() as u64 {
            return Err(err(&p, "store file changed while it was read"));
        }
        Ok(Some(data))
    }
    pub(super) fn create_private_file_raw(root: &File, leaf: &Path, label: &Path) -> Result<File> {
        let leaf = direct_leaf(leaf, label)?;
        let path = label.join(leaf);
        let file = open_leaf(
            root,
            leaf,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
            &path,
        )?;
        ensure_private_regular(&file, &path)?;
        Ok(file)
    }
    pub(super) fn open_private_gate_raw(root: &File, leaf: &Path, label: &Path) -> Result<File> {
        let leaf = direct_leaf(leaf, label)?;
        let path = label.join(leaf);
        match create_private_file_raw(root, Path::new(leaf), label) {
            Ok(file) => {
                if file.metadata().map_err(|e| ioerr(&path, e))?.len() != 0 {
                    return Err(err(&path, "private gate file is not empty"));
                }
                Ok(file)
            }
            Err(error) if is_io_kind(&error, "already_exists") => {
                let file = open_leaf(root, leaf, libc::O_RDWR, 0, &path)?;
                if ensure_private_regular(&file, &path)?.len() != 0 {
                    return Err(err(&path, "private gate file is not empty"));
                }
                Ok(file)
            }
            Err(error) => Err(error),
        }
    }
    fn ensure_private_regular(file: &File, label: &Path) -> Result<std::fs::Metadata> {
        let meta = safe_regular(file, label)?;
        let uid = unsafe { libc::geteuid() };
        if meta.uid() != uid || meta.mode() & 0o077 != 0 {
            return Err(err(label, "store leaf is not owner-private"));
        }
        Ok(meta)
    }
    pub(super) fn rename_regular_between_raw(
        source_parent: &File,
        source_leaf: &Path,
        destination_parent: &File,
        destination_leaf: &Path,
        publication: Publication,
        source_label: &Path,
        destination_label: &Path,
    ) -> Result<()> {
        let source = direct_leaf(source_leaf, source_label)?;
        let destination = direct_leaf(destination_leaf, destination_label)?;
        let source_path = source_label.join(source);
        let destination_path = destination_label.join(destination);
        let source_parent_meta = source_parent
            .metadata()
            .map_err(|e| ioerr(source_label, e))?;
        let destination_parent_meta = destination_parent
            .metadata()
            .map_err(|e| ioerr(destination_label, e))?;
        if source_parent_meta.dev() != destination_parent_meta.dev() {
            return Err(err(
                &destination_path,
                "regular rename crosses filesystem volumes",
            ));
        }
        let source_file = open_leaf(source_parent, source, libc::O_RDONLY, 0, &source_path)?;
        let source_meta = safe_regular(&source_file, &source_path)?;
        match publication {
            Publication::CreateOnly => rename_regular_noreplace(
                source_parent,
                source,
                destination_parent,
                destination,
                &destination_path,
            )?,
            Publication::Replace => {
                let destination_file = open_leaf(
                    destination_parent,
                    destination,
                    libc::O_RDONLY,
                    0,
                    &destination_path,
                )?;
                safe_regular(&destination_file, &destination_path)?;
                rename_regular_replace(
                    source_parent,
                    source,
                    destination_parent,
                    destination,
                    &destination_path,
                )?;
            }
            Publication::Upsert => match rename_regular_noreplace(
                source_parent,
                source,
                destination_parent,
                destination,
                &destination_path,
            ) {
                Ok(()) => {}
                Err(error) if is_io_kind(&error, "already_exists") => {
                    let destination_file = open_leaf(
                        destination_parent,
                        destination,
                        libc::O_RDONLY,
                        0,
                        &destination_path,
                    )?;
                    safe_regular(&destination_file, &destination_path)?;
                    rename_regular_replace(
                        source_parent,
                        source,
                        destination_parent,
                        destination,
                        &destination_path,
                    )?;
                }
                Err(error) => return Err(error),
            },
        }
        let published = open_leaf(
            destination_parent,
            destination,
            libc::O_RDONLY,
            0,
            &destination_path,
        )?;
        let published_meta = safe_regular(&published, &destination_path)?;
        if source_meta.dev() != published_meta.dev() || source_meta.ino() != published_meta.ino() {
            return Err(err(
                &destination_path,
                "published regular-file identity changed",
            ));
        }
        sync_directory(source_parent, source_label)?;
        sync_directory(destination_parent, destination_label)
    }
    fn rename_regular_replace(
        source_parent: &File,
        source: &std::ffi::OsStr,
        destination_parent: &File,
        destination: &std::ffi::OsStr,
        label: &Path,
    ) -> Result<()> {
        let source = CString::new(source.as_encoded_bytes())
            .map_err(|_| err(label, "store leaf contains NUL"))?;
        let destination = CString::new(destination.as_encoded_bytes())
            .map_err(|_| err(label, "store leaf contains NUL"))?;
        if unsafe {
            libc::renameat(
                source_parent.as_raw_fd(),
                source.as_ptr(),
                destination_parent.as_raw_fd(),
                destination.as_ptr(),
            )
        } == 0
        {
            Ok(())
        } else {
            Err(ioerr(label, std::io::Error::last_os_error()))
        }
    }
    fn is_io_kind(error: &crate::KioError, kind: &str) -> bool {
        error.error_code() == "KIO-E-STORE-IO-001"
            && error
                .context()
                .get("io_error_kind")
                .and_then(serde_json::Value::as_str)
                == Some(kind)
    }
    pub(super) fn append(root: &File, relative: &Path, bytes: &[u8], label: &Path) -> Result<()> {
        let (parent, leaf) = parent(root, relative, label)?;
        let p = full(label, relative);
        let mut f = open_leaf(&parent, leaf, libc::O_WRONLY | libc::O_APPEND, 0, &p)?;
        let _ = safe_regular(&f, &p)?;
        f.write_all(bytes).map_err(|e| ioerr(&p, e))?;
        f.sync_data().map_err(|e| ioerr(&p, e))?;
        Ok(())
    }
    pub(super) fn remove_file(root: &File, relative: &Path, label: &Path) -> Result<()> {
        let (parent, leaf) = parent(root, relative, label)?;
        let p = full(label, relative);
        let f = open_leaf(&parent, leaf, libc::O_RDONLY, 0, &p)?;
        let _ = safe_regular(&f, &p)?;
        drop(f);
        cap_fs::remove_file(&parent, Path::new(leaf)).map_err(|e| ioerr(&p, e))?;
        sync_directory(&parent, label)
    }
    pub(super) fn rename_directory_create_only(
        root: &File,
        source: &Path,
        destination: &Path,
        label: &Path,
    ) -> Result<()> {
        let (parent, source_leaf, destination_leaf) =
            same_parent(root, source, destination, label)?;
        let source_path = full(label, source);
        let destination_path = full(label, destination);
        let source_directory = cap_fs::open_dir_nofollow(&parent, Path::new(source_leaf))
            .map_err(|_| err(&source_path, "source directory is missing or unsafe"))?;
        validate_directory(&source_directory, &source_path)?;
        rename_directory_noreplace(
            &parent,
            source_leaf,
            &parent,
            destination_leaf,
            &source_path,
        )?;
        let published =
            cap_fs::open_dir_nofollow(&parent, Path::new(destination_leaf)).map_err(|_| {
                err(
                    &destination_path,
                    "published directory is missing or unsafe",
                )
            })?;
        if !same_directory_identity(
            &source_directory
                .metadata()
                .map_err(|error| ioerr(&source_path, error))?,
            &published
                .metadata()
                .map_err(|error| ioerr(&destination_path, error))?,
        ) {
            return Err(err(
                &destination_path,
                "published directory identity changed",
            ));
        }
        sync_directory(&parent, label)
    }
    pub(super) fn rename_directory_between_create_only(
        source_parent: &File,
        source_leaf: &Path,
        destination_parent: &File,
        destination_leaf: &Path,
        source_label: &Path,
        destination_label: &Path,
    ) -> Result<()> {
        let source = direct_leaf(source_leaf, source_label)?;
        let destination = direct_leaf(destination_leaf, destination_label)?;
        validate_directory(source_parent, source_label)?;
        validate_directory(destination_parent, destination_label)?;
        let source_parent_meta = source_parent
            .metadata()
            .map_err(|e| ioerr(source_label, e))?;
        let destination_parent_meta = destination_parent
            .metadata()
            .map_err(|e| ioerr(destination_label, e))?;
        if source_parent_meta.dev() != destination_parent_meta.dev() {
            return Err(err(
                destination_label,
                "directory rename crosses filesystem volumes",
            ));
        }
        let source_directory = cap_fs::open_dir_nofollow(source_parent, Path::new(source))
            .map_err(|_| err(source_label, "source directory is missing or unsafe"))?;
        validate_directory(&source_directory, source_label)?;
        rename_directory_noreplace(
            source_parent,
            source,
            destination_parent,
            destination,
            source_label,
        )?;
        let published = cap_fs::open_dir_nofollow(destination_parent, Path::new(destination))
            .map_err(|_| {
                err(
                    destination_label,
                    "published directory is missing or unsafe",
                )
            })?;
        if !same_directory_identity(
            &source_directory
                .metadata()
                .map_err(|e| ioerr(source_label, e))?,
            &published
                .metadata()
                .map_err(|e| ioerr(destination_label, e))?,
        ) {
            return Err(err(
                destination_label,
                "published directory identity changed",
            ));
        }
        sync_directory(source_parent, source_label)?;
        sync_directory(destination_parent, destination_label)
    }
    fn direct_leaf<'a>(path: &'a Path, label: &Path) -> Result<&'a std::ffi::OsStr> {
        let components = relative_components(path, false, label)?;
        if components.len() != 1 {
            return Err(err(label, "directory rename requires one direct leaf"));
        }
        Ok(components[0])
    }
    pub(super) fn quarantine_directory(
        root: &File,
        relative: &Path,
        label: &Path,
    ) -> Result<PathBuf> {
        let components = relative_components(relative, false, label)?;
        let mut destination = PathBuf::new();
        for component in &components[..components.len() - 1] {
            destination.push(component);
        }
        destination.push(format!(
            ".kio-directory-quarantine-{}-{}",
            std::process::id(),
            TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        rename_directory_create_only(root, relative, &destination, label)?;
        Ok(destination)
    }
    pub(super) fn remove_directory_all(root: &File, relative: &Path, label: &Path) -> Result<()> {
        let (parent, leaf) = parent(root, relative, label)?;
        let path = full(label, relative);
        let directory = cap_fs::open_dir_nofollow(&parent, Path::new(leaf))
            .map_err(|_| err(&path, "directory removal target is missing or unsafe"))?;
        validate_directory(&directory, &path)?;
        let mut remaining = MAX_DIRECTORY_REMOVAL_ENTRIES;
        remove_directory_contents(&directory, &path, 0, &mut remaining)?;
        drop(directory);
        cap_fs::remove_dir(&parent, Path::new(leaf)).map_err(|error| ioerr(&path, error))?;
        sync_directory(&parent, label)
    }
    fn same_parent<'a>(
        root: &File,
        source: &'a Path,
        destination: &'a Path,
        label: &Path,
    ) -> Result<(File, &'a std::ffi::OsStr, &'a std::ffi::OsStr)> {
        let source_components = relative_components(source, false, label)?;
        let destination_components = relative_components(destination, false, label)?;
        if source_components.len() != destination_components.len()
            || source_components[..source_components.len() - 1]
                != destination_components[..destination_components.len() - 1]
        {
            return Err(err(label, "directory rename requires the same parent"));
        }
        let (parent, source_leaf) = parent(root, source, label)?;
        Ok((
            parent,
            source_leaf,
            *destination_components.last().expect("nonempty destination"),
        ))
    }
    fn remove_directory_contents(
        directory: &File,
        label: &Path,
        depth: usize,
        remaining: &mut usize,
    ) -> Result<()> {
        if depth >= MAX_DIRECTORY_REMOVAL_DEPTH {
            return Err(err(label, "directory removal exceeds maximum depth"));
        }
        for entry in
            cap_fs::read_dir(directory, Path::new(".")).map_err(|error| ioerr(label, error))?
        {
            let entry = entry.map_err(|error| ioerr(label, error))?;
            *remaining = remaining
                .checked_sub(1)
                .ok_or_else(|| err(label, "directory removal exceeds entry limit"))?;
            let name = entry.file_name();
            let child_label = label.join(&name);
            match cap_fs::open_dir_nofollow(directory, Path::new(&name)) {
                Ok(child) => {
                    validate_directory(&child, &child_label)?;
                    remove_directory_contents(&child, &child_label, depth + 1, remaining)?;
                    drop(child);
                    cap_fs::remove_dir(directory, Path::new(&name))
                        .map_err(|error| ioerr(&child_label, error))?;
                }
                Err(_) => {
                    let file = open_leaf(directory, &name, libc::O_RDONLY, 0, &child_label)?;
                    safe_regular(&file, &child_label)?;
                    drop(file);
                    cap_fs::remove_file(directory, Path::new(&name))
                        .map_err(|error| ioerr(&child_label, error))?;
                }
            }
        }
        sync_directory(directory, label)
    }
    #[cfg(target_os = "macos")]
    fn rename_regular_noreplace(
        source_parent: &File,
        source: &std::ffi::OsStr,
        destination_parent: &File,
        destination: &std::ffi::OsStr,
        label: &Path,
    ) -> Result<()> {
        let source = CString::new(source.as_encoded_bytes())
            .map_err(|_| err(label, "store leaf contains NUL"))?;
        let destination = CString::new(destination.as_encoded_bytes())
            .map_err(|_| err(label, "store leaf contains NUL"))?;
        unsafe extern "C" {
            fn renameatx_np(
                fromfd: libc::c_int,
                from: *const libc::c_char,
                tofd: libc::c_int,
                to: *const libc::c_char,
                flags: libc::c_uint,
            ) -> libc::c_int;
        }
        if unsafe {
            renameatx_np(
                source_parent.as_raw_fd(),
                source.as_ptr(),
                destination_parent.as_raw_fd(),
                destination.as_ptr(),
                0x0000_0004,
            )
        } == 0
        {
            Ok(())
        } else {
            Err(ioerr(label, std::io::Error::last_os_error()))
        }
    }
    #[cfg(target_os = "linux")]
    fn rename_regular_noreplace(
        source_parent: &File,
        source: &std::ffi::OsStr,
        destination_parent: &File,
        destination: &std::ffi::OsStr,
        label: &Path,
    ) -> Result<()> {
        let source = CString::new(source.as_encoded_bytes())
            .map_err(|_| err(label, "store leaf contains NUL"))?;
        let destination = CString::new(destination.as_encoded_bytes())
            .map_err(|_| err(label, "store leaf contains NUL"))?;
        if unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                source_parent.as_raw_fd(),
                source.as_ptr(),
                destination_parent.as_raw_fd(),
                destination.as_ptr(),
                1_u32,
            )
        } == 0
        {
            Ok(())
        } else {
            Err(ioerr(label, std::io::Error::last_os_error()))
        }
    }
    #[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
    fn rename_regular_noreplace(
        _source_parent: &File,
        _source: &std::ffi::OsStr,
        _destination_parent: &File,
        _destination: &std::ffi::OsStr,
        label: &Path,
    ) -> Result<()> {
        Err(err(
            label,
            "atomic no-replace regular-file rename is unsupported",
        ))
    }
    #[cfg(target_os = "macos")]
    fn rename_directory_noreplace(
        source_parent: &File,
        source: &std::ffi::OsStr,
        destination_parent: &File,
        destination: &std::ffi::OsStr,
        label: &Path,
    ) -> Result<()> {
        let source = CString::new(source.as_encoded_bytes())
            .map_err(|_| err(label, "directory name contains NUL"))?;
        let destination = CString::new(destination.as_encoded_bytes())
            .map_err(|_| err(label, "directory name contains NUL"))?;
        unsafe extern "C" {
            fn renameatx_np(
                fromfd: libc::c_int,
                from: *const libc::c_char,
                tofd: libc::c_int,
                to: *const libc::c_char,
                flags: libc::c_uint,
            ) -> libc::c_int;
        }
        if unsafe {
            renameatx_np(
                source_parent.as_raw_fd(),
                source.as_ptr(),
                destination_parent.as_raw_fd(),
                destination.as_ptr(),
                0x0000_0004,
            )
        } == 0
        {
            Ok(())
        } else {
            Err(ioerr(label, std::io::Error::last_os_error()))
        }
    }
    #[cfg(target_os = "linux")]
    fn rename_directory_noreplace(
        source_parent: &File,
        source: &std::ffi::OsStr,
        destination_parent: &File,
        destination: &std::ffi::OsStr,
        label: &Path,
    ) -> Result<()> {
        let source = CString::new(source.as_encoded_bytes())
            .map_err(|_| err(label, "directory name contains NUL"))?;
        let destination = CString::new(destination.as_encoded_bytes())
            .map_err(|_| err(label, "directory name contains NUL"))?;
        if unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                source_parent.as_raw_fd(),
                source.as_ptr(),
                destination_parent.as_raw_fd(),
                destination.as_ptr(),
                1_u32,
            )
        } == 0
        {
            Ok(())
        } else {
            Err(ioerr(label, std::io::Error::last_os_error()))
        }
    }
    #[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
    fn rename_directory_noreplace(
        _source_parent: &File,
        _source: &std::ffi::OsStr,
        _destination_parent: &File,
        _destination: &std::ffi::OsStr,
        label: &Path,
    ) -> Result<()> {
        Err(err(
            label,
            "atomic no-replace directory rename is unsupported",
        ))
    }
    pub(super) fn ensure_owner_private(root: &File, relative: &Path, label: &Path) -> Result<()> {
        let (parent, leaf) = parent(root, relative, label)?;
        let p = full(label, relative);
        let f = open_leaf(&parent, leaf, libc::O_RDONLY, 0, &p)?;
        let m = safe_regular(&f, &p)?;
        let uid = unsafe { libc::geteuid() };
        if m.uid() != uid || m.mode() & 0o077 != 0 {
            return Err(err(&p, "store leaf is not owner-private"));
        }
        Ok(())
    }
    pub(super) fn entries(root: &File, relative: &Path, label: &Path) -> Result<Vec<StoreEntry>> {
        let d = directory(root, relative, label)?;
        let mut v = Vec::new();
        for entry in cap_fs::read_dir(&d, Path::new(".")).map_err(|e| ioerr(label, e))? {
            let entry = entry.map_err(|e| ioerr(label, e))?;
            let name = entry.file_name();
            let mut opts = cap_fs::OpenOptions::new();
            opts.read(true)
                ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
            opts.custom_flags(libc::O_NONBLOCK);
            let child = cap_fs::open(&d, Path::new(&name), &opts)
                .map_err(|_| err(label, "store entry is unsafe"))?;
            let m = child.metadata().map_err(|e| ioerr(label, e))?;
            if !(m.is_dir() || (m.is_file() && m.nlink() == 1)) {
                return Err(err(label, "store entry is unsafe"));
            }
            v.push(StoreEntry {
                name,
                is_directory: m.is_dir(),
                is_regular_file: m.is_file(),
            });
        }
        v.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(v)
    }
    pub(super) fn entries_optional(
        root: &File,
        relative: &Path,
        label: &Path,
    ) -> Result<Option<Vec<StoreEntry>>> {
        let components = relative_components(relative, true, label)?;
        let mut d = cap_fs::open_dir_nofollow(root, Path::new(".")).map_err(|e| ioerr(label, e))?;
        for c in components {
            match cap_fs::open_dir_nofollow(&d, Path::new(c)) {
                Ok(next) => d = next,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(_) => return Err(err(&full(label, relative), "store directory is unsafe")),
            }
        }
        entries(root, relative, label).map(Some)
    }
    pub(super) fn sync_directory(d: &File, label: &Path) -> Result<()> {
        let operation = operation_directory(d, label)?;
        operation.sync_all().map_err(|e| ioerr(label, e))
    }
    pub(super) fn restrict_new_private_directory(d: &File) -> Result<()> {
        let operation = operation_directory(d, Path::new("<retained>"))?;
        let m = operation
            .metadata()
            .map_err(|e| ioerr(Path::new("<retained>"), e))?;
        if !m.is_dir() {
            return Err(err(
                Path::new("<retained>"),
                "new private object is not a directory",
            ));
        }
        let mut perms = m.permissions();
        perms.set_mode(0o700);
        operation
            .set_permissions(perms)
            .map_err(|e| ioerr(Path::new("<retained>"), e))
    }

    /// Obtain an operation-capable duplicate of a retained directory without
    /// resolving its diagnostic pathname. Linux may expose `open_dir_nofollow`
    /// handles as O_PATH, which can be inspected but cannot be chmod'ed or
    /// fsync'ed. Opening `.` relative to that descriptor retains the same
    /// namespace authority while yielding a normal directory descriptor.
    fn operation_directory(held: &File, label: &Path) -> Result<File> {
        let before = held.metadata().map_err(|e| ioerr(label, e))?;
        if !before.is_dir() {
            return Err(err(label, "retained store handle is not a directory"));
        }
        let dot = CString::new(".").expect("fixed directory component");
        // SAFETY: `held` is retained by the caller and `dot` is a fixed,
        // NUL-terminated single component. O_NOFOLLOW prevents a substitution
        // through a link while O_DIRECTORY requires a directory result.
        let fd = unsafe {
            libc::openat(
                held.as_raw_fd(),
                dot.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(ioerr(label, std::io::Error::last_os_error()));
        }
        // SAFETY: successful openat returned exactly one owned descriptor.
        let operation = unsafe { File::from_raw_fd(fd) };
        let after = operation.metadata().map_err(|e| ioerr(label, e))?;
        if !same_directory_identity(&before, &after) {
            return Err(err(label, "retained store directory identity changed"));
        }
        Ok(operation)
    }

    #[cfg(test)]
    mod tests {
        use super::same_directory_identity;
        use std::fs;

        #[test]
        fn directory_identity_ignores_namespace_metadata_but_rejects_other_objects() {
            let first = tempfile::tempdir().unwrap();
            let before = fs::metadata(first.path()).unwrap();
            fs::write(first.path().join("child"), b"namespace mutation").unwrap();
            let after = fs::metadata(first.path()).unwrap();
            assert!(same_directory_identity(&before, &after));

            let second = tempfile::tempdir().unwrap();
            let other_directory = fs::metadata(second.path()).unwrap();
            assert!(!same_directory_identity(&before, &other_directory));

            let file_path = first.path().join("regular");
            fs::write(&file_path, b"regular").unwrap();
            let regular = fs::metadata(file_path).unwrap();
            assert!(!same_directory_identity(&before, &regular));
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::{
        MAX_DIRECTORY_REMOVAL_DEPTH, MAX_DIRECTORY_REMOVAL_ENTRIES, Publication, Result,
        StoreEntry, err, ioerr, relative_components,
    };
    use cap_primitives::fs::OpenOptionsExt;
    use cap_primitives::{ambient_authority, fs as cap_fs};
    use std::{
        fs::File,
        io::{Read, Write},
        mem,
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle},
        },
        path::{Component, Path, PathBuf, Prefix},
        ptr,
        sync::atomic::{AtomicU64, Ordering},
    };
    use windows_sys::Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{
            FILE_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT, FILE_RENAME_INFORMATION,
            FILE_SYNCHRONOUS_IO_NONALERT, FileRenameInformation, NtCreateFile,
            NtSetInformationFile,
        },
    };
    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
            OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING,
        },
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, DELETE, FILE_BASIC_INFO, FILE_READ_ATTRIBUTES,
            FILE_READ_DATA, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileBasicInfo,
            GetFileInformationByHandle, GetFileInformationByHandleEx, READ_CONTROL, SYNCHRONIZE,
            WRITE_DAC,
        },
        System::IO::IO_STATUS_BLOCK,
    };
    static TEMP: AtomicU64 = AtomicU64::new(0);
    fn full(label: &Path, relative: &Path) -> std::path::PathBuf {
        label.join(relative)
    }
    pub(super) fn open(path: &Path) -> Result<File> {
        let (root_path, components) = absolute_components(path)?;
        let mut f = cap_fs::open_ambient_dir(&root_path, ambient_authority())
            .map_err(|_| err(path, "cannot open Windows store root"))?;
        validate_directory(&f, path)?;
        for component in components {
            f = cap_fs::open_dir_nofollow(&f, Path::new(component))
                .map_err(|_| err(path, "store path has a missing or reparse component"))?;
            validate_directory(&f, path)?;
        }
        Ok(f)
    }
    fn absolute_components(path: &Path) -> Result<(PathBuf, Vec<&std::ffi::OsStr>)> {
        let mut iter = path.components();
        let Some(Component::Prefix(prefix)) = iter.next() else {
            return Err(err(path, "Windows store path must be absolute drive path"));
        };
        if !matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
            || !matches!(iter.next(), Some(Component::RootDir))
        {
            return Err(err(
                path,
                "Windows store path must be an absolute drive path",
            ));
        }
        let mut root = PathBuf::from(prefix.as_os_str());
        root.push(Path::new(r"\"));
        let mut components = Vec::new();
        for c in iter {
            match c {
                Component::Normal(v) => components.push(v),
                _ => return Err(err(path, "Windows store path contains unsafe component")),
            }
        }
        if components.is_empty() {
            return Err(err(path, "Windows store path requires a directory leaf"));
        }
        Ok((root, components))
    }
    fn change_time(file: &File, label: &Path) -> Result<i64> {
        let mut basic = FILE_BASIC_INFO::default();
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle() as HANDLE,
                FileBasicInfo,
                (&mut basic as *mut FILE_BASIC_INFO).cast(),
                mem::size_of::<FILE_BASIC_INFO>() as u32,
            )
        } == 0
        {
            return Err(ioerr(label, std::io::Error::last_os_error()));
        }
        Ok(basic.ChangeTime)
    }
    fn volume_serial(file: &File, label: &Path) -> Result<u32> {
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        if unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut information) }
            == 0
        {
            return Err(ioerr(label, std::io::Error::last_os_error()));
        }
        Ok(information.dwVolumeSerialNumber)
    }
    pub(super) fn validate_directory(file: &File, label: &Path) -> Result<()> {
        if crate::cas::windows_directory_handle_identity(file).is_none() {
            Err(err(label, "retained store handle is not a real directory"))
        } else {
            Ok(())
        }
    }
    fn operation_directory(directory: &File, label: &Path) -> Result<File> {
        let identity = crate::cas::windows_directory_handle_identity(directory)
            .ok_or_else(|| err(label, "retained store handle is not a real directory"))?;
        // Windows rejects a retained-relative `.` open for a directory.  An
        // empty native name denotes the retained directory itself without
        // resolving its diagnostic path or adopting an ambient namespace.
        let mut empty = UNICODE_STRING {
            Length: 0,
            MaximumLength: 0,
            Buffer: ptr::null_mut(),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: directory.as_raw_handle() as HANDLE,
            ObjectName: &mut empty,
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: ptr::null_mut(),
            SecurityQualityOfService: ptr::null(),
        };
        let mut handle = INVALID_HANDLE_VALUE;
        let mut status_block = IO_STATUS_BLOCK::default();
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                GENERIC_READ | GENERIC_WRITE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                &attributes,
                &mut status_block,
                ptr::null(),
                windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_OPEN,
                FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT,
                ptr::null(),
                0,
            )
        };
        if status != 0 || unsafe { status_block.Anonymous.Status } != 0 {
            if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                unsafe { CloseHandle(handle) };
            }
            return Err(ioerr(
                label,
                nt_status_error(if status != 0 {
                    status
                } else {
                    unsafe { status_block.Anonymous.Status }
                }),
            ));
        }
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(err(
                label,
                "native retained directory open returned an invalid handle",
            ));
        }
        let operation = unsafe { File::from_raw_handle(handle as _) };
        if crate::cas::windows_directory_handle_identity(&operation) != Some(identity) {
            return Err(err(label, "retained store directory identity changed"));
        }
        Ok(operation)
    }
    fn nt_status_error(status: i32) -> std::io::Error {
        std::io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) as i32 })
    }
    pub(super) fn normalize_directory_handle(file: File, label: &Path) -> Result<File> {
        validate_directory(&file, label)?;
        Ok(file)
    }
    fn directory(root: &File, relative: &Path, label: &Path) -> Result<File> {
        let mut d = cap_fs::open_dir_nofollow(root, Path::new(".")).map_err(|e| ioerr(label, e))?;
        for c in relative_components(relative, true, label)? {
            d = cap_fs::open_dir_nofollow(&d, Path::new(c)).map_err(|_| {
                err(
                    &full(label, relative),
                    "store directory is missing or reparse",
                )
            })?;
        }
        validate_directory(&d, &full(label, relative))?;
        Ok(d)
    }
    fn parent<'a>(
        root: &File,
        relative: &'a Path,
        label: &Path,
    ) -> Result<(File, &'a std::ffi::OsStr)> {
        let c = relative_components(relative, false, label)?;
        let leaf = *c.last().unwrap();
        let mut d = cap_fs::open_dir_nofollow(root, Path::new(".")).map_err(|e| ioerr(label, e))?;
        for part in &c[..c.len() - 1] {
            d = cap_fs::open_dir_nofollow(&d, Path::new(part))
                .map_err(|_| err(label, "store parent is missing or reparse"))?;
        }
        Ok((d, leaf))
    }
    fn leaf(
        parent: &File,
        name: &std::ffi::OsStr,
        read: bool,
        write: bool,
        append: bool,
        label: &Path,
    ) -> Result<File> {
        let mut o = cap_fs::OpenOptions::new();
        o.read(read)
            .write(write)
            .append(append)
            ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
        cap_fs::open(parent, Path::new(name), &o).map_err(|e| ioerr(label, e))
    }
    fn parent_optional<'a>(
        root: &File,
        relative: &'a Path,
        label: &Path,
    ) -> Result<Option<(File, &'a std::ffi::OsStr)>> {
        let c = relative_components(relative, false, label)?;
        let leaf = *c.last().unwrap();
        let mut d = cap_fs::open_dir_nofollow(root, Path::new(".")).map_err(|e| ioerr(label, e))?;
        for part in &c[..c.len() - 1] {
            match cap_fs::open_dir_nofollow(&d, Path::new(part)) {
                Ok(next) => d = next,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(_) => return Err(err(label, "store parent is unsafe")),
            }
        }
        Ok(Some((d, leaf)))
    }
    fn leaf_optional(parent: &File, name: &std::ffi::OsStr, label: &Path) -> Result<Option<File>> {
        let mut o = cap_fs::OpenOptions::new();
        o.read(true)._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
        match cap_fs::open(parent, Path::new(name), &o) {
            Ok(f) => Ok(Some(f)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ioerr(label, e)),
        }
    }
    fn regular(file: &File, label: &Path) -> Result<std::fs::Metadata> {
        if crate::cas::windows_regular_file_handle_identity(file).is_none() {
            return Err(err(
                label,
                "store leaf is reparse, nonregular, or hardlinked",
            ));
        }
        file.metadata().map_err(|e| ioerr(label, e))
    }
    pub(super) fn open_directory(root: &File, relative: &Path, label: &Path) -> Result<File> {
        directory(root, relative, label)
    }
    pub(super) fn create_directory_all(root: &File, relative: &Path, label: &Path) -> Result<File> {
        let mut d = cap_fs::open_dir_nofollow(root, Path::new(".")).map_err(|e| ioerr(label, e))?;
        for c in relative_components(relative, true, label)? {
            match cap_fs::open_dir_nofollow(&d, Path::new(c)) {
                Ok(next) => d = next,
                Err(_) => {
                    d = crate::private_fs::create_new_private_at(
                        &d,
                        c,
                        crate::private_fs::NewPrivateObjectKind::Directory,
                    )
                    .map_err(|e| ioerr(label, e))?;
                    validate_directory(&d, label)?;
                    crate::private_fs::verify_owner_private_handle(&d)?;
                    sync_directory(&d, label)?;
                }
            }
        }
        Ok(d)
    }
    pub(super) fn create_directory(root: &File, relative: &Path, label: &Path) -> Result<File> {
        let (parent, leaf) = parent(root, relative, label)?;
        let path = full(label, relative);
        let created = crate::private_fs::create_new_private_at(
            &parent,
            leaf,
            crate::private_fs::NewPrivateObjectKind::Directory,
        )
        .map_err(|e| ioerr(&path, e))?;
        validate_directory(&created, &path)?;
        crate::private_fs::verify_owner_private_handle(&created)?;
        sync_directory(&parent, label)?;
        Ok(created)
    }
    pub(super) fn open_regular_read(
        root: &File,
        relative: &Path,
        max: u64,
        label: &Path,
    ) -> Result<File> {
        let (p, n) = parent(root, relative, label)?;
        let path = full(label, relative);
        let f = leaf(&p, n, true, false, false, &path)?;
        if regular(&f, &path)?.len() > max {
            return Err(err(&path, "store file exceeds byte limit"));
        }
        Ok(f)
    }
    pub(super) fn open_regular_for_removal(
        root: &File,
        relative: &Path,
        max: u64,
        label: &Path,
    ) -> Result<File> {
        let (parent, leaf) = parent(root, relative, label)?;
        let path = full(label, relative);
        let mut options = cap_fs::OpenOptions::new();
        options
            .read(true)
            .access_mode(
                FILE_READ_DATA
                    | READ_CONTROL
                    | WRITE_DAC
                    | DELETE
                    | FILE_READ_ATTRIBUTES
                    | SYNCHRONIZE,
            )
            .share_mode(FILE_SHARE_READ)
            ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
        let file = cap_fs::open(&parent, Path::new(leaf), &options).map_err(|e| ioerr(&path, e))?;
        if regular(&file, &path)?.len() > max {
            return Err(err(&path, "store file exceeds byte limit"));
        }
        Ok(file)
    }
    pub(super) fn read_optional(
        root: &File,
        relative: &Path,
        max: u64,
        label: &Path,
    ) -> Result<Option<Vec<u8>>> {
        let Some((p, n)) = parent_optional(root, relative, label)? else {
            return Ok(None);
        };
        let path = full(label, relative);
        let Some(mut f) = leaf_optional(&p, n, &path)? else {
            return Ok(None);
        };
        let before = regular(&f, &path)?;
        let before_identity = crate::cas::windows_regular_file_handle_identity(&f)
            .ok_or_else(|| err(&path, "store leaf changed type while being read"))?;
        let before_change = change_time(&f, &path)?;
        if before.len() > max {
            return Err(err(&path, "store file exceeds byte limit"));
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut f)
            .take(max.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|e| ioerr(&path, e))?;
        let after = regular(&f, &path)?;
        let after_identity = crate::cas::windows_regular_file_handle_identity(&f)
            .ok_or_else(|| err(&path, "store leaf changed type while being read"))?;
        let after_change = change_time(&f, &path)?;
        if bytes.len() as u64 > max
            || before_identity != after_identity
            || before.len() != after.len()
            || before_change != after_change
            || after.len() != bytes.len() as u64
        {
            return Err(err(&path, "store file changed while it was read"));
        }
        Ok(Some(bytes))
    }
    pub(super) fn create_private_file_raw(root: &File, leaf: &Path, label: &Path) -> Result<File> {
        let leaf = direct_leaf(leaf, label)?;
        let path = label.join(leaf);
        let file = crate::private_fs::create_new_private_at(
            root,
            leaf,
            crate::private_fs::NewPrivateObjectKind::RegularFile,
        )
        .map_err(|e| ioerr(&path, e))?;
        regular(&file, &path)?;
        crate::private_fs::verify_owner_private_handle(&file)?;
        Ok(file)
    }
    pub(super) fn open_private_gate_raw(root: &File, leaf: &Path, label: &Path) -> Result<File> {
        let leaf = direct_leaf(leaf, label)?;
        let path = label.join(leaf);
        match create_private_file_raw(root, Path::new(leaf), label) {
            Ok(file) => {
                if file.metadata().map_err(|e| ioerr(&path, e))?.len() != 0 {
                    return Err(err(&path, "private gate file is not empty"));
                }
                Ok(file)
            }
            Err(error) if is_io_kind(&error, "already_exists") => {
                let mut options = cap_fs::OpenOptions::new();
                options
                    .read(true)
                    .write(true)
                    ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
                let file =
                    cap_fs::open(root, Path::new(leaf), &options).map_err(|e| ioerr(&path, e))?;
                if regular(&file, &path)?.len() != 0 {
                    return Err(err(&path, "private gate file is not empty"));
                }
                crate::private_fs::verify_owner_private_handle(&file)?;
                Ok(file)
            }
            Err(error) => Err(error),
        }
    }
    pub(super) fn rename_regular_between_raw(
        source_parent: &File,
        source_leaf: &Path,
        destination_parent: &File,
        destination_leaf: &Path,
        publication: Publication,
        source_label: &Path,
        destination_label: &Path,
    ) -> Result<()> {
        use windows_sys::Win32::Foundation::GENERIC_READ;

        let source = direct_leaf(source_leaf, source_label)?;
        let destination = direct_leaf(destination_leaf, destination_label)?;
        let source_path = source_label.join(source);
        let destination_path = destination_label.join(destination);
        validate_directory(source_parent, source_label)?;
        validate_directory(destination_parent, destination_label)?;
        if volume_serial(source_parent, source_label)?
            != volume_serial(destination_parent, destination_label)?
        {
            return Err(err(
                &destination_path,
                "regular rename crosses filesystem volumes",
            ));
        }
        let mut source_options = cap_fs::OpenOptions::new();
        source_options
            .read(true)
            .access_mode(GENERIC_READ | DELETE | SYNCHRONIZE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
        let source_file = cap_fs::open(source_parent, Path::new(source), &source_options)
            .map_err(|e| ioerr(&source_path, e))?;
        let source_identity = crate::cas::windows_regular_file_handle_identity(&source_file)
            .ok_or_else(|| err(&source_path, "source is not a single-link regular file"))?;
        match publication {
            Publication::CreateOnly => rename_regular_handle(
                &source_file,
                destination_parent,
                destination,
                false,
                &destination_path,
            )?,
            Publication::Replace => {
                let old = leaf(
                    destination_parent,
                    destination,
                    true,
                    false,
                    false,
                    &destination_path,
                )?;
                regular(&old, &destination_path)?;
                drop(old);
                rename_regular_handle(
                    &source_file,
                    destination_parent,
                    destination,
                    true,
                    &destination_path,
                )?;
            }
            Publication::Upsert => match rename_regular_handle(
                &source_file,
                destination_parent,
                destination,
                false,
                &destination_path,
            ) {
                Ok(()) => {}
                Err(error) if is_io_kind(&error, "already_exists") => {
                    let old = leaf(
                        destination_parent,
                        destination,
                        true,
                        false,
                        false,
                        &destination_path,
                    )?;
                    regular(&old, &destination_path)?;
                    drop(old);
                    rename_regular_handle(
                        &source_file,
                        destination_parent,
                        destination,
                        true,
                        &destination_path,
                    )?;
                }
                Err(error) => return Err(error),
            },
        }
        let published = leaf(
            destination_parent,
            destination,
            true,
            false,
            false,
            &destination_path,
        )?;
        if crate::cas::windows_regular_file_handle_identity(&published) != Some(source_identity) {
            return Err(err(
                &destination_path,
                "published regular-file identity changed",
            ));
        }
        sync_directory(source_parent, source_label)?;
        sync_directory(destination_parent, destination_label)
    }
    pub(super) fn move_verified_regular_handle_to_raw(
        source_parent: &File,
        source: &File,
        destination_parent: &File,
        destination_leaf: &Path,
        source_label: &Path,
        destination_label: &Path,
    ) -> Result<()> {
        let destination = direct_leaf(destination_leaf, destination_label)?;
        let destination_path = destination_label.join(destination);
        validate_directory(source_parent, source_label)?;
        validate_directory(destination_parent, destination_label)?;
        if volume_serial(source_parent, source_label)?
            != volume_serial(destination_parent, destination_label)?
        {
            return Err(err(
                &destination_path,
                "regular rename crosses filesystem volumes",
            ));
        }
        let source_identity = crate::cas::windows_regular_file_handle_identity(source)
            .ok_or_else(|| err(source_label, "source is not a single-link regular file"))?;
        rename_regular_handle(
            source,
            destination_parent,
            destination,
            false,
            &destination_path,
        )?;
        if crate::cas::windows_regular_file_handle_identity(source) != Some(source_identity) {
            return Err(err(
                &destination_path,
                "moved regular-file handle identity changed",
            ));
        }
        sync_directory(source_parent, source_label)?;
        sync_directory(destination_parent, destination_label)
    }
    fn rename_regular_handle(
        source: &File,
        destination_parent: &File,
        destination: &std::ffi::OsStr,
        replace: bool,
        label: &Path,
    ) -> Result<()> {
        rename_retained_handle(source, destination_parent, destination, replace, label)
    }
    fn rename_retained_handle(
        source: &File,
        destination_parent: &File,
        destination: &std::ffi::OsStr,
        replace: bool,
        label: &Path,
    ) -> Result<()> {
        let name = destination.encode_wide().collect::<Vec<_>>();
        if name.is_empty() || name.contains(&0) {
            return Err(err(label, "retained rename destination is invalid"));
        }
        let name_bytes = name
            .len()
            .checked_mul(mem::size_of::<u16>())
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| err(label, "retained rename destination is too long"))?;
        // FILE_RENAME_INFORMATION has one trailing WCHAR and may contain
        // alignment padding after it.  Allocate from the real FileName offset,
        // include a terminating zero, and retain usize alignment for the native
        // structure.  The source and destination capabilities remain live for
        // the synchronous NtSetInformationFile call.
        let total = mem::offset_of!(FILE_RENAME_INFORMATION, FileName)
            .checked_add(name_bytes as usize)
            .and_then(|value| value.checked_add(mem::size_of::<u16>()))
            .ok_or_else(|| err(label, "retained rename buffer overflow"))?;
        let words = total
            .checked_add(mem::size_of::<usize>() - 1)
            .ok_or_else(|| err(label, "retained rename buffer overflow"))?
            / mem::size_of::<usize>();
        let mut buffer = vec![0_usize; words];
        let mut status_block = IO_STATUS_BLOCK::default();
        let status = unsafe {
            let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
            (*info).Anonymous.ReplaceIfExists = replace;
            (*info).RootDirectory = destination_parent.as_raw_handle() as HANDLE;
            (*info).FileNameLength = name_bytes;
            ptr::copy_nonoverlapping(
                name.as_ptr(),
                ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
                name.len(),
            );
            NtSetInformationFile(
                source.as_raw_handle() as HANDLE,
                &mut status_block,
                buffer.as_ptr().cast(),
                u32::try_from(total).map_err(|_| err(label, "retained rename buffer overflow"))?,
                FileRenameInformation,
            )
        };
        if status != 0 || unsafe { status_block.Anonymous.Status } != 0 {
            return Err(ioerr(
                label,
                nt_status_error(if status != 0 {
                    status
                } else {
                    unsafe { status_block.Anonymous.Status }
                }),
            ));
        }
        Ok(())
    }
    fn is_io_kind(error: &crate::KioError, kind: &str) -> bool {
        error.error_code() == "KIO-E-STORE-IO-001"
            && error
                .context()
                .get("io_error_kind")
                .and_then(serde_json::Value::as_str)
                == Some(kind)
    }
    pub(super) fn append(root: &File, relative: &Path, bytes: &[u8], label: &Path) -> Result<()> {
        let (p, n) = parent(root, relative, label)?;
        let path = full(label, relative);
        let mut f = leaf(&p, n, false, true, true, &path)?;
        regular(&f, &path)?;
        f.write_all(bytes).map_err(|e| ioerr(&path, e))?;
        f.sync_data().map_err(|e| ioerr(&path, e))
    }
    pub(super) fn remove_file(root: &File, relative: &Path, label: &Path) -> Result<()> {
        let (p, n) = parent(root, relative, label)?;
        let path = full(label, relative);
        let f = leaf(&p, n, true, false, false, &path)?;
        regular(&f, &path)?;
        drop(f);
        cap_fs::remove_file(&p, Path::new(n)).map_err(|e| ioerr(&path, e))?;
        sync_directory(&p, label)
    }
    pub(super) fn rename_directory_create_only(
        root: &File,
        source: &Path,
        destination: &Path,
        label: &Path,
    ) -> Result<()> {
        let (parent, source_leaf, destination_leaf) =
            same_parent(root, source, destination, label)?;
        let source_path = full(label, source);
        let destination_path = full(label, destination);
        let mut source_options = cap_fs::OpenOptions::new();
        source_options
            .access_mode(DELETE | SYNCHRONIZE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
        let source_directory = cap_fs::open(&parent, Path::new(source_leaf), &source_options)
            .map_err(|_| err(&source_path, "source directory is missing or unsafe"))?;
        validate_directory(&source_directory, &source_path)?;
        rename_directory_noreplace(&source_directory, &parent, destination_leaf, &source_path)?;
        let published =
            cap_fs::open_dir_nofollow(&parent, Path::new(destination_leaf)).map_err(|_| {
                err(
                    &destination_path,
                    "published directory is missing or unsafe",
                )
            })?;
        if crate::cas::windows_directory_handle_identity(&source_directory)
            != crate::cas::windows_directory_handle_identity(&published)
        {
            return Err(err(
                &destination_path,
                "published directory identity changed",
            ));
        }
        sync_directory(&parent, label)
    }
    pub(super) fn rename_directory_between_create_only(
        source_parent: &File,
        source_leaf: &Path,
        destination_parent: &File,
        destination_leaf: &Path,
        source_label: &Path,
        destination_label: &Path,
    ) -> Result<()> {
        let source = direct_leaf(source_leaf, source_label)?;
        let destination = direct_leaf(destination_leaf, destination_label)?;
        let mut options = cap_fs::OpenOptions::new();
        options
            .access_mode(DELETE | SYNCHRONIZE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
        let held = cap_fs::open(source_parent, Path::new(source), &options)
            .map_err(|_| err(source_label, "source directory is missing or unsafe"))?;
        validate_directory(&held, source_label)?;
        rename_directory_noreplace(&held, destination_parent, destination, source_label)?;
        let published = cap_fs::open_dir_nofollow(destination_parent, Path::new(destination))
            .map_err(|_| {
                err(
                    destination_label,
                    "published directory is missing or unsafe",
                )
            })?;
        if crate::cas::windows_directory_handle_identity(&held)
            != crate::cas::windows_directory_handle_identity(&published)
        {
            return Err(err(
                destination_label,
                "published directory identity changed",
            ));
        }
        sync_directory(source_parent, source_label)?;
        sync_directory(destination_parent, destination_label)
    }
    fn direct_leaf<'a>(path: &'a Path, label: &Path) -> Result<&'a std::ffi::OsStr> {
        let c = relative_components(path, false, label)?;
        if c.len() != 1 {
            return Err(err(label, "directory rename requires one direct leaf"));
        }
        Ok(c[0])
    }
    pub(super) fn quarantine_directory(
        root: &File,
        relative: &Path,
        label: &Path,
    ) -> Result<PathBuf> {
        let components = relative_components(relative, false, label)?;
        let mut destination = PathBuf::new();
        for component in &components[..components.len() - 1] {
            destination.push(component);
        }
        destination.push(format!(
            ".kio-directory-quarantine-{}-{}",
            std::process::id(),
            TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        rename_directory_create_only(root, relative, &destination, label)?;
        Ok(destination)
    }
    pub(super) fn remove_directory_all(root: &File, relative: &Path, label: &Path) -> Result<()> {
        let (parent, leaf) = parent(root, relative, label)?;
        let path = full(label, relative);
        let directory = cap_fs::open_dir_nofollow(&parent, Path::new(leaf))
            .map_err(|_| err(&path, "directory removal target is missing or unsafe"))?;
        validate_directory(&directory, &path)?;
        let mut remaining = MAX_DIRECTORY_REMOVAL_ENTRIES;
        remove_directory_contents(&directory, &path, 0, &mut remaining)?;
        drop(directory);
        cap_fs::remove_dir(&parent, Path::new(leaf)).map_err(|error| ioerr(&path, error))?;
        sync_directory(&parent, label)
    }
    fn same_parent<'a>(
        root: &File,
        source: &'a Path,
        destination: &'a Path,
        label: &Path,
    ) -> Result<(File, &'a std::ffi::OsStr, &'a std::ffi::OsStr)> {
        let source_components = relative_components(source, false, label)?;
        let destination_components = relative_components(destination, false, label)?;
        if source_components.len() != destination_components.len()
            || source_components[..source_components.len() - 1]
                != destination_components[..destination_components.len() - 1]
        {
            return Err(err(label, "directory rename requires the same parent"));
        }
        let (parent, source_leaf) = parent(root, source, label)?;
        Ok((
            parent,
            source_leaf,
            *destination_components.last().expect("nonempty destination"),
        ))
    }
    fn remove_directory_contents(
        directory: &File,
        label: &Path,
        depth: usize,
        remaining: &mut usize,
    ) -> Result<()> {
        if depth >= MAX_DIRECTORY_REMOVAL_DEPTH {
            return Err(err(label, "directory removal exceeds maximum depth"));
        }
        for entry in
            cap_fs::read_dir(directory, Path::new(".")).map_err(|error| ioerr(label, error))?
        {
            let entry = entry.map_err(|error| ioerr(label, error))?;
            *remaining = remaining
                .checked_sub(1)
                .ok_or_else(|| err(label, "directory removal exceeds entry limit"))?;
            let name = entry.file_name();
            let child_label = label.join(&name);
            match cap_fs::open_dir_nofollow(directory, Path::new(&name)) {
                Ok(child) => {
                    validate_directory(&child, &child_label)?;
                    remove_directory_contents(&child, &child_label, depth + 1, remaining)?;
                    drop(child);
                    cap_fs::remove_dir(directory, Path::new(&name))
                        .map_err(|error| ioerr(&child_label, error))?;
                }
                Err(_) => {
                    let mut options = cap_fs::OpenOptions::new();
                    options
                        .read(true)
                        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
                    let file = cap_fs::open(directory, Path::new(&name), &options)
                        .map_err(|_| err(&child_label, "directory removal entry is unsafe"))?;
                    regular(&file, &child_label)?;
                    drop(file);
                    cap_fs::remove_file(directory, Path::new(&name))
                        .map_err(|error| ioerr(&child_label, error))?;
                }
            }
        }
        sync_directory(directory, label)
    }
    fn rename_directory_noreplace(
        source: &File,
        parent: &File,
        destination: &std::ffi::OsStr,
        label: &Path,
    ) -> Result<()> {
        rename_retained_handle(source, parent, destination, false, label)
    }
    pub(super) fn ensure_owner_private(root: &File, relative: &Path, label: &Path) -> Result<()> {
        let f = open_regular_read(root, relative, u64::MAX, label)?;
        regular(&f, &full(label, relative))?;
        crate::private_fs::verify_owner_private_handle(&f)
    }
    pub(super) fn entries(root: &File, relative: &Path, label: &Path) -> Result<Vec<StoreEntry>> {
        let d = directory(root, relative, label)?;
        let mut out = Vec::new();
        for entry in cap_fs::read_dir(&d, Path::new(".")).map_err(|e| ioerr(label, e))? {
            let entry = entry.map_err(|e| ioerr(label, e))?;
            let name = entry.file_name();
            let mut o = cap_fs::OpenOptions::new();
            o.read(true)._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
            let child = cap_fs::open(&d, Path::new(&name), &o)
                .map_err(|_| err(label, "store entry is reparse"))?;
            let meta = child.metadata().map_err(|e| ioerr(label, e))?;
            if !meta.is_dir() {
                regular(&child, label)?;
            }
            out.push(StoreEntry {
                name,
                is_directory: meta.is_dir(),
                is_regular_file: meta.is_file(),
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }
    pub(super) fn entries_optional(
        root: &File,
        relative: &Path,
        label: &Path,
    ) -> Result<Option<Vec<StoreEntry>>> {
        let components = relative_components(relative, true, label)?;
        let mut d = cap_fs::open_dir_nofollow(root, Path::new(".")).map_err(|e| ioerr(label, e))?;
        for c in components {
            match cap_fs::open_dir_nofollow(&d, Path::new(c)) {
                Ok(next) => d = next,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(_) => return Err(err(label, "store directory is unsafe")),
            }
        }
        entries(root, relative, label).map(Some)
    }
    pub(super) fn sync_directory(directory: &File, label: &Path) -> Result<()> {
        let operation = operation_directory(directory, label)?;
        operation.sync_all().map_err(|e| ioerr(label, e))
    }
    pub(super) fn restrict_new_private_directory(directory: &File) -> Result<()> {
        validate_directory(directory, Path::new("<retained>"))?;
        crate::private_fs::verify_owner_private_handle(directory)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::store_dir::StoreDirectory;
        use std::{
            fs::{self, OpenOptions},
            os::windows::fs::OpenOptionsExt as _,
        };

        fn directory(path: &Path) -> StoreDirectory {
            fs::create_dir(path).expect("directory fixture");
            StoreDirectory::open(path).expect("retained directory")
        }

        #[test]
        fn verified_handle_move_does_not_adopt_a_replaced_source_name() {
            let root = tempfile::tempdir().expect("fixture");
            let source_parent = directory(&root.path().join("source"));
            let destination_parent = directory(&root.path().join("destination"));
            let source_path = source_parent.path().join("leaf");
            fs::write(&source_path, b"validated").expect("source body");
            let source = OpenOptions::new()
                .read(true)
                .access_mode(FILE_READ_DATA | FILE_READ_ATTRIBUTES | DELETE)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .open(&source_path)
                .expect("share-all source handle");

            fs::rename(&source_path, source_parent.path().join("displaced"))
                .expect("replace namespace source");
            fs::write(&source_path, b"replacement").expect("replacement body");
            let source_handle = source_parent.root_handle();
            let destination_handle = destination_parent.root_handle();
            move_verified_regular_handle_to_raw(
                &source_handle,
                &source,
                &destination_handle,
                Path::new("removed"),
                source_parent.path(),
                destination_parent.path(),
            )
            .expect("move retained source handle");

            assert_eq!(
                fs::read(destination_parent.path().join("removed")).unwrap(),
                b"validated"
            );
            assert_eq!(fs::read(&source_path).unwrap(), b"replacement");
        }

        #[test]
        fn removal_open_blocks_new_writers_and_renames() {
            let root = tempfile::tempdir().expect("fixture");
            let directory = directory(&root.path().join("source"));
            let source_path = directory.path().join("leaf");
            fs::write(&source_path, b"source").expect("source body");
            let root_handle = directory.root_handle();
            let held =
                open_regular_for_removal(&root_handle, Path::new("leaf"), 64, directory.path())
                    .expect("retained removal handle");

            assert!(fs::rename(&source_path, directory.path().join("renamed")).is_err());
            assert!(OpenOptions::new().write(true).open(&source_path).is_err());
            drop(held);
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::{Publication, Result, StoreEntry, err};
    use std::{
        fs::File,
        path::{Path, PathBuf},
    };
    macro_rules! unsupported {($n:ident($($a:ident:$t:ty),*)->$r:ty)=>{pub(super) fn $n($($a:$t),*)->Result<$r>{Err(err(Path::new("<retained>"),"StoreDirectory is unsupported on this platform"))}};}
    unsupported!(open(path:&Path)->File);
    unsupported!(validate_directory(file:&File,label:&Path)->());
    unsupported!(normalize_directory_handle(file:File,label:&Path)->File);
    unsupported!(open_directory(root:&File,path:&Path,label:&Path)->File);
    unsupported!(create_directory_all(root:&File,path:&Path,label:&Path)->File);
    unsupported!(create_directory(root:&File,path:&Path,label:&Path)->File);
    unsupported!(open_regular_read(root:&File,path:&Path,max:u64,label:&Path)->File);
    unsupported!(open_regular_for_removal(root:&File,path:&Path,max:u64,label:&Path)->File);
    unsupported!(read_optional(root:&File,path:&Path,max:u64,label:&Path)->Option<Vec<u8>>);
    unsupported!(create_private_file_raw(root:&File,path:&Path,label:&Path)->File);
    unsupported!(open_private_gate_raw(root:&File,path:&Path,label:&Path)->File);
    unsupported!(rename_regular_between_raw(source_parent:&File,source_leaf:&Path,destination_parent:&File,destination_leaf:&Path,p:Publication,source_label:&Path,destination_label:&Path)->());
    unsupported!(append(root:&File,path:&Path,data:&[u8],label:&Path)->());
    unsupported!(remove_file(root:&File,path:&Path,label:&Path)->());
    unsupported!(rename_directory_create_only(root:&File,source:&Path,destination:&Path,label:&Path)->());
    unsupported!(rename_directory_between_create_only(source_parent:&File,source_leaf:&Path,destination_parent:&File,destination_leaf:&Path,source_label:&Path,destination_label:&Path)->());
    unsupported!(quarantine_directory(root:&File,path:&Path,label:&Path)->PathBuf);
    unsupported!(remove_directory_all(root:&File,path:&Path,label:&Path)->());
    unsupported!(ensure_owner_private(root:&File,path:&Path,label:&Path)->());
    unsupported!(entries(root:&File,path:&Path,label:&Path)->Vec<StoreEntry>);
    unsupported!(entries_optional(root:&File,path:&Path,label:&Path)->Option<Vec<StoreEntry>>);
    unsupported!(sync_directory(root:&File,label:&Path)->());
    unsupported!(restrict_new_private_directory(root:&File)->());
}
