//! Closed-world Q_hard measurement lane.
//!
//! Q_hard is deliberately not a portable synthetic fixture.  Its source
//! documents contain the raster/vector evidence which makes the questions
//! meaningful, so absence of an explicitly attested local fixture is a
//! blocked measurement, never a zero-cost passing result.

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsString,
    fs, io,
    path::{Component, Path, PathBuf},
    process::Command,
    sync::atomic::AtomicU64,
    time::{SystemTime, UNIX_EPOCH},
};
#[cfg(target_os = "macos")]
use std::{
    ffi::{CStr, CString},
    os::raw::{c_char, c_int, c_void},
    os::unix::ffi::OsStrExt,
};

use cap_primitives::{ambient_authority, fs as cap_fs};
use kio_core::{cas::hash_bytes, store_dir::StoreDirectory};
use kio_pipeline::task::{MAX_TASK_STORE_BYTES, TaskStore};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

use crate::{
    boundary::{
        directory_identity_from_file, directory_identity_from_path, sync_retained_directory,
    },
    runner::{BoundedProcessOptions, DEFAULT_PROCESS_TIMEOUT, run_bounded_command},
};

#[path = "comparator_runtime_builder.rs"]
pub mod comparator_runtime_builder;
pub use comparator_runtime_builder::{ComparatorRuntimeInstallSummary, install_comparator_runtime};

pub const FROZEN_GOLDEN_SHA256: &str =
    "sha256:d5c30eccc664e6bd4d96e1068970e225d209d04bde34c50eab300d6245d4e163";
pub const FROZEN_SYNTHETIC_M3_1_GOLDEN_SHA256: &str =
    "sha256:b7183fa3586383883ec522256696268eab8e607c1a032020e09223158a5bf08d";
/// A separate, frozen population used only for the baseline comparison lane.
pub const FROZEN_BASELINE_GOLDEN_SHA256: &str =
    "sha256:bdad3e02c4b70f721e882d7f24c8b5b442621be7c0c03593afde41b8ebca7d45";
const MAX_GOLDEN_BYTES: u64 = 64 * 1024;
const MAX_ATTESTATION_BYTES: u64 = 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 512 * 1024 * 1024;
const MAX_RUNTIME_CLOSURE_ENTRIES: usize = 512;
const MAX_RUNTIME_CLOSURE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_MACHO_RESOLUTION_CONTEXTS: usize = 4_096;
const MAX_MACHO_LOAD_COMMANDS: usize = 256;
const MAX_MACHO_PATH_BYTES: usize = 4 * 1024;
const MAX_MACHO_INSPECT_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_DYLD_INFO_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
#[cfg(target_os = "macos")]
const MAX_RUNTIME_XATTR_LIST_BYTES: usize = 64 * 1024;
const MAX_DYLD_CACHE_IMAGES: usize = 16_384;
const MAX_DYLD_CACHE_EDGES: usize = 262_144;
// Darwin's `MNT_RDONLY`.  Keep this as a policy constant rather than relying
// on an ambient mount option: comparator measurements must be backed by an
// immutable filesystem, not merely root-owned directory modes.
#[cfg(any(target_os = "macos", test))]
const MACOS_MNT_RDONLY: u64 = 0x0000_0001;
const MAX_LIVE_FIXTURE_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_LIVE_FIXTURE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_LIVE_FIXTURE_FILES: usize = 16_384;
const MAX_SCOPES: usize = 64;
const MAX_WALK_DIRECTORIES: usize = 8_192;
/// Bound entries before sorting or retaining them.  Directory entries are
/// attacker-controlled fixture input, including entries we later reject.
const MAX_DIRECTORY_ENTRIES: usize = MAX_LIVE_FIXTURE_FILES + MAX_WALK_DIRECTORIES;
const RESULT_K: usize = 10;
const ONLINE_QUERY_CREDENTIAL_NAMES: [&str; 2] = ["GEMINI_API_KEY", "MISTRAL_API_KEY"];
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct ControlledEnvironment {
    fixed: Vec<(OsString, OsString)>,
    // These are always directories in an evaluator-owned private snapshot.
    // Keep their capabilities even on macOS, where fdescfs cannot resolve a
    // child path below `/dev/fd/<directory-fd>`.
    directories: Vec<(OsString, RetainedDirectory)>,
}

