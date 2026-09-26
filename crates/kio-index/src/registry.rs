//! Device-local scope registry (`~/.local/share/kio/scope-registry.sqlite`).
//!
//! The registry is a search cache, never truth (03-data-model.md §4): it lists
//! candidate scopes for multi-scope search (05-runtime.md §1.8) and resolves
//! `scope_id -> kio_path` for Evidence Pointers (08-evidence-pointer-spec.md
//! §3.1). Losing it must be recoverable by re-running `kio init` / `kio index`
//! in each scope.

use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

use cap_primitives::fs as cap_fs;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryEntry {
    pub scope_id: String,
    /// Absolute path of the `.kio` directory (truth root).
    pub kio_path: String,
    /// Absolute path of the folder that contains `.kio`.
    pub root_path: String,
    pub participates_in_global_search: bool,
    /// True once the scope has a search index (`kio index` completed).
    pub indexed: bool,
    pub last_seen_at: String,
}

/// One app-prevalidated move of a scope's device-local registration.
///
/// `old_kio_paths` is the exact path set observed for `scope_id` before the
/// app checked those former locations. It may include `new_registration`'s
/// path when a prior interrupted rebind already inserted the destination row.
/// This cache helper does not inspect the filesystem or claim that a supplied
/// old path is absent; that authority remains with the app's preflight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeRegistrationRebind {
    pub scope_id: String,
    pub old_kio_paths: Vec<String>,
    pub new_registration: RegistryEntry,
}

pub struct RegistryDb {
    conn: Connection,
    // An inherited `/dev/fd/N` registry is opened through this duplicated
    // parent descriptor's procfs path. Keep it alive until SQLite closes so a
    // caller retargeting the original descriptor cannot redirect the database.
    _inherited_parent: Option<kio_core::store_dir::StoreDirectory>,
    // Declared after `conn`, so the database closes before its private snapshot
    // directory is removed.
    _snapshot: Option<RegistrySnapshot>,
}

/// The size limits are deliberately registry-specific.  Snapshot reads and
/// copies are streaming, so these bounds limit I/O rather than allocating a
/// whole SQLite leaf in memory.
const MAX_REGISTRY_LEAF_BYTES: u64 = 64 * 1024 * 1024;
const MAX_REGISTRY_TOTAL_BYTES: u64 = 128 * 1024 * 1024;
const REGISTRY_SNAPSHOT_RETRY_WINDOW: Duration = Duration::from_secs(5);
const MAX_REGISTRY_SNAPSHOT_ATTEMPTS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrySnapshotError {
    /// The registry main leaf and both sidecars are absent: a normal cache miss.
    Missing,
    /// An unsafe file shape, link, replacement, or integrity failure was found.
    UnsafeIntegrity(String),
    /// A concurrent change, busy source, or transient read prevented a stable
    /// owned snapshot before the bounded retry deadline.
    UnstableBusy(String),
}

impl std::fmt::Display for RegistrySnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str("scope registry is absent"),
            Self::UnsafeIntegrity(message) | Self::UnstableBusy(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for RegistrySnapshotError {}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LeafObservation {
    Absent,
    Present(FileObservation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileObservation {
    identity: FileIdentity,
    bytes: u64,
    sha256: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RegistryManifest {
    main: LeafObservation,
    wal: LeafObservation,
    shm: LeafObservation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PrivateSnapshotManifest {
    main: FileObservation,
    wal: Option<FileObservation>,
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
}

#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity {
    volume_serial_number: u32,
    file_index: u64,
}

#[cfg(not(any(unix, windows)))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity;

const PRIVATE_REGISTRY_MAIN: &str = "snapshot.sqlite";

enum PrivateSnapshotStorage {
    #[cfg(unix)]
    Temp(tempfile::TempDir),
    #[cfg(windows)]
    Windows(crate::registry_windows_security::RegistrySnapshotPrivateDir),
}

impl PrivateSnapshotStorage {
    fn create() -> std::result::Result<Self, SnapshotAttemptError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut builder = tempfile::Builder::new();
            builder.prefix("kio-registry-snapshot-");
            builder.permissions(fs::Permissions::from_mode(0o700));
            builder.tempdir().map(Self::Temp).map_err(|error| {
                SnapshotAttemptError::Unstable(format!(
                    "create registry snapshot directory: {error}"
                ))
            })
        }
        #[cfg(windows)]
        {
            crate::registry_windows_security::RegistrySnapshotPrivateDir::create()
                .map(Self::Windows)
                .map_err(|error| {
                    SnapshotAttemptError::Unstable(format!(
                        "create owner-private registry snapshot directory: {error}"
                    ))
                })
        }
        #[cfg(not(any(unix, windows)))]
        compile_error!("RegistrySnapshot requires a platform-private directory implementation");
    }

    fn path(&self) -> &Path {
        match self {
            #[cfg(unix)]
            Self::Temp(temp) => temp.path(),
            #[cfg(windows)]
            Self::Windows(temp) => temp.path(),
        }
    }

    fn create_file(&self, basename: &str) -> std::result::Result<fs::File, SnapshotAttemptError> {
        match self {
            #[cfg(unix)]
            Self::Temp(temp) => {
                use std::os::unix::fs::OpenOptionsExt;
                let mut options = fs::OpenOptions::new();
                options.write(true).create_new(true).mode(0o600);
                options.open(temp.path().join(basename)).map_err(|error| {
                    SnapshotAttemptError::Unstable(format!(
                        "create registry snapshot {}: {error}",
                        temp.path().join(basename).display()
                    ))
                })
            }
            #[cfg(windows)]
            Self::Windows(temp) => temp.create_file(basename).map_err(|error| {
                SnapshotAttemptError::Unstable(format!(
                    "create owner-private registry snapshot {basename}: {error}"
                ))
            }),
        }
    }

    #[cfg(windows)]
    fn as_windows(
        &self,
    ) -> std::result::Result<
        &crate::registry_windows_security::RegistrySnapshotPrivateDir,
        SnapshotAttemptError,
    > {
        match self {
            Self::Windows(storage) => Ok(storage),
        }
    }
}

struct RegistrySnapshot {
    // This is the descriptor-relative capability used to validate the copied
    // leaves immediately before SQLite is allowed to open the private path.
    // Keep it alive for the lifetime of the connection/snapshot.
    _private_dir: fs::File,
    _storage: PrivateSnapshotStorage,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotTestPhase {
    AfterInitialManifest,
    BeforeSnapshotSqliteOpen,
    AfterSnapshotQueryBeforeRecheck,
    AfterFinalManifestBeforeParentRecheck,
}

#[cfg(test)]
type SnapshotTestHook =
    std::sync::Arc<dyn Fn(SnapshotTestPhase, usize, Option<&Path>) + Send + Sync>;

/// `$XDG_DATA_HOME/kio/scope-registry.sqlite`, falling back to
/// `$HOME/.local/share/kio/scope-registry.sqlite` (03-data-model.md §4).
pub fn default_registry_path() -> Result<PathBuf> {
    // R12-6 / R13-6: honor the XDG validity rules AND require an absolute `HOME`
    // for the fallback (empty/relative treated as unset), so neither a bad
    // `XDG_DATA_HOME` nor a bad `HOME` lands the registry in a CWD-relative `kio/`.
    let data_home = kio_core::xdg::xdg_dir("XDG_DATA_HOME")
        .or_else(|| kio_core::xdg::home_dir().map(|home| home.join(".local/share")))
        .ok_or_else(|| {
            crate::IndexError::Schema(
                "cannot resolve an absolute user data directory; refusing a CWD-relative registry"
                    .to_owned(),
            )
        })?;
    Ok(data_home.join("kio/scope-registry.sqlite"))
}

fn index_error_from_snapshot(error: RegistrySnapshotError) -> crate::IndexError {
    match error {
        RegistrySnapshotError::Missing => crate::IndexError::RegistryMissing,
        RegistrySnapshotError::UnsafeIntegrity(message) => {
            crate::IndexError::RegistryUnsafeIntegrity(message)
        }
        RegistrySnapshotError::UnstableBusy(message) => {
            crate::IndexError::RegistryUnstableBusy(message)
        }
    }
}

#[cfg(target_os = "linux")]
fn inherited_registry_parent(
    path: &Path,
) -> Result<Option<(PathBuf, kio_core::store_dir::StoreDirectory)>> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    let Some((mut parent, suffix)) = kio_core::private_fs::resolve_inherited_private_root(path)
        .map_err(|error| {
            crate::IndexError::Schema(format!(
                "resolve inherited registry directory capability: {error}"
            ))
        })?
    else {
        return Ok(None);
    };
    let (leaf, parent_suffix) = suffix.split_last().ok_or_else(|| {
        crate::IndexError::Schema("inherited scope registry path has no file name".to_owned())
    })?;
    let verify_owner_private_directory =
        |directory: &kio_core::store_dir::StoreDirectory| -> Result<()> {
            let metadata = directory.root_handle().metadata().map_err(|error| {
                crate::IndexError::Schema(format!("inspect inherited registry parent: {error}"))
            })?;
            // SAFETY: geteuid has no arguments and no memory-safety preconditions.
            let uid = unsafe { libc::geteuid() };
            if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
                return Err(crate::IndexError::Schema(format!(
                    "inherited registry parent is not owner-private: {}",
                    directory.path().display()
                )));
            }
            Ok(())
        };

    verify_owner_private_directory(&parent)?;
    let mut logical = parent.path().to_path_buf();
    for name in parent_suffix {
        let component = Path::new(name);
        logical.push(name);
        let next = if parent
            .contains_entry(component)
            .map_err(|error| crate::IndexError::Schema(error.to_string()))?
        {
            parent
                .open_directory(component)
                .map_err(|error| crate::IndexError::Schema(error.to_string()))?
        } else {
            parent
                .create_directory(component)
                .map_err(|error| crate::IndexError::Schema(error.to_string()))?
        };
        parent = kio_core::store_dir::StoreDirectory::from_retained(next, logical.clone())
            .map_err(|error| crate::IndexError::Schema(error.to_string()))?;
        // Existing directories are only verified, never chmod-repaired. New
        // directories were restricted by StoreDirectory before this adoption.
        verify_owner_private_directory(&parent)?;
    }

    let leaf = Path::new(leaf);
    let name = CString::new(leaf.as_os_str().as_bytes()).map_err(|_| {
        crate::IndexError::Schema("inherited scope registry leaf contains NUL".to_owned())
    })?;
    // Create a missing SQLite main leaf descriptor-relatively and owner-only
    // before SQLite sees it. Existing state is opened with O_NOFOLLOW and
    // validated, never chmod-repaired.
    if !parent
        .contains_entry(leaf)
        .map_err(|error| crate::IndexError::Schema(error.to_string()))?
    {
        // SAFETY: `parent` is a retained directory capability and `name` is a
        // single NUL-free component supplied by core's inherited-root parser.
        let fd = unsafe {
            libc::openat(
                parent.root_handle().as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd >= 0 {
            // SAFETY: openat returned an owned descriptor above.
            let created = unsafe { fs::File::from_raw_fd(fd) };
            let metadata = created.metadata().map_err(|error| {
                crate::IndexError::Schema(format!(
                    "inspect created inherited registry leaf: {error}"
                ))
            })?;
            // SAFETY: geteuid has no arguments and no memory-safety preconditions.
            let uid = unsafe { libc::geteuid() };
            if !metadata.is_file()
                || metadata.uid() != uid
                || metadata.mode() & 0o077 != 0
                || metadata.nlink() != 1
            {
                return Err(crate::IndexError::Schema(
                    "created inherited registry leaf is not owner-private".to_owned(),
                ));
            }
        } else {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EEXIST) {
                parent
                    .ensure_owner_private(leaf)
                    .map_err(|error| crate::IndexError::Schema(error.to_string()))?;
            } else {
                return Err(crate::IndexError::Schema(format!(
                    "create inherited scope registry leaf: {error}"
                )));
            }
        }
    } else {
        parent
            .ensure_owner_private(leaf)
            .map_err(|error| crate::IndexError::Schema(error.to_string()))?;
    }
    let sqlite_path = PathBuf::from(format!(
        "/proc/self/fd/{}",
        parent.root_handle().as_raw_fd()
    ))
    .join(leaf);
    Ok(Some((sqlite_path, parent)))
}

fn snapshot_query_error(error: crate::IndexError) -> RegistrySnapshotError {
    match error {
        crate::IndexError::Sqlite(rusqlite::Error::SqliteFailure(code, _))
            if matches!(
                code.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            RegistrySnapshotError::UnstableBusy(format!("query registry snapshot: {code}"))
        }
        crate::IndexError::Sqlite(rusqlite::Error::SqliteFailure(code, _))
            if matches!(
                code.code,
                rusqlite::ErrorCode::DatabaseCorrupt
                    | rusqlite::ErrorCode::NotADatabase
                    | rusqlite::ErrorCode::SchemaChanged
                    // The fixed lookup SQL is product-owned; SQLITE_ERROR here
                    // (for example a missing required scopes column) proves a
                    // malformed stable snapshot schema rather than transient I/O.
                    | rusqlite::ErrorCode::Unknown
            ) =>
        {
            RegistrySnapshotError::UnsafeIntegrity(format!(
                "stable registry snapshot query failed: {code}"
            ))
        }
        error @ crate::IndexError::Sqlite(
            rusqlite::Error::InvalidColumnName(_)
            | rusqlite::Error::InvalidColumnType(_, _, _)
            | rusqlite::Error::FromSqlConversionFailure(_, _, _)
            | rusqlite::Error::SqlInputError { .. },
        ) => RegistrySnapshotError::UnsafeIntegrity(format!(
            "stable registry snapshot returned malformed row/schema: {error}"
        )),
        error => RegistrySnapshotError::UnstableBusy(format!(
            "stable registry snapshot query failed: {error}"
        )),
    }
}

#[derive(Debug)]
enum SnapshotAttemptError {
    Missing,
    Unsafe(String),
    Unstable(String),
}

struct RegistryParentBinding {
    parent: std::sync::Arc<fs::File>,
    leaf: String,
    parent_path: PathBuf,
    // Keep the core-duplicated inherited root for the complete snapshot
    // attempt. A later `/dev/fd/N` re-resolution is compared to this retained
    // capability before a stable read or missing result is accepted.
    #[cfg(target_os = "linux")]
    inherited_root: Option<kio_core::store_dir::StoreDirectory>,
}

fn registry_parent_and_leaf(
    path: &Path,
) -> std::result::Result<RegistryParentBinding, RegistrySnapshotError> {
    #[cfg(target_os = "linux")]
    if let Some((root, suffix)) = kio_core::private_fs::resolve_inherited_private_root(path)
        .map_err(|error| {
            RegistrySnapshotError::UnsafeIntegrity(format!(
                "resolve inherited registry directory capability: {error}"
            ))
        })?
    {
        return inherited_registry_parent_and_leaf(path, root, &suffix);
    }
    let path = normalized_absolute_registry_path(path)?;
    let filesystem_root = registry_filesystem_root(&path)?;
    let leaf = path
        .file_name()
        .and_then(|leaf| leaf.to_str())
        .ok_or_else(|| {
            RegistrySnapshotError::UnsafeIntegrity(format!(
                "scope registry path has no valid UTF-8 file name: {}",
                path.display()
            ))
        })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("/"));
    let mut handle =
        cap_fs::open_ambient_dir(&filesystem_root, cap_primitives::ambient_authority()).map_err(
            |error| RegistrySnapshotError::UnstableBusy(format!("open filesystem root: {error}")),
        )?;
    let mut bound_component_path = filesystem_root.clone();
    #[cfg(test)]
    let mut index = 0usize;
    for component in parent.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let before = match cap_fs::stat(&handle, Path::new(name), cap_fs::FollowSymlinks::No) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(RegistrySnapshotError::Missing);
            }
            Err(error) => {
                return Err(RegistrySnapshotError::UnstableBusy(format!(
                    "inspect registry parent component {}: {error}",
                    name.to_string_lossy()
                )));
            }
        };
        validate_directory(&before, &name.to_string_lossy())?;
        #[cfg(test)]
        run_parent_component_hook(index);
        let child = cap_fs::open_dir_nofollow(&handle, Path::new(name)).map_err(|error| {
            RegistrySnapshotError::UnsafeIntegrity(format!(
                "open registry parent component {}: {error}",
                name.to_string_lossy()
            ))
        })?;
        bound_component_path.push(name);
        let opened = cap_fs::Metadata::from_file(&child).map_err(|error| {
            RegistrySnapshotError::UnstableBusy(format!(
                "inspect opened registry parent component {}: {error}",
                name.to_string_lossy()
            ))
        })?;
        validate_directory(&opened, &name.to_string_lossy())?;
        let current = cap_fs::stat(&handle, Path::new(name), cap_fs::FollowSymlinks::No).map_err(
            |error| {
                RegistrySnapshotError::UnsafeIntegrity(format!(
                    "reinspect registry parent component {} after opening: {error}",
                    name.to_string_lossy()
                ))
            },
        )?;
        validate_directory(&current, &name.to_string_lossy())?;
        let before_identity = leaf_identity(&before).ok_or_else(|| {
            RegistrySnapshotError::UnsafeIntegrity(format!(
                "registry parent component has no usable identity: {}",
                name.to_string_lossy()
            ))
        })?;
        let opened_identity = leaf_identity(&opened).ok_or_else(|| {
            RegistrySnapshotError::UnsafeIntegrity(format!(
                "opened registry parent component has no usable identity: {}",
                name.to_string_lossy()
            ))
        })?;
        let current_identity = leaf_identity(&current).ok_or_else(|| {
            RegistrySnapshotError::UnsafeIntegrity(format!(
                "reinspected registry parent component has no usable identity: {}",
                name.to_string_lossy()
            ))
        })?;
        if before_identity != opened_identity
            || before_identity != current_identity
            || opened_identity != current_identity
        {
            return Err(RegistrySnapshotError::UnsafeIntegrity(format!(
                "registry parent component changed while opening: {}",
                name.to_string_lossy()
            )));
        }
        #[cfg(windows)]
        verify_windows_directory_binding(&bound_component_path, &child, &name.to_string_lossy())?;
        handle = child;
        #[cfg(test)]
        {
            index += 1;
        }
    }
    Ok(RegistryParentBinding {
        parent: std::sync::Arc::new(handle),
        leaf: leaf.to_owned(),
        parent_path: parent.to_owned(),
        #[cfg(target_os = "linux")]
        inherited_root: None,
    })
}

