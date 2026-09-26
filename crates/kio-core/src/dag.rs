//! Snapshot DAG object types.

use std::fmt;
use std::path::{Component, Path};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;

use crate::cas::{hash_json, is_hash};
use crate::error::{KioError, Result};

pub const MAX_TREE_ENTRIES: usize = 10_000;
/// The only shipped chunking strategy.  Persisted trees bind its exact
/// configuration hash so a historical publication cannot be interpreted using
/// a later mutable `config.toml` value.
pub const DEFAULT_CHUNKING_STRATEGY: &str = "heading";
pub const DEFAULT_CHUNKING_MAX_CHARS: u64 = 6_000;

/// Hash the canonical current chunking configuration.  This lives in core
/// because both tree construction and index chunking must name exactly the
/// same immutable configuration object.
pub fn chunking_config_hash(strategy: &str, max_chars: u64) -> Result<String> {
    if strategy != DEFAULT_CHUNKING_STRATEGY {
        return Err(KioError::schema(format!(
            "unsupported chunking strategy: {strategy}"
        )));
    }
    if max_chars == 0 {
        return Err(KioError::schema(
            "chunking max_chars must be greater than zero",
        ));
    }
    hash_json(&json!({
        "max_chars": max_chars,
        "spec_version": 1,
        "strategy": strategy,
    }))
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NormalizeRef {
    pub tool_profile_hash: String,
    /// Generation is part of the normalized-instance identity. It is required
    /// whenever `normalize` is present; raw-only entries omit `normalize`.
    pub r#gen: u64,
    /// PB04 (step4b-contract-tests-p2b.md §B; 03-data-model.md §8, tree
    /// schema v2): content hash of this (raw_hash, tool_profile_hash,
    /// gen)'s normalized-instance manifest.json canonical JCS bytes
    /// (`objects/manifests/` — 03-data-model.md §2.1). Required: an entry
    /// that cannot name its manifest cannot be resolved point-in-time, so
    /// writing one is a failure, not a degraded record to read later.
    pub manifest_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeEntry {
    pub path: String,
    #[serde(rename = "type")]
    pub entry_type: String,
    pub raw_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub normalize: Option<NormalizeRef>,
}

impl TreeEntry {
    pub fn raw_file(path: impl Into<String>, raw_hash: impl Into<String>) -> Result<Self> {
        let entry = Self {
            path: path.into(),
            entry_type: "file".to_owned(),
            raw_hash: raw_hash.into(),
            normalize: None,
        };
        entry.validate_materialization_path()?;
        Ok(entry)
    }

    pub fn validate(&self) -> Result<()> {
        // Persisted tree paths are logical names. Validate their immutable schema
        // independently of the host that happens to read the history: a Unix tree
        // containing `:` / `?` / `CON` must remain inspectable on Windows. Any
        // operation that materializes a logical name (new snapshot / restore) must
        // additionally apply the destination platform rule before constructing a
        // physical path.
        if !is_logical_direct_child(&self.path) {
            return Err(KioError::path(
                "tree entry path must be a logical direct child file name",
                self.path.clone(),
            ));
        }
        if self.entry_type != "file" {
            return Err(KioError::schema("Step 1 tree entry type must be file"));
        }
        if !is_hash(&self.raw_hash) {
            return Err(KioError::schema("raw_hash must be sha256 lowercase hex"));
        }
        if let Some(normalize) = &self.normalize {
            if !is_hash(&normalize.tool_profile_hash) {
                return Err(KioError::schema(
                    "tool_profile_hash must be sha256 lowercase hex",
                ));
            }
            // PB04: format-only check, the same one `tool_profile_hash` gets
            // above. The cross-reference (does this hash resolve to a real
            // manifest object whose identity fields agree with this entry?) is
            // a `kio repair verify-objects` corpus-shaped check (PB04's CAS
            // re-hash comparison), not a per-entry schema invariant.
            if !is_hash(&normalize.manifest_hash) {
                return Err(KioError::schema(
                    "manifest_hash must be sha256 lowercase hex",
                ));
            }
        }
        Ok(())
    }

    /// Validate this logical entry before using `path` as a physical leaf on the
    /// current host. Historical reads use [`Self::validate`] only; new snapshots
    /// and restore destinations must call this stricter boundary before joining.
    pub fn validate_materialization_path(&self) -> Result<()> {
        self.validate()?;
        if !is_platform_safe_direct_child(&self.path) {
            return Err(KioError::path(
                "tree entry path is not safe on the current scope filesystem",
                self.path.clone(),
            ));
        }
        Ok(())
    }
}

/// Preserve names valid on the current scope filesystem while rejecting every
/// component that can escape a direct child on that platform. In particular,
/// `:` and `\` remain legitimate Unix filename bytes, but are path/ADS syntax on
/// Windows and are rejected there.
fn is_logical_direct_child(path: &str) -> bool {
    if path.is_empty() || path == "." || path == ".." || path.contains('/') || path.contains('\0') {
        return false;
    }

    true
}

fn is_platform_safe_direct_child(path: &str) -> bool {
    if !is_logical_direct_child(path) || Path::new(path).is_absolute() {
        return false;
    }

    #[cfg(windows)]
    if crate::portable::portable_leaf_error(path).is_some() {
        return false;
    }

    let mut components = Path::new(path).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

/// Whether a persisted logical tree path can safely become a direct physical
/// child on the current host. Read-only callers use this to avoid interpreting a
/// Unix-only historical name as Windows path syntax.
#[must_use]
pub fn is_materializable_direct_child(path: &str) -> bool {
    is_platform_safe_direct_child(path)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeObject {
    /// Hash of the exact chunking configuration effective while this tree was
    /// constructed. Required with no legacy deserialization default: a tree
    /// without this binding cannot authoritatively select chunk publications.
    pub chunking_config_hash: String,
    pub entries: Vec<TreeEntry>,
    pub object_type: String,
}

impl TreeObject {
    /// Validate the semantic invariants that serde alone cannot enforce.
    ///
    /// Callers that deserialize persisted tree objects must invoke this before
    /// using any entry fields. Construction through [`build_tree`] applies the
    /// same validation after canonical sorting.
    pub fn validate(&self) -> Result<()> {
        if self.object_type != "tree" {
            return Err(KioError::schema("tree object_type must be tree"));
        }
        if !is_hash(&self.chunking_config_hash) {
            return Err(KioError::schema(
                "tree chunking_config_hash must be sha256 lowercase hex",
            ));
        }

        for entry in &self.entries {
            entry.validate()?;
        }

        for pair in self.entries.windows(2) {
            match pair[0].path.as_bytes().cmp(pair[1].path.as_bytes()) {
                std::cmp::Ordering::Equal => {
                    return Err(KioError::duplicate_path(pair[0].path.clone()));
                }
                std::cmp::Ordering::Greater => {
                    return Err(KioError::schema(
                        "tree entries must be sorted by path UTF-8 bytes",
                    ));
                }
                std::cmp::Ordering::Less => {}
            }
        }

        Ok(())
    }
}

pub fn build_tree(entries: Vec<TreeEntry>) -> Result<TreeObject> {
    let config_hash = chunking_config_hash(DEFAULT_CHUNKING_STRATEGY, DEFAULT_CHUNKING_MAX_CHARS)?;
    build_tree_with_chunking_config(entries, config_hash)
}

/// Construct a tree that pins a caller-validated chunking configuration hash.
/// The builder validates it before serialization so no mutable configuration
/// lookup can be substituted after the tree has been constructed.
pub fn build_tree_with_chunking_config(
    mut entries: Vec<TreeEntry>,
    chunking_config_hash: String,
) -> Result<TreeObject> {
    for entry in &entries {
        entry.validate_materialization_path()?;
    }
    entries.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    let tree = TreeObject {
        chunking_config_hash,
        entries,
        object_type: "tree".to_owned(),
    };
    tree.validate()?;
    Ok(tree)
}

pub fn tree_hash(tree: &TreeObject) -> Result<String> {
    hash_json(&serde_json::to_value(tree).map_err(|err| KioError::schema(err.to_string()))?)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitStats {
    pub files_added: u64,
    pub files_modified: u64,
    pub files_deleted: u64,
}

/// Immutable evidence for a managed working-tree restore.  A restored commit
/// remains an ordinary single-parent child of the current HEAD; this field
/// records which reachable historical commit supplied the selected paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreProvenance {
    pub source_commit: String,
    /// Strictly byte-sorted, unique direct-child names whose historical state
    /// was materialized into the restored tree.
    pub paths: Vec<String>,
}

impl RestoreProvenance {
    pub fn validate(&self) -> Result<()> {
        if !is_hash(&self.source_commit) {
            return Err(KioError::schema(
                "restore provenance source_commit must be a sha256 lowercase hex",
            ));
        }
        if self.paths.is_empty() {
            return Err(KioError::schema(
                "restore provenance paths must not be empty",
            ));
        }
        let mut previous: Option<&str> = None;
        for path in &self.paths {
            // Provenance is immutable logical history and must remain readable
            // on a different host.  Materialization applies the stricter
            // platform-specific path rule later.
            if !is_logical_direct_child(path) || path.starts_with(".kio") {
                return Err(KioError::path(
                    "restore provenance path must be a user logical direct child outside .kio",
                    path.clone(),
                ));
            }
            if previous.is_some_and(|value| value >= path.as_str()) {
                return Err(KioError::schema(
                    "restore provenance paths must be strictly sorted ascending",
                ));
            }
            previous = Some(path);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommitObject {
    pub commit_type: CommitType,
    pub created_at: String,
    pub message: String,
    pub object_type: String,
    /// The one immutable predecessor in Kio's linear history. This field is
    /// required in persisted JSON and is `null` only for genesis.
    pub parent: Option<String>,
    pub stats: CommitStats,
    pub tool_lock_hash: String,
    pub tree: String,
    /// Required, non-empty, strictly-sorted-ascending raw_hash list when
    /// `commit_type == Purged`; absent (never serialized) for every other
    /// commit type (03 §8 L705). Marker validity (10 §7.5.1) cross-references a
    /// tombstone/erase-receipt event's raw_hash against this field so a purge
    /// commit borrowed as `in_commit` for an unrelated raw cannot mask a
    /// genuine missing object.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub purged_raws: Vec<String>,
    /// Required only for `commit_type=restored`; this is deliberately absent
    /// from every other commit type so a generic snapshot cannot claim restore
    /// provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_provenance: Option<RestoreProvenance>,
}

impl<'de> Deserialize<'de> for CommitObject {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            commit_type: CommitType,
            created_at: String,
            message: String,
            object_type: String,
            // `Value` deliberately makes absence a deserialization error while
            // still preserving the required `null | string` representation.
            parent: serde_json::Value,
            stats: CommitStats,
            tool_lock_hash: String,
            tree: String,
            #[serde(default)]
            purged_raws: Vec<String>,
            #[serde(default)]
            restore_provenance: Option<RestoreProvenance>,
        }

        let wire = Wire::deserialize(deserializer)?;
        let parent = match wire.parent {
            serde_json::Value::Null => None,
            serde_json::Value::String(value) => Some(value),
            _ => {
                return Err(serde::de::Error::custom(
                    "parent must be null or a sha256 hash string",
                ));
            }
        };
        Ok(Self {
            commit_type: wire.commit_type,
            created_at: wire.created_at,
            message: wire.message,
            object_type: wire.object_type,
            parent,
            stats: wire.stats,
            tool_lock_hash: wire.tool_lock_hash,
            tree: wire.tree,
            purged_raws: wire.purged_raws,
            restore_provenance: wire.restore_provenance,
        })
    }
}

impl CommitObject {
    pub fn new(
        tree: String,
        parent: Option<String>,
        created_at: String,
        message: String,
        tool_lock_hash: String,
        stats: CommitStats,
        commit_type: CommitType,
    ) -> Result<Self> {
        let commit = Self {
            commit_type,
            created_at,
            message,
            object_type: "commit".to_owned(),
            parent,
            stats,
            tool_lock_hash,
            tree,
            purged_raws: Vec::new(),
            restore_provenance: None,
        };
        commit.validate()?;
        Ok(commit)
    }

    /// Construct a `commit_type=purged` commit. `purged_raws` is the purge's
    /// target raw_hash set; it is sorted and deduplicated here so callers never
    /// need to pre-sort (05-runtime.md §3.5's journal already keeps the target
    /// list sorted, but this constructor does not trust that invariant blindly).
    pub fn new_purged(
        tree: String,
        parent: Option<String>,
        created_at: String,
        message: String,
        tool_lock_hash: String,
        stats: CommitStats,
        mut purged_raws: Vec<String>,
    ) -> Result<Self> {
        purged_raws.sort();
        purged_raws.dedup();
        let commit = Self {
            commit_type: CommitType::Purged,
            created_at,
            message,
            object_type: "commit".to_owned(),
            parent,
            stats,
            tool_lock_hash,
            tree,
            purged_raws,
            restore_provenance: None,
        };
        commit.validate()?;
        Ok(commit)
    }

    /// Construct a linear child that records the immutable historical source
    /// of a managed working-tree restore.
    pub fn new_restored(
        tree: String,
        parent: String,
        created_at: String,
        message: String,
        tool_lock_hash: String,
        stats: CommitStats,
        mut provenance: RestoreProvenance,
    ) -> Result<Self> {
        provenance
            .paths
            .sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        provenance.paths.dedup();
        let commit = Self {
            commit_type: CommitType::Restored,
            created_at,
            message,
            object_type: "commit".to_owned(),
            parent: Some(parent),
            stats,
            tool_lock_hash,
            tree,
            purged_raws: Vec::new(),
            restore_provenance: Some(provenance),
        };
        commit.validate()?;
        Ok(commit)
    }

    /// Validate the semantic invariants that serde alone cannot enforce.
    ///
    /// This is intentionally the same validation path used by [`Self::new`]
    /// so persisted commits and newly constructed commits cannot diverge.
    pub fn validate(&self) -> Result<()> {
        if self.object_type != "commit" {
            return Err(KioError::schema("commit object_type must be commit"));
        }
        if !is_hash(&self.tree) {
            return Err(KioError::schema("tree must be sha256 lowercase hex"));
        }
        if !is_hash(&self.tool_lock_hash) {
            return Err(KioError::schema(
                "tool_lock_hash must be sha256 lowercase hex",
            ));
        }
        if let Some(parent) = &self.parent
            && !is_hash(parent)
        {
            return Err(KioError::schema("parent must be sha256 lowercase hex"));
        }
        if !is_valid_created_at(&self.created_at) {
            return Err(KioError::schema(
                "created_at must be UTC ISO8601 YYYY-MM-DDTHH:MM:SSZ",
            ));
        }
        if self.commit_type == CommitType::Purged {
            if self.purged_raws.is_empty() {
                return Err(KioError::schema(
                    "commit_type=purged requires a non-empty purged_raws",
                ));
            }
            let mut previous: Option<&str> = None;
            for raw_hash in &self.purged_raws {
                if !is_hash(raw_hash) {
                    return Err(KioError::schema(
                        "purged_raws entries must be sha256 lowercase hex",
                    ));
                }
                if previous.is_some_and(|value| value >= raw_hash.as_str()) {
                    return Err(KioError::schema(
                        "purged_raws must be strictly sorted ascending",
                    ));
                }
                previous = Some(raw_hash);
            }
        } else if !self.purged_raws.is_empty() {
            return Err(KioError::schema(
                "purged_raws is only valid on a commit_type=purged commit",
            ));
        }
        if self.commit_type == CommitType::Restored {
            if self.parent.is_none() {
                return Err(KioError::schema(
                    "commit_type=restored requires a non-null parent",
                ));
            }
            self.restore_provenance
                .as_ref()
                .ok_or_else(|| {
                    KioError::schema("commit_type=restored requires restore_provenance")
                })?
                .validate()?;
        } else if self.restore_provenance.is_some() {
            return Err(KioError::schema(
                "restore_provenance is only valid on a commit_type=restored commit",
            ));
        }
        Ok(())
    }
}

pub fn commit_hash(commit: &CommitObject) -> Result<String> {
    hash_json(&serde_json::to_value(commit).map_err(|err| KioError::schema(err.to_string()))?)
}

/// Strictly validate a `created_at` timestamp as `YYYY-MM-DDTHH:MM:SSZ`
/// (UTC ISO8601, `06 §11`). An optional fractional-second suffix `.NNN…` before
/// the trailing `Z` is accepted (`06 §11` permits microsecond precision). Checks
/// digit positions, separators, and calendar validity (month-aware day count
/// including leap years). Leap seconds (`:60`) are rejected — Kio only emits
/// second-precision timestamps derived from Unix time, which never produce
/// `:60`.
fn is_valid_created_at(value: &str) -> bool {
    let Some(body) = value.strip_suffix('Z') else {
        return false;
    };
    // Split an optional fractional-second part; it must be all digits.
    let datetime = match body.split_once('.') {
        Some((head, frac)) => {
            if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
                return false;
            }
            head
        }
        None => body,
    };
    let bytes = datetime.as_bytes();
    if bytes.len() != 19 {
        return false;
    }
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return false;
    }
    for &index in &[0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18] {
        if !bytes[index].is_ascii_digit() {
            return false;
        }
    }
    let field = |lo: usize, hi: usize| datetime[lo..hi].parse::<u32>().unwrap_or(u32::MAX);
    let year = field(0, 4);
    let month = field(5, 7);
    let day = field(8, 10);
    let hour = field(11, 13);
    let minute = field(14, 16);
    let second = field(17, 19);
    if !(1..=12).contains(&month) {
        return false;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let max_day = match month {
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => 31,
    };
    (1..=max_day).contains(&day) && hour <= 23 && minute <= 59 && second <= 59
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitType {
    Manual,
    Auto,
    Repaired,
    Purged,
    Restored,
}

impl FromStr for CommitType {
    type Err = KioError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "manual" => Ok(Self::Manual),
            "auto" => Ok(Self::Auto),
            "repaired" => Ok(Self::Repaired),
            "purged" => Ok(Self::Purged),
            "restored" => Ok(Self::Restored),
            _ => Err(KioError::schema("invalid commit_type")),
        }
    }
}

impl fmt::Display for CommitType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Manual => "manual",
            Self::Auto => "auto",
            Self::Repaired => "repaired",
            Self::Purged => "purged",
            Self::Restored => "restored",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcPolicy {
    None,
    Shallow,
    Full,
}

#[must_use]
pub const fn gc_policy(commit_type: CommitType) -> GcPolicy {
    match commit_type {
        CommitType::Auto | CommitType::Repaired => GcPolicy::Shallow,
        CommitType::Manual | CommitType::Purged | CommitType::Restored => GcPolicy::None,
    }
}

#[must_use]
pub const fn protected(commit_type: CommitType) -> bool {
    match commit_type {
        CommitType::Manual | CommitType::Purged | CommitType::Restored => true,
        CommitType::Auto | CommitType::Repaired => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CommitObject, CommitStats, CommitType, DEFAULT_CHUNKING_MAX_CHARS,
        DEFAULT_CHUNKING_STRATEGY, TreeEntry, TreeObject, build_tree,
        build_tree_with_chunking_config, chunking_config_hash, is_valid_created_at,
    };
    use crate::error::Result;
    use serde_json::json;

    const RAW_HASH: &str =
        "sha256:eca8de0abaf2a27a1ea57feff4f44385bcfb3485274e73ddfa7c47144f383e1e";
    const OTHER_RAW_HASH: &str =
        "sha256:9a32a740871b1dd9db1bda186dce07e8e6c60d2cd316f21683ea2bd857c16ffa";
    const TOOL_HASH: &str =
        "sha256:8a32a740871b1dd9db1bda186dce07e8e6c60d2cd316f21683ea2bd857c16ffb";
    const MANIFEST_HASH: &str =
        "sha256:05b3abf2579a5eb66403cd78be557fd860633a1fe2103c7642030defe32c657f";

    fn default_config_hash() -> String {
        chunking_config_hash(DEFAULT_CHUNKING_STRATEGY, DEFAULT_CHUNKING_MAX_CHARS).unwrap()
    }

    fn commit_with_created_at(created_at: &str) -> Result<CommitObject> {
        CommitObject::new(
            RAW_HASH.to_owned(),
            None,
            created_at.to_owned(),
            "m".to_owned(),
            TOOL_HASH.to_owned(),
            CommitStats {
                files_added: 0,
                files_modified: 0,
                files_deleted: 0,
            },
            CommitType::Manual,
        )
    }

    fn valid_entry(path: &str, raw_hash: &str) -> TreeEntry {
        TreeEntry::raw_file(path, raw_hash).unwrap()
    }

    #[test]
    fn created_at_accepts_canonical_and_fractional() {
        assert!(is_valid_created_at("2026-04-29T12:00:00Z"));
        assert!(is_valid_created_at("1970-01-01T00:00:00Z"));
        assert!(is_valid_created_at("2026-04-29T12:00:00.123456Z"));
        assert!(is_valid_created_at("2024-02-29T00:00:00Z")); // leap day
        assert!(is_valid_created_at("2000-02-29T00:00:00Z")); // %400 leap
        assert!(commit_with_created_at("2026-04-29T12:00:00Z").is_ok());
    }

    #[test]
    fn created_at_rejects_malformed() {
        for bad in [
            "2026-04-29T12:00:00",   // missing Z
            "2026-04-29 12:00:00Z",  // space instead of T
            "2026-4-29T12:00:00Z",   // single-digit month
            "2026-13-01T00:00:00Z",  // month 13
            "2026-04-32T00:00:00Z",  // day 32
            "2026-04-29T24:00:00Z",  // hour 24
            "2026-04-29T12:60:00Z",  // minute 60
            "2026-04-29T12:00:60Z",  // leap second not emitted by Kio
            "2026-02-30T00:00:00Z",  // Feb 30
            "2026-04-31T00:00:00Z",  // Apr 31
            "2023-02-29T00:00:00Z",  // non-leap Feb 29
            "2100-02-29T00:00:00Z",  // %100 non-leap Feb 29
            "2026/04/29T12:00:00Z",  // wrong separators
            "2026-04-29T12:00:00.Z", // empty fraction
            "hello Z",               // garbage ending in Z
            "",                      // empty
        ] {
            assert!(!is_valid_created_at(bad), "should reject {bad:?}");
            assert!(
                commit_with_created_at(bad).is_err(),
                "commit should reject {bad:?}"
            );
        }
    }

    #[test]
    fn restored_commit_requires_typed_sorted_provenance_and_is_protected() {
        let restored = CommitObject::new_restored(
            RAW_HASH.to_owned(),
            OTHER_RAW_HASH.to_owned(),
            "2026-04-29T12:00:00Z".to_owned(),
            "restore".to_owned(),
            TOOL_HASH.to_owned(),
            CommitStats {
                files_added: 0,
                files_modified: 1,
                files_deleted: 0,
            },
            super::RestoreProvenance {
                source_commit: RAW_HASH.to_owned(),
                paths: vec!["z.txt".to_owned(), "a.txt".to_owned(), "a.txt".to_owned()],
            },
        )
        .unwrap();
        assert_eq!(restored.commit_type, CommitType::Restored);
        assert_eq!(
            restored.restore_provenance.as_ref().unwrap().paths,
            vec!["a.txt", "z.txt"]
        );
        assert_eq!(
            super::gc_policy(CommitType::Restored),
            super::GcPolicy::None
        );
        assert!(super::protected(CommitType::Restored));

        let mut generic = commit_with_created_at("2026-04-29T12:00:00Z").unwrap();
        generic.restore_provenance = restored.restore_provenance.clone();
        assert!(generic.validate().is_err());

        let mut no_parent = restored.clone();
        no_parent.parent = None;
        assert!(no_parent.validate().is_err());
        let mut control = restored;
        control.restore_provenance.as_mut().unwrap().paths = vec![".kio-control".to_owned()];
        assert!(control.validate().is_err());
    }

    #[test]
    fn restored_provenance_remains_logical_when_a_unix_name_is_not_windows_portable() {
        let restored = CommitObject::new_restored(
            RAW_HASH.to_owned(),
            OTHER_RAW_HASH.to_owned(),
            "2026-04-29T12:00:00Z".to_owned(),
            "restore".to_owned(),
            TOOL_HASH.to_owned(),
            CommitStats {
                files_added: 0,
                files_modified: 1,
                files_deleted: 0,
            },
            super::RestoreProvenance {
                source_commit: RAW_HASH.to_owned(),
                paths: vec!["report:2026.md".to_owned()],
            },
        )
        .unwrap();
        assert!(restored.validate().is_ok());
        #[cfg(windows)]
        assert!(!super::is_materializable_direct_child("report:2026.md"));
    }

    #[test]
    fn semantic_tree_validation_accepts_portable_controls() {
        // A normalized entry names all three identity fields. Raw-only entries
        // instead omit `normalize` entirely.
        let tree: TreeObject = serde_json::from_value(json!({
            "object_type": "tree",
            "chunking_config_hash": default_config_hash(),
            "entries": [{
                "path": "notes.md",
                "type": "file",
                "raw_hash": RAW_HASH,
                "normalize": {
                    "tool_profile_hash": TOOL_HASH,
                    "gen": 0,
                    "manifest_hash": MANIFEST_HASH
                }
            }]
        }))
        .unwrap();

        assert!(tree.validate().is_ok());
        assert!(build_tree(Vec::new()).unwrap().validate().is_ok());
        assert!(
            build_tree(vec![valid_entry("raw.txt", RAW_HASH)])
                .unwrap()
                .validate()
                .is_ok()
        );

        for path in [".env", "a..b", "report final.txt", "C-report.txt"] {
            assert!(TreeEntry::raw_file(path, RAW_HASH).is_ok(), "{path:?}");
        }
        #[cfg(not(windows))]
        for path in ["report:2026.md", r"a\b.md"] {
            assert!(TreeEntry::raw_file(path, RAW_HASH).is_ok(), "{path:?}");
        }

        // Persisted Unix history is logical data and remains readable on every
        // host, even when the original name cannot be materialized on Windows.
        for path in [
            "CON",
            "AUX.txt",
            "question?.md",
            "report:2026.md",
            "trailing.",
            "trailing ",
            r"a\b.md",
        ] {
            let historical: TreeObject = serde_json::from_value(json!({
                "object_type": "tree",
                "chunking_config_hash": default_config_hash(),
                "entries": [{"path":path,"type":"file","raw_hash":RAW_HASH}]
            }))
            .unwrap();
            assert!(historical.validate().is_ok(), "{path:?}");
            #[cfg(windows)]
            assert!(!super::is_materializable_direct_child(path), "{path:?}");
        }
    }

    #[test]
    fn semantic_tree_validation_rejects_invalid_tags_hashes_and_paths() {
        let invalid_cases = [
            json!({
                "object_type": "commit",
                "chunking_config_hash": default_config_hash(),
                "entries": []
            }),
            json!({
                "object_type": "tree",
                "chunking_config_hash": default_config_hash(),
                "entries": [{"path":"notes.md","type":"directory","raw_hash":RAW_HASH}]
            }),
            json!({
                "object_type": "tree",
                "chunking_config_hash": default_config_hash(),
                "entries": [{"path":"notes.md","type":"file","raw_hash":"not-a-hash"}]
            }),
            json!({
                "object_type": "tree",
                "chunking_config_hash": default_config_hash(),
                "entries": [{
                    "path":"notes.md",
                    "type":"file",
                    "raw_hash":RAW_HASH,
                    "normalize":{"tool_profile_hash":"../../outside","gen":0,"manifest_hash":MANIFEST_HASH}
                }]
            }),
        ];

        for value in invalid_cases {
            let tree: TreeObject = serde_json::from_value(value).unwrap();
            assert_eq!(
                tree.validate().unwrap_err().error_code(),
                "KIO-E-CONFIG-SCHEMA-001"
            );
        }

        for path in [
            "",
            ".",
            "..",
            "../outside",
            "nested/file.txt",
            "/etc/passwd",
        ] {
            let tree: TreeObject = serde_json::from_value(json!({
                "object_type": "tree",
                "chunking_config_hash": default_config_hash(),
                "entries": [{"path":path,"type":"file","raw_hash":RAW_HASH}]
            }))
            .unwrap();
            assert_eq!(
                tree.validate().unwrap_err().error_code(),
                "KIO-E-STORE-PATH-001"
            );
        }

        #[cfg(windows)]
        for path in [
            r"..\outside",
            r"nested\file.txt",
            r"\windows\system.ini",
            "C:relative.txt",
            r"C:\absolute.txt",
            r"\\server\share\file.txt",
            r"\\?\C:\file.txt",
            "file.txt:stream",
        ] {
            assert!(TreeEntry::raw_file(path, RAW_HASH).is_err(), "{path:?}");
        }
    }

    #[test]
    fn semantic_tree_validation_rejects_unsorted_and_duplicate_entries() {
        let unsorted = TreeObject {
            chunking_config_hash: chunking_config_hash(
                DEFAULT_CHUNKING_STRATEGY,
                DEFAULT_CHUNKING_MAX_CHARS,
            )
            .unwrap(),
            entries: vec![
                valid_entry("z.txt", OTHER_RAW_HASH),
                valid_entry("a.txt", RAW_HASH),
            ],
            object_type: "tree".to_owned(),
        };
        assert_eq!(
            unsorted.validate().unwrap_err().error_code(),
            "KIO-E-CONFIG-SCHEMA-001"
        );

        let duplicate = TreeObject {
            chunking_config_hash: chunking_config_hash(
                DEFAULT_CHUNKING_STRATEGY,
                DEFAULT_CHUNKING_MAX_CHARS,
            )
            .unwrap(),
            entries: vec![
                valid_entry("same.txt", RAW_HASH),
                valid_entry("same.txt", OTHER_RAW_HASH),
            ],
            object_type: "tree".to_owned(),
        };
        assert_eq!(
            duplicate.validate().unwrap_err().error_code(),
            "KIO-E-STORE-DUP-001"
        );

        let sorted = build_tree(vec![
            valid_entry("z.txt", OTHER_RAW_HASH),
            valid_entry("a.txt", RAW_HASH),
        ])
        .unwrap();
        assert_eq!(sorted.entries[0].path, "a.txt");
        assert!(sorted.validate().is_ok());
    }

    #[test]
    fn tree_requires_a_canonical_chunking_configuration_binding() {
        assert!(
            serde_json::from_value::<TreeObject>(json!({
                "object_type": "tree",
                "entries": []
            }))
            .is_err()
        );

        let malformed = TreeObject {
            chunking_config_hash: "sha256:not-a-canonical-hash".to_owned(),
            entries: Vec::new(),
            object_type: "tree".to_owned(),
        };
        assert_eq!(
            malformed.validate().unwrap_err().error_code(),
            "KIO-E-CONFIG-SCHEMA-001"
        );
    }

    #[test]
    fn tree_builder_pins_distinct_configurations() {
        let default_tree = build_tree(Vec::new()).unwrap();
        assert_eq!(default_tree.chunking_config_hash, default_config_hash());

        let configured_hash = chunking_config_hash("heading", 40).unwrap();
        let configured_tree =
            build_tree_with_chunking_config(Vec::new(), configured_hash.clone()).unwrap();
        assert_eq!(configured_tree.chunking_config_hash, configured_hash);
        assert_ne!(configured_tree, default_tree);
    }

    #[test]
    fn semantic_commit_validation_rejects_invalid_tags_and_hashes() {
        let mut invalid_tag = commit_with_created_at("2026-04-29T12:00:00Z").unwrap();
        invalid_tag.object_type = "tree".to_owned();

        let mut invalid_tree = commit_with_created_at("2026-04-29T12:00:00Z").unwrap();
        invalid_tree.tree = "not-a-hash".to_owned();

        let mut invalid_tool_lock = commit_with_created_at("2026-04-29T12:00:00Z").unwrap();
        invalid_tool_lock.tool_lock_hash = "not-a-hash".to_owned();

        let mut invalid_parent = commit_with_created_at("2026-04-29T12:00:00Z").unwrap();
        invalid_parent.parent = Some("not-a-hash".to_owned());

        for commit in [invalid_tag, invalid_tree, invalid_tool_lock, invalid_parent] {
            assert_eq!(
                commit.validate().unwrap_err().error_code(),
                "KIO-E-CONFIG-SCHEMA-001"
            );
        }

        assert!(
            commit_with_created_at("2026-04-29T12:00:00.123456Z")
                .unwrap()
                .validate()
                .is_ok()
        );
    }
}
