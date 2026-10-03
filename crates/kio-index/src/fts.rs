//! FTS5 external-content index contracts.

#[cfg(target_os = "linux")]
use std::cell::Cell;
#[cfg(target_os = "linux")]
use std::collections::BTreeMap;
use std::collections::BTreeSet;
#[cfg(unix)]
use std::ffi::CStr;
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::sync::Mutex;
#[cfg(unix)]
use std::sync::{Once, OnceLock};

use cap_primitives::fs as cap_fs;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::project_search_text;
use crate::{ChunkRow, IndexError, Result, chunking::validate_unit_hash};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FtsTokenizer {
    Trigram,
    Unicode61RemoveDiacritics2,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FtsSchemaConfig {
    pub tokenizer: FtsTokenizer,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct FtsMatch {
    chunk_id: String,
    rank: u64,
    bm25_score: f64,
}

/// Rows removed from the derived index for one purged raw object.
///
/// `chunk_ids` is sorted so callers can deterministically remove the matching
/// chunk CAS objects and durable-ledger records after this transaction commits.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PurgeRawIndexReport {
    pub chunk_ids: Vec<String>,
    pub deleted_chunks: u64,
    pub deleted_associations: u64,
    pub deleted_chunk_vectors: u64,
    /// `image_vec` rows removed (05 §3.5). Separate from
    /// `deleted_chunk_vectors` because the two are decided by different rules:
    /// a chunk vector goes with its chunk, an image vector only when no
    /// surviving chunk still references the image.
    pub deleted_image_vectors: u64,
    pub deleted_orphan_embeddings: u64,
    /// The `embeddings.id` of every orphan row just deleted — the CAS objects
    /// purge must remove alongside them (05 §3.5). A count cannot name a file,
    /// and the rows are gone by the time the caller could ask.
    pub deleted_embedding_ids: Vec<String>,
}

pub struct SqliteFtsIndex {
    conn: Connection,
    _source: Option<BoundSourceIndex>,
}

pub struct SourceIndexConnection {
    conn: Connection,
    _source: BoundSourceIndex,
}

/// One exact, derived-index candidate for Evidence retargeting.
///
/// This is deliberately only a row from the disposable SQLite projection.
/// Callers must authenticate every returned value against the target
/// manifest/chunk CAS before it can become Evidence output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetargetCandidate {
    pub chunk_id: String,
}

/// Bounded result of an exact Evidence retarget candidate lookup.
///
/// `Overflow` means SQLite produced more than [`RETARGET_CANDIDATE_LIMIT`]
/// exact rows. It is intentionally distinct from an empty or ambiguous row
/// set so the command surface can return its dedicated bounded-query error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetargetCandidates {
    Candidates(Vec<RetargetCandidate>),
    Overflow,
}

/// Maximum exact candidate rows Evidence retargeting may inspect.
pub const RETARGET_CANDIDATE_LIMIT: usize = 4096;
impl std::ops::Deref for SourceIndexConnection {
    type Target = Connection;
    fn deref(&self) -> &Self::Target {
        &self.conn
    }
}
impl std::ops::DerefMut for SourceIndexConnection {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.conn
    }
}
impl std::fmt::Debug for SourceIndexConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceIndexConnection")
            .finish_non_exhaustive()
    }
}

impl SourceIndexConnection {
    /// Recheck that the retained SQLite descriptor is still the file denoted
    /// by its capability-relative public leaf. This catches a replacement
    /// after the connection has opened; callers must treat failure as fatal.
    pub fn recheck_source_identity(&self) -> Result<()> {
        validate_bound_source_name_identity(&self._source)
    }

    /// Select only exact target-instance candidates from the derived index.
    ///
    /// There is no FTS, similarity, ranking, prefix, or normalization path
    /// here. The SQLite database merely supplies bounded candidates for a
    /// later CAS-authenticated reconstruction.
    pub fn exact_retarget_candidates(
        &self,
        raw_hash: &str,
        tool_profile_hash: &str,
        r#gen: u64,
    ) -> Result<RetargetCandidates> {
        let limit_plus_one = i64::try_from(RETARGET_CANDIDATE_LIMIT + 1)
            .expect("retarget candidate limit fits SQLite integer");
        let mut statement = self.conn.prepare(
            "SELECT chunk_id
             FROM chunks
             WHERE raw_hash = ?1 AND tool_profile_hash = ?2 AND gen = ?3
             ORDER BY chunk_id
             LIMIT ?4",
        )?;
        let rows = statement.query_map(
            params![raw_hash, tool_profile_hash, r#gen, limit_plus_one],
            |row| {
                Ok(RetargetCandidate {
                    chunk_id: row.get(0)?,
                })
            },
        )?;
        let candidates = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        if candidates.len() > RETARGET_CANDIDATE_LIMIT {
            return Ok(RetargetCandidates::Overflow);
        }
        Ok(RetargetCandidates::Candidates(candidates))
    }
}

/// Access mode for [`open_existing_source_index_connection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistingSourceIndexOpenMode {
    ReadOnly,
    ReadWrite,
}

/// Resolve an index parent before passing the untrusted leaf to SQLite.
///
/// `SQLITE_OPEN_NOFOLLOW` applies to every component SQLite receives.  The
/// parent therefore needs to be resolved separately: Kio must accept an
/// OS-owned ancestor symlink, but it must never follow a link installed at the
/// final `sqlite.db` component.
fn source_index_path_in_resolved_parent(path: &std::path::Path) -> Result<std::path::PathBuf> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    // The immediate `.kio/index` parent is repository-controlled. Do not
    // canonicalize through a replacement symlink here: unlike an OS-owned
    // ancestor such as `/var`, that would redirect a fresh sqlite.db bootstrap
    // into an attacker-selected directory.
    let parent_metadata = std::fs::symlink_metadata(parent).map_err(|error| {
        IndexError::Schema(format!(
            "inspect source index parent {}: {error}",
            parent.display()
        ))
    })?;
    if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
        return Err(IndexError::Schema(format!(
            "source index parent must be a real directory, not a symlink: {}",
            parent.display()
        )));
    }
    let file_name = path.file_name().ok_or_else(|| {
        IndexError::Schema(format!(
            "source index path has no file name: {}",
            path.display()
        ))
    })?;
    Ok(parent.join(file_name))
}

struct BoundSourceIndex {
    // Keep the repository-owned root (normally `.kio`) as well as `index`.
    // Opening `index` relative to this descriptor is what keeps a concurrent
    // replacement of `.kio` from changing the authority used for SQLite.
    _root: std::fs::File,
    _parent: std::fs::File,
    file: std::fs::File,
    public_path: PathBuf,
}

fn bind_source_index(
    path: &Path,
    writable: bool,
    create_missing: bool,
) -> Result<BoundSourceIndex> {
    let path = source_index_path_in_resolved_parent(path)?;
    let parent = path.parent().expect("source index has a parent");
    let root = parent.parent().filter(|root| !root.as_os_str().is_empty());
    let (root_handle, parent_handle) = if let Some(root) = root {
        // A descriptor-bound child index has already changed cwd to the
        // retained `.kio` directory, which is represented as `.`. Treat that
        // current directory as the root capability directly instead of trying
        // to split `.` into a public parent/leaf path.
        if root == Path::new(".") {
            let root_handle =
                cap_fs::open_ambient_dir(Path::new("."), cap_primitives::ambient_authority())
                    .map_err(|e| {
                        IndexError::Schema(format!("open descriptor-bound source index root: {e}"))
                    })?;
            let parent_leaf = parent.file_name().expect("source index parent has a leaf");
            let parent_handle = cap_fs::open_dir_nofollow(&root_handle, Path::new(parent_leaf))
                .map_err(|e| {
                    IndexError::Schema(format!(
                        "open source index parent {}: {e}",
                        parent.display()
                    ))
                })?;
            (root_handle, parent_handle)
        } else {
            let root_leaf = root.file_name().ok_or_else(|| {
                IndexError::Schema(format!(
                    "source index parent has no repository root component: {}",
                    parent.display()
                ))
            })?;
            let outer = root
                .parent()
                .filter(|outer| !outer.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let before_root = std::fs::symlink_metadata(root).map_err(|e| {
                IndexError::Schema(format!("inspect source index root {}: {e}", root.display()))
            })?;
            if before_root.file_type().is_symlink() || !before_root.is_dir() {
                return Err(IndexError::Schema(format!(
                    "source index root must be a real directory, not a symlink: {}",
                    root.display()
                )));
            }
            #[cfg(windows)]
            let before_root_identity = kio_core::cas::windows_real_directory_identity(root)
                .map_err(|e| {
                    IndexError::Schema(format!(
                        "inspect source index root reparse state {}: {e}",
                        root.display()
                    ))
                })?
                .ok_or_else(|| {
                    IndexError::Schema(format!(
                        "source index root must be a real directory, not a reparse point: {}",
                        root.display()
                    ))
                })?;
            let outer_handle = cap_fs::open_ambient_dir(outer, cap_primitives::ambient_authority())
                .map_err(|e| {
                    IndexError::Schema(format!(
                        "open source index root ancestor {}: {e}",
                        outer.display()
                    ))
                })?;
            let root_handle = cap_fs::open_dir_nofollow(&outer_handle, Path::new(root_leaf))
                .map_err(|e| {
                    IndexError::Schema(format!("open source index root {}: {e}", root.display()))
                })?;
            #[cfg(not(windows))]
            let opened_root = root_handle.metadata().map_err(|e| {
                IndexError::Schema(format!(
                    "inspect opened source index root {}: {e}",
                    root.display()
                ))
            })?;
            #[cfg(not(windows))]
            let root_matches = same_std_and_cap_directory(&before_root, &opened_root);
            #[cfg(windows)]
            let root_matches = kio_core::cas::windows_directory_handle_identity(&root_handle)
                == Some(before_root_identity);
            if !root_matches {
                return Err(IndexError::Schema(format!(
                    "source index root changed while opening: {}",
                    root.display()
                )));
            }
            let parent_leaf = parent.file_name().expect("source index parent has a leaf");
            let parent_handle = cap_fs::open_dir_nofollow(&root_handle, Path::new(parent_leaf))
                .map_err(|e| {
                    IndexError::Schema(format!(
                        "open source index parent {}: {e}",
                        parent.display()
                    ))
                })?;
            (root_handle, parent_handle)
        }
    } else {
        // A bare relative path has no repository-root component to bind. Keep
        // the old lstat/open/identity check for that API convenience case.
        #[cfg(not(windows))]
        let before = std::fs::symlink_metadata(parent).map_err(|e| {
            IndexError::Schema(format!(
                "inspect source index parent {}: {e}",
                parent.display()
            ))
        })?;
        #[cfg(windows)]
        let before_identity = kio_core::cas::windows_real_directory_identity(parent)
            .map_err(|e| {
                IndexError::Schema(format!(
                    "inspect source index parent reparse state {}: {e}",
                    parent.display()
                ))
            })?
            .ok_or_else(|| {
                IndexError::Schema(format!(
                    "source index parent must be a real directory, not a reparse point: {}",
                    parent.display()
                ))
            })?;
        let handle = cap_fs::open_ambient_dir(parent, cap_primitives::ambient_authority())
            .map_err(|e| {
                IndexError::Schema(format!(
                    "open source index parent {}: {e}",
                    parent.display()
                ))
            })?;
        #[cfg(not(windows))]
        let after = handle.metadata().map_err(|e| {
            IndexError::Schema(format!(
                "inspect opened source index parent {}: {e}",
                parent.display()
            ))
        })?;
        #[cfg(not(windows))]
        let parent_matches = same_std_and_cap_directory(&before, &after);
        #[cfg(windows)]
        let parent_matches =
            kio_core::cas::windows_directory_handle_identity(&handle) == Some(before_identity);
        if !parent_matches {
            return Err(IndexError::Schema(format!(
                "source index parent changed while opening: {}",
                parent.display()
            )));
        }
        (
            handle.try_clone().map_err(|e| {
                IndexError::Schema(format!(
                    "retain source index parent {}: {e}",
                    parent.display()
                ))
            })?,
            handle,
        )
    };
    let before = parent_handle.metadata().map_err(|e| {
        IndexError::Schema(format!(
            "inspect opened source index parent {}: {e}",
            parent.display()
        ))
    })?;
    if !before.is_dir() {
        return Err(IndexError::Schema(format!(
            "source index parent is not a directory: {}",
            parent.display()
        )));
    }
    let leaf = path.file_name().expect("source index has a leaf");
    let before_leaf = cap_source_leaf_identity(&parent_handle, Path::new(leaf))?;
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        .write(writable)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    #[cfg(windows)]
    {
        // SQLite's public-path open below cannot be passed this capability
        // handle. Keep this leaf handle open without FILE_SHARE_DELETE so a
        // concurrent rename/delete (and therefore path replacement) is denied
        // until SQLite has completed its own open and the connection closes.
        // cap-primitives gives directory handles this policy by default, but
        // ordinary file OpenOptions intentionally default to allowing delete.
        use cap_fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    }
    if create_missing && before_leaf.is_none() {
        options.create_new(true);
    }
    let file = cap_fs::open(&parent_handle, Path::new(leaf), &options)
        .map_err(|e| IndexError::Schema(format!("inspect source index {}: {e}", path.display())))?;
    validate_bound_source_file(&file, &path)?;
    if let Some(before_leaf) = before_leaf
        && source_file_identity(&file)? != before_leaf
    {
        return Err(IndexError::Schema(format!(
            "source index leaf changed while opening: {}",
            path.display()
        )));
    }
    Ok(BoundSourceIndex {
        _root: root_handle,
        _parent: parent_handle,
        file,
        public_path: path,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct SourceFileIdentity {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(windows)]
    volume_serial_number: Option<u32>,
    #[cfg(windows)]
    file_index: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct SourceFileState {
    identity: SourceFileIdentity,
    len: u64,
    modified_seconds: i64,
    modified_nanos: i64,
    changed_seconds: i64,
    changed_nanos: i64,
}

fn source_file_state_digest(state: SourceFileState) -> String {
    let mut digest = Sha256::new();
    digest.update(b"kio-gc-index-source-state-v1\0");
    #[cfg(unix)]
    {
        digest.update(state.identity.dev.to_be_bytes());
        digest.update(state.identity.ino.to_be_bytes());
    }
    #[cfg(windows)]
    {
        digest.update(
            state
                .identity
                .volume_serial_number
                .unwrap_or_default()
                .to_be_bytes(),
        );
        digest.update(state.identity.file_index.unwrap_or_default().to_be_bytes());
    }
    digest.update(state.len.to_be_bytes());
    digest.update(state.modified_seconds.to_be_bytes());
    digest.update(state.modified_nanos.to_be_bytes());
    digest.update(state.changed_seconds.to_be_bytes());
    digest.update(state.changed_nanos.to_be_bytes());
    let mut encoded = String::with_capacity(71);
    encoded.push_str("sha256:");
    for byte in digest.finalize() {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("write to String");
    }
    encoded
}

#[cfg(unix)]
fn source_file_state(file: &std::fs::File) -> Result<SourceFileState> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file
        .metadata()
        .map_err(|error| IndexError::Schema(format!("inspect GC source index state: {error}")))?;
    Ok(SourceFileState {
        identity: SourceFileIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        },
        len: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanos: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanos: metadata.ctime_nsec(),
    })
}

#[cfg(windows)]
fn source_file_state(file: &std::fs::File) -> Result<SourceFileState> {
    use cap_fs::_WindowsByHandle;
    use std::os::windows::{fs::MetadataExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandleEx,
    };
    let metadata = file
        .metadata()
        .map_err(|error| IndexError::Schema(format!("inspect GC source index state: {error}")))?;
    let by_handle = cap_fs::Metadata::from_file(file).map_err(|error| {
        IndexError::Schema(format!("inspect GC source index handle state: {error}"))
    })?;
    let mut basic: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileBasicInfo,
            (&mut basic as *mut FILE_BASIC_INFO).cast(),
            std::mem::size_of::<FILE_BASIC_INFO>() as u32,
        )
    } == 0
        || basic.ChangeTime < 0
    {
        return Err(IndexError::Schema(format!(
            "inspect GC source index Windows change time: {}",
            std::io::Error::last_os_error()
        )));
    }
    let modified = metadata.last_write_time();
    Ok(SourceFileState {
        identity: SourceFileIdentity {
            volume_serial_number: by_handle.volume_serial_number(),
            file_index: by_handle.file_index(),
        },
        len: metadata.file_size(),
        modified_seconds: i64::try_from(modified / 10_000_000).unwrap_or(i64::MAX),
        modified_nanos: i64::try_from((modified % 10_000_000) * 100).unwrap_or(i64::MAX),
        changed_seconds: basic.ChangeTime / 10_000_000,
        changed_nanos: (basic.ChangeTime % 10_000_000) * 100,
    })
}

#[cfg(not(any(unix, windows)))]
fn source_file_state(file: &std::fs::File) -> Result<SourceFileState> {
    let metadata = file
        .metadata()
        .map_err(|error| IndexError::Schema(format!("inspect GC source index state: {error}")))?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok());
    Ok(SourceFileState {
        identity: SourceFileIdentity {},
        len: metadata.len(),
        modified_seconds: modified
            .as_ref()
            .and_then(|value| i64::try_from(value.as_secs()).ok())
            .unwrap_or(-1),
        modified_nanos: modified
            .as_ref()
            .map_or(-1, |value| i64::from(value.subsec_nanos())),
        changed_seconds: -1,
        changed_nanos: -1,
    })
}