#[cfg(target_os = "linux")]
fn inherited_registry_parent_and_leaf(
    path: &Path,
    root: kio_core::store_dir::StoreDirectory,
    suffix: &[std::ffi::OsString],
) -> std::result::Result<RegistryParentBinding, RegistrySnapshotError> {
    use std::os::unix::fs::MetadataExt;

    let (leaf, parent_suffix) = suffix.split_last().ok_or_else(|| {
        RegistrySnapshotError::UnsafeIntegrity(
            "inherited scope registry path has no file name".to_owned(),
        )
    })?;
    let leaf = leaf.to_str().ok_or_else(|| {
        RegistrySnapshotError::UnsafeIntegrity(
            "inherited scope registry path has no valid UTF-8 file name".to_owned(),
        )
    })?;
    let parent_path = path.parent().ok_or_else(|| {
        RegistrySnapshotError::UnsafeIntegrity(
            "inherited scope registry path has no parent".to_owned(),
        )
    })?;
    let mut parent = root.root_handle();
    // SAFETY: geteuid has no arguments and no memory-safety preconditions.
    let uid = unsafe { libc::geteuid() };
    for name in parent_suffix {
        let label = name.to_string_lossy();
        let before =
            match cap_fs::stat(parent.as_ref(), Path::new(name), cap_fs::FollowSymlinks::No) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(RegistrySnapshotError::Missing);
                }
                Err(error) => {
                    return Err(RegistrySnapshotError::UnstableBusy(format!(
                        "inspect inherited registry parent component {label}: {error}"
                    )));
                }
            };
        validate_directory(&before, &label)?;
        let child =
            cap_fs::open_dir_nofollow(parent.as_ref(), Path::new(name)).map_err(|error| {
                RegistrySnapshotError::UnsafeIntegrity(format!(
                    "open inherited registry parent component {label}: {error}"
                ))
            })?;
        let opened = cap_fs::Metadata::from_file(&child).map_err(|error| {
            RegistrySnapshotError::UnstableBusy(format!(
                "inspect inherited registry parent component {label}: {error}"
            ))
        })?;
        let after = cap_fs::stat(parent.as_ref(), Path::new(name), cap_fs::FollowSymlinks::No)
            .map_err(|error| {
                RegistrySnapshotError::UnsafeIntegrity(format!(
                    "reinspect inherited registry parent component {label}: {error}"
                ))
            })?;
        validate_directory(&opened, &label)?;
        validate_directory(&after, &label)?;
        let before_identity = leaf_identity(&before).ok_or_else(|| {
            RegistrySnapshotError::UnsafeIntegrity(format!(
                "inherited registry parent component has no usable identity: {label}"
            ))
        })?;
        if leaf_identity(&opened).as_ref() != Some(&before_identity)
            || leaf_identity(&after).as_ref() != Some(&before_identity)
        {
            return Err(RegistrySnapshotError::UnsafeIntegrity(format!(
                "inherited registry parent component changed while opening: {label}"
            )));
        }
        let metadata = child.metadata().map_err(|error| {
            RegistrySnapshotError::UnstableBusy(format!(
                "inspect inherited registry parent permissions {label}: {error}"
            ))
        })?;
        if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            return Err(RegistrySnapshotError::UnsafeIntegrity(format!(
                "inherited registry parent is not owner-private: {label}"
            )));
        }
        parent = std::sync::Arc::new(child);
    }
    Ok(RegistryParentBinding {
        parent,
        leaf: leaf.to_owned(),
        parent_path: parent_path.to_owned(),
        inherited_root: Some(root),
    })
}

fn registry_parent_binding_error(error: RegistrySnapshotError) -> SnapshotAttemptError {
    match error {
        RegistrySnapshotError::Missing => SnapshotAttemptError::Missing,
        RegistrySnapshotError::UnsafeIntegrity(message) => SnapshotAttemptError::Unsafe(message),
        RegistrySnapshotError::UnstableBusy(message) => SnapshotAttemptError::Unstable(message),
    }
}

/// A missing canonical parent is a cache miss only after two complete nofollow
/// walks agree. Each snapshot retry obtains a new binding rather than retaining
/// a directory that may have been detached from the canonical path.
fn stable_registry_parent_binding(
    path: &Path,
) -> std::result::Result<Option<RegistryParentBinding>, SnapshotAttemptError> {
    #[cfg(target_os = "linux")]
    if let Some((root, suffix)) = kio_core::private_fs::resolve_inherited_private_root(path)
        .map_err(|error| {
            SnapshotAttemptError::Unsafe(format!(
                "resolve inherited registry directory capability: {error}"
            ))
        })?
    {
        match inherited_registry_parent_and_leaf(path, root.clone(), &suffix) {
            Ok(binding) => return Ok(Some(binding)),
            Err(RegistrySnapshotError::Missing) => {
                match inherited_registry_parent_and_leaf(path, root.clone(), &suffix) {
                    Ok(_) => {
                        return Err(SnapshotAttemptError::Unstable(
                            "inherited registry parent appeared while confirming its absence"
                                .to_owned(),
                        ));
                    }
                    Err(RegistrySnapshotError::Missing) => {
                        let (fresh_root, fresh_suffix) =
                            kio_core::private_fs::resolve_inherited_private_root(path)
                                .map_err(|error| {
                                    SnapshotAttemptError::Unsafe(format!(
                                        "re-resolve inherited registry directory capability: {error}"
                                    ))
                                })?
                                .ok_or_else(|| {
                                    SnapshotAttemptError::Unstable(
                                        "inherited registry root disappeared while confirming absence"
                                            .to_owned(),
                                    )
                                })?;
                        if fresh_suffix != suffix || !same_inherited_root(&root, &fresh_root)? {
                            return Err(SnapshotAttemptError::Unsafe(
                                "inherited registry root changed while confirming absence"
                                    .to_owned(),
                            ));
                        }
                        return Ok(None);
                    }
                    Err(error) => return Err(registry_parent_binding_error(error)),
                }
            }
            Err(error) => return Err(registry_parent_binding_error(error)),
        }
    }
    match registry_parent_and_leaf(path) {
        Ok(binding) => Ok(Some(binding)),
        Err(RegistrySnapshotError::Missing) => match registry_parent_and_leaf(path) {
            Err(RegistrySnapshotError::Missing) => Ok(None),
            Ok(_) => Err(SnapshotAttemptError::Unstable(
                "scope registry parent appeared while confirming its absence".to_owned(),
            )),
            Err(error) => Err(registry_parent_binding_error(error)),
        },
        Err(error) => Err(registry_parent_binding_error(error)),
    }
}

/// Freshly bind the canonical parent at the acceptance boundary and require it
/// to be the same directory as the descriptor used to capture the snapshot.
fn fresh_registry_parent_binding(
    held: &RegistryParentBinding,
) -> std::result::Result<std::sync::Arc<fs::File>, SnapshotAttemptError> {
    let canonical_path = held.parent_path.join(&held.leaf);
    let fresh = match registry_parent_and_leaf(&canonical_path) {
        Ok(binding) => binding,
        Err(RegistrySnapshotError::Missing) => {
            return Err(SnapshotAttemptError::Unstable(
                "scope registry canonical parent disappeared while snapshotting".to_owned(),
            ));
        }
        Err(error) => return Err(registry_parent_binding_error(error)),
    };
    if fresh.leaf != held.leaf || fresh.parent_path != held.parent_path {
        return Err(SnapshotAttemptError::Unsafe(
            "scope registry canonical parent rebound to an unexpected path".to_owned(),
        ));
    }
    #[cfg(target_os = "linux")]
    match (&held.inherited_root, &fresh.inherited_root) {
        (Some(held_root), Some(fresh_root)) => {
            if !same_inherited_root(held_root, fresh_root)? {
                return Err(SnapshotAttemptError::Unsafe(
                    "inherited registry root changed while snapshotting".to_owned(),
                ));
            }
        }
        (None, None) => {}
        _ => {
            return Err(SnapshotAttemptError::Unsafe(
                "registry binding changed inherited-root mode while snapshotting".to_owned(),
            ));
        }
    }
    let held_metadata = cap_fs::Metadata::from_file(held.parent.as_ref()).map_err(|error| {
        SnapshotAttemptError::Unstable(format!("inspect retained registry parent: {error}"))
    })?;
    let fresh_metadata = cap_fs::Metadata::from_file(fresh.parent.as_ref()).map_err(|error| {
        SnapshotAttemptError::Unstable(format!("inspect freshly bound registry parent: {error}"))
    })?;
    let held_identity = leaf_identity(&held_metadata).ok_or_else(|| {
        SnapshotAttemptError::Unsafe(
            "retained registry parent has no usable identity while snapshotting".to_owned(),
        )
    })?;
    let fresh_identity = leaf_identity(&fresh_metadata).ok_or_else(|| {
        SnapshotAttemptError::Unsafe(
            "freshly bound registry parent has no usable identity while snapshotting".to_owned(),
        )
    })?;
    if held_identity != fresh_identity {
        return Err(SnapshotAttemptError::Unsafe(
            "scope registry canonical parent identity changed while snapshotting".to_owned(),
        ));
    }
    Ok(fresh.parent)
}