impl ControlledEnvironment {
    fn apply(&self, command: &mut Command) -> Result<(), QhardError> {
        command.env_clear().envs(self.fixed.iter().cloned());
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            use std::os::{fd::AsRawFd, unix::process::CommandExt};
            let retained = self
                .directories
                .iter()
                .map(|(name, handle)| {
                    handle
                        .handle
                        .try_clone()
                        .map(|handle| (name.clone(), handle))
                        .map_err(|e| {
                            QhardError::Input(format!(
                                "cannot retain controlled environment directory: {e}"
                            ))
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            for (name, handle) in &retained {
                command.env(name, format!("/dev/fd/{}", handle.as_raw_fd()));
            }
            unsafe {
                command.pre_exec(move || {
                    for (_, handle) in &retained {
                        let flags = libc::fcntl(handle.as_raw_fd(), libc::F_GETFD);
                        if flags < 0
                            || libc::fcntl(
                                handle.as_raw_fd(),
                                libc::F_SETFD,
                                flags & !libc::FD_CLOEXEC,
                            ) != 0
                        {
                            return Err(io::Error::last_os_error());
                        }
                    }
                    Ok(())
                });
            }
            Ok(())
        }
        // Darwin's fdescfs reports a directory descriptor as a directory but
        // cannot traverse a child under it (`/dev/fd/N/kio`).  The evaluator
        // has already copied the fixture into an owner-only private snapshot;
        // use only those retained snapshot paths on this platform, and bind
        // them again immediately before/after every subprocess.
        #[cfg(target_os = "macos")]
        {
            self.recheck_private_directories()?;
            for (name, directory) in &self.directories {
                command.env(name, &directory.public_path);
            }
            Ok(())
        }
        #[cfg(windows)]
        {
            let _ = command;
            Err(QhardError::Input(
                "Q_hard controlled fixture environments require descriptor-bound directories on this platform".into(),
            ))
        }
    }

    fn recheck_private_directories(&self) -> Result<(), QhardError> {
        for (name, directory) in &self.directories {
            let listed = fs::symlink_metadata(&directory.public_path).map_err(|e| {
                QhardError::Input(format!(
                    "controlled environment directory {} changed: {e}",
                    name.to_string_lossy()
                ))
            })?;
            if listed.file_type().is_symlink() || !listed.is_dir() {
                return Err(QhardError::Input(format!(
                    "controlled environment directory {} is no longer a real directory",
                    name.to_string_lossy()
                )));
            }
            let listed_identity = directory_identity_from_path(&directory.public_path, &listed)
                .map_err(|e| {
                    QhardError::Input(format!(
                        "cannot inspect controlled environment directory {}: {e}",
                        name.to_string_lossy()
                    ))
                })?;
            let opened_identity = directory_identity_from_file(&directory.handle).map_err(|e| {
                QhardError::Input(format!(
                    "cannot inspect retained controlled environment directory {}: {e}",
                    name.to_string_lossy()
                ))
            })?;
            if !matches!((listed_identity, opened_identity), (Some(listed), Some(opened)) if listed == opened)
            {
                return Err(QhardError::Input(format!(
                    "controlled environment directory {} changed while bound",
                    name.to_string_lossy()
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct QhardOptions {
    pub golden: PathBuf,
    pub fixture_root: PathBuf,
    pub tree: String,
    pub env_name: String,
    pub attestation: Option<PathBuf>,
    pub bin: PathBuf,
    pub k: usize,
    pub online_query: bool,
}

#[derive(Debug, Error)]
pub enum QhardError {
    #[error("invalid Q_hard benchmark input: {0}")]
    Input(String),
    #[error("Q_hard benchmark process failed: {0}")]
    Process(#[from] crate::runner::BoundedProcessError),
    #[error("comparator runtime installation outcome is indeterminate: {0}")]
    Indeterminate(String),
    #[error("could not serialize Q_hard benchmark report: {0}")]
    Serialize(#[from] serde_json::Error),
}

#[derive(Debug, Serialize)]
pub struct QhardReport {
    schema_version: u64,
    benchmark: &'static str,
    measurement_class: &'static str,
    acceptance_eligible: bool,
    blocked_reason: Option<String>,
    fixture: FixtureBinding,
    binary: FileBinding,
    golden: FileBinding,
    configuration: Configuration,
    rows: Vec<Row>,
    hits: usize,
    total: usize,
    recall_at_10: f64,
    synthetic_m3_1: Option<SyntheticBinding>,
    combined_hits: Option<usize>,
    combined_total: Option<usize>,
}

impl QhardReport {
    /// Q_hard alone is an external-fixture evidence lane, not a complete
    /// M3-1 acceptance run.  The CLI may only return success for a combined,
    /// cryptographically run-bound synthetic + Q_hard result supplied by a
    /// future orchestrator API.
    #[must_use]
    pub fn acceptance_passed(&self) -> bool {
        self.acceptance_eligible
            && self.combined_total == Some(26)
            && self.combined_hits.unwrap_or(0) >= 21
    }

    pub fn combine_synthetic_m3_1(
        &mut self,
        golden_path: &Path,
        hits: usize,
        total: usize,
    ) -> Result<(), QhardError> {
        let (_, golden) = binding(golden_path, MAX_GOLDEN_BYTES, "synthetic M3-1 golden")?;
        if golden.sha256 != FROZEN_SYNTHETIC_M3_1_GOLDEN_SHA256 || total != 18 || hits > total {
            return Err(QhardError::Input(
                "synthetic M3-1 measurement differs from the frozen 18-query contract".into(),
            ));
        }
        self.acceptance_eligible = true;
        self.blocked_reason = None;
        self.combined_hits = Some(self.hits + hits);
        self.combined_total = Some(self.total + total);
        self.synthetic_m3_1 = Some(SyntheticBinding {
            golden,
            hits,
            total,
        });
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct FileBinding {
    path: String,
    sha256: String,
    bytes: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RuntimeTrust {
    AdministratorRuntime,
    MacosSealedSystem,
    MacosDyldSharedCache,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct RuntimeClosureEntry {
    path: String,
    trust: RuntimeTrust,
    binding: RuntimeClosureBinding,
}

/// A physical file hash is deliberately not fabricated for an image which is
/// supplied by the sealed dyld shared cache rather than the filesystem.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RuntimeClosureBinding {
    File(FileBinding),
    DyldSharedCache(DyldSharedCacheBinding),
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct DyldSharedCacheBinding {
    install_name: String,
    architecture: String,
    uuid: String,
    linked_dylibs: Vec<DyldSharedCacheEdge>,
    missing_weak_dylibs: Vec<DyldSharedCacheEdge>,
    inspector: FileBinding,
    platform: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
struct DyldSharedCacheEdge {
    attributes: String,
    path: String,
}

#[derive(Debug, Clone)]
struct DyldSharedCacheCatalog {
    inspector: FileBinding,
    architecture: String,
    platform: String,
    images: BTreeMap<String, DyldSharedCacheImage>,
}

#[derive(Debug, Clone)]
struct DyldSharedCacheImage {
    uuid: String,
    linked_dylibs: Vec<DyldSharedCacheEdge>,
}

enum DyldCatalogRecord {
    Ignored,
    NeedUuid {
        path: String,
    },
    NeedUuidValue {
        path: String,
    },
    NeedLinkedDylibs {
        path: String,
        uuid: String,
    },
    NeedAttributes {
        path: String,
        uuid: String,
    },
    Edges {
        path: String,
        uuid: String,
        edges: Vec<DyldSharedCacheEdge>,
    },
}

/// The filesystem identity that held the administrator runtime at binding
/// time.  `fsid`, mount point, source, and filesystem type together make a
/// remount or a replacement volume observable; `read_only` is retained in the
/// report rather than inferred from the successful bind.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct RuntimeMountIdentity {
    fsid: String,
    mount_point: String,
    mounted_from: String,
    filesystem_type: String,
    flags: u64,
    read_only: bool,
}

/// Provenance for every executable image that dyld can load for fixture-B's
/// rga comparison.  Entries are a canonical, sorted closure: each one is
/// either beneath the administrator-supplied runtime root or is a terminal
/// macOS sealed-system library beneath an explicitly allowed system root.
#[derive(Debug, Clone, Serialize)]
struct ComparatorRuntimeProvenance {
    root: String,
    mount: RuntimeMountIdentity,
    xattr_policy: String,
    inspector: FileBinding,
    closure_sha256: String,
    closure: Vec<RuntimeClosureEntry>,
}

#[derive(Debug, Clone, Copy)]
struct RuntimeFileIdentity {
    len: u64,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}

impl RuntimeFileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            #[cfg(unix)]
            dev: metadata.dev(),
            #[cfg(unix)]
            ino: metadata.ino(),
        }
    }

    fn matches(self, metadata: &fs::Metadata) -> bool {
        self.len == metadata.len() && {
            #[cfg(unix)]
            {
                self.dev == metadata.dev() && self.ino == metadata.ino()
            }
            #[cfg(not(unix))]
            {
                true
            }
        }
    }
}

#[derive(Debug, Clone)]
struct RuntimeBoundFile {
    trust: RuntimeTrust,
    binding: FileBinding,
    identity: RuntimeFileIdentity,
}

#[derive(Debug)]
struct ComparatorRuntime {
    root: PathBuf,
    root_handle: RetainedDirectory,
    mount: RuntimeMountIdentity,
    bin_directory: PathBuf,
    config_path: PathBuf,
    config: RuntimeBoundFile,
    provenance: ComparatorRuntimeProvenance,
    files: BTreeMap<PathBuf, RuntimeBoundFile>,
    entry_paths: BTreeMap<String, PathBuf>,
}

/// A newly resolved dynamic-loader view of an administrator runtime.  This is
/// deliberately rebuilt rather than inferred from the initial set of images:
/// dyld's `@rpath` lookup is sensitive to new higher-priority candidates that
/// were not present when the runtime was first bound.
struct ResolvedRuntimeClosure {
    inspector: FileBinding,
    files: BTreeMap<PathBuf, RuntimeBoundFile>,
    closure: Vec<RuntimeClosureEntry>,
    closure_sha256: String,
}
#[derive(Debug, Serialize)]
struct FixtureBinding {
    root: String,
    tree: String,
    env_name: String,
    attestation: FileBinding,
    live_sha256: String,
    scopes: Vec<String>,
}
#[derive(Debug, Serialize)]
struct Configuration {
    k: usize,
    online_query: bool,
    forwarded_credential_names: Vec<&'static str>,
}
#[derive(Debug, Serialize)]
struct SyntheticBinding {
    golden: FileBinding,
    hits: usize,
    total: usize,
}
#[derive(Debug, Serialize)]
struct Row {
    query_id: String,
    class: String,
    hit: bool,
    expected_paths: Vec<String>,
    returned_paths: Vec<String>,
    returncode: i32,
    duration_ms: f64,
}

#[derive(Debug)]
struct Fixture {
    root: RetainedDirectory,
    tree: String,
    env_name: String,
    scope_relatives: Vec<String>,
    attestation: FileBinding,
    live_sha256: String,
}

/// An evaluator-owned, immutable-for-the-duration-of-this-process copy of the
/// attested fixture.  `kio search` still uses path APIs internally, so retained
/// source directory handles alone cannot protect its descendant `.kio` and XDG
/// accesses from a same-UID rename-and-restore race.
struct FixtureSnapshot {
    _temp: tempfile::TempDir,
    root: RetainedDirectory,
    scopes: Vec<RetainedDirectory>,
}

/// A bounded private copy of a regular-file tree, retaining the source handle
/// and its digest so callers can prove the public input did not change while a
/// path-based child process was running against the copy.
pub struct PrivateTreeSnapshot {
    _temp: tempfile::TempDir,
    path: PathBuf,
    source: RetainedDirectory,
    source_digest: String,
}

impl PrivateTreeSnapshot {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn verify_source_unchanged(&self) -> Result<(), QhardError> {
        if fixture_live_digest(&self.source)? != self.source_digest {
            return Err(QhardError::Input(
                "synthetic corpus changed during combined measurement".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
struct RetainedDirectory {
    public_path: PathBuf,
    handle: fs::File,
    sync_metadata: fs::Metadata,
}

impl RetainedDirectory {
    fn open(path: &Path, label: &str) -> Result<Self, QhardError> {
        let listed = fs::symlink_metadata(path).map_err(|e| {
            QhardError::Input(format!("{label} is missing: {}: {e}", path.display()))
        })?;
        if listed.file_type().is_symlink() || !listed.is_dir() {
            return Err(QhardError::Input(format!(
                "{label} must be a real directory"
            )));
        }
        let listed_identity = directory_identity_from_path(path, &listed)
            .map_err(|e| QhardError::Input(format!("cannot inspect {label}: {e}")))?
            .ok_or_else(|| QhardError::Input(format!("{label} must have a stable identity")))?;
        let canonical = fs::canonicalize(path)
            .map_err(|e| QhardError::Input(format!("cannot canonicalize {label}: {e}")))?;
        let parent = canonical
            .parent()
            .ok_or_else(|| QhardError::Input(format!("{label} has no parent")))?;
        let leaf = canonical
            .file_name()
            .ok_or_else(|| QhardError::Input(format!("{label} has no final component")))?;
        let parent_handle = cap_fs::open_ambient_dir(parent, ambient_authority())
            .map_err(|e| QhardError::Input(format!("cannot open {label} parent: {e}")))?;
        let handle = cap_fs::open_dir_nofollow(&parent_handle, Path::new(leaf)).map_err(|_| {
            QhardError::Input(format!("{label} must be a real non-reparse directory"))
        })?;
        let opened = handle
            .metadata()
            .map_err(|e| QhardError::Input(format!("cannot inspect {label}: {e}")))?;
        let opened_identity = directory_identity_from_file(&handle)
            .map_err(|e| QhardError::Input(format!("cannot inspect {label}: {e}")))?
            .ok_or_else(|| QhardError::Input(format!("{label} must have a stable identity")))?;
        if listed_identity != opened_identity {
            return Err(QhardError::Input(format!("{label} changed while binding")));
        }
        Ok(Self {
            public_path: canonical,
            handle,
            sync_metadata: opened,
        })
    }

    fn child(&self, name: &str, label: &str) -> Result<Self, QhardError> {
        let public_path = self.public_path.join(name);
        let listed = fs::symlink_metadata(&public_path)
            .map_err(|e| QhardError::Input(format!("cannot inspect {label}: {e}")))?;
        if listed.file_type().is_symlink() || !listed.is_dir() {
            return Err(QhardError::Input(format!(
                "{label} must be a real directory"
            )));
        }
        let listed_identity = directory_identity_from_path(&public_path, &listed)
            .map_err(|e| QhardError::Input(format!("cannot inspect {label}: {e}")))?
            .ok_or_else(|| QhardError::Input(format!("{label} must have a stable identity")))?;
        let handle = cap_fs::open_dir_nofollow(&self.handle, Path::new(name)).map_err(|_| {
            QhardError::Input(format!("{label} must be a real non-reparse directory"))
        })?;
        let identity = handle
            .metadata()
            .map_err(|e| QhardError::Input(format!("cannot inspect {label}: {e}")))?;
        if !identity.is_dir() {
            return Err(QhardError::Input(format!(
                "{label} must be a real directory"
            )));
        }
        let opened_identity = directory_identity_from_file(&handle)
            .map_err(|e| QhardError::Input(format!("cannot inspect {label}: {e}")))?
            .ok_or_else(|| QhardError::Input(format!("{label} must have a stable identity")))?;
        if listed_identity != opened_identity {
            return Err(QhardError::Input(format!("{label} changed while binding")));
        }
        Ok(Self {
            public_path,
            handle,
            sync_metadata: identity,
        })
    }

    #[cfg(unix)]
    fn configure_command_cwd(&self, command: &mut Command) -> Result<(), QhardError> {
        use std::os::{fd::AsRawFd, unix::process::CommandExt};
        let cwd = self
            .handle
            .try_clone()
            .map_err(|e| QhardError::Input(format!("cannot retain fixture scope cwd: {e}")))?;
        unsafe {
            command.pre_exec(move || {
                if libc::fchdir(cwd.as_raw_fd()) == 0 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            });
        }
        Ok(())
    }

    #[cfg(windows)]
    fn configure_command_cwd(&self, command: &mut Command) -> Result<(), QhardError> {
        command.current_dir(&self.public_path);
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Attestation {
    schema_version: u64,
    fixture_id: String,
    tree: String,
    env_name: String,
    golden_sha256: String,
    fixture_content_sha256: String,
    scopes: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoldenRow {
    query_id: String,
    scenario: String,
    class: String,
    query: String,
    expected: Vec<Expected>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Expected {
    path: String,
    format: String,
    unit_prefix: String,
    section_hint: String,
}

fn regular(path: &Path, maximum: u64, label: &str) -> Result<Vec<u8>, QhardError> {
    let parent = path
        .parent()
        .ok_or_else(|| QhardError::Input(format!("{label} has no parent")))?;
    let name = path
        .file_name()
        .ok_or_else(|| QhardError::Input(format!("{label} has no filename")))?;
    let parent = RetainedDirectory::open(parent, &format!("{label} parent"))?;
    regular_at(&parent.handle, Path::new(name), maximum, label)
}

fn regular_at(
    root: &fs::File,
    name: &Path,
    maximum: u64,
    label: &str,
) -> Result<Vec<u8>, QhardError> {
    let before = cap_fs::stat(root, name, cap_fs::FollowSymlinks::No)
        .map_err(|e| QhardError::Input(format!("{label} is missing: {e}")))?;
    if !before.file_type().is_file() || before.len() > maximum {
        return Err(QhardError::Input(format!(
            "{label} must be a bounded regular file"
        )));
    }
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let file = cap_fs::open(root, name, &options).map_err(|e| {
        QhardError::Input(format!("cannot open {label} without following links: {e}"))
    })?;
    let opened = cap_fs::Metadata::from_file(&file)
        .map_err(|e| QhardError::Input(format!("cannot inspect {label}: {e}")))?;
    let after = cap_fs::stat(root, name, cap_fs::FollowSymlinks::No)
        .map_err(|e| QhardError::Input(format!("cannot recheck {label}: {e}")))?;
    if !opened.file_type().is_file()
        || opened.len() > maximum
        || !same_file(&before, &opened)
        || !same_file(&opened, &after)
    {
        return Err(QhardError::Input(format!("{label} changed while opening")));
    }
    let mut bytes = Vec::with_capacity(opened.len() as usize);
    use io::Read;
    file.take(maximum + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| QhardError::Input(format!("cannot read {label}: {e}")))?;
    if bytes.len() as u64 != opened.len() {
        return Err(QhardError::Input(format!("{label} changed while reading")));
    }
    Ok(bytes)
}

fn binding(path: &Path, maximum: u64, label: &str) -> Result<(Vec<u8>, FileBinding), QhardError> {
    let bytes = regular(path, maximum, label)?;
    Ok((
        bytes.clone(),
        FileBinding {
            path: path.display().to_string(),
            sha256: hash_bytes(&bytes),
            bytes: bytes.len(),
        },
    ))
}

fn binding_at(
    root: &RetainedDirectory,
    name: &str,
    maximum: u64,
    label: &str,
) -> Result<(Vec<u8>, FileBinding), QhardError> {
    let bytes = regular_at(&root.handle, Path::new(name), maximum, label)?;
    Ok((
        bytes.clone(),
        FileBinding {
            path: root.public_path.join(name).display().to_string(),
            sha256: hash_bytes(&bytes),
            bytes: bytes.len(),
        },
    ))
}

/// Snapshot a verified executable into an evaluator-owned directory before
/// running it. `Command::new(path)` cannot retain an already-open executable
/// descriptor, so executing the public path after a check would otherwise
/// retain a path-swap race.
pub fn snapshot_binary(path: &Path) -> Result<(tempfile::TempDir, PathBuf), QhardError> {
    let (bytes, _) = binding(path, MAX_BINARY_BYTES, "kio binary")?;
    let temp = tempfile::Builder::new()
        .prefix("kio-qhard-bin-")
        .tempdir()
        .map_err(|e| QhardError::Input(format!("cannot create private binary snapshot: {e}")))?;
    let snapshot = temp.path().join("kio");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&snapshot)
        .map_err(|e| QhardError::Input(format!("cannot create private binary snapshot: {e}")))?;
    use io::Write;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| QhardError::Input(format!("cannot write private binary snapshot: {e}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o700)).map_err(|e| {
            QhardError::Input(format!(
                "cannot mark private binary snapshot executable: {e}"
            ))
        })?;
    }
    Ok((temp, snapshot))
}

fn bound_executable(path: &Path) -> Result<(tempfile::TempDir, PathBuf, FileBinding), QhardError> {
    let (temp, snapshot) = snapshot_binary(path)?;
    let (_, binding) = binding(&snapshot, MAX_BINARY_BYTES, "private kio binary")?;
    Ok((temp, snapshot, binding))
}

fn safe_name(name: &str, label: &str) -> Result<(), QhardError> {
    let mut components = Path::new(name).components();
    if name.is_empty()
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(QhardError::Input(format!(
            "{label} must be one normal path component"
        )));
    }
    Ok(())
}

fn safe_relative(value: &str) -> Result<PathBuf, QhardError> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(QhardError::Input(
            "attestation scope is not a normalized relative path".into(),
        ));
    }
    Ok(path.to_path_buf())
}

fn same_file(left: &cap_fs::Metadata, right: &cap_fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use cap_primitives::fs::MetadataExt;
        return left.dev() == right.dev() && left.ino() == right.ino();
    }
    #[cfg(windows)]
    {
        use cap_primitives::fs::_WindowsByHandle;
        return left.volume_serial_number() == right.volume_serial_number()
            && left.file_index() == right.file_index();
    }
    #[allow(unreachable_code)]
    false
}

fn discover_scopes(tree: RetainedDirectory) -> Result<Vec<RetainedDirectory>, QhardError> {
    let mut todo = vec![tree];
    let mut found = Vec::new();
    let mut seen = 0usize;
    while let Some(directory) = todo.pop() {
        seen += 1;
        if seen > MAX_WALK_DIRECTORIES {
            return Err(QhardError::Input(
                "fixture tree exceeds directory traversal bound".into(),
            ));
        }
        let mut entries = Vec::new();
        for entry in cap_fs::read_dir(&directory.handle, Path::new("."))
            .map_err(|e| QhardError::Input(format!("cannot read fixture tree: {e}")))?
        {
            entries.push(
                entry.map_err(|e| {
                    QhardError::Input(format!("cannot enumerate fixture tree: {e}"))
                })?,
            );
            if entries.len() > MAX_DIRECTORY_ENTRIES {
                return Err(QhardError::Input(
                    "fixture directory exceeds entry-count bound".into(),
                ));
            }
        }
        entries.sort_by_key(|entry| entry.file_name());
        let mut has_kio = false;
        for entry in entries {
            let ty = entry
                .file_type()
                .map_err(|e| QhardError::Input(format!("cannot inspect fixture entry: {e}")))?;
            if ty.is_symlink() {
                continue;
            }
            if ty.is_dir() {
                if entry.file_name() == ".kio" {
                    has_kio = true;
                } else {
                    todo.push(
                        directory
                            .child(&entry.file_name().to_string_lossy(), "fixture tree entry")?,
                    );
                }
            }
        }
        if has_kio {
            found.push(directory);
            if found.len() > MAX_SCOPES {
                return Err(QhardError::Input(
                    "fixture has too many registered scopes".into(),
                ));
            }
        }
    }
    found.sort_by(|left, right| left.public_path.cmp(&right.public_path));
    if found.is_empty() {
        return Err(QhardError::Input("fixture has no registered scope".into()));
    }
    Ok(found)
}

/// Digest every regular fixture file through retained directory handles.  The
/// digest is deliberately over file names, types, and individual content
/// digests, so it catches replacement, addition, deletion, and content races
/// without retaining an unbounded fixture in memory.
fn fixture_live_digest(root: &RetainedDirectory) -> Result<String, QhardError> {
    let mut records = Vec::new();
    let mut budget = FixtureDigestBudget::default();
    fingerprint_directory(&root.handle, Path::new(""), &mut records, &mut budget)?;
    Ok(hash_bytes(records.join("\n").as_bytes()))
}

/// Stable fixture content digest excludes the root attestation file, avoiding
/// a self-referential digest when the attestation is stored in the fixture.
fn fixture_content_digest(root: &RetainedDirectory) -> Result<String, QhardError> {
    let mut records = Vec::new();
    let mut budget = FixtureDigestBudget::default();
    fingerprint_directory_excluding(
        &root.handle,
        Path::new(""),
        &mut records,
        &mut budget,
        Path::new("qhard-attestation.json"),
    )?;
    Ok(hash_bytes(records.join("\n").as_bytes()))
}

fn fingerprint_directory_excluding(
    directory: &fs::File,
    prefix: &Path,
    records: &mut Vec<String>,
    _budget: &mut FixtureDigestBudget,
    excluded: &Path,
) -> Result<(), QhardError> {
    // Reuse the normal bounded walk when no exclusion is encountered; remove
    // only the deterministic root attestation record from its canonical input.
    if prefix.as_os_str().is_empty() {
        let temp = tempfile::tempdir().map_err(|e| QhardError::Input(e.to_string()))?;
        copy_fixture_directory(
            directory,
            &temp.path().join("copy"),
            &mut FixtureDigestBudget::default(),
        )?;
        let copy = RetainedDirectory::open(&temp.path().join("copy"), "digest copy")?;
        let _ = fs::remove_file(temp.path().join("copy").join(excluded));
        return fingerprint_directory(
            &copy.handle,
            Path::new(""),
            records,
            &mut FixtureDigestBudget::default(),
        );
    }
    fingerprint_directory(directory, prefix, records, _budget)
}

fn copy_fixture_directory(
    source: &fs::File,
    destination: &Path,
    budget: &mut FixtureDigestBudget,
) -> Result<(), QhardError> {
    budget.directories += 1;
    if budget.directories > MAX_WALK_DIRECTORIES {
        return Err(QhardError::Input(
            "fixture snapshot exceeds directory traversal bound".into(),
        ));
    }
    fs::create_dir(destination)
        .map_err(|e| QhardError::Input(format!("cannot create private fixture directory: {e}")))?;
    #[cfg(unix)]
    fs::set_permissions(destination, fs::Permissions::from_mode(0o700))
        .map_err(|e| QhardError::Input(format!("cannot secure private fixture directory: {e}")))?;
    let mut seen = 0usize;
    for entry in cap_fs::read_dir(source, Path::new("."))
        .map_err(|e| QhardError::Input(format!("cannot enumerate fixture snapshot source: {e}")))?
    {
        seen += 1;
        if seen > MAX_DIRECTORY_ENTRIES {
            return Err(QhardError::Input(
                "fixture directory exceeds entry-count bound".into(),
            ));
        }
        let entry = entry.map_err(|e| {
            QhardError::Input(format!("cannot enumerate fixture snapshot source: {e}"))
        })?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| QhardError::Input("fixture contains non-UTF-8 entry".into()))?;
        let ty = entry.file_type().map_err(|e| {
            QhardError::Input(format!("cannot inspect fixture snapshot source: {e}"))
        })?;
        let output = destination.join(name);
        if ty.is_symlink() {
            return Err(QhardError::Input(format!(
                "fixture contains symlink: {name}"
            )));
        }
        if ty.is_dir() {
            let child = cap_fs::open_dir_nofollow(source, Path::new(name)).map_err(|_| {
                QhardError::Input(format!(
                    "fixture directory changed while snapshotting: {name}"
                ))
            })?;
            copy_fixture_directory(&child, &output, budget)?;
        } else if ty.is_file() {
            budget.files += 1;
            if budget.files > MAX_LIVE_FIXTURE_FILES {
                return Err(QhardError::Input(
                    "fixture snapshot exceeds file-count bound".into(),
                ));
            }
            let bytes = regular_at(
                source,
                Path::new(name),
                MAX_LIVE_FIXTURE_FILE_BYTES,
                "fixture snapshot file",
            )?;
            budget.bytes = budget
                .bytes
                .checked_add(bytes.len() as u64)
                .ok_or_else(|| {
                    QhardError::Input("fixture snapshot byte counter overflow".into())
                })?;
            if budget.bytes > MAX_LIVE_FIXTURE_BYTES {
                return Err(QhardError::Input(
                    "fixture snapshot exceeds aggregate byte bound".into(),
                ));
            }
            fs::write(&output, bytes).map_err(|e| {
                QhardError::Input(format!("cannot write private fixture snapshot: {e}"))
            })?;
            #[cfg(unix)]
            fs::set_permissions(&output, fs::Permissions::from_mode(0o600)).map_err(|e| {
                QhardError::Input(format!("cannot secure private fixture file: {e}"))
            })?;
        } else {
            return Err(QhardError::Input(format!(
                "fixture contains unsupported entry: {name}"
            )));
        }
    }
    Ok(())
}

fn rewrite_snapshot_registry(
    snapshot: &Path,
    source_root: &Path,
    env_name: &str,
) -> Result<(), QhardError> {
    let registry = snapshot
        .join("env")
        .join(env_name)
        .join("xdg-data/kio/scope-registry.sqlite");
    if !registry.exists() {
        return Err(QhardError::Input(
            "fixture snapshot has no scope registry".into(),
        ));
    }
    let mut connection = Connection::open(&registry)
        .map_err(|e| QhardError::Input(format!("cannot open snapshot scope registry: {e}")))?;
    let rows = connection
        .prepare("SELECT rowid, kio_path, root_path FROM scopes")
        .and_then(|mut statement| {
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(|e| QhardError::Input(format!("cannot read snapshot scope registry: {e}")))?;
    if rows.is_empty() {
        return Err(QhardError::Input(
            "snapshot scope registry has no fixture-bound scope paths".into(),
        ));
    }
    // Validate the complete registry before changing it. `repair replica`
    // follows these paths, so a single retained off-tree row must never gain a
    // chance to be opened from the evaluator-owned environment.
    for (_, kio_path, root_path) in &rows {
        registry_scope_relative(source_root, root_path, kio_path, "source")?;
    }
    let transaction = connection.transaction().map_err(|e| {
        QhardError::Input(format!("cannot start snapshot scope registry rewrite: {e}"))
    })?;
    let mut changed = 0usize;
    for (rowid, kio_path, root_path) in &rows {
        let root_relative = registry_scope_relative(source_root, root_path, kio_path, "source")?;
        changed += transaction
            .execute(
                "UPDATE scopes SET kio_path = ?1, root_path = ?2 WHERE rowid = ?3",
                rusqlite::params![
                    snapshot.join(&root_relative).join(".kio").to_string_lossy(),
                    snapshot.join(&root_relative).to_string_lossy(),
                    rowid,
                ],
            )
            .map_err(|e| {
                QhardError::Input(format!("cannot rewrite snapshot scope registry: {e}"))
            })?;
    }
    if changed != rows.len() {
        return Err(QhardError::Input(
            "snapshot scope registry rewrite did not update every scope".into(),
        ));
    }
    transaction.commit().map_err(|e| {
        QhardError::Input(format!(
            "cannot commit snapshot scope registry rewrite: {e}"
        ))
    })?;
    let rewritten = connection
        .prepare("SELECT kio_path, root_path FROM scopes")
        .and_then(|mut statement| {
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(|e| QhardError::Input(format!("cannot verify snapshot scope registry: {e}")))?;
    if rewritten.len() != rows.len() {
        return Err(QhardError::Input(
            "snapshot scope registry changed during rewrite".into(),
        ));
    }
    for (kio_path, root_path) in rewritten {
        registry_scope_relative(snapshot, &root_path, &kio_path, "snapshot")?;
    }
    Ok(())
}

/// Return a lexically-normal scope root relative to `bound_root`, rejecting
/// registry paths that could point a private evaluator subprocess outside its
/// snapshot. The registry's Kio directory must be precisely `root/.kio`.
fn registry_scope_relative(
    bound_root: &Path,
    root_path: &str,
    kio_path: &str,
    label: &str,
) -> Result<PathBuf, QhardError> {
    let root_path = Path::new(root_path);
    let relative = root_path.strip_prefix(bound_root).map_err(|_| {
        QhardError::Input(format!(
            "{label} scope registry root_path escapes bound root"
        ))
    })?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(QhardError::Input(format!(
            "{label} scope registry root_path is not a normal descendant"
        )));
    }
    let expected_root = bound_root.join(relative);
    let expected_root = expected_root.to_str().ok_or_else(|| {
        QhardError::Input(format!(
            "{label} bound root cannot be represented in the scope registry"
        ))
    })?;
    // `Path::components` intentionally normalizes repeated separators and
    // interior `.` segments, so compare the original registry strings too.
    // This makes the accepted records canonical lexical descendants, rather
    // than merely paths that happen to resolve below the bound root.
    if root_path != Path::new(expected_root) {
        return Err(QhardError::Input(format!(
            "{label} scope registry root_path is not a canonical descendant"
        )));
    }
    let expected_kio = Path::new(expected_root).join(".kio");
    let expected_kio = expected_kio.to_str().ok_or_else(|| {
        QhardError::Input(format!(
            "{label} expected scope registry kio_path is not UTF-8"
        ))
    })?;
    if kio_path != expected_kio {
        return Err(QhardError::Input(format!(
            "{label} scope registry kio_path does not match root_path/.kio"
        )));
    }
    Ok(relative.to_path_buf())
}

/// Validate that private snapshot task journals retain the exact bytes copied
/// from the attested fixture. Normalized task output references are portable,
/// so relocating a fixture must never rewrite its journal. Parsing the copied
/// store also rejects a snapshot whose journal is corrupt or no longer valid
/// for its destination `.kio` directory.
fn validate_snapshot_task_journals(
    source_root: &Path,
    snapshot_root: &Path,
    source_scopes: impl IntoIterator<Item = PathBuf>,
) -> Result<(), QhardError> {
    let mut relatives = BTreeSet::new();
    for scope in source_scopes {
        let relative = scope
            .strip_prefix(source_root)
            .map_err(|_| QhardError::Input("fixture scope escaped its bound source root".into()))?;
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(QhardError::Input(
                "fixture scope has a non-normal relative path".into(),
            ));
        }
        relatives.insert(relative.to_path_buf());
    }
    for relative in relatives {
        let source_kio = source_root.join(&relative).join(".kio");
        let snapshot_kio = snapshot_root.join(&relative).join(".kio");
        let source_journal = source_kio.join("tasks.jsonl");
        let snapshot_journal = snapshot_kio.join("tasks.jsonl");
        let source_bytes = read_snapshot_task_journal(&source_journal)?;
        let snapshot_bytes = read_snapshot_task_journal(&snapshot_journal)?;
        if source_bytes != snapshot_bytes {
            return Err(QhardError::Input(format!(
                "private fixture task journal was not copied byte-identically for {}",
                relative.display()
            )));
        }
        TaskStore::new(&snapshot_kio).all().map_err(|error| {
            QhardError::Input(format!(
                "cannot validate private fixture task journal for {}: {error}",
                relative.display()
            ))
        })?;
    }
    Ok(())
}

fn read_snapshot_task_journal(path: &Path) -> Result<Option<Vec<u8>>, QhardError> {
    let parent = path
        .parent()
        .filter(|parent| parent.is_absolute())
        .ok_or_else(|| {
            QhardError::Input(format!(
                "private fixture task journal parent is not absolute: {}",
                path.display()
            ))
        })?;
    let directory = StoreDirectory::open(parent).map_err(|error| {
        QhardError::Input(format!(
            "cannot retain private fixture task journal parent {}: {error}",
            parent.display()
        ))
    })?;
    directory
        .read_optional(Path::new("tasks.jsonl"), MAX_TASK_STORE_BYTES)
        .map_err(|error| {
            QhardError::Input(format!(
                "cannot read private fixture task journal {}: {error}",
                path.display()
            ))
        })
}

fn snapshot_fixture(fixture: &Fixture) -> Result<FixtureSnapshot, QhardError> {
    let temp = tempfile::Builder::new()
        .prefix("kio-qhard-fixture-")
        .tempdir()
        .map_err(|e| QhardError::Input(format!("cannot create private fixture snapshot: {e}")))?;
    let root_path = temp.path().join("fixture");
    copy_fixture_directory(
        &fixture.root.handle,
        &root_path,
        &mut FixtureDigestBudget::default(),
    )?;
    let copied_root = RetainedDirectory::open(&root_path, "private fixture snapshot")?;
    if fixture_live_digest(&copied_root)? != fixture.live_sha256 {
        return Err(QhardError::Input(
            "fixture changed while creating private snapshot".into(),
        ));
    }
    let snapshot_root = copied_root.public_path.clone();
    validate_snapshot_task_journals(
        &fixture.root.public_path,
        &snapshot_root,
        fixture
            .scope_relatives
            .iter()
            .map(|relative| fixture.root.public_path.join(relative)),
    )?;
    rewrite_snapshot_registry(&snapshot_root, &fixture.root.public_path, &fixture.env_name)?;
    let root = copied_root;
    let scopes = fixture
        .scope_relatives
        .iter()
        .map(|relative| {
            RetainedDirectory::open(&snapshot_root.join(relative), "private fixture scope")
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(FixtureSnapshot {
        _temp: temp,
        root,
        scopes,
    })
}

/// Snapshot the baseline indexed fixture because `--all-scopes` resolves the
/// registry's absolute paths after its initial descriptor-bound CWD.  The
/// pristine baseline corpus cannot take this route: Spotlight must query the
/// physical indexed location, so comparators instead retain their CWD handle.
fn snapshot_baseline_fixture(
    source: &RetainedDirectory,
    expected_digest: &str,
) -> Result<(tempfile::TempDir, RetainedDirectory), QhardError> {
    let digest = fixture_live_digest(source)?;
    if digest != expected_digest {
        return Err(QhardError::Input(
            "indexed fixture differs from baseline attestation before snapshot".into(),
        ));
    }
    let temp = tempfile::Builder::new()
        .prefix("kio-baseline-fixture-")
        .tempdir()
        .map_err(|e| QhardError::Input(format!("cannot create baseline snapshot: {e}")))?;
    let path = temp.path().join("fixture");
    copy_fixture_directory(&source.handle, &path, &mut FixtureDigestBudget::default())?;
    let root = RetainedDirectory::open(&path, "private baseline fixture")?;
    if fixture_live_digest(&root)? != expected_digest {
        return Err(QhardError::Input(
            "indexed fixture changed while snapshotting".into(),
        ));
    }
    let snapshot_root = root.public_path.clone();
    let mut source_scopes = Vec::new();
    for persona in baseline_personas() {
        source_scopes.extend(
            discover_scopes(source.child(&persona, "indexed baseline persona")?)?
                .into_iter()
                .map(|scope| scope.public_path),
        );
    }
    validate_snapshot_task_journals(&source.public_path, &snapshot_root, source_scopes)?;
    for persona in baseline_personas() {
        rewrite_snapshot_registry(&snapshot_root, &source.public_path, &persona)?;
    }
    Ok((temp, root))
}

/// Snapshot a synthetic corpus before running its normal evaluator. This is
/// intentionally shared with Q_hard rather than trusting `BoundCorpus` alone:
/// Kio's child search resolves descendant `.kio` and XDG paths by name.
pub fn snapshot_regular_tree(source: &Path) -> Result<PrivateTreeSnapshot, QhardError> {
    let source = RetainedDirectory::open(source, "synthetic corpus")?;
    let source_digest = fixture_live_digest(&source)?;
    let temp = tempfile::Builder::new()
        .prefix("kio-eval-synthetic-")
        .tempdir()
        .map_err(|e| QhardError::Input(format!("cannot create private synthetic snapshot: {e}")))?;
    let path = temp.path().join("corpus");
    copy_fixture_directory(&source.handle, &path, &mut FixtureDigestBudget::default())?;
    let copied = RetainedDirectory::open(&path, "private synthetic snapshot")?;
    if fixture_live_digest(&copied)? != source_digest {
        return Err(QhardError::Input(
            "synthetic corpus changed while creating private snapshot".into(),
        ));
    }
    Ok(PrivateTreeSnapshot {
        _temp: temp,
        path,
        source,
        source_digest,
    })
}

#[derive(Default)]
struct FixtureDigestBudget {
    directories: usize,
    files: usize,
    bytes: u64,
}

fn fingerprint_directory(
    directory: &fs::File,
    prefix: &Path,
    records: &mut Vec<String>,
    budget: &mut FixtureDigestBudget,
) -> Result<(), QhardError> {
    budget.directories += 1;
    if budget.directories > MAX_WALK_DIRECTORIES {
        return Err(QhardError::Input(
            "fixture attestation exceeds directory traversal bound".into(),
        ));
    }
    let mut entries = Vec::new();
    for entry in cap_fs::read_dir(directory, Path::new("."))
        .map_err(|e| QhardError::Input(format!("cannot enumerate live fixture: {e}")))?
    {
        entries.push(
            entry.map_err(|e| QhardError::Input(format!("cannot enumerate live fixture: {e}")))?,
        );
        if entries.len() > MAX_DIRECTORY_ENTRIES {
            return Err(QhardError::Input(
                "fixture directory exceeds entry-count bound".into(),
            ));
        }
    }
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| QhardError::Input("fixture contains non-UTF-8 entry".into()))?;
        let relative = prefix.join(name);
        let ty = entry
            .file_type()
            .map_err(|e| QhardError::Input(format!("cannot inspect live fixture: {e}")))?;
        if ty.is_symlink() {
            return Err(QhardError::Input(format!(
                "fixture contains symlink: {}",
                relative.display()
            )));
        }
        if ty.is_dir() {
            records.push(format!("D:{}", relative.display()));
            let child = cap_fs::open_dir_nofollow(directory, Path::new(name)).map_err(|_| {
                QhardError::Input(format!(
                    "fixture directory changed while opening: {}",
                    relative.display()
                ))
            })?;
            fingerprint_directory(&child, &relative, records, budget)?;
        } else if ty.is_file() {
            budget.files += 1;
            if budget.files > MAX_LIVE_FIXTURE_FILES {
                return Err(QhardError::Input(
                    "fixture attestation exceeds file-count bound".into(),
                ));
            }
            let listed = cap_fs::stat(directory, Path::new(name), cap_fs::FollowSymlinks::No)
                .map_err(|e| QhardError::Input(format!("cannot inspect live fixture file: {e}")))?;
            let remaining = MAX_LIVE_FIXTURE_BYTES.saturating_sub(budget.bytes);
            if listed.len() > MAX_LIVE_FIXTURE_FILE_BYTES || listed.len() > remaining {
                return Err(QhardError::Input(
                    "fixture attestation exceeds aggregate byte bound".into(),
                ));
            }
            let bytes = regular_at(
                directory,
                Path::new(name),
                MAX_LIVE_FIXTURE_FILE_BYTES,
                "live fixture file",
            )?;
            budget.bytes = budget
                .bytes
                .checked_add(bytes.len() as u64)
                .ok_or_else(|| {
                    QhardError::Input("fixture attestation byte counter overflow".into())
                })?;
            if budget.bytes > MAX_LIVE_FIXTURE_BYTES {
                return Err(QhardError::Input(
                    "fixture attestation exceeds aggregate byte bound".into(),
                ));
            }
            records.push(format!("F:{}:{}", relative.display(), hash_bytes(&bytes)));
        } else {
            return Err(QhardError::Input(format!(
                "fixture contains unsupported entry: {}",
                relative.display()
            )));
        }
    }
    Ok(())
}

fn load_golden(path: &Path) -> Result<(Vec<GoldenRow>, FileBinding), QhardError> {
    let (bytes, binding) = binding(path, MAX_GOLDEN_BYTES, "Q_hard golden")?;
    if binding.sha256 != FROZEN_GOLDEN_SHA256 {
        return Err(QhardError::Input(
            "Q_hard golden digest does not match the frozen 8-query contract".into(),
        ));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| QhardError::Input("Q_hard golden is not UTF-8".into()))?;
    let rows = text
        .lines()
        .map(|line| {
            serde_json::from_str::<GoldenRow>(line)
                .map_err(|e| QhardError::Input(format!("invalid Q_hard golden row: {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let ids = [
        "qa01", "qa02", "qa03", "qa04", "qa05", "qa06", "qa07", "qa08",
    ];
    let classes = [
        "hard1", "hard1", "hard1", "hard1", "hard3", "hard3", "hard3", "hard3",
    ];
    if rows.len() != ids.len() {
        return Err(QhardError::Input(
            "Q_hard golden must contain exactly 8 rows".into(),
        ));
    }
    for (index, row) in rows.iter().enumerate() {
        if row.query_id != ids[index]
            || row.class != classes[index]
            || row.scenario != "M3-1"
            || row.query.is_empty()
            || row.expected.len() != 1
        {
            return Err(QhardError::Input(
                "Q_hard golden IDs/order/classes/scenarios differ from frozen contract".into(),
            ));
        }
        let expected = &row.expected[0];
        if expected.path.is_empty()
            || !matches!(
                expected.format.as_str(),
                "pdf_rasterized" | "pptx" | "png" | "jpeg"
            )
            || !matches!(expected.unit_prefix.as_str(), "page" | "slide" | "image")
            || expected.section_hint.is_empty()
        {
            return Err(QhardError::Input(
                "Q_hard expected record differs from frozen shape".into(),
            ));
        }
    }
    Ok((rows, binding))
}

fn load_fixture(options: &QhardOptions) -> Result<Fixture, QhardError> {
    safe_name(&options.tree, "--tree")?;
    safe_name(&options.env_name, "--env-name")?;
    let root = RetainedDirectory::open(&options.fixture_root, "--fixture-root")?;
    let tree = root.child(&options.tree, "fixture tree")?;
    let env_root = root.child("env", "fixture env directory")?;
    let _device = env_root.child(&options.env_name, "fixture environment")?;
    let (bytes, attestation_binding) = match &options.attestation {
        Some(path) => binding(path, MAX_ATTESTATION_BYTES, "Q_hard attestation")?,
        None => binding_at(
            &root,
            "qhard-attestation.json",
            MAX_ATTESTATION_BYTES,
            "Q_hard attestation",
        )?,
    };
    let attestation: Attestation = serde_json::from_slice(&bytes)
        .map_err(|e| QhardError::Input(format!("invalid Q_hard attestation: {e}")))?;
    if attestation.schema_version != 1
        || attestation.fixture_id != "kio-qhard-v1"
        || attestation.tree != options.tree
        || attestation.env_name != options.env_name
        || attestation.golden_sha256 != FROZEN_GOLDEN_SHA256
        || attestation.scopes.is_empty()
        || attestation.scopes.len() > MAX_SCOPES
    {
        return Err(QhardError::Input(
            "Q_hard attestation does not bind this frozen fixture".into(),
        ));
    }
    let mut expected = BTreeSet::new();
    for relative in &attestation.scopes {
        expected.insert(safe_relative(relative)?);
    }
    if expected.len() != attestation.scopes.len() {
        return Err(QhardError::Input(
            "Q_hard attestation has duplicate scopes".into(),
        ));
    }
    let scopes = discover_scopes(tree)?;
    let actual = scopes
        .iter()
        .map(|scope| {
            scope
                .public_path
                .strip_prefix(&root.public_path)
                .map(Path::to_path_buf)
                .map_err(|_| QhardError::Input("fixture scope escaped root".into()))
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    if actual != expected {
        return Err(QhardError::Input(
            "registered fixture scopes differ from attestation".into(),
        ));
    }
    let relatives = actual
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    let live_sha256 = fixture_live_digest(&root)?;
    if attestation.fixture_content_sha256 != fixture_content_digest(&root)? {
        return Err(QhardError::Input(
            "Q_hard attestation fixture_content_sha256 does not match fixture content".into(),
        ));
    }
    Ok(Fixture {
        root,
        tree: options.tree.clone(),
        env_name: options.env_name.clone(),
        scope_relatives: relatives,
        attestation: attestation_binding,
        live_sha256,
    })
}

fn qhard_env(
    root: &RetainedDirectory,
    env_name: &str,
    online: bool,
) -> Result<(ControlledEnvironment, Vec<&'static str>), QhardError> {
    let env_root = root.child("env", "fixture env directory")?;
    let base = env_root.child(env_name, "fixture environment")?;
    let mut fixed = vec![
        (
            OsString::from("PATH"),
            env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin")),
        ),
        (OsString::from("LANG"), OsString::from("C.UTF-8")),
        (OsString::from("LC_ALL"), OsString::from("C.UTF-8")),
        (OsString::from("TZ"), OsString::from("UTC")),
    ];
    let mut directories = Vec::new();
    for (key, directory) in [
        ("XDG_CONFIG_HOME", "xdg-config"),
        ("XDG_DATA_HOME", "xdg-data"),
        ("XDG_CACHE_HOME", "xdg-cache"),
    ] {
        let path = base.child(directory, "fixture XDG directory")?;
        directories.push((OsString::from(key), path));
    }
    let home = base.child("home", "fixture home directory")?;
    directories.push((OsString::from("HOME"), home));
    let mut forwarded = Vec::new();
    if online {
        for name in ONLINE_QUERY_CREDENTIAL_NAMES {
            if let Some(value) = env::var_os(name) {
                fixed.push((OsString::from(name), value));
                forwarded.push(name);
            }
        }
    }
    Ok((ControlledEnvironment { fixed, directories }, forwarded))
}

/// Return the deterministic, name-only credential set that an online-query
/// lane is eligible to forward.  Callers still obtain the value separately
/// and record only entries that were actually put in the subprocess
/// environment.
fn available_online_query_credential_names(
    online_query: bool,
    mut is_available: impl FnMut(&'static str) -> bool,
) -> Vec<&'static str> {
    if !online_query {
        return Vec::new();
    }
    ONLINE_QUERY_CREDENTIAL_NAMES
        .into_iter()
        .filter(|name| is_available(name))
        .collect()
}

fn result_paths(stdout: &str, k: usize) -> Result<Vec<String>, QhardError> {
    let value: Value = serde_json::from_str(stdout.trim())
        .map_err(|e| QhardError::Input(format!("Q_hard search emitted invalid JSON: {e}")))?;
    let results = value
        .as_object()
        .and_then(|o| o.get("results"))
        .and_then(Value::as_array)
        .ok_or_else(|| QhardError::Input("Q_hard search response has no results array".into()))?;
    if results.len() > k {
        return Err(QhardError::Input(
            "Q_hard search returned more than requested k".into(),
        ));
    }
    results
        .iter()
        .enumerate()
        .map(|(i, result)| {
            result
                .as_object()
                .and_then(|o| o.get("evidence_pointer"))
                .and_then(Value::as_object)
                .and_then(|pointer| {
                    Some((
                        pointer.get("scope_id")?.as_str()?,
                        pointer.get("path_at_commit")?.as_str()?,
                    ))
                })
                .filter(|(scope, path)| !scope.is_empty() && !path.is_empty())
                .map(|(scope, path)| format!("{scope}:{path}"))
                .ok_or_else(|| QhardError::Input(format!("Q_hard result[{i}] has no pointer path")))
        })
        .collect()
}

fn expected_pointer_keys(
    expected: &[Expected],
    fixture: &Fixture,
    snapshot: &FixtureSnapshot,
) -> Result<Vec<String>, QhardError> {
    expected
        .iter()
        .map(|value| {
            // Golden paths name their fixture pack (for example `qhard-a`),
            // while the registered tree name is configurable (`qhard` by
            // default). Match the remaining scope suffix structurally rather
            // than treating those independent roots as interchangeable.
            let mut expected_components = Path::new(&value.path).components();
            if !matches!(expected_components.next(), Some(Component::Normal(_))) {
                return Err(QhardError::Input(
                    "Q_hard expected path has no fixture-pack component".into(),
                ));
            }
            let relative = expected_components.as_path().to_string_lossy().into_owned();
            let candidates = fixture
                .scope_relatives
                .iter()
                .enumerate()
                .filter_map(|(i, scope)| {
                    let scope = scope.strip_prefix(&format!("{}/", fixture.tree))?;
                    relative
                        .strip_prefix(&format!("{scope}/"))
                        .map(|path| (i, (scope, path)))
                })
                .collect::<Vec<_>>();
            if candidates.len() != 1 {
                return Err(QhardError::Input(
                    "Q_hard expected path must resolve to exactly one attested scope file".into(),
                ));
            }
            let (index, scope) = candidates.into_iter().next().expect("checked exactly one");
            let kio = snapshot.scopes[index].child(".kio", "private fixture .kio")?;
            let bytes = regular_at(
                &kio.handle,
                Path::new("scope.json"),
                MAX_GOLDEN_BYTES,
                "private fixture scope identity",
            )?;
            let scope_id = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|v| {
                    v.get("scope_id")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                })
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    QhardError::Input("private fixture scope.json has no scope_id".into())
                })?;
            Ok(format!("{scope_id}:{}", scope.1))
        })
        .collect()
}

pub fn run(options: QhardOptions) -> Result<QhardReport, QhardError> {
    if options.k != RESULT_K {
        return Err(QhardError::Input("Q_hard only permits --k 10".into()));
    }
    let (golden, golden_binding) = load_golden(&options.golden)?;
    let fixture = load_fixture(&options)?;
    let bin = options.bin.clone();
    let metadata = fs::symlink_metadata(&bin)
        .map_err(|e| QhardError::Input(format!("cannot inspect --bin: {e}")))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(QhardError::Input(
            "--bin must be a regular executable file".into(),
        ));
    }
    let (_binary_snapshot_dir, bound_bin, binary) = bound_executable(&bin)?;
    let snapshot = snapshot_fixture(&fixture)?;
    let (environment, forwarded) =
        qhard_env(&snapshot.root, &fixture.env_name, options.online_query)?;
    let cwd = snapshot.scopes.first().expect("validated nonempty scopes");
    let mut rows = Vec::with_capacity(golden.len());
    for query in &golden {
        let mut command = Command::new(&bound_bin);
        command.args([
            "--json",
            "search",
            &query.query,
            "--all-scopes",
            "--limit",
            "10",
        ]);
        environment.apply(&mut command)?;
        cwd.configure_command_cwd(&mut command)?;
        let output = run_bounded_command(&mut command, BoundedProcessOptions::default(), None)?;
        environment.recheck_private_directories()?;
        let titles = if output.status.success() {
            result_paths(&output.stdout, options.k)?
        } else {
            Vec::new()
        };
        let expected_paths = expected_pointer_keys(&query.expected, &fixture, &snapshot)?;
        let hit = titles
            .iter()
            .any(|returned| expected_paths.iter().any(|expected| expected == returned));
        rows.push(Row {
            query_id: query.query_id.clone(),
            class: query.class.clone(),
            hit,
            expected_paths,
            returned_paths: titles,
            returncode: output.status.code().unwrap_or(-1),
            duration_ms: output.duration.as_secs_f64() * 1_000.0,
        });
    }
    let (_, attestation_after) = match &options.attestation {
        Some(path) => binding(path, MAX_ATTESTATION_BYTES, "Q_hard attestation")?,
        None => binding_at(
            &fixture.root,
            "qhard-attestation.json",
            MAX_ATTESTATION_BYTES,
            "Q_hard attestation",
        )?,
    };
    let (_, golden_after) = binding(&options.golden, MAX_GOLDEN_BYTES, "Q_hard golden")?;
    let (_, binary_after) = binding(&bin, MAX_BINARY_BYTES, "kio binary")?;
    let live_after = fixture_live_digest(&fixture.root)?;
    if attestation_after.sha256 != fixture.attestation.sha256
        || golden_after.sha256 != golden_binding.sha256
        || binary_after.sha256 != binary.sha256
        || live_after != fixture.live_sha256
    {
        return Err(QhardError::Input(
            "Q_hard input artifact changed during measurement".into(),
        ));
    }
    let hits = rows.iter().filter(|row| row.hit).count();
    Ok(QhardReport {
        schema_version: 1,
        benchmark: "kio-qhard-search",
        measurement_class: "attested_external_fixture",
        acceptance_eligible: false,
        blocked_reason: Some("Q_hard-only lane cannot claim combined M3-1 acceptance".into()),
        fixture: FixtureBinding {
            root: fixture.root.public_path.display().to_string(),
            tree: fixture.tree,
            env_name: fixture.env_name,
            attestation: fixture.attestation,
            live_sha256: fixture.live_sha256,
            scopes: fixture.scope_relatives,
        },
        binary,
        golden: golden_binding,
        configuration: Configuration {
            k: options.k,
            online_query: options.online_query,
            forwarded_credential_names: forwarded,
        },
        rows,
        hits,
        total: golden.len(),
        recall_at_10: hits as f64 / golden.len() as f64,
        synthetic_m3_1: None,
        combined_hits: None,
        combined_total: None,
    })
}

/// Serialize a report without allowing a fixture-owned path to become an
/// evaluator output sink. Reports are evidence *about* the fixture, never
/// part of it; writing inside it would make a later attestation ambiguous.
pub fn write_report(
    path: &Path,
    fixture_root: &Path,
    report: &QhardReport,
) -> Result<(), QhardError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .map_err(|e| QhardError::Input(format!("cannot resolve report path: {e}")))?
            .join(path)
    };
    let root = RetainedDirectory::open(fixture_root, "fixture root")?.public_path;
    if absolute.starts_with(&root) {
        return Err(QhardError::Input(
            "Q_hard report must be outside fixture root".into(),
        ));
    }
    let parent_path = absolute
        .parent()
        .ok_or_else(|| QhardError::Input("Q_hard report has no parent directory".into()))?;
    let name = absolute
        .file_name()
        .ok_or_else(|| QhardError::Input("Q_hard report has no filename".into()))?;
    let parent = RetainedDirectory::open(parent_path, "Q_hard report parent")?;
    if parent.public_path.starts_with(&root) {
        return Err(QhardError::Input(
            "Q_hard report must be outside fixture root".into(),
        ));
    }
    let bytes = serde_json::to_vec_pretty(report)?;
    publish_report(
        &parent.handle,
        &parent.sync_metadata,
        &parent.public_path,
        Path::new(name),
        &bytes,
    )?;
    Ok(())
}

fn publish_report(
    parent: &fs::File,
    parent_identity: &fs::Metadata,
    parent_path: &Path,
    name: &Path,
    bytes: &[u8],
) -> Result<(), QhardError> {
    match cap_fs::stat(parent, name, cap_fs::FollowSymlinks::No) {
        Ok(metadata) if !metadata.file_type().is_file() || metadata.file_type().is_symlink() => {
            return Err(QhardError::Input(
                "Q_hard report target must be a regular non-symlink file".into(),
            ));
        }
        Ok(_) | Err(_) => {}
    }
    let mut created = None;
    let mut file = None;
    for _ in 0..16 {
        let temp = format!(
            ".kio-qhard-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let mut options = cap_fs::OpenOptions::new();
        options.write(true).create_new(true);
        match cap_fs::open(parent, Path::new(&temp), &options) {
            Ok(opened) => {
                created = Some(temp);
                file = Some(opened);
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(QhardError::Input(format!(
                    "cannot create Q_hard report: {e}"
                )));
            }
        }
    }
    let temp = created
        .ok_or_else(|| QhardError::Input("cannot reserve Q_hard report temporary file".into()))?;
    let mut file = file.expect("reserved Q_hard temporary has a handle");
    use io::Write;
    if let Err(e) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        let _ = cap_fs::remove_file(parent, Path::new(&temp));
        return Err(QhardError::Input(format!(
            "cannot write Q_hard report: {e}"
        )));
    }
    if let Err(e) = cap_fs::rename(parent, Path::new(&temp), parent, name) {
        let _ = cap_fs::remove_file(parent, Path::new(&temp));
        return Err(QhardError::Input(format!(
            "cannot atomically install Q_hard report: {e}"
        )));
    }
    sync_retained_directory(parent, parent_identity, parent_path)
        .map_err(|e| QhardError::Input(format!("cannot sync Q_hard report directory: {e}")))?;
    Ok(())
}

// Baseline comparison -------------------------------------------------------
//
// This intentionally lives beside Q_hard: the capability-bound tree walk and
// atomic publisher are security properties of a measurement, not Q_hard-only
// conveniences.  Fixture-B is nevertheless a distinct frozen population.

#[derive(Debug, Clone)]
pub struct BaselineOptions {
    pub golden: PathBuf,
    pub fixture_root: PathBuf,
    pub baseline_corpus: PathBuf,
    pub attestation: Option<PathBuf>,
    pub bin: PathBuf,
    pub mdfind: PathBuf,
    pub comparator_runtime: Option<PathBuf>,
    pub online_query: bool,
}

#[derive(Debug, Clone)]
pub struct BaselineAttestOptions {
    pub golden: PathBuf,
    pub fixture_root: PathBuf,
    pub baseline_corpus: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BaselineAttestation {
    schema_version: u64,
    fixture_id: String,
    golden_sha256: String,
    indexed_fixture_sha256: String,
    pristine_corpus_sha256: String,
    source_equivalence_sha256: String,
    personas: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BaselineGoldenRow {
    query_id: String,
    class: String,
    query: String,
    persona: String,
    expected: Vec<BaselineExpected>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BaselineExpected {
    path: String,
    file: String,
}

#[derive(Debug, Serialize)]
pub struct BaselineReport {
    schema_version: u64,
    benchmark: &'static str,
    platform: String,
    measured_at_unix_ms: u128,
    status: &'static str,
    blocked_reason: Option<String>,
    golden: FileBinding,
    attestation: FileBinding,
    indexed_fixture_sha256: String,
    pristine_corpus_sha256: String,
    source_equivalence_sha256: String,
    configuration: BaselineConfiguration,
    tools: BTreeMap<String, BaselineTool>,
    rows: Vec<BaselineRow>,
    recall_at_10: BTreeMap<String, f64>,
    deltas: BTreeMap<String, f64>,
    gate: BaselineGate,
}
#[derive(Debug, Serialize)]
struct BaselineConfiguration {
    /// Whether the Kio lane was allowed to use online-query credentials.
    online_query: bool,
    /// Environment variable names actually forwarded to the Kio lane. Values
    /// are deliberately never retained in the report or evaluator state.
    forwarded_credential_names: Vec<&'static str>,
}
impl BaselineReport {
    #[must_use]
    pub fn acceptance_passed(&self) -> bool {
        self.gate.pass
    }
}
#[derive(Debug, Serialize)]
struct BaselineTool {
    executable_path: String,
    sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    companion_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    helpers: Option<BTreeMap<String, FileBinding>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    configuration: Option<FileBinding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    comparator_runtime: Option<ComparatorRuntimeProvenance>,
    version: String,
    available: bool,
}
#[derive(Debug, Serialize)]
struct BaselineRow {
    query_id: String,
    class: String,
    persona: String,
    expected_files: Vec<String>,
    kio: BaselineResult,
    mdfind: BaselineResult,
    rga: BaselineResult,
}
#[derive(Debug, Serialize)]
struct BaselineResult {
    returned_items: Vec<String>,
    hit: bool,
    returncode: i32,
    duration_ms: f64,
}
#[derive(Debug, Serialize)]
struct BaselineGate {
    kio_ge_0_8: bool,
    margin_mdfind_ge_0_3: bool,
    margin_rga_ge_0_3: bool,
    pass: bool,
}

fn baseline_personas() -> Vec<String> {
    (1..=20).map(|n| format!("p{n:02}")).collect()
}

fn load_baseline_golden(path: &Path) -> Result<(Vec<BaselineGoldenRow>, FileBinding), QhardError> {
    let (bytes, binding) = binding(path, MAX_GOLDEN_BYTES, "baseline golden")?;
    if binding.sha256 != FROZEN_BASELINE_GOLDEN_SHA256 {
        return Err(QhardError::Input(
            "baseline golden digest does not match frozen fixture-B contract".into(),
        ));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| QhardError::Input("baseline golden is not UTF-8".into()))?;
    let rows = text
        .lines()
        .map(|line| {
            serde_json::from_str::<BaselineGoldenRow>(line)
                .map_err(|e| QhardError::Input(format!("invalid baseline golden row: {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let people = baseline_personas();
    if rows.len() != 24
        || rows
            .iter()
            .map(|r| &r.persona)
            .collect::<BTreeSet<_>>()
            .len()
            != 20
    {
        return Err(QhardError::Input(
            "baseline golden must contain the frozen 24 rows across p01..p20".into(),
        ));
    }
    for row in &rows {
        if !people.contains(&row.persona)
            || !matches!(row.class.as_str(), "hard1" | "hard2" | "hard3")
            || row.query.is_empty()
            || row.expected.len() != 1
            || row.expected[0].path.is_empty()
            || row.expected[0].file.is_empty()
        {
            return Err(QhardError::Input(
                "baseline golden differs from frozen shape".into(),
            ));
        }
    }
    Ok((rows, binding))
}

fn digest_persona_tree(root: &RetainedDirectory, skip_kio: bool) -> Result<String, QhardError> {
    fn walk(
        dir: &fs::File,
        prefix: &Path,
        skip_kio: bool,
        records: &mut Vec<String>,
        budget: &mut FixtureDigestBudget,
    ) -> Result<(), QhardError> {
        budget.directories += 1;
        if budget.directories > MAX_WALK_DIRECTORIES {
            return Err(QhardError::Input(
                "baseline tree exceeds traversal bound".into(),
            ));
        }
        let mut entries = cap_fs::read_dir(dir, Path::new("."))
            .map_err(|e| QhardError::Input(format!("cannot enumerate baseline tree: {e}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| QhardError::Input(format!("cannot enumerate baseline tree: {e}")))?;
        if entries.len() > MAX_DIRECTORY_ENTRIES {
            return Err(QhardError::Input(
                "baseline directory exceeds entry-count bound".into(),
            ));
        }
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name();
            let name_s = name
                .to_str()
                .ok_or_else(|| QhardError::Input("baseline tree contains non-UTF-8 name".into()))?;
            if skip_kio && name_s == ".kio" {
                continue;
            }
            let rel = prefix.join(name_s);
            let ty = entry
                .file_type()
                .map_err(|e| QhardError::Input(e.to_string()))?;
            if ty.is_symlink() {
                return Err(QhardError::Input(format!(
                    "baseline tree contains symlink: {}",
                    rel.display()
                )));
            }
            if ty.is_dir() {
                records.push(format!("D:{}", rel.display()));
                let child = cap_fs::open_dir_nofollow(dir, Path::new(name_s)).map_err(|_| {
                    QhardError::Input("baseline directory changed while binding".into())
                })?;
                walk(&child, &rel, skip_kio, records, budget)?;
            } else if ty.is_file() {
                budget.files += 1;
                if budget.files > MAX_LIVE_FIXTURE_FILES {
                    return Err(QhardError::Input(
                        "baseline tree exceeds file-count bound".into(),
                    ));
                }
                let bytes = regular_at(
                    dir,
                    Path::new(name_s),
                    MAX_LIVE_FIXTURE_FILE_BYTES,
                    "baseline tree file",
                )?;
                budget.bytes += bytes.len() as u64;
                if budget.bytes > MAX_LIVE_FIXTURE_BYTES {
                    return Err(QhardError::Input("baseline tree exceeds byte bound".into()));
                }
                records.push(format!("F:{}:{}", rel.display(), hash_bytes(&bytes)));
            } else {
                return Err(QhardError::Input(
                    "baseline tree contains unsupported entry".into(),
                ));
            }
        }
        Ok(())
    }
    let mut records = Vec::new();
    walk(
        &root.handle,
        Path::new(""),
        skip_kio,
        &mut records,
        &mut FixtureDigestBudget::default(),
    )?;
    Ok(hash_bytes(records.join("\n").as_bytes()))
}

fn baseline_bindings(
    options: &BaselineAttestOptions,
) -> Result<
    (
        FileBinding,
        RetainedDirectory,
        RetainedDirectory,
        BaselineAttestation,
    ),
    QhardError,
> {
    let (_, golden) = load_baseline_golden(&options.golden)?;
    let indexed = RetainedDirectory::open(&options.fixture_root, "--fixture-root")?;
    let pristine = RetainedDirectory::open(&options.baseline_corpus, "--baseline-corpus")?;
    let people = baseline_personas();
    let mut equivalent = Vec::new();
    for persona in &people {
        let left = indexed.child(persona, "indexed persona")?;
        let right = pristine.child(persona, "pristine persona")?;
        let l = digest_persona_tree(&left, true)?;
        let r = digest_persona_tree(&right, true)?;
        if l != r {
            return Err(QhardError::Input(format!(
                "indexed and pristine content differ for {persona}"
            )));
        }
        equivalent.push(format!("{persona}:{l}"));
    }
    let att = BaselineAttestation {
        schema_version: 1,
        fixture_id: "kio-baseline-fixture-b-v1".into(),
        golden_sha256: golden.sha256.clone(),
        indexed_fixture_sha256: fixture_live_digest(&indexed)?,
        pristine_corpus_sha256: fixture_live_digest(&pristine)?,
        source_equivalence_sha256: hash_bytes(equivalent.join("\n").as_bytes()),
        personas: people,
    };
    Ok((golden, indexed, pristine, att))
}

pub fn generate_baseline_attestation(
    options: BaselineAttestOptions,
) -> Result<Vec<u8>, QhardError> {
    let (_, _, _, att) = baseline_bindings(&options)?;
    serde_json::to_vec_pretty(&att).map_err(QhardError::Serialize)
}

fn resolve_tool(path: &Path) -> Option<PathBuf> {
    if path.components().count() > 1 || path.is_absolute() {
        return path.is_file().then(|| path.to_path_buf());
    }
    env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .map(PathBuf::from)
        .map(|d| d.join(path))
        .find(|p| p.is_file())
}
#[cfg(test)]
fn executable_tool(
    path: &Path,
    label: &str,
) -> Result<(tempfile::TempDir, PathBuf, FileBinding), QhardError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|e| QhardError::Input(format!("cannot inspect {label}: {e}")))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(QhardError::Input(format!(
            "{label} must be a regular non-symlink executable"
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(QhardError::Input(format!("{label} is not executable")));
        }
    }
    let bound = bound_executable(path)?;
    let valid = match label {
        "rga" => run_tool(&bound.1, &["--version"]).is_ok_and(|output| {
            output.status.success() && output.stdout.to_ascii_lowercase().contains("ripgrep-all")
        }),
        "mdfind" => {
            path == Path::new("/usr/bin/mdfind")
                && run_tool(&bound.1, &["-h"]).is_ok_and(|output| {
                    mdfind_capability_output(output.status.code(), &output.stdout)
                })
        }
        _ => true,
    };
    if !valid {
        return Err(QhardError::Input(format!(
            "{label} failed semantic capability preflight"
        )));
    }
    Ok(bound)
}

/// Validate the stable `mdfind -h` capability shape without treating a
/// leading blank line from the system tool as an identity mismatch.
fn mdfind_capability_output(exit_code: Option<i32>, stdout: &str) -> bool {
    exit_code == Some(5)
        // macOS `mdfind -h` currently prefixes its usage block with a blank
        // line. Accept only leading whitespace, not an arbitrary banner, so
        // this remains a capability/identity check.
        && stdout.trim_start().starts_with("Usage:")
        && stdout.contains("-onlyin")
}

fn trusted_mdfind(path: &Path) -> Result<FileBinding, QhardError> {
    if path != Path::new("/usr/bin/mdfind") {
        return Err(QhardError::Input(
            "mdfind must be the sealed macOS /usr/bin/mdfind".into(),
        ));
    }
    require_sealed_absolute_path(path, "mdfind")?;
    let metadata = fs::symlink_metadata(path).map_err(|e| QhardError::Input(e.to_string()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(QhardError::Input(
            "mdfind must be a regular system executable".into(),
        ));
    }
    let binding = tool_binding(path, "mdfind")?;
    let output = run_tool(path, &["-h"])?;
    if !mdfind_capability_output(output.status.code(), &output.stdout) {
        return Err(QhardError::Input(
            "mdfind failed system capability preflight".into(),
        ));
    }
    Ok(binding)
}

#[derive(Debug, Clone)]
struct MachoInspection {
    loads: Vec<String>,
    rpaths: Vec<String>,
    dylinker: Option<String>,
    has_dyld_environment: bool,
}

#[derive(Debug, Clone)]
struct PendingMachoImage {
    path: PathBuf,
    executable: PathBuf,
    inherited_rpaths: Vec<ResolvedMachoRpath>,
}

#[derive(Debug, Clone)]
enum ResolvedMachoDependency {
    Runtime(PathBuf),
    SealedSystem(PathBuf),
}

/// Evaluate candidate paths in dyld's priority order.  The probe is invoked
/// for every candidate, not only the winner, so a malformed or unsealed
/// lower-priority existing image cannot be hidden behind an earlier match.
/// `None` is reserved for a genuinely absent candidate; all other inspection
/// failures are propagated rather than falling through.
fn first_existing_macho_rpath_candidate<T, R, Probe>(
    candidates: impl IntoIterator<Item = T>,
    mut probe: Probe,
) -> Result<Option<R>, QhardError>
where
    Probe: FnMut(&T) -> Result<Option<R>, QhardError>,
{
    let mut first = None;
    for candidate in candidates {
        if let Some(resolved) = probe(&candidate)?
            && first.is_none()
        {
            first = Some(resolved);
        }
    }
    Ok(first)
}

fn checked_macho_path(value: &str, label: &str) -> Result<(), QhardError> {
    if value.is_empty()
        || value.len() > MAX_MACHO_PATH_BYTES
        || value.contains('\0')
        || value.contains('\r')
        || value.contains('\n')
    {
        return Err(QhardError::Input(format!(
            "{label} is not a bounded Mach-O path"
        )));
    }
    Ok(())
}

fn is_dylib_load_command(command: &str) -> bool {
    matches!(
        command,
        "LC_LOAD_DYLIB"
            | "LC_LOAD_WEAK_DYLIB"
            | "LC_REEXPORT_DYLIB"
            | "LC_LOAD_UPWARD_DYLIB"
            | "LC_LAZY_LOAD_DYLIB"
    )
}

/// Parse the actual `LC_LOAD_*_DYLIB` commands rather than `otool -L`'s
/// presentation output.  A dylib's `otool -L` output starts with its own
/// `LC_ID_DYLIB`, which is not an image that dyld loads as a dependency.
fn parse_otool_load_commands(
    output: &str,
) -> Result<(Vec<String>, Option<String>, bool), QhardError> {
    let mut loads = Vec::new();
    let mut pending_load_name = false;
    let mut pending_dylinker_name = false;
    let mut dylinker = None;
    let mut has_dyld_environment = false;
    for line in output.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("Load command ") {
            if pending_load_name || pending_dylinker_name {
                return Err(QhardError::Input(
                    "otool -l loader command lacks an install name".into(),
                ));
            }
            continue;
        }
        if let Some(command) = trimmed.strip_prefix("cmd ") {
            if pending_load_name || pending_dylinker_name {
                return Err(QhardError::Input(
                    "otool -l loader command lacks an install name".into(),
                ));
            }
            pending_load_name = is_dylib_load_command(command);
            pending_dylinker_name = command == "LC_LOAD_DYLINKER";
            has_dyld_environment |= command == "LC_DYLD_ENVIRONMENT";
            continue;
        }
        if (pending_load_name || pending_dylinker_name) && trimmed.starts_with("name ") {
            let path = trimmed
                .strip_prefix("name ")
                .and_then(|value| value.split_once(" (offset ").map(|(path, _)| path))
                .ok_or_else(|| QhardError::Input("otool -l dependency name is malformed".into()))?;
            checked_macho_path(path, "otool -l loader name")?;
            if pending_load_name {
                loads.push(path.to_owned());
                if loads.len() > MAX_MACHO_LOAD_COMMANDS {
                    return Err(QhardError::Input("Mach-O has too many dependencies".into()));
                }
            } else if dylinker.replace(path.to_owned()).is_some() {
                return Err(QhardError::Input(
                    "Mach-O has more than one dynamic loader command".into(),
                ));
            }
            pending_load_name = false;
            pending_dylinker_name = false;
        }
    }
    if pending_load_name || pending_dylinker_name {
        return Err(QhardError::Input(
            "otool -l loader command lacks an install name".into(),
        ));
    }
    Ok((loads, dylinker, has_dyld_environment))
}

fn parse_otool_rpaths(output: &str) -> Result<Vec<String>, QhardError> {
    let mut rpaths = Vec::new();
    let mut awaiting_path = false;
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed == "cmd LC_RPATH" {
            if awaiting_path {
                return Err(QhardError::Input("otool -l rpath is malformed".into()));
            }
            awaiting_path = true;
            continue;
        }
        if awaiting_path && trimmed.starts_with("Load command ") {
            return Err(QhardError::Input("otool -l rpath lacks a path".into()));
        }
        if awaiting_path && trimmed.starts_with("path ") {
            let value = trimmed
                .strip_prefix("path ")
                .and_then(|value| value.split_once(" (offset ").map(|(path, _)| path))
                .ok_or_else(|| QhardError::Input("otool -l rpath line is malformed".into()))?;
            checked_macho_path(value, "otool -l rpath")?;
            rpaths.push(value.to_owned());
            if rpaths.len() > MAX_MACHO_LOAD_COMMANDS {
                return Err(QhardError::Input("Mach-O has too many rpaths".into()));
            }
            awaiting_path = false;
        }
    }
    if awaiting_path {
        return Err(QhardError::Input("otool -l rpath lacks a path".into()));
    }
    Ok(rpaths)
}

fn safe_macho_join(
    base: &Path,
    suffix: &str,
    root: &Path,
    label: &str,
) -> Result<PathBuf, QhardError> {
    checked_macho_path(suffix, label)?;
    let mut output = base.to_path_buf();
    if !output.starts_with(root) {
        return Err(QhardError::Input(format!(
            "{label} base escapes comparator runtime"
        )));
    }
    for component in Path::new(suffix).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => output.push(part),
            Component::ParentDir => {
                if output == root {
                    return Err(QhardError::Input(format!(
                        "{label} escapes comparator runtime"
                    )));
                }
                if !output.pop() || !output.starts_with(root) {
                    return Err(QhardError::Input(format!(
                        "{label} escapes comparator runtime"
                    )));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(QhardError::Input(format!(
                    "{label} is not a relative Mach-O suffix"
                )));
            }
        }
    }
    Ok(output)
}

fn canonical_path_within(
    root: &Path,
    candidate: &Path,
    label: &str,
) -> Result<PathBuf, QhardError> {
    let canonical = fs::canonicalize(candidate)
        .map_err(|e| QhardError::Input(format!("cannot resolve {label}: {e}")))?;
    if !canonical.starts_with(root) {
        return Err(QhardError::Input(format!(
            "{label} resolves outside the comparator runtime root"
        )));
    }
    Ok(canonical)
}

#[cfg(target_os = "macos")]
mod macos_acl {
    use super::*;

    const ACL_TYPE_EXTENDED: c_int = 0x0000_0100;
    const ACL_FIRST_ENTRY: c_int = 0;

    unsafe extern "C" {
        fn acl_get_file(path: *const c_char, acl_type: c_int) -> *mut c_void;
        fn acl_get_link_np(path: *const c_char, acl_type: c_int) -> *mut c_void;
        fn acl_get_entry(acl: *mut c_void, entry_id: c_int, entry: *mut *mut c_void) -> c_int;
        fn acl_free(object: *mut c_void) -> c_int;
    }

    /// Return whether an object has an extended ACL.  The Darwin ACL API
    /// reports a missing extended ACL as `NULL` + `ENOENT`, even when the
    /// filesystem object itself exists; every other error is unsafe to
    /// interpret as sealed and is therefore propagated.
    pub(super) fn has_extended_acl(path: &Path, nofollow: bool) -> Result<bool, QhardError> {
        use std::os::unix::ffi::OsStrExt;

        let encoded = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            QhardError::Input("comparator runtime path contains an interior NUL".into())
        })?;
        // SAFETY: `encoded` is NUL-terminated for this FFI call. The returned
        // ACL allocation, when non-null, is released exactly once below. The
        // no-follow variant is required for administrator-owned symlink
        // aliases: following it would miss an ACL that permits replacing the
        // link itself.
        let acl = unsafe {
            if nofollow {
                acl_get_link_np(encoded.as_ptr(), ACL_TYPE_EXTENDED)
            } else {
                acl_get_file(encoded.as_ptr(), ACL_TYPE_EXTENDED)
            }
        };
        if acl.is_null() {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOENT) {
                return Ok(false);
            }
            return Err(QhardError::Input(format!(
                "cannot inspect comparator runtime ACL: {error}"
            )));
        }
        let mut entry = std::ptr::null_mut();
        // SAFETY: `acl` is a valid allocation returned by `acl_get_file`, and
        // `entry` provides the output storage required by the API.
        let result = unsafe { acl_get_entry(acl, ACL_FIRST_ENTRY, &mut entry) };
        // SAFETY: `acl` was returned by `acl_get_file` and has not yet been
        // released. The return code cannot make this cleanup unsafe.
        let _ = unsafe { acl_free(acl) };
        match result {
            0 => Ok(true),
            _ => Err(QhardError::Input(format!(
                "cannot enumerate comparator runtime ACL: {}",
                io::Error::last_os_error()
            ))),
        }
    }
}

/// macOS 26 may attach `com.apple.provenance` to otherwise immutable files.
/// It is descriptive metadata rather than an authorization control, so it is
/// the only extended attribute accepted for the administrator runtime.  The
/// names, not their values, are the policy surface: values are intentionally
/// never read or parsed.
#[cfg(target_os = "macos")]
mod macos_xattr {
    use super::*;

    const XATTR_NOFOLLOW: c_int = 0x0001;

    unsafe extern "C" {
        fn listxattr(
            path: *const c_char,
            namebuf: *mut c_char,
            size: usize,
            options: c_int,
        ) -> libc::ssize_t;
        fn removexattr(path: *const c_char, name: *const c_char, options: c_int) -> c_int;
    }

    pub(super) fn list(path: &Path, nofollow: bool) -> Result<BTreeSet<String>, QhardError> {
        let encoded = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            QhardError::Input("comparator runtime path contains an interior NUL".into())
        })?;
        let options = if nofollow { XATTR_NOFOLLOW } else { 0 };
        // SAFETY: `encoded` is NUL-terminated, and a null buffer with size
        // zero is the documented sizing form of `listxattr`.
        let size = unsafe { listxattr(encoded.as_ptr(), std::ptr::null_mut(), 0, options) };
        if size < 0 {
            return Err(QhardError::Input(format!(
                "cannot enumerate comparator runtime xattrs: {}",
                io::Error::last_os_error()
            )));
        }
        let size = usize::try_from(size)
            .map_err(|_| QhardError::Input("comparator runtime xattr size overflows".into()))?;
        if size > MAX_RUNTIME_XATTR_LIST_BYTES {
            return Err(QhardError::Input(
                "comparator runtime xattr list exceeds the bounded limit".into(),
            ));
        }
        if size == 0 {
            return Ok(BTreeSet::new());
        }
        let mut names = vec![0_u8; size];
        // SAFETY: `names` has exactly the size returned by `listxattr`; the
        // no-follow option makes symlink inspection refer to the link itself.
        let listed = unsafe {
            listxattr(
                encoded.as_ptr(),
                names.as_mut_ptr().cast::<c_char>(),
                names.len(),
                options,
            )
        };
        if listed < 0 {
            return Err(QhardError::Input(format!(
                "cannot enumerate comparator runtime xattrs: {}",
                io::Error::last_os_error()
            )));
        }
        let listed = usize::try_from(listed)
            .map_err(|_| QhardError::Input("comparator runtime xattr size overflows".into()))?;
        if listed != names.len() {
            return Err(QhardError::Input(
                "comparator runtime xattr list changed while enumerating".into(),
            ));
        }
        if names.last() != Some(&0) {
            return Err(QhardError::Input(
                "comparator runtime xattr list is not NUL-terminated".into(),
            ));
        }
        let mut result = BTreeSet::new();
        for raw in names[..names.len() - 1].split(|byte| *byte == 0) {
            if raw.is_empty() {
                return Err(QhardError::Input(
                    "comparator runtime xattr list contains an empty name".into(),
                ));
            }
            let name = std::str::from_utf8(raw).map_err(|_| {
                QhardError::Input("comparator runtime xattr name is not UTF-8".into())
            })?;
            if !result.insert(name.to_owned()) {
                return Err(QhardError::Input(
                    "comparator runtime xattr list contains a duplicate name".into(),
                ));
            }
        }
        Ok(result)
    }

    pub(super) fn require_allowed(path: &Path, nofollow: bool) -> Result<(), QhardError> {
        let names = list(path, nofollow)?;
        if runtime_xattr_names_allowed(&names) {
            return Ok(());
        }
        Err(QhardError::Input(format!(
            "comparator runtime has forbidden extended attributes: {}",
            names.into_iter().collect::<Vec<_>>().join(", ")
        )))
    }

    pub(super) fn remove_named(path: &Path, name: &str) -> Result<(), QhardError> {
        use std::os::unix::ffi::OsStrExt;
        let encoded = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            QhardError::Input("comparator runtime path contains an interior NUL".into())
        })?;
        let name = CString::new(name).map_err(|_| {
            QhardError::Input("comparator runtime xattr name contains an interior NUL".into())
        })?;
        // SAFETY: both strings are NUL terminated; options zero intentionally
        // applies only to a regular DMG container, never a symlink.
        if unsafe { removexattr(encoded.as_ptr(), name.as_ptr(), 0) } != 0 {
            return Err(QhardError::Input(format!(
                "cannot remove comparator runtime xattr: {}",
                io::Error::last_os_error()
            )));
        }
        Ok(())
    }
}

#[cfg(any(target_os = "macos", test))]
fn runtime_xattr_names_allowed(names: &BTreeSet<String>) -> bool {
    names.is_empty() || names == &BTreeSet::from(["com.apple.provenance".to_owned()])
}

#[cfg(target_os = "macos")]
fn require_allowed_runtime_xattrs(path: &Path, label: &str) -> Result<(), QhardError> {
    macos_xattr::require_allowed(path, false)
        .map_err(|error| QhardError::Input(format!("{label} xattr validation failed: {error}")))
}

#[cfg(target_os = "macos")]
fn require_allowed_runtime_xattrs_link(path: &Path, label: &str) -> Result<(), QhardError> {
    macos_xattr::require_allowed(path, true).map_err(|error| {
        QhardError::Input(format!("{label} symlink xattr validation failed: {error}"))
    })
}

#[cfg(not(target_os = "macos"))]
fn require_allowed_runtime_xattrs(_path: &Path, label: &str) -> Result<(), QhardError> {
    Err(QhardError::Input(format!(
        "{label} requires a sealed macOS comparator runtime"
    )))
}

#[cfg(not(target_os = "macos"))]
fn require_allowed_runtime_xattrs_link(_path: &Path, label: &str) -> Result<(), QhardError> {
    Err(QhardError::Input(format!(
        "{label} requires a sealed macOS comparator runtime"
    )))
}

/// Inspect the runtime root and every component used to reach it.  This is
/// separate from `require_sealed_absolute_path` because sealed macOS system
/// libraries are trusted under their own platform policy; the explicit xattr
/// allowlist belongs only to the administrator-provided runtime.
fn require_allowed_runtime_path_xattrs(path: &Path, label: &str) -> Result<(), QhardError> {
    if !path.is_absolute() {
        return Err(QhardError::Input(format!("{label} must be absolute")));
    }
    let mut current = PathBuf::from("/");
    require_allowed_runtime_xattrs(&current, label)?;
    for component in path.components() {
        if let Component::Normal(name) = component {
            current.push(name);
            let metadata = fs::symlink_metadata(&current).map_err(|error| {
                QhardError::Input(format!("cannot inspect {label} path component: {error}"))
            })?;
            if metadata.file_type().is_symlink() {
                require_allowed_runtime_xattrs_link(&current, label)?;
            } else {
                require_allowed_runtime_xattrs(&current, label)?;
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn require_no_extended_acl(path: &Path, label: &str) -> Result<(), QhardError> {
    if macos_acl::has_extended_acl(path, false)? {
        return Err(QhardError::Input(format!(
            "{label} has an extended ACL; comparator runtime paths must not grant ACL-based access"
        )));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn require_no_extended_acl_link(path: &Path, label: &str) -> Result<(), QhardError> {
    if macos_acl::has_extended_acl(path, true)? {
        return Err(QhardError::Input(format!(
            "{label} symlink has an extended ACL; comparator runtime aliases must not grant ACL-based access"
        )));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn require_no_extended_acl(_path: &Path, label: &str) -> Result<(), QhardError> {
    Err(QhardError::Input(format!(
        "{label} requires a sealed macOS comparator runtime"
    )))
}

#[cfg(not(target_os = "macos"))]
fn require_no_extended_acl_link(_path: &Path, label: &str) -> Result<(), QhardError> {
    Err(QhardError::Input(format!(
        "{label} requires a sealed macOS comparator runtime"
    )))
}

#[cfg(target_os = "macos")]
fn sealed_runtime_metadata(metadata: &fs::Metadata) -> bool {
    !metadata.file_type().is_symlink()
        && metadata.uid() == 0
        && metadata.permissions().mode() & 0o022 == 0
}

#[cfg(not(target_os = "macos"))]
fn sealed_runtime_metadata(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(target_os = "macos")]
fn require_sealed_absolute_path(path: &Path, label: &str) -> Result<(), QhardError> {
    if !path.is_absolute() {
        return Err(QhardError::Input(format!("{label} must be absolute")));
    }
    let mut current = PathBuf::from("/");
    let root_metadata = fs::symlink_metadata(&current)
        .map_err(|e| QhardError::Input(format!("cannot inspect {label} root: {e}")))?;
    if !sealed_runtime_metadata(&root_metadata) {
        return Err(QhardError::Input(format!(
            "{label} requires a root-owned, non-writable path"
        )));
    }
    require_no_extended_acl(&current, label)?;
    for component in path.components() {
        if let Component::Normal(name) = component {
            current.push(name);
            let metadata = fs::symlink_metadata(&current).map_err(|e| {
                QhardError::Input(format!("cannot inspect {label} path component: {e}"))
            })?;
            if !sealed_runtime_metadata(&metadata) {
                return Err(QhardError::Input(format!(
                    "{label} requires root-owned, non-writable path components"
                )));
            }
            require_no_extended_acl(&current, label)?;
        }
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn require_sealed_absolute_path(_path: &Path, label: &str) -> Result<(), QhardError> {
    Err(QhardError::Input(format!(
        "{label} requires a sealed macOS comparator runtime"
    )))
}

#[cfg(target_os = "macos")]
fn sealed_system_root(path: &Path) -> Option<&'static Path> {
    [Path::new("/usr/lib"), Path::new("/System/Library")]
        .into_iter()
        .find(|root| path.starts_with(root))
}

#[cfg(not(target_os = "macos"))]
fn sealed_system_root(_path: &Path) -> Option<&'static Path> {
    None
}

fn validate_sealed_system_library(path: &Path, label: &str) -> Result<PathBuf, QhardError> {
    let canonical = fs::canonicalize(path)
        .map_err(|e| QhardError::Input(format!("cannot canonicalize {label}: {e}")))?;
    if sealed_system_root(&canonical).is_none() {
        return Err(QhardError::Input(format!(
            "{label} is outside the explicitly allowed macOS sealed-system roots"
        )));
    }
    require_sealed_absolute_path(&canonical, label)?;
    let metadata = fs::symlink_metadata(&canonical)
        .map_err(|e| QhardError::Input(format!("cannot inspect {label}: {e}")))?;
    if !metadata.is_file() {
        return Err(QhardError::Input(format!("{label} must be a regular file")));
    }
    Ok(canonical)
}

fn resolve_runtime_path(
    root: &Path,
    requested: &Path,
    label: &str,
    require_file: bool,
) -> Result<PathBuf, QhardError> {
    if !requested.starts_with(root) {
        return Err(QhardError::Input(format!(
            "{label} is outside the comparator runtime root"
        )));
    }
    let relative = requested.strip_prefix(root).map_err(|_| {
        QhardError::Input(format!("{label} is outside the comparator runtime root"))
    })?;
    let mut raw = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(QhardError::Input(format!(
                "{label} is not a normalized comparator runtime path"
            )));
        };
        raw.push(name);
        let metadata = fs::symlink_metadata(&raw)
            .map_err(|e| QhardError::Input(format!("cannot inspect {label}: {e}")))?;
        if metadata.file_type().is_symlink() {
            require_no_extended_acl_link(&raw, label)?;
            require_allowed_runtime_xattrs_link(&raw, label)?;
            // Symlinks are permitted only as administrator-owned aliases that
            // resolve back into this exact runtime root. The resolved target
            // is then checked below as a sealed canonical path.
            #[cfg(unix)]
            if metadata.uid() != 0 {
                return Err(QhardError::Input(format!(
                    "{label} symlink is not administrator-owned"
                )));
            }
            let target = fs::canonicalize(&raw)
                .map_err(|e| QhardError::Input(format!("cannot resolve {label} symlink: {e}")))?;
            if !target.starts_with(root) {
                return Err(QhardError::Input(format!(
                    "{label} symlink resolves outside the comparator runtime root"
                )));
            }
        } else if !sealed_runtime_metadata(&metadata) {
            return Err(QhardError::Input(format!(
                "{label} has a non-sealed comparator runtime component"
            )));
        } else {
            require_no_extended_acl(&raw, label)?;
            require_allowed_runtime_xattrs(&raw, label)?;
        }
    }
    let canonical = canonical_path_within(root, requested, label)?;
    // The requested spelling may traverse an allowed runtime-internal
    // symlink. Revalidate the canonical target as well, so xattrs on the
    // target's own path components and final closure file cannot hide behind
    // the alias.
    require_allowed_runtime_path_xattrs(&canonical, label)?;
    require_sealed_absolute_path(&canonical, label)?;
    let metadata = fs::symlink_metadata(&canonical)
        .map_err(|e| QhardError::Input(format!("cannot inspect {label}: {e}")))?;
    if (require_file && !metadata.is_file()) || (!require_file && !metadata.is_dir()) {
        return Err(QhardError::Input(format!(
            "{label} has the wrong filesystem type"
        )));
    }
    Ok(canonical)
}

fn macho_architecture() -> Result<&'static str, QhardError> {
    match env::consts::ARCH {
        "aarch64" => Ok("arm64"),
        "x86_64" => Ok("x86_64"),
        other => Err(QhardError::Input(format!(
            "unsupported Mach-O evaluator architecture: {other}"
        ))),
    }
}

fn trusted_otool() -> Result<FileBinding, QhardError> {
    let path = Path::new("/usr/bin/otool");
    require_sealed_absolute_path(path, "otool")?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|e| QhardError::Input(format!("cannot inspect otool: {e}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(QhardError::Input(
            "otool must be the sealed macOS system executable".into(),
        ));
    }
    tool_binding(path, "otool")
}

/// `dyld_info` is a CLT shim on current macOS releases.  Bind the final
/// executable, not the shim, so a replacement indirection cannot alter the
/// cache catalog between comparator subprocesses.
fn trusted_dyld_info() -> Result<(PathBuf, FileBinding), QhardError> {
    let selector = Path::new("/usr/bin/xcode-select");
    require_sealed_absolute_path(selector, "xcode-select")?;
    let metadata = fs::symlink_metadata(selector)
        .map_err(|e| QhardError::Input(format!("cannot inspect xcode-select: {e}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(QhardError::Input(
            "xcode-select must be a sealed system executable".into(),
        ));
    }
    let output = run_sealed_system_tool(selector, &["-p"], MAX_MACHO_INSPECT_OUTPUT_BYTES)?;
    if !output.status.success() || output.stderr.len() > MAX_MACHO_INSPECT_OUTPUT_BYTES {
        return Err(QhardError::Input(
            "cannot determine active Apple developer directory".into(),
        ));
    }
    let developer = output.stdout.trim();
    if developer.is_empty() || developer.len() > MAX_MACHO_PATH_BYTES {
        return Err(QhardError::Input(
            "active Apple developer directory is malformed".into(),
        ));
    }
    let path = fs::canonicalize(Path::new(developer).join("usr/bin/dyld_info"))
        .map_err(|e| QhardError::Input(format!("cannot resolve dyld_info executable: {e}")))?;
    // A full Xcode bundle is mutable by the interactive developer account on
    // common installations. The evaluator intentionally supports only the
    // administrator-managed CommandLineTools location.
    if !path.starts_with("/Library/Developer/CommandLineTools/usr/bin/") {
        return Err(QhardError::Input(
            "dyld_info must resolve beneath the sealed CommandLineTools root".into(),
        ));
    }
    require_sealed_absolute_path(&path, "dyld_info executable")?;
    let metadata = fs::symlink_metadata(&path)
        .map_err(|e| QhardError::Input(format!("cannot inspect dyld_info: {e}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(QhardError::Input(
            "dyld_info must resolve to a regular executable".into(),
        ));
    }
    require_apple_code_signature(&path, "dyld_info")?;
    Ok((path.clone(), tool_binding(&path, "dyld_info")?))
}

fn run_sealed_system_tool(
    path: &Path,
    args: &[&str],
    maximum: usize,
) -> Result<crate::runner::BoundedProcessOutput, QhardError> {
    let mut command = Command::new(path);
    command
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("TZ", "UTC");
    Ok(run_bounded_command(
        &mut command,
        BoundedProcessOptions {
            timeout: DEFAULT_PROCESS_TIMEOUT,
            max_stdout_bytes: maximum,
            max_stderr_bytes: maximum,
        },
        None,
    )?)
}

fn require_apple_code_signature(path: &Path, label: &str) -> Result<(), QhardError> {
    let codesign = Path::new("/usr/bin/codesign");
    require_sealed_absolute_path(codesign, "codesign")?;
    let rendered = path
        .to_str()
        .ok_or_else(|| QhardError::Input(format!("{label} path is not UTF-8")))?;
    let output = run_sealed_system_tool(
        codesign,
        &[
            "-v",
            "--strict",
            "-R=identifier \"com.apple.dyld_info\" and anchor apple",
            rendered,
        ],
        MAX_MACHO_INSPECT_OUTPUT_BYTES,
    )?;
    if !output.status.success() {
        return Err(QhardError::Input(format!("{label} is not signed by Apple")));
    }
    Ok(())
}

fn shared_cache_path_allowed(path: &str) -> bool {
    Path::new(path).is_absolute()
        && (path.starts_with("/usr/lib/") || path.starts_with("/System/Library/"))
        && !path.contains("//")
        && !Path::new(path)
            .components()
            .any(|component| matches!(component, Component::ParentDir))
}

fn cache_architecture() -> Result<&'static str, QhardError> {
    match macho_architecture()? {
        // Apple Silicon caches may expose arm64e images even to an arm64
        // process.  Probe the stronger ABI first, then use arm64 only if it
        // is absent from the same bounded catalog.
        "arm64" => Ok("arm64e"),
        other => Ok(other),
    }
}

fn dyld_cache_platform_tuple() -> Result<String, QhardError> {
    let sysctl = Path::new("/usr/sbin/sysctl");
    require_sealed_absolute_path(sysctl, "sysctl")?;
    let metadata = fs::symlink_metadata(sysctl)
        .map_err(|e| QhardError::Input(format!("cannot inspect sysctl: {e}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(QhardError::Input(
            "sysctl must be a sealed system executable".into(),
        ));
    }
    let product = run_sealed_system_tool(sysctl, &["-n", "kern.osproductversion"], 1024)?;
    let build = run_sealed_system_tool(sysctl, &["-n", "kern.osversion"], 1024)?;
    let kernel = run_sealed_system_tool(sysctl, &["-n", "kern.osrelease"], 1024)?;
    if !product.status.success() || !build.status.success() || !kernel.status.success() {
        return Err(QhardError::Input(
            "cannot determine macOS shared-cache platform".into(),
        ));
    }
    let product = product.stdout.trim();
    let build = build.stdout.trim();
    let kernel = kernel.stdout.trim();
    if product.is_empty()
        || build.is_empty()
        || kernel.is_empty()
        || product.len() > 128
        || build.len() > 128
        || kernel.len() > 128
    {
        return Err(QhardError::Input(
            "macOS shared-cache platform tuple is malformed".into(),
        ));
    }
    Ok(format!(
        "macos:{product}:{build}:{kernel}:{}",
        env::consts::ARCH
    ))
}

fn parse_dyld_cache_catalog_optional(
    output: &str,
    architecture: &str,
    platform: &str,
    inspector: FileBinding,
) -> Result<Option<DyldSharedCacheCatalog>, QhardError> {
    if output.len() > MAX_DYLD_INFO_OUTPUT_BYTES || architecture.is_empty() || platform.is_empty() {
        return Err(QhardError::Input(
            "dyld shared-cache catalog is unbounded or malformed".into(),
        ));
    }
    let mut images = BTreeMap::new();
    let mut current: Option<DyldCatalogRecord> = None;
    let mut edges = 0_usize;
    let finish = |current: &mut Option<DyldCatalogRecord>,
                  images: &mut BTreeMap<String, DyldSharedCacheImage>|
     -> Result<(), QhardError> {
        let Some(record) = current.take() else {
            return Ok(());
        };
        match record {
            DyldCatalogRecord::Ignored => Ok(()),
            DyldCatalogRecord::Edges {
                path,
                uuid,
                mut edges,
            } => {
                edges.sort();
                edges.dedup();
                if images
                    .insert(
                        path,
                        DyldSharedCacheImage {
                            uuid,
                            linked_dylibs: edges,
                        },
                    )
                    .is_some()
                {
                    return Err(QhardError::Input(
                        "dyld shared-cache catalog has duplicate image".into(),
                    ));
                }
                if images.len() > MAX_DYLD_CACHE_IMAGES {
                    return Err(QhardError::Input(
                        "dyld shared-cache has too many images".into(),
                    ));
                }
                Ok(())
            }
            _ => Err(QhardError::Input(
                "dyld shared-cache record is truncated".into(),
            )),
        }
    };
    for line in output.lines() {
        if let Some((path, arch)) = line.rsplit_once(" [")
            && let Some(arch) = arch.strip_suffix("]:")
        {
            finish(&mut current, &mut images)?;
            if arch.len() > 32 || !arch.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Err(QhardError::Input(
                    "dyld shared-cache image header is malformed".into(),
                ));
            }
            if arch != architecture || path.starts_with("/System/iOSSupport/") {
                current = Some(DyldCatalogRecord::Ignored);
                continue;
            }
            if !shared_cache_path_allowed(path) {
                return Err(QhardError::Input(
                    "dyld shared-cache image header escapes macOS sealed roots".into(),
                ));
            }
            current = Some(DyldCatalogRecord::NeedUuid {
                path: path.to_owned(),
            });
            continue;
        }
        let Some(record) = current.as_mut() else {
            if line.trim().is_empty() {
                continue;
            }
            return Err(QhardError::Input(
                "dyld shared-cache catalog has content before an image header".into(),
            ));
        };
        if matches!(record, DyldCatalogRecord::Ignored) {
            continue;
        }
        let trimmed = line.trim();
        match record {
            DyldCatalogRecord::NeedUuidValue { .. } => {
                let DyldCatalogRecord::NeedUuidValue { path } =
                    std::mem::replace(record, DyldCatalogRecord::Ignored)
                else {
                    unreachable!()
                };
                if trimmed.len() != 36
                    || !trimmed.bytes().enumerate().all(|(i, b)| {
                        matches!(i, 8 | 13 | 18 | 23) && b == b'-'
                            || !matches!(i, 8 | 13 | 18 | 23) && b.is_ascii_hexdigit()
                    })
                {
                    return Err(QhardError::Input(
                        "dyld shared-cache image UUID is malformed".into(),
                    ));
                }
                *record = DyldCatalogRecord::NeedLinkedDylibs {
                    path,
                    uuid: trimmed.to_ascii_uppercase(),
                };
                continue;
            }
            DyldCatalogRecord::NeedUuid { .. } if trimmed == "-uuid:" => {
                let DyldCatalogRecord::NeedUuid { path } =
                    std::mem::replace(record, DyldCatalogRecord::Ignored)
                else {
                    unreachable!()
                };
                *record = DyldCatalogRecord::NeedUuidValue { path };
                continue;
            }
            DyldCatalogRecord::NeedUuid { .. } => {
                return Err(QhardError::Input(
                    "dyld shared-cache record lacks -uuid marker".into(),
                ));
            }
            DyldCatalogRecord::NeedLinkedDylibs { .. } if trimmed == "-linked_dylibs:" => {
                let DyldCatalogRecord::NeedLinkedDylibs { path, uuid } =
                    std::mem::replace(record, DyldCatalogRecord::Ignored)
                else {
                    unreachable!()
                };
                *record = DyldCatalogRecord::NeedAttributes { path, uuid };
                continue;
            }
            DyldCatalogRecord::NeedLinkedDylibs { .. } => {
                return Err(QhardError::Input(
                    "dyld shared-cache record lacks -linked_dylibs marker".into(),
                ));
            }
            DyldCatalogRecord::NeedAttributes { .. } if trimmed == "attributes     load path" => {
                let DyldCatalogRecord::NeedAttributes { path, uuid } =
                    std::mem::replace(record, DyldCatalogRecord::Ignored)
                else {
                    unreachable!()
                };
                *record = DyldCatalogRecord::Edges {
                    path,
                    uuid,
                    edges: Vec::new(),
                };
                continue;
            }
            DyldCatalogRecord::NeedAttributes { .. } => {
                return Err(QhardError::Input(
                    "dyld shared-cache record lacks attributes/load header".into(),
                ));
            }
            DyldCatalogRecord::Edges { edges: linked, .. } => {
                if trimmed.is_empty() {
                    continue;
                }
                let Some(path) = trimmed
                    .split_whitespace()
                    .last()
                    .filter(|path| path.starts_with('/'))
                else {
                    return Err(QhardError::Input(
                        "dyld shared-cache edge is malformed".into(),
                    ));
                };
                if !shared_cache_path_allowed(path) {
                    // The complete cache catalog includes images with edges
                    // into non-macOS support roots. Exclude the whole record;
                    // an exact lookup of that image will then fail closed.
                    *record = DyldCatalogRecord::Ignored;
                    continue;
                }
                let edge = DyldSharedCacheEdge {
                    attributes: trimmed.strip_suffix(path).unwrap_or("").trim().to_owned(),
                    path: path.to_owned(),
                };
                validate_shared_cache_edge_attributes(&edge)?;
                linked.push(edge);
                edges += 1;
                if edges > MAX_DYLD_CACHE_EDGES {
                    return Err(QhardError::Input(
                        "dyld shared-cache has too many dependency edges".into(),
                    ));
                }
                continue;
            }
            DyldCatalogRecord::Ignored => unreachable!(),
        }
    }
    finish(&mut current, &mut images)?;
    if images.is_empty() {
        return Ok(None);
    }
    Ok(Some(DyldSharedCacheCatalog {
        inspector,
        architecture: architecture.to_owned(),
        platform: platform.to_owned(),
        images,
    }))
}

fn parse_dyld_cache_catalog(
    output: &str,
    architecture: &str,
    platform: &str,
    inspector: FileBinding,
) -> Result<DyldSharedCacheCatalog, QhardError> {
    parse_dyld_cache_catalog_optional(output, architecture, platform, inspector)?.ok_or_else(|| {
        QhardError::Input("dyld shared-cache catalog has no matching architecture images".into())
    })
}

fn load_dyld_cache_catalog() -> Result<DyldSharedCacheCatalog, QhardError> {
    let (path, binding) = trusted_dyld_info()?;
    let architecture = cache_architecture()?;
    let platform = dyld_cache_platform_tuple()?;
    let run = |arch: &str| -> Result<String, QhardError> {
        let mut command = Command::new(&path);
        command
            .args(["-arch", arch, "-uuid", "-linked_dylibs", "-all_dyld_cache"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .env("TZ", "UTC");
        let output = run_bounded_command(
            &mut command,
            BoundedProcessOptions {
                timeout: DEFAULT_PROCESS_TIMEOUT,
                max_stdout_bytes: MAX_DYLD_INFO_OUTPUT_BYTES,
                max_stderr_bytes: MAX_DYLD_INFO_OUTPUT_BYTES,
            },
            None,
        )?;
        if !output.status.success() {
            return Err(QhardError::Input(
                "dyld_info shared-cache catalog command failed".into(),
            ));
        }
        Ok(output.stdout)
    };
    let raw = run(architecture)?;
    match parse_dyld_cache_catalog_optional(&raw, architecture, &platform, binding.clone()) {
        Ok(Some(catalog)) => Ok(catalog),
        Ok(None) if architecture == "arm64e" => {
            let arm64 = run("arm64")?;
            parse_dyld_cache_catalog(&arm64, "arm64", &platform, binding)
        }
        Ok(None) => Err(QhardError::Input(
            "dyld shared-cache catalog has no matching architecture images".into(),
        )),
        Err(error) => Err(error),
    }
}

fn inspect_macho(path: &Path) -> Result<MachoInspection, QhardError> {
    let _otool = trusted_otool()?;
    let architecture = macho_architecture()?;
    let render = path
        .to_str()
        .ok_or_else(|| QhardError::Input("Mach-O path is not UTF-8".into()))?;
    let inspect = |flag: &str| -> Result<crate::runner::BoundedProcessOutput, QhardError> {
        let mut command = Command::new("/usr/bin/otool");
        command
            .args(["-arch", architecture, flag, render])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .env("TZ", "UTC");
        Ok(run_bounded_command(
            &mut command,
            BoundedProcessOptions {
                timeout: DEFAULT_PROCESS_TIMEOUT,
                max_stdout_bytes: MAX_MACHO_INSPECT_OUTPUT_BYTES,
                max_stderr_bytes: MAX_MACHO_INSPECT_OUTPUT_BYTES,
            },
            None,
        )?)
    };
    let load_commands = inspect("-l")?;
    if !load_commands.status.success() {
        return Err(QhardError::Input(format!(
            "otool -l failed for {} with exit {}",
            path.display(),
            load_commands.status.code().unwrap_or(-1)
        )));
    }
    let (loads, dylinker, has_dyld_environment) = parse_otool_load_commands(&load_commands.stdout)?;
    Ok(MachoInspection {
        loads,
        rpaths: parse_otool_rpaths(&load_commands.stdout)?,
        dylinker,
        has_dyld_environment,
    })
}
#[cfg(unix)]
fn same_std_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_std_file(_left: &fs::Metadata, _right: &fs::Metadata) -> bool {
    true
}

fn add_runtime_closure_entry(
    files: &mut BTreeMap<PathBuf, RuntimeBoundFile>,
    path: &Path,
    trust: RuntimeTrust,
    runtime_mount: &RuntimeMountIdentity,
    total_bytes: &mut u64,
) -> Result<(), QhardError> {
    if files.contains_key(path) {
        return Ok(());
    }
    if trust == RuntimeTrust::MacosSealedSystem {
        let canonical = validate_sealed_system_library(path, "macOS sealed-system closure entry")?;
        if canonical != path {
            return Err(QhardError::Input(
                "macOS sealed-system closure entry is not in canonical form".into(),
            ));
        }
    } else {
        require_runtime_mount(runtime_mount, path, "comparator runtime closure image")?;
    }
    if files.len() >= MAX_RUNTIME_CLOSURE_ENTRIES {
        return Err(QhardError::Input(
            "comparator runtime exceeds dynamic dependency entry limit".into(),
        ));
    }
    let binding = tool_binding(path, "comparator runtime image")?;
    let bytes = u64::try_from(binding.bytes)
        .map_err(|_| QhardError::Input("comparator runtime image size overflows".into()))?;
    *total_bytes = total_bytes
        .checked_add(bytes)
        .ok_or_else(|| QhardError::Input("comparator runtime byte total overflows".into()))?;
    if *total_bytes > MAX_RUNTIME_CLOSURE_BYTES {
        return Err(QhardError::Input(
            "comparator runtime exceeds dynamic dependency byte limit".into(),
        ));
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|e| QhardError::Input(format!("cannot inspect comparator runtime image: {e}")))?;
    if !metadata.is_file() {
        return Err(QhardError::Input(
            "comparator runtime image must be a regular file".into(),
        ));
    }
    files.insert(
        path.to_path_buf(),
        RuntimeBoundFile {
            trust,
            binding,
            identity: RuntimeFileIdentity::from_metadata(&metadata),
        },
    );
    Ok(())
}

/// A shared-cache edge which is absent from the catalog normally has to be
/// bound to its exact sealed filesystem image.  The sole exception is dyld's
/// explicit weak-link contract: a missing weak target is observable through
/// the catalog edge itself, while a later appearance changes the closure from
/// absent to bound and therefore changes its digest.
fn validate_shared_cache_edge_attributes(edge: &DyldSharedCacheEdge) -> Result<bool, QhardError> {
    if edge.attributes.is_empty() {
        return Ok(false);
    }
    let mut attributes = BTreeSet::new();
    let mut weak_link = false;
    for attribute in edge.attributes.split_whitespace() {
        if !matches!(
            attribute,
            "upward" | "delay-init" | "weak-link" | "re-export"
        ) {
            return Err(QhardError::Input(format!(
                "dyld shared-cache edge has unknown attribute {attribute:?}"
            )));
        }
        if !attributes.insert(attribute) {
            return Err(QhardError::Input(format!(
                "dyld shared-cache edge repeats attribute {attribute:?}"
            )));
        }
        weak_link |= attribute == "weak-link";
    }
    if attributes.is_empty() {
        return Err(QhardError::Input(
            "dyld shared-cache edge attributes are whitespace only".into(),
        ));
    }
    Ok(weak_link)
}

fn classify_shared_cache_physical_edge(
    edge: &DyldSharedCacheEdge,
    lookup: Result<(), io::ErrorKind>,
) -> Result<bool, QhardError> {
    match lookup {
        Ok(()) => {
            validate_shared_cache_edge_attributes(edge)?;
            Ok(true)
        }
        Err(io::ErrorKind::NotFound) if validate_shared_cache_edge_attributes(edge)? => Ok(false),
        Err(io::ErrorKind::NotFound) => Err(QhardError::Input(format!(
            "missing required dyld shared-cache physical dependency: {}",
            edge.path
        ))),
        Err(kind) => Err(QhardError::Input(format!(
            "cannot inspect dyld shared-cache physical dependency {}: {kind}",
            edge.path
        ))),
    }
}

fn add_shared_cache_closure(
    cache: &DyldSharedCacheCatalog,
    requested: &Path,
    entries: &mut BTreeMap<String, DyldSharedCacheBinding>,
    files: &mut BTreeMap<PathBuf, RuntimeBoundFile>,
    runtime_mount: &RuntimeMountIdentity,
    total_bytes: &mut u64,
) -> Result<(), QhardError> {
    let mut queue = vec![requested.display().to_string()];
    while let Some(path) = queue.pop() {
        if entries.contains_key(&path) {
            continue;
        }
        let image = cache.images.get(&path).ok_or_else(|| {
            QhardError::Input(format!(
                "missing sealed-system library is not an exact dyld shared-cache image: {path}"
            ))
        })?;
        if entries.len() >= MAX_DYLD_CACHE_IMAGES {
            return Err(QhardError::Input(
                "comparator runtime exceeds shared-cache entry limit".into(),
            ));
        }
        let mut binding = DyldSharedCacheBinding {
            install_name: path.clone(),
            architecture: cache.architecture.clone(),
            uuid: image.uuid.clone(),
            linked_dylibs: image.linked_dylibs.clone(),
            missing_weak_dylibs: Vec::new(),
            inspector: cache.inspector.clone(),
            platform: cache.platform.clone(),
        };
        for edge in &binding.linked_dylibs {
            // A cache dependency must also be in the chosen architecture's
            // catalog.  Do not silently fall back to a filesystem image.
            if cache.images.contains_key(&edge.path) {
                queue.push(edge.path.clone());
            } else if classify_shared_cache_physical_edge(
                edge,
                // Follow a sealed-system symlink here: a dangling weak-link
                // target is absent to dyld even though the symlink inode is
                // still present. All other resolution failures remain fatal.
                fs::metadata(&edge.path).map(|_| ()).map_err(|e| e.kind()),
            )? {
                let physical = validate_sealed_system_library(
                    Path::new(&edge.path),
                    "dyld shared-cache physical dependency",
                )?;
                // A present weak/delay edge may still be loaded by dyld, so
                // bind its exact sealed filesystem image just like a required
                // edge.
                add_runtime_closure_entry(
                    files,
                    &physical,
                    RuntimeTrust::MacosSealedSystem,
                    runtime_mount,
                    total_bytes,
                )?;
            } else {
                // Preserve the verified absence as explicit closure
                // provenance. A later physical appearance changes this field
                // and adds a physical binding, so rechecking fails closed.
                binding.missing_weak_dylibs.push(edge.clone());
            }
        }
        binding.missing_weak_dylibs.sort();
        binding.missing_weak_dylibs.dedup();
        entries.insert(path, binding);
    }
    Ok(())
}

fn bind_runtime_rga_config(path: &Path) -> Result<RuntimeBoundFile, QhardError> {
    let (bytes, binding) = binding(path, MAX_GOLDEN_BYTES, "comparator runtime rga config")?;
    if bytes != br#"{"custom_adapters":[]}"# {
        return Err(QhardError::Input(
            "comparator runtime rga config bytes must be exactly {\"custom_adapters\":[]}".into(),
        ));
    }
    let metadata = fs::symlink_metadata(path).map_err(|e| {
        QhardError::Input(format!("cannot inspect comparator runtime rga config: {e}"))
    })?;
    if !metadata.is_file() {
        return Err(QhardError::Input(
            "comparator runtime rga config must be a regular file".into(),
        ));
    }
    Ok(RuntimeBoundFile {
        trust: RuntimeTrust::AdministratorRuntime,
        binding,
        identity: RuntimeFileIdentity::from_metadata(&metadata),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ResolvedMachoRpath {
    Runtime(PathBuf),
    SealedSystem(PathBuf),
}

fn resolve_runtime_rpath(
    root: &Path,
    runtime_mount: &RuntimeMountIdentity,
    raw: &str,
    owner: &Path,
    executable: &Path,
) -> Result<ResolvedMachoRpath, QhardError> {
    let candidate = runtime_rpath_candidate(root, raw, owner, executable)?;
    if candidate.starts_with(root) {
        let resolved = resolve_runtime_path(root, &candidate, "Mach-O rpath", false)?;
        require_runtime_mount(runtime_mount, &resolved, "Mach-O runtime rpath")?;
        return Ok(ResolvedMachoRpath::Runtime(resolved));
    }
    let canonical = fs::canonicalize(&candidate)
        .map_err(|e| QhardError::Input(format!("cannot resolve Mach-O rpath: {e}")))?;
    if sealed_system_root(&canonical).is_none() {
        return Err(QhardError::Input(
            "Mach-O rpath is outside the comparator runtime and sealed-system roots".into(),
        ));
    }
    require_sealed_absolute_path(&canonical, "Mach-O sealed-system rpath")?;
    let metadata = fs::symlink_metadata(&canonical)
        .map_err(|e| QhardError::Input(format!("cannot inspect Mach-O rpath: {e}")))?;
    if !metadata.is_dir() {
        return Err(QhardError::Input(
            "Mach-O sealed-system rpath must be a directory".into(),
        ));
    }
    Ok(ResolvedMachoRpath::SealedSystem(canonical))
}

/// Expand one LC_RPATH value before binding it to either the dedicated runtime
/// mount or a sealed-system root.  The exact dyld tokens are valid rpaths: the
/// omitted suffix denotes the owning image's or executable's parent directory.
fn runtime_rpath_candidate(
    root: &Path,
    raw: &str,
    owner: &Path,
    executable: &Path,
) -> Result<PathBuf, QhardError> {
    checked_macho_path(raw, "Mach-O rpath")?;
    if raw == "@loader_path" {
        return safe_macho_join(
            owner.parent().ok_or_else(|| {
                QhardError::Input("Mach-O loader image has no parent directory".into())
            })?,
            ".",
            root,
            "Mach-O @loader_path rpath",
        );
    }
    if let Some(suffix) = raw.strip_prefix("@loader_path/") {
        return safe_macho_join(
            owner.parent().ok_or_else(|| {
                QhardError::Input("Mach-O loader image has no parent directory".into())
            })?,
            suffix,
            root,
            "Mach-O @loader_path rpath",
        );
    }
    if raw == "@executable_path" {
        return safe_macho_join(
            executable.parent().ok_or_else(|| {
                QhardError::Input("Mach-O executable has no parent directory".into())
            })?,
            ".",
            root,
            "Mach-O @executable_path rpath",
        );
    }
    if let Some(suffix) = raw.strip_prefix("@executable_path/") {
        return safe_macho_join(
            executable.parent().ok_or_else(|| {
                QhardError::Input("Mach-O executable has no parent directory".into())
            })?,
            suffix,
            root,
            "Mach-O @executable_path rpath",
        );
    }
    if raw.starts_with("@rpath/") {
        return Err(QhardError::Input(
            "LC_RPATH must not itself use @rpath".into(),
        ));
    }
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(QhardError::Input(
            "LC_RPATH must be absolute, @loader_path, or @executable_path".into(),
        ));
    }
    Ok(path.to_path_buf())
}

fn expanded_rpaths(
    root: &Path,
    runtime_mount: &RuntimeMountIdentity,
    inspection: &MachoInspection,
    owner: &Path,
    executable: &Path,
    inherited: &[ResolvedMachoRpath],
) -> Result<Vec<ResolvedMachoRpath>, QhardError> {
    let mut rpaths = Vec::new();
    for raw in &inspection.rpaths {
        let resolved = resolve_runtime_rpath(root, runtime_mount, raw, owner, executable)?;
        if !rpaths.contains(&resolved) {
            rpaths.push(resolved);
        }
    }
    for inherited_path in inherited {
        if !rpaths.contains(inherited_path) {
            rpaths.push(inherited_path.clone());
        }
    }
    Ok(rpaths)
}

fn resolve_macho_dependency(
    root: &Path,
    cache: &DyldSharedCacheCatalog,
    install_name: &str,
    owner: &Path,
    executable: &Path,
    rpaths: &[ResolvedMachoRpath],
) -> Result<ResolvedMachoDependency, QhardError> {
    checked_macho_path(install_name, "Mach-O install name")?;
    let runtime_file = |candidate: PathBuf, label: &str| {
        resolve_runtime_path(root, &candidate, label, true).map(ResolvedMachoDependency::Runtime)
    };
    if let Some(suffix) = install_name.strip_prefix("@loader_path/") {
        let base = owner.parent().ok_or_else(|| {
            QhardError::Input("Mach-O loader image has no parent directory".into())
        })?;
        return runtime_file(
            safe_macho_join(base, suffix, root, "Mach-O @loader_path dependency")?,
            "Mach-O @loader_path dependency",
        );
    }
    if let Some(suffix) = install_name.strip_prefix("@executable_path/") {
        let base = executable
            .parent()
            .ok_or_else(|| QhardError::Input("Mach-O executable has no parent directory".into()))?;
        return runtime_file(
            safe_macho_join(base, suffix, root, "Mach-O @executable_path dependency")?,
            "Mach-O @executable_path dependency",
        );
    }
    if let Some(suffix) = install_name.strip_prefix("@rpath/") {
        let mut candidates = Vec::new();
        for rpath in rpaths {
            let (candidate, trust) = match rpath {
                ResolvedMachoRpath::Runtime(path) => (
                    safe_macho_join(path, suffix, root, "Mach-O @rpath dependency")?,
                    RuntimeTrust::AdministratorRuntime,
                ),
                ResolvedMachoRpath::SealedSystem(path) => (
                    safe_macho_join(path, suffix, path, "Mach-O @rpath dependency")?,
                    RuntimeTrust::MacosSealedSystem,
                ),
            };
            candidates.push((candidate, trust));
        }
        return first_existing_macho_rpath_candidate(candidates, |(candidate, trust)| {
            match fs::symlink_metadata(candidate) {
                Ok(_) => match trust {
                    RuntimeTrust::AdministratorRuntime => {
                        resolve_runtime_path(root, candidate, "Mach-O @rpath dependency", true)
                            .map(ResolvedMachoDependency::Runtime)
                            .map(Some)
                    }
                    RuntimeTrust::MacosSealedSystem => {
                        validate_sealed_system_library(candidate, "Mach-O @rpath system dependency")
                            .map(ResolvedMachoDependency::SealedSystem)
                            .map(Some)
                    }
                    RuntimeTrust::MacosDyldSharedCache => {
                        unreachable!("cache images are not rpath directories")
                    }
                },
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound
                        && *trust == RuntimeTrust::MacosSealedSystem
                        && cache.images.contains_key(&candidate.display().to_string()) =>
                {
                    Ok(Some(ResolvedMachoDependency::SealedSystem(
                        candidate.clone(),
                    )))
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(QhardError::Input(format!(
                    "cannot inspect Mach-O @rpath dependency: {error}"
                ))),
            }
        })?
        .ok_or_else(|| {
            QhardError::Input(format!(
                "Mach-O @rpath dependency is unresolved: {install_name}"
            ))
        });
    }
    let path = Path::new(install_name);
    if !path.is_absolute() {
        return Err(QhardError::Input(format!(
            "Mach-O dependency is not absolute or tokenized: {install_name}"
        )));
    }
    if path.starts_with(root) {
        return runtime_file(path.to_path_buf(), "absolute Mach-O runtime dependency");
    }
    match validate_sealed_system_library(path, "absolute Mach-O system dependency") {
        Ok(path) => Ok(ResolvedMachoDependency::SealedSystem(path)),
        // Modern macOS exposes many exact `/usr/lib` install names only via
        // the dyld shared cache.  Defer acceptance to the catalog-backed
        // visitor; never turn an arbitrary missing system-looking path into
        // a dependency.
        Err(_error)
            if sealed_system_root(path).is_some()
                && matches!(fs::symlink_metadata(path), Err(ref missing) if missing.kind() == io::ErrorKind::NotFound) =>
        {
            Ok(ResolvedMachoDependency::SealedSystem(path.to_path_buf()))
        }
        Err(error) => Err(error),
    }
}

/// Walk a dynamic-loader closure without assuming that one image has one
/// global resolution context.  A shared dylib can be reached from multiple
/// executables with different inherited runpaths, so the visited key includes
/// both the image and the executable that established its `@executable_path`.
///
/// Keeping this traversal separate from filesystem inspection gives tests a
/// way to exercise cyclic and recursively sealed graphs without pretending a
/// user-owned temporary directory is an administrator runtime.
fn traverse_macho_closure<Inspect, Expand, Resolve, Visit>(
    initial: Vec<PendingMachoImage>,
    mut inspect: Inspect,
    mut expand_rpaths: Expand,
    mut resolve: Resolve,
    mut visit: Visit,
) -> Result<(), QhardError>
where
    Inspect: FnMut(&Path) -> Result<MachoInspection, QhardError>,
    Expand:
        FnMut(&PendingMachoImage, &MachoInspection) -> Result<Vec<ResolvedMachoRpath>, QhardError>,
    Resolve: FnMut(
        &str,
        &PendingMachoImage,
        &[ResolvedMachoRpath],
    ) -> Result<ResolvedMachoDependency, QhardError>,
    Visit: FnMut(&Path, RuntimeTrust) -> Result<(), QhardError>,
{
    let mut queue = initial;
    let mut visited = BTreeSet::new();
    while let Some(image) = queue.pop() {
        if !visited.insert((
            image.path.clone(),
            image.executable.clone(),
            image.inherited_rpaths.clone(),
        )) {
            continue;
        }
        if visited.len() > MAX_MACHO_RESOLUTION_CONTEXTS {
            return Err(QhardError::Input(
                "comparator runtime exceeds Mach-O resolution-context limit".into(),
            ));
        }
        visit(&image.path, RuntimeTrust::AdministratorRuntime)?;
        let inspection = inspect(&image.path)?;
        if inspection.has_dyld_environment {
            return Err(QhardError::Input(
                "Mach-O LC_DYLD_ENVIRONMENT is forbidden in comparator runtime".into(),
            ));
        }
        if image.path == image.executable {
            if inspection.dylinker.as_deref() != Some("/usr/lib/dyld") {
                return Err(QhardError::Input(
                    "comparator executable must use exactly the sealed /usr/lib/dyld loader".into(),
                ));
            }
            visit(Path::new("/usr/lib/dyld"), RuntimeTrust::MacosSealedSystem)?;
        } else if inspection.dylinker.is_some() {
            return Err(QhardError::Input(
                "comparator dylib must not contain an LC_LOAD_DYLINKER command".into(),
            ));
        }
        let rpaths = expand_rpaths(&image, &inspection)?;
        for install_name in inspection.loads {
            match resolve(&install_name, &image, &rpaths)? {
                ResolvedMachoDependency::Runtime(path) => queue.push(PendingMachoImage {
                    path,
                    executable: image.executable.clone(),
                    inherited_rpaths: rpaths.clone(),
                }),
                ResolvedMachoDependency::SealedSystem(path) => {
                    visit(&path, RuntimeTrust::MacosSealedSystem)?;
                }
            }
        }
    }
    Ok(())
}

/// Resolve and bind the complete Mach-O dependency closure as it exists now.
///
/// This must be used both at initial binding and during measurement.  Checking
/// only the old paths is insufficient: a new file in an earlier `LC_RPATH`
/// directory can make dyld load a different image without changing any old
/// closure entry.
fn resolve_runtime_closure(
    root: &Path,
    runtime_mount: &RuntimeMountIdentity,
    entry_paths: &BTreeMap<String, PathBuf>,
) -> Result<ResolvedRuntimeClosure, QhardError> {
    let inspector = trusted_otool()?;
    let cache = load_dyld_cache_catalog()?;
    let mut files = BTreeMap::new();
    let mut shared_cache = BTreeMap::new();
    let mut total_bytes = 0_u64;
    let queue = entry_paths
        .values()
        .map(|path| PendingMachoImage {
            path: path.clone(),
            executable: path.clone(),
            inherited_rpaths: Vec::new(),
        })
        .collect();
    traverse_macho_closure(
        queue,
        inspect_macho,
        |image, inspection| {
            expanded_rpaths(
                root,
                runtime_mount,
                inspection,
                &image.path,
                &image.executable,
                &image.inherited_rpaths,
            )
        },
        |install_name, image, rpaths| {
            resolve_macho_dependency(
                root,
                &cache,
                install_name,
                &image.path,
                &image.executable,
                rpaths,
            )
        },
        |path, trust| match trust {
            RuntimeTrust::MacosSealedSystem if matches!(fs::symlink_metadata(path), Err(ref error) if error.kind() == io::ErrorKind::NotFound) => {
                add_shared_cache_closure(
                    &cache,
                    path,
                    &mut shared_cache,
                    &mut files,
                    runtime_mount,
                    &mut total_bytes,
                )
            }
            _ => {
                add_runtime_closure_entry(&mut files, path, trust, runtime_mount, &mut total_bytes)
            }
        },
    )?;
    // `inspect_macho` invokes otool repeatedly.  Do not accept a closure
    // inspected by one system otool binary and attest a different one.
    let final_inspector = trusted_otool()?;
    if final_inspector.sha256 != inspector.sha256 {
        return Err(QhardError::Input(
            "otool changed while resolving comparator runtime closure".into(),
        ));
    }
    let mut closure = files
        .iter()
        .map(|(path, file)| RuntimeClosureEntry {
            path: path.display().to_string(),
            trust: file.trust.clone(),
            binding: RuntimeClosureBinding::File(file.binding.clone()),
        })
        .collect::<Vec<_>>();
    closure.extend(shared_cache.values().map(|binding| RuntimeClosureEntry {
        path: binding.install_name.clone(),
        trust: RuntimeTrust::MacosDyldSharedCache,
        binding: RuntimeClosureBinding::DyldSharedCache(binding.clone()),
    }));
    closure.sort_by(|left, right| left.path.cmp(&right.path));
    let closure_bytes = serde_jcs::to_vec(&closure).map_err(|error| {
        QhardError::Input(format!(
            "cannot canonically serialize comparator closure: {error}"
        ))
    })?;
    Ok(ResolvedRuntimeClosure {
        inspector: final_inspector,
        files,
        closure,
        closure_sha256: hash_bytes(&closure_bytes),
    })
}

fn runtime_closure_matches(
    provenance: &ComparatorRuntimeProvenance,
    resolved: &ResolvedRuntimeClosure,
) -> bool {
    resolved.inspector == provenance.inspector
        && resolved.closure_sha256 == provenance.closure_sha256
        && resolved.closure == provenance.closure
}

#[cfg(any(target_os = "macos", test))]
fn runtime_mount_is_read_only(flags: u64) -> bool {
    flags & MACOS_MNT_RDONLY != 0
}

fn runtime_mount_matches(expected: &RuntimeMountIdentity, current: &RuntimeMountIdentity) -> bool {
    current.read_only && current == expected
}

fn runtime_mount_pair_matches(
    expected: &RuntimeMountIdentity,
    public_path: &RuntimeMountIdentity,
    retained_fd: &RuntimeMountIdentity,
) -> bool {
    runtime_mount_matches(expected, public_path)
        && runtime_mount_matches(expected, retained_fd)
        && public_path == retained_fd
}

fn require_runtime_mount(
    expected: &RuntimeMountIdentity,
    path: &Path,
    label: &str,
) -> Result<(), QhardError> {
    let actual = inspect_runtime_mount(path)?;
    if !runtime_mount_matches(expected, &actual) {
        return Err(QhardError::Input(format!(
            "{label} is not on the bound comparator runtime read-only mount"
        )));
    }
    Ok(())
}

fn runtime_public_root_matches_handle(
    path: &Path,
    handle: &RetainedDirectory,
) -> Result<(), QhardError> {
    let listed = fs::symlink_metadata(path)
        .map_err(|e| QhardError::Input(format!("cannot inspect comparator runtime root: {e}")))?;
    let listed_identity = directory_identity_from_path(path, &listed)
        .map_err(|e| QhardError::Input(format!("cannot inspect comparator runtime root: {e}")))?;
    let retained_identity = directory_identity_from_file(&handle.handle).map_err(|e| {
        QhardError::Input(format!(
            "cannot inspect retained comparator runtime root: {e}"
        ))
    })?;
    if listed.file_type().is_symlink()
        || !listed.is_dir()
        || !matches!((listed_identity, retained_identity), (Some(listed), Some(retained)) if listed == retained)
    {
        return Err(QhardError::Input(
            "comparator runtime public root changed while bound".into(),
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn mount_string(value: &[c_char], label: &str) -> Result<String, QhardError> {
    // Darwin's statfs strings are fixed-size NUL-terminated arrays.  Reject a
    // non-UTF-8 or unterminated value rather than collapsing it into a lossy
    // mount identity that could compare equal across distinct mounts.
    let nul = value.iter().position(|byte| *byte == 0).ok_or_else(|| {
        QhardError::Input(format!("comparator runtime {label} is not NUL-terminated"))
    })?;
    let bytes = unsafe { CStr::from_ptr(value.as_ptr()) }.to_bytes();
    if bytes.len() != nul {
        return Err(QhardError::Input(format!(
            "comparator runtime {label} has an invalid mount string"
        )));
    }
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| QhardError::Input(format!("comparator runtime {label} is not UTF-8")))
}

#[cfg(target_os = "macos")]
fn runtime_mount_identity(details: &libc::statfs) -> Result<RuntimeMountIdentity, QhardError> {
    let flags = u64::from(details.f_flags);
    let mount = RuntimeMountIdentity {
        // libc intentionally hides fsid_t's representation, but preserves
        // Debug precisely so platform-specific opaque values remain
        // comparable.  Retain it as reportable provenance rather than
        // assuming a Linux-shaped public field.
        fsid: format!("{:?}", details.f_fsid),
        mount_point: mount_string(&details.f_mntonname, "mount point")?,
        mounted_from: mount_string(&details.f_mntfromname, "mount source")?,
        filesystem_type: mount_string(&details.f_fstypename, "filesystem type")?,
        flags,
        read_only: runtime_mount_is_read_only(flags),
    };
    Ok(mount)
}

#[cfg(target_os = "macos")]
fn observe_runtime_mount(path: &Path) -> Result<RuntimeMountIdentity, QhardError> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        QhardError::Input("comparator runtime path contains an interior NUL byte".into())
    })?;
    let mut details: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &mut details) } != 0 {
        return Err(QhardError::Input(format!(
            "cannot inspect comparator runtime mount: {}",
            io::Error::last_os_error()
        )));
    }
    runtime_mount_identity(&details)
}

#[cfg(target_os = "macos")]
fn observe_runtime_mount_fd(handle: &fs::File) -> Result<RuntimeMountIdentity, QhardError> {
    use std::os::fd::AsRawFd;
    let mut details: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(handle.as_raw_fd(), &mut details) } != 0 {
        return Err(QhardError::Input(format!(
            "cannot inspect retained comparator runtime mount: {}",
            io::Error::last_os_error()
        )));
    }
    runtime_mount_identity(&details)
}

#[cfg(not(target_os = "macos"))]
fn observe_runtime_mount(_path: &Path) -> Result<RuntimeMountIdentity, QhardError> {
    Err(QhardError::Input(
        "comparator runtime measurements require macOS MNT_RDONLY mount verification".into(),
    ))
}

#[cfg(not(target_os = "macos"))]
fn observe_runtime_mount_fd(_handle: &fs::File) -> Result<RuntimeMountIdentity, QhardError> {
    Err(QhardError::Input(
        "comparator runtime measurements require macOS MNT_RDONLY mount verification".into(),
    ))
}

fn inspect_runtime_mount(path: &Path) -> Result<RuntimeMountIdentity, QhardError> {
    let mount = observe_runtime_mount(path)?;
    if !mount.read_only {
        return Err(QhardError::Input(
            "comparator runtime must be on an MNT_RDONLY read-only mount".into(),
        ));
    }
    Ok(mount)
}

fn inspect_runtime_mount_fd(handle: &fs::File) -> Result<RuntimeMountIdentity, QhardError> {
    let mount = observe_runtime_mount_fd(handle)?;
    if !mount.read_only {
        return Err(QhardError::Input(
            "comparator runtime must be on an MNT_RDONLY read-only mount".into(),
        ));
    }
    Ok(mount)
}

impl ComparatorRuntime {
    fn bind(input: &Path) -> Result<Self, QhardError> {
        if !input.is_absolute() {
            return Err(QhardError::Input(
                "--comparator-runtime must be an absolute canonical path".into(),
            ));
        }
        let listed = fs::symlink_metadata(input)
            .map_err(|e| QhardError::Input(format!("cannot inspect --comparator-runtime: {e}")))?;
        if listed.file_type().is_symlink() || !listed.is_dir() {
            return Err(QhardError::Input(
                "--comparator-runtime must be a real directory, not a symlink".into(),
            ));
        }
        let root = fs::canonicalize(input).map_err(|e| {
            QhardError::Input(format!("cannot canonicalize --comparator-runtime: {e}"))
        })?;
        if root != input {
            return Err(QhardError::Input(
                "--comparator-runtime must use its canonical path spelling".into(),
            ));
        }
        require_sealed_absolute_path(&root, "comparator runtime root")?;
        require_allowed_runtime_path_xattrs(&root, "comparator runtime root")?;
        let root_handle = RetainedDirectory::open(&root, "comparator runtime root")?;
        let mount = inspect_runtime_mount(&root)?;
        let retained_mount = inspect_runtime_mount_fd(&root_handle.handle)?;
        if !runtime_mount_pair_matches(&mount, &mount, &retained_mount) {
            return Err(QhardError::Input(
                "comparator runtime mount differs between public path and retained handle".into(),
            ));
        }
        let bin_directory = resolve_runtime_path(
            &root,
            &root.join("bin"),
            "comparator runtime bin directory",
            false,
        )?;
        require_runtime_mount(&mount, &bin_directory, "comparator runtime bin directory")?;
        let config_path = resolve_runtime_path(
            &root,
            &root.join("config/rga-config.json"),
            "comparator runtime rga config",
            true,
        )?;
        require_runtime_mount(&mount, &config_path, "comparator runtime rga config")?;
        let config = bind_runtime_rga_config(&config_path)?;
        let mut entry_paths = BTreeMap::new();
        for name in ["rga", "rga-preproc", "pandoc", "pdftotext", "rg"] {
            let requested = root.join("bin").join(name);
            let path =
                resolve_runtime_path(&root, &requested, &format!("runtime bin/{name}"), true)?;
            require_runtime_mount(&mount, &path, &format!("runtime bin/{name}"))?;
            entry_paths.insert(name.to_owned(), path);
        }
        let resolved = resolve_runtime_closure(&root, &mount, &entry_paths)?;
        let final_mount = inspect_runtime_mount(&root)?;
        let final_retained_mount = inspect_runtime_mount_fd(&root_handle.handle)?;
        if !runtime_mount_pair_matches(&mount, &final_mount, &final_retained_mount) {
            return Err(QhardError::Input(
                "comparator runtime mount changed while binding".into(),
            ));
        }
        runtime_public_root_matches_handle(&root, &root_handle)?;
        Ok(Self {
            root: root.clone(),
            root_handle,
            mount: mount.clone(),
            bin_directory,
            config_path,
            config,
            provenance: ComparatorRuntimeProvenance {
                root: root.display().to_string(),
                mount,
                xattr_policy: "only-com.apple.provenance".into(),
                inspector: resolved.inspector,
                closure_sha256: resolved.closure_sha256,
                closure: resolved.closure,
            },
            files: resolved.files,
            entry_paths,
        })
    }

    fn entry(&self, name: &str) -> Result<(&Path, &RuntimeBoundFile), QhardError> {
        let path = self.entry_paths.get(name).ok_or_else(|| {
            QhardError::Input(format!("comparator runtime has no required bin/{name}"))
        })?;
        let file = self.files.get(path).ok_or_else(|| {
            QhardError::Input(format!("comparator runtime closure omitted bin/{name}"))
        })?;
        Ok((path, file))
    }

    fn recheck(&self, hash_contents: bool) -> Result<(), QhardError> {
        let mount = inspect_runtime_mount(&self.root)?;
        let retained_mount = inspect_runtime_mount_fd(&self.root_handle.handle)?;
        if !runtime_mount_pair_matches(&self.mount, &mount, &retained_mount) {
            return Err(QhardError::Input(
                "comparator runtime mount identity changed while bound".into(),
            ));
        }
        runtime_public_root_matches_handle(&self.root, &self.root_handle)?;
        require_sealed_absolute_path(&self.root, "comparator runtime root")?;
        require_allowed_runtime_path_xattrs(&self.root, "comparator runtime root")?;
        let bin_directory = resolve_runtime_path(
            &self.root,
            &self.root.join("bin"),
            "comparator runtime bin directory",
            false,
        )?;
        require_runtime_mount(
            &self.mount,
            &bin_directory,
            "comparator runtime bin directory",
        )?;
        if bin_directory != self.bin_directory {
            return Err(QhardError::Input(
                "comparator runtime bin directory changed while bound".into(),
            ));
        }
        let config_path = resolve_runtime_path(
            &self.root,
            &self.root.join("config/rga-config.json"),
            "comparator runtime rga config",
            true,
        )?;
        require_runtime_mount(&self.mount, &config_path, "comparator runtime rga config")?;
        if config_path != self.config_path {
            return Err(QhardError::Input(
                "comparator runtime rga config path changed while bound".into(),
            ));
        }
        let config_metadata = fs::symlink_metadata(&self.config_path).map_err(|e| {
            QhardError::Input(format!("cannot inspect comparator runtime rga config: {e}"))
        })?;
        if !config_metadata.is_file() || !self.config.identity.matches(&config_metadata) {
            return Err(QhardError::Input(
                "comparator runtime rga config changed while bound".into(),
            ));
        }
        if hash_contents
            && bind_runtime_rga_config(&self.config_path)?.binding.sha256
                != self.config.binding.sha256
        {
            return Err(QhardError::Input(
                "comparator runtime rga config content changed while bound".into(),
            ));
        }
        for (name, expected) in &self.entry_paths {
            let actual = resolve_runtime_path(
                &self.root,
                &self.root.join("bin").join(name),
                &format!("runtime bin/{name}"),
                true,
            )?;
            if &actual != expected {
                return Err(QhardError::Input(format!(
                    "comparator runtime bin/{name} changed while bound"
                )));
            }
            require_runtime_mount(&self.mount, &actual, &format!("runtime bin/{name}"))?;
        }
        for (path, file) in &self.files {
            match file.trust {
                RuntimeTrust::AdministratorRuntime => {
                    let resolved = resolve_runtime_path(
                        &self.root,
                        path,
                        "comparator runtime closure entry",
                        true,
                    )?;
                    if &resolved != path {
                        return Err(QhardError::Input(
                            "comparator runtime closure path changed while bound".into(),
                        ));
                    }
                    require_runtime_mount(
                        &self.mount,
                        &resolved,
                        "comparator runtime closure entry",
                    )?;
                }
                RuntimeTrust::MacosSealedSystem => {
                    let resolved =
                        validate_sealed_system_library(path, "macOS sealed-system closure entry")?;
                    if &resolved != path {
                        return Err(QhardError::Input(
                            "macOS sealed-system closure path changed while bound".into(),
                        ));
                    }
                }
                RuntimeTrust::MacosDyldSharedCache => {
                    return Err(QhardError::Input(
                        "shared-cache image was incorrectly treated as a filesystem closure entry"
                            .into(),
                    ));
                }
            }
            let metadata = fs::symlink_metadata(path).map_err(|e| {
                QhardError::Input(format!(
                    "cannot inspect comparator runtime closure entry: {e}"
                ))
            })?;
            if !metadata.is_file() || !file.identity.matches(&metadata) {
                return Err(QhardError::Input(
                    "comparator runtime closure entry changed while bound".into(),
                ));
            }
            if hash_contents
                && tool_binding(path, "comparator runtime closure entry")?.sha256
                    != file.binding.sha256
            {
                return Err(QhardError::Input(
                    "comparator runtime closure content changed while bound".into(),
                ));
            }
        }
        // Rebuild the graph, rather than merely checking the paths recorded at
        // binding time. This re-evaluates every loader token, symlink, sealed
        // system dependency, LC_LOAD_DYLINKER, and LC_DYLD_ENVIRONMENT control
        // and catches a newly introduced higher-priority `@rpath` candidate.
        // Always hash this fresh graph: callers invoke this before and after
        // each rga subprocess, so an altered image cannot improve a score.
        let resolved = resolve_runtime_closure(&self.root, &self.mount, &self.entry_paths)?;
        if resolved.inspector != self.provenance.inspector {
            return Err(QhardError::Input(
                "otool changed during comparator measurement".into(),
            ));
        }
        if !runtime_closure_matches(&self.provenance, &resolved) {
            return Err(QhardError::Input(
                "comparator runtime Mach-O closure changed while bound".into(),
            ));
        }
        let final_mount = inspect_runtime_mount(&self.root)?;
        let final_retained_mount = inspect_runtime_mount_fd(&self.root_handle.handle)?;
        if !runtime_mount_pair_matches(&self.mount, &final_mount, &final_retained_mount) {
            return Err(QhardError::Input(
                "comparator runtime mount changed during revalidation".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
struct RetainedToolFile {
    public_path: PathBuf,
    handle: fs::File,
    binding: FileBinding,
}

impl RetainedToolFile {
    fn open_runtime(runtime: &ComparatorRuntime, name: &str) -> Result<Self, QhardError> {
        let (public_path, expected) = runtime.entry(name)?;
        let public_path = public_path.to_path_buf();
        let listed = fs::symlink_metadata(&public_path)
            .map_err(|e| QhardError::Input(format!("cannot inspect {name}: {e}")))?;
        if listed.file_type().is_symlink() || !listed.is_file() {
            return Err(QhardError::Input(format!(
                "{name} must be a regular comparator runtime executable"
            )));
        }
        #[cfg(unix)]
        if listed.permissions().mode() & 0o111 == 0 {
            return Err(QhardError::Input(format!("{name} is not executable")));
        }
        let handle = fs::File::open(&public_path)
            .map_err(|e| QhardError::Input(format!("cannot retain {name}: {e}")))?;
        let opened = handle
            .metadata()
            .map_err(|e| QhardError::Input(format!("cannot inspect retained {name}: {e}")))?;
        if !same_std_file(&listed, &opened) {
            return Err(QhardError::Input(format!("{name} changed while binding")));
        }
        Ok(Self {
            public_path,
            handle,
            binding: expected.binding.clone(),
        })
    }

    fn recheck(&self, tool: &str) -> Result<(), QhardError> {
        let listed = fs::symlink_metadata(&self.public_path)
            .map_err(|e| QhardError::Input(format!("{tool} changed: {e}")))?;
        let opened = self
            .handle
            .metadata()
            .map_err(|e| QhardError::Input(format!("cannot inspect retained {tool}: {e}")))?;
        if listed.file_type().is_symlink() || !listed.is_file() || !same_std_file(&listed, &opened)
        {
            return Err(QhardError::Input(format!("{tool} changed while bound")));
        }
        // The initial digest is report provenance. The enclosing comparator
        // runtime is sealed before this object is accepted; retained
        // descriptor plus inode/device checks detect deletion or replacement
        // without making each query hash a large Mach-O executable again.
        Ok(())
    }
}

#[derive(Debug)]
struct PrivateRgaHelpers {
    _temp: tempfile::TempDir,
    cache: RetainedDirectory,
    pandoc: RetainedToolFile,
    pdftotext: RetainedToolFile,
    rg: RetainedToolFile,
}

impl PrivateRgaHelpers {
    fn new(runtime: &ComparatorRuntime) -> Result<Self, QhardError> {
        let pandoc = RetainedToolFile::open_runtime(runtime, "pandoc")?;
        let pdftotext = RetainedToolFile::open_runtime(runtime, "pdftotext")?;
        let rg = RetainedToolFile::open_runtime(runtime, "rg")?;
        let temp = tempfile::Builder::new()
            .prefix("kio-qhard-rga-helpers-")
            .tempdir()
            .map_err(|e| {
                QhardError::Input(format!("cannot create private rga helper directory: {e}"))
            })?;
        #[cfg(unix)]
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).map_err(|e| {
            QhardError::Input(format!("cannot secure private rga helper directory: {e}"))
        })?;
        let cache_path = temp.path().join("cache");
        fs::create_dir(&cache_path)
            .map_err(|e| QhardError::Input(format!("cannot create private rga cache: {e}")))?;
        #[cfg(unix)]
        fs::set_permissions(&cache_path, fs::Permissions::from_mode(0o700))
            .map_err(|e| QhardError::Input(format!("cannot secure private rga cache: {e}")))?;
        let cache = RetainedDirectory::open(&cache_path, "private rga cache")?;
        Ok(Self {
            _temp: temp,
            cache,
            pandoc,
            pdftotext,
            rg,
        })
    }

    fn recheck(&self) -> Result<(), QhardError> {
        let listed_cache = fs::symlink_metadata(&self.cache.public_path)
            .map_err(|e| QhardError::Input(format!("private rga cache changed: {e}")))?;
        let opened_cache = self
            .cache
            .handle
            .metadata()
            .map_err(|e| QhardError::Input(format!("cannot inspect private rga cache: {e}")))?;
        if listed_cache.file_type().is_symlink()
            || !listed_cache.is_dir()
            || !same_std_file(&listed_cache, &opened_cache)
        {
            return Err(QhardError::Input(
                "private rga cache changed while bound".into(),
            ));
        }
        self.pandoc.recheck("pandoc")?;
        self.pdftotext.recheck("pdftotext")?;
        self.rg.recheck("rg")?;
        Ok(())
    }
}

#[derive(Debug)]
struct BoundRga {
    runtime: ComparatorRuntime,
    rga: RetainedToolFile,
    preproc: RetainedToolFile,
    helpers: PrivateRgaHelpers,
    version: String,
}
fn bound_rga(runtime_root: &Path) -> Result<BoundRga, QhardError> {
    let runtime = ComparatorRuntime::bind(runtime_root)?;
    let rga = RetainedToolFile::open_runtime(&runtime, "rga")?;
    let preproc = RetainedToolFile::open_runtime(&runtime, "rga-preproc")?;
    let helpers = PrivateRgaHelpers::new(&runtime)?;
    let bound = BoundRga {
        runtime,
        rga,
        preproc,
        helpers,
        version: String::new(),
    };
    // Preflight under the exact environment used for queries. This catches a
    // malformed private config or helper binding before measurement begins.
    let output = run_bound_rga(&bound, &["--version"], None)?;
    if !output.status.success() || !output.stdout.to_ascii_lowercase().contains("ripgrep-all") {
        return Err(QhardError::Input(
            "rga failed private helper capability preflight".into(),
        ));
    }
    Ok(BoundRga {
        version: output.stdout.lines().next().unwrap_or("rga").to_owned(),
        ..bound
    })
}
fn tool_binding(path: &Path, label: &str) -> Result<FileBinding, QhardError> {
    let (_, binding) = binding(path, MAX_BINARY_BYTES, label)?;
    Ok(binding)
}
fn run_tool(path: &Path, args: &[&str]) -> Result<crate::runner::BoundedProcessOutput, QhardError> {
    let mut cmd = Command::new(path);
    cmd.args(args)
        .env_clear()
        .env(
            "PATH",
            env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin")),
        )
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("TZ", "UTC");
    Ok(run_bounded_command(
        &mut cmd,
        BoundedProcessOptions::default(),
        None,
    )?)
}
fn run_tool_in_directory(
    path: &Path,
    args: &[&str],
    directory: &RetainedDirectory,
) -> Result<crate::runner::BoundedProcessOutput, QhardError> {
    let mut command = Command::new(path);
    command
        .args(args)
        .env_clear()
        .env(
            "PATH",
            env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin")),
        )
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("TZ", "UTC");
    directory.configure_command_cwd(&mut command)?;
    Ok(run_bounded_command(
        &mut command,
        BoundedProcessOptions::default(),
        None,
    )?)
}

fn run_bound_rga(
    bound: &BoundRga,
    args: &[&str],
    directory: Option<&RetainedDirectory>,
) -> Result<crate::runner::BoundedProcessOutput, QhardError> {
    bound.runtime.recheck(false)?;
    bound.helpers.recheck()?;
    let mut command = Command::new(&bound.rga.public_path);
    let config_argument = format!("--rga-config-file={}", bound.runtime.config_path.display());
    let cache_argument = format!(
        "--rga-cache-path={}",
        bound.helpers.cache.public_path.display()
    );
    command.args([
        "--rga-adapters=pandoc,poppler",
        &cache_argument,
        &config_argument,
    ]);
    command
        .args(args)
        .env_clear()
        // The helper lookup directory is inside the sealed administrator
        // runtime. Do not create a user-owned symlink farm: rga-preproc
        // resolves pandoc/pdftotext/rg by PATH and a private directory would
        // reopen a same-UID path-swap window.
        .env("PATH", &bound.runtime.bin_directory)
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("TZ", "UTC");
    if let Some(directory) = directory {
        directory.configure_command_cwd(&mut command)?;
    }
    let output = run_bounded_command(&mut command, BoundedProcessOptions::default(), None)?;
    bound.helpers.recheck()?;
    bound.runtime.recheck(false)?;
    Ok(output)
}
fn query_fragments(query: &str) -> Vec<String> {
    // Exact implementation of the four frozen baseline fragment classes. Do not use
    // Unicode `is_alphanumeric`: it would admit Japanese words as an extra,
    // more selective baseline query and make Kio's margin look better.
    let input: Vec<char> = query.nfkc().collect();
    let mut found = BTreeSet::new();
    // Pattern 1: ASCII word. Each pattern has its own scan, deliberately
    // preserving overlaps (e.g. `F-12` yields both `f-12` and `12`).
    let mut i = 0;
    while i < input.len() {
        let c = input[i];
        if c.is_ascii_alphabetic() {
            let start = i;
            i += 1;
            while i < input.len()
                && (input[i].is_ascii_alphanumeric() || matches!(input[i], '_' | '+' | '-'))
            {
                i += 1;
            }
            if i - start >= 2 {
                found.insert(
                    input[start..i]
                        .iter()
                        .collect::<String>()
                        .to_ascii_lowercase(),
                );
            }
            continue;
        }
        i += 1;
    }
    // Pattern 2: `\d+(?:\.\d+)?`; digits are independent of words.
    i = 0;
    while i < input.len() {
        let c = input[i];
        if c.is_ascii_digit() {
            let start = i;
            while i < input.len() && input[i].is_ascii_digit() {
                i += 1;
            }
            if i + 1 < input.len() && input[i] == '.' && input[i + 1].is_ascii_digit() {
                i += 1;
                while i < input.len() && input[i].is_ascii_digit() {
                    i += 1;
                }
            }
            found.insert(input[start..i].iter().collect());
            continue;
        }
        i += 1;
    }
    // Pattern 3: Katakana runs.
    i = 0;
    while i < input.len() {
        let c = input[i];
        if matches!(c, 'ァ'..='ヶ' | 'ー') {
            let start = i;
            while i < input.len() && matches!(input[i], 'ァ'..='ヶ' | 'ー') {
                i += 1;
            }
            if i - start >= 2 {
                found.insert(input[start..i].iter().collect());
            }
            continue;
        }
        i += 1;
    }
    // Pattern 4: Han runs.
    i = 0;
    while i < input.len() {
        let c = input[i];
        if matches!(c, '\u{4e00}'..='\u{9fff}') {
            let start = i;
            while i < input.len() && matches!(input[i], '\u{4e00}'..='\u{9fff}') {
                i += 1;
            }
            if i - start >= 2 {
                found.insert(input[start..i].iter().collect());
            }
            continue;
        }
        i += 1;
    }
    found.into_iter().collect()
}
fn parse_kio_titles(stdout: &str) -> Result<Vec<String>, QhardError> {
    let v: Value = serde_json::from_str(stdout.trim())
        .map_err(|e| QhardError::Input(format!("baseline kio emitted invalid JSON: {e}")))?;
    Ok(v.get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| QhardError::Input("baseline kio response has no results".into()))?
        .iter()
        .take(10)
        .filter_map(|x| {
            x.get("title")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .collect())
}

/// The baseline's Kio lane uses one evaluator-owned device environment for
/// both rebuilding and searching.  Fixture-B's p01 registry is the global
/// registry; using a per-row persona environment would make the query observe
/// a different (and, for the minimal fixture, unpopulated) device replica.
fn baseline_kio_context(
    indexed_snapshot: &RetainedDirectory,
    online_query: bool,
) -> Result<(RetainedDirectory, ControlledEnvironment, Vec<&'static str>), QhardError> {
    let p01 = indexed_snapshot.child("p01", "private indexed p01 persona")?;
    let scope = discover_scopes(p01)?
        .into_iter()
        .next()
        .ok_or_else(|| QhardError::Input("indexed p01 persona has no scope".into()))?;
    let env_base = indexed_snapshot
        .child("env", "fixture env")?
        .child("p01", "p01 environment")?;
    let mut environment = ControlledEnvironment {
        fixed: vec![
            (
                OsString::from("PATH"),
                env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin")),
            ),
            (OsString::from("LANG"), OsString::from("C.UTF-8")),
            (OsString::from("LC_ALL"), OsString::from("C.UTF-8")),
            (OsString::from("TZ"), OsString::from("UTC")),
        ],
        directories: [
            ("XDG_CONFIG_HOME", "xdg-config"),
            ("XDG_DATA_HOME", "xdg-data"),
            ("XDG_CACHE_HOME", "xdg-cache"),
        ]
        .into_iter()
        .map(|(key, directory)| {
            Ok((
                OsString::from(key),
                env_base.child(directory, "fixture XDG")?,
            ))
        })
        .collect::<Result<Vec<_>, QhardError>>()?,
    };
    let mut forwarded = Vec::new();
    for name in
        available_online_query_credential_names(online_query, |name| env::var_os(name).is_some())
    {
        // Re-read immediately before inserting: the report must contain only
        // names that entered the controlled subprocess environment, never a
        // merely available credential name.
        if let Some(value) = env::var_os(name) {
            environment.fixed.push((OsString::from(name), value));
            forwarded.push(name);
        }
    }
    Ok((scope, environment, forwarded))
}

/// Recreate the disposable device replica once before the baseline queries.
/// The fixture intentionally omits this cache, so direct searches would
/// otherwise honestly report every registered scope as `index_missing`.
fn rebuild_baseline_replica(
    kio_path: &Path,
    scope: &RetainedDirectory,
    environment: &ControlledEnvironment,
) -> Result<(), QhardError> {
    let mut command = Command::new(kio_path);
    command.args(["--json", "repair", "replica"]);
    environment.apply(&mut command)?;
    scope.configure_command_cwd(&mut command)?;
    let output = run_bounded_command(&mut command, BoundedProcessOptions::default(), None)?;
    environment.recheck_private_directories()?;
    if !output.status.success() {
        return Err(QhardError::Input(format!(
            "private baseline device replica rebuild failed with exit {}",
            output.status.code().unwrap_or(-1)
        )));
    }
    Ok(())
}

fn mark_runtime_blocked(slot: &mut Option<String>, message: impl Into<String>) {
    if slot.is_none() {
        *slot = Some(message.into());
    }
}

fn baseline_measurement_status(
    missing: bool,
    runtime_blocked: bool,
    passed_gate: bool,
) -> &'static str {
    if passed_gate {
        "pass"
    } else if missing || runtime_blocked {
        "blocked-unmeasured"
    } else {
        "fail"
    }
}

pub fn run_baseline(options: BaselineOptions) -> Result<BaselineReport, QhardError> {
    let attest_opts = BaselineAttestOptions {
        golden: options.golden.clone(),
        fixture_root: options.fixture_root.clone(),
        baseline_corpus: options.baseline_corpus.clone(),
    };
    let (golden_rows, golden) = load_baseline_golden(&options.golden)?;
    let (_, indexed, pristine, computed) = baseline_bindings(&attest_opts)?;
    let attestation_path = options.attestation.as_ref().ok_or_else(|| {
        QhardError::Input(
            "baseline requires --attestation generated by benchmark baseline-attest".into(),
        )
    })?;
    let (att_bytes, attestation) = binding(
        attestation_path,
        MAX_ATTESTATION_BYTES,
        "baseline attestation",
    )?;
    let supplied: BaselineAttestation = serde_json::from_slice(&att_bytes)
        .map_err(|e| QhardError::Input(format!("invalid baseline attestation: {e}")))?;
    if supplied.schema_version != 1
        || supplied.fixture_id != computed.fixture_id
        || supplied.golden_sha256 != computed.golden_sha256
        || supplied.indexed_fixture_sha256 != computed.indexed_fixture_sha256
        || supplied.pristine_corpus_sha256 != computed.pristine_corpus_sha256
        || supplied.source_equivalence_sha256 != computed.source_equivalence_sha256
        || supplied.personas != computed.personas
    {
        return Err(QhardError::Input(
            "baseline attestation does not bind frozen golden and equivalent fixture trees".into(),
        ));
    }
    let bin = bound_executable(&options.bin)?;
    let kio_path = bin.1;
    let kio_binding = bin.2;
    let (_indexed_snapshot_temp, indexed_snapshot) =
        snapshot_baseline_fixture(&indexed, &computed.indexed_fixture_sha256)?;
    let mdfind_path = resolve_tool(&options.mdfind);
    // mdfind is a sealed macOS executable: copied snapshots are killed by
    // code-signing. Bind its exact system binary and execute that path.
    let (bound_mdfind, mdfind_unavailable_reason) = match mdfind_path.as_ref() {
        Some(path) => match trusted_mdfind(path) {
            Ok(bound) => (Some(bound), None),
            Err(error) => (None, Some(format!("mdfind is unavailable: {error}"))),
        },
        None => (
            None,
            Some("mdfind is unavailable: executable was not found".into()),
        ),
    };
    let (bound_rga, rga_unavailable_reason) = match options.comparator_runtime.as_ref() {
        Some(root) => match bound_rga(root) {
            Ok(bound) => (Some(bound), None),
            Err(error) => (None, Some(format!("rga is unavailable: {error}"))),
        },
        None => (
            None,
            Some("rga is unavailable: --comparator-runtime was not supplied".into()),
        ),
    };
    let mut tools = BTreeMap::new();
    tools.insert(
        "kio".into(),
        BaselineTool {
            executable_path: options.bin.display().to_string(),
            sha256: Some(kio_binding.sha256.clone()),
            companion_sha256: None,
            helpers: None,
            configuration: None,
            comparator_runtime: None,
            version: "kio under test".into(),
            available: true,
        },
    );
    tools.insert(
        "mdfind".into(),
        BaselineTool {
            executable_path: mdfind_path.as_ref().map_or_else(
                || options.mdfind.display().to_string(),
                |p| p.display().to_string(),
            ),
            sha256: bound_mdfind.as_ref().map(|b| b.sha256.clone()),
            companion_sha256: None,
            helpers: None,
            configuration: None,
            comparator_runtime: None,
            version: "macOS sealed-system Spotlight tool (mdfind has no version flag)".into(),
            available: bound_mdfind.is_some(),
        },
    );
    let rga_version = bound_rga
        .as_ref()
        .map(|bound| bound.version.clone())
        .unwrap_or_else(|| "unavailable".into());
    tools.insert(
        "rga".into(),
        BaselineTool {
            executable_path: options.comparator_runtime.as_ref().map_or_else(
                || "<no comparator runtime supplied>".into(),
                |root| root.join("bin/rga").display().to_string(),
            ),
            sha256: bound_rga
                .as_ref()
                .map(|bound| bound.rga.binding.sha256.clone()),
            companion_sha256: bound_rga
                .as_ref()
                .map(|bound| bound.preproc.binding.sha256.clone()),
            helpers: bound_rga.as_ref().map(|bound| {
                BTreeMap::from([
                    ("pandoc".into(), bound.helpers.pandoc.binding.clone()),
                    ("pdftotext".into(), bound.helpers.pdftotext.binding.clone()),
                    ("rg".into(), bound.helpers.rg.binding.clone()),
                ])
            }),
            configuration: bound_rga
                .as_ref()
                .map(|bound| bound.runtime.config.binding.clone()),
            comparator_runtime: bound_rga
                .as_ref()
                .map(|bound| bound.runtime.provenance.clone()),
            version: rga_version,
            available: bound_rga.is_some(),
        },
    );
    let missing = bound_mdfind.is_none() || bound_rga.is_none();
    let missing_reason = [mdfind_unavailable_reason, rga_unavailable_reason]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("; ");
    let mut configuration = BaselineConfiguration {
        online_query: options.online_query,
        // Empty until the controlled Kio subprocess environment is created.
        // A lane blocked before that point did not forward credentials.
        forwarded_credential_names: Vec::new(),
    };
    let mut rows = Vec::new();
    let mut runtime_blocked = None;
    if !missing {
        let (kio_scope, kio_environment, forwarded_credential_names) =
            baseline_kio_context(&indexed_snapshot, options.online_query)?;
        configuration.forwarded_credential_names = forwarded_credential_names;
        rebuild_baseline_replica(&kio_path, &kio_scope, &kio_environment)?;
        'queries: for q in &golden_rows {
            let expected = q
                .expected
                .iter()
                .map(|e| e.file.clone())
                .collect::<Vec<_>>();
            let mut command = Command::new(&kio_path);
            command.args([
                "--json",
                "search",
                &q.query,
                "--all-scopes",
                "--limit",
                "10",
            ]);
            kio_environment.apply(&mut command)?;
            kio_scope.configure_command_cwd(&mut command)?;
            let ko = run_bounded_command(&mut command, BoundedProcessOptions::default(), None)?;
            kio_environment.recheck_private_directories()?;
            let kt = if ko.status.success() {
                parse_kio_titles(&ko.stdout)?
            } else {
                vec![]
            };
            let root = pristine.child(&q.persona, "pristine persona")?;
            let before = digest_persona_tree(&root, false)?;
            let mo = match run_tool_in_directory(
                mdfind_path
                    .as_ref()
                    .expect("checked comparator availability"),
                &["-onlyin", ".", &q.query],
                &root,
            ) {
                Ok(output) => output,
                Err(error) => {
                    mark_runtime_blocked(
                        &mut runtime_blocked,
                        format!("mdfind could not run for {}: {error}", q.query_id),
                    );
                    break 'queries;
                }
            };
            let mdfind_current = match tool_binding(
                mdfind_path
                    .as_ref()
                    .expect("checked comparator availability"),
                "mdfind",
            ) {
                Ok(binding) => binding,
                Err(error) => {
                    mark_runtime_blocked(
                        &mut runtime_blocked,
                        format!(
                            "mdfind could not be revalidated for {}: {error}",
                            q.query_id
                        ),
                    );
                    break 'queries;
                }
            };
            if mdfind_current.sha256
                != bound_mdfind
                    .as_ref()
                    .expect("checked comparator availability")
                    .sha256
            {
                mark_runtime_blocked(&mut runtime_blocked, "mdfind changed during measurement");
                break 'queries;
            }
            if !mo.status.success() {
                mark_runtime_blocked(
                    &mut runtime_blocked,
                    format!(
                        "mdfind failed for {} with exit {}",
                        q.query_id,
                        mo.status.code().unwrap_or(-1)
                    ),
                );
                break 'queries;
            }
            let mt: Vec<String> = mo
                .stdout
                .lines()
                .filter_map(|s| {
                    Path::new(s)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(ToOwned::to_owned)
                })
                .take(10)
                .collect();
            let mut scores: BTreeMap<String, usize> = BTreeMap::new();
            let mut rga_returncode = 0;
            let mut rga_duration_ms = 0.0;
            for fragment in query_fragments(&q.query) {
                let ro = match run_bound_rga(
                    bound_rga.as_ref().expect("checked comparator availability"),
                    &[
                        "-l",
                        "--no-messages",
                        "--ignore-case",
                        "--fixed-strings",
                        &fragment,
                        ".",
                    ],
                    Some(&root),
                ) {
                    Ok(output) => output,
                    Err(error) => {
                        mark_runtime_blocked(
                            &mut runtime_blocked,
                            format!(
                                "rga runtime verification failed for {}: {error}",
                                q.query_id
                            ),
                        );
                        break 'queries;
                    }
                };
                rga_returncode = ro.status.code().unwrap_or(-1);
                rga_duration_ms += ro.duration.as_secs_f64() * 1000.0;
                if rga_returncode != 0 && rga_returncode != 1 {
                    mark_runtime_blocked(
                        &mut runtime_blocked,
                        format!("rga failed for {} with exit {}", q.query_id, rga_returncode),
                    );
                    break 'queries;
                }
                for line in ro.stdout.lines() {
                    *scores.entry(line.to_owned()).or_default() += 1;
                }
            }
            let mut ranked = scores.into_iter().collect::<Vec<_>>();
            ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let rt: Vec<String> = ranked
                .into_iter()
                .take(10)
                .filter_map(|(p, _)| {
                    Path::new(&p)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(ToOwned::to_owned)
                })
                .collect();
            if before != digest_persona_tree(&root, false)? {
                return Err(QhardError::Input(
                    "pristine corpus changed during baseline subprocess".into(),
                ));
            }
            let kio_hit = kt.iter().any(|x| expected.contains(x));
            let mdfind_hit = mt.iter().any(|x| expected.contains(x));
            let rga_hit = rt.iter().any(|x| expected.contains(x));
            rows.push(BaselineRow {
                query_id: q.query_id.clone(),
                class: q.class.clone(),
                persona: q.persona.clone(),
                expected_files: expected,
                kio: BaselineResult {
                    returned_items: kt.clone(),
                    hit: kio_hit,
                    returncode: ko.status.code().unwrap_or(-1),
                    duration_ms: ko.duration.as_secs_f64() * 1000.0,
                },
                mdfind: BaselineResult {
                    returned_items: mt.clone(),
                    hit: mdfind_hit,
                    returncode: mo.status.code().unwrap_or(-1),
                    duration_ms: mo.duration.as_secs_f64() * 1000.0,
                },
                rga: BaselineResult {
                    returned_items: rt.clone(),
                    hit: rga_hit,
                    returncode: rga_returncode,
                    duration_ms: rga_duration_ms,
                },
            });
        }
    }
    if let Some(bound) = &bound_rga {
        // The runtime root is sealed against the invoking user; this full
        // content check closes the measurement interval for administrator
        // maintenance or corruption without hashing its large Mach-O closure
        // once per query.
        if let Err(error) = bound
            .helpers
            .recheck()
            .and_then(|_| bound.runtime.recheck(true))
        {
            mark_runtime_blocked(
                &mut runtime_blocked,
                format!("rga runtime changed during measurement: {error}"),
            );
        }
    }
    if let Some(bound) = &bound_mdfind {
        match mdfind_path.as_ref().map(|path| trusted_mdfind(path)) {
            Some(Ok(current)) if current.sha256 == bound.sha256 => {}
            Some(Ok(_)) => {
                mark_runtime_blocked(&mut runtime_blocked, "mdfind changed during measurement")
            }
            Some(Err(error)) => mark_runtime_blocked(
                &mut runtime_blocked,
                format!("mdfind could not be revalidated after measurement: {error}"),
            ),
            None => mark_runtime_blocked(
                &mut runtime_blocked,
                "mdfind path disappeared during measurement",
            ),
        }
    }
    if fixture_live_digest(&indexed)? != computed.indexed_fixture_sha256
        || fixture_live_digest(&pristine)? != computed.pristine_corpus_sha256
        || tool_binding(&options.bin, "kio binary")?.sha256 != kio_binding.sha256
    {
        return Err(QhardError::Input(
            "baseline input changed during measurement".into(),
        ));
    }
    let recall = |f: fn(&BaselineRow) -> bool| {
        if rows.is_empty() {
            0.0
        } else {
            rows.iter().filter(|r| f(r)).count() as f64 / rows.len() as f64
        }
    };
    let kr = recall(|r| r.kio.hit);
    let mr = recall(|r| r.mdfind.hit);
    let rr = recall(|r| r.rga.hit);
    let gate = BaselineGate {
        kio_ge_0_8: kr >= 0.8,
        margin_mdfind_ge_0_3: kr - mr >= 0.3,
        margin_rga_ge_0_3: kr - rr >= 0.3,
        pass: !missing
            && runtime_blocked.is_none()
            && kr >= 0.8
            && kr - mr >= 0.3
            && kr - rr >= 0.3,
    };
    let mut recalls = BTreeMap::new();
    recalls.insert("kio".into(), kr);
    recalls.insert("mdfind".into(), mr);
    recalls.insert("rga".into(), rr);
    let mut deltas = BTreeMap::new();
    deltas.insert("kio_minus_mdfind".into(), kr - mr);
    deltas.insert("kio_minus_rga".into(), kr - rr);
    Ok(BaselineReport {
        schema_version: 2,
        benchmark: "kio-baseline-fixture-b",
        platform: format!("{}-{}", env::consts::OS, env::consts::ARCH),
        measured_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| QhardError::Input(format!("system clock is before Unix epoch: {e}")))?
            .as_millis(),
        status: baseline_measurement_status(missing, runtime_blocked.is_some(), gate.pass),
        blocked_reason: runtime_blocked.or_else(|| missing.then_some(missing_reason)),
        golden,
        attestation,
        indexed_fixture_sha256: computed.indexed_fixture_sha256,
        pristine_corpus_sha256: computed.pristine_corpus_sha256,
        source_equivalence_sha256: computed.source_equivalence_sha256,
        configuration,
        tools,
        rows,
        recall_at_10: recalls,
        deltas,
        gate,
    })
}

pub fn write_baseline_report(
    path: &Path,
    fixture_root: &Path,
    pristine_root: &Path,
    report: &BaselineReport,
) -> Result<(), QhardError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .map_err(|e| QhardError::Input(e.to_string()))?
            .join(path)
    };
    let indexed = RetainedDirectory::open(fixture_root, "fixture root")?.public_path;
    let pristine = RetainedDirectory::open(pristine_root, "pristine root")?.public_path;
    if absolute.starts_with(&indexed) || absolute.starts_with(&pristine) {
        return Err(QhardError::Input(
            "baseline report must be outside both fixture roots".into(),
        ));
    }
    let parent = RetainedDirectory::open(
        absolute
            .parent()
            .ok_or_else(|| QhardError::Input("baseline report has no parent".into()))?,
        "baseline report parent",
    )?;
    if parent.public_path.starts_with(&indexed) || parent.public_path.starts_with(&pristine) {
        return Err(QhardError::Input(
            "baseline report must be outside both fixture roots".into(),
        ));
    }
    publish_report(
        &parent.handle,
        &parent.sync_metadata,
        &parent.public_path,
        Path::new(
            absolute
                .file_name()
                .ok_or_else(|| QhardError::Input("baseline report has no filename".into()))?,
        ),
        &serde_json::to_vec_pretty(report)?,
    )
}

pub fn write_baseline_attestation(
    path: &Path,
    fixture_root: &Path,
    pristine_root: &Path,
    bytes: &[u8],
) -> Result<(), QhardError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .map_err(|e| QhardError::Input(e.to_string()))?
            .join(path)
    };
    let indexed = RetainedDirectory::open(fixture_root, "fixture root")?.public_path;
    let pristine = RetainedDirectory::open(pristine_root, "pristine root")?.public_path;
    if absolute.starts_with(&indexed) || absolute.starts_with(&pristine) {
        return Err(QhardError::Input(
            "baseline attestation must be outside both fixture roots".into(),
        ));
    }
    let parent = RetainedDirectory::open(
        absolute
            .parent()
            .ok_or_else(|| QhardError::Input("baseline attestation has no parent".into()))?,
        "baseline attestation parent",
    )?;
    if parent.public_path.starts_with(&indexed) || parent.public_path.starts_with(&pristine) {
        return Err(QhardError::Input(
            "baseline attestation must be outside both fixture roots".into(),
        ));
    }
    publish_report(
        &parent.handle,
        &parent.sync_metadata,
        &parent.public_path,
        Path::new(
            absolute
                .file_name()
                .ok_or_else(|| QhardError::Input("baseline attestation has no filename".into()))?,
        ),
        bytes,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use kio_pipeline::{
        markdownize::MarkdownizeMode,
        task::{TaskDescriptor, TaskStatus, TaskStore, TaskType},
    };
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    #[test]
    fn strict_golden_digest_rejects_non_frozen_input() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("golden.jsonl");
        fs::write(&path, b"{}\n").unwrap();
        assert!(load_golden(&path).is_err());
    }

    #[test]
    fn baseline_frozen_golden_has_required_population_and_fragments_are_stable() {
        let golden = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../eval/golden-queries-fixture-b.jsonl");
        let (rows, binding) = load_baseline_golden(&golden).unwrap();
        assert_eq!(binding.sha256, FROZEN_BASELINE_GOLDEN_SHA256);
        assert_eq!(rows.len(), 24);
        assert_eq!(
            query_fragments("修復作業 45 分の API_limit A ３.５ カタカナー"),
            vec!["3.5", "45", "api_limit", "カタカナー", "修復作業"]
        );
        // This fixed table ensures future Rust changes preserve every overlap
        // in the independently frozen fragment vectors.
        let expected = [
            "一度|上限",
            "何分|修復作業|時間枠",
            "保持例外|決裁日",
            "乱数条件|成果|水準",
            "予測|実績|西日本地域",
            "6|cohort|回目|測定",
            "何円|天井額|資料箱|運搬補償",
            "四半期|対象",
            "信頼|抽出|発話",
            "上限|予算|調達",
            "場合|契約|延長",
            "初動",
            "保全除外|期限",
            "キャッシュ|三月|営業",
            "採用提案|評価値",
            "基準値|投薬",
            "12|f-12|ミリ|区画",
            "ミリ|寸法|検査",
            "採点|最終日",
            "取材先|時刻|確認",
            "6.1|cc6|パーセント|証跡充足率",
            "arendt|ノード|流入引用数",
            "12|パネル|有害事象数",
            "delta|時刻",
        ];
        assert_eq!(rows.len(), expected.len());
        for (row, expected) in rows.iter().zip(expected) {
            assert_eq!(
                query_fragments(&row.query).join("|"),
                expected,
                "{}",
                row.query_id
            );
        }
    }

    #[test]
    fn baseline_equivalence_rejects_kio_independent_mutation() {
        let indexed = tempfile::tempdir().unwrap();
        let pristine = tempfile::tempdir().unwrap();
        fs::create_dir_all(indexed.path().join("p01/home/.kio")).unwrap();
        fs::create_dir_all(pristine.path().join("p01/home")).unwrap();
        fs::write(indexed.path().join("p01/home/a.txt"), b"one").unwrap();
        fs::write(pristine.path().join("p01/home/a.txt"), b"two").unwrap();
        let left = RetainedDirectory::open(&indexed.path().join("p01"), "left").unwrap();
        let right = RetainedDirectory::open(&pristine.path().join("p01"), "right").unwrap();
        assert_ne!(
            digest_persona_tree(&left, true).unwrap(),
            digest_persona_tree(&right, true).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn comparator_preflight_rejects_symlink_and_nonexecutable() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain");
        fs::write(&plain, b"#!/bin/sh\nexit 0\n").unwrap();
        assert!(executable_tool(&plain, "test tool").is_err());
        let link = dir.path().join("link");
        symlink(&plain, &link).unwrap();
        assert!(executable_tool(&link, "test tool").is_err());
        let true_binary = Path::new("/usr/bin/true");
        if true_binary.is_file() {
            assert!(executable_tool(true_binary, "rga").is_err());
            assert!(executable_tool(true_binary, "mdfind").is_err());
        }
    }

    #[test]
    fn mdfind_capability_accepts_the_system_help_preamble_only() {
        assert!(mdfind_capability_output(
            Some(5),
            "\nUsage: /usr/bin/mdfind [-onlyin directory] query\n\t-onlyin <dir>\n",
        ));
        assert!(!mdfind_capability_output(
            Some(0),
            "\nUsage: /usr/bin/mdfind [-onlyin directory] query\n",
        ));
        assert!(!mdfind_capability_output(
            Some(5),
            "other tool\nUsage: /usr/bin/mdfind [-onlyin directory] query\n",
        ));
    }

    #[test]
    fn macho_parsers_accept_bounded_loads_and_rpaths() {
        let load_commands = "Load command 1\n          cmd LC_LOAD_DYLINKER\n      cmdsize 32\n         name /usr/lib/dyld (offset 12)\nLoad command 2\n          cmd LC_ID_DYLIB\n      cmdsize 56\n         name @rpath/librga.dylib (offset 24)\nLoad command 3\n          cmd LC_LOAD_DYLIB\n      cmdsize 72\n         name @loader_path/../lib/liba.dylib (offset 24)\nLoad command 4\n          cmd LC_REEXPORT_DYLIB\n      cmdsize 64\n         name /usr/lib/libSystem.B.dylib (offset 24)\nLoad command 5\n          cmd LC_RPATH\n      cmdsize 40\n         path @loader_path/../lib (offset 12)\nLoad command 6\n          cmd LC_RPATH\n      cmdsize 40\n         path /sealed/runtime/lib (offset 12)\n";
        let (loads, dylinker, has_environment) = parse_otool_load_commands(load_commands).unwrap();
        assert_eq!(
            loads,
            vec![
                "@loader_path/../lib/liba.dylib",
                "/usr/lib/libSystem.B.dylib"
            ]
        );
        assert_eq!(dylinker.as_deref(), Some("/usr/lib/dyld"));
        assert!(!has_environment);
        assert_eq!(
            parse_otool_rpaths(load_commands).unwrap(),
            vec!["@loader_path/../lib", "/sealed/runtime/lib"]
        );
    }

    #[test]
    fn macho_parsers_reject_malformed_or_unbounded_closure_text() {
        assert!(parse_otool_load_commands("Load command 1\n  cmd LC_LOAD_DYLIB\n").is_err());
        assert!(
            parse_otool_load_commands("Load command 1\n  cmd LC_LOAD_DYLIB\n  name lib.dylib\n")
                .is_err()
        );
        assert!(parse_otool_rpaths("cmd LC_RPATH\nLoad command 2\n").is_err());
        assert!(checked_macho_path(&"x".repeat(MAX_MACHO_PATH_BYTES + 1), "test").is_err());
        let (_, _, environment) = parse_otool_load_commands(
            "Load command 1\n  cmd LC_DYLD_ENVIRONMENT\n  cmdsize 48\n     name DYLD_INSERT_LIBRARIES=/tmp/evil.dylib (offset 24)\n",
        )
        .unwrap();
        assert!(environment);
    }

    /// These are deliberately parser-level contract vectors so Linux checks
    /// exercise the same fail-closed Mach-O admission boundary as macOS. They
    /// replace source-shape assertions about the runtime builder.
    #[test]
    fn comparator_runtime_rejects_untrusted_macho_metadata_vectors() {
        for output in [
            "Load command 1\n  cmd LC_LOAD_DYLIB\n  name /tmp/evil\r.dylib (offset 12)\n",
            "Load command 1\n  cmd LC_LOAD_DYLINKER\n  name /usr/lib/dyld (offset 12)\nLoad command 2\n  cmd LC_LOAD_DYLINKER\n  name /usr/lib/dyld (offset 12)\n",
            "Load command 1\n  cmd LC_LOAD_DYLIB\n  name @rpath/lib\0evil.dylib (offset 12)\n",
        ] {
            assert!(parse_otool_load_commands(output).is_err(), "{output:?}");
        }
        for output in [
            "cmd LC_RPATH\n  path @loader_path\r (offset 12)\n",
            "cmd LC_RPATH\n  path @loader_path\0/../outside (offset 12)\n",
        ] {
            assert!(parse_otool_rpaths(output).is_err(), "{output:?}");
        }
    }

    fn test_dyld_info_binding() -> FileBinding {
        FileBinding {
            path: "/Library/Developer/CommandLineTools/usr/bin/dyld_info".into(),
            sha256: "catalog-tool".into(),
            bytes: 1,
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dyld_shared_cache_catalog_is_strict_and_arch_specific() {
        let catalog = parse_dyld_cache_catalog(
            "/usr/lib/libSystem.B.dylib [arm64e]:\n    -uuid:\n        40277974-D20C-3EC8-B25C-43AE30D8CC60\n    -linked_dylibs:\n        attributes     load path\n        upward         /usr/lib/libobjc.A.dylib\n/usr/lib/libobjc.A.dylib [arm64e]:\n    -uuid:\n        40277974-D20C-3EC8-B25C-43AE30D8CC61\n    -linked_dylibs:\n        attributes     load path\n/usr/lib/libSystem.B.dylib [x86_64]:\n    -uuid:\n        40277974-D20C-3EC8-B25C-43AE30D8CC62\n    -linked_dylibs:\n        attributes     load path\n",
            "arm64e", "macos:test:build:kernel:aarch64", test_dyld_info_binding()).unwrap();
        assert_eq!(catalog.images.len(), 2);
        assert_eq!(
            catalog.images["/usr/lib/libSystem.B.dylib"].linked_dylibs[0].path,
            "/usr/lib/libobjc.A.dylib"
        );
        assert!(parse_dyld_cache_catalog(
            "/tmp/evil.dylib [arm64e]:\n    -uuid:\n        40277974-D20C-3EC8-B25C-43AE30D8CC60\n",
            "arm64e",
            "macos:test:build:kernel:aarch64",
            test_dyld_info_binding()
        )
        .is_err());
        assert!(
            parse_dyld_cache_catalog(
                "/usr/lib/a.dylib [arm64e]:\n    -uuid:\n        invalid\n",
                "arm64e",
                "macos:test:build:kernel:aarch64",
                test_dyld_info_binding()
            )
            .is_err()
        );
        assert!(parse_dyld_cache_catalog(
            "/usr/lib/a.dylib [arm64e]:\n    -uuid:\n        40277974-D20C-3EC8-B25C-43AE30D8CC60\n/usr/lib/a.dylib [arm64e]:\n    -uuid:\n        40277974-D20C-3EC8-B25C-43AE30D8CC61\n", "arm64e", "macos:test:build:kernel:aarch64", test_dyld_info_binding()).is_err());
        assert!(parse_dyld_cache_catalog(
            "/usr/lib/a.dylib [arm64e]:\n    -uuid:\n        40277974-D20C-3EC8-B25C-43AE30D8CC60\n    -linked_dylibs:\n        attributes     load path\n        unexpected     /usr/lib/b.dylib\n/usr/lib/b.dylib [arm64e]:\n    -uuid:\n        40277974-D20C-3EC8-B25C-43AE30D8CC61\n    -linked_dylibs:\n        attributes     load path\n",
            "arm64e",
            "macos:test:build:kernel:aarch64",
            test_dyld_info_binding()
        )
        .is_err());
        for malformed in [
            "/usr/lib/a.dylib [arm64e]:\n",
            "/usr/lib/a.dylib [arm64e]:\n-uuid:\n40277974-D20C-3EC8-B25C-43AE30D8CC60\n",
            "/usr/lib/a.dylib [arm64e]:\n-uuid:\n40277974-D20C-3EC8-B25C-43AE30D8CC60\n-linked_dylibs:\n",
            "/usr/lib/a.dylib [arm64e]:\n-uuid:\n40277974-D20C-3EC8-B25C-43AE30D8CC60\n-linked_dylibs:\nattributes load path\n",
            "/usr/lib/a.dylib [arm64e]:\n-uuid:\n40277974-D20C-3EC8-B25C-43AE30D8CC60\n-linked_dylibs:\nattributes     load path\ngarbage\n",
        ] {
            assert!(
                parse_dyld_cache_catalog(
                    malformed,
                    "arm64e",
                    "macos:test:build:kernel:aarch64",
                    test_dyld_info_binding()
                )
                .is_err()
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dyld_shared_cache_frozen_catalog_covers_cache_only_libsystem() {
        let catalog = parse_dyld_cache_catalog(
            "/usr/lib/libSystem.B.dylib [arm64e]:\n    -uuid:\n        40277974-D20C-3EC8-B25C-43AE30D8CC60\n    -linked_dylibs:\n        attributes     load path\n        upward         /usr/lib/libobjc.A.dylib\n/usr/lib/libobjc.A.dylib [arm64e]:\n    -uuid:\n        40277974-D20C-3EC8-B25C-43AE30D8CC61\n    -linked_dylibs:\n        attributes     load path\n",
            "arm64e",
            "macos:test:build:kernel:aarch64",
            test_dyld_info_binding(),
        )
        .unwrap();
        assert!(catalog.images.contains_key("/usr/lib/libSystem.B.dylib"));
        assert_eq!(
            catalog.images["/usr/lib/libSystem.B.dylib"].linked_dylibs,
            vec![DyldSharedCacheEdge {
                attributes: "upward".into(),
                path: "/usr/lib/libobjc.A.dylib".into(),
            }]
        );
    }

    #[test]
    fn dyld_shared_cache_closure_is_recursive_and_digest_sensitive() {
        let mut images = BTreeMap::new();
        images.insert(
            "/usr/lib/a.dylib".into(),
            DyldSharedCacheImage {
                uuid: "AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA".into(),
                linked_dylibs: vec![DyldSharedCacheEdge {
                    attributes: "upward".into(),
                    path: "/usr/lib/b.dylib".into(),
                }],
            },
        );
        images.insert(
            "/usr/lib/b.dylib".into(),
            DyldSharedCacheImage {
                uuid: "BBBBBBBB-BBBB-BBBB-BBBB-BBBBBBBBBBBB".into(),
                linked_dylibs: vec![DyldSharedCacheEdge {
                    attributes: "".into(),
                    path: "/usr/lib/a.dylib".into(),
                }],
            },
        );
        let catalog = DyldSharedCacheCatalog {
            inspector: test_dyld_info_binding(),
            architecture: "arm64e".into(),
            platform: "macos-aarch64".into(),
            images,
        };
        let mut closure = BTreeMap::new();
        let mut files = BTreeMap::new();
        let mut bytes = 0;
        let mount = RuntimeMountIdentity {
            fsid: "test".into(),
            mount_point: "/sealed".into(),
            mounted_from: "test".into(),
            filesystem_type: "test".into(),
            flags: MACOS_MNT_RDONLY,
            read_only: true,
        };
        add_shared_cache_closure(
            &catalog,
            Path::new("/usr/lib/a.dylib"),
            &mut closure,
            &mut files,
            &mount,
            &mut bytes,
        )
        .unwrap();
        assert_eq!(closure.len(), 2);
        let before = hash_bytes(&serde_jcs::to_vec(&closure).unwrap());
        closure.get_mut("/usr/lib/b.dylib").unwrap().uuid =
            "CCCCCCCC-CCCC-CCCC-CCCC-CCCCCCCCCCCC".into();
        assert_ne!(before, hash_bytes(&serde_jcs::to_vec(&closure).unwrap()));
    }

    #[test]
    fn shared_cache_missing_weak_edges_are_recorded_but_required_edges_fail_closed() {
        let weak = DyldSharedCacheEdge {
            attributes: "upward weak-link".into(),
            path: "/usr/lib/__kio_eval_missing_weak_edge.dylib".into(),
        };
        let delay_init_weak = DyldSharedCacheEdge {
            attributes: "delay-init weak-link".into(),
            path: "/usr/lib/__kio_eval_missing_delay_init_weak_edge.dylib".into(),
        };
        let required = DyldSharedCacheEdge {
            attributes: "upward".into(),
            path: "/usr/lib/__kio_eval_missing_required_edge.dylib".into(),
        };
        assert!(!classify_shared_cache_physical_edge(&weak, Err(io::ErrorKind::NotFound)).unwrap());
        assert!(
            !classify_shared_cache_physical_edge(&delay_init_weak, Err(io::ErrorKind::NotFound))
                .unwrap()
        );
        assert!(
            classify_shared_cache_physical_edge(&required, Err(io::ErrorKind::NotFound)).is_err()
        );
        assert!(classify_shared_cache_physical_edge(&weak, Ok(())).unwrap());
        assert!(
            validate_shared_cache_edge_attributes(&DyldSharedCacheEdge {
                attributes: "weak-link unexpected".into(),
                path: weak.path.clone(),
            })
            .is_err()
        );

        let mut images = BTreeMap::new();
        images.insert(
            "/usr/lib/__kio_eval_parent_weak_edge.dylib".into(),
            DyldSharedCacheImage {
                uuid: "AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA".into(),
                linked_dylibs: vec![weak.clone(), delay_init_weak.clone()],
            },
        );
        let catalog = DyldSharedCacheCatalog {
            inspector: test_dyld_info_binding(),
            architecture: "arm64e".into(),
            platform: "test".into(),
            images,
        };
        let mount = RuntimeMountIdentity {
            fsid: "test".into(),
            mount_point: "/sealed".into(),
            mounted_from: "test".into(),
            filesystem_type: "test".into(),
            flags: MACOS_MNT_RDONLY,
            read_only: true,
        };
        let mut closure = BTreeMap::new();
        let mut files = BTreeMap::new();
        let mut bytes = 0;
        add_shared_cache_closure(
            &catalog,
            Path::new("/usr/lib/__kio_eval_parent_weak_edge.dylib"),
            &mut closure,
            &mut files,
            &mount,
            &mut bytes,
        )
        .unwrap();
        assert_eq!(closure.len(), 1);
        assert_eq!(
            closure["/usr/lib/__kio_eval_parent_weak_edge.dylib"].linked_dylibs,
            vec![weak, delay_init_weak]
        );
        let mut expected_missing = closure["/usr/lib/__kio_eval_parent_weak_edge.dylib"]
            .linked_dylibs
            .clone();
        expected_missing.sort();
        assert_eq!(
            closure["/usr/lib/__kio_eval_parent_weak_edge.dylib"].missing_weak_dylibs,
            expected_missing
        );
        assert!(files.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn shared_cache_dangling_weak_symlink_is_classified_as_absent() {
        let temporary = tempfile::tempdir().unwrap();
        let dangling = temporary.path().join("missing-weak.dylib");
        symlink("absent-target.dylib", &dangling).unwrap();
        // `symlink_metadata` observes the link inode, but dyld needs the
        // target and therefore sees this as missing. The production probe
        // must follow links with `metadata` before applying weak-link policy.
        assert!(fs::symlink_metadata(&dangling).is_ok());
        let lookup = || {
            fs::metadata(&dangling)
                .map(|_| ())
                .map_err(|error| error.kind())
        };
        let weak = DyldSharedCacheEdge {
            attributes: "weak-link".into(),
            path: dangling.display().to_string(),
        };
        let required = DyldSharedCacheEdge {
            attributes: "upward".into(),
            path: dangling.display().to_string(),
        };
        assert!(!classify_shared_cache_physical_edge(&weak, lookup()).unwrap());
        assert!(classify_shared_cache_physical_edge(&required, lookup()).is_err());
    }

    #[test]
    fn shared_cache_weak_edge_appearance_changes_runtime_closure() {
        let inspector = test_dyld_info_binding();
        let weak = DyldSharedCacheEdge {
            attributes: "weak-link".into(),
            path: "/usr/lib/__kio_eval_weak_edge_appeared.dylib".into(),
        };
        let cache_binding = |missing_weak_dylibs| DyldSharedCacheBinding {
            install_name: "/usr/lib/__kio_eval_weak_edge_parent.dylib".into(),
            architecture: "arm64e".into(),
            uuid: "AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA".into(),
            linked_dylibs: vec![weak.clone()],
            missing_weak_dylibs,
            inspector: inspector.clone(),
            platform: "test".into(),
        };
        let cache_entry = |binding: DyldSharedCacheBinding| RuntimeClosureEntry {
            path: binding.install_name.clone(),
            trust: RuntimeTrust::MacosDyldSharedCache,
            binding: RuntimeClosureBinding::DyldSharedCache(binding),
        };
        let initial_closure = vec![cache_entry(cache_binding(vec![weak.clone()]))];
        let closure_digest = |closure: &[RuntimeClosureEntry]| {
            hash_bytes(&serde_jcs::to_vec(closure).expect("test closure serializes"))
        };
        let provenance = ComparatorRuntimeProvenance {
            root: "/sealed/runtime".into(),
            mount: RuntimeMountIdentity {
                fsid: "test".into(),
                mount_point: "/sealed".into(),
                mounted_from: "test".into(),
                filesystem_type: "test".into(),
                flags: MACOS_MNT_RDONLY,
                read_only: true,
            },
            xattr_policy: "only-com.apple.provenance".into(),
            inspector: inspector.clone(),
            closure_sha256: closure_digest(&initial_closure),
            closure: initial_closure,
        };
        let physical_entry = RuntimeClosureEntry {
            path: weak.path.clone(),
            trust: RuntimeTrust::MacosSealedSystem,
            binding: RuntimeClosureBinding::File(FileBinding {
                path: weak.path.clone(),
                sha256: "physical-weak-edge".into(),
                bytes: 1,
            }),
        };
        let mut after_appearance_closure =
            vec![cache_entry(cache_binding(Vec::new())), physical_entry];
        after_appearance_closure.sort_by(|left, right| left.path.cmp(&right.path));
        let after_appearance_digest = closure_digest(&after_appearance_closure);
        assert_ne!(provenance.closure_sha256, after_appearance_digest);
        let after_appearance = ResolvedRuntimeClosure {
            inspector,
            files: BTreeMap::new(),
            closure: after_appearance_closure,
            closure_sha256: after_appearance_digest,
        };
        assert!(!runtime_closure_matches(&provenance, &after_appearance));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macho_rpath_resolves_cache_only_candidate_exactly() {
        let mut images = BTreeMap::new();
        images.insert(
            "/usr/lib/libcache-only-test.dylib".into(),
            DyldSharedCacheImage {
                uuid: "AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA".into(),
                linked_dylibs: vec![],
            },
        );
        let cache = DyldSharedCacheCatalog {
            inspector: test_dyld_info_binding(),
            architecture: "arm64e".into(),
            platform: "test".into(),
            images,
        };
        let resolved = resolve_macho_dependency(
            Path::new("/sealed/runtime"),
            &cache,
            "@rpath/libcache-only-test.dylib",
            Path::new("/sealed/runtime/bin/rga"),
            Path::new("/sealed/runtime/bin/rga"),
            &[ResolvedMachoRpath::SealedSystem(PathBuf::from("/usr/lib"))],
        )
        .unwrap();
        assert!(matches!(
            resolved,
            ResolvedMachoDependency::SealedSystem(path)
                if path == Path::new("/usr/lib/libcache-only-test.dylib")
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sealed_macos_dyld_runtime_catalog_is_manual_opt_in() -> Result<(), QhardError> {
        // This is intentionally absent from ordinary CI: it probes the sealed
        // host runtime and requires the Apple-signed CommandLineTools
        // `dyld_info`. Run it explicitly with
        // `KIO_EVAL_SEALED_MACOS_DYLD_RUNTIME=1 cargo test -p kio-eval
        // qhard::tests::sealed_macos_dyld_runtime_catalog_is_manual_opt_in`.
        if std::env::var_os("KIO_EVAL_SEALED_MACOS_DYLD_RUNTIME") != Some(OsString::from("1")) {
            return Ok(());
        }
        let catalog = load_dyld_cache_catalog()?;
        assert!(catalog.images.contains_key("/usr/lib/libSystem.B.dylib"));
        Ok(())
    }

    #[test]
    fn macho_token_paths_cannot_escape_the_runtime_root() {
        let root = Path::new("/sealed/runtime");
        assert_eq!(
            safe_macho_join(
                Path::new("/sealed/runtime/bin"),
                "../lib/liba.dylib",
                root,
                "test",
            )
            .unwrap(),
            PathBuf::from("/sealed/runtime/lib/liba.dylib")
        );
        assert!(
            safe_macho_join(
                Path::new("/sealed/runtime/bin"),
                "../../outside.dylib",
                root,
                "test",
            )
            .is_err()
        );
        assert!(
            safe_macho_join(
                Path::new("/sealed/runtime/bin"),
                "/outside.dylib",
                root,
                "test",
            )
            .is_err()
        );
    }

    #[test]
    fn macho_rpath_exact_tokens_resolve_to_their_parent_directories() {
        let root = Path::new("/sealed/runtime");
        let owner = Path::new("/sealed/runtime/lib/nested/libowner.dylib");
        let executable = Path::new("/sealed/runtime/bin/rga");

        assert_eq!(
            runtime_rpath_candidate(root, "@loader_path", owner, executable).unwrap(),
            PathBuf::from("/sealed/runtime/lib/nested")
        );
        assert_eq!(
            runtime_rpath_candidate(root, "@executable_path", owner, executable).unwrap(),
            PathBuf::from("/sealed/runtime/bin")
        );
        assert_eq!(
            runtime_rpath_candidate(root, "@loader_path/../lib", owner, executable).unwrap(),
            PathBuf::from("/sealed/runtime/lib/lib")
        );
        assert_eq!(
            runtime_rpath_candidate(root, "@executable_path/../lib", owner, executable).unwrap(),
            PathBuf::from("/sealed/runtime/lib")
        );
    }

    #[test]
    fn macho_rpath_exact_tokens_preserve_runtime_bounds_and_relative_rejection() {
        let root = Path::new("/sealed/runtime");
        let owner = Path::new("/sealed/runtime/lib/libowner.dylib");
        let executable = Path::new("/sealed/runtime/bin/rga");

        assert!(
            runtime_rpath_candidate(root, "@loader_path/../../outside", owner, executable).is_err()
        );
        assert!(
            runtime_rpath_candidate(root, "@executable_path/../../outside", owner, executable)
                .is_err()
        );
        assert!(
            runtime_rpath_candidate(
                root,
                "@loader_path",
                Path::new("/outside/libowner.dylib"),
                executable
            )
            .is_err()
        );
        assert!(runtime_rpath_candidate(root, "lib", owner, executable).is_err());
        assert!(runtime_rpath_candidate(root, "@rpath/lib", owner, executable).is_err());
    }

    #[test]
    fn macho_traversal_accepts_complete_sealed_closure() {
        let root = PathBuf::from("/sealed/runtime");
        let rga = root.join("bin/rga");
        let library_a = root.join("lib/liba.dylib");
        let library_b = root.join("lib/libb.dylib");
        let graph = BTreeMap::from([
            (
                rga.clone(),
                MachoInspection {
                    loads: vec!["@loader_path/../lib/liba.dylib".into()],
                    rpaths: vec![],
                    dylinker: Some("/usr/lib/dyld".into()),
                    has_dyld_environment: false,
                },
            ),
            (
                library_a.clone(),
                MachoInspection {
                    loads: vec!["@rpath/libb.dylib".into()],
                    rpaths: vec!["@loader_path".into()],
                    dylinker: None,
                    has_dyld_environment: false,
                },
            ),
            (
                library_b.clone(),
                MachoInspection {
                    loads: vec![
                        "@rpath/liba.dylib".into(),
                        "/usr/lib/libSystem.B.dylib".into(),
                    ],
                    rpaths: vec!["@loader_path".into()],
                    dylinker: None,
                    has_dyld_environment: false,
                },
            ),
        ]);
        let mut visited = Vec::new();
        traverse_macho_closure(
            vec![PendingMachoImage {
                path: rga.clone(),
                executable: rga,
                inherited_rpaths: vec![],
            }],
            |path| Ok(graph.get(path).expect("synthetic graph image").clone()),
            |image, _| {
                if image.path == library_a || image.path == library_b {
                    Ok(vec![ResolvedMachoRpath::Runtime(root.join("lib"))])
                } else {
                    Ok(vec![])
                }
            },
            |name, image, _| match (image.path.as_path(), name) {
                (path, "@loader_path/../lib/liba.dylib") if path == root.join("bin/rga") => {
                    Ok(ResolvedMachoDependency::Runtime(library_a.clone()))
                }
                (_, "@rpath/libb.dylib") => Ok(ResolvedMachoDependency::Runtime(library_b.clone())),
                (_, "@rpath/liba.dylib") => Ok(ResolvedMachoDependency::Runtime(library_a.clone())),
                (_, "/usr/lib/libSystem.B.dylib") => Ok(ResolvedMachoDependency::SealedSystem(
                    PathBuf::from("/usr/lib/libSystem.B.dylib"),
                )),
                _ => panic!("unexpected synthetic install name: {name}"),
            },
            |path, trust| {
                visited.push((path.to_path_buf(), trust));
                Ok(())
            },
        )
        .unwrap();
        assert!(visited.contains(&(library_a, RuntimeTrust::AdministratorRuntime)));
        assert!(visited.contains(&(library_b, RuntimeTrust::AdministratorRuntime)));
        assert!(visited.contains(&(
            PathBuf::from("/usr/lib/libSystem.B.dylib"),
            RuntimeTrust::MacosSealedSystem,
        )));
        assert!(visited.contains(&(
            PathBuf::from("/usr/lib/dyld"),
            RuntimeTrust::MacosSealedSystem,
        )));
    }

    #[test]
    fn macho_rpath_reresolution_detects_new_higher_priority_candidate() {
        let temporary = tempfile::tempdir().unwrap();
        let first_dir = temporary.path().join("first");
        let second_dir = temporary.path().join("second");
        fs::create_dir_all(&first_dir).unwrap();
        fs::create_dir_all(&second_dir).unwrap();
        let first = first_dir.join("libchoice.dylib");
        let second = second_dir.join("libchoice.dylib");
        fs::write(&second, b"second").unwrap();
        let select = || {
            first_existing_macho_rpath_candidate([first.clone(), second.clone()], |candidate| {
                match fs::symlink_metadata(candidate) {
                    Ok(metadata) if metadata.is_file() => Ok(Some(candidate.clone())),
                    Ok(_) => Err(QhardError::Input("test candidate is not a file".into())),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                    Err(error) => Err(QhardError::Input(format!(
                        "cannot inspect test rpath candidate: {error}"
                    ))),
                }
            })
            .unwrap()
            .expect("one rpath candidate exists")
        };

        // The initial lookup reaches only the second directory.  Adding the
        // same name to the first directory switches dyld's priority result.
        let initially_resolved = select();
        assert_eq!(initially_resolved, second);
        fs::write(&first, b"first").unwrap();
        let freshly_resolved = select();
        assert_eq!(freshly_resolved, first);

        let binding = |path: &Path| FileBinding {
            path: path.display().to_string(),
            sha256: "test-digest".into(),
            bytes: 1,
        };
        let initial_entry = RuntimeClosureEntry {
            path: initially_resolved.display().to_string(),
            trust: RuntimeTrust::AdministratorRuntime,
            binding: RuntimeClosureBinding::File(binding(&initially_resolved)),
        };
        let provenance = ComparatorRuntimeProvenance {
            root: temporary.path().display().to_string(),
            mount: RuntimeMountIdentity {
                fsid: "fsid(7,11)".into(),
                mount_point: "/sealed".into(),
                mounted_from: "disk3s1".into(),
                filesystem_type: "apfs".into(),
                flags: MACOS_MNT_RDONLY,
                read_only: true,
            },
            xattr_policy: "only-com.apple.provenance".into(),
            inspector: binding(Path::new("/usr/bin/otool")),
            closure_sha256: "initial-closure".into(),
            closure: vec![initial_entry],
        };
        let fresh_entry = RuntimeClosureEntry {
            path: freshly_resolved.display().to_string(),
            trust: RuntimeTrust::AdministratorRuntime,
            binding: RuntimeClosureBinding::File(binding(&freshly_resolved)),
        };
        let fresh = ResolvedRuntimeClosure {
            inspector: binding(Path::new("/usr/bin/otool")),
            files: BTreeMap::new(),
            closure_sha256: "fresh-closure".into(),
            closure: vec![fresh_entry],
        };
        assert!(!runtime_closure_matches(&provenance, &fresh));
    }

    #[test]
    fn read_only_mount_policy_rejects_writable_nested_and_identity_changes() {
        assert!(runtime_mount_is_read_only(MACOS_MNT_RDONLY));
        assert!(!runtime_mount_is_read_only(0));
        let bound = RuntimeMountIdentity {
            fsid: "fsid(17,29)".into(),
            mount_point: "/Library/KioComparatorRuntime/v1".into(),
            mounted_from: "/dev/disk9s1".into(),
            filesystem_type: "apfs".into(),
            flags: MACOS_MNT_RDONLY,
            read_only: true,
        };
        assert!(runtime_mount_matches(&bound, &bound));

        let mut remounted = bound.clone();
        remounted.fsid = "fsid(18,29)".into();
        assert!(!runtime_mount_matches(&bound, &remounted));
        // A nested APFS/UDIF mount can itself be read-only, yet must not be
        // accepted: it can be unmounted independently of the bound runtime.
        let mut nested_read_only = bound.clone();
        nested_read_only.mount_point = "/Library/KioComparatorRuntime/v1/lib".into();
        nested_read_only.mounted_from = "/dev/disk10s1".into();
        assert!(!runtime_mount_matches(&bound, &nested_read_only));

        let mut writable = bound.clone();
        writable.flags = 0;
        writable.read_only = false;
        assert!(!runtime_mount_matches(&bound, &writable));

        assert!(!runtime_mount_pair_matches(&bound, &bound, &remounted));
        assert!(!runtime_mount_pair_matches(
            &bound,
            &bound,
            &nested_read_only
        ));
    }

    #[test]
    fn comparator_runtime_xattr_policy_allows_only_empty_or_provenance() {
        assert!(runtime_xattr_names_allowed(&BTreeSet::new()));
        assert!(runtime_xattr_names_allowed(&BTreeSet::from([
            "com.apple.provenance".to_owned(),
        ])));
        assert!(!runtime_xattr_names_allowed(&BTreeSet::from([
            "com.example.untrusted".to_owned(),
        ])));
        assert!(!runtime_xattr_names_allowed(&BTreeSet::from([
            "com.apple.FinderInfo".to_owned(),
        ])));
        assert!(!runtime_xattr_names_allowed(&BTreeSet::from([
            "com.apple.diskimages.recentcksum".to_owned(),
        ])));
        assert!(!runtime_xattr_names_allowed(&BTreeSet::from([
            "com.apple.provenance".to_owned(),
            "com.example.untrusted".to_owned(),
        ])));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn comparator_runtime_rejects_unexpected_xattr_without_reading_value() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("runtime-image");
        fs::write(&path, b"sealed-by-mode-only").unwrap();
        let output = Command::new("/usr/bin/xattr")
            .args([
                "-w",
                "com.kio.untrusted",
                "opaque",
                &path.display().to_string(),
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "xattr did not add a test attribute"
        );
        assert!(require_allowed_runtime_xattrs(&path, "test runtime image").is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn comparator_runtime_rejects_writable_mount_before_measurement() {
        let temporary = tempfile::tempdir().unwrap();
        let error = inspect_runtime_mount(temporary.path()).unwrap_err();
        assert!(error.to_string().contains("MNT_RDONLY read-only mount"));
    }

    #[test]
    fn macho_loader_controls_reject_external_dyld_and_embedded_environment() {
        let root = PathBuf::from("/sealed/runtime");
        let executable = root.join("bin/rga");
        let run = |inspection: MachoInspection| {
            traverse_macho_closure(
                vec![PendingMachoImage {
                    path: executable.clone(),
                    executable: executable.clone(),
                    inherited_rpaths: vec![],
                }],
                |_| Ok(inspection.clone()),
                |_, _| Ok(vec![]),
                |_, _, _| panic!("synthetic image has no dylib loads"),
                |_, _| Ok(()),
            )
        };
        assert!(
            run(MachoInspection {
                loads: vec![],
                rpaths: vec![],
                dylinker: Some("/tmp/untrusted-dyld".into()),
                has_dyld_environment: false,
            })
            .is_err()
        );
        assert!(
            run(MachoInspection {
                loads: vec![],
                rpaths: vec![],
                dylinker: Some("/usr/lib/dyld".into()),
                has_dyld_environment: true,
            })
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn runtime_symlink_to_external_writable_dylib_is_rejected() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("runtime");
        let external = temporary.path().join("external");
        fs::create_dir_all(root.join("lib")).unwrap();
        fs::create_dir_all(&external).unwrap();
        let outside = external.join("libunsafe.dylib");
        fs::write(&outside, b"unsealed").unwrap();
        symlink(&outside, root.join("lib/libunsafe.dylib")).unwrap();
        assert!(
            canonical_path_within(
                &fs::canonicalize(&root).unwrap(),
                &root.join("lib/libunsafe.dylib"),
                "test external dylib",
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn runtime_file_identity_detects_replaced_helper_or_dylib() {
        let temporary = tempfile::tempdir().unwrap();
        let image = temporary.path().join("image");
        let replacement = temporary.path().join("replacement");
        fs::write(&image, b"original").unwrap();
        let identity = RuntimeFileIdentity::from_metadata(&fs::metadata(&image).unwrap());
        fs::write(&replacement, b"changed!").unwrap();
        fs::rename(&replacement, &image).unwrap();
        assert!(!identity.matches(&fs::metadata(&image).unwrap()));
    }

    #[test]
    fn comparator_unavailability_or_runtime_validation_is_blocked_not_passed() {
        assert_eq!(
            baseline_measurement_status(true, false, false),
            "blocked-unmeasured"
        );
        assert_eq!(
            baseline_measurement_status(false, true, false),
            "blocked-unmeasured"
        );
        assert_eq!(baseline_measurement_status(false, false, false), "fail");
        assert_eq!(baseline_measurement_status(false, false, true), "pass");
    }

    #[test]
    fn comparator_runtime_config_requires_exact_frozen_bytes() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("rga-config.json");
        fs::write(&path, br#"{"custom_adapters":[]}"#).unwrap();
        assert!(bind_runtime_rga_config(&path).is_ok());

        for noncanonical in [
            br#"{"custom_adapters":[]}\n"#.as_slice(),
            br#"{ "custom_adapters": [] }"#.as_slice(),
            br#"{"custom_adapters":[],"extra":null}"#.as_slice(),
        ] {
            fs::write(&path, noncanonical).unwrap();
            assert!(bind_runtime_rga_config(&path).is_err());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn comparator_runtime_rejects_extended_acl_write_grants() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("runtime-image");
        fs::write(&path, b"sealed-by-mode-only").unwrap();
        let user = env::var("USER").unwrap();
        let output = Command::new("/bin/chmod")
            .args([
                "+a",
                &format!("user:{user} allow write"),
                &path.display().to_string(),
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "chmod +a did not add a test ACL");
        assert!(macos_acl::has_extended_acl(&path, false).unwrap());
        assert!(require_no_extended_acl(&path, "test runtime image").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn comparator_runtime_rejects_evaluator_owned_root_before_any_measurement() {
        let temporary = tempfile::tempdir().unwrap();
        assert!(ComparatorRuntime::bind(temporary.path()).is_err());
    }

    #[test]
    fn baseline_snapshot_rejects_source_digest_not_matching_attestation() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("changed.txt"), b"changed").unwrap();
        let source = RetainedDirectory::open(dir.path(), "source").unwrap();
        assert!(snapshot_baseline_fixture(&source, "sha256:not-the-live-digest").is_err());
    }

    #[test]
    fn baseline_kio_context_always_uses_the_shared_p01_scope_and_environment() {
        let fixture = tempfile::tempdir().unwrap();
        fs::create_dir_all(fixture.path().join("p01/home/work/.kio")).unwrap();
        fs::create_dir_all(fixture.path().join("p02/home/work/.kio")).unwrap();
        for directory in ["xdg-config", "xdg-data", "xdg-cache"] {
            fs::create_dir_all(fixture.path().join("env/p01").join(directory)).unwrap();
        }
        let root = RetainedDirectory::open(fixture.path(), "baseline fixture").unwrap();
        let expected_env = root.public_path.join("env/p01");

        let (scope, environment, forwarded) = baseline_kio_context(&root, false).unwrap();

        assert_eq!(
            scope.public_path,
            fs::canonicalize(fixture.path().join("p01/home/work")).unwrap()
        );
        assert!(
            environment
                .directories
                .iter()
                .all(|(_, directory)| directory.public_path.starts_with(&expected_env))
        );
        assert!(forwarded.is_empty());
    }

    #[test]
    fn baseline_online_query_configuration_is_name_only_and_deterministic() {
        assert!(available_online_query_credential_names(false, |_| true).is_empty());
        assert_eq!(
            available_online_query_credential_names(true, |name| name == "MISTRAL_API_KEY"),
            vec!["MISTRAL_API_KEY"]
        );

        let rendered = serde_json::to_string(&BaselineConfiguration {
            online_query: true,
            forwarded_credential_names: vec!["GEMINI_API_KEY", "MISTRAL_API_KEY"],
        })
        .unwrap();
        assert_eq!(
            rendered,
            r#"{"online_query":true,"forwarded_credential_names":["GEMINI_API_KEY","MISTRAL_API_KEY"]}"#
        );
        assert!(!rendered.contains("test-secret-value"));
    }

    #[cfg(unix)]
    #[test]
    fn baseline_replica_rebuild_reports_a_clear_nonzero_failure() {
        let false_binary = Path::new("/usr/bin/false");
        if !false_binary.is_file() {
            return;
        }
        let fixture = tempfile::tempdir().unwrap();
        fs::create_dir_all(fixture.path().join("p01/home/work/.kio")).unwrap();
        for directory in ["xdg-config", "xdg-data", "xdg-cache"] {
            fs::create_dir_all(fixture.path().join("env/p01").join(directory)).unwrap();
        }
        let root = RetainedDirectory::open(fixture.path(), "baseline fixture").unwrap();
        let (scope, environment, _) = baseline_kio_context(&root, false).unwrap();

        let error = rebuild_baseline_replica(false_binary, &scope, &environment).unwrap_err();

        assert!(matches!(
            error,
            QhardError::Input(message)
                if message == "private baseline device replica rebuild failed with exit 1"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn private_snapshot_copies_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let source = tempfile::tempdir().unwrap();
        fs::create_dir(source.path().join("nested")).unwrap();
        fs::write(source.path().join("nested/evidence"), b"secret").unwrap();
        let bound = RetainedDirectory::open(source.path(), "source").unwrap();
        let output = tempfile::tempdir().unwrap();
        let copied = output.path().join("copy");
        copy_fixture_directory(&bound.handle, &copied, &mut FixtureDigestBudget::default())
            .unwrap();
        assert_eq!(
            fs::metadata(&copied).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(copied.join("nested"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(copied.join("nested/evidence"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    #[cfg(unix)]
    #[test]
    fn scope_discovery_skips_symlink_and_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/.kio")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), dir.path().join("escape")).unwrap();
        assert_eq!(
            discover_scopes(RetainedDirectory::open(dir.path(), "test tree").unwrap())
                .unwrap()
                .into_iter()
                .map(|scope| scope.public_path)
                .collect::<Vec<_>>(),
            vec![fs::canonicalize(dir.path().join("a")).unwrap()]
        );
    }
    #[test]
    fn live_fixture_digest_detects_content_mutation() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("evidence.txt"), b"before").unwrap();
        let bound = RetainedDirectory::open(dir.path(), "fixture").unwrap();
        let before = fixture_live_digest(&bound).unwrap();
        fs::write(dir.path().join("evidence.txt"), b"after").unwrap();
        assert_ne!(before, fixture_live_digest(&bound).unwrap());
    }
    #[test]
    fn fixture_content_digest_excludes_only_root_attestation() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("qhard-attestation.json"),
            b"first attestation",
        )
        .unwrap();
        fs::write(dir.path().join("evidence.txt"), b"original evidence").unwrap();
        let bound = RetainedDirectory::open(dir.path(), "fixture").unwrap();

        let before = fixture_content_digest(&bound).unwrap();
        fs::write(
            dir.path().join("qhard-attestation.json"),
            b"updated attestation",
        )
        .unwrap();
        assert_eq!(before, fixture_content_digest(&bound).unwrap());

        fs::write(dir.path().join("evidence.txt"), b"changed evidence").unwrap();
        assert_ne!(before, fixture_content_digest(&bound).unwrap());
    }
    #[test]
    fn live_fixture_digest_rejects_aggregate_byte_overflow_before_reading() {
        let dir = tempfile::tempdir().unwrap();
        let file = fs::File::create(dir.path().join("oversized.bin")).unwrap();
        file.set_len(MAX_LIVE_FIXTURE_FILE_BYTES + 1).unwrap();
        let bound = RetainedDirectory::open(dir.path(), "fixture").unwrap();
        assert!(fixture_live_digest(&bound).is_err());
    }
    #[test]
    fn snapshot_registry_paths_are_private() {
        let source = tempfile::tempdir().unwrap();
        let snapshot = tempfile::tempdir().unwrap();
        let source_qhard = source.path().join("qhard");
        let source_scope = source_qhard.join("a");
        let source_kio = source_scope.join(".kio");
        let registry = snapshot
            .path()
            .join("env/qhard/xdg-data/kio/scope-registry.sqlite");
        fs::create_dir_all(registry.parent().unwrap()).unwrap();
        let connection = Connection::open(&registry).unwrap();
        connection
            .execute_batch("CREATE TABLE scopes (kio_path TEXT, root_path TEXT);")
            .unwrap();
        connection
            .execute(
                "INSERT INTO scopes VALUES (?1, ?2)",
                rusqlite::params![source_kio.to_string_lossy(), source_scope.to_string_lossy()],
            )
            .unwrap();
        drop(connection);
        rewrite_snapshot_registry(snapshot.path(), source.path(), "qhard").unwrap();
        let connection = Connection::open(registry).unwrap();
        let path: String = connection
            .query_row("SELECT kio_path FROM scopes", [], |row| row.get(0))
            .unwrap();
        assert!(path.starts_with(&snapshot.path().to_string_lossy().to_string()));
        assert!(!path.starts_with(&source.path().to_string_lossy().to_string()));
    }

    #[test]
    fn snapshot_registry_rejects_off_tree_scope_before_rewrite() {
        let source = tempfile::tempdir().unwrap();
        let snapshot = tempfile::tempdir().unwrap();
        let registry = snapshot
            .path()
            .join("env/qhard/xdg-data/kio/scope-registry.sqlite");
        fs::create_dir_all(registry.parent().unwrap()).unwrap();
        let connection = Connection::open(&registry).unwrap();
        connection
            .execute_batch("CREATE TABLE scopes (kio_path TEXT, root_path TEXT);")
            .unwrap();
        connection
            .execute(
                "INSERT INTO scopes VALUES (?1, ?2)",
                rusqlite::params![
                    source.path().join("qhard/a/.kio").to_string_lossy(),
                    source.path().join("qhard/a").to_string_lossy(),
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO scopes VALUES (?1, ?2)",
                rusqlite::params!["/private/off-tree/.kio", "/private/off-tree"],
            )
            .unwrap();
        drop(connection);

        assert!(rewrite_snapshot_registry(snapshot.path(), source.path(), "qhard").is_err());

        let connection = Connection::open(registry).unwrap();
        let path: String = connection
            .query_row(
                "SELECT kio_path FROM scopes ORDER BY rowid LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(path, source.path().join("qhard/a/.kio").to_string_lossy());
    }

    #[test]
    fn fixture_snapshot_preserves_portable_current_task_output_refs() {
        let source = tempfile::tempdir().unwrap();
        let scope_relative = PathBuf::from("qhard/p01/home/work");
        let source_root = fs::canonicalize(source.path()).unwrap();
        let source_scope = source_root.join(&scope_relative);
        let source_kio = source_scope.join(".kio");
        let raw_hash = format!("sha256:{}", "a".repeat(64));
        let tool_hash = format!("sha256:{}", "b".repeat(64));
        let source_output = kio_pipeline::markdownize::normalized_instance_dir(
            &source_kio,
            &raw_hash,
            &tool_hash,
            0,
        );
        fs::create_dir_all(&source_output).unwrap();
        TaskStore::new(&source_kio)
            .replace_all(&[TaskDescriptor {
                task_id: "task_01H".into(),
                task_type: TaskType::Markdownize,
                mode: Some(MarkdownizeMode::Full),
                input_path: "fixture.pdf".into(),
                input_hash: raw_hash.clone(),
                previous_raw_hash: None,
                parent_run_id: None,
                changed_unit_keys: Vec::new(),
                output_ref: kio_pipeline::task::normalized_task_output_ref(
                    &raw_hash, &tool_hash, 0,
                ),
                unit_keys: None,
                status: TaskStatus::Done,
                attempts: 1,
                next_retry_at: None,
                deadline: None,
                heartbeat_at: None,
                fallback_reason: None,
                created_at: "2026-08-13T00:00:00Z".into(),
                bbox_annotation_enabled: None,
                hold_reason: None,
                reserved_usd: None,
                reserved_month: None,
                reservation_id: None,
            }])
            .unwrap();
        let registry = source
            .path()
            .join("env/qhard/xdg-data/kio/scope-registry.sqlite");
        fs::create_dir_all(registry.parent().unwrap()).unwrap();
        let connection = Connection::open(&registry).unwrap();
        connection
            .execute_batch("CREATE TABLE scopes (kio_path TEXT, root_path TEXT);")
            .unwrap();
        connection
            .execute(
                "INSERT INTO scopes VALUES (?1, ?2)",
                rusqlite::params![source_kio.to_string_lossy(), source_scope.to_string_lossy(),],
            )
            .unwrap();
        drop(connection);

        let root = RetainedDirectory::open(source.path(), "fixture").unwrap();
        let live_sha256 = fixture_live_digest(&root).unwrap();
        let fixture = Fixture {
            root,
            tree: "qhard".into(),
            env_name: "qhard".into(),
            scope_relatives: vec![scope_relative.to_string_lossy().into_owned()],
            attestation: FileBinding {
                path: "test-attestation".into(),
                sha256: "sha256:test".into(),
                bytes: 0,
            },
            live_sha256,
        };

        let snapshot = snapshot_fixture(&fixture).unwrap();
        let snapshot_kio = snapshot.root.public_path.join(&scope_relative).join(".kio");
        let tasks = TaskStore::new(&snapshot_kio).all().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(
            tasks[0].output_ref,
            kio_pipeline::task::normalized_task_output_ref(&raw_hash, &tool_hash, 0),
        );
        assert_eq!(
            fs::read(source_kio.join("tasks.jsonl")).unwrap(),
            fs::read(snapshot_kio.join("tasks.jsonl")).unwrap(),
        );
    }

    #[test]
    fn baseline_snapshot_preserves_each_current_scope_journal() {
        let source = tempfile::tempdir().unwrap();
        let source_root = fs::canonicalize(source.path()).unwrap();
        let p01_scope = source_root.join("p01/home/work");
        let p01_kio = p01_scope.join(".kio");
        let raw_hash = format!("sha256:{}", "c".repeat(64));
        let tool_hash = format!("sha256:{}", "d".repeat(64));
        let p01_output =
            kio_pipeline::markdownize::normalized_instance_dir(&p01_kio, &raw_hash, &tool_hash, 0);
        fs::create_dir_all(&p01_output).unwrap();
        TaskStore::new(&p01_kio)
            .replace_all(&[TaskDescriptor {
                task_id: "task_01H".into(),
                task_type: TaskType::Markdownize,
                mode: Some(MarkdownizeMode::Full),
                input_path: "fixture.pdf".into(),
                input_hash: raw_hash.clone(),
                previous_raw_hash: None,
                parent_run_id: None,
                changed_unit_keys: Vec::new(),
                output_ref: kio_pipeline::task::normalized_task_output_ref(
                    &raw_hash, &tool_hash, 0,
                ),
                unit_keys: None,
                status: TaskStatus::Done,
                attempts: 1,
                next_retry_at: None,
                deadline: None,
                heartbeat_at: None,
                fallback_reason: None,
                created_at: "2026-08-13T00:00:00Z".into(),
                bbox_annotation_enabled: None,
                hold_reason: None,
                reserved_usd: None,
                reserved_month: None,
                reservation_id: None,
            }])
            .unwrap();
        for persona in baseline_personas() {
            let scope = source_root.join(&persona).join("home/work/.kio");
            fs::create_dir_all(&scope).unwrap();
            let registry = source_root
                .join("env")
                .join(&persona)
                .join("xdg-data/kio/scope-registry.sqlite");
            fs::create_dir_all(registry.parent().unwrap()).unwrap();
            let connection = Connection::open(&registry).unwrap();
            connection
                .execute_batch("CREATE TABLE scopes (kio_path TEXT, root_path TEXT);")
                .unwrap();
            connection
                .execute(
                    "INSERT INTO scopes VALUES (?1, ?2)",
                    rusqlite::params![p01_kio.to_string_lossy(), p01_scope.to_string_lossy(),],
                )
                .unwrap();
        }

        let bound = RetainedDirectory::open(&source_root, "baseline fixture").unwrap();
        let digest = fixture_live_digest(&bound).unwrap();
        let (_temp, snapshot) = snapshot_baseline_fixture(&bound, &digest).unwrap();
        let snapshot_kio = snapshot.public_path.join("p01/home/work/.kio");
        assert_eq!(
            TaskStore::new(&snapshot_kio).all().unwrap()[0].output_ref,
            kio_pipeline::task::normalized_task_output_ref(&raw_hash, &tool_hash, 0),
        );
        assert_eq!(
            fs::read(p01_kio.join("tasks.jsonl")).unwrap(),
            fs::read(snapshot_kio.join("tasks.jsonl")).unwrap(),
        );
    }

    #[test]
    fn regular_tree_snapshot_rejects_changed_copy() {
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("evidence.txt"), b"before").unwrap();
        let snapshot = snapshot_regular_tree(source.path()).unwrap();
        fs::write(source.path().join("evidence.txt"), b"after").unwrap();
        assert!(snapshot.verify_source_unchanged().is_err());
    }
    #[cfg(unix)]
    #[test]
    fn controlled_environment_exposes_nested_retained_directory() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("nested")).unwrap();
        fs::write(dir.path().join("nested/proof"), b"bound").unwrap();
        let bound = RetainedDirectory::open(dir.path(), "environment").unwrap();
        let environment = ControlledEnvironment {
            fixed: vec![],
            directories: vec![(OsString::from("HOME"), bound)],
        };
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "test \"$(cat \"$HOME/nested/proof\")\" = bound"]);
        environment.apply(&mut command).unwrap();
        assert!(command.status().unwrap().success());
        environment.recheck_private_directories().unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn controlled_environment_rejects_replaced_private_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("environment");
        fs::create_dir(&path).unwrap();
        let bound = RetainedDirectory::open(&path, "environment").unwrap();
        fs::rename(&path, dir.path().join("retained")).unwrap();
        fs::create_dir(&path).unwrap();
        let environment = ControlledEnvironment {
            fixed: vec![],
            directories: vec![(OsString::from("HOME"), bound)],
        };
        assert!(environment.recheck_private_directories().is_err());
    }
    #[cfg(unix)]
    #[test]
    fn report_rejects_symlinked_parent() {
        let fixture = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();
        let report = QhardReport {
            schema_version: 1,
            benchmark: "test",
            measurement_class: "test",
            acceptance_eligible: false,
            blocked_reason: Some("test".into()),
            fixture: FixtureBinding {
                root: fixture.path().display().to_string(),
                tree: "t".into(),
                env_name: "e".into(),
                attestation: FileBinding {
                    path: "a".into(),
                    sha256: "b".into(),
                    bytes: 0,
                },
                live_sha256: "x".into(),
                scopes: vec![],
            },
            binary: FileBinding {
                path: "b".into(),
                sha256: "c".into(),
                bytes: 0,
            },
            golden: FileBinding {
                path: "g".into(),
                sha256: "h".into(),
                bytes: 0,
            },
            configuration: Configuration {
                k: 10,
                online_query: false,
                forwarded_credential_names: vec![],
            },
            rows: vec![],
            hits: 0,
            total: 8,
            recall_at_10: 0.0,
            synthetic_m3_1: None,
            combined_hits: None,
            combined_total: None,
        };
        assert!(
            write_report(
                &root.path().join("escape/report.json"),
                fixture.path(),
                &report
            )
            .is_err()
        );
    }
    #[test]
    fn missing_fixture_attestation_is_not_a_measurement() {
        let dir = tempfile::tempdir().unwrap();
        let options = QhardOptions {
            golden: dir.path().join("golden.jsonl"),
            fixture_root: dir.path().to_path_buf(),
            tree: "qhard".into(),
            env_name: "qhard".into(),
            attestation: None,
            bin: PathBuf::from("kio"),
            k: 10,
            online_query: false,
        };
        assert!(load_fixture(&options).is_err());
    }
    #[test]
    fn result_response_requires_evidence_pointer_and_k_bound() {
        assert_eq!(
            result_paths(
                r#"{"results":[{"evidence_pointer":{"scope_id":"scope-1","path_at_commit":"nested/doc.pdf"}}]}"#,
                10,
            )
            .unwrap(),
            vec!["scope-1:nested/doc.pdf"]
        );
        assert!(result_paths(r#"{"results":[{}]}"#, 10).is_err());
        let many = format!(
            r#"{{"results":[{}]}}"#,
            (0..11)
                .map(|_| r#"{"title":"x"}"#)
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(result_paths(&many, 10).is_err());
    }
}