fn gc_leaf_state(parent: &std::fs::File, leaf: &str) -> Result<SourceFileState> {
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let file = cap_fs::open(parent, Path::new(leaf), &options)
        .map_err(|error| IndexError::Schema(format!("open GC index state {leaf}: {error}")))?;
    validate_bound_source_file(&file, Path::new(leaf))?;
    source_file_state(&file)
}
#[cfg(unix)]
fn cap_source_leaf_identity(
    parent: &std::fs::File,
    leaf: &Path,
) -> Result<Option<SourceFileIdentity>> {
    use cap_fs::MetadataExt;
    match cap_fs::stat(parent, leaf, cap_fs::FollowSymlinks::No) {
        Ok(metadata) if metadata.is_file() && metadata.nlink() == 1 => {
            Ok(Some(SourceFileIdentity {
                dev: metadata.dev(),
                ino: metadata.ino(),
            }))
        }
        Ok(metadata) if metadata.is_file() => Err(IndexError::Schema(format!(
            "source index target must have exactly one hard link (found {}): {}",
            metadata.nlink(),
            leaf.display()
        ))),
        Ok(_) => Err(IndexError::Schema(format!(
            "source index target is not a regular file: {}",
            leaf.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(IndexError::Schema(format!(
            "inspect source index {}: {error}",
            leaf.display()
        ))),
    }
}
#[cfg(windows)]
fn cap_source_leaf_identity(
    parent: &std::fs::File,
    leaf: &Path,
) -> Result<Option<SourceFileIdentity>> {
    use cap_fs::_WindowsByHandle;
    match cap_fs::stat(parent, leaf, cap_fs::FollowSymlinks::No) {
        Ok(metadata) if metadata.is_file() && metadata.number_of_links() == Some(1) => {
            Ok(Some(SourceFileIdentity {
                volume_serial_number: metadata.volume_serial_number(),
                file_index: metadata.file_index(),
            }))
        }
        Ok(metadata) if metadata.is_file() => Err(IndexError::Schema(format!(
            "source index target must have exactly one hard link (found {}): {}",
            metadata
                .number_of_links()
                .map_or_else(|| "unknown".to_owned(), |links| links.to_string()),
            leaf.display()
        ))),
        Ok(_) => Err(IndexError::Schema(format!(
            "source index target is not a regular file: {}",
            leaf.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(IndexError::Schema(format!(
            "inspect source index {}: {error}",
            leaf.display()
        ))),
    }
}
#[cfg(not(any(unix, windows)))]
fn cap_source_leaf_identity(
    _parent: &std::fs::File,
    _leaf: &Path,
) -> Result<Option<SourceFileIdentity>> {
    Err(IndexError::Schema(
        "source SQLite capability binding is unsupported on this platform".to_owned(),
    ))
}
#[cfg(unix)]
fn source_file_identity(file: &std::fs::File) -> Result<SourceFileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let m = file
        .metadata()
        .map_err(|e| IndexError::Schema(format!("inspect opened source index: {e}")))?;
    Ok(SourceFileIdentity {
        dev: m.dev(),
        ino: m.ino(),
    })
}
/// Return a stable, marker-safe representation of the primary SQLite file's
/// platform identity.  This intentionally has no pathname component: it is
/// the identity of the retained descriptor, and is compared with a fresh
/// descriptor-relative observation before every destructive GC step.
#[cfg(unix)]
fn canonical_gc_index_identity(file: &std::fs::File) -> Result<String> {
    let identity = source_file_identity(file)?;
    Ok(format!("unix:{:016x}:{:016x}", identity.dev, identity.ino))
}

#[cfg(windows)]
fn canonical_gc_index_identity(file: &std::fs::File) -> Result<String> {
    let identity = source_file_identity(file)?;
    let volume = identity.volume_serial_number.ok_or_else(|| {
        IndexError::Schema("GC source index has no Windows volume identity".to_owned())
    })?;
    let index = identity.file_index.ok_or_else(|| {
        IndexError::Schema("GC source index has no Windows file identity".to_owned())
    })?;
    Ok(format!("windows:{volume:08x}:{index:016x}"))
}

#[cfg(not(any(unix, windows)))]
fn canonical_gc_index_identity(_: &std::fs::File) -> Result<String> {
    Err(IndexError::Schema(
        "GC source SQLite identity is unsupported on this platform".to_owned(),
    ))
}

fn gc_leaf_identity(parent: &std::fs::File, leaf: &str) -> Result<String> {
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let file = cap_fs::open(parent, Path::new(leaf), &options)
        .map_err(|error| IndexError::Schema(format!("open GC index leaf {leaf}: {error}")))?;
    validate_bound_source_file(&file, Path::new(leaf))?;
    canonical_gc_index_identity(&file)
}

/// `cap_fs::open_dir_nofollow` intentionally retains Linux directories as
/// `O_PATH` capabilities.  That is the right authority for every name lookup
/// below it, but Linux rejects `fsync` on the retained descriptor.  Reopen
/// exactly `.` below that capability with read access before syncing; this
/// neither reconstructs nor trusts an ambient pathname.  Comparing the two
/// pinned identities makes an unexpected capability-wrapper regression fail
/// closed before any durability claim is made.
#[cfg(unix)]
fn sync_bound_gc_directory(directory: &std::fs::File, context: &str) -> Result<()> {
    let expected = source_file_identity(directory)?;
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let syncable = cap_fs::open(directory, Path::new("."), &options)
        .map_err(|error| IndexError::Schema(format!("{context}: {error}")))?;
    let metadata = syncable
        .metadata()
        .map_err(|error| IndexError::Schema(format!("{context}: {error}")))?;
    if !metadata.is_dir() || source_file_identity(&syncable)? != expected {
        return Err(IndexError::Schema(format!(
            "{context}: retained directory changed while reopening for fsync"
        )));
    }
    syncable
        .sync_all()
        .map_err(|error| IndexError::Schema(format!("{context}: {error}")))
}

/// Windows does not support flushing directory handles.  Validate the exact
/// retained capability is a real directory (never a reparse point), then rely
/// on the already-synced file contents and NTFS journaling for namespace
/// persistence; do not claim a POSIX directory-`fsync` guarantee here.
#[cfg(windows)]
fn sync_bound_gc_directory(directory: &std::fs::File, context: &str) -> Result<()> {
    if kio_core::cas::windows_directory_handle_identity(directory).is_none() {
        return Err(IndexError::Schema(format!(
            "{context}: retained directory is not a real Windows directory"
        )));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn sync_bound_gc_directory(_: &std::fs::File, context: &str) -> Result<()> {
    Err(IndexError::Schema(format!(
        "{context}: directory durability is unsupported on this platform"
    )))
}

fn validate_gc_temp_leaf(leaf: &str) -> Result<()> {
    if !leaf.starts_with(".gc-index-")
        || leaf.len() > 128
        || !leaf
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err(IndexError::Schema(
            "invalid private GC index name".to_owned(),
        ));
    }
    Ok(())
}

/// Open the operation-reserved namespace for private GC index copies.  This
/// deliberately lives below `gc/internal`, never beside the public index: a
/// crashed preparation must not leave an executable-looking database in the
/// public index directory.
fn open_gc_internal_index_dir(kio_dir: &std::fs::File) -> Result<std::fs::File> {
    fn open_or_create(parent: &std::fs::File, leaf: &str) -> Result<std::fs::File> {
        match cap_fs::open_dir_nofollow(parent, Path::new(leaf)) {
            Ok(dir) => Ok(dir),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let options = cap_fs::DirOptions::new();
                cap_fs::create_dir(parent, Path::new(leaf), &options).map_err(|error| {
                    IndexError::Schema(format!("create private GC index directory {leaf}: {error}"))
                })?;
                sync_bound_gc_directory(parent, &format!("fsync private GC index parent {leaf}"))?;
                cap_fs::open_dir_nofollow(parent, Path::new(leaf)).map_err(|error| {
                    IndexError::Schema(format!("open private GC index directory {leaf}: {error}"))
                })
            }
            Err(error) => Err(IndexError::Schema(format!(
                "open private GC index directory {leaf}: {error}"
            ))),
        }
    }
    let gc = open_or_create(kio_dir, "gc")?;
    let internal = open_or_create(&gc, "internal")?;
    open_or_create(&internal, "index")
}

fn open_existing_gc_internal_index_dir(kio_dir: &std::fs::File) -> Result<std::fs::File> {
    let gc = cap_fs::open_dir_nofollow(kio_dir, Path::new("gc")).map_err(|error| {
        IndexError::Schema(format!("open private GC index gc directory: {error}"))
    })?;
    let internal = cap_fs::open_dir_nofollow(&gc, Path::new("internal")).map_err(|error| {
        IndexError::Schema(format!("open private GC index internal directory: {error}"))
    })?;
    cap_fs::open_dir_nofollow(&internal, Path::new("index"))
        .map_err(|error| IndexError::Schema(format!("open private GC index directory: {error}")))
}

fn gc_private_dir_identity(dir: &std::fs::File) -> Result<String> {
    canonical_gc_index_identity(dir)
}

fn require_gc_private_dir_identity(dir: &std::fs::File, expected: &str) -> Result<()> {
    if gc_private_dir_identity(dir)? != expected {
        return Err(IndexError::Schema(
            "GC private index directory changed".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn exchange_gc_index_leaves(
    left_dir: &std::fs::File,
    left: &str,
    right_dir: &std::fs::File,
    right: &str,
) -> Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn renameatx_np(
            fromfd: libc::c_int,
            from: *const libc::c_char,
            tofd: libc::c_int,
            to: *const libc::c_char,
            flags: libc::c_uint,
        ) -> libc::c_int;
    }
    let left =
        CString::new(left).map_err(|_| IndexError::Schema("invalid GC index name".into()))?;
    let right =
        CString::new(right).map_err(|_| IndexError::Schema("invalid GC index name".into()))?;
    if unsafe {
        renameatx_np(
            left_dir.as_raw_fd(),
            left.as_ptr(),
            right_dir.as_raw_fd(),
            right.as_ptr(),
            2,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(IndexError::Schema(format!(
            "atomic GC index exchange: {}",
            std::io::Error::last_os_error()
        )))
    }
}
#[cfg(target_os = "linux")]
fn exchange_gc_index_leaves(
    left_dir: &std::fs::File,
    left: &str,
    right_dir: &std::fs::File,
    right: &str,
) -> Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    let left =
        CString::new(left).map_err(|_| IndexError::Schema("invalid GC index name".into()))?;
    let right =
        CString::new(right).map_err(|_| IndexError::Schema("invalid GC index name".into()))?;
    if unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            left_dir.as_raw_fd(),
            left.as_ptr(),
            right_dir.as_raw_fd(),
            right.as_ptr(),
            2_u32,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(IndexError::Schema(format!(
            "atomic GC index exchange: {}",
            std::io::Error::last_os_error()
        )))
    }
}
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn exchange_gc_index_leaves(_: &std::fs::File, _: &str, _: &std::fs::File, _: &str) -> Result<()> {
    Err(IndexError::Schema(
        "atomic GC index exchange is unsupported on this platform".to_owned(),
    ))
}
#[cfg(not(any(unix, windows)))]
fn source_file_identity(_: &std::fs::File) -> Result<SourceFileIdentity> {
    Ok(SourceFileIdentity {})
}
#[cfg(windows)]
fn source_file_identity(file: &std::fs::File) -> Result<SourceFileIdentity> {
    use cap_fs::_WindowsByHandle;
    let metadata = cap_fs::Metadata::from_file(file)
        .map_err(|e| IndexError::Schema(format!("inspect opened source index: {e}")))?;
    Ok(SourceFileIdentity {
        volume_serial_number: metadata.volume_serial_number(),
        file_index: metadata.file_index(),
    })
}

#[cfg(unix)]
fn same_std_and_cap_directory(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    before.dev() == after.dev() && before.ino() == after.ino()
}
#[cfg(not(any(unix, windows)))]
fn same_std_and_cap_directory(_: &std::fs::Metadata, _: &std::fs::Metadata) -> bool {
    false
}

fn validate_bound_source_file(file: &std::fs::File, path: &Path) -> Result<()> {
    let metadata = file.metadata().map_err(|e| {
        IndexError::Schema(format!(
            "inspect opened source index {}: {e}",
            path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(IndexError::Schema(format!(
            "source index target is not a regular file: {}",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(IndexError::Schema(format!(
                "source index target must have exactly one hard link (found {}): {}",
                metadata.nlink(),
                path.display()
            )));
        }
    }
    #[cfg(windows)]
    {
        use cap_fs::_WindowsByHandle;
        let by_handle = cap_fs::Metadata::from_file(file).map_err(|error| {
            IndexError::Schema(format!(
                "inspect opened source index handle {}: {error}",
                path.display()
            ))
        })?;
        if by_handle.number_of_links() != Some(1) {
            return Err(IndexError::Schema(format!(
                "source index target must have exactly one hard link (found {}): {}",
                by_handle
                    .number_of_links()
                    .map_or_else(|| "unknown".to_owned(), |links| links.to_string()),
                path.display()
            )));
        }
    }
    Ok(())
}
impl BoundSourceIndex {
    #[cfg(target_os = "linux")]
    fn sqlite_path(&self) -> PathBuf {
        use std::os::fd::AsRawFd;

        let leaf = self
            .public_path
            .file_name()
            .expect("bound source index has a file name");
        PathBuf::from(format!("/proc/self/fd/{}", self._parent.as_raw_fd())).join(leaf)
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    fn sqlite_path(&self) -> PathBuf {
        use std::os::fd::AsRawFd;
        PathBuf::from(format!("/dev/fd/{}", self.file.as_raw_fd()))
    }
    #[cfg(not(any(unix, windows)))]
    fn sqlite_path(&self) -> PathBuf {
        self.public_path.clone()
    }
}

#[cfg(unix)]
const BOUND_SOURCE_VFS_NAME: &CStr = c"kio-bound-source-unix";
#[cfg(unix)]
static BOUND_SOURCE_VFS_INIT: Once = Once::new();
#[cfg(unix)]
static BOUND_SOURCE_VFS_RESULT: OnceLock<std::result::Result<(), String>> = OnceLock::new();
#[cfg(unix)]
static BOUND_SOURCE_DEFAULT_VFS: OnceLock<usize> = OnceLock::new();
#[cfg(target_os = "linux")]
static BOUND_SOURCE_LINUX_OPEN: Mutex<()> = Mutex::new(());
#[cfg(target_os = "linux")]
std::thread_local! {
    static BOUND_SOURCE_LINUX_EXPECTED: Cell<Option<SourceFileIdentity>> = const { Cell::new(None) };
}

#[cfg(unix)]
fn open_bound_source_connection(source: &BoundSourceIndex, flags: OpenFlags) -> Result<Connection> {
    #[cfg(not(target_os = "linux"))]
    use std::os::unix::ffi::OsStrExt;

    let path = source.sqlite_path();
    #[cfg(not(target_os = "linux"))]
    if !is_bound_source_fd_name(path.as_os_str().as_bytes()) {
        return Err(IndexError::Schema(
            "bound source SQLite path is not an internal descriptor path".to_owned(),
        ));
    }
    BOUND_SOURCE_VFS_INIT.call_once(|| {
        let result = unsafe {
            let original = rusqlite::ffi::sqlite3_vfs_find(std::ptr::null());
            if original.is_null() {
                Err("SQLite has no default VFS".to_owned())
            } else {
                let _ = BOUND_SOURCE_DEFAULT_VFS.set(original as usize);
                let mut wrapped = Box::new(*original);
                wrapped.zName = BOUND_SOURCE_VFS_NAME.as_ptr();
                wrapped.xOpen = Some(bound_source_x_open);
                wrapped.xFullPathname = Some(bound_source_x_full_pathname);
                let code = rusqlite::ffi::sqlite3_vfs_register(Box::into_raw(wrapped), 0);
                if code == rusqlite::ffi::SQLITE_OK {
                    Ok(())
                } else {
                    Err(format!(
                        "register bound source SQLite VFS: SQLite error {code}"
                    ))
                }
            }
        };
        let _ = BOUND_SOURCE_VFS_RESULT.set(result);
    });
    BOUND_SOURCE_VFS_RESULT
        .get()
        .expect("VFS initializer sets result")
        .as_ref()
        .map_err(|e| IndexError::Schema(e.clone()))?;
    #[cfg(target_os = "linux")]
    {
        if !is_bound_source_linux_parent_fd_name(path.as_os_str().as_encoded_bytes()) {
            return Err(IndexError::Schema(
                "bound source SQLite path is not an internal descriptor-root path".to_owned(),
            ));
        }
        let _open_guard = BOUND_SOURCE_LINUX_OPEN.lock().map_err(|_| {
            IndexError::Schema("bound source SQLite open mutex is poisoned".to_owned())
        })?;
        let expected = source_file_identity(&source.file)?;
        BOUND_SOURCE_LINUX_EXPECTED.with(|slot| slot.set(Some(expected)));
        let outcome = Connection::open_with_flags_and_vfs(&path, flags, "kio-bound-source-unix");
        BOUND_SOURCE_LINUX_EXPECTED.with(|slot| slot.set(None));
        let conn = outcome?;
        Ok(conn)
    }
    #[cfg(not(target_os = "linux"))]
    Ok(Connection::open_with_flags_and_vfs(
        &path,
        flags,
        "kio-bound-source-unix",
    )?)
}
#[cfg(windows)]
mod windows;
#[cfg(windows)]
mod windows_gc;

#[cfg(windows)]
fn open_bound_source_connection(source: &BoundSourceIndex, flags: OpenFlags) -> Result<Connection> {
    windows::open(source, flags)
}

#[cfg(not(any(unix, windows)))]
fn open_bound_source_connection(source: &BoundSourceIndex, flags: OpenFlags) -> Result<Connection> {
    Ok(Connection::open_with_flags(source.sqlite_path(), flags)?)
}

#[cfg(unix)]
unsafe extern "C" fn bound_source_x_full_pathname(
    _: *mut rusqlite::ffi::sqlite3_vfs,
    name: *const std::ffi::c_char,
    output_len: std::ffi::c_int,
    output: *mut std::ffi::c_char,
) -> std::ffi::c_int {
    if name.is_null() || output.is_null() || output_len <= 0 {
        return rusqlite::ffi::SQLITE_CANTOPEN;
    }
    let bytes = unsafe { CStr::from_ptr(name).to_bytes() };
    if is_bound_source_fd_name(bytes)
        || (cfg!(target_os = "linux") && is_bound_source_linux_parent_fd_name(bytes))
    {
        if bytes.len() + 1 > output_len as usize {
            return rusqlite::ffi::SQLITE_CANTOPEN;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr().cast::<std::ffi::c_char>(),
                output,
                bytes.len(),
            );
            *output.add(bytes.len()) = 0;
        }
        return rusqlite::ffi::SQLITE_OK;
    }
    let Some(default_vfs) = BOUND_SOURCE_DEFAULT_VFS.get() else {
        return rusqlite::ffi::SQLITE_CANTOPEN;
    };
    let default_vfs = *default_vfs as *mut rusqlite::ffi::sqlite3_vfs;
    let Some(callback) = (unsafe { (*default_vfs).xFullPathname }) else {
        return rusqlite::ffi::SQLITE_CANTOPEN;
    };
    unsafe { callback(default_vfs, name, output_len, output) }
}

#[cfg(unix)]
unsafe extern "C" fn bound_source_x_open(
    _: *mut rusqlite::ffi::sqlite3_vfs,
    name: rusqlite::ffi::sqlite3_filename,
    file: *mut rusqlite::ffi::sqlite3_file,
    flags: std::ffi::c_int,
    out_flags: *mut std::ffi::c_int,
) -> std::ffi::c_int {
    let Some(default_vfs) = BOUND_SOURCE_DEFAULT_VFS.get() else {
        return rusqlite::ffi::SQLITE_CANTOPEN;
    };
    let default_vfs = *default_vfs as *mut rusqlite::ffi::sqlite3_vfs;
    let Some(callback) = (unsafe { (*default_vfs).xOpen }) else {
        return rusqlite::ffi::SQLITE_CANTOPEN;
    };

    #[cfg(target_os = "linux")]
    if !name.is_null()
        && is_bound_source_linux_parent_fd_name(unsafe { CStr::from_ptr(name).to_bytes() })
        && flags & rusqlite::ffi::SQLITE_OPEN_MAIN_DB != 0
    {
        let expected = BOUND_SOURCE_LINUX_EXPECTED.with(Cell::get);
        let Some(expected) = expected else {
            return rusqlite::ffi::SQLITE_CANTOPEN;
        };
        let Ok(before) = linux_regular_fd_inventory() else {
            return rusqlite::ffi::SQLITE_CANTOPEN;
        };
        let code = unsafe { callback(default_vfs, name, file, flags, out_flags) };
        if code != rusqlite::ffi::SQLITE_OK {
            return code;
        }
        let verified = linux_regular_fd_inventory().is_ok_and(|after| {
            let opened: Vec<_> = after
                .iter()
                .filter(|(fd, identity)| before.get(fd) != Some(*identity))
                .collect();
            // The main database must add exactly one descriptor for the
            // retained source. Concurrent same-process work may open other
            // regular files, which cannot grant authority over this source;
            // it is trusted in-process code. A second descriptor for this
            // identity is fail-closed, so another operation cannot obscure
            // which descriptor SQLite selected.
            opened
                .iter()
                .filter(|(_, identity)| **identity == expected)
                .count()
                == 1
        });
        if verified {
            return rusqlite::ffi::SQLITE_OK;
        }
        if !file.is_null() {
            let methods = unsafe { (*file).pMethods };
            if !methods.is_null()
                && let Some(close) = unsafe { (*methods).xClose }
            {
                let _ = unsafe { close(file) };
            }
        }
        return rusqlite::ffi::SQLITE_CANTOPEN;
    }

    unsafe { callback(default_vfs, name, file, flags, out_flags) }
}

#[cfg(unix)]
fn is_bound_source_fd_name(value: &[u8]) -> bool {
    value
        .strip_prefix(b"/dev/fd/")
        .is_some_and(|fd| !fd.is_empty() && fd.iter().all(u8::is_ascii_digit))
}

/// A Linux-only descriptor-root spelling.  This names a retained parent
/// directory and a single final leaf, so SQLite's unix VFS can apply
/// O_NOFOLLOW to the real repository-controlled leaf while retaining its
/// normal lock registry and close semantics.
#[cfg(unix)]
fn is_bound_source_linux_parent_fd_name(value: &[u8]) -> bool {
    let Some(value) = value.strip_prefix(b"/proc/self/fd/") else {
        return false;
    };
    let Some(separator) = value.iter().position(|byte| *byte == b'/') else {
        return false;
    };
    let (fd, leaf_with_separator) = value.split_at(separator);
    let leaf = &leaf_with_separator[1..];
    !fd.is_empty()
        && fd.iter().all(u8::is_ascii_digit)
        && !leaf.is_empty()
        && !leaf.contains(&b'/')
        && leaf != b"."
        && leaf != b".."
}

#[cfg(target_os = "linux")]
fn linux_regular_fd_inventory() -> Result<BTreeMap<i32, SourceFileIdentity>> {
    use std::os::unix::fs::MetadataExt;

    let entries = std::fs::read_dir("/proc/self/fd")
        .map_err(|e| IndexError::Schema(format!("inspect process descriptor inventory: {e}")))?;
    let mut result = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|e| {
            IndexError::Schema(format!("read process descriptor inventory entry: {e}"))
        })?;
        let Ok(fd) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Ok(metadata) = std::fs::metadata(entry.path()) else {
            continue;
        };
        if metadata.is_file() {
            result.insert(
                fd,
                SourceFileIdentity {
                    dev: metadata.dev(),
                    ino: metadata.ino(),
                },
            );
        }
    }
    Ok(result)
}

/// Open an existing, current source index without following its final path
/// component or creating a missing database.
///
/// This is for callers that need the raw SQLite connection rather than the FTS
/// wrapper. It validates the complete public schema before returning, so it
/// cannot accidentally adopt an empty, partial, or non-current `sqlite.db`.
pub fn open_existing_source_index_connection(
    path: impl AsRef<std::path::Path>,
    mode: ExistingSourceIndexOpenMode,
    config: &FtsSchemaConfig,
) -> Result<SourceIndexConnection> {
    crate::vec::ensure_registered();
    let source = bind_source_index(
        path.as_ref(),
        mode == ExistingSourceIndexOpenMode::ReadWrite,
        false,
    )?;

    let flags = match mode {
        ExistingSourceIndexOpenMode::ReadOnly => {
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW
        }
        ExistingSourceIndexOpenMode::ReadWrite => {
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW
        }
    };
    let conn = open_bound_source_connection(&source, flags)?;
    // The pre-open metadata check is not a lock. Recheck the visible leaf
    // after SQLite's NOFOLLOW lookup to catch the common replacement races
    // without making unsafe assumptions about raw file descriptors.
    validate_bound_source_file(&source.file, &source.public_path)?;
    if mode == ExistingSourceIndexOpenMode::ReadWrite {
        // Keep mutable raw-connection users subject to the same sidecar policy
        // as `SqliteFtsIndex::open`.
        conn.pragma_update(None, "journal_mode", "MEMORY")?;
    }
    if !validate_current_schema(&conn, config)? {
        return Err(schema_rebuild_error(
            "source index has no current index schema".to_owned(),
        ));
    }
    Ok(SourceIndexConnection {
        conn,
        _source: source,
    })
}

/// Read the metadata for the fixed `index/sqlite.db` leaf below a retained
/// `.kio` directory capability.  Unlike the public-path helper this never
/// re-resolves the scope or `.kio` pathname after GC has bound it.
///
/// `Ok(None)` means the leaf was absent at the descriptor-relative lookup;
/// callers that had already recorded a present index must treat that as a
/// fail-closed state transition rather than as an empty index.
pub fn read_bound_gc_index_metadata(
    kio_dir: &std::fs::File,
    config: &FtsSchemaConfig,
) -> Result<Option<BoundGcIndexMetadata>> {
    let Some((source, conn)) = open_bound_gc_index(kio_dir, config, false)? else {
        return Ok(None);
    };
    // Keep both the primary file and its directory capabilities alive for the
    // complete SQLite operation. In particular, `/dev/fd/N` is only safe while
    // `source.file` remains open.
    let metadata = read_index_metadata(&conn)?;
    validate_bound_source_name_identity(&source)?;
    let identity = canonical_gc_index_identity(&source.file)?;
    drop(conn);
    drop(source);
    Ok(metadata.map(|metadata| BoundGcIndexMetadata {
        metadata,
        file_identity: identity,
    }))
}

/// Metadata and the platform file identity observed from the exact primary
/// SQLite descriptor used by a descriptor-bound GC operation.  GC persists
/// this identity in its recovery marker so a same-generation replacement is
/// never accepted as the source index after tree retirement has started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundGcIndexMetadata {
    pub metadata: IndexMetadata,
    pub file_identity: String,
}

/// A durable, singleton statement written into a private GC index clone in
/// the *same SQLite transaction* that advances its generation.  The public
/// filename/inode binding lives in the GC marker; this record binds the
/// otherwise forgeable logical generation to that marker's frozen operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcIndexRotationAttestation {
    pub sweep_id: String,
    pub role: String,
    pub plan_digest: String,
    pub source_generation: String,
    pub target_generation: String,
}

/// A descriptor-relative, fully materialized replacement database.  The
/// caller must publish its `temp_leaf` and returned state in the GC marker
/// before calling [`exchange_prepared_bound_gc_index`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedBoundGcIndexRotation {
    pub temp_leaf: String,
    pub private_dir_identity: String,
    pub source: BoundGcIndexMetadata,
    pub source_state_digest: String,
    pub target: BoundGcIndexMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreparedGcIndexCleanup {
    Removed,
    AlreadyAbsent,
}

pub fn prepare_bound_gc_index_rotation(
    kio_dir: &std::fs::File,
    temp_leaf: &str,
    generation: &str,
    expected: (&str, &str),
    attestation: &GcIndexRotationAttestation,
    config: &FtsSchemaConfig,
) -> Result<PreparedBoundGcIndexRotation> {
    validate_gc_temp_leaf(temp_leaf)?;
    let source = read_bound_gc_index_metadata(kio_dir, config)?.ok_or_else(|| {
        IndexError::Schema("GC source index disappeared before durable rotation".to_owned())
    })?;
    if source.metadata.index_generation != expected.0 || source.file_identity != expected.1 {
        return Err(IndexError::Schema(format!(
            "GC source index changed before durable rotation (expected generation {}, identity {}; found generation {}, identity {})",
            expected.0, expected.1, source.metadata.index_generation, source.file_identity
        )));
    }
    let index = cap_fs::open_dir_nofollow(kio_dir, Path::new("index")).map_err(|error| {
        IndexError::Schema(format!(
            "open GC index directory for durable rotation: {error}"
        ))
    })?;
    let private = open_gc_internal_index_dir(kio_dir)?;
    let private_dir_identity = gc_private_dir_identity(&private)?;
    let mut source_options = cap_fs::OpenOptions::new();
    source_options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    #[cfg(windows)]
    {
        use cap_fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
        // Keep the copied source immutable until the final state recheck.
        source_options.share_mode(FILE_SHARE_READ);
    }
    let mut input = cap_fs::open(&index, Path::new("sqlite.db"), &source_options)
        .map_err(|error| IndexError::Schema(format!("open GC source index copy: {error}")))?;
    validate_bound_source_file(&input, Path::new("sqlite.db"))?;
    if canonical_gc_index_identity(&input)? != source.file_identity {
        return Err(IndexError::Schema(
            "GC source index changed before private copy".to_owned(),
        ));
    }
    let source_state = source_file_state(&input)?;
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create_new(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut output = cap_fs::open(&private, Path::new(temp_leaf), &options)
        .map_err(|error| IndexError::Schema(format!("create GC private index copy: {error}")))?;
    std::io::copy(&mut input, &mut output)
        .map_err(|error| IndexError::Schema(format!("copy GC private index: {error}")))?;
    wait_at_bound_gc_index_copy_barrier();
    output
        .sync_all()
        .map_err(|error| IndexError::Schema(format!("fsync GC private index copy: {error}")))?;
    drop(output);
    sync_bound_gc_directory(&private, "fsync private GC index directory")?;
    // The exact descriptor copied above and the public name must still agree
    // before SQLite is allowed to mutate the private clone.  This makes the
    // source generation/identity binding cover the whole copy interval.
    if source_file_state(&input)? != source_state
        || gc_leaf_state(&index, "sqlite.db")? != source_state
    {
        return Err(IndexError::Schema(
            "GC source index changed during private copy".to_owned(),
        ));
    }
    let target = rotate_bound_gc_index_leaf(
        &private,
        temp_leaf,
        generation,
        None,
        Some(attestation),
        config,
    )?
    .ok_or_else(|| IndexError::Schema("GC private index copy disappeared".to_owned()))?;
    sync_bound_gc_directory(&private, "fsync completed private GC index")?;
    Ok(PreparedBoundGcIndexRotation {
        temp_leaf: temp_leaf.to_owned(),
        private_dir_identity,
        source,
        source_state_digest: source_file_state_digest(source_state),
        target,
    })
}

#[cfg(debug_assertions)]
fn wait_at_bound_gc_index_copy_barrier() {
    // This is a mid-operation barrier, not an operation root. Only an
    // already-installed CLI snapshot may enable it.
    let control = kio_core::test_control::current_or_default();
    let Some(ready_path) = control.core.gc_index_copy_ready.as_deref() else {
        return;
    };
    if std::fs::write(ready_path, b"ready").is_err() {
        return;
    }
    let release_path = ready_path.with_extension("release");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !release_path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[cfg(not(debug_assertions))]
fn wait_at_bound_gc_index_copy_barrier() {}

/// Remove an operation-owned private index leaf after its source copy is no
/// longer needed.  The identity check makes this safe for recovery cleanup:
/// a substituted leaf is never unlinked merely because it has a GC-looking
/// name.  The directory fsync makes successful cleanup durable.
#[cfg(not(windows))]
pub fn remove_prepared_bound_gc_index(
    kio_dir: &std::fs::File,
    temp_leaf: &str,
    expected_private_dir_identity: &str,
    expected_identity: &str,
) -> Result<PreparedGcIndexCleanup> {
    validate_gc_temp_leaf(temp_leaf)?;
    let private = open_existing_gc_internal_index_dir(kio_dir)?;
    require_gc_private_dir_identity(&private, expected_private_dir_identity)?;
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let file = match cap_fs::open(&private, Path::new(temp_leaf), &options) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PreparedGcIndexCleanup::AlreadyAbsent);
        }
        Err(error) => {
            return Err(IndexError::Schema(format!(
                "open GC private index copy for cleanup: {error}"
            )));
        }
    };
    validate_bound_source_file(&file, Path::new(temp_leaf))?;
    let identity = canonical_gc_index_identity(&file)?;
    if gc_leaf_identity(&private, temp_leaf)? != identity {
        return Err(IndexError::Schema(
            "GC private index cleanup input changed".to_owned(),
        ));
    }
    if identity != expected_identity {
        return Err(IndexError::Schema(
            "GC private index cleanup input changed".to_owned(),
        ));
    }
    match cap_fs::remove_file(&private, Path::new(temp_leaf)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PreparedGcIndexCleanup::AlreadyAbsent);
        }
        Err(error) => {
            return Err(IndexError::Schema(format!(
                "remove GC private index copy: {error}"
            )));
        }
    }
    sync_bound_gc_directory(&private, "fsync private GC index cleanup")?;
    Ok(PreparedGcIndexCleanup::Removed)
}

#[cfg(windows)]
pub fn remove_prepared_bound_gc_index(
    kio_dir: &std::fs::File,
    temp_leaf: &str,
    expected_private_dir_identity: &str,
    expected_identity: &str,
) -> Result<PreparedGcIndexCleanup> {
    windows_gc::remove_prepared(
        kio_dir,
        temp_leaf,
        expected_private_dir_identity,
        expected_identity,
    )
}

/// Retire stale private rotation leaves before a new attempt.  Only bounded,
/// strict private names are considered; every candidate is re-opened
/// descriptor-relatively no-follow and required to remain a single-link
/// regular file before unlink.  The caller supplies the one marker-owned leaf
/// which must survive recovery.
pub fn cleanup_stale_bound_gc_index_rotations(
    kio_dir: &std::fs::File,
    keep_leaf: Option<&str>,
) -> Result<()> {
    const MAX_PRIVATE_GC_INDEX_LEAVES: usize = 32;
    const MAX_PRIVATE_GC_INDEX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
    #[cfg(windows)]
    windows_gc::ensure_no_pending(kio_dir)?;
    let private = open_gc_internal_index_dir(kio_dir)?;
    let mut stale = Vec::new();
    let mut bytes = 0_u64;
    let entries = cap_fs::read_dir(&private, Path::new("."))
        .map_err(|error| IndexError::Schema(format!("enumerate GC index cleanup: {error}")))?;
    for entry in entries {
        let entry = entry
            .map_err(|error| IndexError::Schema(format!("enumerate GC index cleanup: {error}")))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(".gc-index-") {
            continue;
        }
        validate_gc_temp_leaf(name)?;
        if Some(name) != keep_leaf {
            let mut options = cap_fs::OpenOptions::new();
            options
                .read(true)
                ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
            let file = cap_fs::open(&private, Path::new(name), &options).map_err(|error| {
                IndexError::Schema(format!("open stale GC index copy for bounds: {error}"))
            })?;
            validate_bound_source_file(&file, Path::new(name))?;
            bytes = bytes
                .checked_add(
                    file.metadata()
                        .map_err(|error| {
                            IndexError::Schema(format!(
                                "inspect stale GC index copy for bounds: {error}"
                            ))
                        })?
                        .len(),
                )
                .ok_or_else(|| {
                    IndexError::Schema("stale private GC index bytes overflow".to_owned())
                })?;
            if bytes > MAX_PRIVATE_GC_INDEX_BYTES {
                return Err(IndexError::Schema(
                    "too many stale private GC index bytes".to_owned(),
                ));
            }
            stale.push(name.to_owned());
            if stale.len() > MAX_PRIVATE_GC_INDEX_LEAVES {
                return Err(IndexError::Schema(
                    "too many stale private GC index copies".to_owned(),
                ));
            }
        }
    }
    for leaf in stale {
        let mut options = cap_fs::OpenOptions::new();
        options
            .read(true)
            ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
        let file = cap_fs::open(&private, Path::new(&leaf), &options)
            .map_err(|error| IndexError::Schema(format!("open stale GC index copy: {error}")))?;
        validate_bound_source_file(&file, Path::new(&leaf))?;
        let identity = canonical_gc_index_identity(&file)?;
        if gc_leaf_identity(&private, &leaf)? != identity {
            return Err(IndexError::Schema(
                "stale GC index copy changed during cleanup".to_owned(),
            ));
        }
        #[cfg(not(windows))]
        cap_fs::remove_file(&private, Path::new(&leaf))
            .map_err(|error| IndexError::Schema(format!("remove stale GC index copy: {error}")))?;
        #[cfg(windows)]
        windows_gc::remove_exact(&private, &leaf, &identity)?;
    }
    sync_bound_gc_directory(&private, "fsync private GC index stale cleanup")
}

#[cfg(not(windows))]
pub fn exchange_prepared_bound_gc_index(
    kio_dir: &std::fs::File,
    temp_leaf: &str,
    expected_private_dir_identity: &str,
    expected_source_identity: &str,
    expected_source_state_digest: &str,
    expected_target_identity: &str,
) -> Result<()> {
    validate_gc_temp_leaf(temp_leaf)?;
    let index = cap_fs::open_dir_nofollow(kio_dir, Path::new("index")).map_err(|error| {
        IndexError::Schema(format!("open GC index directory for exchange: {error}"))
    })?;
    let private = open_existing_gc_internal_index_dir(kio_dir)?;
    require_gc_private_dir_identity(&private, expected_private_dir_identity)?;
    let source = gc_leaf_identity(&index, "sqlite.db")?;
    let source_state = gc_leaf_state(&index, "sqlite.db")?;
    let temp = gc_leaf_identity(&private, temp_leaf)?;
    if source != expected_source_identity
        || source_file_state_digest(source_state) != expected_source_state_digest
        || temp != expected_target_identity
    {
        return Err(IndexError::Schema(
            "GC durable index exchange inputs changed".to_owned(),
        ));
    }
    exchange_gc_index_leaves(&index, "sqlite.db", &private, temp_leaf)?;
    // Exchange spans two directories.  Persist the destination namespace
    // first: after a power loss recovery may see both names, but never rely on
    // a source-directory fsync having made the target name durable first.
    sync_bound_gc_directory(&index, "fsync GC index exchange")?;
    sync_bound_gc_directory(&private, "fsync private GC index exchange")?;
    if gc_leaf_identity(&index, "sqlite.db")? != expected_target_identity
        || gc_leaf_identity(&private, temp_leaf)? != expected_source_identity
    {
        return Err(IndexError::Schema(
            "GC durable index exchange changed unexpectedly".to_owned(),
        ));
    }
    Ok(())
}

/// Finish a Windows index exchange whose journal is bound to the durable GC
/// rotation marker. A missing public index is admissible only when that exact
/// journal accounts for both files. This is namespace recovery, not SQLite
/// attestation: callers must revalidate the published generation and operation
/// attestation before retiring any tree.
#[cfg(windows)]
pub fn recover_prepared_bound_gc_index(
    kio_dir: &std::fs::File,
    temp_leaf: &str,
    expected_private_dir_identity: &str,
    expected_source_identity: &str,
    expected_source_state_digest: &str,
    expected_target_identity: &str,
) -> Result<bool> {
    windows_gc::recover(
        kio_dir,
        temp_leaf,
        expected_private_dir_identity,
        expected_source_identity,
        expected_source_state_digest,
        expected_target_identity,
    )
}

#[cfg(windows)]
pub fn exchange_prepared_bound_gc_index(
    kio_dir: &std::fs::File,
    temp_leaf: &str,
    expected_private_dir_identity: &str,
    expected_source_identity: &str,
    expected_source_state_digest: &str,
    expected_target_identity: &str,
) -> Result<()> {
    windows_gc::exchange(
        kio_dir,
        temp_leaf,
        expected_private_dir_identity,
        expected_source_identity,
        expected_source_state_digest,
        expected_target_identity,
    )
}

/// Rotate the fixed `index/sqlite.db` generation below a retained `.kio`
/// capability and return the metadata read from the *same pinned connection*
/// after the write commits.  No ambient pathname is accepted by this API.
#[cfg(test)]
fn rotate_bound_gc_index_generation(
    kio_dir: &std::fs::File,
    generation: &str,
    expected_current: Option<(&str, &str)>,
    config: &FtsSchemaConfig,
) -> Result<Option<BoundGcIndexMetadata>> {
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (kio_dir, generation, expected_current, config);
        unsupported_bound_gc_index_rotation()
    }
    #[cfg(any(unix, windows))]
    {
        let index = cap_fs::open_dir_nofollow(kio_dir, Path::new("index")).map_err(|error| {
            IndexError::Schema(format!(
                "open GC index directory below retained capability: {error}"
            ))
        })?;
        rotate_bound_gc_index_leaf(
            &index,
            "sqlite.db",
            generation,
            expected_current,
            None,
            config,
        )
    }
}

fn rotate_bound_gc_index_leaf(
    index: &std::fs::File,
    leaf: &str,
    generation: &str,
    expected_current: Option<(&str, &str)>,
    attestation: Option<&GcIndexRotationAttestation>,
    config: &FtsSchemaConfig,
) -> Result<Option<BoundGcIndexMetadata>> {
    let Some((source, mut conn)) = open_bound_gc_index_leaf(index, leaf, config, true)? else {
        return Ok(None);
    };
    let before = read_index_metadata(&conn)?.ok_or_else(|| {
        schema_rebuild_error("GC source index is missing index metadata".to_owned())
    })?;
    let before_identity = canonical_gc_index_identity(&source.file)?;
    if let Some((expected_generation, expected_identity)) = expected_current {
        if before.index_generation != expected_generation {
            return Err(IndexError::Schema(format!(
                "GC source index generation changed before rotation (expected {expected_generation}, found {})",
                before.index_generation
            )));
        }
        if before_identity != expected_identity {
            return Err(IndexError::Schema(
                "GC source index identity changed before rotation".to_owned(),
            ));
        }
    }
    if let Some(attestation) = attestation {
        if attestation.target_generation != generation
            || attestation.source_generation != before.index_generation
        {
            return Err(IndexError::Schema(
                "GC rotation attestation does not bind source and target generation".to_owned(),
            ));
        }
        write_gc_rotation_attestation(&mut conn, attestation, before.last_lifecycle_epoch)?;
    } else {
        rotate_index_generation(&conn, generation, before.last_lifecycle_epoch)?;
    }
    let after = read_index_metadata(&conn)?.ok_or_else(|| {
        IndexError::Schema("GC source index metadata disappeared during rotation".to_owned())
    })?;
    // The descriptor's own identity remains authoritative, but re-check its
    // shape after SQLite completes so a hardlink/replacement is never silently
    // accepted for a later operation.
    validate_bound_source_name_identity(&source)?;
    let after_identity = canonical_gc_index_identity(&source.file)?;
    if after_identity != before_identity {
        return Err(IndexError::Schema(
            "GC source index identity changed during rotation".to_owned(),
        ));
    }
    // SQLite must release its connection before the exact primary descriptor
    // is forced durable.  A directory fsync alone does not persist the
    // private database's changed pages on every filesystem.
    drop(conn);
    source.file.sync_all().map_err(|error| {
        IndexError::Schema(format!("fsync rotated GC index primary file: {error}"))
    })?;
    Ok(Some(BoundGcIndexMetadata {
        metadata: after,
        file_identity: after_identity,
    }))
}

/// Read and strictly validate the operation attestation from the exact
/// descriptor-bound public SQLite leaf. `None` is meaningful: a pre-GC index
/// has no operation authorization and must not be accepted for tree removal.
pub fn read_bound_gc_index_rotation_attestation(
    kio_dir: &std::fs::File,
    config: &FtsSchemaConfig,
) -> Result<
    Option<(
        BoundGcIndexMetadata,
        GcIndexRotationAttestation,
        std::fs::File,
    )>,
> {
    let Some((source, conn)) = open_bound_gc_index(kio_dir, config, false)? else {
        return Ok(None);
    };
    let metadata = read_index_metadata(&conn)?
        .ok_or_else(|| IndexError::Schema("GC attestation index has no metadata row".to_owned()))?;
    let attestation = read_gc_rotation_attestation(&conn)?;
    validate_bound_source_name_identity(&source)?;
    let identity = canonical_gc_index_identity(&source.file)?;
    let attested_file = source
        .file
        .try_clone()
        .map_err(|error| IndexError::Schema(format!("retain attested GC index: {error}")))?;
    Ok(attestation.map(|attestation| {
        (
            BoundGcIndexMetadata {
                metadata,
                file_identity: identity,
            },
            attestation,
            attested_file,
        )
    }))
}

fn write_gc_rotation_attestation(
    conn: &mut Connection,
    attestation: &GcIndexRotationAttestation,
    lifecycle_epoch: u64,
) -> Result<()> {
    if !valid_gc_rotation_attestation(attestation) {
        return Err(IndexError::Schema(
            "invalid GC rotation attestation fields".to_owned(),
        ));
    }
    let tx = conn.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS gc_rotation_attestation (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            version INTEGER NOT NULL CHECK (version = 1),
            sweep_id TEXT NOT NULL,
            role TEXT NOT NULL CHECK (role IN ('pre_sweep', 'final')),
            plan_digest TEXT NOT NULL,
            source_generation TEXT NOT NULL,
            target_generation TEXT NOT NULL
        )",
    )?;
    // The attestation is singleton and replaces any attestation from an older
    // rotation only in this clone. The generation advance and replacement are
    // committed together, so observers never see just one of the two.
    tx.execute(
        "INSERT INTO index_metadata (id, index_generation, last_lifecycle_epoch)
         VALUES (1, ?1, ?2)
         ON CONFLICT (id) DO UPDATE SET
             index_generation = excluded.index_generation,
             last_lifecycle_epoch = excluded.last_lifecycle_epoch",
        params![
            attestation.target_generation,
            i64::try_from(lifecycle_epoch).unwrap_or(i64::MAX)
        ],
    )?;
    tx.execute(
        "INSERT INTO gc_rotation_attestation
             (id, version, sweep_id, role, plan_digest, source_generation, target_generation)
         VALUES (1, 1, ?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (id) DO UPDATE SET
             version = excluded.version,
             sweep_id = excluded.sweep_id,
             role = excluded.role,
             plan_digest = excluded.plan_digest,
             source_generation = excluded.source_generation,
             target_generation = excluded.target_generation",
        params![
            attestation.sweep_id,
            attestation.role,
            attestation.plan_digest,
            attestation.source_generation,
            attestation.target_generation,
        ],
    )?;
    tx.commit()?;
    Ok(())
}

fn read_gc_rotation_attestation(conn: &Connection) -> Result<Option<GcIndexRotationAttestation>> {
    if !table_exists(conn, "gc_rotation_attestation")? {
        return Ok(None);
    }
    validate_optional_gc_rotation_attestation(conn)?;
    let rows = conn
        .prepare(
            "SELECT sweep_id, role, plan_digest, source_generation, target_generation
             FROM gc_rotation_attestation ORDER BY id",
        )?
        .query_map([], |row| {
            Ok(GcIndexRotationAttestation {
                sweep_id: row.get(0)?,
                role: row.get(1)?,
                plan_digest: row.get(2)?,
                source_generation: row.get(3)?,
                target_generation: row.get(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    match rows.as_slice() {
        [] => Err(IndexError::Schema(
            "GC rotation attestation table is empty".to_owned(),
        )),
        [one] if valid_gc_rotation_attestation(one) => Ok(Some(one.clone())),
        [..] if rows.len() == 1 => Err(IndexError::Schema(
            "GC rotation attestation has invalid fields".to_owned(),
        )),
        _ => Err(IndexError::Schema(
            "GC rotation attestation table is not singleton".to_owned(),
        )),
    }
}

fn valid_gc_rotation_attestation(attestation: &GcIndexRotationAttestation) -> bool {
    matches!(attestation.role.as_str(), "pre_sweep" | "final")
        && valid_gc_ulid(&attestation.sweep_id)
        && valid_gc_ulid(&attestation.source_generation)
        && valid_gc_ulid(&attestation.target_generation)
        && attestation.source_generation != attestation.target_generation
        && is_sha256_digest(&attestation.plan_digest)
}

fn valid_gc_ulid(value: &str) -> bool {
    value.len() == 26
        && value.bytes().all(|byte| {
            matches!(byte, b'0'..=b'9' | b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z')
        })
}

fn is_sha256_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value.as_bytes()[7..]
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn validate_optional_gc_rotation_attestation(conn: &Connection) -> Result<()> {
    if !table_exists(conn, "gc_rotation_attestation")? {
        return Ok(());
    }
    validate_table(
        conn,
        "gc_rotation_attestation",
        &[
            ("id", "INTEGER", false, 1),
            ("version", "INTEGER", true, 0),
            ("sweep_id", "TEXT", true, 0),
            ("role", "TEXT", true, 0),
            ("plan_digest", "TEXT", true, 0),
            ("source_generation", "TEXT", true, 0),
            ("target_generation", "TEXT", true, 0),
        ],
    )?;
    validate_exact_schema_sql(
        conn,
        "table",
        "gc_rotation_attestation",
        CURRENT_GC_ROTATION_ATTESTATION_SQL,
    )
}

/// Descriptor-relative, fixed-leaf variant of `bind_source_index` for GC.
/// The caller owns the already-bound `.kio` descriptor; this function only
/// opens `index` and `sqlite.db` beneath it with no symlink traversal.
fn open_bound_gc_index(
    kio_dir: &std::fs::File,
    config: &FtsSchemaConfig,
    writable: bool,
) -> Result<Option<(BoundSourceIndex, Connection)>> {
    // Unix descriptor binding and Windows actual-HANDLE verification both
    // prove SQLite adopted the retained primary file before pager access.
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (kio_dir, config, writable);
        unsupported_bound_gc_index_rotation()
    }
    #[cfg(any(unix, windows))]
    {
        crate::vec::ensure_registered();
        let root = kio_dir
            .try_clone()
            .map_err(|e| IndexError::Schema(format!("retain GC .kio capability: {e}")))?;
        let parent = match cap_fs::open_dir_nofollow(&root, Path::new("index")) {
            Ok(parent) => parent,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(IndexError::Schema(format!(
                    "open GC index directory below retained capability: {error}"
                )));
            }
        };
        open_bound_gc_index_leaf(&parent, "sqlite.db", config, writable)
    }
}

#[cfg(not(any(unix, windows)))]
fn unsupported_bound_gc_index_rotation<T>() -> Result<T> {
    Err(IndexError::Schema(
        "capability-bound GC SQLite rotation is unsupported on this platform".to_owned(),
    ))
}

fn open_bound_gc_index_leaf(
    parent: &std::fs::File,
    leaf: &str,
    config: &FtsSchemaConfig,
    writable: bool,
) -> Result<Option<(BoundSourceIndex, Connection)>> {
    let root = parent
        .try_clone()
        .map_err(|e| IndexError::Schema(format!("retain GC index capability: {e}")))?;
    let parent = parent
        .try_clone()
        .map_err(|e| IndexError::Schema(format!("retain GC index parent: {e}")))?;
    let public_path = PathBuf::from(leaf);
    let before = cap_source_leaf_identity(&parent, Path::new(leaf))?;
    let Some(before) = before else {
        return Ok(None);
    };
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        .write(writable)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    #[cfg(windows)]
    {
        use cap_fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
        // Writable SQLite connections need write sharing. Read-only GC
        // attestation handles and their retained clones must exclude writers.
        options.share_mode(FILE_SHARE_READ | if writable { FILE_SHARE_WRITE } else { 0 });
    }
    let file = cap_fs::open(&parent, Path::new(leaf), &options)
        .map_err(|error| IndexError::Schema(format!("open bound GC source index: {error}")))?;
    validate_bound_source_file(&file, &public_path)?;
    if source_file_identity(&file)? != before {
        return Err(IndexError::Schema(
            "GC source index changed while opening retained capability".to_owned(),
        ));
    }
    let source = BoundSourceIndex {
        _root: root,
        _parent: parent,
        file,
        public_path,
    };
    let flags = if writable {
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW
    };
    let conn = open_bound_source_connection(&source, flags)?;
    validate_bound_source_name_identity(&source)?;
    if writable {
        conn.pragma_update(None, "journal_mode", "MEMORY")?;
    }
    if !validate_current_schema(&conn, config)? {
        return Err(schema_rebuild_error(
            "GC source index has no current index schema".to_owned(),
        ));
    }
    validate_bound_source_name_identity(&source)?;
    Ok(Some((source, conn)))
}

/// Prove that the descriptor SQLite used is still what the retained
/// descriptor-relative `index/sqlite.db` name denotes.  A check of the open
/// descriptor alone is insufficient: an attacker can rename it away and put a
/// different regular database at the same name while the operation is in
/// flight.  Do this after every bound open/rotation before returning success.
fn validate_bound_source_name_identity(source: &BoundSourceIndex) -> Result<()> {
    validate_bound_source_file(&source.file, &source.public_path)?;
    let leaf = source.public_path.file_name().ok_or_else(|| {
        IndexError::Schema(format!(
            "bound source index has no file name: {}",
            source.public_path.display()
        ))
    })?;
    let named = cap_source_leaf_identity(&source._parent, Path::new(leaf))?.ok_or_else(|| {
        IndexError::Schema(format!(
            "bound source index disappeared while operating: {}",
            source.public_path.display()
        ))
    })?;
    if named != source_file_identity(&source.file)? {
        return Err(IndexError::Schema(format!(
            "bound source index name changed while operating: {}",
            source.public_path.display()
        )));
    }
    Ok(())
}

impl SqliteFtsIndex {
    pub fn open(path: impl AsRef<std::path::Path>, config: FtsSchemaConfig) -> Result<Self> {
        // The `vec0` module must be registered before the connection opens, else
        // the `chunk_vec` virtual table cannot be created or queried (04 §4.3).
        crate::vec::ensure_registered();
        let source = bind_source_index(path.as_ref(), true, true)?;
        // The source index is durable, but its SQLite journal is not an
        // independently validated Kio artifact.  Keep the rollback journal in
        // memory so SQLite never opens a `-journal`, `-wal`, or `-shm` sidecar
        // through a pathname that is not protected by this primary-file
        // `NOFOLLOW` open.  This is a derived database and can be rebuilt from
        // Kio's durable commit/tree/CAS sources if a process or power failure
        // interrupts a write; it deliberately does not promise crash recovery
        // from an on-disk SQLite journal.
        //
        // Resolve the parent first (rather than rejecting every ancestor
        // symlink): OS-owned directories such as `/var` can legitimately be
        // symlinked.  SQLite then performs the security-sensitive final lookup
        // with `SQLITE_OPEN_NOFOLLOW`, closing the validation/open race.
        let conn = open_bound_source_connection(
            &source,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        validate_bound_source_file(&source.file, &source.public_path)?;
        conn.pragma_update(None, "journal_mode", "MEMORY")?;
        ensure_schema_on_connection(&conn, config)?;
        Ok(Self {
            conn,
            _source: Some(source),
        })
    }

    #[cfg(test)]
    pub(crate) fn in_memory(config: FtsSchemaConfig) -> Result<Self> {
        crate::vec::ensure_registered();
        let conn = Connection::open_in_memory()?;
        ensure_schema_on_connection(&conn, config)?;
        Ok(Self {
            conn,
            _source: None,
        })
    }

    #[must_use]
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn index_chunk(&mut self, row: &ChunkRow) -> Result<()> {
        self.index_chunk_with_association_rowid(row, None)
            .map(|_| ())
    }

    /// Insert an immutable chunk row and append its chunking-config generation.
    ///
    /// Fresh indexing passes `None` and lets SQLite allocate the monotonically
    /// increasing association rowid. Durable-ledger replay may pass the recorded
    /// rowid so a rebuilt database preserves cursor ordering exactly.
    fn index_chunk_with_association_rowid(
        &mut self,
        row: &ChunkRow,
        association_rowid: Option<u64>,
    ) -> Result<u64> {
        self.index_chunk_with_rowids(row, None, association_rowid)
            .map(|(_, association_rowid)| association_rowid)
    }

    /// Replay one durable chunk/config ledger record with stable rowids.
    ///
    /// `chunk_rowid` is shared by every association record for the immutable
    /// chunk. Both explicit rowids are collision-checked inside one savepoint so
    /// a malformed ledger cannot partially publish either side of the relation.
    ///
    /// The association is the durable creation pair `(chunk_id,
    /// chunking_config_hash)`. Chunk-level publication events are the sole
    /// temporal relation and remain a separate, caller-driven concern — see
    /// [`record_chunk_publication`].
    pub fn index_chunk_with_rowids(
        &mut self,
        row: &ChunkRow,
        chunk_rowid: Option<u64>,
        association_rowid: Option<u64>,
    ) -> Result<(u64, u64)> {
        validate_unit_hash("unit_content_hash", &row.unit_content_hash)?;
        if chunk_rowid == Some(0) {
            return Err(IndexError::Contract(
                "chunk rowid must be positive".to_owned(),
            ));
        }
        // Q4: the trigram tokenizer stops at a NUL byte, so any text after a
        // U+0000 (e.g. a UTF-16-LE `.txt` decoded lossily keeps interleaved NULs)
        // would be silently unsearchable even though `index` reported success.
        // Strip NULs from the value bound into the external-content `text` column
        // (which feeds `chunk_fts`) so the whole chunk is tokenizable. Identity /
        // evidence are untouched: `chunk_id`, `text_hash`, `byte_start/end` and the
        // persisted `chunks.jsonl` / normalized markdown all still carry the
        // original bytes — only this derived search index projection is sanitized.
        //
        // F2: normalize the projection to NFC first. The trigram tokenizer is not
        // Unicode-normalizing, so NFD content (common on macOS/APFS, some IMEs, and
        // OCR/PDF extraction) would be silently unsearchable by an NFC query and
        // vice versa. The CLI query path is normalized to the same NFC form, so
        // canonically-equivalent content and queries match regardless of input
        // form. This is a derived-index projection only; the char offsets that
        // evidence resolves against remain over the original `row.text`.
        //
        // F3: resolve Markdown escapes last, over text that is already NFC and
        // already free of NULs so that neither can sit between a backslash and
        // the character it escapes. 07 §5.2.1 has provider raw text escaped
        // maximally on the way in, which puts a backslash in front of every
        // ASCII punctuation character, so a recovered `期限 7/10` is stored as
        // `期限 7\/10` and the query `7/10` never even becomes a candidate. See
        // `search_projection` for why code is exempted rather than unescaped
        // along with everything else.
        let indexed_text = project_search_text(&row.text);
        with_savepoint(&self.conn, "kio_index_chunk", || {
            let requested_chunk_rowid = chunk_rowid.map(sql_rowid).transpose()?;
            let existing_chunk_rowid = self
                .conn
                .query_row(
                    "SELECT rowid FROM chunks WHERE chunk_id = ?1",
                    params![row.chunk_id],
                    |result| result.get::<_, i64>(0),
                )
                .optional()?;
            let actual_chunk_rowid = match existing_chunk_rowid {
                Some(existing) => {
                    if requested_chunk_rowid.is_some_and(|requested| requested != existing) {
                        return Err(IndexError::Contract(format!(
                            "chunk {} has rowid {existing}, not requested rowid {}",
                            row.chunk_id,
                            requested_chunk_rowid.expect("checked as some")
                        )));
                    }
                    existing
                }
                None => {
                    let heading_path =
                        serde_json::to_string(&row.heading_path.clone().unwrap_or_default())?;
                    if let Some(requested) = requested_chunk_rowid {
                        let occupied = self
                            .conn
                            .query_row(
                                "SELECT chunk_id FROM chunks WHERE rowid = ?1",
                                params![requested],
                                |result| result.get::<_, String>(0),
                            )
                            .optional()?;
                        if let Some(occupied) = occupied {
                            return Err(IndexError::Contract(format!(
                                "chunk rowid {requested} is already occupied by {occupied}"
                            )));
                        }
                        self.conn.execute(
                            "INSERT INTO chunks(
                                rowid, chunk_id, raw_hash, tool_profile_hash, gen, unit_key,
                                unit_content_hash,
                                raw_path, heading_path, section_id, byte_start, byte_end,
                                text_hash, text, created_at
                             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                            params![
                                requested,
                                row.chunk_id,
                                row.raw_hash,
                                row.tool_profile_hash,
                                row.r#gen,
                                row.unit_key,
                                row.unit_content_hash,
                                row.raw_path,
                                heading_path,
                                row.section_id,
                                row.byte_start,
                                row.byte_end,
                                row.text_hash,
                                indexed_text,
                                row.created_at,
                            ],
                        )?;
                        requested
                    } else {
                        self.conn.execute(
                            "INSERT INTO chunks(
                                chunk_id, raw_hash, tool_profile_hash, gen, unit_key,
                                unit_content_hash,
                                raw_path, heading_path, section_id, byte_start, byte_end,
                                text_hash, text, created_at
                             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                            params![
                                row.chunk_id,
                                row.raw_hash,
                                row.tool_profile_hash,
                                row.r#gen,
                                row.unit_key,
                                row.unit_content_hash,
                                row.raw_path,
                                heading_path,
                                row.section_id,
                                row.byte_start,
                                row.byte_end,
                                row.text_hash,
                                indexed_text,
                                row.created_at,
                            ],
                        )?;
                        self.conn.last_insert_rowid()
                    }
                }
            };
            let association_rowid = record_chunk_config_association(
                &self.conn,
                &row.chunk_id,
                &row.chunking_config_hash,
                &row.created_at,
                association_rowid,
            )?;
            Ok((sql_u64_rowid(actual_chunk_rowid)?, association_rowid))
        })
    }

    /// Transactionally remove every derived-index row owned by `raw_hash`.
    ///
    /// Embeddings are keyed by normalized text rather than raw objects, so an
    /// embedding is removed only when no surviving chunk references its text
    /// hash. `tree_entries` is intentionally untouched: immutable commit/tree
    /// history is governed by the purge tombstone/barrier rather than rewritten.
    pub fn purge_raw(
        &mut self,
        raw_hash: &str,
        orphaned_image_hashes: &BTreeSet<String>,
    ) -> Result<PurgeRawIndexReport> {
        with_savepoint(&self.conn, "kio_purge_raw", || {
            let targets = {
                let mut statement = self.conn.prepare(
                    "SELECT chunk_id, text_hash
                     FROM chunks
                     WHERE raw_hash = ?1
                     ORDER BY chunk_id",
                )?;
                let rows = statement.query_map(params![raw_hash], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                rows.collect::<std::result::Result<Vec<_>, _>>()?
            };

            let mut report = PurgeRawIndexReport {
                chunk_ids: targets
                    .iter()
                    .map(|(chunk_id, _)| chunk_id.clone())
                    .collect(),
                ..PurgeRawIndexReport::default()
            };
            let text_hashes = targets
                .iter()
                .map(|(_, text_hash)| text_hash.clone())
                .collect::<BTreeSet<_>>();

            for (chunk_id, _) in &targets {
                report.deleted_chunk_vectors += u64::try_from(self.conn.execute(
                    "DELETE FROM chunk_vec WHERE chunk_id = ?1",
                    params![chunk_id],
                )?)
                .map_err(|_| IndexError::Contract("deleted row count exceeds u64".to_owned()))?;
                report.deleted_associations += u64::try_from(self.conn.execute(
                    "DELETE FROM chunk_config_generations WHERE chunk_id = ?1",
                    params![chunk_id],
                )?)
                .map_err(|_| IndexError::Contract("deleted row count exceeds u64".to_owned()))?;
            }

            report.deleted_chunks = u64::try_from(
                self.conn
                    .execute("DELETE FROM chunks WHERE raw_hash = ?1", params![raw_hash])?,
            )
            .map_err(|_| IndexError::Contract("deleted row count exceeds u64".to_owned()))?;

            for text_hash in text_hashes {
                // RETURNING the ids rather than only counting them: the CAS
                // object under `objects/embeddings/<id>` is the same vector and
                // purge has to take it too, but nothing can enumerate the rows
                // once they are deleted.
                let mut orphans = self.conn.prepare(
                    "DELETE FROM embeddings
                     WHERE target_type = 'chunk'
                       AND target_id = ?1
                       AND NOT EXISTS (
                           SELECT 1 FROM chunks WHERE text_hash = ?1 LIMIT 1
                       )
                     RETURNING id",
                )?;
                let ids = orphans.query_map(params![text_hash], |row| row.get::<_, String>(0))?;
                for id in ids {
                    report.deleted_embedding_ids.push(id?);
                    report.deleted_orphan_embeddings += 1;
                }
            }

            // 05 §3.5: image vectors go the same way, on the same
            // live-reference-0 rule. Which images are orphaned is decided by
            // the CALLER, because the answer lives in the Markdown image
            // grammar (`kio://…/object/image/…`) and that parser belongs to
            // kio-search — a second copy of it here is the kind of duplicate
            // liveness rule that drifts. The caller computes
            // "referenced by the purge target, and by nothing that survives"
            // before any row is deleted, and this deletes them inside the same
            // savepoint so the two halves cannot land apart.
            for image_hash in orphaned_image_hashes {
                report.deleted_image_vectors += u64::try_from(self.conn.execute(
                    "DELETE FROM image_vec WHERE image_id = ?1",
                    params![image_hash],
                )?)
                .map_err(|_| IndexError::Contract("deleted row count exceeds u64".to_owned()))?;
                let mut orphans = self.conn.prepare(
                    "DELETE FROM embeddings
                     WHERE target_type = 'image' AND target_id = ?1
                     RETURNING id",
                )?;
                let ids = orphans.query_map(params![image_hash], |row| row.get::<_, String>(0))?;
                for id in ids {
                    report.deleted_embedding_ids.push(id?);
                    report.deleted_orphan_embeddings += 1;
                }
            }

            Ok(report)
        })
    }

    /// Schema/tokenizer contract probe: a bare `chunk_fts MATCH` over the whole
    /// table, used by the CT3-FTS unit tests to pin the external-content
    /// trigger sync and trigram behavior. The production query path is
    /// kio-cli's `execute_fts_tier`, which layers the liveness filters
    /// (tree_entries join, current chunking_config_hash, `rowid <= max_rowid`)
    /// and column-weighted BM25 on the same index.
    #[cfg(test)]
    fn search(&self, query: &str, limit: u64) -> Result<Vec<FtsMatch>> {
        if query.chars().count() < 2 {
            return Ok(Vec::new());
        }
        let sql = "SELECT c.chunk_id, rank
                   FROM chunk_fts f
                   JOIN chunks c ON c.rowid = f.rowid
                   WHERE chunk_fts MATCH ?1
                   ORDER BY rank, c.chunk_id
                   LIMIT ?2";
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params![query, limit], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        })?;
        let mut matches = Vec::new();
        for (index, row) in rows.enumerate() {
            let (chunk_id, bm25_score) = row?;
            matches.push(FtsMatch {
                chunk_id,
                rank: index as u64 + 1,
                bm25_score,
            });
        }
        Ok(matches)
    }
}

/// Append a chunk/config generation association and return its stable rowid.
///
/// The `(chunk_id, chunking_config_hash)` creation relation is idempotent.
/// Temporal/history visibility is represented only by `chunk_publications`.
/// When an explicit rowid is supplied (during durable-ledger rebuild), the pair
/// and rowid must agree with an
/// existing record; a collision is a contract error rather than a silent
/// renumbering that could invalidate signed cursors.
fn record_chunk_config_association(
    conn: &Connection,
    chunk_id: &str,
    chunking_config_hash: &str,
    created_at: &str,
    association_rowid: Option<u64>,
) -> Result<u64> {
    if association_rowid == Some(0) {
        return Err(IndexError::Contract(
            "chunk/config association rowid must be positive".to_owned(),
        ));
    }
    let chunk_exists = conn
        .query_row(
            "SELECT 1 FROM chunks WHERE chunk_id = ?1 LIMIT 1",
            params![chunk_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !chunk_exists {
        return Err(IndexError::Contract(format!(
            "cannot associate missing chunk {chunk_id}"
        )));
    }

    let requested_rowid = association_rowid.map(sql_rowid).transpose()?;
    let existing_for_pair = conn
        .query_row(
            "SELECT association_rowid
             FROM chunk_config_generations
             WHERE chunk_id = ?1
               AND chunking_config_hash = ?2",
            params![chunk_id, chunking_config_hash],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;

    if let Some(existing_rowid) = existing_for_pair {
        if let Some(requested_rowid) = requested_rowid
            && existing_rowid != requested_rowid
        {
            return Err(IndexError::Contract(format!(
                "chunk/config association {chunk_id}/{chunking_config_hash} \
                 has rowid {existing_rowid}, not requested rowid {requested_rowid}"
            )));
        }
        return sql_u64_rowid(existing_rowid);
    }

    if let Some(requested_rowid) = requested_rowid {
        let occupied = conn
            .query_row(
                "SELECT chunk_id, chunking_config_hash
                 FROM chunk_config_generations
                 WHERE association_rowid = ?1",
                params![requested_rowid],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        if let Some((occupied_chunk, occupied_config)) = occupied {
            return Err(IndexError::Contract(format!(
                "chunk/config association rowid {requested_rowid} is already occupied by \
                 {occupied_chunk}/{occupied_config}"
            )));
        }
        conn.execute(
            "INSERT INTO chunk_config_generations(
                association_rowid, chunk_id, chunking_config_hash, created_at
             ) VALUES (?1, ?2, ?3, ?4)",
            params![requested_rowid, chunk_id, chunking_config_hash, created_at],
        )?;
        return sql_u64_rowid(requested_rowid);
    }

    conn.execute(
        "INSERT INTO chunk_config_generations(
            chunk_id, chunking_config_hash, created_at
         ) VALUES (?1, ?2, ?3)",
        params![chunk_id, chunking_config_hash, created_at],
    )?;
    sql_u64_rowid(conn.last_insert_rowid())
}

/// Maximum generation-association rowid frozen into a page-1 cursor.
/// Empty databases use zero, which cannot name an AUTOINCREMENT row.
pub fn max_chunk_config_association_rowid(conn: &Connection) -> Result<u64> {
    let maximum = conn.query_row(
        "SELECT COALESCE(MAX(association_rowid), 0) FROM chunk_config_generations",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    sql_u64_rowid(maximum)
}

/// Whether a chunk had an association with the effective config at the frozen
/// association maximum.
#[cfg(test)]
fn chunk_has_current_config_association(
    conn: &Connection,
    chunk_id: &str,
    chunking_config_hash: &str,
    max_association_rowid: u64,
) -> Result<bool> {
    let max_association_rowid = sql_rowid(max_association_rowid)?;
    Ok(conn
        .query_row(
            "SELECT 1
             FROM chunk_config_generations
             WHERE chunk_id = ?1
               AND chunking_config_hash = ?2
               AND association_rowid <= ?3
             LIMIT 1",
            params![chunk_id, chunking_config_hash, max_association_rowid],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Return chunks satisfying the shared row/config cursor eligibility filter.
/// Snapshot/tree liveness is intentionally layered on by the caller because it
/// differs between default, `--at`, all-history, and include-deleted modes.
#[cfg(test)]
fn current_config_eligible_chunk_ids(
    conn: &Connection,
    chunking_config_hash: &str,
    max_chunk_rowid: u64,
    max_association_rowid: u64,
) -> Result<BTreeSet<String>> {
    let max_chunk_rowid = sql_rowid(max_chunk_rowid)?;
    let max_association_rowid = sql_rowid(max_association_rowid)?;
    let mut stmt = conn.prepare(
        "SELECT c.chunk_id
         FROM chunks c
         JOIN chunk_config_generations g ON g.chunk_id = c.chunk_id
         WHERE c.rowid <= ?1
           AND g.chunking_config_hash = ?2
           AND g.association_rowid <= ?3
           AND EXISTS (
               SELECT 1 FROM chunk_publications p
               WHERE p.chunk_id = c.chunk_id
                 AND p.chunking_config_hash = g.chunking_config_hash
           )
         ORDER BY c.chunk_id",
    )?;
    let rows = stmt.query_map(
        params![max_chunk_rowid, chunking_config_hash, max_association_rowid],
        |row| row.get::<_, String>(0),
    )?;
    rows.collect::<std::result::Result<BTreeSet<_>, _>>()
        .map_err(IndexError::from)
}

/// PC37 (04 §4.1 / 05 §1.6): append one authenticated
/// `(chunk_id, chunking_config_hash, introduction_commit)` publication row.
/// The triple is idempotent, while distinct introductions for one exact
/// association accumulate (merge side branches and independent imports).
pub fn record_chunk_publication(
    conn: &Connection,
    chunk_id: &str,
    chunking_config_hash: &str,
    introduction_commit: &str,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO chunk_publications(chunk_id, chunking_config_hash, introduction_commit)
         VALUES (?1, ?2, ?3)",
        params![chunk_id, chunking_config_hash, introduction_commit],
    )?;
    Ok(())
}

/// Every recorded introduction commit for one exact chunk/config association,
/// in byte order. Empty means that association is ineligible; callers must not
/// fall back to a chunk-wide creation marker or another config's publication.
pub fn chunk_publication_introductions(
    conn: &Connection,
    chunk_id: &str,
    chunking_config_hash: &str,
) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT introduction_commit FROM chunk_publications
         WHERE chunk_id = ?1 AND chunking_config_hash = ?2
         ORDER BY introduction_commit",
    )?;
    let rows = stmt.query_map(params![chunk_id, chunking_config_hash], |row| {
        row.get::<_, String>(0)
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(IndexError::from)
}

fn ensure_schema_on_connection(conn: &Connection, config: FtsSchemaConfig) -> Result<()> {
    // A derived database is disposable, but it is not a migration target.  Do
    // this read-only fingerprint before *any* DDL: a partial or older sqlite.db
    // must be rebuilt from the durable commit/tree/CAS sources, never repaired
    // in place.  Besides making the boundary explicit, this keeps the failed
    // open byte-for-byte non-mutating for `kio repair rebuild-db` to replace.
    let has_current_objects = validate_current_schema(conn, &config)?;
    if has_current_objects {
        return Ok(());
    }
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS chunks (
            -- QB29 (step4b-contract-tests-p3b.md §C, 04 §4.1 L385-386 /
            -- 03-data-model.md §8, U98): a rowid table's `TEXT PRIMARY KEY`
            -- does NOT imply NOT NULL by itself — spelled out explicitly.
            chunk_id TEXT NOT NULL PRIMARY KEY,
            raw_hash TEXT NOT NULL,
            tool_profile_hash TEXT NOT NULL,
            gen INTEGER NOT NULL,
            unit_key TEXT NOT NULL,
            unit_content_hash TEXT NOT NULL CHECK (
                length(unit_content_hash) = 71
                AND substr(unit_content_hash, 1, 7) = 'sha256:'
                AND substr(unit_content_hash, 8) NOT GLOB '*[^0-9a-f]*'
            ),
            raw_path TEXT NOT NULL,
            heading_path TEXT NOT NULL,
            section_id TEXT,
            byte_start INTEGER NOT NULL,
            byte_end INTEGER NOT NULL,
            text_hash TEXT NOT NULL,
            text TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_chunks_ident
            ON chunks(raw_hash, tool_profile_hash, gen, unit_key, unit_content_hash);
        CREATE TABLE IF NOT EXISTS chunk_config_generations (
            association_rowid INTEGER PRIMARY KEY AUTOINCREMENT,
            chunk_id TEXT NOT NULL,
            chunking_config_hash TEXT NOT NULL,
            created_at TEXT NOT NULL,
            UNIQUE(chunk_id, chunking_config_hash)
        );
        CREATE TABLE IF NOT EXISTS chunk_publications (
            publication_rowid INTEGER PRIMARY KEY AUTOINCREMENT,
            chunk_id TEXT NOT NULL,
            chunking_config_hash TEXT NOT NULL,
            introduction_commit TEXT NOT NULL,
            UNIQUE(chunk_id, chunking_config_hash, introduction_commit)
        );
        CREATE INDEX IF NOT EXISTS idx_chunk_publications_chunk_id
            ON chunk_publications(chunk_id, chunking_config_hash);
        CREATE TABLE IF NOT EXISTS embeddings (
            -- QB29: see the `chunks.chunk_id` comment above — same rowid-table
            -- TEXT PRIMARY KEY nullability gap, closed explicitly.
            id TEXT NOT NULL PRIMARY KEY,
            target_type TEXT NOT NULL,
            target_id TEXT NOT NULL,
            modality TEXT NOT NULL,
            vector BLOB NOT NULL,
            dimensions INTEGER NOT NULL,
            distance TEXT NOT NULL,
            profile_hash TEXT NOT NULL,
            -- 2026-07-24 (07 §5.3 contextual-embedding addendum): the humanized
            -- filename context a chunk vector was embedded with, so a rebuild
            -- can disambiguate several rows sharing one `target_id` (text_hash).
            -- NULL for non-contextual symbolic-name chunk embeddings.
            context_key TEXT
        );
        -- Keep target-type lookups bounded without scanning the corpus-sized
        -- `embeddings` table.
        CREATE INDEX IF NOT EXISTS idx_embeddings_type ON embeddings(target_type);
        CREATE TABLE IF NOT EXISTS tree_entries (
            commit_hash TEXT NOT NULL,
            path TEXT NOT NULL,
            raw_hash TEXT NOT NULL,
            tool_profile_hash TEXT,
            gen INTEGER,
            manifest_hash TEXT,
            PRIMARY KEY (commit_hash, path)
        );
        CREATE INDEX IF NOT EXISTS idx_tree_entries_ident
            ON tree_entries(commit_hash, raw_hash, tool_profile_hash, gen);
        CREATE TABLE IF NOT EXISTS index_metadata (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            index_generation TEXT NOT NULL,
            last_lifecycle_epoch INTEGER NOT NULL DEFAULT 0
        );
        "#,
    )?;

    let tokenizer = match config.tokenizer {
        FtsTokenizer::Trigram => "trigram",
        FtsTokenizer::Unicode61RemoveDiacritics2 => "unicode61 remove_diacritics 2",
    };
    conn.execute_batch(&format!(
        r#"
        CREATE VIRTUAL TABLE IF NOT EXISTS chunk_fts
        USING fts5(text, heading_path, content='chunks', content_rowid='rowid', tokenize='{tokenizer}');

        CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
            INSERT INTO chunk_fts(rowid, text, heading_path)
            VALUES (new.rowid, new.text, new.heading_path);
        END;

        CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
            INSERT INTO chunk_fts(chunk_fts, rowid, text, heading_path)
            VALUES ('delete', old.rowid, old.text, old.heading_path);
        END;

        CREATE TRIGGER IF NOT EXISTS chunks_au AFTER UPDATE OF text, heading_path ON chunks BEGIN
            INSERT INTO chunk_fts(chunk_fts, rowid, text, heading_path)
            VALUES ('delete', old.rowid, old.text, old.heading_path);
            INSERT INTO chunk_fts(rowid, text, heading_path)
            VALUES (new.rowid, new.text, new.heading_path);
        END;
        "#
    ))?;
    conn.execute("INSERT INTO chunk_fts(chunk_fts) VALUES('rebuild')", [])?;

    // `chunk_vec` is a sqlite-vec `vec0` virtual table (04 §4.3): the KNN
    // acceleration layer derived from the `embeddings` table. Fixed at the adopted
    // profile's 768 dimensions / cosine distance (07 §5.3). Since our stored and
    // query vectors are L2-normalized, cosine distance ordering is exact.
    crate::vec::ensure_registered();
    conn.execute_batch(&format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS chunk_vec USING vec0(
            chunk_id TEXT PRIMARY KEY,
            embedding float[{CHUNK_VEC_DIMENSIONS}] distance_metric=cosine
        );"
    ))?;
    // `image_vec` is `chunk_vec`'s counterpart for image objects (04 §4.3).
    // `embeddings.target_type` has admitted `'image'` since it was written, but
    // with no vec0 table to hold them there was no way to search one.
    //
    // Same width and metric on purpose: 03 §7 fixes ONE multimodal vector
    // space, so image and chunk vectors are directly comparable and the split
    // is only sqlite-vec's one-primary-key-type-per-table constraint, not a
    // semantic boundary. `image_id` is the `objects/image/` content hash.
    conn.execute_batch(&format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS image_vec USING vec0(
            image_id TEXT PRIMARY KEY,
            embedding float[{CHUNK_VEC_DIMENSIONS}] distance_metric=cosine
        );"
    ))?;
    Ok(())
}

/// Verify a pre-existing derived-index schema without modifying it.
///
/// `Ok(false)` means this connection contains no Kio index objects and can be
/// initialized as a fresh store. `Ok(true)` means every required current object
/// matches the public fingerprint. Any partial, non-current, or incompatible Kio
/// shape returns [`IndexError::Schema`] with rebuild guidance. Callers that
/// intentionally open SQLite directly (notably repair) can use this check
/// before selecting from the database.
fn validate_current_schema(conn: &Connection, config: &FtsSchemaConfig) -> Result<bool> {
    const REQUIRED_OBJECTS: &[&str] = &[
        "chunks",
        "chunk_config_generations",
        "chunk_publications",
        "embeddings",
        "tree_entries",
        "index_metadata",
        "chunk_fts",
        "chunk_vec",
        "image_vec",
        "idx_chunks_ident",
        "idx_chunk_publications_chunk_id",
        "idx_embeddings_type",
        "idx_tree_entries_ident",
        "chunks_ai",
        "chunks_ad",
        "chunks_au",
    ];
    let present = REQUIRED_OBJECTS
        .iter()
        .map(|name| object_exists(conn, name))
        .collect::<Result<Vec<_>>>()?;
    if present.iter().all(|present| !present) {
        if has_non_internal_user_objects(conn)? {
            return Err(schema_rebuild_error(
                "sqlite.db contains unknown user objects rather than a fresh index",
            ));
        }
        return Ok(false);
    }
    if present.iter().any(|present| !present) {
        return Err(schema_rebuild_error(
            "missing required current index object",
        ));
    }
    // GC's private-copy generation rotation may add one strict attestation
    // table. It is optional because only current rotation states create it;
    // when present it is validated below and is never treated as arbitrary
    // user schema.
    const OPTIONAL_GC_OBJECTS: &[&str] = &["gc_rotation_attestation"];
    validate_no_unknown_user_objects_with_optional(conn, REQUIRED_OBJECTS, OPTIONAL_GC_OBJECTS)?;

    validate_table(
        conn,
        "chunks",
        &[
            ("chunk_id", "TEXT", true, 1),
            ("raw_hash", "TEXT", true, 0),
            ("tool_profile_hash", "TEXT", true, 0),
            ("gen", "INTEGER", true, 0),
            ("unit_key", "TEXT", true, 0),
            ("unit_content_hash", "TEXT", true, 0),
            ("raw_path", "TEXT", true, 0),
            ("heading_path", "TEXT", true, 0),
            ("section_id", "TEXT", false, 0),
            ("byte_start", "INTEGER", true, 0),
            ("byte_end", "INTEGER", true, 0),
            ("text_hash", "TEXT", true, 0),
            ("text", "TEXT", true, 0),
            ("created_at", "TEXT", true, 0),
        ],
    )?;
    validate_exact_schema_sql(conn, "table", "chunks", CURRENT_CHUNKS_SQL)?;
    validate_table(
        conn,
        "chunk_config_generations",
        &[
            ("association_rowid", "INTEGER", false, 1),
            ("chunk_id", "TEXT", true, 0),
            ("chunking_config_hash", "TEXT", true, 0),
            ("created_at", "TEXT", true, 0),
        ],
    )?;
    validate_exact_schema_sql(
        conn,
        "table",
        "chunk_config_generations",
        CURRENT_CHUNK_CONFIG_GENERATIONS_SQL,
    )?;
    validate_table(
        conn,
        "chunk_publications",
        &[
            ("publication_rowid", "INTEGER", false, 1),
            ("chunk_id", "TEXT", true, 0),
            ("chunking_config_hash", "TEXT", true, 0),
            ("introduction_commit", "TEXT", true, 0),
        ],
    )?;
    validate_exact_schema_sql(
        conn,
        "table",
        "chunk_publications",
        CURRENT_CHUNK_PUBLICATIONS_SQL,
    )?;
    validate_table(
        conn,
        "embeddings",
        &[
            ("id", "TEXT", true, 1),
            ("target_type", "TEXT", true, 0),
            ("target_id", "TEXT", true, 0),
            ("modality", "TEXT", true, 0),
            ("vector", "BLOB", true, 0),
            ("dimensions", "INTEGER", true, 0),
            ("distance", "TEXT", true, 0),
            ("profile_hash", "TEXT", true, 0),
            ("context_key", "TEXT", false, 0),
        ],
    )?;
    validate_exact_schema_sql(conn, "table", "embeddings", CURRENT_EMBEDDINGS_SQL)?;
    validate_table(
        conn,
        "tree_entries",
        &[
            ("commit_hash", "TEXT", true, 1),
            ("path", "TEXT", true, 2),
            ("raw_hash", "TEXT", true, 0),
            ("tool_profile_hash", "TEXT", false, 0),
            ("gen", "INTEGER", false, 0),
            ("manifest_hash", "TEXT", false, 0),
        ],
    )?;
    validate_exact_schema_sql(conn, "table", "tree_entries", CURRENT_TREE_ENTRIES_SQL)?;
    let partial_tree_entry: bool = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM tree_entries
             WHERE NOT (
                 tool_profile_hash IS NULL AND gen IS NULL AND manifest_hash IS NULL
                 OR tool_profile_hash IS NOT NULL AND gen IS NOT NULL AND manifest_hash IS NOT NULL
             )
         )",
        [],
        |row| row.get(0),
    )?;
    if partial_tree_entry {
        return Err(schema_rebuild_error(
            "tree_entries has a partial normalize projection",
        ));
    }
    validate_table(
        conn,
        "index_metadata",
        &[
            ("id", "INTEGER", false, 1),
            ("index_generation", "TEXT", true, 0),
            ("last_lifecycle_epoch", "INTEGER", true, 0),
        ],
    )?;
    validate_exact_schema_sql(conn, "table", "index_metadata", CURRENT_INDEX_METADATA_SQL)?;
    validate_optional_gc_rotation_attestation(conn)?;
    validate_index(
        conn,
        "idx_chunks_ident",
        "chunks",
        &[
            "raw_hash",
            "tool_profile_hash",
            "gen",
            "unit_key",
            "unit_content_hash",
        ],
    )?;
    validate_exact_schema_sql(
        conn,
        "index",
        "idx_chunks_ident",
        CURRENT_IDX_CHUNKS_IDENT_SQL,
    )?;
    validate_index(
        conn,
        "idx_chunk_publications_chunk_id",
        "chunk_publications",
        &["chunk_id", "chunking_config_hash"],
    )?;
    validate_exact_schema_sql(
        conn,
        "index",
        "idx_chunk_publications_chunk_id",
        CURRENT_IDX_CHUNK_PUBLICATIONS_SQL,
    )?;
    validate_index(conn, "idx_embeddings_type", "embeddings", &["target_type"])?;
    validate_exact_schema_sql(
        conn,
        "index",
        "idx_embeddings_type",
        CURRENT_IDX_EMBEDDINGS_TYPE_SQL,
    )?;
    validate_index(
        conn,
        "idx_tree_entries_ident",
        "tree_entries",
        &["commit_hash", "raw_hash", "tool_profile_hash", "gen"],
    )?;
    validate_exact_schema_sql(
        conn,
        "index",
        "idx_tree_entries_ident",
        CURRENT_IDX_TREE_ENTRIES_IDENT_SQL,
    )?;
    let vector_column = format!("embedding float[{CHUNK_VEC_DIMENSIONS}] distance_metric=cosine");
    validate_virtual_table(
        conn,
        "chunk_fts",
        "fts5",
        &[
            "text",
            "heading_path",
            "content='chunks'",
            "content_rowid='rowid'",
            tokenizer_sql(config),
        ],
    )?;
    validate_exact_schema_sql(conn, "table", "chunk_fts", &current_chunk_fts_sql(config))?;
    validate_virtual_table(
        conn,
        "chunk_vec",
        "vec0",
        &["chunk_id text primary key", &vector_column],
    )?;
    validate_exact_schema_sql(
        conn,
        "table",
        "chunk_vec",
        &current_chunk_vec_sql("chunk_id"),
    )?;
    validate_virtual_table(
        conn,
        "image_vec",
        "vec0",
        &["image_id text primary key", &vector_column],
    )?;
    validate_exact_schema_sql(
        conn,
        "table",
        "image_vec",
        &current_chunk_vec_sql("image_id"),
    )?;
    validate_trigger(
        conn,
        "chunks_ai",
        &[
            "after insert on chunks",
            "insert into chunk_fts",
            "new.rowid",
            "new.text",
            "new.heading_path",
        ],
    )?;
    validate_exact_schema_sql(conn, "trigger", "chunks_ai", CURRENT_CHUNKS_AI_SQL)?;
    validate_trigger(
        conn,
        "chunks_ad",
        &[
            "after delete on chunks",
            "insert into chunk_fts",
            "'delete'",
            "old.rowid",
            "old.text",
            "old.heading_path",
        ],
    )?;
    validate_exact_schema_sql(conn, "trigger", "chunks_ad", CURRENT_CHUNKS_AD_SQL)?;
    validate_trigger(
        conn,
        "chunks_au",
        &[
            "after update of text, heading_path on chunks",
            "'delete'",
            "old.rowid",
            "new.rowid",
            "new.text",
            "new.heading_path",
        ],
    )?;
    validate_exact_schema_sql(conn, "trigger", "chunks_au", CURRENT_CHUNKS_AU_SQL)?;
    Ok(true)
}

/// `index_metadata`'s single row (04-pipeline.md §4.1 / Step4b LC42-45): the
/// search-cursor-generation ULID and the lifecycle-epoch value this row was
/// last synchronized against. `kio_core::purge` owns the counter files this
/// mirrors; this crate only stores the caller-supplied snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexMetadata {
    pub index_generation: String,
    pub last_lifecycle_epoch: u64,
}

/// The one `index_metadata` row, or `None` when the required current table is
/// present but its single row has not yet been initialized. A missing table is
/// a non-current derived database and must be rebuilt rather than interpreted.
pub fn read_index_metadata(conn: &Connection) -> Result<Option<IndexMetadata>> {
    if !table_exists(conn, "index_metadata")? {
        return Err(schema_rebuild_error(
            "missing required current index_metadata table",
        ));
    }
    Ok(conn
        .query_row(
            "SELECT index_generation, last_lifecycle_epoch FROM index_metadata WHERE id = 1",
            [],
            |row| {
                let last_lifecycle_epoch: i64 = row.get(1)?;
                Ok(IndexMetadata {
                    index_generation: row.get(0)?,
                    last_lifecycle_epoch: u64::try_from(last_lifecycle_epoch).unwrap_or(0),
                })
            },
        )
        .optional()?)
}

/// LC42: create the single `index_metadata` row only if absent — never
/// overwrites an existing row during a current store's first write-command
/// visit. `generation` is
/// the caller-minted ULID; `last_lifecycle_epoch` must be the *current*
/// `.kio/tombstones/lifecycle-epoch` counter value at the moment of this
/// call — never the column's own `DEFAULT 0`, which LC42 explicitly warns
/// would falsely read as a permanent rollback on every subsequent LC45
/// read-side check.
pub fn ensure_index_metadata(
    conn: &Connection,
    generation: &str,
    last_lifecycle_epoch: u64,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO index_metadata (id, index_generation, last_lifecycle_epoch)
         VALUES (1, ?1, ?2)",
        params![
            generation,
            i64::try_from(last_lifecycle_epoch).unwrap_or(i64::MAX)
        ],
    )?;
    Ok(())
}

/// LC25/LC44: unconditionally replace `index_metadata`'s row — a fresh
/// `index_generation` ULID (retiring every outstanding search cursor, LC25)
/// paired with the lifecycle-epoch value this rotation is now synchronized
/// to (LC44's post-rollback-recovery write). Callers hold `.kio/.lock` (a
/// write command) when calling this; never called from a read-only path.
pub fn rotate_index_generation(
    conn: &Connection,
    generation: &str,
    last_lifecycle_epoch: u64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO index_metadata (id, index_generation, last_lifecycle_epoch)
         VALUES (1, ?1, ?2)
         ON CONFLICT (id) DO UPDATE SET
             index_generation = excluded.index_generation,
             last_lifecycle_epoch = excluded.last_lifecycle_epoch",
        params![
            generation,
            i64::try_from(last_lifecycle_epoch).unwrap_or(i64::MAX)
        ],
    )?;
    Ok(())
}

fn table_exists(conn: &Connection, table_name: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type IN ('table', 'view') AND name = ?1 LIMIT 1",
            params![table_name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn object_exists(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE name = ?1 LIMIT 1",
            params![name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn has_non_internal_user_objects(conn: &Connection) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master
             WHERE type IN ('table', 'index', 'trigger', 'view')
               AND name NOT LIKE 'sqlite_%'
             LIMIT 1",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn validate_no_unknown_user_objects_with_optional(
    conn: &Connection,
    required: &[&str],
    optional: &[&str],
) -> Result<()> {
    let mut statement = conn.prepare(
        "SELECT type, name FROM sqlite_master
         WHERE name NOT LIKE 'sqlite_%' ORDER BY name",
    )?;
    let objects = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (object_type, name) in objects {
        if required.contains(&name.as_str()) || optional.contains(&name.as_str()) {
            continue;
        }
        let recognized_shadow = object_type == "table" && is_current_virtual_shadow(&name);
        if !recognized_shadow {
            return Err(schema_rebuild_error(format!(
                "unknown user object {name} is present in sqlite.db"
            )));
        }
    }
    Ok(())
}

fn is_current_virtual_shadow(name: &str) -> bool {
    [
        "chunk_fts_data",
        "chunk_fts_idx",
        "chunk_fts_docsize",
        "chunk_fts_config",
    ]
    .contains(&name)
        || ["chunk_vec", "image_vec"].iter().any(|base| {
            name == format!("{base}_info")
                || name == format!("{base}_rowids")
                || name == format!("{base}_chunks")
                || name == format!("{base}_vector_chunks00")
        })
}

fn schema_rebuild_error(detail: impl std::fmt::Display) -> IndexError {
    IndexError::Schema(format!(
        "incompatible derived sqlite.db ({detail}); run `kio repair rebuild-db`"
    ))
}

// `PRAGMA table_info` cannot observe AUTOINCREMENT, CHECK, UNIQUE, trigger
// WHEN clauses, or virtual-table options. These public definitions therefore
// form the current schema fingerprint; canonicalization below intentionally
// ignores only case, whitespace, and SQL comments.
const CURRENT_CHUNKS_SQL: &str = "CREATE TABLE chunks (chunk_id TEXT NOT NULL PRIMARY KEY, raw_hash TEXT NOT NULL, tool_profile_hash TEXT NOT NULL, gen INTEGER NOT NULL, unit_key TEXT NOT NULL, unit_content_hash TEXT NOT NULL CHECK (length(unit_content_hash) = 71 AND substr(unit_content_hash, 1, 7) = 'sha256:' AND substr(unit_content_hash, 8) NOT GLOB '*[^0-9a-f]*'), raw_path TEXT NOT NULL, heading_path TEXT NOT NULL, section_id TEXT, byte_start INTEGER NOT NULL, byte_end INTEGER NOT NULL, text_hash TEXT NOT NULL, text TEXT NOT NULL, created_at TEXT NOT NULL)";
const CURRENT_CHUNK_CONFIG_GENERATIONS_SQL: &str = "CREATE TABLE chunk_config_generations (association_rowid INTEGER PRIMARY KEY AUTOINCREMENT, chunk_id TEXT NOT NULL, chunking_config_hash TEXT NOT NULL, created_at TEXT NOT NULL, UNIQUE(chunk_id, chunking_config_hash))";
const CURRENT_CHUNK_PUBLICATIONS_SQL: &str = "CREATE TABLE chunk_publications (publication_rowid INTEGER PRIMARY KEY AUTOINCREMENT, chunk_id TEXT NOT NULL, chunking_config_hash TEXT NOT NULL, introduction_commit TEXT NOT NULL, UNIQUE(chunk_id, chunking_config_hash, introduction_commit))";
const CURRENT_EMBEDDINGS_SQL: &str = "CREATE TABLE embeddings (id TEXT NOT NULL PRIMARY KEY, target_type TEXT NOT NULL, target_id TEXT NOT NULL, modality TEXT NOT NULL, vector BLOB NOT NULL, dimensions INTEGER NOT NULL, distance TEXT NOT NULL, profile_hash TEXT NOT NULL, context_key TEXT)";
const CURRENT_TREE_ENTRIES_SQL: &str = "CREATE TABLE tree_entries (commit_hash TEXT NOT NULL, path TEXT NOT NULL, raw_hash TEXT NOT NULL, tool_profile_hash TEXT, gen INTEGER, manifest_hash TEXT, PRIMARY KEY (commit_hash, path))";
const CURRENT_INDEX_METADATA_SQL: &str = "CREATE TABLE index_metadata (id INTEGER PRIMARY KEY CHECK (id = 1), index_generation TEXT NOT NULL, last_lifecycle_epoch INTEGER NOT NULL DEFAULT 0)";
const CURRENT_GC_ROTATION_ATTESTATION_SQL: &str = "CREATE TABLE gc_rotation_attestation (id INTEGER PRIMARY KEY CHECK (id = 1), version INTEGER NOT NULL CHECK (version = 1), sweep_id TEXT NOT NULL, role TEXT NOT NULL CHECK (role IN ('pre_sweep', 'final')), plan_digest TEXT NOT NULL, source_generation TEXT NOT NULL, target_generation TEXT NOT NULL)";
const CURRENT_IDX_CHUNKS_IDENT_SQL: &str = "CREATE INDEX idx_chunks_ident ON chunks(raw_hash, tool_profile_hash, gen, unit_key, unit_content_hash)";
const CURRENT_IDX_CHUNK_PUBLICATIONS_SQL: &str = "CREATE INDEX idx_chunk_publications_chunk_id ON chunk_publications(chunk_id, chunking_config_hash)";
const CURRENT_IDX_EMBEDDINGS_TYPE_SQL: &str =
    "CREATE INDEX idx_embeddings_type ON embeddings(target_type)";
const CURRENT_IDX_TREE_ENTRIES_IDENT_SQL: &str = "CREATE INDEX idx_tree_entries_ident ON tree_entries(commit_hash, raw_hash, tool_profile_hash, gen)";
const CURRENT_CHUNKS_AI_SQL: &str = "CREATE TRIGGER chunks_ai AFTER INSERT ON chunks BEGIN INSERT INTO chunk_fts(rowid, text, heading_path) VALUES (new.rowid, new.text, new.heading_path); END";
const CURRENT_CHUNKS_AD_SQL: &str = "CREATE TRIGGER chunks_ad AFTER DELETE ON chunks BEGIN INSERT INTO chunk_fts(chunk_fts, rowid, text, heading_path) VALUES ('delete', old.rowid, old.text, old.heading_path); END";
const CURRENT_CHUNKS_AU_SQL: &str = "CREATE TRIGGER chunks_au AFTER UPDATE OF text, heading_path ON chunks BEGIN INSERT INTO chunk_fts(chunk_fts, rowid, text, heading_path) VALUES ('delete', old.rowid, old.text, old.heading_path); INSERT INTO chunk_fts(rowid, text, heading_path) VALUES (new.rowid, new.text, new.heading_path); END";

fn current_chunk_fts_sql(config: &FtsSchemaConfig) -> String {
    format!(
        "CREATE VIRTUAL TABLE chunk_fts USING fts5(text, heading_path, content='chunks', content_rowid='rowid', tokenize='{}')",
        match config.tokenizer {
            FtsTokenizer::Trigram => "trigram",
            FtsTokenizer::Unicode61RemoveDiacritics2 => "unicode61 remove_diacritics 2",
        }
    )
}

fn current_chunk_vec_sql(id_column: &str) -> String {
    format!(
        "CREATE VIRTUAL TABLE {} USING vec0({id_column} TEXT PRIMARY KEY, embedding float[{CHUNK_VEC_DIMENSIONS}] distance_metric=cosine)",
        if id_column == "chunk_id" {
            "chunk_vec"
        } else {
            "image_vec"
        }
    )
}

fn validate_exact_schema_sql(
    conn: &Connection,
    object_type: &str,
    name: &str,
    expected: &str,
) -> Result<()> {
    let actual: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = ?1 AND name = ?2",
            params![object_type, name],
            |row| row.get(0),
        )
        .optional()?;
    if actual
        .as_deref()
        .is_none_or(|actual| canonical_sql(actual) != canonical_sql(expected))
    {
        return Err(schema_rebuild_error(format!(
            "{object_type} {name} definition does not match current schema"
        )));
    }
    Ok(())
}

fn validate_table(
    conn: &Connection,
    table: &str,
    expected: &[(&str, &str, bool, i64)],
) -> Result<()> {
    let quoted = format!("'{}'", table.replace('\'', "''"));
    let mut statement = conn.prepare(&format!("PRAGMA table_info({quoted})"))?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?.to_ascii_uppercase(),
            row.get::<_, i64>(3)? != 0,
            row.get::<_, i64>(5)?,
        ))
    })?;
    let actual = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    if actual.len() != expected.len()
        || actual
            .iter()
            .zip(expected)
            .enumerate()
            .any(|(index, (actual, expected))| {
                actual.0 != index as i64
                    || actual.1 != expected.0
                    || actual.2 != expected.1
                    || actual.3 != expected.2
                    || actual.4 != expected.3
            })
    {
        return Err(schema_rebuild_error(format!(
            "{table} columns do not match current schema"
        )));
    }
    Ok(())
}

fn validate_index(conn: &Connection, index: &str, table: &str, columns: &[&str]) -> Result<()> {
    let actual_table: Option<String> = conn
        .query_row(
            "SELECT tbl_name FROM sqlite_master WHERE type = 'index' AND name = ?1",
            params![index],
            |row| row.get(0),
        )
        .optional()?;
    if actual_table.as_deref() != Some(table) {
        return Err(schema_rebuild_error(format!(
            "missing required index {index}"
        )));
    }
    let quoted = format!("'{}'", index.replace('\'', "''"));
    let mut statement = conn.prepare(&format!("PRAGMA index_info({quoted})"))?;
    let actual = statement
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(2)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if actual.len() != columns.len()
        || actual
            .iter()
            .zip(columns)
            .enumerate()
            .any(|(position, (actual, expected))| {
                actual.0 != position as i64 || actual.1 != *expected
            })
    {
        return Err(schema_rebuild_error(format!(
            "index {index} does not match current schema"
        )));
    }
    Ok(())
}

fn validate_virtual_table(
    conn: &Connection,
    table: &str,
    module: &str,
    fragments: &[&str],
) -> Result<()> {
    let sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![table],
            |row| row.get(0),
        )
        .optional()?;
    let sql = sql
        .map(|sql| canonical_sql(&sql))
        .ok_or_else(|| schema_rebuild_error(format!("missing virtual table {table}")))?;
    if !sql.contains(&canonical_sql(&format!(
        "create virtual table {table} using {module}"
    ))) || fragments
        .iter()
        .any(|fragment| !sql.contains(&canonical_sql(fragment)))
    {
        return Err(schema_rebuild_error(format!(
            "virtual table {table} does not match current schema"
        )));
    }
    Ok(())
}

fn validate_trigger(conn: &Connection, trigger: &str, fragments: &[&str]) -> Result<()> {
    let sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = ?1",
            params![trigger],
            |row| row.get(0),
        )
        .optional()?;
    let sql = sql
        .map(|sql| canonical_sql(&sql))
        .ok_or_else(|| schema_rebuild_error(format!("missing required trigger {trigger}")))?;
    if fragments
        .iter()
        .any(|fragment| !sql.contains(&canonical_sql(fragment)))
    {
        return Err(schema_rebuild_error(format!(
            "trigger {trigger} does not match current schema"
        )));
    }
    Ok(())
}

fn canonical_sql(sql: &str) -> String {
    sql.lines()
        .map(|line| line.split_once("--").map_or(line, |(before, _)| before))
        .collect::<String>()
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<String>()
}

#[cfg(test)]
fn table_has_column(conn: &Connection, table_name: &str, column_name: &str) -> Result<bool> {
    let quoted = format!("'{}'", table_name.replace('\'', "''"));
    let mut statement = conn.prepare(&format!("PRAGMA table_info({quoted})"))?;
    let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
    Ok(columns
        .collect::<std::result::Result<Vec<_>, _>>()?
        .iter()
        .any(|name| name == column_name))
}

fn tokenizer_sql(config: &FtsSchemaConfig) -> &'static str {
    match config.tokenizer {
        FtsTokenizer::Trigram => "tokenize='trigram'",
        FtsTokenizer::Unicode61RemoveDiacritics2 => "tokenize='unicode61 remove_diacritics 2'",
    }
}