#[cfg(target_os = "linux")]
fn same_inherited_root(
    held: &kio_core::store_dir::StoreDirectory,
    fresh: &kio_core::store_dir::StoreDirectory,
) -> std::result::Result<bool, SnapshotAttemptError> {
    let held_metadata =
        cap_fs::Metadata::from_file(held.root_handle().as_ref()).map_err(|error| {
            SnapshotAttemptError::Unstable(format!(
                "inspect retained inherited registry root: {error}"
            ))
        })?;
    let fresh_metadata =
        cap_fs::Metadata::from_file(fresh.root_handle().as_ref()).map_err(|error| {
            SnapshotAttemptError::Unstable(format!(
                "inspect fresh inherited registry root: {error}"
            ))
        })?;
    Ok(leaf_identity(&held_metadata) == leaf_identity(&fresh_metadata))
}

#[cfg(windows)]
fn verify_windows_directory_binding(
    path: &Path,
    opened: &fs::File,
    label: &str,
) -> std::result::Result<(), RegistrySnapshotError> {
    let path_identity = kio_core::cas::windows_real_directory_identity(path).map_err(|error| {
        RegistrySnapshotError::UnstableBusy(format!(
            "inspect Windows registry parent component {label}: {error}"
        ))
    })?;
    let handle_identity = kio_core::cas::windows_directory_handle_identity(opened);
    if path_identity.is_none() || path_identity != handle_identity {
        return Err(RegistrySnapshotError::UnsafeIntegrity(format!(
            "Windows registry parent component is a reparse point or changed while opening: {label}"
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn verify_windows_regular_binding(
    path: &Path,
    opened: &fs::File,
    label: &str,
) -> std::result::Result<(), SnapshotAttemptError> {
    let path_identity =
        kio_core::cas::windows_real_regular_file_identity(path).map_err(|error| {
            SnapshotAttemptError::Unstable(format!("inspect Windows registry {label}: {error}"))
        })?;
    let handle_identity = kio_core::cas::windows_regular_file_handle_identity(opened);
    if path_identity.is_none() || path_identity != handle_identity {
        return Err(SnapshotAttemptError::Unsafe(format!(
            "Windows registry {label} is a reparse point or changed while opening"
        )));
    }
    Ok(())
}

#[cfg(not(windows))]
fn normalized_absolute_registry_path(
    path: &Path,
) -> std::result::Result<PathBuf, RegistrySnapshotError> {
    if !path.is_absolute() {
        return Err(RegistrySnapshotError::UnsafeIntegrity(format!(
            "scope registry path must be absolute: {}",
            path.display()
        )));
    }
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                #[cfg(target_os = "macos")]
                if normalized == Path::new("/") && name == "var" {
                    normalized.push("private");
                    normalized.push("var");
                } else {
                    normalized.push(name);
                }
                #[cfg(not(target_os = "macos"))]
                normalized.push(name);
            }
            Component::CurDir | Component::ParentDir => {
                return Err(RegistrySnapshotError::UnsafeIntegrity(format!(
                    "scope registry path is not normalized: {}",
                    path.display()
                )));
            }
            Component::Prefix(_) => {
                return Err(RegistrySnapshotError::UnsafeIntegrity(format!(
                    "scope registry path prefix is unsupported: {}",
                    path.display()
                )));
            }
        }
    }
    Ok(normalized)
}

#[cfg(windows)]
fn normalized_absolute_registry_path(
    path: &Path,
) -> std::result::Result<PathBuf, RegistrySnapshotError> {
    use std::path::Prefix;

    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return Err(RegistrySnapshotError::UnsafeIntegrity(format!(
            "scope registry path must use an absolute drive or UNC path: {}",
            path.display()
        )));
    };
    if !matches!(prefix.kind(), Prefix::Disk(_) | Prefix::UNC(_, _))
        || !matches!(components.next(), Some(Component::RootDir))
    {
        return Err(RegistrySnapshotError::UnsafeIntegrity(format!(
            "scope registry path must use a normalized absolute drive or UNC path: {}",
            path.display()
        )));
    }
    let mut normalized = PathBuf::from(prefix.as_os_str());
    normalized.push("\\");
    for component in components {
        match component {
            Component::Normal(name) => normalized.push(name),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(RegistrySnapshotError::UnsafeIntegrity(format!(
                    "scope registry path is not normalized: {}",
                    path.display()
                )));
            }
        }
    }
    Ok(normalized)
}

fn registry_filesystem_root(path: &Path) -> std::result::Result<PathBuf, RegistrySnapshotError> {
    #[cfg(windows)]
    {
        let mut components = path.components();
        use std::path::Prefix;
        let Some(Component::Prefix(prefix)) = components.next() else {
            return Err(RegistrySnapshotError::UnsafeIntegrity(format!(
                "scope registry path must have an absolute Windows volume/share root: {}",
                path.display()
            )));
        };
        if !matches!(prefix.kind(), Prefix::Disk(_) | Prefix::UNC(_, _))
            || !matches!(components.next(), Some(Component::RootDir))
        {
            return Err(RegistrySnapshotError::UnsafeIntegrity(format!(
                "scope registry path must be absolute on Windows: {}",
                path.display()
            )));
        }
        let mut root = PathBuf::from(prefix.as_os_str());
        root.push("\\");
        Ok(root)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Ok(PathBuf::from("/"))
    }
}

fn validate_directory(
    metadata: &cap_fs::Metadata,
    label: &str,
) -> std::result::Result<(), RegistrySnapshotError> {
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(RegistrySnapshotError::UnsafeIntegrity(format!(
            "registry parent component must be a real directory: {label}"
        )));
    }
    Ok(())
}

#[cfg(test)]
type ParentComponentHook = std::sync::Arc<dyn Fn(usize)>;

#[cfg(test)]
thread_local! {
    static PARENT_COMPONENT_HOOK: std::cell::RefCell<Option<ParentComponentHook>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn run_parent_component_hook(index: usize) {
    PARENT_COMPONENT_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow().as_ref() {
            hook(index);
        }
    });
}

fn leaf_identity(metadata: &cap_fs::Metadata) -> Option<FileIdentity> {
    #[cfg(unix)]
    {
        use cap_fs::MetadataExt;
        Some(FileIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }
    #[cfg(windows)]
    {
        use cap_fs::_WindowsByHandle;
        Some(FileIdentity {
            volume_serial_number: metadata.volume_serial_number()?,
            file_index: metadata.file_index()?,
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        Some(FileIdentity)
    }
}

fn validate_regular(
    metadata: &cap_fs::Metadata,
    label: &str,
) -> std::result::Result<(), SnapshotAttemptError> {
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(SnapshotAttemptError::Unsafe(format!(
            "scope registry {label} must be a regular non-symlink file"
        )));
    }
    if metadata.len() > MAX_REGISTRY_LEAF_BYTES {
        return Err(SnapshotAttemptError::Unsafe(format!(
            "scope registry {label} exceeds {} byte limit",
            MAX_REGISTRY_LEAF_BYTES
        )));
    }
    #[cfg(unix)]
    {
        use cap_fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(SnapshotAttemptError::Unsafe(format!(
                "scope registry {label} must have exactly one hard link"
            )));
        }
    }
    #[cfg(windows)]
    {
        use cap_fs::_WindowsByHandle;
        if metadata.number_of_links() != Some(1) {
            return Err(SnapshotAttemptError::Unsafe(format!(
                "scope registry {label} must have exactly one hard link"
            )));
        }
    }
    Ok(())
}

fn observe_leaf(
    parent: &fs::File,
    parent_path: &Path,
    name: &str,
    copy_to: Option<(&PrivateSnapshotStorage, &str)>,
) -> std::result::Result<LeafObservation, SnapshotAttemptError> {
    #[cfg(not(windows))]
    let _ = parent_path;
    let before = match cap_fs::stat(parent, Path::new(name), cap_fs::FollowSymlinks::No) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LeafObservation::Absent);
        }
        Err(error) => {
            return Err(SnapshotAttemptError::Unstable(format!(
                "stat scope registry {name}: {error}"
            )));
        }
    };
    validate_regular(&before, name)?;
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut input = cap_fs::open(parent, Path::new(name), &options).map_err(|error| {
        SnapshotAttemptError::Unstable(format!("open scope registry {name}: {error}"))
    })?;
    let opened = cap_fs::Metadata::from_file(&input).map_err(|error| {
        SnapshotAttemptError::Unstable(format!("inspect opened registry {name}: {error}"))
    })?;
    validate_regular(&opened, name)?;
    #[cfg(windows)]
    verify_windows_regular_binding(&parent_path.join(name), &input, name)?;
    let before_identity = leaf_identity(&before).ok_or_else(|| {
        SnapshotAttemptError::Unsafe(format!("scope registry {name} has no usable identity"))
    })?;
    let opened_identity = leaf_identity(&opened).ok_or_else(|| {
        SnapshotAttemptError::Unsafe(format!(
            "opened scope registry {name} has no usable identity"
        ))
    })?;
    if before_identity != opened_identity || before.len() != opened.len() {
        return Err(SnapshotAttemptError::Unsafe(format!(
            "scope registry {name} changed while opening"
        )));
    }

    let mut output = match copy_to {
        Some((storage, basename)) => Some(storage.create_file(basename)?),
        None => None,
    };
    let mut hash = Sha256::new();
    let mut total = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = input.read(&mut buf).map_err(|error| {
            SnapshotAttemptError::Unstable(format!("read scope registry {name}: {error}"))
        })?;
        if read == 0 {
            break;
        }
        total = total.checked_add(read as u64).ok_or_else(|| {
            SnapshotAttemptError::Unsafe(format!("scope registry {name} size overflow"))
        })?;
        if total > MAX_REGISTRY_LEAF_BYTES {
            return Err(SnapshotAttemptError::Unsafe(format!(
                "scope registry {name} exceeded byte limit while reading"
            )));
        }
        hash.update(&buf[..read]);
        if let Some(output) = output.as_mut() {
            output.write_all(&buf[..read]).map_err(|error| {
                SnapshotAttemptError::Unstable(format!("write registry snapshot {name}: {error}"))
            })?;
        }
    }
    if let Some(output) = output.as_mut() {
        output.flush().map_err(|error| {
            SnapshotAttemptError::Unstable(format!("flush registry snapshot {name}: {error}"))
        })?;
    }
    let after =
        cap_fs::stat(parent, Path::new(name), cap_fs::FollowSymlinks::No).map_err(|error| {
            SnapshotAttemptError::Unstable(format!("restat scope registry {name}: {error}"))
        })?;
    validate_regular(&after, name)?;
    #[cfg(windows)]
    verify_windows_regular_binding(&parent_path.join(name), &input, name)?;
    let after_identity = leaf_identity(&after).ok_or_else(|| {
        SnapshotAttemptError::Unsafe(format!(
            "reinspected scope registry {name} has no usable identity"
        ))
    })?;
    if opened_identity != after_identity || opened.len() != after.len() || total != opened.len() {
        return Err(SnapshotAttemptError::Unsafe(format!(
            "scope registry {name} changed while reading"
        )));
    }
    Ok(LeafObservation::Present(FileObservation {
        identity: opened_identity,
        bytes: total,
        sha256: hash.finalize().into(),
    }))
}

fn observe_manifest(
    parent: &fs::File,
    parent_path: &Path,
    main: &str,
    storage: Option<&PrivateSnapshotStorage>,
    missing_is_cache_miss: bool,
) -> std::result::Result<RegistryManifest, SnapshotAttemptError> {
    let wal = format!("{main}-wal");
    let shm = format!("{main}-shm");
    let main_observation = observe_leaf(
        parent,
        parent_path,
        main,
        storage.map(|storage| (storage, PRIVATE_REGISTRY_MAIN)),
    )?;
    let wal_observation = observe_leaf(
        parent,
        parent_path,
        &wal,
        storage.map(|storage| (storage, "snapshot.sqlite-wal")),
    )?;
    let shm_observation = observe_leaf(parent, parent_path, &shm, None)?;
    let manifest = RegistryManifest {
        main: main_observation,
        wal: wal_observation,
        shm: shm_observation,
    };
    let total = [&manifest.main, &manifest.wal, &manifest.shm]
        .iter()
        .filter_map(|leaf| match leaf {
            LeafObservation::Present(observation) => Some(observation.bytes),
            LeafObservation::Absent => None,
        })
        .try_fold(0u64, |total, bytes| total.checked_add(bytes))
        .ok_or_else(|| {
            SnapshotAttemptError::Unsafe("scope registry aggregate size overflow".to_owned())
        })?;
    if total > MAX_REGISTRY_TOTAL_BYTES {
        return Err(SnapshotAttemptError::Unsafe(format!(
            "scope registry exceeds {} byte aggregate limit",
            MAX_REGISTRY_TOTAL_BYTES
        )));
    }
    if missing_is_cache_miss {
        match (&manifest.main, &manifest.wal, &manifest.shm) {
            (LeafObservation::Absent, LeafObservation::Absent, LeafObservation::Absent) => {
                return Err(SnapshotAttemptError::Missing);
            }
            (LeafObservation::Absent, _, _) => {
                return Err(SnapshotAttemptError::Unsafe(
                    "scope registry sidecar exists without main database".to_owned(),
                ));
            }
            _ => {}
        }
    }
    // Final observation intentionally records every presence shape.  A source
    // main disappearing after a valid initial capture is concurrent drift
    // (retryable), not a cache miss or an integrity fallback.
    Ok(manifest)
}

fn sqlite_snapshot_error(
    error: rusqlite::Error,
    operation: &str,
    fixed_schema_probe: bool,
) -> SnapshotAttemptError {
    match error {
        rusqlite::Error::SqliteFailure(code, _)
            if matches!(
                code.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            SnapshotAttemptError::Unstable(format!("{operation}: {code}"))
        }
        rusqlite::Error::SqliteFailure(code, _)
            if matches!(
                code.code,
                rusqlite::ErrorCode::DatabaseCorrupt
                    | rusqlite::ErrorCode::NotADatabase
                    | rusqlite::ErrorCode::SchemaChanged
            ) || (fixed_schema_probe && code.code == rusqlite::ErrorCode::Unknown) =>
        {
            SnapshotAttemptError::Unsafe(format!("{operation}: {code}"))
        }
        // Private snapshot setup/open/query errors such as CANTOPEN and IOERR
        // do not prove source or owned-copy corruption.  Retry inside the
        // bounded snapshot window and fail as UnstableBusy if they persist.
        error => SnapshotAttemptError::Unstable(format!("{operation}: {error}")),
    }
}

