//! Folder-scope repository operations for Step 1.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Component;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use crate::ExitCode;
use crate::cas::{
    CAS_STREAM_BUFFER_BYTES, ContentObjectKind, MAX_RAW_OBJECT_BYTES, ObjectKind, ObjectStore,
    canonical_json_bytes, is_hash, read_bounded_regular_file,
};
use crate::dag::{
    CommitObject, CommitStats, CommitType, DEFAULT_CHUNKING_MAX_CHARS, DEFAULT_CHUNKING_STRATEGY,
    NormalizeRef, TreeEntry, TreeObject, build_tree_with_chunking_config, chunking_config_hash,
    is_materializable_direct_child,
};
use crate::error::{IoResultExt, KioError, Result};
use crate::gc::{SnapshotAutoBinding, SnapshotAutoStateBinding};
#[cfg(all(test, unix))]
use crate::platform::scope_effective_uid as effective_uid;
use crate::platform::{
    BoundMetadataObservation, configure_private_stage, scheduled_source_identity,
    scope_open_bound_nofollow as open_bound_scope_file_nofollow,
    scope_open_nofollow as open_scope_file_nofollow,
    scope_read_bound_regular_text_observed_at as read_bound_regular_text_observed_at,
    set_scope_directory_permissions, validate_scope_path_private, validate_scope_store_private,
};
use crate::portable::{
    PORTABLE_TAGS_DIRECTORY, portable_leaf_error, portable_tag_collision_key,
    portable_tag_digest64, portable_tag_leaf,
};
use crate::purge::PurgeState;
use crate::schema::{SchemaKind, validate_json_schema};
use crate::store_dir::StoreDirectory;

#[cfg(all(test, windows))]
use crate::platform::scope_same_file_identity as same_scope_file_identity;

mod construction;
mod control;
pub(crate) mod locks;
mod managed_restore;

mod approvals;
mod config;
mod history;
mod logging;
mod repository;
mod snapshot;
mod working_tree;
pub(crate) use approvals::validate_bound_store_lock_authority;
pub use approvals::{
    NETWORK_APPROVAL_EXECUTION_MODE, network_approval_active, network_approval_row_present,
    network_approvals_initialized, publish_network_approval, read_network_approval_pending,
    read_network_approvals, revoke_network_approval, write_network_approval_pending,
};
use approvals::{
    network_approval_active_from_value, network_approval_pending_from_value,
    network_approval_row_present_from_value, network_approvals_from_value,
    network_approvals_initialized_from_value, publish_network_approval_in_directory,
    read_bound_config_document, read_bound_scope_json_value, revoke_network_approval_in_directory,
    validate_scope_config_document, validate_scope_json_value, validate_stable_store_gate,
    validate_store_lock_authority, write_network_approval_pending_in_directory,
};
pub use logging::{
    DEFAULT_LOG_RETENTION_DAYS, append_error_log, append_event_log, append_jsonl_rotating,
    append_warn_log, read_adapter_lane, read_logs_retention_days,
};
pub(crate) use logging::{append_jsonl_rotating_with_control, append_observation};
#[cfg(test)]
use logging::{prune_rotated_logs, redact_context, redact_message_paths, rotate_stale_log};

pub use locks::{LockSession, StoreLock};
use locks::{
    acquire_retained_lock_kind_with_control, acquire_retained_store_lock,
    acquire_retained_store_lock_with_control,
};
pub(crate) use managed_restore::reject_pending_managed_restore;
pub use managed_restore::{
    ManagedRestoreChange, ManagedRestoreOutcome, ManagedRestorePlan, ManagedRestoreRequest,
};

/// Exact on-disk scope format understood by this pre-stable reader.
pub const KIO_FORMAT_VERSION: &str = "3.0.0";
pub const DEFAULT_MAX_ARCHIVE_FILE_BYTES: u64 = MAX_RAW_OBJECT_BYTES;
pub const DEFAULT_MAX_ARCHIVE_SCOPE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub use crate::dag::MAX_TREE_ENTRIES;
const MAX_TAG_REF_BYTES: u64 = 128;
const MAX_TAG_NAMES_BYTES: u64 = 16 * 1024 * 1024;
const PUBLICATION_JOURNAL_LEAF: &str = "publication-v1.json";
const MAX_PUBLICATION_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;

/// Durable intent for the only mutable history publication.  The journal is
/// deliberately separate from the CAS: it records the two mutable files that
/// cannot be committed as one filesystem transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationJournal {
    version: u8,
    expected_head: Option<String>,
    new_head: String,
    manifest: Value,
}
/// Bound structured scope metadata on every supported operating system.
/// Management and policy reads must never become unbounded parser inputs.
const MAX_BOUND_CONFIG_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveLimits {
    pub max_file_bytes: u64,
    pub max_scope_bytes: u64,
}

impl ArchiveLimits {
    #[must_use]
    pub const fn new(max_file_bytes: u64, max_scope_bytes: u64) -> Self {
        Self {
            max_file_bytes,
            max_scope_bytes,
        }
    }
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self::new(
            DEFAULT_MAX_ARCHIVE_FILE_BYTES,
            DEFAULT_MAX_ARCHIVE_SCOPE_BYTES,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScopeIdentity {
    pub scope_id: String,
    pub canonical_root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingNormalizeRef {
    pub expected_raw_hash: String,
    pub normalize: NormalizeRef,
}

#[derive(Debug)]
struct WorkingFileCandidate {
    path: PathBuf,
    file_name: String,
    /// Bound-child candidates are re-opened relative to this retained scope
    /// directory. `path` remains diagnostic-only in that mode.
    bound_root: Option<Arc<File>>,
}

#[derive(Debug)]
struct StagedWorkingFile {
    candidate: WorkingFileCandidate,
    temp_path: PathBuf,
    file: File,
    raw_hash: String,
    size_bytes: u64,
}

impl Drop for StagedWorkingFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.temp_path);
    }
}

#[derive(Debug, Clone)]
pub struct Repository {
    lock_session: LockSession,
    root: PathBuf,
    /// Stable public scope identity. Ordinary repositories use the same path
    /// as `root`; bound child indexing keeps it separate from operational I/O.
    canonical_root: PathBuf,
    kio_dir: PathBuf,
    store: ObjectStore,
    /// Retained descriptors used only by an internal child-index process.
    ///
    /// A bound repository retains both authority directories.  Operational
    /// I/O must resolve from these handles, never from a replaceable public
    /// `.kio` path or process-wide current directory.
    bound_root: Option<Arc<File>>,
    bound_kio: Option<Arc<File>>,
}

impl Repository {
    pub fn with_verified_reads(
        &self,
        reads: &crate::cas::VerifiedCasReadCapability,
    ) -> Result<Self> {
        let mut repo = self.clone();
        repo.store = self.store.with_verified_reads(reads)?;
        Ok(repo)
    }

    /// Clone the repository's retained CAS authority for bounded object reads.
    #[must_use]
    pub fn object_store(&self) -> ObjectStore {
        self.store.clone()
    }
}

#[derive(Debug, Clone)]
pub struct WorkingTree {
    pub tree: TreeObject,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    pub relative_path: String,
    /// The working-tree-vs-HEAD classification (`new`/`modified`/`deleted`/
    /// `unchanged`). R15-4: `None` (field omitted) when HEAD is shallow — the prior
    /// tree needed to classify is gone, so a pure `kio status` degrades to listing
    /// current files without a classification rather than dying.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_hash: Option<String>,
}

/// The result of [`Repository::status`]: the per-file statuses plus a degradation
/// flag. R15-4: when HEAD names a commit whose tree object is gone (shallow), the
/// per-file `status` classification cannot be computed — `head_shallow` is `true`,
/// each file's `status` is omitted, and the command still succeeds (exit 0) so the
/// scope stays inspectable instead of bricking on a raw `KIO-E-STORE-NOT-FOUND-001`.
#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    pub files: Vec<FileStatus>,
    pub head_shallow: bool,
}

/// R15-4: the availability of the HEAD commit's tree object.
enum HeadTreeState {
    /// No HEAD commit — an unborn branch (a first snapshot creates the root).
    Unborn,
    /// HEAD names a commit whose TREE object is gone, OR whose COMMIT object itself
    /// is gone (shallow: GC'd / manually deleted / corrupt CAS). R16-1 folds the
    /// missing-commit case in here too: it is the same corruption class as a missing
    /// tree, with the same policy — pure reads degrade; writes must fail loudly. (The
    /// write paths carry their own HEAD hash for the error context, so this variant
    /// needs no payload.)
    Shallow,
    /// HEAD's commit and tree object are both present.
    Present(TreeObject),
}

#[derive(Debug, Clone)]
pub struct SnapshotOutcome {
    pub noop: bool,
    pub message: String,
    pub tree_hash: String,
    pub commit_hash: Option<String>,
    pub commit: Option<CommitObject>,
    pub stats: CommitStats,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub commit_hash: String,
    #[serde(flatten)]
    pub commit: CommitObject,
}

/// The result of [`Repository::log`]: the reachable history plus a truncation flag.
/// R16-1: when an ancestor commit object is gone (shallow / external corruption),
/// history traversal stops at that point and returns the healthy prefix from HEAD
/// with `truncated = true`, rather than bricking the whole `log` on a raw
/// `KIO-E-STORE-NOT-FOUND-001`. The omission is always explicit, never silent.
#[derive(Debug, Clone, Serialize)]
pub struct LogReport {
    pub entries: Vec<LogEntry>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    pub relative_path: String,
    pub change: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_raw_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_raw_hash: Option<String>,
}

#[derive(Debug, Clone)]
pub enum InspectedObject {
    Tree(TreeObject),
    Commit(CommitObject),
    Raw { raw_hash: String, size_bytes: u64 },
}

/// Create the first `.kio` tree through the already no-follow-opened directory
/// handle. This is intentionally separate from [`Repository::init`]: a bound
/// child must not use `create_dir_all` or any public child path while its name
/// can be replaced by another same-UID process.
fn initialize_bound_kio_layout_directory(
    directory: &StoreDirectory,
    canonical_root: &Path,
) -> Result<()> {
    initialize_planned_kio_layout(directory, canonical_root, &new_ulid(canonical_root))
}

const PLANNED_KIO_DIRECTORIES: &[&str] = &[
    "objects/raw",
    "objects/trees",
    "objects/commits",
    "logs",
    "refs/tags-v1",
];
const PLANNED_KIO_ROOT_DIRECTORIES: &[&str] = &["objects", "logs", "refs"];

/// Populate a private, caller-owned staging directory with a complete unborn
/// store whose identity was durably planned before allocation. This does not
/// activate management authority or publish the stage at the scope root.
/// Every initial leaf is create-only; an interrupted stage must be handled by
/// its owning journal rather than adopted as an existing live repository.
pub fn initialize_planned_kio_layout(
    directory: &StoreDirectory,
    canonical_root: &Path,
    scope_id: &str,
) -> Result<()> {
    let files = planned_kio_files(canonical_root, scope_id)?;
    for name in files.keys() {
        if directory
            .read_optional(Path::new(name), MAX_BOUND_CONFIG_BYTES)?
            .is_some()
        {
            return Err(KioError::schema("planned initial leaf already exists"));
        }
    }
    for relative in PLANNED_KIO_DIRECTORIES {
        directory.create_directory_all(Path::new(relative))?;
    }
    for (name, bytes) in &files {
        directory.write_atomic(
            Path::new(name),
            bytes,
            crate::store_dir::Publication::CreateOnly,
        )?;
    }
    Ok(())
}

/// Resume a private planned `.kio` stage only when every extant entry belongs
/// to the unborn-store inventory and every extant leaf has its exact planned
/// bytes. Missing entries are then filled through retained create-only writes.
pub fn complete_planned_kio_layout(
    directory: &StoreDirectory,
    canonical_root: &Path,
    scope_id: &str,
    expected_aux: &[(&str, &[u8])],
) -> Result<()> {
    use crate::store_dir::Publication;
    let files = planned_kio_layout_files(canonical_root, scope_id, expected_aux)?;
    validate_planned_stage(directory, &files)?;
    directory.recover_atomic(&[directory])?;
    validate_planned_stage(directory, &files)?;
    for relative in PLANNED_KIO_DIRECTORIES {
        directory.create_directory_all(Path::new(relative))?;
    }
    for (name, bytes) in &files {
        if !planned_leaf_present(directory, Path::new(name), bytes)? {
            directory.write_atomic(Path::new(name), bytes, Publication::CreateOnly)?;
        }
    }
    validate_planned_stage(directory, &files)
}

/// Validate a private planned `.kio` stage without creating or changing any
/// entry. The stage may be empty or partial, but every existing entry must be
/// part of the unborn-store inventory and have its exact planned bytes.
pub fn validate_planned_kio_layout(
    directory: &StoreDirectory,
    canonical_root: &Path,
    scope_id: &str,
    expected_aux: &[(&str, &[u8])],
) -> Result<()> {
    let files = planned_kio_layout_files(canonical_root, scope_id, expected_aux)?;
    validate_planned_stage(directory, &files)
}

fn planned_kio_layout_files(
    canonical_root: &Path,
    scope_id: &str,
    expected_aux: &[(&str, &[u8])],
) -> Result<std::collections::BTreeMap<String, Vec<u8>>> {
    let mut files = planned_kio_files(canonical_root, scope_id)?;
    for (name, bytes) in expected_aux {
        validate_planned_aux_name(name)?;
        if PLANNED_KIO_ROOT_DIRECTORIES.contains(name)
            || files.insert((*name).to_owned(), bytes.to_vec()).is_some()
        {
            return Err(KioError::schema(
                "planned auxiliary leaf conflicts with built-in layout",
            ));
        }
    }
    Ok(files)
}

fn planned_kio_files(
    canonical_root: &Path,
    scope_id: &str,
) -> Result<std::collections::BTreeMap<String, Vec<u8>>> {
    validate_planned_layout_inputs(canonical_root, scope_id)?;
    let mut files = std::collections::BTreeMap::new();
    files.insert("HEAD".into(), b"unborn\n".to_vec());
    files.insert("config.toml".into(), Vec::new());
    files.insert(".store-gate".into(), Vec::new());
    files.insert("scope.json".into(), serde_json::to_string_pretty(&json!({"kio_format_version": KIO_FORMAT_VERSION,"scope_id": scope_id,"scope_path": canonical_root})).map_err(|e| KioError::schema(e.to_string()))?.into_bytes());
    files.insert(
        "manifest.json".into(),
        b"{\n  \"schema_version\": 1,\n  \"files\": []\n}\n".to_vec(),
    );
    files.insert(
        "tool-lock.json".into(),
        b"{\n  \"spec_version\": 1\n}\n".to_vec(),
    );
    Ok(files)
}

fn validate_planned_layout_inputs(canonical_root: &Path, scope_id: &str) -> Result<()> {
    if !is_ulid(scope_id) {
        return Err(KioError::schema("planned scope_id must be a ULID"));
    }
    if !canonical_root.is_absolute()
        || canonical_root
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(KioError::invalid_usage(
            "planned scope root must be absolute and canonical",
        ));
    }
    Ok(())
}

fn validate_planned_aux_name(name: &str) -> Result<()> {
    if name == crate::store_dir::ATOMIC_WORKSPACE_DIR || name == crate::management::CASE_PROBE_LEAF
    {
        return Err(KioError::schema(
            "planned auxiliary leaf conflicts with reserved recovery state",
        ));
    }
    if name.as_bytes().contains(&0) {
        return Err(KioError::schema(
            "planned auxiliary leaf must be one normal filename",
        ));
    }
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(KioError::schema(
            "planned auxiliary leaf must be one normal filename",
        ));
    }
    Ok(())
}