fn with_savepoint<T>(
    conn: &Connection,
    name: &str,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    conn.execute_batch(&format!("SAVEPOINT {name}"))?;
    match operation() {
        Ok(value) => {
            conn.execute_batch(&format!("RELEASE SAVEPOINT {name}"))?;
            Ok(value)
        }
        Err(error) => {
            let _ = conn.execute_batch(&format!(
                "ROLLBACK TO SAVEPOINT {name}; RELEASE SAVEPOINT {name}"
            ));
            Err(error)
        }
    }
}

fn sql_rowid(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| {
        IndexError::Contract(format!(
            "SQLite rowid must not exceed {} (received {value})",
            i64::MAX
        ))
    })
}

fn sql_u64_rowid(value: i64) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| IndexError::Schema(format!("SQLite returned a negative rowid: {value}")))
}

/// Adopted embedding dimensionality (07 §5.3 / 03 §7). `chunk_vec` is fixed to
/// this width; incompatible-width embeddings never reach vector search.
pub const CHUNK_VEC_DIMENSIONS: usize = 768;

#[cfg(test)]
mod tests {
    use super::*;

    fn row(chunk_id: &str, text: &str) -> ChunkRow {
        ChunkRow {
            chunk_id: chunk_id.to_owned(),
            raw_hash: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .to_owned(),
            tool_profile_hash:
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
            r#gen: 0,
            unit_key: "doc:1".to_owned(),
            unit_content_hash:
                "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".to_owned(),
            chunking_config_hash:
                "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".to_owned(),
            raw_path: "a.md".to_owned(),
            heading_path: Some(vec!["認証仕様".to_owned()]),
            section_id: Some("認証仕様".to_owned()),
            byte_start: 0,
            byte_end: text.len() as u64,
            text_hash: "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
                .to_owned(),
            text: text.to_owned(),
            created_at: "2026-07-03T00:00:00Z".to_owned(),
        }
    }