fn classify_snapshot_sqlite_failure(
    parent: &fs::File,
    parent_path: &Path,
    main: &str,
    initial: &RegistryManifest,
    error: SnapshotAttemptError,
) -> SnapshotAttemptError {
    // A copied main/WAL pair can be temporarily incoherent while a writer is
    // rotating/checkpointing.  Re-observe all source leaves before calling a
    // stable copy malformed; source drift is retryable rather than corruption.
    match observe_manifest(parent, parent_path, main, None, false) {
        Ok(current) if &current != initial => SnapshotAttemptError::Unstable(
            "scope registry changed while preparing SQLite snapshot".to_owned(),
        ),
        Ok(_) => error,
        Err(SnapshotAttemptError::Unsafe(message)) => SnapshotAttemptError::Unsafe(message),
        Err(SnapshotAttemptError::Missing) | Err(SnapshotAttemptError::Unstable(_)) => {
            SnapshotAttemptError::Unstable(
                "scope registry could not be rechecked after snapshot SQLite failure".to_owned(),
            )
        }
    }
}

/// Verify the private copy through the already-held directory capability
/// before SQLite sees its path.  The source manifest is intentionally used
/// only for bytes/digest expectations: a copied file has a different identity
/// from its source.  This rejects pre-created sidecars, replacement leaves,
/// links, and every name SQLite could otherwise interpret before the first
/// private SQLite open.
fn verify_private_snapshot_leaf(
    private_dir: &fs::File,
    name: &str,
    expected: &FileObservation,
) -> std::result::Result<FileObservation, SnapshotAttemptError> {
    let before = cap_fs::stat(private_dir, Path::new(name), cap_fs::FollowSymlinks::No).map_err(
        |error| {
            SnapshotAttemptError::Unsafe(format!(
                "private registry snapshot {name} is missing or unsafe: {error}"
            ))
        },
    )?;
    validate_regular(&before, name)?;
    if before.len() != expected.bytes {
        return Err(SnapshotAttemptError::Unsafe(format!(
            "private registry snapshot {name} has unexpected size"
        )));
    }
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut file = cap_fs::open(private_dir, Path::new(name), &options).map_err(|error| {
        SnapshotAttemptError::Unsafe(format!("open private registry snapshot {name}: {error}"))
    })?;
    let opened = cap_fs::Metadata::from_file(&file).map_err(|error| {
        SnapshotAttemptError::Unstable(format!("inspect private registry snapshot {name}: {error}"))
    })?;
    validate_regular(&opened, name)?;
    let before_identity = leaf_identity(&before).ok_or_else(|| {
        SnapshotAttemptError::Unsafe(format!(
            "private registry snapshot {name} has no usable identity"
        ))
    })?;
    let opened_identity = leaf_identity(&opened).ok_or_else(|| {
        SnapshotAttemptError::Unsafe(format!(
            "opened private registry snapshot {name} has no usable identity"
        ))
    })?;
    if before_identity != opened_identity || before.len() != opened.len() {
        return Err(SnapshotAttemptError::Unsafe(format!(
            "private registry snapshot {name} changed while opening"
        )));
    }
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buf).map_err(|error| {
            SnapshotAttemptError::Unstable(format!(
                "read private registry snapshot {name}: {error}"
            ))
        })?;
        if read == 0 {
            break;
        }
        bytes = bytes.checked_add(read as u64).ok_or_else(|| {
            SnapshotAttemptError::Unsafe(format!("private registry snapshot {name} size overflow"))
        })?;
        if bytes > MAX_REGISTRY_LEAF_BYTES {
            return Err(SnapshotAttemptError::Unsafe(format!(
                "private registry snapshot {name} exceeds byte limit"
            )));
        }
        hash.update(&buf[..read]);
    }
    let after = cap_fs::stat(private_dir, Path::new(name), cap_fs::FollowSymlinks::No).map_err(
        |error| {
            SnapshotAttemptError::Unsafe(format!(
                "reinspect private registry snapshot {name}: {error}"
            ))
        },
    )?;
    validate_regular(&after, name)?;
    let actual_sha256: [u8; 32] = hash.finalize().into();
    let after_identity = leaf_identity(&after).ok_or_else(|| {
        SnapshotAttemptError::Unsafe(format!(
            "reinspected private registry snapshot {name} has no usable identity"
        ))
    })?;
    if opened_identity != after_identity
        || opened.len() != after.len()
        || bytes != expected.bytes
        || actual_sha256 != expected.sha256
    {
        return Err(SnapshotAttemptError::Unsafe(format!(
            "private registry snapshot {name} changed or differs from its source copy"
        )));
    }
    Ok(FileObservation {
        identity: opened_identity,
        bytes,
        sha256: actual_sha256,
    })
}

fn verify_private_snapshot_before_sqlite(
    private_dir: &fs::File,
    initial: &RegistryManifest,
) -> std::result::Result<PrivateSnapshotManifest, SnapshotAttemptError> {
    let main = PRIVATE_REGISTRY_MAIN;
    let wal = format!("{PRIVATE_REGISTRY_MAIN}-wal");
    let shm = format!("{PRIVATE_REGISTRY_MAIN}-shm");
    let mut allowed = std::collections::BTreeSet::new();
    let LeafObservation::Present(main_expected) = &initial.main else {
        return Err(SnapshotAttemptError::Unsafe(
            "private registry snapshot has no main source observation".to_owned(),
        ));
    };
    allowed.insert(main.to_owned());
    let private_main = verify_private_snapshot_leaf(private_dir, main, main_expected)?;
    let private_wal = if let LeafObservation::Present(wal_expected) = &initial.wal {
        allowed.insert(wal.clone());
        Some(verify_private_snapshot_leaf(
            private_dir,
            &wal,
            wal_expected,
        )?)
    } else {
        None
    };
    if !matches!(
        cap_fs::stat(private_dir, Path::new(&shm), cap_fs::FollowSymlinks::No),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    ) {
        return Err(SnapshotAttemptError::Unsafe(format!(
            "private registry snapshot has unexpected pre-open sidecar {shm}"
        )));
    }
    let entries = cap_fs::read_base_dir(private_dir).map_err(|error| {
        SnapshotAttemptError::Unstable(format!("enumerate private registry snapshot: {error}"))
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            SnapshotAttemptError::Unstable(format!(
                "enumerate private registry snapshot entry: {error}"
            ))
        })?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            return Err(SnapshotAttemptError::Unsafe(
                "private registry snapshot has a non-UTF-8 leaf".to_owned(),
            ));
        };
        if !allowed.contains(&name) {
            return Err(SnapshotAttemptError::Unsafe(format!(
                "private registry snapshot has unexpected pre-open leaf {name}"
            )));
        }
    }
    Ok(PrivateSnapshotManifest {
        main: private_main,
        wal: private_wal,
    })
}

fn snapshot_attempt(
    binding: &RegistryParentBinding,
    #[cfg(test)] hook: Option<&SnapshotTestHook>,
    #[cfg(not(test))] _hook: Option<&()>,
    #[cfg(test)] attempt: usize,
    #[cfg(not(test))] _attempt: usize,
) -> std::result::Result<(Connection, RegistrySnapshot), SnapshotAttemptError> {
    let parent = binding.parent.as_ref();
    let parent_path = binding.parent_path.as_path();
    let main = binding.leaf.as_str();
    let storage = PrivateSnapshotStorage::create()?;
    let initial = match observe_manifest(parent, parent_path, main, Some(&storage), true) {
        Ok(initial) => initial,
        Err(SnapshotAttemptError::Missing) => {
            #[cfg(test)]
            if let Some(hook) = hook {
                hook(SnapshotTestPhase::AfterInitialManifest, attempt, None);
            }
            let fresh_parent = fresh_registry_parent_binding(binding)?;
            return match observe_manifest(fresh_parent.as_ref(), parent_path, main, None, true) {
                Err(SnapshotAttemptError::Missing) => {
                    #[cfg(test)]
                    if let Some(hook) = hook {
                        hook(
                            SnapshotTestPhase::AfterFinalManifestBeforeParentRecheck,
                            attempt,
                            None,
                        );
                    }
                    fresh_registry_parent_binding(binding)?;
                    Err(SnapshotAttemptError::Missing)
                }
                Ok(_) => Err(SnapshotAttemptError::Unstable(
                    "scope registry appeared while confirming an all-absent snapshot".to_owned(),
                )),
                Err(error) => Err(error),
            };
        }
        Err(error) => return Err(error),
    };
    let private_dir = {
        #[cfg(windows)]
        {
            storage.as_windows()?.capability().map_err(|error| {
                SnapshotAttemptError::Unstable(format!(
                    "clone owner-private registry snapshot capability: {error}"
                ))
            })?
        }
        #[cfg(not(windows))]
        cap_fs::open_ambient_dir(storage.path(), cap_primitives::ambient_authority()).map_err(
            |error| {
                SnapshotAttemptError::Unstable(format!(
                    "open private registry snapshot directory capability: {error}"
                ))
            },
        )?
    };
    let private_initial = verify_private_snapshot_before_sqlite(&private_dir, &initial)?;
    #[cfg(test)]
    if let Some(hook) = hook {
        hook(SnapshotTestPhase::AfterInitialManifest, attempt, None);
        hook(
            SnapshotTestPhase::BeforeSnapshotSqliteOpen,
            attempt,
            Some(storage.path()),
        );
    }
    let private_current = verify_private_snapshot_before_sqlite(&private_dir, &initial)?;
    if private_initial != private_current {
        return Err(SnapshotAttemptError::Unsafe(
            "private registry snapshot changed before SQLite open".to_owned(),
        ));
    }
    let snapshot_path = storage.path().join(PRIVATE_REGISTRY_MAIN);
    let conn = match Connection::open_with_flags(
        &snapshot_path,
        // The snapshot directory is private and owned.  Do not pass NOFOLLOW
        // here: SQLite's Unix VFS applies it to the private SHM sidecar too,
        // preventing the read-only handle from initializing that private file.
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(conn) => conn,
        Err(error) => {
            return Err(classify_snapshot_sqlite_failure(
                parent,
                parent_path,
                main,
                &initial,
                sqlite_snapshot_error(error, "open registry snapshot", false),
            ));
        }
    };
    if let Err(error) = conn.busy_timeout(Duration::from_millis(5000)) {
        return Err(classify_snapshot_sqlite_failure(
            parent,
            parent_path,
            main,
            &initial,
            sqlite_snapshot_error(error, "configure registry snapshot", false),
        ));
    }
    if let Err(error) = conn.pragma_update(None, "query_only", "ON") {
        return Err(classify_snapshot_sqlite_failure(
            parent,
            parent_path,
            main,
            &initial,
            sqlite_snapshot_error(error, "make registry snapshot query-only", false),
        ));
    }
    // Execute a real query before the final source recheck.  This exercises
    // committed WAL visibility without ever passing the source path to SQLite.
    if let Err(error) = conn
        .query_row("SELECT 1 FROM scopes LIMIT 1", [], |row| {
            row.get::<_, i64>(0)
        })
        .optional()
    {
        return Err(classify_snapshot_sqlite_failure(
            parent,
            parent_path,
            main,
            &initial,
            sqlite_snapshot_error(error, "query registry snapshot", true),
        ));
    }
    #[cfg(test)]
    if let Some(hook) = hook {
        hook(
            SnapshotTestPhase::AfterSnapshotQueryBeforeRecheck,
            attempt,
            None,
        );
    }
    let fresh_parent = fresh_registry_parent_binding(binding)?;
    let final_manifest = observe_manifest(fresh_parent.as_ref(), parent_path, main, None, false)?;
    if initial != final_manifest {
        return Err(SnapshotAttemptError::Unstable(
            "scope registry main/WAL/SHM changed while snapshotting".to_owned(),
        ));
    }
    #[cfg(test)]
    if let Some(hook) = hook {
        hook(
            SnapshotTestPhase::AfterFinalManifestBeforeParentRecheck,
            attempt,
            None,
        );
    }
    fresh_registry_parent_binding(binding)?;
    Ok((
        conn,
        RegistrySnapshot {
            _private_dir: private_dir,
            _storage: storage,
        },
    ))
}

impl RegistryDb {
    /// Open the registry as a strictly read-only cache. Evidence operations use
    /// this path so an absent registry never creates directories, a database,
    /// WAL sidecars, or schema state.
    pub fn open_read_only(
        path: impl AsRef<Path>,
    ) -> std::result::Result<Self, RegistrySnapshotError> {
        Self::open_read_only_with_budget(path.as_ref(), REGISTRY_SNAPSHOT_RETRY_WINDOW)
    }

    /// Return a private, owned snapshot rather than opening the mutable source
    /// registry.  SQLite is therefore free to create a private SHM file while
    /// the source main/WAL/SHM leaves remain untouched.  This is deliberately
    /// stable-or-fail, not a claim of a cross-file atomic filesystem snapshot.
    fn open_read_only_with_budget(
        path: &Path,
        retry_window: Duration,
    ) -> std::result::Result<Self, RegistrySnapshotError> {
        let started = Instant::now();
        let mut saw_instability = None;
        for attempt in 0..MAX_REGISTRY_SNAPSHOT_ATTEMPTS {
            let result = match stable_registry_parent_binding(path) {
                Ok(Some(binding)) => snapshot_attempt(&binding, None, attempt),
                Ok(None) => Err(SnapshotAttemptError::Missing),
                Err(error) => Err(error),
            };
            match result {
                Ok((conn, snapshot)) => {
                    return Ok(Self {
                        conn,
                        _inherited_parent: None,
                        _snapshot: Some(snapshot),
                    });
                }
                Err(SnapshotAttemptError::Unsafe(message)) => {
                    return Err(RegistrySnapshotError::UnsafeIntegrity(message));
                }
                Err(SnapshotAttemptError::Missing) => {
                    return match saw_instability {
                        Some(message) => Err(RegistrySnapshotError::UnstableBusy(message)),
                        None => Err(RegistrySnapshotError::Missing),
                    };
                }
                Err(SnapshotAttemptError::Unstable(message)) => {
                    saw_instability = Some(message.clone());
                    if started.elapsed() >= retry_window
                        || attempt + 1 == MAX_REGISTRY_SNAPSHOT_ATTEMPTS
                    {
                        return Err(RegistrySnapshotError::UnstableBusy(message));
                    }
                }
            }
            // Keep retrying within the same five-second busy boundary used by
            // registry SQLite handles.  A short yield avoids a hot spin while
            // preserving a bounded failure mode.
            std::thread::yield_now();
        }
        Err(RegistrySnapshotError::UnstableBusy(
            saw_instability
                .unwrap_or_else(|| "scope registry snapshot retry budget exhausted".to_owned()),
        ))
    }