/// Observe presence only after checking the exact planned contents.
fn planned_leaf_present(
    directory: &StoreDirectory,
    relative: &Path,
    expected: &[u8],
) -> Result<bool> {
    let matches = if expected.is_empty() {
        // A caller may hold an exclusive lock on an empty gate. On Windows a
        // ReadFile request through another handle then fails even at EOF. The
        // no-follow, single-link regular open and retained metadata establish
        // the empty-content invariant without issuing any range read.
        let file = match directory.open_regular_read(relative, 0) {
            Ok(file) => file,
            Err(error)
                if error.error_code() == "KIO-E-STORE-IO-001"
                    && error.context().get("io_error_kind").and_then(Value::as_str)
                        == Some("not_found") =>
            {
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        file.metadata()
            .kio_io(&directory.path().join(relative))?
            .len()
            == 0
    } else {
        let Some(actual) = directory.read_optional(relative, expected.len() as u64)? else {
            return Ok(false);
        };
        actual == expected
    };
    if !matches {
        return Err(KioError::schema(
            "planned stage leaf differs from expected bytes",
        ));
    }
    Ok(true)
}

fn validate_planned_stage(
    directory: &StoreDirectory,
    files: &std::collections::BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    // Only the exact, bounded private workspace and reserved case probe may
    // accompany a partial layout. Read-only validation never clears either.
    directory.inspect_atomic()?;
    crate::management::validate_case_probe(directory)?;
    let allowed_root: std::collections::BTreeSet<String> = files
        .keys()
        .cloned()
        .chain([
            "objects".into(),
            "logs".into(),
            "refs".into(),
            crate::store_dir::ATOMIC_WORKSPACE_DIR.into(),
            crate::management::CASE_PROBE_LEAF.into(),
        ])
        .collect();
    validate_planned_root_entries(directory, &allowed_root, files)?;
    for (path, allowed) in [
        ("objects", &["raw", "trees", "commits"][..]),
        ("refs", &["tags-v1"][..]),
    ] {
        let allowed = allowed.iter().map(|name| (*name).to_owned()).collect();
        validate_planned_entries(directory, Path::new(path), &allowed, files)?;
    }
    for path in PLANNED_KIO_DIRECTORIES {
        validate_planned_entries(
            directory,
            Path::new(path),
            &std::collections::BTreeSet::new(),
            files,
        )?;
    }
    for (name, expected) in files {
        planned_leaf_present(directory, Path::new(name), expected)?;
    }
    Ok(())
}
fn validate_planned_entries(
    directory: &StoreDirectory,
    path: &Path,
    allowed: &std::collections::BTreeSet<String>,
    files: &std::collections::BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    let Some(entries) = directory.entries_optional(path)? else {
        return Ok(());
    };
    for entry in entries {
        let name = entry.name.to_string_lossy().to_string();
        if !allowed.contains(&name) {
            return Err(KioError::schema("planned stage contains an unknown entry"));
        }
        if files.contains_key(&name) {
            if !entry.is_regular_file {
                return Err(KioError::schema("planned stage leaf is unsafe"));
            }
        } else if !entry.is_directory {
            return Err(KioError::schema("planned stage directory is unsafe"));
        }
    }
    Ok(())
}
fn validate_planned_root_entries(
    directory: &StoreDirectory,
    allowed: &std::collections::BTreeSet<String>,
    files: &std::collections::BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    let entries = directory.entries(Path::new(""))?;
    for entry in entries {
        let name = entry.name.to_string_lossy().to_string();
        if !allowed.contains(&name) {
            return Err(KioError::schema("planned stage contains an unknown entry"));
        }
        if files.contains_key(&name) || name == crate::management::CASE_PROBE_LEAF {
            if !entry.is_regular_file {
                return Err(KioError::schema("planned stage leaf is unsafe"));
            }
        } else if !entry.is_directory {
            return Err(KioError::schema("planned stage directory is unsafe"));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScheduledSnapshotAuthority {
    head: String,
    tool_lock: String,
    head_observation: BoundMetadataObservation,
    tool_lock_observation: BoundMetadataObservation,
}

pub(crate) fn canonical_tool_lock_value(value: &Value) -> Result<Value> {
    let object = value
        .as_object()
        .ok_or_else(|| KioError::schema("tool-lock.json must be an object"))?;
    let spec_version = object
        .get("spec_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| KioError::schema("tool-lock.json missing spec_version"))?;
    if spec_version != 1 {
        return Err(KioError::schema(format!(
            "unsupported tool-lock spec_version: {spec_version}"
        )));
    }
    for key in object.keys() {
        if key != "spec_version" && !["prepare", "markdown", "embedding"].contains(&key.as_str()) {
            return Err(KioError::schema(format!(
                "unknown tool-lock.json role `{key}`"
            )));
        }
    }
    let mut canonical = Map::new();
    canonical.insert("spec_version".to_owned(), Value::from(spec_version));
    for key in ["prepare", "markdown"] {
        if let Some(entry) = canonical_tool_entry(object, key, false)? {
            canonical.insert(key.to_owned(), entry);
        }
    }
    if let Some(entry) = canonical_tool_entry(object, "embedding", true)? {
        canonical.insert("embedding".to_owned(), entry);
    }
    Ok(Value::Object(canonical))
}

fn canonical_tool_entry(
    object: &Map<String, Value>,
    key: &str,
    embedding: bool,
) -> Result<Option<Value>> {
    let Some(entry) = object.get(key) else {
        return Ok(None);
    };
    if entry.is_null() {
        return Ok(None);
    }
    let entry = entry
        .as_object()
        .ok_or_else(|| KioError::schema(format!("{key} must be an object")))?;
    let allowed_fields: &[&str] = match key {
        "prepare" => &["tool_id", "profile_hash", "kind"],
        "markdown" => &["tool_id", "profile_hash", "kind", "capabilities"],
        "embedding" => &[
            "tool_id",
            "dimensions",
            "distance",
            "modality",
            "profile_hash",
            "kind",
            "mode",
        ],
        _ => unreachable!("tool-lock entry role was validated"),
    };
    for field in entry.keys() {
        if !allowed_fields.contains(&field.as_str()) {
            return Err(KioError::schema(format!(
                "unknown field `{key}.{field}` in tool-lock.json"
            )));
        }
    }
    let mut canonical = Map::new();
    if embedding {
        canonical.insert(
            "dimensions".to_owned(),
            required_lock_integer(entry, key, "dimensions")?,
        );
        canonical.insert(
            "distance".to_owned(),
            required_lock_string(entry, key, "distance")?,
        );
        canonical.insert(
            "modality".to_owned(),
            required_lock_string(entry, key, "modality")?,
        );
    }
    canonical.insert(
        "profile_hash".to_owned(),
        required_lock_string(entry, key, "profile_hash")?,
    );
    canonical.insert(
        "tool_id".to_owned(),
        required_lock_string(entry, key, "tool_id")?,
    );
    Ok(Some(Value::Object(canonical)))
}

fn required_lock_string(object: &Map<String, Value>, key: &str, field: &str) -> Result<Value> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(|value| Value::String(value.to_owned()))
        .ok_or_else(|| KioError::schema(format!("{key}.{field} must be a string")))
}

fn required_lock_integer(object: &Map<String, Value>, key: &str, field: &str) -> Result<Value> {
    object
        .get(field)
        .and_then(Value::as_u64)
        .map(Value::from)
        .ok_or_else(|| KioError::schema(format!("{key}.{field} must be an integer")))
}

/// R12-2 / R12-1: enforce the *semantics* of documented config keys that the JSON
/// Schema can only type-check. A key whose value selects behavior Kio has not
/// implemented is rejected LOUDLY (`KIO-E-CONFIG-NOT-IMPLEMENTED-001`, exit 1 —
/// R9-6 convention) rather than silently ignored, but the documented DEFAULT value
/// is always accepted as a harmless no-op so pasting the docs/07 §7 `[adapter.policy]`
/// block (all defaults) never bricks a scope (the R12-2 failure mode). Called on
/// every scope-config load (`validate_config`) and on the user-config load.
///
/// Wired keys (`allow_network`, `redact_logs`, `max_input_bytes`, the
/// `markdownize.incremental` enabled/threshold/max_consecutive, and the whole
/// `[search]` block) are NOT checked here — they change behavior, they are not
/// rejected.
pub(crate) fn enforce_scope_config_semantics(config: &Value) -> Result<()> {
    enforce_config_semantics(config)?;
    if config
        .get("adapter")
        .and_then(|adapter| adapter.get("policy"))
        .and_then(|policy| policy.get("offline_api"))
        .and_then(|offline| offline.get("ca_pem_path"))
        .is_some()
    {
        return Err(KioError::not_implemented(
            "adapter.policy.offline_api.ca_pem_path is device-local and cannot be stored in .kio/config.toml",
        ));
    }
    Ok(())
}

pub fn enforce_config_semantics(config: &Value) -> Result<()> {
    // Child scopes persist the parent's rules through a bounded, strict
    // generated-policy envelope. Reject source config that cannot make that
    // transition before a later child discovery can turn it into a partial
    // index result. JSON Schema enforces the structural maximums; this closes
    // the byte-length and bare-negation gaps JSON Schema cannot express here.
    if let Some(ignore) = config
        .get("scope")
        .and_then(|scope| scope.get("ignore"))
        .and_then(Value::as_array)
    {
        for value in ignore {
            let pattern = value
                .as_str()
                .ok_or_else(|| KioError::schema("scope.ignore entries must be strings"))?;
            let effective = pattern.trim_start_matches('!');
            if effective.is_empty() || effective.len() > 4_096 {
                return Err(KioError::schema(
                    "scope.ignore entries must contain a bounded pattern",
                ));
            }
        }
    }
    if let Some(policy) = config
        .get("adapter")
        .and_then(|adapter| adapter.get("policy"))
    {
        // allowed_scope: only "." (scope containment, 07 §7.1.2 P1) is implemented.
        if let Some(scope) = policy.get("allowed_scope").and_then(Value::as_str)
            && scope != "."
        {
            return Err(KioError::not_implemented(
                "adapter.policy.allowed_scope other than \".\"",
            ));
        }
        // Request/response body persistence is never done (07 §7 "ログ本文禁止" —
        // only hashes are logged), so a `true` request is unimplemented.
        if policy.get("store_request_body").and_then(Value::as_bool) == Some(true) {
            return Err(KioError::not_implemented(
                "adapter.policy.store_request_body = true",
            ));
        }
        if policy.get("store_response_body").and_then(Value::as_bool) == Some(true) {
            return Err(KioError::not_implemented(
                "adapter.policy.store_response_body = true",
            ));
        }
        // The first-run command/URL approval flow (07 §7) is mandatory and cannot
        // be turned off.
        if policy
            .get("require_command_confirmation")
            .and_then(Value::as_bool)
            == Some(false)
        {
            return Err(KioError::not_implemented(
                "adapter.policy.require_command_confirmation = false",
            ));
        }
        // timeout_seconds: a per-adapter execution timeout is not threaded through
        // the adapter HTTP path (it would touch every adapter's transport). Accept
        // the documented default (300); reject any other value loudly rather than
        // silently ignore it. (R12-2 decision: real wiring is a large change.)
        if let Some(timeout) = policy.get("timeout_seconds").and_then(Value::as_i64)
            && timeout != 300
        {
            return Err(KioError::not_implemented(
                "adapter.policy.timeout_seconds other than 300",
            ));
        }
        // D7 (07 §7): `[adapter.policy.<execution_mode>]` overrides the parent
        // for that mode. Only `offline_api` is wired -- the CLI reads it into
        // the execution-timeout registry and the offline embedding client is
        // built under it. The other two modes are rejected here rather than
        // accepted, because a value nothing honours is worse than a value
        // refused: today's loud error would become a silent no-op, and the
        // operator would believe a timeout is in force that is not.
        for mode in ["online_api", "deterministic_library"] {
            if policy.get(mode).is_some() {
                return Err(KioError::not_implemented(format!(
                    "adapter.policy.{mode} (only offline_api is wired)"
                )));
            }
        }
    }
    // QA61 (step4b-contract-tests-p3a.md §R, arbitration #7): `include_neighbors`
    // was removed from `config.schema.json` entirely (it disappeared from the
    // documented config example with no implementation concept, R12-1) — an
    // unknown key is now a schema error at the JSON-schema layer
    // (`additionalProperties: false`), so this function no longer needs a
    // dedicated NOT-IMPLEMENTED check for it.
    Ok(())
}

/// Reject a tag-name / commit-ref operand that could escape `refs/tags` when
/// joined onto the store path (N4, 03 §3 scope boundary). A ref is only ever
/// `HEAD`, a hash, or a tag name, so a path separator (`/` or `\`), `.`/`..`,
/// an absolute path, or any `ParentDir`/`RootDir`/`Prefix` component is always
/// a traversal attempt. Shared by `tag()` and `resolve_commit()`.
fn validate_ref_operand(value: &str) -> Result<()> {
    let traversal = value.is_empty()
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || Path::new(value).is_absolute()
        || Path::new(value).components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        });
    if traversal {
        return Err(KioError::invalid_usage(
            "commit reference must not contain path separators or `.`/`..` traversal",
        ));
    }
    Ok(())
}

fn validate_tag_name(name: &str) -> Result<()> {
    validate_ref_operand(name)?;
    if crate::portable::portable_tag_name_has_unassigned(name) {
        return Err(KioError::invalid_usage(
            "tag name contains a character unassigned in Unicode 16",
        ));
    }
    // `resolve_commit` resolves HEAD and hash operands before tag refs. A tag
    // under either name would be permanently shadowed, so reject it for both
    // creation and deletion while leaving those valid commit operands intact.
    let collision_key = portable_tag_collision_key(name);
    if collision_key == "head" || is_hash(&collision_key) {
        return Err(KioError::invalid_usage(
            "tag name collides with `HEAD` or a commit hash",
        ));
    }
    if let Some(reason) = portable_leaf_error(name) {
        return Err(KioError::invalid_usage(format!(
            "tag name is not a portable filesystem leaf: {reason}"
        )));
    }
    Ok(())
}

/// `.kio/refs/tags-v1/names.jsonl` (03-data-model.md §2 L80/141): the
/// append-only logical-tag-name ledger, co-located with the canonical
/// `tags-v1/` ref directory it describes. Public so fsck (verify_objects.rs,
/// PB07-09) can locate the same path without re-deriving the convention.
#[must_use]
pub fn names_jsonl_path(kio_dir: &Path) -> PathBuf {
    kio_dir
        .join("refs")
        .join(PORTABLE_TAGS_DIRECTORY)
        .join("names.jsonl")
}

/// Validate a store-owned tag directory without following a symlink/junction.
/// `allow_missing` supports opening pre-portability stores that do not yet have
/// the versioned canonical directory; the first tag write creates it under the
/// store lock and validates it again.
fn validate_tag_refs_directory(path: &Path, allow_missing: bool) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && allow_missing => {
            return Ok(false);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(tag_ref_corrupt(path, "tag refs directory is missing"));
        }
        Err(error) => return Err(KioError::io(error.to_string(), path.display().to_string())),
    };
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(unsafe_store_error(
            path,
            "tag refs path must be a real directory inside the store",
        ));
    }
    crate::platform::scope_semantics::validate_tag_directory_native(path)?;
    let resolved = path.canonicalize().kio_io(path)?;
    let expected = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?
            .join(path)
    };
    if resolved != expected {
        return Err(unsafe_store_error(
            path,
            "tag refs directory resolves outside its store path",
        ));
    }
    Ok(true)
}

/// Add canonical tag targets while excluding the adjacent logical-name ledger.
fn collect_tag_ref_targets(path: &Path, targets: &mut BTreeSet<String>) -> Result<()> {
    if !validate_tag_refs_directory(path, true)? {
        return Ok(());
    }
    for entry in fs::read_dir(path).kio_io(path)? {
        let entry = entry.kio_io(path)?;
        let leaf = entry.file_name();
        let leaf = leaf
            .to_str()
            .ok_or_else(|| tag_ref_corrupt(&entry.path(), "tag ref leaf is not UTF-8"))?;
        if leaf == "names.jsonl" {
            continue;
        }
        let valid = leaf.strip_prefix("tag-").is_some_and(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        });
        if !valid {
            return Err(tag_ref_corrupt(
                &entry.path(),
                "canonical tag leaf is invalid",
            ));
        }
        // Keep the existing single-link/no-follow reader as the sole parser
        // for tag contents, including its bounded-size and canonical-hash
        // checks.
        targets.insert(read_tag_ref(&entry.path())?);
    }
    Ok(())
}

/// Read one bounded, regular, single-link commit ref without following it.
/// Only the literal `unborn` sentinel represents an empty history. Empty,
/// missing, or malformed HEAD is corruption and is never treated as genesis.
fn read_commit_ref(path: &Path, allow_unborn: bool) -> Result<Option<String>> {
    let listed = fs::symlink_metadata(path).kio_io(path)?;
    if !listed.file_type().is_file() || listed.len() > MAX_TAG_REF_BYTES {
        return Err(tag_ref_corrupt(
            path,
            "commit ref is not a bounded regular file",
        ));
    }
    crate::platform::scope_semantics::validate_commit_ref_links(&listed, path)?;
    let file = open_scope_file_nofollow(path)?;
    let opened = file.metadata().kio_io(path)?;
    if !opened.is_file() || opened.len() != listed.len() || opened.len() > MAX_TAG_REF_BYTES {
        return Err(tag_ref_corrupt(path, "commit ref changed while opening"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_TAG_REF_BYTES + 1)
        .read_to_end(&mut bytes)
        .kio_io(path)?;
    if bytes.len() as u64 > MAX_TAG_REF_BYTES {
        return Err(tag_ref_corrupt(
            path,
            "commit ref exceeds the bounded size limit",
        ));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| tag_ref_corrupt(path, "commit ref is not UTF-8"))?;
    if allow_unborn {
        return parse_head_ref(text).map_err(|_| tag_ref_corrupt(path, "HEAD is not canonical"));
    }
    let hash = text.trim();
    if !is_hash(hash) {
        return Err(tag_ref_corrupt(
            path,
            "commit ref target is not a canonical commit hash",
        ));
    }
    Ok(Some(hash.to_owned()))
}

/// Parse the one mutable ref's fixed wire format.  `unborn\n` is the only
/// empty-history representation; every committed HEAD is a canonical hash plus
/// exactly one trailing newline.  This keeps empty and whitespace-only files
/// distinguishable from an initialized repository.
fn parse_head_ref(text: &str) -> Result<Option<String>> {
    if text == "unborn\n" {
        return Ok(None);
    }
    let Some(hash) = text.strip_suffix('\n') else {
        return Err(KioError::schema(
            "HEAD must use the canonical newline-terminated format",
        ));
    };
    if is_hash(hash) {
        Ok(Some(hash.to_owned()))
    } else {
        Err(KioError::schema("HEAD must contain a commit_hash"))
    }
}

fn read_tag_ref(path: &Path) -> Result<String> {
    read_commit_ref(path, false)?.ok_or_else(|| tag_ref_corrupt(path, "tag ref is empty"))
}

pub(crate) fn tag_ref_corrupt(path: &Path, message: &str) -> KioError {
    KioError::new(
        "KIO-E-STORE-CORRUPT-001",
        message,
        json!({ "path": path }),
        ExitCode::PermanentFailure,
    )
}

fn data_home(control: &crate::control::ControlHandle) -> PathBuf {
    // R12-6 / R13-6: empty/relative `XDG_DATA_HOME` AND empty/relative `HOME` are
    // both invalid — fall back to `$HOME/.local/share` only for an absolute `HOME`,
    // never to a CWD-relative dir. The CLI startup guard rejects the no-absolute-
    // base case loudly before we reach the `"."` last resort.
    crate::xdg::xdg_dir("XDG_DATA_HOME")
        .or_else(|| {
            crate::xdg::home_dir_with_control(control).map(|home| home.join(".local/share"))
        })
        .unwrap_or_else(|| PathBuf::from("."))
}

/// QA7 (step4b-contract-tests-p3a.md §B, arbitration #1): the built-in Tier A
/// pattern set as data, not just imperative comparisons — the single source
/// both [`is_tier_a_secret_name`] and [`tier_a_template_text`] (10 §1.1's
/// `effective_ignore_hash` input) consult, so a pattern-list edit is
/// mechanically guaranteed to change the hash instead of relying on someone
/// remembering to bump a version literal.
const TIER_A_EXACT_PATHS: &[&str] = &[".kube/config", ".docker/config.json"];
const TIER_A_PATH_PREFIXES: &[&str] = &[".ssh/", ".gnupg/", ".aws/", ".kube/", ".docker/"];
const TIER_A_EXACT_NAMES: &[&str] = &[
    ".env", ".ssh", ".gnupg", ".aws", ".kube", ".docker", ".netrc", ".npmrc", ".pypirc",
];
const TIER_A_NAME_PREFIXES: &[&str] = &[".env.", "id_rsa", "id_ecdsa", "id_ed25519"];
const TIER_A_NAME_SUFFIXES: &[&str] = &[".pem", ".key", ".p12", ".pfx", ".keystore", ".tfstate"];
const TIER_A_NAME_CONTAINS: &[&str] = &[".tfstate."];

/// Built-in Tier-A name policy applied again at the closing archive read.
/// Keep this predicate aligned with `kio_pipeline::scan::classify_secret`;
/// callers pass explicitly unignored Tier-A paths separately.
#[must_use]
pub fn is_tier_a_secret_name(path: &str) -> bool {
    let normalized = path.trim_start_matches('/').replace('\\', "/");
    let name = normalized.rsplit('/').next().unwrap_or(&normalized);
    let lower = name.to_ascii_lowercase();
    let lower_path = normalized.to_ascii_lowercase();
    TIER_A_EXACT_PATHS.contains(&lower_path.as_str())
        || TIER_A_PATH_PREFIXES
            .iter()
            .any(|prefix| lower_path.starts_with(prefix))
        || TIER_A_EXACT_NAMES.contains(&lower.as_str())
        || TIER_A_NAME_PREFIXES
            .iter()
            .any(|prefix| lower.starts_with(prefix))
        || TIER_A_NAME_SUFFIXES
            .iter()
            .any(|suffix| lower.ends_with(suffix))
        || TIER_A_NAME_CONTAINS
            .iter()
            .any(|needle| lower.contains(needle))
}

/// QA7: a canonical, deterministic text rendering of every built-in Tier
/// A/B pattern (this module's Tier A arrays plus
/// `kio_pipeline::scan::TIER_B_NEEDLES`) — the input to 10 §1.1's
/// `effective_ignore_hash` (`hash_bytes(tier_a_template_text().as_bytes())`
/// at the call site). A pattern addition/removal/edit in any of these arrays
/// changes this text, so it changes the hash — the property 10 §1.1
/// requires ("パターン更新が承認記録の同一性判定に反映される") that a fixed
/// version-string literal could not guarantee.
#[must_use]
pub fn tier_a_template_text(tier_b_needles: &[&str]) -> String {
    let mut text = String::new();
    for (label, patterns) in [
        ("exact_paths", TIER_A_EXACT_PATHS),
        ("path_prefixes", TIER_A_PATH_PREFIXES),
        ("exact_names", TIER_A_EXACT_NAMES),
        ("name_prefixes", TIER_A_NAME_PREFIXES),
        ("name_suffixes", TIER_A_NAME_SUFFIXES),
        ("name_contains", TIER_A_NAME_CONTAINS),
    ] {
        text.push_str(label);
        text.push(':');
        text.push_str(&patterns.join(","));
        text.push('\n');
    }
    text.push_str("tier_b_needles:");
    text.push_str(&tier_b_needles.join(","));
    text.push('\n');
    text
}

fn validate_store_directory(kio_dir: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(kio_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(KioError::invalid_usage("not a kio scope"));
        }
        Err(error) => {
            return Err(KioError::io(
                error.to_string(),
                kio_dir.display().to_string(),
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(unsafe_store_error(
            kio_dir,
            ".kio must be a real directory inside the selected scope root",
        ));
    }
    let resolved = kio_dir.canonicalize().kio_io(kio_dir)?;
    // Bound child indexing intentionally operates with the relative `.kio`
    // path below a retained cwd. Compare canonical paths in the same form;
    // ordinary callers already pass an absolute root-derived path.
    let declared = if kio_dir.is_absolute() {
        kio_dir.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|err| KioError::io(err.to_string(), "."))?
            .join(kio_dir)
    };
    if resolved != declared {
        return Err(unsafe_store_error(
            kio_dir,
            ".kio resolves outside the selected scope root",
        ));
    }

    validate_scope_path_private(&metadata, kio_dir)?;
    Ok(())
}

/// The retained equivalent of [`validate_store_directory`].  Name binding is
/// verified by `ManagementBinding`; this checks the still-open store itself so
/// a permissive or foreign-owned `.kio` is not accepted merely because its
/// descriptor remains valid.
fn validate_retained_store_directory(kio: &File, label: &Path) -> Result<()> {
    validate_scope_store_private(kio, label)
}

pub(crate) fn unsafe_store_error(path: &Path, message: &str) -> KioError {
    KioError::new(
        "KIO-E-STORE-UNSAFE-001",
        message,
        json!({ "kio_path": path }),
        ExitCode::PermanentFailure,
    )
}

fn open_working_file_candidate(
    candidate: &WorkingFileCandidate,
    control: &crate::control::ControlHandle,
) -> Result<File> {
    if let Some(root) = candidate.bound_root.as_deref() {
        return open_bound_scope_file_nofollow(
            root,
            &candidate.file_name,
            &candidate.path,
            control,
        );
    }
    open_scope_file_nofollow(&candidate.path)
}

fn hash_scope_file(
    file: &mut File,
    path: &Path,
    file_name: &str,
    allowed: u64,
    limits: ArchiveLimits,
) -> Result<(String, u64)> {
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; CAS_STREAM_BUFFER_BYTES];
    let mut total = 0_u64;
    loop {
        let read_cap = allowed
            .saturating_sub(total)
            .saturating_add(1)
            .min(buffer.len() as u64) as usize;
        let count = file.read(&mut buffer[..read_cap]).kio_io(path)?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .ok_or_else(|| scope_input_oversized(file_name, limits, u64::MAX))?;
        if total > allowed {
            return Err(scope_input_oversized(file_name, limits, total));
        }
        hasher.update(&buffer[..count]);
    }
    Ok((format!("sha256:{}", hex_digest(&hasher.finalize())), total))
}

fn stage_scope_file(
    kio_dir: &Path,
    candidate: WorkingFileCandidate,
    allowed: u64,
    limits: ArchiveLimits,
    control: &crate::control::ControlHandle,
) -> Result<StagedWorkingFile> {
    let mut source = open_working_file_candidate(&candidate, control)?;
    let metadata = source.metadata().kio_io(&candidate.path)?;
    if metadata.len() > allowed {
        return Err(scope_input_oversized(
            &candidate.file_name,
            limits,
            metadata.len(),
        ));
    }
    let raw_base = kio_dir.join("objects/raw");
    let (temp_path, mut staged) = create_raw_staging_file(&raw_base)?;
    let result = (|| -> Result<(String, u64)> {
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; CAS_STREAM_BUFFER_BYTES];
        let mut total = 0_u64;
        loop {
            let read_cap = allowed
                .saturating_sub(total)
                .saturating_add(1)
                .min(buffer.len() as u64) as usize;
            let count = source
                .read(&mut buffer[..read_cap])
                .kio_io(&candidate.path)?;
            if count == 0 {
                break;
            }
            total = total
                .checked_add(count as u64)
                .ok_or_else(|| scope_input_oversized(&candidate.file_name, limits, u64::MAX))?;
            if total > allowed {
                return Err(scope_input_oversized(&candidate.file_name, limits, total));
            }
            hasher.update(&buffer[..count]);
            staged.write_all(&buffer[..count]).kio_io(&temp_path)?;
        }
        if source.metadata().kio_io(&candidate.path)?.len() != total {
            return Err(scope_file_changed(&candidate.file_name));
        }
        staged.sync_all().kio_io(&temp_path)?;
        Ok((format!("sha256:{}", hex_digest(&hasher.finalize())), total))
    })();
    match result {
        Ok((raw_hash, size_bytes)) => Ok(StagedWorkingFile {
            candidate,
            temp_path,
            file: staged,
            raw_hash,
            size_bytes,
        }),
        Err(error) => {
            drop(staged);
            let _ = fs::remove_file(&temp_path);
            Err(error)
        }
    }
}

fn create_raw_staging_file(parent: &Path) -> Result<(PathBuf, File)> {
    for attempt in 0..16_u8 {
        let path = parent.join(format!(
            ".ingest-{}-{}-{attempt}",
            std::process::id(),
            unix_nanos()
        ));
        let mut options = OpenOptions::new();
        options.write(true).read(true).create_new(true);
        configure_private_stage(&mut options);
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(KioError::io(error.to_string(), path.display().to_string())),
        }
    }
    Err(KioError::io(
        "could not allocate a unique raw-ingest staging file",
        parent.display().to_string(),
    ))
}