    fn basis_vector_bytes(axis: usize) -> Vec<u8> {
        let mut vector = vec![0.0_f32; CHUNK_VEC_DIMENSIONS];
        vector[axis] = 1.0;
        vector.into_iter().flat_map(f32::to_le_bytes).collect()
    }

    #[test]
    fn ct3_fts_001_external_content_triggers_sync_insert_delete() {
        let mut fts = SqliteFtsIndex::in_memory(FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        })
        .unwrap();
        fts.index_chunk(&row("c1", "認証仕様の更新")).unwrap();
        assert_eq!(fts.search("認証仕様", 10).unwrap()[0].chunk_id, "c1");
        fts.purge_raw(&row("c1", "認証仕様の更新").raw_hash, &BTreeSet::new())
            .unwrap();
        assert!(fts.search("認証仕様", 10).unwrap().is_empty());
    }

    /// PC37 (04 §4.1): `chunk_publications` is scoped to a chunk/config
    /// association, accepts multiple introductions for that exact triple, and
    /// cannot publish a second config merely because it shares the chunk id.
    #[test]
    fn pc37_chunk_publications_records_multiple_introductions_idempotently() {
        let mut fts = SqliteFtsIndex::in_memory(FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        })
        .unwrap();
        fts.index_chunk(&row("c1", "merge introduction test"))
            .unwrap();
        let conn = fts.connection();
        let config_a = "sha256:config-a";
        let config_b = "sha256:config-b";
        record_chunk_publication(conn, "c1", config_a, "sha256:cccccccc").unwrap();
        record_chunk_publication(conn, "c1", config_a, "sha256:aaaaaaaa").unwrap();
        // Re-publishing the same association/introduction triple (a
        // resurrection or a repeated rebuild pass) does not duplicate the row.
        record_chunk_publication(conn, "c1", config_a, "sha256:aaaaaaaa").unwrap();
        record_chunk_publication(conn, "c1", config_b, "sha256:bbbbbbbb").unwrap();

        let introductions = chunk_publication_introductions(conn, "c1", config_a).unwrap();
        assert_eq!(
            introductions,
            vec!["sha256:aaaaaaaa".to_owned(), "sha256:cccccccc".to_owned()]
        );
        assert!(
            chunk_publication_introductions(conn, "c-never-published", config_a)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn purge_raw_is_atomic_and_preserves_shared_content_embeddings() {
        const RAW_TARGET: &str =
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        const RAW_SURVIVOR: &str =
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        const TEXT_SHARED: &str =
            "sha256:1111111111111111111111111111111111111111111111111111111111111111";
        const TEXT_UNIQUE: &str =
            "sha256:2222222222222222222222222222222222222222222222222222222222222222";

        let mut fts = SqliteFtsIndex::in_memory(FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        })
        .unwrap();
        let mut target_shared = row("c-target-shared", "shared searchable phrase");
        target_shared.raw_hash = RAW_TARGET.to_owned();
        target_shared.text_hash = TEXT_SHARED.to_owned();
        let mut survivor = row("c-survivor", "shared searchable phrase");
        survivor.raw_hash = RAW_SURVIVOR.to_owned();
        survivor.text_hash = TEXT_SHARED.to_owned();
        let mut target_unique = row("c-target-unique", "unique purge phrase");
        target_unique.raw_hash = RAW_TARGET.to_owned();
        target_unique.text_hash = TEXT_UNIQUE.to_owned();

        fts.index_chunk(&target_shared).unwrap();
        fts.index_chunk(&survivor).unwrap();
        fts.index_chunk(&target_unique).unwrap();
        crate::embedding_store::write_chunk_embedding(
            fts.connection(),
            "sha256:embedding-shared",
            TEXT_SHARED,
            &target_shared.chunk_id,
            &basis_vector_bytes(0),
            CHUNK_VEC_DIMENSIONS as u64,
            "cosine",
            "multimodal",
            "sha256:profile",
            None,
        )
        .unwrap();
        crate::embedding_store::write_chunk_embedding(
            fts.connection(),
            "sha256:embedding-shared",
            TEXT_SHARED,
            &survivor.chunk_id,
            &basis_vector_bytes(0),
            CHUNK_VEC_DIMENSIONS as u64,
            "cosine",
            "multimodal",
            "sha256:profile",
            None,
        )
        .unwrap();
        crate::embedding_store::write_chunk_embedding(
            fts.connection(),
            "sha256:embedding-unique",
            TEXT_UNIQUE,
            &target_unique.chunk_id,
            &basis_vector_bytes(1),
            CHUNK_VEC_DIMENSIONS as u64,
            "cosine",
            "multimodal",
            "sha256:profile",
            None,
        )
        .unwrap();

        let report = fts.purge_raw(RAW_TARGET, &BTreeSet::new()).unwrap();
        assert_eq!(
            report,
            PurgeRawIndexReport {
                chunk_ids: vec!["c-target-shared".to_owned(), "c-target-unique".to_owned()],
                deleted_chunks: 2,
                deleted_associations: 2,
                deleted_chunk_vectors: 2,
                deleted_image_vectors: 0,
                deleted_orphan_embeddings: 1,
                // The unique chunk's own embedding; the shared one survives
                // because another chunk still carries its `text_hash`.
                deleted_embedding_ids: vec!["sha256:embedding-unique".to_owned()],
            }
        );
        assert_eq!(
            fts.search("shared searchable", 10)
                .unwrap()
                .into_iter()
                .map(|hit| hit.chunk_id)
                .collect::<Vec<_>>(),
            vec!["c-survivor"]
        );
        assert!(fts.search("unique purge", 10).unwrap().is_empty());
        assert!(
            crate::embedding_store::read_chunk_vector(fts.connection(), &target_shared.chunk_id)
                .unwrap()
                .is_none()
        );
        assert!(
            crate::embedding_store::read_chunk_vector(fts.connection(), &target_unique.chunk_id)
                .unwrap()
                .is_none()
        );
        assert!(
            crate::embedding_store::read_chunk_vector(fts.connection(), &survivor.chunk_id)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            fts.connection()
                .query_row(
                    "SELECT COUNT(*) FROM embeddings WHERE target_id = ?1",
                    params![TEXT_SHARED],
                    |row| row.get::<_, u64>(0)
                )
                .unwrap(),
            1
        );
        assert_eq!(
            fts.connection()
                .query_row(
                    "SELECT COUNT(*) FROM embeddings WHERE target_id = ?1",
                    params![TEXT_UNIQUE],
                    |row| row.get::<_, u64>(0)
                )
                .unwrap(),
            0
        );

        assert_eq!(
            fts.purge_raw(RAW_TARGET, &BTreeSet::new()).unwrap(),
            PurgeRawIndexReport::default(),
            "replay after a completed purge is idempotent"
        );
    }

    /// 05 §3.5: an image vector left behind is the purged figure still
    /// rankable by vector search. Which images are orphaned is the caller's
    /// judgement (the rule needs the Markdown image grammar); this pins that
    /// what it names is deleted, and that an image it does NOT name — one a
    /// surviving document still shows — is preserved.
    #[test]
    fn purge_raw_deletes_the_image_vectors_the_caller_named_and_no_others() {
        let mut fts = SqliteFtsIndex::in_memory(FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        })
        .unwrap();
        const ORPHANED: &str =
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        const SHARED: &str =
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let target = row("c-target", "target body");
        fts.index_chunk(&target).unwrap();
        for (index, image_hash) in [ORPHANED, SHARED].iter().enumerate() {
            crate::embedding_store::write_image_embedding(
                fts.connection(),
                &format!("sha256:embedding-image-{index}"),
                image_hash,
                &basis_vector_bytes(index),
                CHUNK_VEC_DIMENSIONS as u64,
                "cosine",
                "multimodal",
                "sha256:profile",
            )
            .unwrap();
        }

        let orphaned = BTreeSet::from([ORPHANED.to_owned()]);
        let report = fts.purge_raw(&target.raw_hash, &orphaned).unwrap();
        assert_eq!(report.deleted_image_vectors, 1);
        assert!(
            report
                .deleted_embedding_ids
                .contains(&"sha256:embedding-image-0".to_owned())
        );

        assert!(
            crate::embedding_store::read_image_vector(fts.connection(), ORPHANED)
                .unwrap()
                .is_none(),
            "an image only the purged document referenced must lose its vector"
        );
        assert!(
            crate::embedding_store::read_image_vector(fts.connection(), SHARED)
                .unwrap()
                .is_some(),
            "an image a surviving document still shows has not stopped existing"
        );
    }

    #[test]
    fn purge_raw_rolls_back_all_index_layers_when_chunk_delete_fails() {
        let mut fts = SqliteFtsIndex::in_memory(FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        })
        .unwrap();
        let target = row("c-target", "rollback searchable phrase");
        fts.index_chunk(&target).unwrap();
        crate::embedding_store::write_chunk_embedding(
            fts.connection(),
            "sha256:embedding-rollback",
            &target.text_hash,
            &target.chunk_id,
            &basis_vector_bytes(0),
            CHUNK_VEC_DIMENSIONS as u64,
            "cosine",
            "multimodal",
            "sha256:profile",
            None,
        )
        .unwrap();
        fts.connection()
            .execute_batch(
                "CREATE TRIGGER reject_purge BEFORE DELETE ON chunks BEGIN
                     SELECT RAISE(ABORT, 'synthetic purge failure');
                 END;",
            )
            .unwrap();

        let error = fts
            .purge_raw(&target.raw_hash, &BTreeSet::new())
            .unwrap_err();
        assert!(error.to_string().contains("synthetic purge failure"));
        assert_eq!(fts.search("rollback searchable", 10).unwrap().len(), 1);
        assert!(
            crate::embedding_store::read_chunk_vector(fts.connection(), &target.chunk_id)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            fts.connection()
                .query_row("SELECT COUNT(*) FROM chunk_config_generations", [], |row| {
                    row.get::<_, u64>(0)
                })
                .unwrap(),
            1,
            "config association deletion must roll back with the chunk"
        );
    }

    #[test]
    fn ct3_fts_003_trigram_matches_cjk_substrings_and_short_query_skips() {
        let mut fts = SqliteFtsIndex::in_memory(FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        })
        .unwrap();
        fts.index_chunk(&row("c1", "認証仕様の更新")).unwrap();
        assert_eq!(fts.search("認証仕様", 10).unwrap().len(), 1);
        assert!(fts.search("認", 10).unwrap().is_empty());
    }

    #[test]
    fn index_metadata_reader_requires_the_current_table() {
        let non_current = Connection::open_in_memory().unwrap();
        let error = read_index_metadata(&non_current)
            .expect_err("a missing current index_metadata table must be rejected");
        assert!(error.to_string().contains("kio repair rebuild-db"));

        let current = SqliteFtsIndex::in_memory(FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        })
        .unwrap();
        assert_eq!(read_index_metadata(current.connection()).unwrap(), None);
    }

    #[test]
    fn current_file_schema_reopens_without_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sqlite.db");
        let config = FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        };
        drop(SqliteFtsIndex::open(&path, config.clone()).unwrap());
        let before = std::fs::read(&path).unwrap();
        let reopened = SqliteFtsIndex::open(&path, config).unwrap();
        assert!(
            validate_current_schema(
                reopened.connection(),
                &FtsSchemaConfig {
                    tokenizer: FtsTokenizer::Trigram
                }
            )
            .unwrap()
        );
        drop(reopened);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn existing_source_connection_opens_only_a_current_real_database() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sqlite.db");
        let config = FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        };
        drop(SqliteFtsIndex::open(&path, config.clone()).unwrap());

        let conn = open_existing_source_index_connection(
            &path,
            ExistingSourceIndexOpenMode::ReadOnly,
            &config,
        )
        .expect("current source index must open read-only");
        assert!(validate_current_schema(&conn, &config).unwrap());
        drop(conn);

        let missing = directory.path().join("missing.sqlite");
        let error = open_existing_source_index_connection(
            &missing,
            ExistingSourceIndexOpenMode::ReadOnly,
            &config,
        )
        .expect_err("helper must never create a missing source index");
        assert!(error.to_string().contains("inspect source index"));
        assert!(!missing.exists());
    }

    #[test]
    fn exact_retarget_candidates_are_exact_bounded_and_read_only() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sqlite.db");
        let config = FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        };
        let mut index = SqliteFtsIndex::open(&path, config.clone()).unwrap();
        let mut first = row("retarget-a", "first");
        first.raw_hash =
            "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".to_owned();
        first.raw_path = "docs/a.md".to_owned();
        first.heading_path = Some(vec!["A".to_owned(), "B".to_owned()]);
        first.r#gen = 7;
        first.unit_key = "doc:target-a".to_owned();
        index.index_chunk(&first).unwrap();
        let mut second = first.clone();
        second.chunk_id = "retarget-b".to_owned();
        second.unit_key = "doc:target-b".to_owned();
        index.index_chunk(&second).unwrap();
        let mut other = first.clone();
        other.chunk_id = "retarget-other".to_owned();
        other.raw_path = "docs/other.md".to_owned();
        other.heading_path = Some(vec!["Other heading".to_owned()]);
        index.index_chunk(&other).unwrap();
        drop(index);
        let before = std::fs::read(&path).unwrap();

        let source = open_existing_source_index_connection(
            &path,
            ExistingSourceIndexOpenMode::ReadOnly,
            &config,
        )
        .unwrap();
        let exact = source
            .exact_retarget_candidates(&first.raw_hash, &first.tool_profile_hash, first.r#gen)
            .unwrap();
        let RetargetCandidates::Candidates(rows) = exact else {
            panic!("exact rows must not overflow");
        };
        // The query deliberately does not trust a historical path or heading
        // from SQLite. All three same-instance rows are candidates, including
        // the one with different derived path/heading.
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].chunk_id, "retarget-a");
        assert_eq!(rows[1].chunk_id, "retarget-b");
        assert_eq!(rows[2].chunk_id, "retarget-other");
        assert!(matches!(
            source
                .exact_retarget_candidates(
                    &first.raw_hash,
                    "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
                    first.r#gen,
                )
                .unwrap(),
            RetargetCandidates::Candidates(rows) if rows.is_empty()
        ));
        source.recheck_source_identity().unwrap();
        drop(source);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn exact_retarget_candidates_classify_overflow() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sqlite.db");
        let config = FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        };
        let mut index = SqliteFtsIndex::open(&path, config.clone()).unwrap();
        let mut candidate = row("retarget-00000", "candidate");
        candidate.raw_hash =
            "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".to_owned();
        candidate.raw_path = "docs/overflow.md".to_owned();
        candidate.heading_path = Some(vec!["Overflow".to_owned()]);
        index.connection().execute_batch("BEGIN IMMEDIATE").unwrap();
        let insert_candidates = (|| -> Result<()> {
            // Keep every public index_chunk call: each one exercises the
            // row validation and nested-savepoint path. The outer transaction
            // only avoids 4,097 separate SQLite durable commits for this
            // overflow-classification fixture.
            for number in 0..=RETARGET_CANDIDATE_LIMIT {
                candidate.chunk_id = format!("retarget-{number:05}");
                candidate.unit_key = format!("doc:{number}");
                index.index_chunk(&candidate)?;
            }
            Ok(())
        })();
        match insert_candidates {
            Ok(()) => index.connection().execute_batch("COMMIT").unwrap(),
            Err(error) => {
                let _ = index.connection().execute_batch("ROLLBACK");
                panic!("overflow fixture insertion must roll back on failure: {error}");
            }
        }
        drop(index);

        let source = open_existing_source_index_connection(
            &path,
            ExistingSourceIndexOpenMode::ReadOnly,
            &config,
        )
        .unwrap();
        assert_eq!(
            source
                .exact_retarget_candidates(
                    &candidate.raw_hash,
                    &candidate.tool_profile_hash,
                    candidate.r#gen,
                )
                .unwrap(),
            RetargetCandidates::Overflow
        );
    }

    #[cfg(unix)]
    #[test]
    fn source_connection_recheck_rejects_named_leaf_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sqlite.db");
        let replacement = directory.path().join("replacement.sqlite");
        let config = FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        };
        drop(SqliteFtsIndex::open(&path, config.clone()).unwrap());
        drop(SqliteFtsIndex::open(&replacement, config.clone()).unwrap());
        let source = open_existing_source_index_connection(
            &path,
            ExistingSourceIndexOpenMode::ReadOnly,
            &config,
        )
        .unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let error = source
            .recheck_source_identity()
            .expect_err("replacement must invalidate retained source identity");
        assert!(
            error.to_string().contains("hard link")
                || error.to_string().contains("name changed while operating")
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn bound_gc_rotation_stays_on_retained_kio_capability_and_reports_missing() {
        let directory = tempfile::tempdir().unwrap();
        let kio = directory.path().join(".kio");
        let index = kio.join("index");
        std::fs::create_dir_all(&index).unwrap();
        let path = index.join("sqlite.db");
        let config = FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        };
        drop(SqliteFtsIndex::open(&path, config.clone()).unwrap());
        let generation = "01J00000000000000000000000";
        let conn = Connection::open(&path).unwrap();
        ensure_index_metadata(&conn, generation, 7).unwrap();
        drop(conn);

        let kio_handle = cap_fs::open_ambient_dir(&kio, cap_primitives::ambient_authority())
            .expect("open retained .kio test capability");
        let rotated = rotate_bound_gc_index_generation(
            &kio_handle,
            "01J00000000000000000000001",
            None,
            &config,
        )
        .unwrap()
        .expect("present index must remain present");
        assert_eq!(
            rotated.metadata.index_generation,
            "01J00000000000000000000001"
        );
        assert_eq!(rotated.metadata.last_lifecycle_epoch, 7);

        std::fs::remove_file(&path).unwrap();
        assert!(
            read_bound_gc_index_metadata(&kio_handle, &config)
                .unwrap()
                .is_none()
        );
        assert!(
            rotate_bound_gc_index_generation(
                &kio_handle,
                "01J00000000000000000000002",
                None,
                &config,
            )
            .unwrap()
            .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn bound_gc_rotation_rejects_an_unexpected_initial_generation_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let kio = directory.path().join(".kio");
        let index = kio.join("index");
        std::fs::create_dir_all(&index).unwrap();
        let path = index.join("sqlite.db");
        let config = FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        };
        drop(SqliteFtsIndex::open(&path, config.clone()).unwrap());
        let conn = Connection::open(&path).unwrap();
        ensure_index_metadata(&conn, "01J00000000000000000000000", 7).unwrap();
        drop(conn);
        let kio_handle = cap_fs::open_ambient_dir(&kio, cap_primitives::ambient_authority())
            .expect("open retained .kio test capability");

        let error = rotate_bound_gc_index_generation(
            &kio_handle,
            "01J00000000000000000000001",
            Some((
                "01J00000000000000000000099",
                "unix:0000000000000000:0000000000000000",
            )),
            &config,
        )
        .expect_err("mismatched initial generation must fail closed");
        assert!(
            error
                .to_string()
                .contains("generation changed before rotation")
        );
        let metadata = read_bound_gc_index_metadata(&kio_handle, &config)
            .unwrap()
            .unwrap();
        assert_eq!(
            metadata.metadata.index_generation,
            "01J00000000000000000000000"
        );
    }

    #[cfg(unix)]
    #[test]
    fn bound_gc_rotation_rejects_index_symlink_without_touching_victim() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let victim = tempfile::tempdir().unwrap();
        let victim_db = victim.path().join("sqlite.db");
        let config = FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        };
        drop(SqliteFtsIndex::open(&victim_db, config.clone()).unwrap());
        let before = std::fs::read(&victim_db).unwrap();
        let kio = directory.path().join(".kio");
        std::fs::create_dir(&kio).unwrap();
        symlink(victim.path(), kio.join("index")).unwrap();
        let kio_handle = cap_fs::open_ambient_dir(&kio, cap_primitives::ambient_authority())
            .expect("open retained .kio test capability");

        let error = rotate_bound_gc_index_generation(
            &kio_handle,
            "01J00000000000000000000003",
            None,
            &config,
        )
        .expect_err("bound GC rotation must reject index symlink");
        assert!(error.to_string().contains("open GC index directory"));
        assert_eq!(std::fs::read(&victim_db).unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn open_refuses_a_symlink_source_index_without_touching_its_destination() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.sqlite");
        let config = FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        };
        drop(SqliteFtsIndex::open(&target, config.clone()).unwrap());
        let before = std::fs::read(&target).unwrap();
        let path = directory.path().join("sqlite.db");
        symlink(&target, &path).unwrap();

        let error = SqliteFtsIndex::open(&path, config)
            .err()
            .expect("source index symlink must be rejected");
        assert!(error.to_string().contains("not a regular file"));
        assert_eq!(std::fs::read(&target).unwrap(), before);
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn source_index_hardlinks_are_rejected_without_touching_the_other_path() {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.sqlite");
        let config = FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        };
        drop(SqliteFtsIndex::open(&target, config.clone()).unwrap());
        let before = std::fs::read(&target).unwrap();
        let path = directory.path().join("sqlite.db");
        std::fs::hard_link(&target, &path).unwrap();

        let error = SqliteFtsIndex::open(&path, config.clone())
            .err()
            .expect("hardlinked source index must be rejected");
        assert!(error.to_string().contains("exactly one hard link"));
        assert_eq!(std::fs::read(&target).unwrap(), before);

        let error = open_existing_source_index_connection(
            &path,
            ExistingSourceIndexOpenMode::ReadWrite,
            &config,
        )
        .expect_err("helper must reject a hardlinked source index");
        assert!(error.to_string().contains("exactly one hard link"));
        assert_eq!(std::fs::read(&target).unwrap(), before);
        #[cfg(unix)]
        {
            assert_eq!(std::fs::metadata(&target).unwrap().nlink(), 2);
            assert_eq!(std::fs::metadata(&path).unwrap().nlink(), 2);
        }
        #[cfg(windows)]
        {
            use cap_fs::_WindowsByHandle;
            let target = std::fs::File::open(&target).unwrap();
            let path = std::fs::File::open(&path).unwrap();
            assert_eq!(
                cap_fs::Metadata::from_file(&target)
                    .unwrap()
                    .number_of_links(),
                Some(2)
            );
            assert_eq!(
                cap_fs::Metadata::from_file(&path)
                    .unwrap()
                    .number_of_links(),
                Some(2)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn open_refuses_a_symlink_source_index_parent_without_creating_a_victim_database() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let victim = tempfile::tempdir().unwrap();
        let parent = directory.path().join("index");
        symlink(victim.path(), &parent).unwrap();
        let path = parent.join("sqlite.db");
        let config = FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        };

        let error = SqliteFtsIndex::open(&path, config.clone())
            .err()
            .expect("source index parent symlink must be rejected");
        assert!(
            error
                .to_string()
                .contains("parent must be a real directory")
        );
        assert!(
            !victim.path().join("sqlite.db").exists(),
            "fresh source-index bootstrap must not create a database through a parent symlink"
        );

        let error = open_existing_source_index_connection(
            &path,
            ExistingSourceIndexOpenMode::ReadOnly,
            &config,
        )
        .expect_err("existing-source helper must reject a symlink parent too");
        assert!(
            error
                .to_string()
                .contains("parent must be a real directory")
        );
    }

    #[cfg(unix)]
    #[test]
    fn bound_source_fd_survives_parent_replacement_without_touching_victim() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let victim = tempfile::tempdir().unwrap();
        let index = directory.path().join("index");
        std::fs::create_dir(&index).unwrap();
        let path = index.join("sqlite.db");
        let config = FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        };
        let index_connection = SqliteFtsIndex::open(&path, config).unwrap();

        let original = directory.path().join("index-original");
        std::fs::rename(&index, &original).unwrap();
        symlink(victim.path(), &index).unwrap();
        index_connection
            .connection()
            .execute("CREATE TABLE post_parent_replace (id INTEGER)", [])
            .unwrap();
        drop(index_connection);

        assert!(!victim.path().join("sqlite.db").exists());
        let original_db = original.join("sqlite.db");
        let reopened = Connection::open(&original_db).unwrap();
        assert!(table_exists(&reopened, "post_parent_replace").unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn bound_source_fd_survives_repository_root_replacement_without_touching_victim() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let victim = tempfile::tempdir().unwrap();
        let kio = directory.path().join(".kio");
        let index = kio.join("index");
        std::fs::create_dir_all(&index).unwrap();
        let path = index.join("sqlite.db");
        let config = FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        };
        let index_connection = SqliteFtsIndex::open(&path, config).unwrap();

        let original = directory.path().join(".kio-original");
        std::fs::rename(&kio, &original).unwrap();
        symlink(victim.path(), &kio).unwrap();
        index_connection
            .connection()
            .execute("CREATE TABLE post_root_replace (id INTEGER)", [])
            .unwrap();
        drop(index_connection);

        assert!(!victim.path().join("index/sqlite.db").exists());
        let reopened = Connection::open(original.join("index/sqlite.db")).unwrap();
        assert!(table_exists(&reopened, "post_root_replace").unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn bound_source_leaf_blocks_rename_and_delete_until_connection_drops() {
        use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sqlite.db");
        let config = FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        };
        let index = SqliteFtsIndex::open(&path, config).unwrap();
        let moved = directory.path().join("sqlite-moved.db");

        let rename_error = std::fs::rename(&path, &moved)
            .expect_err("the retained no-delete-share leaf must block rename");
        assert_eq!(
            rename_error.raw_os_error(),
            Some(ERROR_SHARING_VIOLATION as i32)
        );
        let delete_error = std::fs::remove_file(&path)
            .expect_err("the retained no-delete-share leaf must block deletion");
        assert_eq!(
            delete_error.raw_os_error(),
            Some(ERROR_SHARING_VIOLATION as i32)
        );

        drop(index);
        std::fs::rename(&path, &moved)
            .expect("rename must succeed after the bound SQLite connection drops");
        std::fs::remove_file(&moved)
            .expect("deletion must succeed after the bound SQLite connection drops");
    }

    #[cfg(unix)]
    #[test]
    fn existing_source_connection_refuses_a_symlink_without_touching_target() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.sqlite");
        let config = FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        };
        drop(SqliteFtsIndex::open(&target, config.clone()).unwrap());
        let before = std::fs::read(&target).unwrap();
        let path = directory.path().join("sqlite.db");
        symlink(&target, &path).unwrap();

        let error = open_existing_source_index_connection(
            &path,
            ExistingSourceIndexOpenMode::ReadWrite,
            &config,
        )
        .expect_err("source index symlink must be rejected");
        assert!(error.to_string().contains("not a regular file"));
        assert_eq!(std::fs::read(&target).unwrap(), before);
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn existing_writable_source_connection_uses_memory_journal_without_sidecars() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sqlite.db");
        let config = FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        };
        drop(SqliteFtsIndex::open(&path, config.clone()).unwrap());
        let conn = open_existing_source_index_connection(
            &path,
            ExistingSourceIndexOpenMode::ReadWrite,
            &config,
        )
        .unwrap();
        assert_eq!(
            conn.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
                .unwrap()
                .to_ascii_lowercase(),
            "memory"
        );
        conn.execute(
            "INSERT INTO index_metadata (id, index_generation, last_lifecycle_epoch)
             VALUES (1, 'test-generation', 0)
             ON CONFLICT(id) DO UPDATE SET index_generation = excluded.index_generation",
            [],
        )
        .unwrap();
        drop(conn);
        for suffix in ["-journal", "-wal", "-shm"] {
            assert!(
                !std::path::PathBuf::from(format!("{}{}", path.display(), suffix)).exists(),
                "memory journal must not leave a {suffix} sidecar"
            );
        }
    }

    #[test]
    fn unknown_user_object_is_rejected_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sqlite.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE unrelated_application_state (id INTEGER PRIMARY KEY);")
            .unwrap();
        drop(conn);
        let before = std::fs::read(&path).unwrap();

        let error = SqliteFtsIndex::open(
            &path,
            FtsSchemaConfig {
                tokenizer: FtsTokenizer::Trigram,
            },
        )
        .err()
        .expect("unknown sqlite objects must not be adopted as a Kio index");
        assert!(error.to_string().contains("kio repair rebuild-db"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn current_schema_with_extra_user_object_is_rejected_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sqlite.db");
        let config = FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        };
        drop(SqliteFtsIndex::open(&path, config.clone()).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE obsolete_cache (id INTEGER PRIMARY KEY);")
            .unwrap();
        drop(conn);
        let before = std::fs::read(&path).unwrap();

        let error = SqliteFtsIndex::open(&path, config)
            .err()
            .expect("a complete schema must not conceal non-current objects");
        assert!(
            error
                .to_string()
                .contains("unknown user object obsolete_cache"),
            "{error}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn missing_unique_constraint_is_rejected_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sqlite.db");
        let config = FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        };
        drop(SqliteFtsIndex::open(&path, config.clone()).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA writable_schema = ON;").unwrap();
        conn.execute(
            "UPDATE sqlite_master
             SET sql = replace(sql, 'UNIQUE(chunk_id, chunking_config_hash)', 'introduction_commit TEXT NOT NULL, UNIQUE(chunk_id, chunking_config_hash, introduction_commit)')
             WHERE type = 'table' AND name = 'chunk_config_generations'",
            [],
        )
        .unwrap();
        let schema_version: i64 = conn
            .query_row("PRAGMA schema_version", [], |row| row.get(0))
            .unwrap();
        conn.pragma_update(None, "schema_version", schema_version + 1)
            .unwrap();
        conn.execute_batch("PRAGMA writable_schema = OFF;").unwrap();
        drop(conn);
        let before = std::fs::read(&path).unwrap();

        let error = SqliteFtsIndex::open(&path, config)
            .err()
            .expect("weakened table constraint must be rejected");
        assert!(
            error
                .to_string()
                .contains("chunk_config_generations columns do not match current schema"),
            "{error}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn disabled_trigger_is_rejected_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sqlite.db");
        let config = FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        };
        drop(SqliteFtsIndex::open(&path, config.clone()).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA writable_schema = ON;").unwrap();
        conn.execute(
            "UPDATE sqlite_master
             SET sql = replace(sql, 'AFTER INSERT ON chunks', 'AFTER INSERT ON chunks WHEN 0')
             WHERE type = 'trigger' AND name = 'chunks_ai'",
            [],
        )
        .unwrap();
        conn.execute_batch("PRAGMA writable_schema = OFF;").unwrap();
        drop(conn);
        let before = std::fs::read(&path).unwrap();

        let error = SqliteFtsIndex::open(&path, config)
            .err()
            .expect("disabled trigger must be rejected");
        assert!(error.to_string().contains("chunks_ai definition"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn tree_entries_distinguish_raw_only_from_normalized_entries() {
        let fts = SqliteFtsIndex::in_memory(FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        })
        .unwrap();
        let conn = fts.connection();
        conn.execute(
            "INSERT INTO tree_entries(
                 commit_hash, path, raw_hash, tool_profile_hash, gen, manifest_hash
             ) VALUES ('c1', 'raw-only.md', 'sha256:raw', NULL, NULL, NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tree_entries(
                 commit_hash, path, raw_hash, tool_profile_hash, gen, manifest_hash
             ) VALUES ('c1', 'normalized.md', 'sha256:normalized', 'sha256:tool', 4, 'sha256:manifest')",
            [],
        )
        .unwrap();
        let rows = conn
            .prepare(
                "SELECT path, tool_profile_hash, gen, manifest_hash
                 FROM tree_entries ORDER BY path",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            rows[0],
            (
                "normalized.md".to_owned(),
                Some("sha256:tool".to_owned()),
                Some(4),
                Some("sha256:manifest".to_owned())
            )
        );
        assert_eq!(rows[1], ("raw-only.md".to_owned(), None, None, None));
        assert!(
            validate_current_schema(
                conn,
                &FtsSchemaConfig {
                    tokenizer: FtsTokenizer::Trigram
                }
            )
            .unwrap()
        );

        conn.execute(
            "INSERT INTO tree_entries(
                 commit_hash, path, raw_hash, tool_profile_hash, gen, manifest_hash
             ) VALUES ('c1', 'partial.md', 'sha256:partial', 'sha256:tool', 5, NULL)",
            [],
        )
        .unwrap();
        let error = validate_current_schema(
            conn,
            &FtsSchemaConfig {
                tokenizer: FtsTokenizer::Trigram,
            },
        )
        .expect_err("partial normalize projection must be rejected");
        assert!(error.to_string().contains("partial normalize projection"));
    }

    #[test]
    fn ct4_chunk_config_schema_is_an_append_only_association() {
        let mut fts = SqliteFtsIndex::in_memory(FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        })
        .unwrap();
        assert!(!table_has_column(fts.connection(), "chunks", "chunking_config_hash").unwrap());
        assert!(
            table_has_column(
                fts.connection(),
                "chunk_config_generations",
                "association_rowid"
            )
            .unwrap()
        );
        assert_eq!(
            max_chunk_config_association_rowid(fts.connection()).unwrap(),
            0
        );

        let first = row("c1", "認証仕様の更新");
        fts.index_chunk_with_association_rowid(&first, Some(17))
            .unwrap();
        assert_eq!(
            current_config_eligible_chunk_ids(
                fts.connection(),
                &first.chunking_config_hash,
                17,
                17
            )
            .unwrap(),
            BTreeSet::new(),
            "a chunk without an authoritative publication relation is ineligible"
        );
        record_chunk_publication(
            fts.connection(),
            "c1",
            &first.chunking_config_hash,
            "sha256:commit",
        )
        .unwrap();
        // Replaying the same durable association triple is idempotent and does
        // not burn another AUTOINCREMENT value.
        fts.index_chunk_with_association_rowid(&first, Some(17))
            .unwrap();

        let mut next_generation = first.clone();
        next_generation.chunking_config_hash = "sha256:next-config".to_owned();
        fts.index_chunk(&next_generation).unwrap();

        let conn = fts.connection();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM chunks", [], |row| row
                .get::<_, u64>(0))
                .unwrap(),
            1,
            "one immutable chunk row is shared by both configs"
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM chunk_config_generations", [], |row| {
                row.get::<_, u64>(0)
            })
            .unwrap(),
            2
        );
        assert_eq!(max_chunk_config_association_rowid(conn).unwrap(), 18);
        assert!(
            chunk_has_current_config_association(conn, "c1", &first.chunking_config_hash, 17)
                .unwrap()
        );
        assert!(
            !chunk_has_current_config_association(
                conn,
                "c1",
                &next_generation.chunking_config_hash,
                17
            )
            .unwrap()
        );
        assert_eq!(
            current_config_eligible_chunk_ids(conn, &next_generation.chunking_config_hash, 1, 17)
                .unwrap(),
            BTreeSet::new(),
            "a page-1 association maximum excludes a later generation"
        );
        assert_eq!(
            current_config_eligible_chunk_ids(conn, &next_generation.chunking_config_hash, 1, 18)
                .unwrap(),
            BTreeSet::new(),
            "a publication for config A cannot make config B eligible"
        );
    }

    #[test]
    fn ct4_explicit_association_rowid_conflicts_roll_back_the_chunk() {
        let mut fts = SqliteFtsIndex::in_memory(FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        })
        .unwrap();
        fts.index_chunk_with_association_rowid(&row("c1", "first chunk"), Some(9))
            .unwrap();

        let error = fts
            .index_chunk_with_association_rowid(&row("c2", "second chunk"), Some(9))
            .unwrap_err();
        assert!(error.to_string().contains("already occupied"));
        assert_eq!(
            fts.connection()
                .query_row(
                    "SELECT COUNT(*) FROM chunks WHERE chunk_id = 'c2'",
                    [],
                    |row| row.get::<_, u64>(0)
                )
                .unwrap(),
            0,
            "chunk and association publication are atomic"
        );

        let error = fts
            .index_chunk_with_association_rowid(&row("c1", "first chunk"), Some(10))
            .unwrap_err();
        assert!(error.to_string().contains("not requested rowid"));
        assert_eq!(
            max_chunk_config_association_rowid(fts.connection()).unwrap(),
            9
        );
    }

    #[test]
    fn config_associations_are_idempotent_creation_pairs() {
        let mut fts = SqliteFtsIndex::in_memory(FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        })
        .unwrap();
        let first = row("c1", "same config on two branches");
        assert_eq!(
            fts.index_chunk_with_association_rowid(&first, Some(17))
                .unwrap(),
            17
        );

        let incomparable = first.clone();
        assert_eq!(
            fts.index_chunk_with_association_rowid(&incomparable, None)
                .unwrap(),
            17,
            "the same creation pair is idempotent regardless of publication history"
        );
        assert_eq!(
            fts.index_chunk_with_association_rowid(&incomparable, None)
                .unwrap(),
            17,
            "an automatic replay of the exact pair does not append another row"
        );
        assert_eq!(
            fts.index_chunk_with_association_rowid(&incomparable, Some(17))
                .unwrap(),
            17,
            "replaying the exact pair remains idempotent"
        );
        let error = fts
            .index_chunk_with_association_rowid(&incomparable, Some(18))
            .unwrap_err();
        assert!(error.to_string().contains("not requested rowid"));
        assert_eq!(
            fts.connection()
                .query_row("SELECT COUNT(*) FROM chunk_config_generations", [], |row| {
                    row.get::<_, u64>(0)
                })
                .unwrap(),
            1
        );
    }

    #[test]
    fn ct4_durable_replay_preserves_chunk_and_association_rowids() {
        let mut fts = SqliteFtsIndex::in_memory(FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        })
        .unwrap();
        let first = row("c1", "first chunk");
        assert_eq!(
            fts.index_chunk_with_rowids(&first, Some(41), Some(101))
                .unwrap(),
            (41, 101)
        );

        let mut second_config = first.clone();
        second_config.chunking_config_hash = "sha256:next-config".to_owned();
        assert_eq!(
            fts.index_chunk_with_rowids(&second_config, Some(41), Some(205))
                .unwrap(),
            (41, 205)
        );
        assert_eq!(
            fts.connection()
                .query_row("SELECT COUNT(*) FROM chunks", [], |row| row
                    .get::<_, u64>(0))
                .unwrap(),
            1
        );

        let error = fts
            .index_chunk_with_rowids(&row("c2", "collision"), Some(41), Some(206))
            .unwrap_err();
        assert!(error.to_string().contains("already occupied"));
        assert_eq!(
            max_chunk_config_association_rowid(fts.connection()).unwrap(),
            205
        );
    }

    #[test]
    fn ct4_non_current_chunk_config_column_is_rejected_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sqlite.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE chunks (
                    chunk_id TEXT PRIMARY KEY,
                    raw_hash TEXT NOT NULL,
                    tool_profile_hash TEXT NOT NULL,
                    gen INTEGER NOT NULL,
                    unit_key TEXT NOT NULL,
                    chunking_config_hash TEXT NOT NULL,
                    raw_path TEXT NOT NULL,
                    heading_path TEXT NOT NULL,
                    section_id TEXT,
                    byte_start INTEGER NOT NULL,
                    byte_end INTEGER NOT NULL,
                    text_hash TEXT NOT NULL,
                    text TEXT NOT NULL,
                    created_at TEXT NOT NULL
                );
                CREATE VIRTUAL TABLE chunk_fts
                USING fts5(
                    text,
                    heading_path,
                    content='chunks',
                    content_rowid='rowid',
                    tokenize='trigram'
                );
                CREATE TRIGGER chunks_ai AFTER INSERT ON chunks BEGIN
                    INSERT INTO chunk_fts(rowid, text, heading_path)
                    VALUES (new.rowid, new.text, new.heading_path);
                END;
                CREATE TRIGGER chunks_ad AFTER DELETE ON chunks BEGIN
                    INSERT INTO chunk_fts(chunk_fts, rowid, text, heading_path)
                    VALUES ('delete', old.rowid, old.text, old.heading_path);
                END;
                CREATE TRIGGER chunks_au AFTER UPDATE OF text, heading_path ON chunks BEGIN
                    INSERT INTO chunk_fts(chunk_fts, rowid, text, heading_path)
                    VALUES ('delete', old.rowid, old.text, old.heading_path);
                    INSERT INTO chunk_fts(rowid, text, heading_path)
                    VALUES (new.rowid, new.text, new.heading_path);
                END;
                INSERT INTO chunks(
                    rowid, chunk_id, raw_hash, tool_profile_hash, gen, unit_key,
                    chunking_config_hash, raw_path, heading_path, section_id,
                    byte_start, byte_end, text_hash, text, created_at
                ) VALUES
                    (7, 'c7', 'sha256:raw7', 'sha256:profile', 0, 'doc:7',
                     'sha256:cfg7', 'seven.md', '[]', NULL, 0, 16,
                     'sha256:text7', '認証仕様の更新', '2026-07-01T00:00:00Z'),
                    (42, 'c42', 'sha256:raw42', 'sha256:profile', 0, 'doc:42',
                     'sha256:cfg42', 'forty-two.md', '[]', NULL, 0, 18,
                     'sha256:text42', '検索インデックス', '2026-07-02T00:00:00Z');
                "#,
            )
            .unwrap();
        }

        let before = std::fs::read(&path).unwrap();
        let error = SqliteFtsIndex::open(
            &path,
            FtsSchemaConfig {
                tokenizer: FtsTokenizer::Trigram,
            },
        )
        .err()
        .expect("non-current schema must be rejected");
        assert!(error.to_string().contains("kio repair rebuild-db"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn q4_nul_bytes_are_stripped_from_the_fts_index() {
        let mut fts = SqliteFtsIndex::in_memory(FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        })
        .unwrap();
        // A UTF-16-LE ".txt" decoded lossily keeps a NUL after every ASCII char.
        // The trigram tokenizer stops at the first NUL, so before the fix every
        // word after the leading `d` ("distinctword") was silently unsearchable
        // even though `index` reported success.
        let nul_text = "d\u{0}i\u{0}s\u{0}t\u{0}i\u{0}n\u{0}c\u{0}t\u{0}w\u{0}o\u{0}r\u{0}d\u{0}";
        fts.index_chunk(&row("c1", nul_text)).unwrap();
        let hits = fts.search("distinct", 10).unwrap();
        assert_eq!(hits.len(), 1, "NUL-suffixed word must be searchable");
        assert_eq!(hits[0].chunk_id, "c1");
    }

    #[test]
    fn f2_nfd_content_is_searchable_by_nfc_query() {
        let mut fts = SqliteFtsIndex::in_memory(FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        })
        .unwrap();
        // Body carries the DECOMPOSED (NFD) form: "cafe" + U+0301 COMBINING ACUTE.
        // The index projection normalizes it to NFC, so the trigram tokenizer sees
        // the same bytes a composed query produces.
        let nfd_body = "cafe\u{301} latte menu";
        assert!(nfd_body.contains('\u{301}'), "test body must be NFD");
        fts.index_chunk(&row("c1", nfd_body)).unwrap();
        // Composed (NFC) query "café" must hit the NFD-stored content.
        let hits = fts.search("caf\u{e9}", 10).unwrap();
        assert_eq!(hits.len(), 1, "NFC query must match NFD-stored content");
        assert_eq!(hits[0].chunk_id, "c1");
    }

    fn indexed_text_of(fts: &SqliteFtsIndex, chunk_id: &str) -> String {
        fts.conn
            .query_row(
                "SELECT text FROM chunks WHERE chunk_id = ?1",
                params![chunk_id],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
    }

    #[test]
    fn f3_escaped_punctuation_is_searchable_by_the_plain_query() {
        let mut fts = SqliteFtsIndex::in_memory(FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        })
        .unwrap();
        // What a recovered `number` block actually looks like once 07 §5.2.1's
        // escaping has been applied to the text the service read as `期限 7/10`.
        fts.index_chunk(&row("c1", "\u{671f}\u{9650} 7\\/10"))
            .unwrap();
        let hits = fts.search("\"7/10\"", 10).unwrap();
        assert_eq!(hits.len(), 1, "the plain query must match escaped content");
        assert_eq!(hits[0].chunk_id, "c1");
        // The same column is what `snippet` is taken from, so the Agent is shown
        // `期限 7/10` rather than the backslash the storage layer needed.
        assert_eq!(indexed_text_of(&fts, "c1"), "\u{671f}\u{9650} 7/10");
        // And the escaped spelling is deliberately no longer in the index: the
        // projection holds one rendering of the text, the one a reader sees.
        assert!(
            fts.search("\"7\\/10\"", 10).unwrap().is_empty(),
            "the escaped spelling must not survive in the projection"
        );
    }

    #[test]
    fn f3_fenced_code_keeps_the_backslashes_a_reader_sees() {
        let mut fts = SqliteFtsIndex::in_memory(FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        })
        .unwrap();
        // The shape that dominates the eval corpus: a shell fence whose
        // backslashes are content, not escaping. Unescaping these would rewrite
        // the corpus the eval suite searches.
        let fenced = "```sh\nfind . -type f -exec shasum {} \\;\n```\n";
        fts.index_chunk(&row("c1", fenced)).unwrap();
        assert_eq!(indexed_text_of(&fts, "c1"), fenced);
        let hits = fts.search("\"shasum {} \\;\"", 10).unwrap();
        assert_eq!(hits.len(), 1, "code must stay searchable as written");
        assert_eq!(hits[0].chunk_id, "c1");
    }
}