    #[cfg(test)]
    fn open_read_only_for_test(
        path: &Path,
        attempts: usize,
        hook: SnapshotTestHook,
    ) -> std::result::Result<Self, RegistrySnapshotError> {
        let mut saw_instability = None;
        for attempt in 0..attempts {
            let result = match stable_registry_parent_binding(path) {
                Ok(Some(binding)) => snapshot_attempt(&binding, Some(&hook), attempt),
                Ok(None) => Err(SnapshotAttemptError::Missing),
                Err(error) => Err(error),
            };
            match result {
                Ok((conn, snapshot)) => {
                    return Ok(Self {
                        conn,
                        _inherited_parent: None,
                        _snapshot: Some(snapshot),
                    });
                }
                Err(SnapshotAttemptError::Unsafe(message)) => {
                    return Err(RegistrySnapshotError::UnsafeIntegrity(message));
                }
                Err(SnapshotAttemptError::Missing) => {
                    return match saw_instability {
                        Some(message) => Err(RegistrySnapshotError::UnstableBusy(message)),
                        None => Err(RegistrySnapshotError::Missing),
                    };
                }
                Err(SnapshotAttemptError::Unstable(message)) if attempt + 1 == attempts => {
                    return Err(RegistrySnapshotError::UnstableBusy(message));
                }
                Err(SnapshotAttemptError::Unstable(message)) => saw_instability = Some(message),
            }
        }
        Err(RegistrySnapshotError::UnstableBusy(
            "scope registry test retry budget exhausted".to_owned(),
        ))
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        #[cfg(target_os = "linux")]
        let inherited = inherited_registry_parent(path.as_ref())?;
        #[cfg(not(target_os = "linux"))]
        let inherited: Option<(PathBuf, kio_core::store_dir::StoreDirectory)> = None;

        let (sqlite_path, inherited_parent) = if let Some((sqlite_path, parent)) = inherited {
            (sqlite_path, Some(parent))
        } else {
            if let Some(parent) = path.as_ref().parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|err| crate::IndexError::Schema(err.to_string()))?;
                // P2: the device data dir (`~/.local/share/kio`) that holds this
                // registry, the cost ledger, logs and the open-cache carries usage
                // patterns and the scope map — restrict it to the owner (0700) so a
                // multi-user host cannot read another user's data. Best-effort (the
                // registry is a recoverable cache); no-op on non-unix.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ =
                        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
                }
            }
            (path.as_ref().to_path_buf(), None)
        };
        let conn = Connection::open(sqlite_path)?;
        // P6 (05 §1.8 / docs/05:565): serialize concurrent writers with WAL +
        // busy_timeout so a parallel `kio init`/`index` upsert waits (up to 5s)
        // for the write lock instead of hitting SQLITE_BUSY and silently dropping
        // the scope registration, and so a concurrent reader sees the last
        // committed snapshot rather than a transient open failure. WAL is a
        // persistent DB property; busy_timeout is per-connection.
        conn.busy_timeout(Duration::from_millis(5000))?;
        let _journal_mode: String =
            conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS scopes (
                scope_id TEXT NOT NULL,
                kio_path TEXT NOT NULL,
                root_path TEXT NOT NULL,
                participates_in_global_search INTEGER NOT NULL DEFAULT 1,
                indexed INTEGER NOT NULL DEFAULT 0,
                last_seen_at TEXT NOT NULL,
                PRIMARY KEY (scope_id, kio_path)
            );",
        )?;
        Ok(Self {
            conn,
            _inherited_parent: inherited_parent,
            _snapshot: None,
        })
    }

    pub fn open_default() -> Result<Self> {
        Self::open(default_registry_path()?)
    }

    pub fn open_default_read_only() -> Result<Self> {
        Self::open_read_only(default_registry_path()?).map_err(index_error_from_snapshot)
    }

    /// R15-3: retire every registration for `kio_path` whose `scope_id` differs
    /// from `current_scope_id`, returning the number of rows removed. A deleted-
    /// then-re-`init`ed `.kio` mints a FRESH `scope_id` at the SAME path; because
    /// the primary key is `(scope_id, kio_path)`, the stale row otherwise survives
    /// forever and multi-scope search returns the same document twice — once via the
    /// dead `scope_id` whose Evidence Pointers can no longer resolve
    /// (`KIO-E-EVIDENCE-SCOPE-UNREACHABLE-001`). The registry is a recoverable search
    /// cache (03 §4), so dropping a stale row is always safe: the live scope re-adds
    /// itself here. Call this immediately before [`upsert`](Self::upsert).
    pub fn retire_stale_kio_path(&self, kio_path: &str, current_scope_id: &str) -> Result<usize> {
        let removed = self.conn.execute(
            "DELETE FROM scopes WHERE kio_path = ?1 AND scope_id != ?2",
            params![kio_path, current_scope_id],
        )?;
        Ok(removed)
    }

    pub fn upsert(&self, entry: &RegistryEntry) -> Result<()> {
        self.conn.execute(
            "INSERT INTO scopes (
                scope_id, kio_path, root_path,
                participates_in_global_search, indexed, last_seen_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT (scope_id, kio_path) DO UPDATE SET
                root_path = excluded.root_path,
                participates_in_global_search = excluded.participates_in_global_search,
                indexed = MAX(scopes.indexed, excluded.indexed),
                last_seen_at = excluded.last_seen_at",
            params![
                entry.scope_id,
                entry.kio_path,
                entry.root_path,
                entry.participates_in_global_search as i64,
                entry.indexed as i64,
                entry.last_seen_at,
            ],
        )?;
        Ok(())
    }

    /// Atomically replace app-prevalidated former registrations with their new
    /// paths. This is deliberately narrower than pruning: only the exact
    /// `(scope_id, kio_path)` pairs supplied by the app are deleted.
    pub fn rebind_scope_registrations(
        &mut self,
        rebinds: &[ScopeRegistrationRebind],
    ) -> Result<()> {
        let transaction = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut seen_scope_ids = BTreeSet::new();
        let mut seen_new_kio_paths = BTreeSet::new();
        for rebind in rebinds {
            if !seen_scope_ids.insert(rebind.scope_id.as_str()) {
                return Err(crate::IndexError::Contract(format!(
                    "scope registration rebind repeats scope_id: {}",
                    rebind.scope_id
                )));
            }
            if rebind.scope_id != rebind.new_registration.scope_id {
                return Err(crate::IndexError::Contract(
                    "scope registration rebind scope_id disagrees with new registration".to_owned(),
                ));
            }
            if !seen_new_kio_paths.insert(rebind.new_registration.kio_path.as_str()) {
                return Err(crate::IndexError::Contract(format!(
                    "scope registration rebind repeats destination kio_path: {}",
                    rebind.new_registration.kio_path
                )));
            }
            let expected = rebind
                .old_kio_paths
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>();
            if expected.len() != rebind.old_kio_paths.len() {
                return Err(crate::IndexError::Contract(format!(
                    "scope registration rebind repeats an old kio_path for scope_id: {}",
                    rebind.scope_id
                )));
            }
            let actual = {
                let mut statement = transaction
                    .prepare("SELECT kio_path FROM scopes WHERE scope_id = ?1 ORDER BY kio_path")?;
                statement
                    .query_map([&rebind.scope_id], |row| row.get::<_, String>(0))?
                    .collect::<std::result::Result<BTreeSet<_>, _>>()?
            };
            let resumed = BTreeSet::from([rebind.new_registration.kio_path.clone()]);
            if actual != expected && actual != resumed {
                return Err(crate::IndexError::Contract(format!(
                    "scope registration rebind observed unexpected paths for scope_id: {}",
                    rebind.scope_id
                )));
            }

            for old_kio_path in &rebind.old_kio_paths {
                if old_kio_path != &rebind.new_registration.kio_path {
                    transaction.execute(
                        "DELETE FROM scopes WHERE scope_id = ?1 AND kio_path = ?2",
                        params![&rebind.scope_id, old_kio_path],
                    )?;
                }
            }
            // A moved scope must be indexed again before global search uses its
            // new path, even when a resumed destination was indexed earlier.
            transaction.execute(
                "INSERT INTO scopes (
                    scope_id, kio_path, root_path,
                    participates_in_global_search, indexed, last_seen_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                ON CONFLICT(scope_id, kio_path) DO UPDATE SET
                    root_path = excluded.root_path,
                    participates_in_global_search = excluded.participates_in_global_search,
                    indexed = excluded.indexed,
                    last_seen_at = excluded.last_seen_at",
                params![
                    &rebind.scope_id,
                    &rebind.new_registration.kio_path,
                    &rebind.new_registration.root_path,
                    rebind.new_registration.participates_in_global_search as i64,
                    false,
                    &rebind.new_registration.last_seen_at,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Scopes eligible for default cross-scope search (05-runtime.md §1.8):
    /// `participates_in_global_search = true` and indexed. Deterministic order.
    pub fn search_targets(&self) -> Result<Vec<RegistryEntry>> {
        self.query_entries(
            "SELECT scope_id, kio_path, root_path,
                    participates_in_global_search, indexed, last_seen_at
             FROM scopes
             WHERE participates_in_global_search = 1 AND indexed = 1
             ORDER BY root_path, scope_id",
            params![],
        )
    }

    /// All registrations for a scope_id, most recently seen first
    /// (08-evidence-pointer-spec.md §3.1 step 1b).
    pub fn lookup_scope_id(&self, scope_id: &str) -> Result<Vec<RegistryEntry>> {
        self.query_entries(
            "SELECT scope_id, kio_path, root_path,
                    participates_in_global_search, indexed, last_seen_at
             FROM scopes
             WHERE scope_id = ?1
             ORDER BY last_seen_at DESC, kio_path",
            params![scope_id],
        )
    }

    /// Evidence callers use this typed variant so an owned snapshot query
    /// failure cannot be mistaken for a missing cache and bypass duplicate
    /// resolution safeguards.
    pub fn lookup_scope_id_snapshot(
        &self,
        scope_id: &str,
    ) -> std::result::Result<Vec<RegistryEntry>, RegistrySnapshotError> {
        self.lookup_scope_id(scope_id).map_err(snapshot_query_error)
    }

    pub fn get(&self, scope_id: &str, kio_path: &str) -> Result<Option<RegistryEntry>> {
        let entry = self
            .conn
            .query_row(
                "SELECT scope_id, kio_path, root_path,
                        participates_in_global_search, indexed, last_seen_at
                 FROM scopes
                 WHERE scope_id = ?1 AND kio_path = ?2",
                params![scope_id, kio_path],
                row_to_entry,
            )
            .optional()?;
        Ok(entry)
    }

    /// Every registration, deterministic order — PB25's `--registry-prune`
    /// (step4b-contract-tests-p2b.md §H) enumerates the whole table to find
    /// unreachable rows, not just search targets or one scope_id's rows.
    pub fn all_entries(&self) -> Result<Vec<RegistryEntry>> {
        self.query_entries(
            "SELECT scope_id, kio_path, root_path,
                    participates_in_global_search, indexed, last_seen_at
             FROM scopes
             ORDER BY root_path, scope_id",
            params![],
        )
    }

    /// PB25: remove one `(scope_id, kio_path)` registration — used only for a
    /// row proven unreachable (no re-init, no re-discovery possible); a live
    /// duplicate is never removed here (PB21's fail-closed dedupe is a user
    /// decision, not automatic).
    pub fn remove(&self, scope_id: &str, kio_path: &str) -> Result<bool> {
        let removed = self.conn.execute(
            "DELETE FROM scopes WHERE scope_id = ?1 AND kio_path = ?2",
            params![scope_id, kio_path],
        )?;
        Ok(removed > 0)
    }

    fn query_entries(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<Vec<RegistryEntry>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params, row_to_entry)?;
        let mut entries = Vec::new();
        for row in rows {
            entries.push(row?);
        }
        Ok(entries)
    }
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<RegistryEntry> {
    Ok(RegistryEntry {
        scope_id: row.get(0)?,
        kio_path: row.get(1)?,
        root_path: row.get(2)?,
        participates_in_global_search: row.get::<_, i64>(3)? != 0,
        indexed: row.get::<_, i64>(4)? != 0,
        last_seen_at: row.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    struct RegistrySnapshot {
        bytes: Vec<u8>,
        is_file: bool,
        len: u64,
        readonly: bool,
        #[cfg(unix)]
        mode: u32,
        #[cfg(unix)]
        nlink: u64,
    }

    fn snapshot_registry(path: &Path) -> RegistrySnapshot {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        let metadata = std::fs::symlink_metadata(path).unwrap();
        RegistrySnapshot {
            bytes: std::fs::read(path).unwrap(),
            is_file: metadata.file_type().is_file(),
            len: metadata.len(),
            readonly: metadata.permissions().readonly(),
            #[cfg(unix)]
            mode: metadata.mode(),
            #[cfg(unix)]
            nlink: metadata.nlink(),
        }
    }

    fn entry(scope_id: &str, root: &str, participates: bool, indexed: bool) -> RegistryEntry {
        RegistryEntry {
            scope_id: scope_id.to_owned(),
            kio_path: format!("{root}/.kio"),
            root_path: root.to_owned(),
            participates_in_global_search: participates,
            indexed,
            last_seen_at: "2026-07-03T00:00:00Z".to_owned(),
        }
    }

    fn open_temp() -> (tempfile::TempDir, RegistryDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = RegistryDb::open(dir.path().join("scope-registry.sqlite")).unwrap();
        (dir, db)
    }

    #[cfg(target_os = "linux")]
    fn inherited_private_tempdir() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;

        tempfile::Builder::new()
            .prefix("kio-registry-inherited-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap()
    }

    #[test]
    fn open_sets_wal_and_busy_timeout() {
        // P6: the registry must open with WAL + a 5000ms busy_timeout so parallel
        // init/index writers serialize instead of silently dropping registrations.
        let (_dir, db) = open_temp();
        let journal: String = db
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal.to_lowercase(), "wal");
        let timeout: i64 = db
            .conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(timeout, 5000);
    }

    #[test]
    fn read_only_open_never_creates_a_missing_registry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("scope-registry.sqlite");
        assert!(RegistryDb::open_read_only(&path).is_err());
        assert!(!path.exists());
        assert!(!path.parent().unwrap().exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inherited_registry_writer_keeps_duplicated_parent_after_fd_retarget() {
        use std::os::fd::AsRawFd;

        let first = inherited_private_tempdir();
        let replacement = inherited_private_tempdir();
        let held_first = fs::File::open(first.path()).unwrap();
        // SAFETY: fcntl duplicates a live descriptor; the returned descriptor
        // is owned by this test and closed below.
        let descriptor = unsafe { libc::fcntl(held_first.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 100) };
        assert!(descriptor >= 100);
        let inherited = PathBuf::from(format!("/dev/fd/{descriptor}/kio/scope-registry.sqlite"));

        let db = RegistryDb::open(&inherited).unwrap();
        let replacement_root = fs::File::open(replacement.path()).unwrap();
        // SAFETY: both descriptors are live directory descriptors. Replacing
        // the caller-owned spelling must not retarget RegistryDb's duplicate.
        assert_eq!(
            unsafe { libc::dup2(replacement_root.as_raw_fd(), descriptor) },
            descriptor
        );
        db.upsert(&entry("scope_fd", "/tmp/fd", true, true))
            .unwrap();
        drop(db);

        assert!(first.path().join("kio/scope-registry.sqlite").is_file());
        assert!(
            !replacement
                .path()
                .join("kio/scope-registry.sqlite")
                .exists()
        );
        // SAFETY: this test owns `descriptor` after the dup2 above.
        assert_eq!(unsafe { libc::close(descriptor) }, 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn read_only_reads_inherited_descriptor_registry_without_mutating_source() {
        use std::os::fd::AsRawFd;

        let root = inherited_private_tempdir();
        let held_root = fs::File::open(root.path()).unwrap();
        let path = PathBuf::from(format!(
            "/dev/fd/{}/kio/scope-registry.sqlite",
            held_root.as_raw_fd()
        ));
        let writer = RegistryDb::open(&path).unwrap();
        writer
            .upsert(&entry(
                "scope_inherited_read",
                "/tmp/inherited-read",
                true,
                true,
            ))
            .unwrap();
        drop(writer);
        let source = root.path().join("kio/scope-registry.sqlite");
        let before = snapshot_registry(&source);

        let reader = RegistryDb::open_read_only(&path).unwrap();
        assert_eq!(
            reader
                .lookup_scope_id("scope_inherited_read")
                .unwrap()
                .len(),
            1
        );
        drop(reader);
        assert_eq!(snapshot_registry(&source), before);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn read_only_inherited_missing_suffix_is_a_zero_effect_cache_miss() {
        use std::os::fd::AsRawFd;

        let root = inherited_private_tempdir();
        let held_root = fs::File::open(root.path()).unwrap();
        let path = PathBuf::from(format!(
            "/dev/fd/{}/missing/scope-registry.sqlite",
            held_root.as_raw_fd()
        ));

        assert!(matches!(
            RegistryDb::open_read_only(&path),
            Err(RegistrySnapshotError::Missing)
        ));
        assert!(!root.path().join("missing").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn read_only_rejects_inherited_descriptor_retarget_during_snapshot() {
        use std::os::fd::AsRawFd;
        use std::sync::atomic::{AtomicBool, Ordering};

        let first = inherited_private_tempdir();
        let replacement = inherited_private_tempdir();
        let first_path = first.path().join("kio/scope-registry.sqlite");
        let replacement_path = replacement.path().join("kio/scope-registry.sqlite");
        let writer = RegistryDb::open(&first_path).unwrap();
        writer
            .upsert(&entry("scope_first", "/tmp/first", true, true))
            .unwrap();
        drop(writer);
        drop(RegistryDb::open(&replacement_path).unwrap());

        let held_first = fs::File::open(first.path()).unwrap();
        // SAFETY: fcntl duplicates a live directory descriptor and this test
        // closes the returned descriptor after the snapshot attempt.
        let descriptor = unsafe { libc::fcntl(held_first.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 100) };
        assert!(descriptor >= 100);
        let inherited = PathBuf::from(format!("/dev/fd/{descriptor}/kio/scope-registry.sqlite"));
        let replacement_root = fs::File::open(replacement.path()).unwrap();
        let swapped = std::sync::Arc::new(AtomicBool::new(false));
        let swapped_once = swapped.clone();
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, _, _| {
            if phase == SnapshotTestPhase::AfterInitialManifest
                && !swapped_once.swap(true, Ordering::SeqCst)
            {
                // SAFETY: both descriptors remain live for the hook and the
                // test owns `descriptor` until it closes it below.
                assert_eq!(
                    unsafe { libc::dup2(replacement_root.as_raw_fd(), descriptor) },
                    descriptor
                );
            }
        });

        assert!(matches!(
            RegistryDb::open_read_only_for_test(&inherited, 1, hook),
            Err(RegistrySnapshotError::UnsafeIntegrity(_))
        ));
        // SAFETY: this test owns `descriptor` after the dup2 above.
        assert_eq!(unsafe { libc::close(descriptor) }, 0);
    }

    #[cfg(unix)]
    #[test]
    fn read_only_rejects_symlinked_registry_parent_without_touching_target() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let victim = tempfile::tempdir().unwrap();
        let linked_parent = root.path().join("registry-parent");
        symlink(victim.path(), &linked_parent).unwrap();
        let path = linked_parent.join("scope-registry.sqlite");

        assert!(matches!(
            RegistryDb::open_read_only(&path),
            Err(RegistrySnapshotError::UnsafeIntegrity(_))
        ));
        assert!(!victim.path().join("scope-registry.sqlite").exists());
    }

    #[test]
    fn read_only_open_reads_existing_registry_without_schema_changes() {
        let (dir, db) = open_temp();
        db.upsert(&entry("scope_a", "/tmp/a", true, true)).unwrap();
        let db = RegistryDb::open_read_only(dir.path().join("scope-registry.sqlite")).unwrap();
        assert_eq!(db.lookup_scope_id("scope_a").unwrap().len(), 1);
    }

    #[test]
    fn read_only_lookup_does_not_create_wal_sidecars_or_mutate_registry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scope-registry.sqlite");
        let wal_path = dir.path().join("scope-registry.sqlite-wal");
        let shm_path = dir.path().join("scope-registry.sqlite-shm");
        let expected = entry("scope_a", "/tmp/a", true, true);

        // Create a WAL-mode registry with a committed lookup row, then close the
        // writer. Closing the only writer checkpoints it, leaving no sidecars for
        // the read-only evidence path to inherit.
        let writer = RegistryDb::open(&path).unwrap();
        writer.upsert(&expected).unwrap();
        let journal_mode: String = writer
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode.to_lowercase(), "wal");
        drop(writer);
        assert!(
            !wal_path.exists(),
            "test precondition: writer close must leave no -wal sidecar"
        );
        assert!(
            !shm_path.exists(),
            "test precondition: writer close must leave no -shm sidecar"
        );
        let before = snapshot_registry(&path);
        assert_eq!(before.bytes.get(18), Some(&2), "WAL read version missing");
        assert_eq!(before.bytes.get(19), Some(&2), "WAL write version missing");

        let reader = RegistryDb::open_read_only(&path).unwrap();
        assert_eq!(reader.lookup_scope_id("scope_a").unwrap(), vec![expected]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let temp = reader._snapshot.as_ref().unwrap()._storage.path();
            assert_eq!(fs::metadata(temp).unwrap().mode() & 0o777, 0o700);
            assert_eq!(
                fs::metadata(temp.join(PRIVATE_REGISTRY_MAIN))
                    .unwrap()
                    .mode()
                    & 0o777,
                0o600
            );
            // Any SQLite sidecars here are private implementation state; the
            // source sidecars remain absent below.
        }
        drop(reader);

        assert_eq!(snapshot_registry(&path), before, "main registry changed");
        let wal_exists = wal_path.exists();
        let shm_exists = shm_path.exists();
        assert!(
            !wal_exists && !shm_exists,
            "read-only lookup created WAL sidecars: -wal={wal_exists}, -shm={shm_exists}"
        );
    }

    #[test]
    fn read_only_snapshot_sees_committed_uncheckpointed_wal_without_touching_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scope-registry.sqlite");
        let wal = dir.path().join("scope-registry.sqlite-wal");
        let shm = dir.path().join("scope-registry.sqlite-shm");
        let expected = entry("scope_wal", "/tmp/wal", true, true);

        let writer = RegistryDb::open(&path).unwrap();
        writer
            .conn
            .pragma_update(None, "wal_autocheckpoint", 0i64)
            .unwrap();
        // Keep a reader transaction alive so the committed WAL frame remains
        // available for the snapshot instead of relying on close/checkpoint.
        let reader = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        reader
            .execute_batch("BEGIN; SELECT count(*) FROM scopes;")
            .unwrap();
        writer.upsert(&expected).unwrap();
        assert!(wal.is_file(), "fixture requires a source WAL");
        assert!(shm.is_file(), "fixture requires a source SHM");
        let fresh = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let count: i64 = fresh
            .query_row(
                "SELECT count(*) FROM scopes WHERE scope_id = ?1",
                params!["scope_wal"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "fixture committed row must be normally visible");
        let before_main = snapshot_registry(&path);
        let before_wal = snapshot_registry(&wal);
        let before_shm = snapshot_registry(&shm);

        let snapshot = RegistryDb::open_read_only(&path).unwrap();
        assert_eq!(
            snapshot.lookup_scope_id("scope_wal").unwrap(),
            vec![expected]
        );
        drop(snapshot);

        assert_eq!(snapshot_registry(&path), before_main);
        assert_eq!(snapshot_registry(&wal), before_wal);
        assert_eq!(snapshot_registry(&shm), before_shm);
        drop(fresh);
        drop(reader);
        drop(writer);
    }

    #[test]
    fn read_only_missing_registry_is_typed_and_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("scope-registry.sqlite");
        assert!(matches!(
            RegistryDb::open_read_only(&path),
            Err(RegistrySnapshotError::Missing)
        ));
        assert!(!path.parent().unwrap().exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_registry_path_normalization_only_rewrites_var_alias() {
        assert_eq!(
            normalized_absolute_registry_path(Path::new("/var/folders/kio/scope-registry.sqlite"))
                .unwrap(),
            PathBuf::from("/private/var/folders/kio/scope-registry.sqlite")
        );
        assert_eq!(
            normalized_absolute_registry_path(Path::new("/tmp/kio/scope-registry.sqlite")).unwrap(),
            PathBuf::from("/tmp/kio/scope-registry.sqlite")
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_only_snapshot_rejects_immediate_parent_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        symlink(&target, &link).unwrap();
        let path = link.join("scope-registry.sqlite");
        assert!(matches!(
            RegistryDb::open_read_only(&path),
            Err(RegistrySnapshotError::UnsafeIntegrity(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn read_only_snapshot_rejects_nested_ancestor_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        symlink(&target, &link).unwrap();
        let child = link.join("child");
        fs::create_dir(target.join("child")).unwrap();
        let path = child.join("scope-registry.sqlite");
        assert!(matches!(
            RegistryDb::open_read_only(&path),
            Err(RegistrySnapshotError::UnsafeIntegrity(_))
        ));
    }

    #[test]
    fn parent_component_regular_directory_replacement_between_stat_and_open_is_unsafe() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("nested");
        fs::create_dir(&nested).unwrap();
        let path = nested.join("scope-registry.sqlite");
        let moved = dir.path().join("moved");
        let final_index = normalized_absolute_registry_path(&path)
            .unwrap()
            .parent()
            .unwrap()
            .components()
            .filter(|component| matches!(component, Component::Normal(_)))
            .count()
            - 1;
        PARENT_COMPONENT_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(std::sync::Arc::new(move |index| {
                if index == final_index {
                    fs::rename(&nested, &moved).unwrap();
                    fs::create_dir(&nested).unwrap();
                }
            }));
        });
        let result = RegistryDb::open_read_only(&path);
        PARENT_COMPONENT_HOOK.with(|slot| *slot.borrow_mut() = None);
        match result {
            Err(RegistrySnapshotError::UnsafeIntegrity(_)) => {}
            Err(error) => panic!("expected unsafe replacement failure, got {error}"),
            Ok(_) => panic!("expected replacement failure"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn all_absent_whole_parent_substitution_is_unsafe_not_missing() {
        let dir = tempfile::tempdir().unwrap();
        let canonical_parent = dir.path().join("canonical");
        fs::create_dir(&canonical_parent).unwrap();
        let path = canonical_parent.join("scope-registry.sqlite");
        let moved_parent = dir.path().join("detached");
        let replacement_parent = canonical_parent.clone();
        let replacement_path = path.clone();
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, attempt, _| {
            if attempt == 0 && phase == SnapshotTestPhase::AfterFinalManifestBeforeParentRecheck {
                fs::rename(&replacement_parent, &moved_parent).unwrap();
                fs::create_dir(&replacement_parent).unwrap();
                let replacement = RegistryDb::open(&replacement_path).unwrap();
                replacement
                    .upsert(&entry("replacement", "/tmp/replacement", true, true))
                    .unwrap();
            }
        });

        match RegistryDb::open_read_only_for_test(&path, 1, hook) {
            Err(RegistrySnapshotError::UnsafeIntegrity(message)) => {
                assert!(message.contains("canonical parent identity changed"));
            }
            Err(error) => panic!("expected unsafe parent substitution, got {error}"),
            Ok(_) => panic!("detached all-absent parent must not produce a snapshot"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn present_snapshot_whole_parent_substitution_is_unsafe() {
        let dir = tempfile::tempdir().unwrap();
        let canonical_parent = dir.path().join("canonical");
        fs::create_dir(&canonical_parent).unwrap();
        let path = canonical_parent.join("scope-registry.sqlite");
        let original = RegistryDb::open(&path).unwrap();
        original
            .upsert(&entry("original", "/tmp/original", true, true))
            .unwrap();
        drop(original);
        let moved_parent = dir.path().join("detached");
        let replacement_parent = canonical_parent.clone();
        let replacement_path = path.clone();
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, attempt, _| {
            if attempt == 0 && phase == SnapshotTestPhase::AfterFinalManifestBeforeParentRecheck {
                fs::rename(&replacement_parent, &moved_parent).unwrap();
                fs::create_dir(&replacement_parent).unwrap();
                let replacement = RegistryDb::open(&replacement_path).unwrap();
                replacement
                    .upsert(&entry("replacement", "/tmp/replacement", true, true))
                    .unwrap();
            }
        });

        match RegistryDb::open_read_only_for_test(&path, 1, hook) {
            Err(RegistrySnapshotError::UnsafeIntegrity(message)) => {
                assert!(message.contains("canonical parent identity changed"));
            }
            Err(error) => panic!("expected unsafe parent substitution, got {error}"),
            Ok(_) => panic!("detached private snapshot must not be accepted"),
        }
    }

    #[test]
    fn snapshot_final_all_absent_is_unstable_not_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scope-registry.sqlite");
        let writer = RegistryDb::open(&path).unwrap();
        writer
            .upsert(&entry("scope_a", "/tmp/a", true, true))
            .unwrap();
        drop(writer);
        let main = path.clone();
        let wal = dir.path().join("scope-registry.sqlite-wal");
        let shm = dir.path().join("scope-registry.sqlite-shm");
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, _, _| {
            if phase == SnapshotTestPhase::AfterSnapshotQueryBeforeRecheck {
                let _ = fs::remove_file(&main);
                let _ = fs::remove_file(&wal);
                let _ = fs::remove_file(&shm);
            }
        });
        assert!(matches!(
            RegistryDb::open_read_only_for_test(&path, 1, hook),
            Err(RegistrySnapshotError::UnstableBusy(_))
        ));
    }

    #[test]
    fn snapshot_absent_to_present_sidecar_is_unstable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scope-registry.sqlite");
        let writer = RegistryDb::open(&path).unwrap();
        writer
            .upsert(&entry("scope_a", "/tmp/a", true, true))
            .unwrap();
        drop(writer);
        let shm = dir.path().join("scope-registry.sqlite-shm");
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, _, _| {
            if phase == SnapshotTestPhase::AfterSnapshotQueryBeforeRecheck {
                fs::write(&shm, b"changed").unwrap();
            }
        });
        assert!(matches!(
            RegistryDb::open_read_only_for_test(&path, 1, hook),
            Err(RegistrySnapshotError::UnstableBusy(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn private_snapshot_preopen_sidecar_symlink_is_unsafe() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scope-registry.sqlite");
        let writer = RegistryDb::open(&path).unwrap();
        writer
            .upsert(&entry("scope_a", "/tmp/a", true, true))
            .unwrap();
        drop(writer);
        let target = dir.path().join("outside-shm-target");
        fs::write(&target, b"outside").unwrap();
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, _, private| {
            if phase == SnapshotTestPhase::BeforeSnapshotSqliteOpen {
                let private = private.expect("pre-open hook carries private snapshot path");
                symlink(&target, private.join("snapshot.sqlite-shm")).unwrap();
            }
        });
        assert!(matches!(
            RegistryDb::open_read_only_for_test(&path, 1, hook),
            Err(RegistrySnapshotError::UnsafeIntegrity(_))
        ));
    }

    #[test]
    fn private_snapshot_preopen_main_replacement_is_unsafe() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scope-registry.sqlite");
        let writer = RegistryDb::open(&path).unwrap();
        writer
            .upsert(&entry("scope_a", "/tmp/a", true, true))
            .unwrap();
        drop(writer);
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, _, private| {
            if phase == SnapshotTestPhase::BeforeSnapshotSqliteOpen {
                let private = private.expect("pre-open hook carries private snapshot path");
                let main = private.join(PRIVATE_REGISTRY_MAIN);
                let replacement = private.join("replacement.sqlite");
                fs::copy(&main, &replacement).unwrap();
                #[cfg(windows)]
                fs::remove_file(&main).unwrap();
                fs::rename(&replacement, &main).unwrap();
            }
        });
        assert!(matches!(
            RegistryDb::open_read_only_for_test(&path, 1, hook),
            Err(RegistrySnapshotError::UnsafeIntegrity(_))
        ));
    }

    #[test]
    fn snapshot_identity_replacement_is_unstable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scope-registry.sqlite");
        let writer = RegistryDb::open(&path).unwrap();
        writer
            .upsert(&entry("scope_a", "/tmp/a", true, true))
            .unwrap();
        drop(writer);
        let replacement = dir.path().join("replacement.sqlite");
        let source = path.clone();
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, _, _| {
            if phase == SnapshotTestPhase::AfterSnapshotQueryBeforeRecheck {
                fs::copy(&source, &replacement).unwrap();
                #[cfg(windows)]
                fs::remove_file(&source).unwrap();
                fs::rename(&replacement, &source).unwrap();
            }
        });
        assert!(matches!(
            RegistryDb::open_read_only_for_test(&path, 1, hook),
            Err(RegistrySnapshotError::UnstableBusy(_))
        ));
    }

    #[test]
    fn snapshot_churn_exhausts_deterministic_test_retry_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scope-registry.sqlite");
        let writer = RegistryDb::open(&path).unwrap();
        writer
            .upsert(&entry("scope_a", "/tmp/a", true, true))
            .unwrap();
        drop(writer);
        let shm = dir.path().join("scope-registry.sqlite-shm");
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, attempt, _| {
            if phase == SnapshotTestPhase::AfterSnapshotQueryBeforeRecheck {
                fs::write(&shm, format!("churn-{attempt}")).unwrap();
            }
        });
        assert!(matches!(
            RegistryDb::open_read_only_for_test(&path, 2, hook),
            Err(RegistrySnapshotError::UnstableBusy(_))
        ));
    }

    #[test]
    fn snapshot_instability_followed_by_disappearance_is_not_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scope-registry.sqlite");
        let writer = RegistryDb::open(&path).unwrap();
        writer
            .upsert(&entry("scope_a", "/tmp/a", true, true))
            .unwrap();
        drop(writer);
        let main = path.clone();
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, attempt, _| {
            if phase == SnapshotTestPhase::AfterSnapshotQueryBeforeRecheck && attempt == 0 {
                fs::remove_file(&main).unwrap();
            }
        });
        assert!(matches!(
            RegistryDb::open_read_only_for_test(&path, 2, hook),
            Err(RegistrySnapshotError::UnstableBusy(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_wal_disappearance_is_unstable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scope-registry.sqlite");
        let wal = dir.path().join("scope-registry.sqlite-wal");
        let writer = RegistryDb::open(&path).unwrap();
        writer
            .conn
            .pragma_update(None, "wal_autocheckpoint", 0i64)
            .unwrap();
        writer
            .upsert(&entry("scope_a", "/tmp/a", true, true))
            .unwrap();
        assert!(wal.exists());
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, _, _| {
            if phase == SnapshotTestPhase::AfterSnapshotQueryBeforeRecheck {
                fs::remove_file(&wal).unwrap();
            }
        });
        assert!(matches!(
            RegistryDb::open_read_only_for_test(&path, 1, hook),
            Err(RegistrySnapshotError::UnstableBusy(_))
        ));
        drop(writer);
    }

    // Windows retains the canonical parent without delete sharing for the entire
    // snapshot attempt.  The Unix replacement race above is therefore
    // structurally impossible: prove that the OS blocks it and that the stable
    // original binding has the expected cache-miss semantics instead.
    #[cfg(windows)]
    #[test]
    fn all_absent_bound_parent_blocks_substitution_and_returns_true_missing() {
        use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

        let dir = tempfile::tempdir().unwrap();
        let canonical_parent = dir.path().join("canonical");
        fs::create_dir(&canonical_parent).unwrap();
        let path = canonical_parent.join("scope-registry.sqlite");
        let moved_parent = dir.path().join("detached");
        let replacement_parent = canonical_parent.clone();
        let hook_moved_parent = moved_parent.clone();
        let rename_checked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_checked = std::sync::Arc::clone(&rename_checked);
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, attempt, _| {
            if attempt == 0 && phase == SnapshotTestPhase::AfterFinalManifestBeforeParentRecheck {
                let error = fs::rename(&replacement_parent, &hook_moved_parent)
                    .expect_err("the retained no-delete-share parent must block substitution");
                assert_eq!(
                    error.raw_os_error(),
                    Some(ERROR_SHARING_VIOLATION as i32),
                    "the canonical parent must remain bound for the full snapshot attempt"
                );
                hook_checked.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });

        assert!(matches!(
            RegistryDb::open_read_only_for_test(&path, 1, hook),
            Err(RegistrySnapshotError::Missing)
        ));
        assert!(
            rename_checked.load(std::sync::atomic::Ordering::SeqCst),
            "the test hook must prove the no-delete-share boundary"
        );
        assert!(
            canonical_parent.is_dir(),
            "the original parent must remain live"
        );
        assert!(
            !moved_parent.exists(),
            "a denied replacement must not create a detached parent"
        );
    }

    #[cfg(windows)]
    #[test]
    fn present_bound_parent_blocks_substitution_and_keeps_original_snapshot() {
        use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

        let dir = tempfile::tempdir().unwrap();
        let canonical_parent = dir.path().join("canonical");
        fs::create_dir(&canonical_parent).unwrap();
        let path = canonical_parent.join("scope-registry.sqlite");
        let original = RegistryDb::open(&path).unwrap();
        original
            .upsert(&entry("original", "/tmp/original", true, true))
            .unwrap();
        drop(original);
        let moved_parent = dir.path().join("detached");
        let replacement_parent = canonical_parent.clone();
        let hook_moved_parent = moved_parent.clone();
        let rename_checked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_checked = std::sync::Arc::clone(&rename_checked);
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, attempt, _| {
            if attempt == 0 && phase == SnapshotTestPhase::AfterFinalManifestBeforeParentRecheck {
                let error = fs::rename(&replacement_parent, &hook_moved_parent)
                    .expect_err("the retained no-delete-share parent must block substitution");
                assert_eq!(error.raw_os_error(), Some(ERROR_SHARING_VIOLATION as i32));
                hook_checked.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });

        let snapshot = RegistryDb::open_read_only_for_test(&path, 1, hook)
            .expect("the original stable parent and snapshot must remain usable");
        assert_eq!(
            snapshot.lookup_scope_id("original").unwrap(),
            vec![entry("original", "/tmp/original", true, true)]
        );
        assert!(
            rename_checked.load(std::sync::atomic::Ordering::SeqCst),
            "the test hook must prove the no-delete-share boundary"
        );
        assert!(
            canonical_parent.is_dir(),
            "the original parent must remain live"
        );
        assert!(
            !moved_parent.exists(),
            "substitution must not have occurred"
        );
    }

    // A live Windows SQLite writer owns its WAL without delete sharing.  The
    // Unix disappearance race is thus impossible while the writer is alive;
    // prove the sharing boundary and that the untouched WAL-backed snapshot is
    // accepted from its original source rather than a fabricated transition.
    #[cfg(windows)]
    #[test]
    fn live_wal_blocks_deletion_and_preserves_original_snapshot() {
        use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scope-registry.sqlite");
        let wal = dir.path().join("scope-registry.sqlite-wal");
        let expected = entry("scope_a", "/tmp/a", true, true);
        let writer = RegistryDb::open(&path).unwrap();
        writer
            .conn
            .pragma_update(None, "wal_autocheckpoint", 0i64)
            .unwrap();
        writer.upsert(&expected).unwrap();
        assert!(wal.exists());
        let delete_checked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_checked = std::sync::Arc::clone(&delete_checked);
        let hook: SnapshotTestHook = std::sync::Arc::new(move |phase, _, _| {
            if phase == SnapshotTestPhase::AfterSnapshotQueryBeforeRecheck {
                let error = fs::remove_file(&wal)
                    .expect_err("the live writer must block WAL deletion on Windows");
                assert_eq!(error.raw_os_error(), Some(ERROR_SHARING_VIOLATION as i32));
                hook_checked.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });

        let snapshot = RegistryDb::open_read_only_for_test(&path, 1, hook)
            .expect("an unchanged, WAL-backed snapshot must remain valid");
        assert_eq!(snapshot.lookup_scope_id("scope_a").unwrap(), vec![expected]);
        assert!(
            delete_checked.load(std::sync::atomic::Ordering::SeqCst),
            "the test hook must prove the live-WAL sharing boundary"
        );
        drop(writer);
    }

    #[test]
    fn stable_malformed_registry_schema_is_unsafe() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scope-registry.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE scopes (scope_id TEXT);")
            .unwrap();
        drop(conn);
        let snapshot = RegistryDb::open_read_only(&path).unwrap();
        match snapshot.lookup_scope_id_snapshot("scope") {
            Err(RegistrySnapshotError::UnsafeIntegrity(_)) => {}
            Err(error) => panic!("expected malformed schema unsafe error, got {error}"),
            Ok(_) => panic!("malformed schema lookup unexpectedly succeeded"),
        }
    }

    #[test]
    fn stable_empty_sqlite_registry_without_scopes_table_is_unsafe() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scope-registry.sqlite");
        drop(Connection::open(&path).unwrap());
        assert!(matches!(
            RegistryDb::open_read_only(&path),
            Err(RegistrySnapshotError::UnsafeIntegrity(_))
        ));
    }

    #[test]
    fn busy_snapshot_query_error_is_retryable() {
        let error = crate::IndexError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        ));
        assert!(matches!(
            snapshot_query_error(error),
            RegistrySnapshotError::UnstableBusy(_)
        ));
    }

    #[test]
    fn snapshot_query_cannot_open_and_io_errors_are_retryable() {
        for code in [rusqlite::ffi::SQLITE_CANTOPEN, rusqlite::ffi::SQLITE_IOERR] {
            let error = crate::IndexError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                None,
            ));
            assert!(matches!(
                snapshot_query_error(error),
                RegistrySnapshotError::UnstableBusy(_)
            ));
        }
    }

    #[test]
    fn snapshot_query_proven_corruption_and_schema_errors_are_unsafe() {
        for code in [
            rusqlite::ffi::SQLITE_CORRUPT,
            rusqlite::ffi::SQLITE_NOTADB,
            rusqlite::ffi::SQLITE_SCHEMA,
        ] {
            let error = crate::IndexError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                None,
            ));
            assert!(matches!(
                snapshot_query_error(error),
                RegistrySnapshotError::UnsafeIntegrity(_)
            ));
        }
    }

    #[test]
    fn private_snapshot_cannot_open_and_io_errors_are_retryable() {
        for code in [rusqlite::ffi::SQLITE_CANTOPEN, rusqlite::ffi::SQLITE_IOERR] {
            let error = rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None);
            assert!(matches!(
                sqlite_snapshot_error(error, "open registry snapshot", false),
                SnapshotAttemptError::Unstable(_)
            ));
        }
    }

    #[test]
    fn proven_snapshot_corruption_and_schema_errors_are_unsafe() {
        for code in [
            rusqlite::ffi::SQLITE_CORRUPT,
            rusqlite::ffi::SQLITE_NOTADB,
            rusqlite::ffi::SQLITE_SCHEMA,
        ] {
            let error = rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None);
            assert!(matches!(
                sqlite_snapshot_error(error, "query registry snapshot", true),
                SnapshotAttemptError::Unsafe(_)
            ));
        }
    }

    #[test]
    fn sqlite_error_is_unsafe_only_for_fixed_schema_probe() {
        let open_error = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
            None,
        );
        assert!(matches!(
            sqlite_snapshot_error(open_error, "open registry snapshot", false),
            SnapshotAttemptError::Unstable(_)
        ));
        let probe_error = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
            None,
        );
        assert!(matches!(
            sqlite_snapshot_error(probe_error, "query registry snapshot", true),
            SnapshotAttemptError::Unsafe(_)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_registry_path_policy_accepts_only_absolute_disk_or_unc_roots() {
        for accepted in [
            r"C:\kio\scope-registry.sqlite",
            r"\\server\share\kio\scope-registry.sqlite",
        ] {
            assert!(
                normalized_absolute_registry_path(Path::new(accepted)).is_ok(),
                "{accepted}"
            );
        }
        for rejected in [
            r"kio\scope-registry.sqlite",
            r"C:kio\scope-registry.sqlite",
            r"C:\kio\..\scope-registry.sqlite",
            r"\\?\C:\kio\scope-registry.sqlite",
            r"\\?\UNC\server\share\kio\scope-registry.sqlite",
            r"\\.\PhysicalDrive0",
        ] {
            assert!(
                normalized_absolute_registry_path(Path::new(rejected)).is_err(),
                "{rejected}"
            );
        }
    }

    #[test]
    fn snapshot_rejects_oversized_leaf_before_sqlite_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scope-registry.sqlite");
        let file = fs::File::create(&path).unwrap();
        file.set_len(MAX_REGISTRY_LEAF_BYTES + 1).unwrap();
        assert!(matches!(
            RegistryDb::open_read_only(&path),
            Err(RegistrySnapshotError::UnsafeIntegrity(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn read_only_snapshot_rejects_hardlinked_wal_leaf() {
        use std::fs::hard_link;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scope-registry.sqlite");
        let wal = dir.path().join("scope-registry.sqlite-wal");
        let victim = dir.path().join("wal-victim");
        let writer = RegistryDb::open(&path).unwrap();
        writer
            .conn
            .pragma_update(None, "wal_autocheckpoint", 0i64)
            .unwrap();
        writer
            .upsert(&entry("scope_wal", "/tmp/wal", true, true))
            .unwrap();
        assert!(wal.is_file());
        hard_link(&wal, &victim).unwrap();
        let error = match RegistryDb::open_read_only(&path) {
            Ok(_) => panic!("hardlinked WAL must be rejected"),
            Err(error) => error,
        };
        assert!(matches!(error, RegistrySnapshotError::UnsafeIntegrity(_)));
        drop(writer);
    }

    #[cfg(unix)]
    #[test]
    fn read_only_snapshot_rejects_symlinked_main_leaf() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.sqlite");
        fs::write(&target, b"not a registry").unwrap();
        let path = dir.path().join("scope-registry.sqlite");
        symlink(&target, &path).unwrap();
        assert!(matches!(
            RegistryDb::open_read_only(&path),
            Err(RegistrySnapshotError::UnsafeIntegrity(_))
        ));
    }

    #[test]
    fn search_targets_filters_participation_and_indexed() {
        let (_dir, db) = open_temp();
        db.upsert(&entry("scope_a", "/tmp/a", true, true)).unwrap();
        db.upsert(&entry("scope_b", "/tmp/b", false, true)).unwrap();
        db.upsert(&entry("scope_c", "/tmp/c", true, false)).unwrap();
        let targets = db.search_targets().unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].scope_id, "scope_a");
    }

    #[test]
    fn upsert_is_idempotent_and_updates_fields() {
        let (_dir, db) = open_temp();
        db.upsert(&entry("scope_a", "/tmp/a", true, false)).unwrap();
        let mut updated = entry("scope_a", "/tmp/a", true, true);
        updated.last_seen_at = "2026-07-04T00:00:00Z".to_owned();
        db.upsert(&updated).unwrap();
        let got = db.get("scope_a", "/tmp/a/.kio").unwrap().unwrap();
        assert!(got.indexed);
        assert_eq!(got.last_seen_at, "2026-07-04T00:00:00Z");
        assert_eq!(db.lookup_scope_id("scope_a").unwrap().len(), 1);
    }

    #[test]
    fn indexed_flag_is_not_cleared_by_later_unindexed_upsert() {
        let (_dir, db) = open_temp();
        db.upsert(&entry("scope_a", "/tmp/a", true, true)).unwrap();
        db.upsert(&entry("scope_a", "/tmp/a", true, false)).unwrap();
        assert!(db.get("scope_a", "/tmp/a/.kio").unwrap().unwrap().indexed);
    }

    #[test]
    fn rebind_scope_registrations_replaces_exact_old_rows_in_one_transaction() {
        let (_dir, mut db) = open_temp();
        db.upsert(&entry("scope_a", "/tmp/a-old", true, true))
            .unwrap();
        db.upsert(&entry("scope_b", "/tmp/b-old", false, true))
            .unwrap();
        let mut new_a = entry("scope_a", "/tmp/a-new", true, true);
        new_a.last_seen_at = "2026-07-05T00:00:00Z".to_owned();
        let new_b = entry("scope_b", "/tmp/b-new", false, true);

        db.rebind_scope_registrations(&[
            ScopeRegistrationRebind {
                scope_id: "scope_a".to_owned(),
                old_kio_paths: vec!["/tmp/a-old/.kio".to_owned()],
                new_registration: new_a.clone(),
            },
            ScopeRegistrationRebind {
                scope_id: "scope_b".to_owned(),
                old_kio_paths: vec!["/tmp/b-old/.kio".to_owned()],
                new_registration: new_b.clone(),
            },
        ])
        .unwrap();

        let rows_a = db.lookup_scope_id("scope_a").unwrap();
        let rows_b = db.lookup_scope_id("scope_b").unwrap();
        assert_eq!(rows_a.len(), 1);
        assert_eq!(rows_b.len(), 1);
        assert_eq!(rows_a[0].kio_path, new_a.kio_path);
        assert_eq!(rows_b[0].kio_path, new_b.kio_path);
        assert!(!rows_a[0].indexed);
        assert!(!rows_b[0].indexed);
    }

    #[test]
    fn rebind_scope_registrations_rejects_unexpected_rows_and_rolls_back_every_scope() {
        let (_dir, mut db) = open_temp();
        db.upsert(&entry("scope_a", "/tmp/a-old", true, true))
            .unwrap();
        db.upsert(&entry("scope_a", "/tmp/a-unexpected", true, true))
            .unwrap();
        db.upsert(&entry("scope_b", "/tmp/b-old", true, true))
            .unwrap();

        let result = db.rebind_scope_registrations(&[
            ScopeRegistrationRebind {
                scope_id: "scope_b".to_owned(),
                old_kio_paths: vec!["/tmp/b-old/.kio".to_owned()],
                new_registration: entry("scope_b", "/tmp/b-new", true, false),
            },
            ScopeRegistrationRebind {
                scope_id: "scope_a".to_owned(),
                old_kio_paths: vec!["/tmp/a-old/.kio".to_owned()],
                new_registration: entry("scope_a", "/tmp/a-new", true, false),
            },
        ]);
        assert!(matches!(result, Err(crate::IndexError::Contract(_))));
        assert!(db.get("scope_b", "/tmp/b-old/.kio").unwrap().is_some());
        assert!(db.get("scope_b", "/tmp/b-new/.kio").unwrap().is_none());
    }

    #[test]
    fn rebind_scope_registrations_accepts_already_completed_singleton() {
        let (_dir, mut db) = open_temp();
        db.upsert(&entry("scope_a", "/tmp/a-new", true, true))
            .unwrap();
        let new_registration = entry("scope_a", "/tmp/a-new", false, true);

        db.rebind_scope_registrations(&[ScopeRegistrationRebind {
            scope_id: "scope_a".to_owned(),
            old_kio_paths: vec!["/tmp/a-old/.kio".to_owned()],
            new_registration: new_registration.clone(),
        }])
        .unwrap();

        let rows = db.lookup_scope_id("scope_a").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kio_path, new_registration.kio_path);
        assert!(!rows[0].indexed);
        assert!(!rows[0].participates_in_global_search);
    }

    // R15-3: a deleted-then-re-`init`ed `.kio` mints a fresh scope_id at the same
    // path. Retiring the stale row before re-registering leaves exactly one row for
    // that path (no duplicate search target, no dead-pointer scope_id).
    #[test]
    fn retire_stale_kio_path_removes_only_other_scope_ids_at_same_path() {
        let (_dir, db) = open_temp();
        // Old scope_id registered + indexed at /tmp/a.
        db.upsert(&entry("scope_old", "/tmp/a", true, true))
            .unwrap();
        // An unrelated path must be untouched.
        db.upsert(&entry("scope_x", "/tmp/other", true, true))
            .unwrap();

        // Re-init: fresh scope_id at the SAME `.kio` path. Retire, then re-register.
        let removed = db
            .retire_stale_kio_path("/tmp/a/.kio", "scope_new")
            .unwrap();
        assert_eq!(removed, 1, "exactly the stale same-path row is retired");
        db.upsert(&entry("scope_new", "/tmp/a", true, true))
            .unwrap();

        // Only the fresh registration survives at /tmp/a.
        assert!(db.lookup_scope_id("scope_old").unwrap().is_empty());
        assert_eq!(db.lookup_scope_id("scope_new").unwrap().len(), 1);
        // The unrelated path is untouched.
        assert_eq!(db.lookup_scope_id("scope_x").unwrap().len(), 1);
        // Exactly one search target remains for the re-init'd path.
        let targets = db.search_targets().unwrap();
        assert_eq!(
            targets
                .iter()
                .filter(|t| t.kio_path == "/tmp/a/.kio")
                .count(),
            1
        );
    }

    #[test]
    fn lookup_scope_id_orders_by_last_seen_desc() {
        let (_dir, db) = open_temp();
        let mut old = entry("scope_a", "/tmp/old", true, true);
        old.last_seen_at = "2026-07-01T00:00:00Z".to_owned();
        db.upsert(&old).unwrap();
        let mut new = entry("scope_a", "/tmp/new", true, true);
        new.last_seen_at = "2026-07-02T00:00:00Z".to_owned();
        db.upsert(&new).unwrap();
        let found = db.lookup_scope_id("scope_a").unwrap();
        assert_eq!(found[0].root_path, "/tmp/new");
    }

    // PB25 (step4b-contract-tests-p2b.md §H): `all_entries`/`remove` are the
    // primitives `kio repair registry-prune` uses to enumerate the whole
    // table and delete a proven-unreachable row.
    #[test]
    fn all_entries_lists_every_row_and_remove_deletes_exactly_one() {
        let (_dir, db) = open_temp();
        db.upsert(&entry("scope_a", "/tmp/a", true, true)).unwrap();
        db.upsert(&entry("scope_b", "/tmp/b", true, true)).unwrap();
        assert_eq!(db.all_entries().unwrap().len(), 2);

        assert!(db.remove("scope_a", "/tmp/a/.kio").unwrap());
        let remaining = db.all_entries().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].scope_id, "scope_b");

        // Idempotent: removing an already-absent row is `false`, not an error.
        assert!(!db.remove("scope_a", "/tmp/a/.kio").unwrap());
    }
}