/// Remove crash-orphaned raw-ingest transaction files while the caller holds
/// the scope store lock. The namespace walk and aggregate physical bytes are
/// bounded; every candidate is opened no-follow and must be a single-link,
/// bounded regular file before unlinking.
pub fn cleanup_orphan_raw_ingest_temps(kio_dir: &Path) -> Result<u64> {
    const MAX_RAW_DIRECTORY_ENTRIES: usize = 100_000;
    const MAX_ORPHAN_BYTES: u64 = DEFAULT_MAX_ARCHIVE_SCOPE_BYTES;

    validate_store_directory(kio_dir)?;
    let objects = kio_dir.join("objects");
    let raw_base = objects.join("raw");
    for directory in [&objects, &raw_base] {
        validate_ingest_directory(directory)?;
    }

    let mut visited = 0_usize;
    let mut total_bytes = 0_u64;
    let mut removed = 0_u64;
    let entries = fs::read_dir(&raw_base).kio_io(&raw_base)?;
    for entry in entries {
        let entry = entry.kio_io(&raw_base)?;
        visited = visited.saturating_add(1);
        if visited > MAX_RAW_DIRECTORY_ENTRIES {
            return Err(orphan_raw_ingest_error(
                &raw_base,
                "raw object directory exceeds the orphan-cleanup entry limit",
            ));
        }
        if !entry
            .file_name()
            .as_encoded_bytes()
            .starts_with(b".ingest-")
        {
            continue;
        }

        let path = entry.path();
        let listed = fs::symlink_metadata(&path).kio_io(&path)?;
        if listed.file_type().is_symlink()
            || !listed.file_type().is_file()
            || listed.len() > MAX_RAW_OBJECT_BYTES
        {
            return Err(orphan_raw_ingest_error(
                &path,
                "raw-ingest orphan is not a bounded regular file",
            ));
        }
        crate::platform::scope_semantics::validate_orphan_file_native(&path, &listed)?;

        let opened = open_scope_file_nofollow(&path)?;
        let opened_metadata = opened.metadata().kio_io(&path)?;
        if opened_metadata.len() != listed.len() || opened_metadata.len() > MAX_RAW_OBJECT_BYTES {
            return Err(orphan_raw_ingest_error(
                &path,
                "raw-ingest orphan changed while it was opened",
            ));
        }
        total_bytes = total_bytes
            .checked_add(opened_metadata.len())
            .ok_or_else(|| {
                orphan_raw_ingest_error(&raw_base, "raw-ingest orphan byte count overflow")
            })?;
        if total_bytes > MAX_ORPHAN_BYTES {
            return Err(orphan_raw_ingest_error(
                &raw_base,
                "raw-ingest orphans exceed the aggregate cleanup byte limit",
            ));
        }
        drop(opened);
        fs::remove_file(&path).kio_io(&path)?;
        removed = removed.saturating_add(1);
    }
    if removed > 0
        && let Ok(directory) = File::open(&raw_base)
    {
        let _ = directory.sync_all();
    }
    Ok(removed)
}

fn validate_ingest_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).kio_io(path)?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(orphan_raw_ingest_error(
            path,
            "raw-ingest namespace ancestor is not a real directory",
        ));
    }
    crate::platform::scope_semantics::validate_ingest_directory_native(path)?;
    Ok(())
}

pub(crate) fn orphan_raw_ingest_error(path: &Path, message: &str) -> KioError {
    KioError::new(
        "KIO-E-STORE-CORRUPT-001",
        message,
        json!({ "path": path }),
        ExitCode::PermanentFailure,
    )
}

/// U19/LC22: a public tombstone (or fsck-only erase receipt) no longer
/// permanently rejects identical-byte re-ingest — that block is reversed into
/// the resurrection flow (retire-on-republish, see
/// [`Repository::retire_resurrected_after_publish`]). Only an active
/// post-`prepared` purge journal barrier for this exact raw_hash (an
/// in-progress, not-yet-complete purge) still gates publication.
fn ensure_raw_publication_allowed(purge: &PurgeState, raw_hash: &str) -> Result<()> {
    if purge.barrier_blocks(raw_hash)? {
        return Err(KioError::new(
            "KIO-E-PURGE-INCOMPLETE-001",
            "raw publication is blocked by an active purge transaction",
            json!({ "component": "raw_ingest" }),
            ExitCode::PartialFailure,
        ));
    }
    Ok(())
}

fn bound_marker_exists(kio: &File, namespace: &str, raw_hash: &str) -> Result<bool> {
    use cap_primitives::fs as cap_fs;

    let digest = raw_hash
        .strip_prefix("sha256:")
        .ok_or_else(|| KioError::schema("scheduled marker lookup requires a sha256 raw hash"))?;
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(KioError::schema(
            "scheduled marker lookup requires a sha256 raw hash",
        ));
    }
    let mut directory = kio
        .try_clone()
        .map_err(|error| KioError::io(error.to_string(), namespace))?;
    for component in Path::new(namespace).components().chain([
        Component::Normal(std::ffi::OsStr::new(&digest[..2])),
        Component::Normal(std::ffi::OsStr::new(&digest[2..4])),
    ]) {
        let Component::Normal(component) = component else {
            continue;
        };
        directory = match cap_fs::open_dir_nofollow(&directory, Path::new(component)) {
            Ok(next) => next,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(KioError::io(error.to_string(), namespace)),
        };
    }
    match cap_fs::stat(&directory, Path::new(digest), cap_fs::FollowSymlinks::No) {
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(_) => Err(unsafe_store_error(
            Path::new(namespace),
            "purge marker leaf is not a regular file",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(KioError::io(error.to_string(), namespace)),
    }
}

/// Debug-only synchronization seam for descriptor-replacement integration
/// tests. Production builds never inspect the environment or wait.
#[cfg(debug_assertions)]
fn wait_at_bound_snapshot_auto_layout_barrier(control: &crate::control::ControlHandle) {
    // This helper can run inside an already captured snapshot operation. It
    // must not consult ambient environment variables midway through that
    // operation, or a test barrier could change after the writer lock.
    let test_control = &control.debug().core;
    wait_at_bound_snapshot_auto_barrier(test_control.snapshot_bound_layout_ready.as_deref());
}

#[cfg(not(debug_assertions))]
fn wait_at_bound_snapshot_auto_layout_barrier(_control: &crate::control::ControlHandle) {}

#[cfg(debug_assertions)]
fn wait_at_bound_snapshot_auto_barrier(ready_path: Option<&Path>) {
    let Some(ready_path) = ready_path else {
        return;
    };
    if fs::write(ready_path, b"ready").is_err() {
        return;
    }
    let release_path = ready_path.with_extension("release");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !release_path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[cfg(not(debug_assertions))]
fn wait_at_bound_snapshot_auto_barrier(_: Option<&Path>) {}

fn attach_pending_normalize(
    tree_entry: &mut TreeEntry,
    file_name: &str,
    normalize_by_path: &BTreeMap<String, PendingNormalizeRef>,
) {
    if let Some(pending) = normalize_by_path.get(file_name) {
        if pending.expected_raw_hash == tree_entry.raw_hash {
            tree_entry.normalize = Some(pending.normalize.clone());
        } else {
            eprintln!(
                "warning: normalization metadata not attached because {file_name} changed after normalization"
            );
        }
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn scope_input_oversized(file_name: &str, limits: ArchiveLimits, actual: u64) -> KioError {
    KioError::new(
        "KIO-E-SCOPE-INPUT-OVERSIZED-001",
        "scope input exceeds the core archive byte budget",
        json!({
            "path": file_name,
            "actual_bytes": actual,
            "max_file_bytes": limits.max_file_bytes,
            "max_scope_bytes": limits.max_scope_bytes,
        }),
        ExitCode::PermanentFailure,
    )
}

fn scope_tree_entries_oversized(observed: usize) -> KioError {
    KioError::new(
        "KIO-E-SCOPE-INPUT-OVERSIZED-001",
        "scope tree entry count exceeds the persisted tree limit",
        json!({
            "observed_entries": observed,
            "max_tree_entries": MAX_TREE_ENTRIES,
        }),
        ExitCode::PermanentFailure,
    )
}

fn scope_file_changed(file_name: &str) -> KioError {
    KioError::new(
        "KIO-E-SCOPE-FILE-CHANGED-001",
        "scope file changed while it was being archived",
        json!({ "path": file_name }),
        ExitCode::Failure,
    )
}

pub(crate) fn scope_file_changed_path(path: &Path) -> KioError {
    KioError::new(
        "KIO-E-SCOPE-FILE-CHANGED-001",
        "scope path no longer identifies the checked regular file",
        json!({ "path": path }),
        ExitCode::Failure,
    )
}

fn snapshot_authority_changed(message: &str) -> KioError {
    KioError::new(
        "KIO-E-SNAPSHOT-AUTHORITY-CHANGED-001",
        message,
        json!({}),
        ExitCode::PartialFailure,
    )
}

/// Restrict a directory that may hold document bytes / secrets / usage data to
/// owner-only access (0700) on unix (P2). Applied to the `.kio` tree and the
/// device data dir (`~/.local/share/kio`) at creation so a multi-user host
/// cannot read another user's archive. A 0700 parent blocks traversal into the
/// whole subtree regardless of child modes. No-op on non-unix.
pub fn restrict_dir_to_owner(dir: &Path) -> Result<()> {
    set_scope_directory_permissions(dir)
}

fn tree_map(tree: &TreeObject) -> BTreeMap<String, String> {
    tree.entries
        .iter()
        .map(|entry| (entry.path.clone(), entry.raw_hash.clone()))
        .collect()
}

/// Commit stats of `working` against an optional prior tree (`None` = unborn: all
/// files count as added). R15-4: pure (no I/O) — the caller reads the prior tree
/// once (and rejects a shallow HEAD) rather than re-reading it here.
fn commit_stats(prior: Option<&TreeObject>, working: &TreeObject) -> CommitStats {
    let current = tree_map(working);
    let old = prior.map(tree_map).unwrap_or_default();
    let mut added = 0;
    let mut modified = 0;
    let mut deleted = 0;

    let mut paths = BTreeSet::new();
    paths.extend(current.keys().cloned());
    paths.extend(old.keys().cloned());
    for path in paths {
        match (old.get(&path), current.get(&path)) {
            (None, Some(_)) => added += 1,
            (Some(_), None) => deleted += 1,
            (Some(a), Some(b)) if a != b => modified += 1,
            _ => {}
        }
    }

    CommitStats {
        files_added: added,
        files_modified: modified,
        files_deleted: deleted,
    }
}

/// R15-4: is `error` a raw missing-CAS-object error? Used to fold a missing tree
/// object into the shallow-commit policy (degrade reads / fail writes loudly).
fn is_store_not_found(error: &KioError) -> bool {
    error.error_code() == "KIO-E-STORE-NOT-FOUND-001"
}

fn validate_manifest_matches_tree(manifest: &Value, tree: &TreeObject) -> Result<()> {
    validate_json_schema(SchemaKind::Manifest, manifest)?;
    let files = manifest
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| KioError::schema("publication manifest missing files"))?;
    let live: BTreeMap<_, _> = files
        .iter()
        .filter(|file| file.get("status").and_then(Value::as_str) != Some("deleted"))
        .map(|file| {
            let path = file
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| KioError::schema("publication manifest file path missing"))?;
            let raw = file
                .get("raw_hash")
                .and_then(Value::as_str)
                .ok_or_else(|| KioError::schema("publication manifest raw hash missing"))?;
            Ok((path, raw))
        })
        .collect::<Result<_>>()?;
    let expected: BTreeMap<_, _> = tree
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry.raw_hash.as_str()))
        .collect();
    if live != expected {
        return Err(KioError::schema(
            "publication manifest does not describe the new commit tree",
        ));
    }
    Ok(())
}

/// R17-5: a commit operand (`diff`/`tag` hash literal, or a tag-name target) whose
/// commit object is gone (shallow: discarded / corrupt) folds into
/// KIO-E-COMMIT-SHALLOW-001 — the same class every other shallow-commit site raises
/// (R16-1 / R16-5) — instead of a raw, opaque KIO-E-STORE-NOT-FOUND-001. Used by
/// `resolve_commit`, which runs before `diff_side_tree`'s R16-5 absorption, so
/// without this a hash-literal / tag-name shallow commit would bypass the
/// COMMIT-SHALLOW contract that the `HEAD` operand already reaches. Kept generic (no
/// diff side) because `resolve_commit` is shared by `diff` and `tag`; the diff side
/// is still named for the tree-GC case that reaches `diff_side_tree`.
fn resolve_commit_shallow_error(commit_hash: &str) -> KioError {
    KioError::commit_shallow(
        "referenced commit object is missing (shallow: discarded / corrupt); \
         restore the commit object or reference a non-shallow commit",
        commit_hash.to_owned(),
    )
}

/// R16-5: the `diff` shallow-side error, naming which operand (`a`/`b`) is shallow.
fn diff_side_shallow_error(side: &str, commit_hash: &str) -> KioError {
    KioError::new(
        "KIO-E-COMMIT-SHALLOW-001",
        format!(
            "diff side `{side}` is shallow (its commit or tree object is discarded); \
             a full-file diff is not possible when either side is shallow — diff a \
             non-shallow commit pair or restore the missing object"
        ),
        json!({ "commit_hash": commit_hash, "side": side }),
        ExitCode::Failure,
    )
}

fn validate_format_version(version: &str) -> Result<()> {
    // This is a pre-stable store format, not the package's semver
    // compatibility range.  The current reader understands exactly the
    // current on-disk format; every different string is either older,
    // malformed, or newer and must take the version-specific fail-closed
    // path before current-schema validation.
    if version == KIO_FORMAT_VERSION {
        Ok(())
    } else {
        Err(KioError::incompatible_format(version))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct LockFile {
    pub(crate) pid: u32,
    pub(crate) token: String,
    pub(crate) created_at: String,
}

/// Reserved owner written by the lock-release exchange protocol. No supported
/// platform can assign this value to a Kio process; treating it as dead is part
/// of the on-disk protocol and must not depend on spawning a liveness probe.
pub(crate) const RELEASED_LOCK_PID: u32 = u32::MAX;

pub(crate) fn new_lock_token(pid: u32) -> String {
    let seed = format!(
        "{pid}:{}:{}",
        unix_nanos(),
        std::thread::current().name().unwrap_or("")
    );
    let digest = Sha256::digest(seed.as_bytes());
    hex_prefix(&digest, 32)
}

/// A descriptor-relative owner of the ordinary `.kio/.lock` protocol.
/// Common writer authority and same-thread leases wrap its selected native inner.
pub struct BoundStoreLock {
    _native: crate::platform::BoundLock,
}
/// A mutation-free shared reader barrier over the retained `.kio` capability.
pub(crate) struct BoundStoreReadGuard {
    native: crate::platform::ReadGate,
}
impl BoundStoreReadGuard {
    pub(crate) fn recheck_idle(&self) -> Result<()> {
        self.native.recheck_idle()
    }
}
#[cfg(test)]
pub(crate) fn acquire_bound_store_lock(kio: &File) -> Result<BoundStoreLock> {
    acquire_bound_store_lock_with_control(kio, &crate::control::ControlHandle::default())
}

pub(crate) fn acquire_bound_store_lock_with_control(
    kio: &File,
    control: &crate::control::ControlHandle,
) -> Result<BoundStoreLock> {
    validate_bound_store_lock_authority(kio, control)?;
    Ok(BoundStoreLock {
        _native: crate::platform::acquire_bound_lock(kio, true, control)?,
    })
}
#[cfg(test)]
pub(crate) fn acquire_bound_store_read_guard(kio: &File) -> Result<BoundStoreReadGuard> {
    acquire_bound_store_read_guard_with_control(kio, &crate::control::ControlHandle::default())
}

pub(crate) fn acquire_bound_store_read_guard_with_control(
    kio: &File,
    control: &crate::control::ControlHandle,
) -> Result<BoundStoreReadGuard> {
    validate_bound_store_lock_authority(kio, control)?;
    Ok(BoundStoreReadGuard {
        native: crate::platform::acquire_bound_read_gate(kio)?,
    })
}

/// A same-thread lease of the descriptor-relative `.kio/.lock` owner.  The
/// `Rc` deliberately makes it !Send: a different thread must contend through
/// the real retained lock rather than inheriting reentrancy by path spelling.
pub struct RetainedStoreLock {
    _session: LockSession,
    _owner: Rc<BoundStoreLock>,
}

pub struct RetainedPublicationLock {
    _owner: RetainedStoreLock,
}

fn acquire_retained_publication_lock(
    session: &LockSession,
    kio: &File,
    control: &crate::control::ControlHandle,
) -> Result<RetainedPublicationLock> {
    validate_bound_store_lock_authority(kio, control)?;
    let directory = StoreDirectory::from_retained_with_control(
        kio.try_clone()
            .map_err(|error| KioError::io(error.to_string(), ".kio"))?,
        PathBuf::from(".kio"),
        control.clone(),
    )?;
    // A distinct retained child gives publication its own OS-lock identity,
    // including the existing stale-owner recovery protocol, without taking a
    // second flock on the writer's `.kio` directory.
    let publication = directory.create_directory_all(Path::new("internal/publication"))?;
    Ok(RetainedPublicationLock {
        _owner: acquire_retained_lock_kind_with_control(session, &publication, false, control)?,
    })
}

/// Scheduler and ordinary repository writers use the identical retained
/// identity lease; the historical path-alias bridge no longer exists.
pub type BoundReentrantStoreLock = RetainedStoreLock;

pub(crate) fn acquire_bound_reentrant_store_lock_with_control(
    session: &LockSession,
    kio: &File,
    control: &crate::control::ControlHandle,
) -> Result<BoundReentrantStoreLock> {
    acquire_retained_store_lock_with_control(session, kio, control)
}

pub(crate) fn canonical_lock_bytes_with_control(
    control: &crate::control::ControlHandle,
    pid: u32,
    token: &str,
) -> Result<Vec<u8>> {
    let lock = LockFile {
        pid,
        token: token.to_owned(),
        created_at: control.now_utc_seconds(),
    };
    serde_json::to_vec(&lock).map_err(|error| KioError::io(error.to_string(), ".kio/.lock"))
}

pub(crate) fn canonical_lock_bytes_for(lock: &LockFile) -> Result<Vec<u8>> {
    serde_json::to_vec(lock).map_err(|error| KioError::io(error.to_string(), ".kio/.lock"))
}

#[cfg(debug_assertions)]
fn maybe_hold_lock_for_tests(ready: Option<&Path>) {
    if let Some(ready) = ready {
        let release = ready.with_extension("release");
        std::fs::write(ready, b"ready").expect("test lock ready marker must be writable");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !release.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            release.exists(),
            "test lock release marker was not published: {}",
            release.display()
        );
    }
}

#[cfg(not(debug_assertions))]
fn maybe_hold_lock_for_tests(_: Option<&Path>) {}

pub(crate) fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

pub fn new_ulid(path: &Path) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let timestamp_ms = now.as_millis() as u64;
    let mut bytes = [0_u8; 16];
    bytes[0] = ((timestamp_ms >> 40) & 0xff) as u8;
    bytes[1] = ((timestamp_ms >> 32) & 0xff) as u8;
    bytes[2] = ((timestamp_ms >> 24) & 0xff) as u8;
    bytes[3] = ((timestamp_ms >> 16) & 0xff) as u8;
    bytes[4] = ((timestamp_ms >> 8) & 0xff) as u8;
    bytes[5] = (timestamp_ms & 0xff) as u8;

    let seed = format!(
        "{}:{}:{}",
        path.display(),
        std::process::id(),
        now.as_nanos()
    );
    let digest = Sha256::digest(seed.as_bytes());
    bytes[6..].copy_from_slice(&digest[..10]);
    encode_crockford_base32(&bytes)
}

fn encode_crockford_base32(bytes: &[u8; 16]) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut value = u128::from_be_bytes(*bytes);
    let mut chars = [b'0'; 26];
    for index in (0..26).rev() {
        chars[index] = ALPHABET[(value & 0x1f) as usize];
        value >>= 5;
    }
    String::from_utf8(chars.to_vec()).expect("base32 alphabet is UTF-8")
}

fn hex_prefix(bytes: &[u8], chars: usize) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(chars);
    for byte in bytes {
        if out.len() >= chars {
            break;
        }
        out.push(HEX[(byte >> 4) as usize] as char);
        if out.len() < chars {
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    out
}

pub(crate) fn is_ulid(value: &str) -> bool {
    value.len() == 26
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z'))
}

pub fn now_utc_seconds() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    format_unix_seconds(secs)
}

fn format_unix_seconds(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let second_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = second_of_day / 3_600;
    let minute = (second_of_day % 3_600) / 60;
    let second = second_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096).div_euclid(365);
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2).div_euclid(153);
    let day = doy - (153 * mp + 2).div_euclid(5) + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

/// Inverse of [`civil_from_days`]: days since the Unix epoch for a civil date
/// (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 }.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2).div_euclid(5) + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Format Unix seconds as an RFC3339 UTC-seconds timestamp (`YYYY-MM-DDTHH:MM:SSZ`),
/// the shape produced by [`now_utc_seconds`].
#[must_use]
pub fn format_utc_seconds(secs: i64) -> String {
    format_unix_seconds(secs)
}

/// Parse an RFC3339 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`, the shape produced
/// by [`now_utc_seconds`], or `YYYY-MM-DDTHH:MM:SS.<digits>Z` with an
/// optional fractional-seconds suffix -- `docs/06-cli-spec.md` §12's other
/// persisted-timestamp shape, e.g. commit `created_at`) into Unix seconds.
/// Returns `None` when the input matches neither shape. Sub-second digits are
/// validated but discarded: this function's return unit is whole seconds
/// (unchanged by R23-16 -- only the accepted input shape widened).
#[must_use]
pub fn parse_utc_seconds(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[bytes.len() - 1] != b'Z'
    {
        return None;
    }
    // R23-16 (06 §11 L513, "正: 2026-04-25T12:00:00.123456Z"): byte 19 is
    // either the terminating `Z` (the original fixed 20-byte shape, already
    // confirmed by the trailing-byte check above) or the start of `.` + >=1
    // ASCII digit + the same trailing `Z`. A parser that rejected the
    // fractional shape outright silently dropped every persisted timestamp
    // that happened to carry sub-second precision (e.g. `--since` filtering
    // commit `created_at`).
    if bytes.len() > 20 {
        let digits = &bytes[20..bytes.len() - 1];
        if bytes[19] != b'.' || digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
    }
    let field = |start: usize, end: usize| value.get(start..end)?.parse::<i64>().ok();
    let year = field(0, 4)?;
    let month = field(5, 7)?;
    let day = field(8, 10)?;
    let hour = field(11, 13)?;
    let minute = field(14, 16)?;
    let second = field(17, 19)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

// ===========================================================================
// QA21/22/23/24/25/26/27 (step4b-contract-tests-p3a.md §G/§H, 07-adapter-spec.md
// §3 L85-249, 10-operations.md §11.3): the adapter-level network opt-in gate's
// persistent storage — `.kio/scope.json`'s `approvals[]` / `approval_pending` /
// `approvals_initialized`. This is the sole persistent source of truth the
// send gate reads (QA22 — device-global `consents.jsonl` is untouched by this
// module and is no longer consulted for the send decision). Free functions
// (not `Repository` methods) so a multi-scope caller (e.g. the query-embedding
// consent OR-across-scopes check) can evaluate a scope it has not opened as a
// full `Repository` for.
// ===========================================================================

/// QA25/26/27: the outcome of a `kio adapter revoke` scope.json mutation.
#[derive(Debug, Clone, Default, Serialize)]
pub struct NetworkRevokeOutcome {
    /// `tool_id`s whose `approvals[]` row was flipped `status=revoked` by
    /// this call (already-revoked rows are left alone and not repeated
    /// here).
    pub revoked_tool_ids: Vec<String>,
    /// Whether a matching `approval_pending` was removed by this call.
    pub pending_removed: bool,
    /// Whether this call wrote the `approvals_initialized` marker (only
    /// happens when it was previously absent AND this call actually
    /// changed something — 07 §3: a no-op revoke never writes it).
    pub marker_written: bool,
}

impl NetworkRevokeOutcome {
    /// Whether this call changed anything at all — the idempotent
    /// "no target" case (07 §3: "対象なし...は冪等成功") is `false`.
    #[must_use]
    pub fn changed(&self) -> bool {
        !self.revoked_tool_ids.is_empty() || self.pending_removed
    }
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    fn canonical_lock_bytes(pid: u32, token: &str) -> super::Result<Vec<u8>> {
        super::canonical_lock_bytes_with_control(
            &crate::control::ControlHandle::default(),
            pid,
            token,
        )
    }
    use super::{
        ArchiveLimits, DEFAULT_MAX_ARCHIVE_FILE_BYTES, MAX_TREE_ENTRIES, PendingNormalizeRef,
        RELEASED_LOCK_PID, Repository, StoreLock, append_jsonl_rotating, civil_from_days,
        format_unix_seconds, format_utc_seconds, open_scope_file_nofollow, parse_utc_seconds,
        prune_rotated_logs, publish_network_approval, read_adapter_lane, read_logs_retention_days,
        read_network_approval_pending, read_network_approvals, redact_context,
        redact_message_paths, revoke_network_approval, rotate_stale_log,
        write_network_approval_pending,
    };
    use super::{enforce_config_semantics, enforce_scope_config_semantics};
    use crate::cas::canonical_json_bytes;
    use crate::platform::lock::process_is_alive;
    #[cfg(windows)]
    use crate::platform::lock::{
        create_windows_lock, is_windows_lock_contention, open_windows_lock_parent,
        read_windows_lock, release_windows_owned_lock, windows_lock_leaf,
    };
    use serde_json::Value;
    use std::path::Path;
    #[cfg(windows)]
    use std::path::PathBuf;

    #[test]
    fn planned_store_gate_is_required_empty_and_read_only_validation_never_repairs_it() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        let gate = repo.kio_dir().join(".store-gate");
        assert_eq!(fs::read(&gate).unwrap(), b"");
        let kio = repo
            .store_directory()
            .unwrap()
            .root_handle()
            .try_clone()
            .unwrap();
        fs::remove_file(&gate).unwrap();
        for error in [
            repo.scope_identity().unwrap_err(),
            super::acquire_bound_store_read_guard(&kio).err().unwrap(),
            super::acquire_bound_store_lock(&kio).err().unwrap(),
            StoreLock::acquire(repo.kio_dir()).err().unwrap(),
        ] {
            assert_eq!(error.error_code(), "KIO-E-STORE-LOCKED-001");
        }
        assert!(!gate.exists());
        assert!(!repo.kio_dir().join(".lock").exists());
        fs::write(&gate, b"foreign").unwrap();
        assert_eq!(
            repo.scope_identity().unwrap_err().error_code(),
            "KIO-E-STORE-LOCKED-001"
        );
        assert_eq!(fs::read(gate).unwrap(), b"foreign");
    }

    #[test]
    fn old_format_rejects_before_missing_gate_and_low_level_writer_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        let kio = repo
            .store_directory()
            .unwrap()
            .root_handle()
            .try_clone()
            .unwrap();
        let path = repo.kio_dir().join("scope.json");
        let mut scope: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        scope["kio_format_version"] = json!("1.0.0");
        fs::write(&path, serde_json::to_vec(&scope).unwrap()).unwrap();
        fs::remove_file(repo.kio_dir().join(".store-gate")).unwrap();
        for error in [
            repo.scope_identity().unwrap_err(),
            super::acquire_bound_store_read_guard(&kio).err().unwrap(),
            super::acquire_bound_store_lock(&kio).err().unwrap(),
            StoreLock::acquire(repo.kio_dir()).err().unwrap(),
        ] {
            assert_eq!(error.error_code(), "KIO-E-STORE-VERSION-001");
        }
        assert!(!repo.kio_dir().join(".lock").exists());
        assert!(!repo.kio_dir().join(".store-gate").exists());
    }

    #[cfg(unix)]
    #[test]
    fn external_root_permissions_allow_init_with_private_bound_store() {
        use crate::store_dir::StoreDirectory;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        // docs14/ST01: external root permissions do not admit Kio's own store.
        for constructor in 0..3 {
            let root = tempfile::tempdir().unwrap();
            fs::set_permissions(root.path(), fs::Permissions::from_mode(0o777)).unwrap();
            let canonical = root.path().canonicalize().unwrap();
            let directory = StoreDirectory::open(&canonical).unwrap();
            let before = directory.root_handle().metadata().unwrap();
            let retained = directory.root_handle().as_ref().try_clone().unwrap();
            let repo = match constructor {
                0 => Repository::init(root.path()),
                1 => Repository::init_bound(canonical.clone(), retained),
                _ => Repository::create_bound(canonical.clone(), retained),
            }
            .unwrap();
            assert_eq!(repo.canonical_root(), canonical);
            let after = fs::metadata(root.path()).unwrap();
            assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
            assert_eq!(
                after.permissions().mode() & 0o777,
                0o777,
                "external root must not be repaired"
            );
            let external = crate::private_fs::retain_external_directory(root.path()).unwrap();
            crate::private_fs::verify_private_directory_at(&external, Path::new(".kio")).unwrap();
            assert_eq!(
                fs::metadata(root.path().join(".kio"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn normal_open_resolves_existing_tags_from_the_retained_store_after_public_replacement() {
        use crate::cas::{ObjectKind, ObjectStore, hash_bytes};
        use crate::dag::{CommitObject, CommitStats, CommitType, build_tree};

        let fixture = tempfile::tempdir().unwrap();
        let kio = fixture.path().join(".kio");
        Repository::init(fixture.path()).unwrap();
        let store = ObjectStore::new(&kio);
        let tree = build_tree(Vec::new()).unwrap();
        let (tree_hash, _) = store
            .write_json(ObjectKind::Tree, &serde_json::to_value(tree).unwrap())
            .unwrap();
        let commit = CommitObject::new(
            tree_hash,
            None,
            "2026-09-08T00:00:00Z".into(),
            "tag binding fixture".into(),
            hash_bytes(b"tag binding tool lock"),
            CommitStats {
                files_added: 0,
                files_modified: 0,
                files_deleted: 0,
            },
            CommitType::Manual,
        )
        .unwrap();
        let (commit_hash, _) = store
            .write_json(ObjectKind::Commit, &serde_json::to_value(commit).unwrap())
            .unwrap();
        fs::write(kio.join("HEAD"), format!("{commit_hash}\n")).unwrap();
        fs::write(kio.join("refs/tags-v1/names.jsonl"), b"").unwrap();

        let repo = Repository::open(fixture.path()).unwrap();
        repo.tag("retained-tag", None).unwrap();
        let displaced = fixture.path().join("displaced-kio");
        fs::rename(&kio, &displaced).unwrap();
        fs::create_dir(&kio).unwrap();

        assert_eq!(repo.resolve_commit("retained-tag").unwrap(), commit_hash);
        assert!(!kio.join("refs/tags-v1").exists());
    }

    #[cfg(unix)]
    #[test]
    fn normal_open_replaces_metadata_through_the_retained_store_after_public_replacement() {
        let fixture = tempfile::tempdir().unwrap();
        let kio = fixture.path().join(".kio");
        Repository::init(fixture.path()).unwrap();
        let repo = Repository::open(fixture.path()).unwrap();
        let displaced = fixture.path().join("displaced-kio");
        fs::rename(&kio, &displaced).unwrap();
        fs::create_dir(&kio).unwrap();

        repo.replace_config_value(
            toml::from_str("[chunking]\nstrategy = \"heading\"\nmax_chars = 123").unwrap(),
        )
        .unwrap();

        assert!(
            fs::read_to_string(displaced.join("config.toml"))
                .unwrap()
                .contains("max_chars = 123")
        );
        assert!(kio.join("config.toml").read_dir().is_err());
    }

    #[test]
    fn normal_open_does_not_materialize_lazy_content_namespaces() {
        let fixture = tempfile::tempdir().unwrap();
        Repository::init(fixture.path()).unwrap();
        let objects = fixture.path().join(".kio/objects");
        let before: std::collections::BTreeSet<_> = fs::read_dir(&objects)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        Repository::open(fixture.path()).unwrap();
        let after: std::collections::BTreeSet<_> = fs::read_dir(&objects)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(after, before);
    }

    #[cfg(unix)]
    #[test]
    fn bound_lock_directory_sync_reopens_linux_o_path_capability() {
        use cap_primitives::{ambient_authority, fs as cap_fs};

        let scope = tempfile::tempdir().unwrap();
        let repo = Repository::init(scope.path()).unwrap();
        let retained = cap_fs::open_ambient_dir(repo.kio_dir(), ambient_authority()).unwrap();
        crate::platform::lock::sync_bound_directory(&retained, repo.kio_dir()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn bound_gc_lock_contends_with_ordinary_lock_and_recovers_canonical_stale_lock() {
        use cap_primitives::{ambient_authority, fs as cap_fs};

        let scope = tempfile::tempdir().unwrap();
        let repo = Repository::init(scope.path()).unwrap();
        let retained = cap_fs::open_ambient_dir(repo.kio_dir(), ambient_authority()).unwrap();

        let held = super::acquire_bound_store_lock(&retained).unwrap();
        match StoreLock::acquire(repo.kio_dir()) {
            Err(error) => assert_eq!(error.error_code(), "KIO-E-STORE-LOCKED-001"),
            Ok(_) => panic!("ordinary writer acquired a bound GC lock"),
        }
        drop(held);
        match StoreLock::acquire(repo.kio_dir()) {
            Ok(_) => {}
            Err(error) => panic!("ordinary recovery after GC release failed: {error:?}"),
        }

        let stale = super::LockFile {
            pid: 999_999_999,
            token: "dead-owner".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
        };
        assert!(!crate::platform::lock::process_is_alive(stale.pid));
        let stale_bytes = serde_json::to_vec(&stale).unwrap();
        std::fs::write(repo.kio_dir().join(".lock"), &stale_bytes).unwrap();
        let recovered = super::acquire_bound_store_lock(&retained).unwrap();
        let active: serde_json::Value =
            serde_json::from_slice(&std::fs::read(repo.kio_dir().join(".lock")).unwrap()).unwrap();
        assert!(active.get("pid").is_some());
        assert!(active.get("token").is_some());
        assert!(active.get("created_at").is_some());
        let lock_archive = repo.kio_dir().join("internal/locks");
        assert_eq!(std::fs::read_dir(lock_archive).unwrap().count(), 0);
        drop(recovered);
        // A normal writer can parse the GC lock after the GC process crashed
        // and reclaimed it; no private GC-only lock encoding exists.
        assert!(StoreLock::acquire(repo.kio_dir()).is_ok());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn bound_reentrant_lock_allows_nested_repository_store_lock() {
        use cap_primitives::{ambient_authority, fs as cap_fs};

        let scope = tempfile::tempdir().unwrap();
        let repo = Repository::init(scope.path()).unwrap();
        let retained = cap_fs::open_ambient_dir(repo.kio_dir(), ambient_authority()).unwrap();
        let outer = super::acquire_bound_reentrant_store_lock_with_control(
            repo.lock_session(),
            &retained,
            repo.control(),
        )
        .unwrap();
        // Model a fork-inherited duplicate of the directory open-file
        // description. The final logical lease must explicitly unlock it.
        let inherited_gate = outer._owner._native._gate.file.try_clone().unwrap();
        let nested = repo.lock_store_in_session(repo.lock_session()).unwrap();
        drop(outer);
        // Each logical guard owns the same Rc lease; dropping the outer
        // scheduler guard cannot release the physical lock while nested work
        // is still executing.
        assert!(super::acquire_bound_store_lock(&retained).is_err());
        drop(nested);
        assert!(StoreLock::acquire(repo.kio_dir()).is_ok());
        drop(inherited_gate);
    }

    #[cfg(unix)]
    #[test]
    fn retained_repository_lock_contends_across_threads() {
        let scope = tempfile::tempdir().unwrap();
        let repo = Repository::init(scope.path()).unwrap();
        let held = repo.lock_store_in_session(repo.lock_session()).unwrap();
        let root = scope.path().to_path_buf();
        let contender =
            std::thread::spawn(move || Repository::open(&root).unwrap().lock_store().map(drop));
        assert!(contender.join().unwrap().is_err());
        drop(held);
        assert!(Repository::open(scope.path()).unwrap().lock_store().is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn retained_repository_lock_keys_replaced_public_stores_by_identity() {
        let scope = tempfile::tempdir().unwrap();
        let original = Repository::init(scope.path()).unwrap();
        let retained = scope.path().join("retained-kio");
        fs::rename(scope.path().join(".kio"), &retained).unwrap();
        let replacement = Repository::init(scope.path()).unwrap();

        let original_lock = original.lock_store().unwrap();
        let replacement_lock = replacement.lock_store().unwrap();
        assert!(retained.join(".lock").exists());
        assert!(scope.path().join(".kio/.lock").exists());
        drop(replacement_lock);
        drop(original_lock);
    }

    #[cfg(unix)]
    #[test]
    fn retained_publication_lock_is_distinct_from_writer_and_stays_on_retained_parent() {
        let scope = tempfile::tempdir().unwrap();
        let repo = Repository::init(scope.path()).unwrap();
        let writer = repo.lock_store_in_session(repo.lock_session()).unwrap();
        let publication = repo.lock_purge_publication().unwrap();
        assert!(
            scope
                .path()
                .join(".kio/internal/publication/.lock")
                .exists()
        );
        drop(publication);
        drop(writer);

        let retained = scope.path().join("retained-kio");
        fs::rename(scope.path().join(".kio"), &retained).unwrap();
        Repository::init(scope.path()).unwrap();
        let publication = repo.lock_purge_publication().unwrap();
        assert!(retained.join("internal/publication/.lock").exists());
        assert!(
            !scope
                .path()
                .join(".kio/internal/publication/.lock")
                .exists()
        );
        drop(publication);
    }

    #[cfg(unix)]
    #[test]
    fn bound_gc_lock_never_creates_a_lock_in_a_public_path_replacement() {
        use cap_primitives::{ambient_authority, fs as cap_fs};

        let original_scope = tempfile::tempdir().unwrap();
        let original = Repository::init(original_scope.path()).unwrap();
        let retained = cap_fs::open_ambient_dir(original.kio_dir(), ambient_authority()).unwrap();
        let victim_scope = tempfile::tempdir().unwrap();
        let victim = Repository::init(victim_scope.path()).unwrap();
        let saved = original_scope.path().join(".kio-retained");
        std::fs::rename(original.kio_dir(), &saved).unwrap();
        std::fs::rename(victim.kio_dir(), original_scope.path().join(".kio")).unwrap();

        let held = super::acquire_bound_store_lock(&retained).unwrap();
        assert!(!original_scope.path().join(".kio/.lock").exists());
        assert!(saved.join(".lock").exists());
        drop(held);
    }

    #[cfg(unix)]
    #[test]
    fn bound_gc_live_lock_rejection_does_not_create_gc_internal_state() {
        use cap_primitives::{ambient_authority, fs as cap_fs};

        let scope = tempfile::tempdir().unwrap();
        let repo = Repository::init(scope.path()).unwrap();
        let retained = cap_fs::open_ambient_dir(repo.kio_dir(), ambient_authority()).unwrap();
        let live = StoreLock::acquire(repo.kio_dir()).unwrap();
        let before_lock = std::fs::read(repo.kio_dir().join(".lock")).unwrap();
        assert!(!repo.kio_dir().join("gc").exists());

        match super::acquire_bound_store_lock(&retained) {
            Err(error) => assert_eq!(error.error_code(), "KIO-E-STORE-LOCKED-001"),
            Ok(_) => panic!("bound GC acquired a live ordinary lock"),
        }
        assert_eq!(
            std::fs::read(repo.kio_dir().join(".lock")).unwrap(),
            before_lock
        );
        assert!(!repo.kio_dir().join("gc").exists());
        drop(live);
    }

    #[cfg(unix)]
    #[test]
    fn bound_store_lock_generic_archive_setup_failure_does_not_publish_an_unowned_live_lock() {
        use cap_primitives::{ambient_authority, fs as cap_fs};

        let scope = tempfile::tempdir().unwrap();
        let repo = Repository::init(scope.path()).unwrap();
        let retained = cap_fs::open_ambient_dir(repo.kio_dir(), ambient_authority()).unwrap();
        std::fs::write(repo.kio_dir().join("internal"), b"not a directory").unwrap();

        assert!(super::acquire_bound_store_lock(&retained).is_err());
        assert!(
            !repo.kio_dir().join(".lock").exists(),
            "failed archive setup must not strand a live lock"
        );

        std::fs::remove_file(repo.kio_dir().join("internal")).unwrap();
        assert!(super::acquire_bound_store_lock(&retained).is_ok());
    }

    #[test]
    fn tags_and_retained_roots_cannot_publish_detached_or_future_history() {
        use crate::cas::ObjectKind;

        let scope = tempfile::tempdir().unwrap();
        let repo = Repository::init(scope.path()).unwrap();
        fs::write(scope.path().join("note.md"), b"ancestor").unwrap();
        let ancestor = repo
            .snapshot(Some("ancestor"), None)
            .unwrap()
            .commit_hash
            .unwrap();
        fs::write(scope.path().join("note.md"), b"head").unwrap();
        let head = repo
            .snapshot(Some("head"), None)
            .unwrap()
            .commit_hash
            .unwrap();
        repo.tag("ancestor", Some(&ancestor)).unwrap();
        repo.tag("current", None).unwrap();
        assert_eq!(
            repo.current_ref_targets().unwrap(),
            BTreeSet::from([ancestor, head.clone()])
        );
        let names = repo.kio_dir().join("refs/tags-v1/names.jsonl");
        let original_names = fs::read(&names).unwrap();
        let original_refs = fs::read_dir(names.parent().unwrap()).unwrap().count();

        for (name, parent) in [("detached", None), ("future", Some(head.clone()))] {
            let mut commit = repo.read_commit(&head).unwrap();
            commit.parent = parent;
            commit.message = name.to_owned();
            let (hash, _) = repo
                .object_store()
                .write_json(ObjectKind::Commit, &serde_json::to_value(commit).unwrap())
                .unwrap();
            assert!(repo.tag(name, Some(&hash)).is_err());
            assert!(
                repo.validate_published_history_roots(&BTreeSet::from([
                    head.clone(),
                    hash.clone()
                ]))
                .is_err()
            );
            assert_eq!(fs::read(&names).unwrap(), original_names);
            assert_eq!(
                fs::read_dir(names.parent().unwrap()).unwrap().count(),
                original_refs
            );

            // Imported refs cannot bypass the same ancestry boundary.
            let injected = names.parent().unwrap().join(super::portable_tag_leaf(name));
            fs::write(&injected, &hash).unwrap();
            assert!(repo.current_ref_targets().is_err());
            fs::remove_file(injected).unwrap();
        }
        fs::write(repo.kio_dir().join("HEAD"), b"unborn\n").unwrap();
        assert!(repo.tag("unborn", Some(&head)).is_err());
        assert!(repo.current_ref_targets().is_err());
        assert_eq!(fs::read(&names).unwrap(), original_names);
    }

    #[test]
    fn delete_tag_removes_only_its_ref_and_retains_name_audit() {
        let scope = tempfile::tempdir().unwrap();
        let repo = Repository::init(scope.path()).unwrap();
        fs::write(scope.path().join("note.md"), b"ancestor").unwrap();
        let ancestor = repo
            .snapshot(Some("ancestor"), None)
            .unwrap()
            .commit_hash
            .unwrap();
        fs::write(scope.path().join("note.md"), b"head").unwrap();
        let head = repo
            .snapshot(Some("head"), None)
            .unwrap()
            .commit_hash
            .unwrap();
        repo.tag("Keep", Some(&ancestor)).unwrap();
        repo.tag("other", Some(&head)).unwrap();
        let names = repo.kio_dir().join("refs/tags-v1/names.jsonl");
        let audit_before = fs::read(&names).unwrap();
        let head_before = fs::read(repo.kio_dir().join("HEAD")).unwrap();
        assert_eq!(
            repo.current_ref_targets().unwrap(),
            BTreeSet::from([ancestor.clone(), head.clone()])
        );

        assert_eq!(repo.delete_tag("keep").unwrap(), ancestor);
        assert_eq!(fs::read(&names).unwrap(), audit_before);
        assert_eq!(fs::read(repo.kio_dir().join("HEAD")).unwrap(), head_before);
        assert_eq!(repo.resolve_commit("other").unwrap(), head);
        assert_eq!(repo.current_ref_targets().unwrap(), BTreeSet::from([head]));
        assert_eq!(
            repo.delete_tag("Keep").unwrap_err().error_code(),
            "KIO-E-STORE-NOT-FOUND-001"
        );

        // The old logical-name row remains, and a new spelling records a new
        // row while occupying the same canonical slot.
        repo.tag("KEEP", None).unwrap();
        assert!(fs::read(&names).unwrap().starts_with(&audit_before));
        assert!(fs::read(&names).unwrap().len() > audit_before.len());
    }

    #[test]
    fn delete_tag_rejects_invalid_names_and_corrupt_or_unsafe_refs() {
        let scope = tempfile::tempdir().unwrap();
        let repo = Repository::init(scope.path()).unwrap();
        fs::write(scope.path().join("note.md"), b"note").unwrap();
        let head = repo
            .snapshot(Some("head"), None)
            .unwrap()
            .commit_hash
            .unwrap();
        repo.tag("valid", None).unwrap();
        for name in ["HEAD", "head", "../valid", "CON", "bad."] {
            assert_eq!(
                repo.delete_tag(name).unwrap_err().error_code(),
                "KIO-E-CONFIG-USAGE-001"
            );
        }
        let ref_path = repo
            .kio_dir()
            .join("refs/tags-v1")
            .join(super::portable_tag_leaf("valid"));
        fs::write(&ref_path, format!("{head}\n")).unwrap();
        assert_eq!(
            repo.delete_tag("valid").unwrap_err().error_code(),
            "KIO-E-STORE-CORRUPT-001"
        );
        assert!(ref_path.exists());
        fs::write(&ref_path, &head).unwrap();

        #[cfg(unix)]
        {
            let outside = scope.path().join("outside");
            fs::write(&outside, &head).unwrap();
            fs::remove_file(&ref_path).unwrap();
            std::os::unix::fs::symlink(&outside, &ref_path).unwrap();
            assert!(repo.delete_tag("valid").is_err());
            assert_eq!(fs::read(&outside).unwrap(), head.as_bytes());
            fs::remove_file(&ref_path).unwrap();
            fs::hard_link(&outside, &ref_path).unwrap();
            assert!(repo.delete_tag("valid").is_err());
            assert_eq!(fs::read(&outside).unwrap(), head.as_bytes());
        }
    }

    #[test]
    fn delete_tag_honors_store_lock_and_retained_scope_identity() {
        let scope = tempfile::tempdir().unwrap();
        let repo = Repository::init(scope.path()).unwrap();
        fs::write(scope.path().join("note.md"), b"note").unwrap();
        repo.snapshot(Some("head"), None).unwrap();
        repo.tag("held", None).unwrap();
        let ref_path = repo
            .kio_dir()
            .join("refs/tags-v1")
            .join(super::portable_tag_leaf("held"));

        let held = repo.lock_store_in_session(repo.lock_session()).unwrap();
        let root = repo.canonical_root().to_path_buf();
        let blocked = std::thread::scope(|scope| {
            scope
                .spawn(|| Repository::open(&root).unwrap().delete_tag("held"))
                .join()
                .unwrap()
                .unwrap_err()
        });
        assert_eq!(blocked.error_code(), "KIO-E-STORE-LOCKED-001");
        assert!(ref_path.exists());
        drop(held);

        let detached = scope.path().join("detached-kio");
        fs::rename(repo.kio_dir(), &detached).unwrap();
        fs::create_dir(repo.kio_dir()).unwrap();
        assert!(repo.delete_tag("held").is_err());
        assert!(
            detached
                .join("refs/tags-v1")
                .join(super::portable_tag_leaf("held"))
                .exists()
        );
    }

    #[test]
    fn delete_tag_does_not_require_target_commit_object() {
        let scope = tempfile::tempdir().unwrap();
        let repo = Repository::init(scope.path()).unwrap();
        fs::write(scope.path().join("note.md"), b"ancestor").unwrap();
        let ancestor = repo
            .snapshot(Some("ancestor"), None)
            .unwrap()
            .commit_hash
            .unwrap();
        fs::write(scope.path().join("note.md"), b"head").unwrap();
        repo.snapshot(Some("head"), None).unwrap();
        repo.tag("old", Some(&ancestor)).unwrap();

        // Simulate missing historical CAS material. Removing the ref is still
        // possible because deletion authorizes against the exact ref bytes.
        let digest = ancestor.strip_prefix("sha256:").unwrap();
        let commit_path = repo
            .kio_dir()
            .join("objects/commits")
            .join(&digest[..2])
            .join(&digest[2..4])
            .join(digest);
        fs::remove_file(commit_path).unwrap();
        assert_eq!(repo.delete_tag("old").unwrap(), ancestor);
    }

    #[test]
    fn delete_tag_makes_expired_auto_commit_eligible_for_gc() {
        let scope = tempfile::tempdir().unwrap();
        let repo = Repository::init(scope.path()).unwrap();
        fs::write(scope.path().join("note.md"), b"auto version").unwrap();
        let auto = repo
            .auto_snapshot_with_normalize(
                Some("auto"),
                Some("2026-01-01T00:00:00Z"),
                &BTreeSet::new(),
                &BTreeMap::new(),
            )
            .unwrap()
            .commit_hash
            .unwrap();
        fs::write(scope.path().join("note.md"), b"manual version").unwrap();
        repo.snapshot(Some("manual"), Some("2026-01-02T00:00:00Z"))
            .unwrap();
        repo.tag("preserve", Some(&auto)).unwrap();

        let planner = crate::gc::GcPlanner::bind(repo.canonical_root()).unwrap();
        let as_of = super::parse_utc_seconds("2027-01-01T00:00:00Z").unwrap();
        let protected = planner.plan_at(as_of).unwrap();
        assert!(
            !protected
                .candidates
                .iter()
                .any(|item| item.commit_hash == auto)
        );

        assert_eq!(repo.delete_tag("preserve").unwrap(), auto);
        let released = planner.plan_at(as_of).unwrap();
        assert!(
            released
                .candidates
                .iter()
                .any(|item| item.commit_hash == auto)
        );
    }

    #[test]
    fn tag_refuses_mismatched_or_coexisting_shallow_receipts() {
        use std::collections::{BTreeMap, BTreeSet};

        let scope = tempfile::tempdir().unwrap();
        std::fs::write(scope.path().join("note.txt"), b"fixture").unwrap();
        let repo = Repository::init(scope.path()).unwrap();
        let commit = repo
            .snapshot(Some("fixture"), Some("2026-01-01T00:00:00Z"))
            .unwrap()
            .commit_hash
            .expect("snapshot must create a commit");
        let tree = repo.read_commit(&commit).unwrap().tree;
        let shallow = repo.kio_dir().join("gc/shallowed");
        let shallow_directory =
            crate::store_dir::StoreDirectory::open_or_create_directories(&shallow).unwrap();
        crate::private_fs::verify_private_directory_handle(&shallow_directory).unwrap();
        let leaf = &commit["sha256:".len()..];
        let write_receipt = |leaf: &str, bytes: &[u8]| {
            use std::io::Write as _;
            let mut file = shallow_directory
                .create_private_file(Path::new(leaf))
                .unwrap();
            file.write_all(bytes).unwrap();
        };

        let wrong_tree = format!("sha256:{}", "f".repeat(64));
        write_receipt(
            leaf,
            &crate::gc::ShallowReceipt::new(
                commit.clone(),
                wrong_tree,
                "2026-01-01T00:00:01Z".into(),
            )
            .unwrap()
            .canonical_bytes()
            .unwrap(),
        );
        let mismatch = repo.tag("mismatched", Some(&commit)).unwrap_err();
        assert_eq!(mismatch.error_code(), "KIO-E-STORE-CORRUPT-001");
        std::fs::remove_file(shallow.join(leaf)).unwrap();

        write_receipt(
            leaf,
            &crate::gc::ShallowReceipt::new(
                commit.clone(),
                tree.clone(),
                "2026-01-01T00:00:01Z".into(),
            )
            .unwrap()
            .canonical_bytes()
            .unwrap(),
        );
        let coexistence = repo.tag("coexisting", Some(&commit)).unwrap_err();
        assert_eq!(coexistence.error_code(), "KIO-E-STORE-CORRUPT-001");
        std::fs::remove_file(shallow.join(leaf)).unwrap();

        let store = crate::cas::ObjectStore::new(repo.kio_dir());
        // A real final shallow boundary is an Auto commit that is no longer a
        // ref tip.  It remains in history through the current Manual child,
        // but a tag must not revive it as a live tip.
        std::fs::write(scope.path().join("note.txt"), b"auto fixture").unwrap();
        let auto = repo
            .auto_snapshot_with_normalize(
                Some("auto fixture"),
                Some("2026-01-01T00:00:02Z"),
                &BTreeSet::new(),
                &BTreeMap::new(),
            )
            .unwrap()
            .commit_hash
            .expect("auto snapshot must create a commit");
        let auto_tree = repo.read_commit(&auto).unwrap().tree;
        std::fs::write(scope.path().join("note.txt"), b"manual child").unwrap();
        repo.snapshot(Some("manual child"), Some("2026-01-01T00:00:03Z"))
            .unwrap();
        let auto_leaf = &auto["sha256:".len()..];
        write_receipt(
            auto_leaf,
            &crate::gc::ShallowReceipt::new(
                auto.clone(),
                auto_tree.clone(),
                "2026-01-01T00:00:04Z".into(),
            )
            .unwrap()
            .canonical_bytes()
            .unwrap(),
        );
        std::fs::remove_file(
            store
                .object_path(crate::cas::ObjectKind::Tree, &auto_tree)
                .unwrap(),
        )
        .unwrap();
        let canonical_final = repo.tag("canonical-final", Some(&auto)).unwrap_err();
        assert_eq!(
            canonical_final.error_code(),
            "KIO-E-COMMIT-SHALLOW-001",
            "canonical final shallow boundary: {canonical_final:?}"
        );
    }

    #[test]
    fn snapshot_rejects_invalid_shallow_base_before_archiving_working_bytes() {
        let scope = tempfile::tempdir().unwrap();
        std::fs::write(scope.path().join("note.txt"), b"base bytes").unwrap();
        let repo = Repository::init(scope.path()).unwrap();
        let base = repo
            .snapshot(Some("base"), Some("2026-01-01T00:00:00Z"))
            .unwrap()
            .commit_hash
            .expect("snapshot must create a commit");
        let base_tree = repo.read_commit(&base).unwrap().tree;
        let store = crate::cas::ObjectStore::new(repo.kio_dir());
        std::fs::remove_file(
            store
                .object_path(crate::cas::ObjectKind::Tree, &base_tree)
                .unwrap(),
        )
        .unwrap();

        let new_bytes = b"must not be archived";
        std::fs::write(scope.path().join("new.txt"), new_bytes).unwrap();
        let new_raw = crate::cas::hash_bytes(new_bytes);
        let error = repo
            .snapshot(Some("must fail"), Some("2026-01-01T00:00:01Z"))
            .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
        assert!(
            !store
                .object_path(crate::cas::ObjectKind::Raw, &new_raw)
                .unwrap()
                .exists(),
            "a rejected base must not archive a newly discovered working file"
        );
        assert_eq!(repo.head_commit_hash().unwrap(), Some(base));
    }

    #[test]
    fn config_rejects_retired_format_version_key() {
        let scope = tempfile::tempdir().unwrap();
        let repo = Repository::init(scope.path()).unwrap();
        std::fs::write(
            repo.kio_dir().join("config.toml"),
            "kio_format_version = \"0.1.0\"\n",
        )
        .unwrap();

        let error = Repository::open(scope.path()).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-CONFIG-SCHEMA-001");
    }

    #[test]
    fn working_tree_pins_the_validated_chunking_config() {
        let scope = tempfile::tempdir().unwrap();
        let repo = Repository::init(scope.path()).unwrap();
        fs::write(scope.path().join("note.md"), b"body").unwrap();

        let default_tree = repo.build_working_tree(false).unwrap().tree;
        assert_eq!(
            default_tree.chunking_config_hash,
            super::chunking_config_hash("heading", 6_000).unwrap()
        );

        fs::write(
            repo.kio_dir().join("config.toml"),
            "[chunking]\nstrategy = \"heading\"\nmax_chars = 40\n",
        )
        .unwrap();
        let configured_tree = repo.build_working_tree(false).unwrap().tree;
        assert_eq!(
            configured_tree.chunking_config_hash,
            super::chunking_config_hash("heading", 40).unwrap()
        );
        assert_ne!(
            configured_tree.chunking_config_hash,
            default_tree.chunking_config_hash
        );

        fs::write(
            repo.kio_dir().join("config.toml"),
            "[chunking]\nmax_chars = 0\n",
        )
        .unwrap();
        assert_eq!(
            repo.build_working_tree(false).unwrap_err().error_code(),
            "KIO-E-CONFIG-SCHEMA-001"
        );
        fs::write(
            repo.kio_dir().join("config.toml"),
            "[chunking]\nmax_chars = \"40\"\n",
        )
        .unwrap();
        assert_eq!(
            repo.build_working_tree(false).unwrap_err().error_code(),
            "KIO-E-CONFIG-SCHEMA-001"
        );
    }

    #[test]
    fn current_ref_targets_reads_head_and_canonical_tags_without_names_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        fs::write(dir.path().join("note.md"), b"ancestor").unwrap();
        let tag = repo
            .snapshot(Some("ancestor"), None)
            .unwrap()
            .commit_hash
            .unwrap();
        fs::write(dir.path().join("note.md"), b"head").unwrap();
        let head = repo
            .snapshot(Some("head"), None)
            .unwrap()
            .commit_hash
            .unwrap();
        let tags = repo.kio_dir().join("refs/tags-v1");
        fs::create_dir_all(&tags).unwrap();
        fs::write(tags.join(format!("tag-{}", "d".repeat(64))), &tag).unwrap();
        fs::write(tags.join("names.jsonl"), b"not a ref\n").unwrap();

        let expected = BTreeSet::from([head, tag]);
        assert_eq!(repo.current_ref_targets().unwrap(), expected);
    }

    #[test]
    fn current_ref_targets_rejects_invalid_tag_leaf_instead_of_omitting_it() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let tags = repo.kio_dir().join("refs/tags-v1");
        fs::create_dir_all(&tags).unwrap();
        fs::write(tags.join("old-tag"), format!("sha256:{}", "a".repeat(64))).unwrap();

        assert!(repo.current_ref_targets().is_err());
    }

    /// D7 (07 §7): the `offline_api` sub-table is wired, so a non-default
    /// timeout there is accepted rather than rejected.
    #[test]
    fn d7_offline_api_timeout_override_is_accepted() {
        let config = serde_json::json!({
            "adapter": { "policy": { "offline_api": { "timeout_seconds": 1800 } } }
        });
        assert!(enforce_config_semantics(&config).is_ok());
    }

    #[test]
    fn device_ca_is_accepted_for_user_config_but_refused_in_scope_config() {
        let config = serde_json::json!({
            "adapter": {
                "policy": {
                    "offline_api": { "ca_pem_path": "/private/device-ca.pem" }
                }
            }
        });
        assert!(enforce_config_semantics(&config).is_ok());
        let error = enforce_scope_config_semantics(&config).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-CONFIG-NOT-IMPLEMENTED-001");
    }

    /// The parent value is untouched by D7 — still only the documented default.
    /// Widening it would change every online adapter's behaviour, including the
    /// billed ones, which D7 does not ask for.
    #[test]
    fn d7_leaves_the_parent_timeout_rejection_alone() {
        let config = serde_json::json!({
            "adapter": { "policy": { "timeout_seconds": 600 } }
        });
        assert!(enforce_config_semantics(&config).is_err());
        let documented_default = serde_json::json!({
            "adapter": { "policy": { "timeout_seconds": 300 } }
        });
        assert!(enforce_config_semantics(&documented_default).is_ok());
    }

    /// The other two modes are refused, not ignored. Nothing honours them yet,
    /// and accepting a timeout that never takes effect is worse than refusing
    /// it: the operator would believe a limit is in force that is not.
    #[test]
    fn d7_refuses_the_execution_modes_that_are_not_wired() {
        for mode in ["online_api", "deterministic_library"] {
            let config = serde_json::json!({
                "adapter": { "policy": { mode: { "timeout_seconds": 600 } } }
            });
            let error = enforce_config_semantics(&config).expect_err(mode);
            assert_eq!(
                error.error_code(),
                "KIO-E-CONFIG-NOT-IMPLEMENTED-001",
                "{mode}"
            );
        }
    }
    use crate::cas::{ContentObjectKind, ObjectKind, ObjectStore, hash_bytes};
    use crate::dag::{CommitType, NormalizeRef};
    use crate::purge::PurgeState;
    use serde_json::json;
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;

    #[test]
    fn read_network_approval_pending_returns_none_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        assert_eq!(read_network_approval_pending(repo.kio_dir()).unwrap(), None);
    }

    #[test]
    fn read_network_approval_pending_returns_the_stored_object_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let scope_id = repo.scope_identity().unwrap().scope_id;
        let pending = json!({
            "scope_id": scope_id,
            "tool_id": "mistral_ocr_markdownize",
            "execution_mode": "online_api",
            "tool_profile_hash": format!("sha256:{}", "a".repeat(64)),
            "approved_at": "2026-07-22T00:00:00Z",
            "approval_method": "approve",
        });
        write_network_approval_pending(repo.kio_dir(), pending.clone()).unwrap();
        assert_eq!(
            read_network_approval_pending(repo.kio_dir()).unwrap(),
            Some(pending)
        );
    }

    #[test]
    fn approval_publication_is_serialized_with_revoke_and_stale_pending_cannot_resurrect_it() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let scope_id = repo.scope_identity().unwrap().scope_id;
        let pending = json!({
            "scope_id": scope_id,
            "tool_id": "mistral_ocr_markdownize",
            "execution_mode": "online_api",
            "tool_profile_hash": format!("sha256:{}", "a".repeat(64)),
            "approved_at": "2026-07-22T00:00:00Z",
            "approval_method": "approve",
        });
        let row = json!({
            "scope_id": scope_id,
            "tool_id": "mistral_ocr_markdownize",
            "execution_mode": "online_api",
            "tool_profile_hash": format!("sha256:{}", "a".repeat(64)),
            "approved_at": "2026-07-22T00:00:00Z",
            "approval_method": "approve",
            "status": "active",
        });
        write_network_approval_pending(repo.kio_dir(), pending.clone()).unwrap();

        // The cross-thread call cannot enter revoke's read/modify/write
        // region while this command owns the same retained store identity.
        // This is a deterministic contention check, not a scheduling race.
        let held = repo.lock_store_in_session(repo.lock_session()).unwrap();
        let kio = repo.kio_dir().to_path_buf();
        let blocked = std::thread::spawn(move || {
            revoke_network_approval(
                &kio,
                Some("mistral_ocr_markdownize"),
                "2026-07-23T00:00:00Z",
            )
        })
        .join()
        .unwrap()
        .unwrap_err();
        assert_eq!(blocked.error_code(), "KIO-E-STORE-LOCKED-001");
        assert_eq!(
            read_network_approval_pending(repo.kio_dir()).unwrap(),
            Some(pending.clone())
        );
        drop(held);

        let revoked = revoke_network_approval(
            repo.kio_dir(),
            Some("mistral_ocr_markdownize"),
            "2026-07-23T00:00:00Z",
        )
        .unwrap();
        assert!(revoked.pending_removed);
        let error = publish_network_approval(repo.kio_dir(), row, Some(&pending)).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-ADAPTER-APPROVAL-CONFLICT-001");
        assert!(read_network_approvals(repo.kio_dir()).unwrap().is_empty());
        assert_eq!(read_network_approval_pending(repo.kio_dir()).unwrap(), None);
    }

    #[test]
    fn scope_validation_rejects_missing_or_non_string_version_as_incompatible() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let scope_path = repo.kio_dir().join("scope.json");

        for invalid_version in [None, Some(json!(1))] {
            let mut scope: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&scope_path).unwrap()).unwrap();
            match invalid_version {
                Some(version) => scope["kio_format_version"] = version,
                None => {
                    scope.as_object_mut().unwrap().remove("kio_format_version");
                }
            }
            fs::write(&scope_path, serde_json::to_vec_pretty(&scope).unwrap()).unwrap();
            let error = repo.scope_identity().unwrap_err();
            assert_eq!(error.error_code(), "KIO-E-STORE-VERSION-001");
        }
    }

    #[test]
    fn incompatible_scope_versions_precede_current_schema_validation() {
        for version in [
            "0.0.0",
            "0.1.0",
            "0.2.0",
            "1.0.0",
            "1.0.1",
            "2.0.0",
            "4.0.0",
            "malformed",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let repo = Repository::init(dir.path()).unwrap();
            let scope_path = repo.kio_dir().join("scope.json");
            let mut scope: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&scope_path).unwrap()).unwrap();
            scope["kio_format_version"] = json!(version);
            scope["future_only_key"] = json!(true);
            fs::write(&scope_path, serde_json::to_vec_pretty(&scope).unwrap()).unwrap();

            let error = repo.scope_identity().unwrap_err();
            assert_eq!(error.error_code(), "KIO-E-STORE-VERSION-001");
            assert_eq!(error.context()["found"], version);
        }
    }

    #[test]
    fn old_format_is_rejected_before_any_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let scope_path = repo.kio_dir().join("scope.json");
        let mut scope: Value = serde_json::from_slice(&fs::read(&scope_path).unwrap()).unwrap();
        scope["kio_format_version"] = json!("2.0.0");
        fs::write(&scope_path, serde_json::to_vec(&scope).unwrap()).unwrap();
        let before_scope = fs::read(&scope_path).unwrap();
        let before_head = fs::read(repo.kio_dir().join("HEAD")).unwrap();
        let error = Repository::open(dir.path()).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-VERSION-001");
        assert_eq!(fs::read(&scope_path).unwrap(), before_scope);
        assert_eq!(fs::read(repo.kio_dir().join("HEAD")).unwrap(), before_head);
    }

    #[test]
    fn unborn_head_is_explicit_and_blank_head_is_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        assert_eq!(fs::read(repo.kio_dir().join("HEAD")).unwrap(), b"unborn\n");
        assert_eq!(repo.head_commit_hash().unwrap(), None);
        fs::write(repo.kio_dir().join("HEAD"), b"").unwrap();
        assert!(repo.head_commit_hash().is_err());
    }

    #[test]
    fn approval_readers_reject_malformed_current_scope_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let scope_path = repo.kio_dir().join("scope.json");
        let scope_id = repo.scope_identity().unwrap().scope_id;
        let row = |status: &str| {
            json!({
                "scope_id": scope_id,
                "tool_id": "mistral_ocr_markdownize",
                "execution_mode": "online_api",
                "tool_profile_hash": format!("sha256:{}", "a".repeat(64)),
                "approved_at": "2026-07-22T00:00:00Z",
                "approval_method": "approve",
                "status": status,
            })
        };

        let cases = [
            json!({ "approvals": [{
                "scope_id": scope_id,
                "tool_id": "mistral_ocr_markdownize",
                "execution_mode": "online_api",
                "tool_profile_hash": format!("sha256:{}", "a".repeat(64)),
                "approved_at": "2026-07-22T00:00:00Z",
                "approval_method": "approve"
            }]}),
            json!({ "approvals": [row("revoked")] }),
            json!({ "approval_pending": {
                "scope_id": scope_id,
                "tool_id": "mistral_ocr_markdownize",
                "execution_mode": "online_api",
                "tool_profile_hash": format!("sha256:{}", "a".repeat(64))
            }}),
        ];

        for patch in cases {
            let mut scope: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&scope_path).unwrap()).unwrap();
            for (key, value) in patch.as_object().unwrap() {
                scope[key] = value.clone();
            }
            fs::write(&scope_path, serde_json::to_vec_pretty(&scope).unwrap()).unwrap();
            assert!(read_network_approvals(repo.kio_dir()).is_err());

            let mut current: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&scope_path).unwrap()).unwrap();
            current.as_object_mut().unwrap().remove("approvals");
            current.as_object_mut().unwrap().remove("approval_pending");
            fs::write(&scope_path, serde_json::to_vec_pretty(&current).unwrap()).unwrap();
        }
    }

    #[test]
    fn process_liveness_recognizes_current_process_and_released_sentinel() {
        assert!(process_is_alive(std::process::id()));
        assert!(!process_is_alive(RELEASED_LOCK_PID));
    }

    #[cfg(windows)]
    #[test]
    fn windows_retained_lock_contends_with_ordinary_writer_and_releases_exact_owner() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let kio = repo.bound_kio_handle().unwrap();
        let held = super::acquire_bound_store_lock(kio).unwrap();
        assert!(repo.lock_store_in_session(repo.lock_session()).is_err());
        drop(held);
        let ordinary = repo.lock_store_in_session(repo.lock_session()).unwrap();
        assert!(super::acquire_bound_store_lock(kio).is_err());
        drop(ordinary);
        assert!(super::acquire_bound_store_lock(kio).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn windows_working_file_read_remains_bound_after_root_rename() {
        use std::io::Read;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("scope");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("doc.md"), b"retained input").unwrap();
        let repo = Repository::init(&root).unwrap();
        let candidates = repo
            .working_file_candidates(
                &BTreeSet::new(),
                &BTreeSet::new(),
                true,
                ArchiveLimits::default(),
            )
            .unwrap();
        let candidate = candidates
            .iter()
            .find(|item| item.file_name == "doc.md")
            .unwrap();
        fs::rename(&root, dir.path().join("moved")).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(root.join("doc.md"), b"replacement must not be read").unwrap();
        let mut file = super::open_working_file_candidate(
            candidate,
            &crate::control::ControlHandle::default(),
        )
        .unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"retained input");
    }

    #[cfg(windows)]
    #[test]
    fn windows_event_log_first_creates_private_parents_without_repairing_existing_prefix() {
        use crate::{
            platform::verify_owner_private_handle,
            private_fs::verify_private_directory,
            store_dir::StoreDirectory,
            test_control::{TestEnvGuard, test_env_lock},
        };
        use std::{mem, os::windows::io::AsRawHandle, ptr};
        use windows_sys::Win32::{
            Foundation::LocalFree,
            Security::{
                ACL,
                Authorization::{GetSecurityInfo, SE_FILE_OBJECT},
                DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl, PSECURITY_DESCRIPTOR,
            },
            Storage::FileSystem::{FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx},
        };

        fn snapshot(file: &std::fs::File) -> (u64, [u8; 16], u16, Vec<u8>) {
            let mut identity = FILE_ID_INFO::default();
            assert_ne!(
                unsafe {
                    GetFileInformationByHandleEx(
                        file.as_raw_handle() as _,
                        FileIdInfo,
                        (&mut identity as *mut FILE_ID_INFO).cast(),
                        mem::size_of::<FILE_ID_INFO>() as u32,
                    )
                },
                0
            );
            let mut acl = ptr::null_mut::<ACL>();
            let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
            assert_eq!(
                unsafe {
                    GetSecurityInfo(
                        file.as_raw_handle() as _,
                        SE_FILE_OBJECT,
                        DACL_SECURITY_INFORMATION,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        &mut acl,
                        ptr::null_mut(),
                        &mut descriptor,
                    )
                },
                0
            );
            let mut control = 0;
            let mut revision = 0;
            let control_ok =
                unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) };
            let bytes = if acl.is_null() {
                Vec::new()
            } else {
                // GetSecurityInfo supplies a valid ACL in its returned allocation.
                unsafe { std::slice::from_raw_parts(acl.cast::<u8>(), (*acl).AclSize as usize) }
                    .to_vec()
            };
            unsafe { LocalFree(descriptor) };
            assert_ne!(control_ok, 0);
            (
                identity.VolumeSerialNumber,
                identity.FileId.Identifier,
                control,
                bytes,
            )
        }

        let _env_lock = test_env_lock().lock().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let existing_path = fixture.path().canonicalize().unwrap();
        let existing = StoreDirectory::open(&existing_path).unwrap();
        let existing_before = snapshot(&existing.root_handle());
        assert!(verify_owner_private_handle(&existing.root_handle()).is_err());
        let device = existing_path.join("device");
        let _data_home = TestEnvGuard::set("XDG_DATA_HOME", &device);
        assert!(!device.exists());

        super::append_event_log("PRIVATE-PARENT-FIRST", "first log append", json!({})).unwrap();
        let paths = [device.clone(), device.join("kio"), device.join("kio/logs")];
        let mut created = Vec::new();
        for path in &paths {
            let directory = verify_private_directory(path).unwrap();
            verify_owner_private_handle(&directory.root_handle()).unwrap();
            created.push(snapshot(&directory.root_handle()));
        }
        let log = device.join("kio/logs/events.jsonl");
        let reopened = open_windows_lock_parent(&device.join("kio/logs/scrub.lock")).unwrap();
        assert_eq!(snapshot(&reopened), created[2]);
        drop(reopened);
        super::append_event_log("PRIVATE-PARENT-SECOND", "second log append", json!({})).unwrap();
        for (path, before) in paths.iter().zip(&created) {
            let directory = verify_private_directory(path).unwrap();
            assert_eq!(&snapshot(&directory.root_handle()), before);
        }
        let events = fs::read_to_string(log).unwrap();
        assert!(events.contains("PRIVATE-PARENT-FIRST"));
        assert!(events.contains("PRIVATE-PARENT-SECOND"));
        assert_eq!(snapshot(&existing.root_handle()), existing_before);
        assert!(verify_owner_private_handle(&existing.root_handle()).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_lock_leaf_rejects_parent_traversal_and_preserves_foreign_token() {
        let traversal = PathBuf::from("safe").join("..").join("ordinary.lock");
        assert!(open_windows_lock_parent(&traversal).is_err());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ordinary.lock");
        let parent = open_windows_lock_parent(&path).unwrap();
        let token = "foreign-token";
        let bytes = canonical_lock_bytes(4242, token).unwrap();
        let owner =
            create_windows_lock(&parent, windows_lock_leaf(&path).unwrap(), &bytes).unwrap();
        release_windows_owned_lock(&owner, 4242, "different-token").unwrap();
        assert_eq!(read_windows_lock(&owner).unwrap().0.token, token);
        release_windows_owned_lock(&owner, 4242, token).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_lock_contention_classifier_is_limited_to_lock_create_open_conflicts() {
        use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION};

        for raw_code in [ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION] {
            assert!(is_windows_lock_contention(
                &std::io::Error::from_raw_os_error(raw_code as i32)
            ));
        }
        for raw_code in [2, 87, 112] {
            assert!(!is_windows_lock_contention(
                &std::io::Error::from_raw_os_error(raw_code)
            ));
        }
        assert!(is_windows_lock_contention(&std::io::Error::from(
            std::io::ErrorKind::AlreadyExists
        )));
        assert!(is_windows_lock_contention(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        )));
    }

    #[cfg(windows)]
    #[test]
    fn windows_lock_rejects_malformed_existing_leaf_without_touching_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ordinary.lock");
        let bytes = b"malformed Windows lock record";
        fs::write(&path, bytes).unwrap();

        let error = match StoreLock::acquire_path(path.clone()) {
            Ok(_) => panic!("malformed lock must be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.error_code(), "KIO-E-STORE-LOCKED-001");
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[cfg(windows)]
    #[test]
    fn windows_lock_rejects_hardlinked_stale_leaf_without_touching_either_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ordinary.lock");
        let alias = dir.path().join("ordinary.alias");
        let bytes = canonical_lock_bytes(RELEASED_LOCK_PID, "retired-owner").unwrap();
        fs::write(&path, &bytes).unwrap();
        fs::hard_link(&path, &alias).unwrap();

        let error = match StoreLock::acquire_path(path.clone()) {
            Ok(_) => panic!("hardlinked lock must be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.error_code(), "KIO-E-STORE-LOCKED-001");
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(fs::read(&alias).unwrap(), bytes);
    }

    #[cfg(windows)]
    #[test]
    fn windows_concurrent_stale_reclaim_has_exactly_one_owner() {
        use std::sync::{Arc, Barrier, Condvar, Mutex};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("contended.lock");
        let parent = crate::store_dir::StoreDirectory::open(&root).unwrap();
        let stale = create_windows_lock(
            &parent.root_handle(),
            std::path::Path::new("contended.lock"),
            &canonical_lock_bytes(RELEASED_LOCK_PID, "retired-owner").unwrap(),
        )
        .unwrap();
        drop(stale);
        let start = Arc::new(Barrier::new(3));
        // The successful owner remains live until the other worker has made
        // its attempt, so the assertion measures overlap rather than timing.
        let state = Arc::new((Mutex::new((0_usize, false)), Condvar::new()));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let start = Arc::clone(&start);
            let state = Arc::clone(&state);
            let path = path.clone();
            workers.push(std::thread::spawn(move || {
                start.wait();
                match StoreLock::acquire_path(path) {
                    Ok(lock) => {
                        let (mutex, wake) = &*state;
                        let mut state = mutex.lock().unwrap();
                        state.0 += 1;
                        wake.notify_all();
                        while !state.1 {
                            state = wake.wait(state).unwrap();
                        }
                        drop(lock);
                        Ok(true)
                    }
                    Err(error) => {
                        let (mutex, wake) = &*state;
                        let mut state = mutex.lock().unwrap();
                        state.0 += 1;
                        wake.notify_all();
                        Err(error.error_code().to_owned())
                    }
                }
            }));
        }
        start.wait();
        let timed_out = {
            let (mutex, wake) = &*state;
            let mut state = mutex.lock().unwrap();
            let mut timed_out = false;
            while state.0 != 2 {
                let (next, timeout) = wake
                    .wait_timeout(state, std::time::Duration::from_secs(5))
                    .unwrap();
                state = next;
                if timeout.timed_out() {
                    timed_out = true;
                    break;
                }
            }
            state.1 = true;
            wake.notify_all();
            timed_out
        };
        let outcomes: Vec<Result<bool, String>> = workers
            .into_iter()
            .map(|worker| worker.join().expect("worker must not panic"))
            .collect();
        assert!(!timed_out, "both workers must report their attempt");
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, Ok(true)))
                .count(),
            1
        );
        for outcome in outcomes {
            if let Err(error_code) = outcome {
                assert_eq!(error_code, "KIO-E-STORE-LOCKED-001");
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_lock_rejects_junction_parent_without_touching_external_target() {
        let root = tempfile::tempdir().unwrap();
        let external = root.path().join("external");
        let junction = root.path().join("junction");
        fs::create_dir(&external).unwrap();
        let status = std::process::Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&external)
            .status()
            .expect("create Windows junction fixture");
        assert!(status.success(), "mklink /J must create the test junction");

        let path = junction.join("nested/ordinary.lock");
        assert!(StoreLock::acquire_path(path).is_err());
        assert!(
            !external.join("nested/ordinary.lock").exists(),
            "a junction must not receive a lock parent or leaf"
        );
        fs::remove_dir(&junction).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unix_process_liveness_rejects_values_outside_the_pid_range() {
        assert!(!process_is_alive(0));
        assert!(!process_is_alive(RELEASED_LOCK_PID - 1));
    }

    #[cfg(windows)]
    #[test]
    fn windows_process_liveness_rejects_reserved_pid_zero() {
        assert!(!process_is_alive(0));
    }

    // F8: a device-global lock (e.g. the pre-2026-07-18 JSONL cost ledger's) is
    // acquired via `acquire_path` at an arbitrary path outside any `.kio`. It
    // creates the parent dir, then uses the verified exchange on macOS/Linux,
    // retained-handle deletion on Windows; it refuses an independently held lock.
    #[test]
    fn f8_acquire_path_is_device_global_and_excludes_a_held_lock() {
        let dir = tempfile::tempdir().unwrap();
        // Nested path whose parent does not exist yet — acquire_path must create it.
        let lock_path = dir.path().join("kio/device-example.lock");
        {
            let _guard = StoreLock::acquire_path(lock_path.clone()).unwrap();
            assert!(
                lock_path.exists(),
                "lock file must be created under a fresh dir"
            );
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        assert!(
            lock_path.exists(),
            "macOS/Linux drop leaves an atomic release sentinel"
        );
        #[cfg(windows)]
        assert!(
            !lock_path.exists(),
            "Windows releases only its retained owned handle"
        );

        let reacquired = StoreLock::acquire_path(lock_path.clone())
            .expect("next writer reacquires after release");
        assert!(lock_path.exists(), "reacquired writer owns the lock path");
        drop(reacquired);

        // A pre-existing lock file at the path blocks a fresh acquisition with
        // STORE-LOCKED, proving acquire_path honors a held device-global lock.
        fs::write(&lock_path, b"held by another charge").unwrap();
        // Keep the acquisition result (and a hypothetical successful guard)
        // alive across the assertion, matching Rust 2021's tail-expression
        // drop order even though this branch is expected to be an error.
        let contested_acquisition = StoreLock::acquire_path(lock_path.clone());
        match contested_acquisition {
            Ok(_) => panic!("a held device-global lock must block acquisition"),
            Err(err) => assert_eq!(err.error_code(), "KIO-E-STORE-LOCKED-001"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn ordinary_lock_drop_unlocks_an_inherited_open_file_description() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inherited.lock");
        let held = StoreLock::acquire_path(path.clone()).unwrap();
        let inherited_gate = held
            .native
            .as_ref()
            .unwrap()
            ._gate
            ._file
            .try_clone()
            .unwrap();

        drop(held);
        assert!(StoreLock::acquire_path(path).is_ok());
        drop(inherited_gate);
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_reclaim_of_release_sentinel_has_one_owner() {
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contended.lock");
        drop(StoreLock::acquire_path(path.clone()).unwrap());
        let start = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let start = Arc::clone(&start);
            let path = path.clone();
            workers.push(std::thread::spawn(move || {
                start.wait();
                match StoreLock::acquire_path(path) {
                    Ok(lock) => {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        drop(lock);
                        true
                    }
                    Err(error) => {
                        assert_eq!(error.error_code(), "KIO-E-STORE-LOCKED-001");
                        false
                    }
                }
            }));
        }
        start.wait();
        let outcomes: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(outcomes.into_iter().filter(|outcome| *outcome).count(), 1);
    }

    #[test]
    fn redact_message_paths_masks_absolute_paths_only() {
        // P4: the exact leak shapes — `io error at {path}` and
        // `corrupt store file at {path}` — must lose the absolute path.
        assert_eq!(
            redact_message_paths("io error at /private/var/x/.kio/tasks.jsonl: Permission denied"),
            "io error at [redacted] Permission denied"
        );
        assert_eq!(
            redact_message_paths(
                "corrupt store file at /home/u/.kio/tasks.jsonl: expected value at line 1"
            ),
            "corrupt store file at [redacted] expected value at line 1"
        );
        // Relative tokens and plain prose are untouched (no false positives).
        assert_eq!(
            redact_message_paths("scope registry write failed (recover with index)"),
            "scope registry write failed (recover with index)"
        );
        assert!(!redact_message_paths("read /etc/hosts now").contains("/etc/hosts"));
        assert_eq!(
            redact_message_paths(
                r"corrupt store file at C:\Users\runner\.kio\tasks.jsonl: expected value"
            ),
            "corrupt store file at [redacted] expected value"
        );
        assert_eq!(
            redact_message_paths("read C:/Users/runner/.kio/tasks.jsonl now"),
            "read [redacted] now"
        );
        assert_eq!(
            redact_message_paths(r"io error at \\server\share\scope\.kio\tasks.jsonl: denied"),
            "io error at [redacted] denied"
        );
        assert_eq!(
            redact_message_paths(r"read \\?\C:\scope\.kio\tasks.jsonl now"),
            "read [redacted] now"
        );
        assert_eq!(
            redact_message_paths(r"relative C:scope\.kio\tasks.jsonl is unchanged"),
            r"relative C:scope\.kio\tasks.jsonl is unchanged"
        );
    }

    #[test]
    fn redact_context_masks_scope_path_and_candidates() {
        // P4: the extended allowlist covers the path-bearing keys used by the
        // purge / scope-ambiguous / registry error contexts.
        let mut context = json!({
            "scope_path": "/private/var/x/.kio",
            "candidates": ["/a/.kio", "/b/.kio"],
            "root_path": "/private/var/x",
            "kio_path": "/private/var/x/.kio",
            "raw_hash": "sha256:abc",
        });
        redact_context(&mut context);
        assert_eq!(context["scope_path"], "[redacted]");
        assert_eq!(context["candidates"], "[redacted]");
        assert_eq!(context["root_path"], "[redacted]");
        assert_eq!(context["kio_path"], "[redacted]");
        // Non-sensitive keys are preserved.
        assert_eq!(context["raw_hash"], "sha256:abc");
    }

    #[test]
    fn format_unix_seconds_known_vectors() {
        // Epoch and known Unix timestamps.
        assert_eq!(format_unix_seconds(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_unix_seconds(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(format_unix_seconds(1_777_464_000), "2026-04-29T12:00:00Z");
        // 2024 is a leap year: 02-29 exists and spans a full day.
        assert_eq!(format_unix_seconds(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(format_unix_seconds(1_709_251_199), "2024-02-29T23:59:59Z");
        // 2100 is NOT a leap year (÷100, not ÷400): 02-28 rolls to 03-01.
        assert_eq!(format_unix_seconds(4_107_542_399), "2100-02-28T23:59:59Z");
        assert_eq!(format_unix_seconds(4_107_542_400), "2100-03-01T00:00:00Z");
        // Month / year boundary.
        assert_eq!(format_unix_seconds(1_704_067_199), "2023-12-31T23:59:59Z");
        assert_eq!(format_unix_seconds(1_704_067_200), "2024-01-01T00:00:00Z");
    }

    #[test]
    fn parse_utc_seconds_round_trips_and_rejects_bad_shapes() {
        // Round-trips against the known format vectors.
        for secs in [
            0,
            1_700_000_000,
            1_709_251_199,
            4_107_542_400,
            1_704_067_200,
        ] {
            assert_eq!(parse_utc_seconds(&format_utc_seconds(secs)), Some(secs));
        }
        // Offset arithmetic used by retry backoff scheduling.
        let base = parse_utc_seconds("2026-07-03T00:00:00Z").unwrap();
        assert_eq!(format_utc_seconds(base + 2), "2026-07-03T00:00:02Z");
        assert_eq!(format_utc_seconds(base + 60), "2026-07-03T00:01:00Z");
        // Malformed inputs are rejected rather than silently misparsed.
        assert_eq!(parse_utc_seconds("2026-07-03T00:00:00"), None);
        assert_eq!(parse_utc_seconds("2026-13-03T00:00:00Z"), None);
        assert_eq!(parse_utc_seconds("not-a-timestamp"), None);
    }

    /// R23-16 (06 §11 L513): a fractional-seconds suffix is a valid persisted
    /// timestamp shape ("正: 2026-04-25T12:00:00.123456Z") that the parser
    /// used to reject outright (exact 20-byte match only) -- silently
    /// excluding any record whose `created_at` carried sub-second precision
    /// from callers like `kio log --since`.
    #[test]
    fn r23_16_parse_utc_seconds_accepts_fractional_suffix() {
        let whole = parse_utc_seconds("2026-07-03T00:00:00Z").unwrap();
        // Sub-second digits parse to the SAME whole-second value regardless of
        // digit count -- the return unit stays whole seconds.
        assert_eq!(
            parse_utc_seconds("2026-07-03T00:00:00.123456Z"),
            Some(whole)
        );
        assert_eq!(parse_utc_seconds("2026-07-03T00:00:00.1Z"), Some(whole));
        assert_eq!(
            parse_utc_seconds("2026-07-03T00:00:00.000000001Z"),
            Some(whole)
        );
        // Still rejects shapes that only superficially resemble the
        // fractional form.
        assert_eq!(parse_utc_seconds("2026-07-03T00:00:00.Z"), None); // no digits
        assert_eq!(parse_utc_seconds("2026-07-03T00:00:00.123456"), None); // no trailing Z
        assert_eq!(parse_utc_seconds("2026-07-03T00:00:00.12x456Z"), None); // non-digit
        assert_eq!(parse_utc_seconds("2026-07-03T00:00:00,123456Z"), None); // wrong separator
    }

    // R13-3: a live log whose last-written day differs from "today" is rotated to
    // a dated name and the fixed name starts fresh (logrotate style); dated files
    // older than retention_days are pruned.
    #[test]
    fn r13_3_rotate_renames_stale_log_and_prune_drops_old_dated_files() {
        let (_fixture, dir) = private_log_fixture();
        let log = dir.path().join("events.jsonl");
        dir.append_private(std::path::Path::new("events.jsonl"), b"line1\n")
            .unwrap(); // mtime = real today

        // A far-future "today" makes the live file stale → rotate it away.
        rotate_stale_log(&log, "2099-01-02").unwrap();
        assert!(!log.exists(), "the stale live file must be renamed away");
        let dated: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("events-") && name.ends_with(".jsonl"))
            .collect();
        assert_eq!(dated.len(), 1, "exactly one dated file created: {dated:?}");

        // Prune: a dated file older than retention_days goes; a recent one stays.
        dir.append_private(std::path::Path::new("events-2000-01-01.jsonl"), b"old\n")
            .unwrap();
        dir.append_private(std::path::Path::new("events-2099-01-01.jsonl"), b"recent\n")
            .unwrap();
        prune_rotated_logs(&log, "2099-01-02", 30).unwrap();
        assert!(
            !dir.path().join("events-2000-01-01.jsonl").exists(),
            "a dated file older than retention_days must be pruned"
        );
        assert!(
            dir.path().join("events-2099-01-01.jsonl").exists(),
            "a dated file within retention_days must be kept"
        );
    }

    fn private_log_fixture() -> (tempfile::TempDir, crate::store_dir::StoreDirectory) {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        let external = crate::store_dir::StoreDirectory::open(&root).unwrap();
        let directory = crate::store_dir::StoreDirectory::from_retained(
            external
                .create_directory(std::path::Path::new("logs"))
                .unwrap(),
            root.join("logs"),
        )
        .unwrap();
        (fixture, directory)
    }

    #[test]
    fn r13_3_append_rotating_keeps_same_day_writes_in_the_live_file() {
        let (_fixture, dir) = private_log_fixture();
        let log = dir.path().join("events.jsonl");
        append_jsonl_rotating(&log, &json!({ "a": 1 }), 30).unwrap();
        append_jsonl_rotating(&log, &json!({ "a": 2 }), 30).unwrap();
        let content = fs::read_to_string(&log).unwrap();
        assert_eq!(
            content.lines().count(),
            2,
            "same-day appends stay in the live fixed-name file"
        );
        let rotated = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().starts_with("events-"));
        assert!(!rotated, "no rotation may happen within the same day");
    }

    #[test]
    fn ct4_purge_log_appenders_honor_device_and_scope_scrub_locks() {
        for (file_name, lock_name) in [
            ("events.jsonl", "scrub.lock"),
            ("access.jsonl", "access.scrub.lock"),
        ] {
            let (_fixture, dir) = private_log_fixture();
            let log = dir.path().join(file_name);
            let lock = dir.path().join(lock_name);
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let holder = std::thread::spawn(move || {
                let _guard = StoreLock::acquire_path(lock).unwrap();
                ready_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            });
            ready_rx.recv().unwrap();
            let error = append_jsonl_rotating(&log, &json!({ "target": "late" }), 30).unwrap_err();
            assert_eq!(error.error_code(), "KIO-E-STORE-LOCKED-001");
            assert!(!log.exists(), "a contended post-scrub append must not land");
            release_tx.send(()).unwrap();
            holder.join().unwrap();

            append_jsonl_rotating(&log, &json!({ "target": "after" }), 30).unwrap();
            assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 1);
        }
    }

    #[test]
    fn r13_3_prune_skips_unparseable_dated_files_and_append_still_lands() {
        // R13-3 / R12-5: rotation/prune are best-effort — a garbage file that looks
        // like a dated log but has no parseable date must be skipped (not error),
        // and the append must still succeed regardless.
        let (_fixture, dir) = private_log_fixture();
        let log = dir.path().join("events.jsonl");
        fs::write(dir.path().join("events-not-a-date.jsonl"), "junk\n").unwrap();
        prune_rotated_logs(&log, "2099-01-02", 30).unwrap();
        assert!(
            dir.path().join("events-not-a-date.jsonl").exists(),
            "an unparseable dated name must be skipped, not deleted or errored"
        );
        append_jsonl_rotating(&log, &json!({ "b": 2 }), 30).unwrap();
        assert!(log.exists(), "the append must land in the live file");
    }

    #[test]
    fn read_logs_retention_days_uses_only_observability_key() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        fs::write(&cfg, "[observability]\nretention_days = 7\n").unwrap();
        assert_eq!(read_logs_retention_days(&cfg), Some(7));
        fs::write(&cfg, "[observability]\nretention_days = 0\n").unwrap();
        assert_eq!(read_logs_retention_days(&cfg), None);
        fs::write(&cfg, "[observability]\nretention_days = 3651\n").unwrap();
        assert_eq!(read_logs_retention_days(&cfg), None);
        fs::write(&cfg, "[logs]\nretention_days = 7\n").unwrap();
        assert_eq!(read_logs_retention_days(&cfg), None);
        assert_eq!(
            read_logs_retention_days(&dir.path().join("missing.toml")),
            None
        );
    }

    #[test]
    fn civil_from_days_boundaries() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(19_783), (2024, 3, 1));
        assert_eq!(civil_from_days(47_540), (2100, 2, 28));
        assert_eq!(civil_from_days(47_541), (2100, 3, 1));
        // Negative day index -> proleptic pre-epoch date.
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[cfg(unix)]
    #[test]
    fn cand_008_symlinked_kio_store_is_rejected() {
        use std::os::unix::fs::symlink;

        let victim = tempfile::tempdir().unwrap();
        let victim_repo = Repository::init(victim.path()).unwrap();
        let lure = tempfile::tempdir().unwrap();
        symlink(victim_repo.kio_dir(), lure.path().join(".kio")).unwrap();

        let error = Repository::open(lure.path()).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-UNSAFE-001");
    }

    #[cfg(unix)]
    #[test]
    fn cand_024_existing_store_requires_private_owner_mode() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let kio_dir = repo.kio_dir().to_path_buf();
        assert_eq!(
            fs::metadata(&kio_dir).unwrap().uid(),
            super::effective_uid()
        );

        fs::set_permissions(&kio_dir, fs::Permissions::from_mode(0o755)).unwrap();
        let error = Repository::open(dir.path()).unwrap_err();
        // docs06/docs14: unsafe own store uses the managed-store namespace.
        assert_eq!(error.error_code(), "KIO-E-MANAGEMENT-ROOT-UNSAFE-001");
        assert_eq!(
            fs::metadata(&kio_dir).unwrap().permissions().mode() & 0o777,
            0o755
        );

        fs::set_permissions(&kio_dir, fs::Permissions::from_mode(0o700)).unwrap();
        Repository::open(dir.path()).unwrap();
        fs::set_permissions(&kio_dir, fs::Permissions::from_mode(0o500)).unwrap();
        Repository::open(dir.path()).unwrap();
        fs::set_permissions(&kio_dir, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn cand_025_scope_identity_binds_portable_id_to_canonical_root() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let identity = repo.scope_identity().unwrap();
        assert_eq!(identity.canonical_root, dir.path().canonicalize().unwrap());
        assert!(!identity.scope_id.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn cand_018_checked_regular_replaced_by_symlink_is_never_opened() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.txt");
        fs::write(&path, b"benign").unwrap();
        let entry = fs::read_dir(dir.path()).unwrap().next().unwrap().unwrap();
        assert!(entry.file_type().unwrap().is_file());

        let original = dir.path().join("original.txt");
        let outside = dir.path().join("outside.txt");
        fs::rename(&path, &original).unwrap();
        fs::write(&outside, b"outside marker").unwrap();
        symlink(&outside, &path).unwrap();

        assert!(open_scope_file_nofollow(&path).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn cand_018_windows_scope_identity_rejects_distinct_equal_sized_files() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        fs::write(&first, b"same-size").unwrap();
        fs::write(&second, b"same-size").unwrap();
        let first_handle = fs::File::open(&first).unwrap();
        let same_handle = fs::File::open(&first).unwrap();
        let second_handle = fs::File::open(&second).unwrap();

        assert!(super::same_scope_file_identity(&first_handle, &same_handle));
        assert!(!super::same_scope_file_identity(
            &first_handle,
            &second_handle
        ));
        assert!(open_scope_file_nofollow(&first).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn cand_018_stable_symlink_remains_skipped() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let outside = dir.path().join("outside-source");
        fs::write(&outside, b"outside marker").unwrap();
        symlink(&outside, dir.path().join("linked.txt")).unwrap();
        let tree = repo.build_working_tree(false).unwrap().tree;
        assert!(tree.entries.iter().all(|entry| entry.path != "linked.txt"));
    }

    #[test]
    fn cand_019_archive_limits_accept_exact_and_reject_plus_one_and_aggregate() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let empty_paths = BTreeSet::new();
        let empty_normalize = BTreeMap::new();
        let limit = 64 * 1024;
        fs::write(dir.path().join("exact.bin"), vec![b'x'; limit]).unwrap();
        repo.build_working_tree_with_limits(
            false,
            &empty_paths,
            &empty_normalize,
            ArchiveLimits::new(limit as u64, limit as u64),
        )
        .unwrap();

        fs::write(dir.path().join("exact.bin"), vec![b'x'; limit + 1]).unwrap();
        let error = repo
            .build_working_tree_with_limits(
                false,
                &empty_paths,
                &empty_normalize,
                ArchiveLimits::new(limit as u64, (2 * limit) as u64),
            )
            .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-SCOPE-INPUT-OVERSIZED-001");

        fs::write(dir.path().join("exact.bin"), vec![b'x'; 40 * 1024]).unwrap();
        fs::write(dir.path().join("second.bin"), vec![b'y'; 40 * 1024]).unwrap();
        let error = repo
            .build_working_tree_with_limits(
                false,
                &empty_paths,
                &empty_normalize,
                ArchiveLimits::new(limit as u64, limit as u64),
            )
            .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-SCOPE-INPUT-OVERSIZED-001");
    }

    #[test]
    fn cand_019_oversized_sparse_snapshot_does_not_advance_head_or_write_raw() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let path = dir.path().join("oversized.bin");
        let file = fs::File::create(&path).unwrap();
        file.set_len(DEFAULT_MAX_ARCHIVE_FILE_BYTES + 1).unwrap();
        drop(file);
        let head_before = fs::read(repo.kio_dir().join("HEAD")).unwrap();

        let error = repo.snapshot(Some("oversized"), None).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-SCOPE-INPUT-OVERSIZED-001");
        assert_eq!(fs::read(repo.kio_dir().join("HEAD")).unwrap(), head_before);
        assert_eq!(
            fs::read_dir(repo.kio_dir().join("objects/raw"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn ct4_raw_archive_removes_crash_orphans_before_new_staging() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let orphan = repo.kio_dir().join("objects/raw/.ingest-crashed-writer");
        fs::write(&orphan, b"stale private transaction bytes").unwrap();
        fs::write(dir.path().join("doc.md"), b"new authoritative bytes").unwrap();

        let outcome = repo.snapshot(Some("cleanup and archive"), None).unwrap();
        assert!(!orphan.exists());
        let tree = repo.read_tree(&outcome.tree_hash).unwrap();
        let raw_hash = tree
            .entries
            .iter()
            .find(|entry| entry.path == "doc.md")
            .unwrap()
            .raw_hash
            .clone();
        ObjectStore::new(repo.kio_dir())
            .inspect_object(ObjectKind::Raw, &raw_hash)
            .unwrap();
    }

    #[test]
    fn ct4_raw_archive_rejects_oversized_orphan_before_publication() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let orphan = repo.kio_dir().join("objects/raw/.ingest-oversized");
        fs::File::create(&orphan)
            .unwrap()
            .set_len(crate::cas::MAX_RAW_OBJECT_BYTES + 1)
            .unwrap();
        fs::write(dir.path().join("doc.md"), b"must not publish").unwrap();
        let head_before = fs::read(repo.kio_dir().join("HEAD")).unwrap();

        let error = repo.snapshot(Some("blocked cleanup"), None).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
        assert!(orphan.exists());
        assert_eq!(fs::read(repo.kio_dir().join("HEAD")).unwrap(), head_before);
    }

    #[test]
    fn cand_041_closing_snapshot_reclassifies_new_tier_a_names() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        fs::write(dir.path().join("notes.md"), b"notes").unwrap();
        fs::write(dir.path().join(".env"), b"synthetic=value").unwrap();

        let outcome = repo
            .snapshot_filtered(Some("closing"), None, &BTreeSet::new())
            .unwrap();
        let tree = repo.read_tree(&outcome.tree_hash).unwrap();
        assert_eq!(tree.entries.len(), 1);
        assert_eq!(tree.entries[0].path, "notes.md");

        let allowed = BTreeSet::from([".env".to_owned()]);
        let outcome = repo
            .snapshot_filtered_with_policy(Some("explicit lift"), None, &BTreeSet::new(), &allowed)
            .unwrap();
        let tree = repo.read_tree(&outcome.tree_hash).unwrap();
        assert!(tree.entries.iter().any(|entry| entry.path == ".env"));
    }

    #[test]
    fn cand_042_normalize_ref_attaches_only_to_its_expected_raw_hash() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let path = dir.path().join("doc.txt");
        fs::write(&path, b"old bytes").unwrap();
        let normalize = NormalizeRef {
            tool_profile_hash: hash_bytes(b"profile"),
            r#gen: 0,
            manifest_hash: hash_bytes(b"manifest"),
        };
        let pending = BTreeMap::from([(
            "doc.txt".to_owned(),
            PendingNormalizeRef {
                expected_raw_hash: hash_bytes(b"old bytes"),
                normalize: normalize.clone(),
            },
        )]);
        let limits = ArchiveLimits::new(1024, 1024);
        let tree = repo
            .build_working_tree_with_bound_normalize_and_limits(
                false,
                &BTreeSet::new(),
                &pending,
                &BTreeSet::new(),
                limits,
            )
            .unwrap()
            .tree;
        assert_eq!(tree.entries[0].normalize, Some(normalize));

        fs::write(&path, b"new bytes").unwrap();
        let tree = repo
            .build_working_tree_with_bound_normalize_and_limits(
                false,
                &BTreeSet::new(),
                &pending,
                &BTreeSet::new(),
                limits,
            )
            .unwrap()
            .tree;
        assert_eq!(tree.entries[0].raw_hash, hash_bytes(b"new bytes"));
        assert!(tree.entries[0].normalize.is_none());
    }

    #[test]
    fn index_publication_guard_rejects_commit_and_noop_without_moving_refs() {
        for changed in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let repo = Repository::init(dir.path()).unwrap();
            fs::write(dir.path().join("doc.txt"), b"original").unwrap();
            repo.snapshot(Some("initial"), Some("2026-08-14T00:00:00Z"))
                .unwrap();
            let head = fs::read(repo.kio_dir().join("HEAD")).unwrap();
            let manifest = fs::read(repo.kio_dir().join("manifest.json")).unwrap();
            if changed {
                fs::write(dir.path().join("doc.txt"), b"changed").unwrap();
            }
            let calls = std::cell::Cell::new(0);
            let guard = || {
                calls.set(calls.get() + 1);
                Err(crate::KioError::invalid_usage(
                    "policy revoked before publication",
                ))
            };
            let error = repo
                .auto_snapshot_with_bound_normalize(
                    Some("guarded index"),
                    None,
                    &BTreeSet::new(),
                    &BTreeMap::new(),
                    &BTreeSet::new(),
                    Some(&guard),
                )
                .unwrap_err();
            assert!(error.to_string().contains("policy revoked"));
            assert_eq!(calls.get(), 1);
            assert_eq!(fs::read(repo.kio_dir().join("HEAD")).unwrap(), head);
            assert_eq!(
                fs::read(repo.kio_dir().join("manifest.json")).unwrap(),
                manifest
            );
            assert!(
                !repo
                    .kio_dir()
                    .join(super::PUBLICATION_JOURNAL_LEAF)
                    .exists()
            );
        }
    }

    #[test]
    fn scheduled_snapshot_rejects_raw_map_drift_before_cas_publication() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let path = dir.path().join("doc.txt");
        fs::write(&path, b"observed bytes").unwrap();
        repo.snapshot(Some("initial"), Some("2026-08-14T00:00:00Z"))
            .unwrap();
        let head_before = repo.head_commit_hash().unwrap();
        let expected = BTreeMap::from([("doc.txt".to_owned(), hash_bytes(b"observed bytes"))]);

        let changed = b"changed after the scheduled preview";
        fs::write(&path, changed).unwrap();
        let changed_raw = ObjectStore::new(repo.kio_dir())
            .object_path(ObjectKind::Raw, &hash_bytes(changed))
            .unwrap();
        assert!(!changed_raw.exists());

        let error = repo
            .snapshot_with_type(
                Some("scheduled test"),
                Some("2026-08-14T00:01:00Z"),
                CommitType::Auto,
                &BTreeSet::new(),
                &BTreeMap::new(),
                &BTreeSet::new(),
                false,
                &[],
                true,
                Some(&expected),
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-SNAPSHOT-AUTHORITY-CHANGED-001");
        assert_eq!(repo.head_commit_hash().unwrap(), head_before);
        assert!(!changed_raw.exists());
        assert!(
            fs::read_dir(repo.kio_dir().join("objects/raw"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".ingest-"))
        );
    }

    #[test]
    fn cand_043_poisoned_raw_slot_prevents_snapshot_publication() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        fs::write(dir.path().join("doc.txt"), b"expected bytes").unwrap();
        let hash = hash_bytes(b"expected bytes");
        let store = ObjectStore::new(repo.kio_dir());
        let object_path = store.object_path(ObjectKind::Raw, &hash).unwrap();
        store.write_raw(b"expected bytes").unwrap();
        fs::write(&object_path, b"poisoned").unwrap();
        let head_before = fs::read(repo.kio_dir().join("HEAD")).unwrap();

        let error = repo.snapshot(Some("poisoned"), None).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
        assert_eq!(fs::read(repo.kio_dir().join("HEAD")).unwrap(), head_before);
    }

    #[test]
    fn cand_036_persisted_dag_objects_are_semantically_validated() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let store = ObjectStore::new(repo.kio_dir());
        let invalid_tree = json!({
            "entries": [{"path":"sub/file", "type":"file", "raw_hash":hash_bytes(b"raw")}],
            "object_type":"tree"
        });
        let (hash, _) = store.write_json(ObjectKind::Tree, &invalid_tree).unwrap();
        assert!(repo.read_tree(&hash).is_err());
    }

    #[test]
    fn cand_046_dag_cardinality_limits_apply_after_bounded_deserialization() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let store = ObjectStore::new(repo.kio_dir());
        let commit = json!({
            "commit_type":"manual",
            "created_at":"2026-07-12T00:00:00Z",
            "message":"bounded",
            "object_type":"commit",
            "parents":[hash_bytes(b"parent")],
            "stats":{"files_added":0,"files_modified":0,"files_deleted":0},
            "tool_lock_hash":hash_bytes(b"tool"),
            "tree":hash_bytes(b"tree")
        });
        let (commit_hash, _) = store.write_json(ObjectKind::Commit, &commit).unwrap();
        assert!(repo.read_commit(&commit_hash).is_err());

        let entry = json!({"path":"a", "type":"file", "raw_hash":hash_bytes(b"raw")});
        let tree = json!({
            "entries":vec![entry; MAX_TREE_ENTRIES + 1],
            "object_type":"tree"
        });
        let (tree_hash, _) = store.write_json(ObjectKind::Tree, &tree).unwrap();
        assert!(repo.read_tree(&tree_hash).is_err());
    }

    #[test]
    fn cand_046_snapshot_rejects_tree_entry_overflow_before_publication() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        for index in 0..=MAX_TREE_ENTRIES {
            fs::File::create(dir.path().join(format!("entry-{index:05}.txt"))).unwrap();
        }

        let head_before = fs::read(repo.kio_dir().join("HEAD")).unwrap();
        let error = repo.snapshot(Some("over-limit"), None).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-SCOPE-INPUT-OVERSIZED-001");
        assert_eq!(
            error.context()["observed_entries"],
            json!(MAX_TREE_ENTRIES + 1)
        );
        assert_eq!(error.context()["max_tree_entries"], json!(MAX_TREE_ENTRIES));
        assert_eq!(fs::read(repo.kio_dir().join("HEAD")).unwrap(), head_before);
        for kind in ["raw", "trees", "commits"] {
            assert_eq!(
                fs::read_dir(repo.kio_dir().join("objects").join(kind))
                    .unwrap()
                    .count(),
                0,
                "over-limit snapshot published a {kind} CAS object"
            );
        }

        fs::remove_file(
            dir.path()
                .join(format!("entry-{:05}.txt", MAX_TREE_ENTRIES)),
        )
        .unwrap();
        let boundary = repo.build_working_tree(false).unwrap();
        assert_eq!(boundary.tree.entries.len(), MAX_TREE_ENTRIES);
    }

    #[test]
    fn publication_journal_recovers_only_the_expected_head_child() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        fs::write(dir.path().join("doc.txt"), "v1").unwrap();
        let c1_hash = repo
            .snapshot(Some("c1"), None)
            .unwrap()
            .commit_hash
            .unwrap();
        fs::write(dir.path().join("doc.txt"), "v2").unwrap();
        let c2_hash = repo
            .snapshot(Some("c2"), None)
            .unwrap()
            .commit_hash
            .unwrap();
        let manifest: Value =
            serde_json::from_slice(&fs::read(repo.kio_dir().join("manifest.json")).unwrap())
                .unwrap();
        let journal = super::PublicationJournal {
            version: 1,
            expected_head: Some(c1_hash.clone()),
            new_head: c2_hash.clone(),
            manifest: manifest.clone(),
        };
        fs::write(
            repo.kio_dir().join(super::PUBLICATION_JOURNAL_LEAF),
            canonical_json_bytes(&serde_json::to_value(journal).unwrap()).unwrap(),
        )
        .unwrap();
        fs::write(repo.kio_dir().join("HEAD"), format!("{c1_hash}\n")).unwrap();
        assert_eq!(
            Repository::open_read_only(dir.path())
                .unwrap_err()
                .error_code(),
            "KIO-E-PUBLICATION-RECOVERY-REQUIRED-001"
        );
        let recover = Repository::open_for_recovery(dir.path()).unwrap();
        let _lock = recover
            .lock_store_in_session(recover.lock_session())
            .unwrap();
        assert!(recover.recover_publication().unwrap());
        assert_eq!(recover.head_commit_hash().unwrap(), Some(c2_hash.clone()));
        assert!(
            !repo
                .kio_dir()
                .join(super::PUBLICATION_JOURNAL_LEAF)
                .exists()
        );

        fs::write(dir.path().join("doc.txt"), "v3").unwrap();
        let c3_hash = recover
            .snapshot(Some("c3"), None)
            .unwrap()
            .commit_hash
            .unwrap();
        let conflicting = super::PublicationJournal {
            version: 1,
            expected_head: Some(c1_hash),
            new_head: c2_hash,
            manifest,
        };
        fs::write(
            repo.kio_dir().join(super::PUBLICATION_JOURNAL_LEAF),
            canonical_json_bytes(&serde_json::to_value(conflicting).unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            recover.recover_publication().unwrap_err().error_code(),
            "KIO-E-PUBLICATION-CONFLICT-001"
        );
        assert_eq!(recover.head_commit_hash().unwrap(), Some(c3_hash));
    }

    #[test]
    fn purge_snapshot_forces_protected_child_when_tree_is_unchanged() {
        // P2-A / journal-barrier fix: purging a raw_hash that HEAD's tree no
        // longer references at all (superseded by a later edit) is the
        // "tree genuinely unchanged" case — `doc.txt` currently holds
        // "version two", so the working-tree scan never stages a "version
        // one" candidate and `purge_self_targets`' exclusion has nothing to
        // do. This is also the realistic shape of this scenario: purging an
        // old historical version while today's content is untouched.
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        fs::write(dir.path().join("doc.txt"), b"version one").unwrap();
        repo.snapshot(Some("v1"), Some("2026-07-11T00:00:00Z"))
            .unwrap();
        fs::write(dir.path().join("doc.txt"), b"version two").unwrap();
        let parent = repo
            .snapshot(Some("v2"), Some("2026-07-12T00:00:00Z"))
            .unwrap()
            .commit_hash
            .unwrap();
        let parent_tree = repo.read_commit(&parent).unwrap().tree;

        let outcome = repo
            .purged_snapshot(
                "legal",
                Some("2026-07-13T00:00:00Z"),
                &[hash_bytes(b"version one")],
                true,
            )
            .unwrap();
        assert!(!outcome.noop);
        let commit_hash = outcome.commit_hash.unwrap();
        let commit = repo.read_commit(&commit_hash).unwrap();
        assert_eq!(commit.commit_type, CommitType::Purged);
        assert_eq!(commit.message, "legal");
        assert_eq!(commit.parent, Some(parent));
        assert_eq!(commit.tree, parent_tree);
        assert_eq!(commit.created_at, "2026-07-13T00:00:00Z");
        assert_eq!(commit.purged_raws, vec![hash_bytes(b"version one")]);
    }

    /// P2-A finding fix (05 §3.5 L741; `archive_staged_working_tree`'s doc
    /// comment): purge's own final `publish_ref=true` snapshot must succeed
    /// — and its tree must NOT carry the purged entry — even while the
    /// purge target's exact bytes are still physically present in the
    /// working tree (Kio never deletes the working-tree original). Before
    /// the fix, this raw publication attempt hit `archive_staged_working_tree`'s
    /// barrier check (an active journal targeting this same raw_hash) and the
    /// whole snapshot failed with `KIO-E-PURGE-INCOMPLETE-001` — reachable any
    /// time a real purge orchestration republishes its planned commit after
    /// the tombstone/journal phase has advanced past `prepared`, i.e. every
    /// ordinary purge whose target's working-tree residual was never deleted.
    #[test]
    fn purge_snapshot_succeeds_and_excludes_the_entry_while_journal_barrier_is_active() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        fs::write(dir.path().join("keep.txt"), b"keep").unwrap();
        fs::write(dir.path().join("residual.txt"), b"purge target bytes").unwrap();
        let raw_hash = hash_bytes(b"purge target bytes");
        repo.snapshot(Some("initial"), Some("2026-07-12T00:00:00Z"))
            .unwrap();

        // Simulate the real orchestration's post-`prepared` state: an active
        // purge journal whose barrier now covers this raw_hash (05 §3.5's
        // `tombstoned`/`deleted` phases), WITHOUT deleting the working-tree
        // original (purge never does — this is the residual).
        let purge = PurgeState::open(repo.kio_dir()).unwrap();
        let closure = crate::purge::PurgeClosure::new(
            "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned(),
            vec![crate::purge::ClosureItem {
                object_type: "raw".to_owned(),
                hash: raw_hash.clone(),
            }],
            Vec::new(),
        )
        .unwrap();
        purge.write_closure(&closure).unwrap();
        let closure_hash = crate::purge::closure_content_hash(&closure).unwrap();
        let (journal, _) = match purge
            .begin(
                vec![raw_hash.clone()],
                crate::purge::PurgeReason::Legal,
                crate::purge::TombstoneMode::Default,
                "test-actor".to_owned(),
                "2026-07-13T00:00:00Z".to_owned(),
                1,
                // `planned_commit` only needs to be a well-formed hash here —
                // this test does not exercise `publish_planned_commit`'s
                // cross-check against it (that belongs to the CLI-level
                // orchestration tests).
                hash_bytes(b"planned"),
                closure_hash,
                "01ARZ3NDEKTSV4RRFFQ69G5FAW".to_owned(),
            )
            .unwrap()
        {
            crate::purge::BeginOutcome::Started(journal) => (journal, true),
            other => panic!("expected a fresh journal start, got {other:?}"),
        };
        let journal = purge
            .advance_phase(&journal, crate::purge::PurgePhase::Tombstoned)
            .unwrap();
        purge
            .advance_phase(&journal, crate::purge::PurgePhase::Deleted)
            .unwrap();
        assert!(purge.barrier_blocks(&raw_hash).unwrap());

        let outcome = repo
            .purged_snapshot(
                "legal",
                Some("2026-07-13T00:00:00Z"),
                std::slice::from_ref(&raw_hash),
                true,
            )
            .expect("purge's own snapshot must not trip its own journal barrier");
        let commit = outcome.commit.unwrap();
        assert_eq!(commit.commit_type, CommitType::Purged);
        let tree = repo.read_tree(&commit.tree).unwrap();
        assert_eq!(
            tree.entries
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            vec!["keep.txt"],
            "the purge target's entry must be excluded even though its working-tree \
             bytes are still physically present"
        );
        // The residual original is untouched — 05 §3.5's "working tree の
        // 原本には触れない" — re-ingestion is deferred to the next `kio index`.
        assert_eq!(
            fs::read(dir.path().join("residual.txt")).unwrap(),
            b"purge target bytes"
        );
    }

    #[test]
    fn lc48_purged_snapshot_dry_run_computes_planned_commit_without_publishing_ref() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        fs::write(dir.path().join("doc.txt"), b"retained").unwrap();
        let parent = repo
            .snapshot(Some("initial"), Some("2026-07-12T00:00:00Z"))
            .unwrap()
            .commit_hash
            .unwrap();

        let dry = repo
            .purged_snapshot(
                "legal",
                Some("2026-07-13T00:00:00Z"),
                &[hash_bytes(b"retained")],
                false,
            )
            .unwrap();
        let planned_commit = dry.commit_hash.clone().unwrap();
        // The commit object is durably content-addressed (readable by hash)...
        let commit = repo.read_commit(&planned_commit).unwrap();
        assert_eq!(commit.commit_type, CommitType::Purged);
        // ...but HEAD/refs were not advanced onto it.
        assert_eq!(repo.head_commit_hash().unwrap(), Some(parent.clone()));

        // Publishing for real re-derives the identical hash (deterministic,
        // content-addressed) and advances HEAD.
        let real = repo
            .purged_snapshot(
                "legal",
                Some("2026-07-13T00:00:00Z"),
                &[hash_bytes(b"retained")],
                true,
            )
            .unwrap();
        assert_eq!(real.commit_hash, Some(planned_commit.clone()));
        assert_eq!(repo.head_commit_hash().unwrap(), Some(planned_commit));
    }

    #[test]
    fn purge_snapshot_captures_the_current_working_tree() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        fs::write(dir.path().join("keep.txt"), b"keep").unwrap();
        fs::write(dir.path().join("removed.txt"), b"remove before purge").unwrap();
        let parent = repo
            .snapshot(Some("initial"), Some("2026-07-12T00:00:00Z"))
            .unwrap()
            .commit_hash
            .unwrap();
        fs::remove_file(dir.path().join("removed.txt")).unwrap();

        let outcome = repo
            .purged_snapshot(
                "privacy",
                Some("2026-07-13T00:00:00Z"),
                &[hash_bytes(b"remove before purge")],
                true,
            )
            .unwrap();
        let commit = outcome.commit.unwrap();
        let tree = repo.read_tree(&commit.tree).unwrap();
        assert_eq!(commit.commit_type, CommitType::Purged);
        assert_eq!(commit.parent, Some(parent));
        assert_eq!(outcome.stats.files_deleted, 1);
        assert_eq!(
            tree.entries
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            vec!["keep.txt"]
        );
    }

    #[test]
    fn r23_29_log_from_stops_before_exceeding_the_first_parent_commit_cap() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        for index in 0..4 {
            fs::write(dir.path().join("file.txt"), format!("v{index}")).unwrap();
            repo.snapshot(Some(&format!("commit {index}")), None)
                .unwrap();
        }

        let report = repo.log_from_with_limit(None, 1_000).unwrap();
        assert!(!report.truncated);
        let total = report.entries.len() as u64;
        assert!(total >= 4);

        // One below the actual first-parent chain length: R23-29 stops
        // BEFORE crossing (05-runtime.md §1.6 L313's "次の object/entry で
        // 1 つでも超える前に停止する"), never returning a partial log.
        let error = repo.log_from_with_limit(None, total - 1).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-COMMIT-HISTORY-LIMIT-001");
        assert_eq!(error.context()["exceeded"], json!("commits"));

        // Exactly at the true chain length still succeeds.
        let exact = repo.log_from_with_limit(None, total).unwrap();
        assert_eq!(exact.entries.len() as u64, total);
        assert!(!exact.truncated);

        // The public `log_from`/`log` entry points thread the real default
        // (100,000 -- `history::DEFAULT_MAX_HISTORY_COMMITS`), confirmed not
        // to trip for this tiny chain.
        assert!(!repo.log().unwrap().truncated);
    }
    #[test]
    fn adapter_lane_reads_only_the_two_declared_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // Absent key / absent file → the caller keeps its own default.
        assert_eq!(read_adapter_lane(&path), None);
        std::fs::write(&path, "[chunking]\nstrategy = \"heading\"\n").unwrap();
        assert_eq!(read_adapter_lane(&path), None);
        // Both declared values round-trip.
        std::fs::write(&path, "[adapter]\nlane = \"batch\"\n").unwrap();
        assert_eq!(read_adapter_lane(&path).as_deref(), Some("batch"));
        std::fs::write(&path, "[adapter]\nlane = \"realtime\"\n").unwrap();
        assert_eq!(read_adapter_lane(&path).as_deref(), Some("realtime"));
        // Anything else is ignored here; the JSON schema is what rejects it
        // loudly at startup, and this defensive read must not invent a lane.
        std::fs::write(&path, "[adapter]\nlane = \"sometimes\"\n").unwrap();
        assert_eq!(read_adapter_lane(&path), None);
        std::fs::write(&path, "[adapter]\nlane = 3\n").unwrap();
        assert_eq!(read_adapter_lane(&path), None);
    }

    #[test]
    fn tool_lock_canonicalization_rejects_unknown_inner_fields() {
        let value = serde_json::json!({
            "spec_version": 1,
            "markdown": {
                "tool_id": "mistral_ocr_markdownize",
                "profile_hash": "sha256:test",
                "auth": "plain:shared-secret"
            }
        });
        let error = super::canonical_tool_lock_value(&value).unwrap_err();
        assert!(error.to_string().contains("markdown.auth"));
    }

    #[test]
    fn unchanged_snapshot_does_not_repair_or_publish_a_missing_tool_lock() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        fs::write(dir.path().join("doc.txt"), b"stable").unwrap();
        let first = repo
            .snapshot(Some("initial"), Some("2026-08-20T00:00:00Z"))
            .unwrap();
        let commit = first.commit.unwrap();
        let tool_lock_path = ObjectStore::new(repo.kio_dir())
            .content_path(ContentObjectKind::Toollock, &commit.tool_lock_hash)
            .unwrap();
        fs::remove_file(&tool_lock_path).unwrap();

        let second = repo
            .snapshot(Some("unchanged"), Some("2026-08-20T00:00:01Z"))
            .unwrap();
        assert!(second.noop);
        assert!(!tool_lock_path.exists());
    }
}
