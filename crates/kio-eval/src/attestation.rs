//! Bounded immutable-CAS attestation for evaluator evidence pointers.
//!
//! This module deliberately resolves only the caller-supplied corpus scopes.
//! An untrusted `scope_id` never becomes a filesystem path or a discovery
//! request. CAS reads walk only retained capability handles, preserving the
//! core CAS fanout, hash, and representation checks without reopening paths.

use std::{collections::HashMap, fs, io::Read, path::Path};

use cap_primitives::fs as cap_fs;
use kio_core::{
    cas::{
        MAX_CHUNK_OBJECT_BYTES, MAX_COMMIT_OBJECT_BYTES, MAX_TREE_OBJECT_BYTES,
        canonical_json_bytes, hash_bytes, is_hash,
    },
    scope::KIO_FORMAT_VERSION,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::boundary::BoundCorpus;

/// Maximum bytes verified across one evaluator invocation.
pub const MAX_POINTER_ATTESTATION_BYTES: u64 = 512 * 1024 * 1024;
/// Maximum number of returned pointers checked for one query.
pub const MAX_POINTER_ATTESTATIONS_PER_QUERY: usize = 10;
const MAX_SCOPE_RECORD_BYTES: u64 = 64 * 1024;
/// Current rerank dumps have a stricter source-render budget than generic
/// historical attestation, so reject a large chunk before JSON parsing.
const MAX_CURRENT_CHUNK_OBJECT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TREE_ENTRIES: usize = 10_000;

fn deserialize_present_heading_path<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    Vec::<String>::deserialize(deserializer).map(Some)
}

/// Evaluator-owned wire contract for a returned Evidence Pointer.
///
/// Acceptance deliberately does not deserialize the production issuer's type
/// or call its validator. This shape is frozen here so a shared bug cannot make
/// both issuance and independent acceptance agree on malformed evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PointerWire {
    pub(crate) schema_version: u64,
    pub(crate) commit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tree: Option<String>,
    pub(crate) raw_hash: String,
    pub(crate) tool_profile_hash: String,
    pub(crate) chunk_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) path_at_commit: Option<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_present_heading_path",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) heading_path: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) section_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) byte_start: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) byte_end: Option<u64>,
    pub(crate) scope_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) scope_path: Option<String>,
}

impl PointerWire {
    fn validate(&self) -> AttestationResult<()> {
        if self.schema_version != 1 {
            return Err(PointerAttestationError::new(
                "unsupported evaluator pointer schema version",
            ));
        }
        for (field, hash) in [
            ("commit", self.commit.as_str()),
            ("raw_hash", self.raw_hash.as_str()),
            ("tool_profile_hash", self.tool_profile_hash.as_str()),
            ("chunk_hash", self.chunk_hash.as_str()),
        ] {
            if !is_hash(hash) {
                return Err(PointerAttestationError::new(format!(
                    "pointer {field} is not a canonical SHA-256 hash"
                )));
            }
        }
        if self.tree.as_deref().is_some_and(|tree| !is_hash(tree)) {
            return Err(PointerAttestationError::new(
                "pointer tree is not a canonical SHA-256 hash",
            ));
        }
        if !is_ulid(&self.scope_id) {
            return Err(PointerAttestationError::new(
                "pointer scope_id is not a canonical ULID",
            ));
        }
        if self
            .path_at_commit
            .as_deref()
            .is_some_and(|path| !is_logical_direct_child(path))
        {
            return Err(PointerAttestationError::new(
                "pointer path_at_commit is not a logical direct child",
            ));
        }
        if self.heading_path.as_ref().is_some_and(|headings| {
            headings.is_empty() || headings.len() > 64 || headings.iter().any(String::is_empty)
        }) {
            return Err(PointerAttestationError::new(
                "pointer heading_path is invalid",
            ));
        }
        if self.section_id.as_ref().is_some_and(String::is_empty)
            || self.scope_path.as_ref().is_some_and(String::is_empty)
        {
            return Err(PointerAttestationError::new(
                "pointer contains an empty optional string",
            ));
        }
        match (self.byte_start, self.byte_end) {
            (None, None) => {}
            (Some(start), Some(end)) if start <= end => {}
            _ => {
                return Err(PointerAttestationError::new(
                    "pointer byte range must be an ordered complete pair",
                ));
            }
        }
        Ok(())
    }
}

pub(crate) fn parse_pointer_wire(value: &Value) -> AttestationResult<PointerWire> {
    let pointer: PointerWire = serde_json::from_value(value.clone())
        .map_err(|_| PointerAttestationError::new("result has invalid evidence_pointer"))?;
    pointer.validate()?;
    Ok(pointer)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitWire {
    commit_type: String,
    created_at: String,
    message: String,
    object_type: String,
    parent: Option<String>,
    stats: CommitStatsWire,
    tool_lock_hash: String,
    tree: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    purged_raws: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitStatsWire {
    files_added: u64,
    files_modified: u64,
    files_deleted: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TreeWire {
    chunking_config_hash: String,
    entries: Vec<TreeEntryWire>,
    object_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TreeEntryWire {
    path: String,
    #[serde(rename = "type")]
    entry_type: String,
    raw_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    normalize: Option<NormalizeWire>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NormalizeWire {
    tool_profile_hash: String,
    r#gen: u64,
    manifest_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChunkWire {
    spec_version: u64,
    raw_hash: String,
    tool_profile_hash: String,
    r#gen: u64,
    unit_key: String,
    unit_content_hash: String,
    heading_path: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    section_id: Option<String>,
    byte_start: u64,
    byte_end: u64,
    text_hash: String,
    text: String,
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[error("{message}")]
pub struct PointerAttestationError {
    message: String,
}

impl PointerAttestationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

type AttestationResult<T> = std::result::Result<T, PointerAttestationError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum AttestedObjectKind {
    Commit,
    Tree,
    Chunk,
}

impl AttestedObjectKind {
    const fn max_bytes(self) -> u64 {
        match self {
            Self::Commit => MAX_COMMIT_OBJECT_BYTES,
            Self::Tree => MAX_TREE_OBJECT_BYTES,
            Self::Chunk => MAX_CHUNK_OBJECT_BYTES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ObjectKey {
    scope_id: String,
    kind: AttestedObjectKind,
    hash: String,
}

#[derive(Debug, Clone)]
enum CachedObject {
    Commit(CommitWire),
    Tree(TreeWire),
    Failure(PointerAttestationError),
}

/// A fixed-map, byte-bounded attestor for the evaluator's historical hits.
#[derive(Debug)]
pub struct PointerAttestor {
    /// Retained capability handles, indexed only after reading each bound
    /// scope's `scope.json` through its own `.kio` handle.
    scope_kio_dirs: HashMap<String, fs::File>,
    object_cache: HashMap<ObjectKey, CachedObject>,
    verified_bytes: u64,
}

impl PointerAttestor {
    /// Bind exactly `scopes` below `corpus_dir`; no descendants are discovered.
    ///
    /// A bad scope record fails construction: callers must not silently run a
    /// history attestation with only a subset of their declared corpus.
    pub fn new(corpus_dir: &Path, scopes: &[String]) -> AttestationResult<Self> {
        let corpus = BoundCorpus::bind(corpus_dir, scopes)
            .map_err(|_| PointerAttestationError::new("invalid evaluator corpus boundary"))?;
        Self::from_bound_corpus(&corpus)
    }

    /// Construct an attestor from an already-bound corpus. Public paths are
    /// deliberately not consulted after this point.
    pub fn from_bound_corpus(corpus: &BoundCorpus) -> AttestationResult<Self> {
        let mut scope_kio_dirs = HashMap::new();
        for scope in corpus.scopes() {
            let kio = scope
                .try_clone_kio_handle()
                .map_err(|_| PointerAttestationError::new("scope capability unavailable"))?;
            let scope_id = read_scope_id(&kio)?;
            if scope_kio_dirs.insert(scope_id.clone(), kio).is_some() {
                return Err(PointerAttestationError::new(
                    "duplicate scope_id in evaluator corpus",
                ));
            }
        }
        Ok(Self {
            scope_kio_dirs,
            object_cache: HashMap::new(),
            verified_bytes: 0,
        })
    }

    #[must_use]
    pub const fn verified_bytes(&self) -> u64 {
        self.verified_bytes
    }

    /// Attest one JSON evidence-pointer value.
    pub fn attest(&mut self, value: &Value) -> AttestationResult<()> {
        self.attest_chunk(value, true).map(drop)
    }

    /// Attest a current-tree result and return its immutable chunk text.
    ///
    /// Current snapshot trees may predate normalization promotion and therefore
    /// lack a tree-level normalize reference. The pointer still has to bind a
    /// real commit/tree path and matching raw/profile Chunk CAS. Historical
    /// scoring continues to use [`Self::attest`], which requires the stronger
    /// generation link.
    pub fn attest_current_chunk_text(&mut self, value: &Value) -> AttestationResult<String> {
        let chunk = self.attest_chunk(value, false)?;
        Ok(chunk.text)
    }

    fn attest_chunk(
        &mut self,
        value: &Value,
        require_normalize: bool,
    ) -> AttestationResult<ChunkWire> {
        let pointer = parse_pointer_wire(value)?;
        let path = pointer
            .path_at_commit
            .as_deref()
            .filter(|path| !path.is_empty())
            .ok_or_else(|| PointerAttestationError::new("pointer has invalid path_at_commit"))?;

        let commit = self.read_commit(&pointer.scope_id, &pointer.commit)?;
        if !require_normalize
            && self.current_head(&pointer.scope_id)?.as_deref() != Some(&pointer.commit)
        {
            return Err(PointerAttestationError::new(
                "pointer commit is not the retained scope's current HEAD",
            ));
        }
        if pointer
            .tree
            .as_deref()
            .is_some_and(|tree| tree != commit.tree)
        {
            return Err(PointerAttestationError::new(
                "pointer tree does not match commit",
            ));
        }
        let tree = self.read_tree(&pointer.scope_id, &commit.tree)?;
        let entry = tree
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .ok_or_else(|| PointerAttestationError::new("tree does not contain pointer path"))?;
        if entry.raw_hash != pointer.raw_hash {
            return Err(PointerAttestationError::new(
                "tree path raw_hash does not match pointer",
            ));
        }
        let normalize = entry.normalize.as_ref();
        if require_normalize && normalize.is_none() {
            return Err(PointerAttestationError::new(
                "tree path has no normalized identity",
            ));
        }
        if normalize
            .is_some_and(|normalize| normalize.tool_profile_hash != pointer.tool_profile_hash)
        {
            return Err(PointerAttestationError::new(
                "tree path profile does not match pointer",
            ));
        }

        let chunk_limit = if require_normalize {
            MAX_CHUNK_OBJECT_BYTES
        } else {
            MAX_CURRENT_CHUNK_OBJECT_BYTES
        };
        let chunk = self.read_chunk(&pointer.scope_id, &pointer.chunk_hash, chunk_limit)?;
        if chunk.raw_hash != pointer.raw_hash {
            return Err(PointerAttestationError::new(
                "chunk raw_hash does not match pointer",
            ));
        }
        if chunk.tool_profile_hash != pointer.tool_profile_hash {
            return Err(PointerAttestationError::new(
                "chunk profile does not match pointer",
            ));
        }
        if normalize.is_some_and(|normalize| chunk.r#gen != normalize.r#gen) {
            return Err(PointerAttestationError::new(
                "chunk generation does not match tree path",
            ));
        }
        if pointer
            .heading_path
            .as_ref()
            .is_some_and(|headings| headings != &chunk.heading_path)
            || pointer
                .section_id
                .as_ref()
                .is_some_and(|section| chunk.section_id.as_ref() != Some(section))
            || pointer
                .byte_start
                .is_some_and(|start| start != chunk.byte_start)
            || pointer.byte_end.is_some_and(|end| end != chunk.byte_end)
        {
            return Err(PointerAttestationError::new(
                "pointer fields do not match the immutable chunk object",
            ));
        }
        // Chunk reads may be large enough for a writer to advance HEAD while
        // they are in progress. Recheck the retained authority records before
        // handing current-tree text to the dump consumer.
        if !require_normalize
            && self.current_head(&pointer.scope_id)?.as_deref() != Some(&pointer.commit)
        {
            return Err(PointerAttestationError::new(
                "pointer commit is not the retained scope's current HEAD",
            ));
        }
        Ok(chunk)
    }

    /// Attest direct pointer values, bounded to the supplied top `k`.
    pub fn attest_top_k(&mut self, pointers: &[Value], k: usize) -> Vec<AttestationResult<()>> {
        pointers
            .iter()
            .take(k.min(MAX_POINTER_ATTESTATIONS_PER_QUERY))
            .map(|value| self.attest(value))
            .collect()
    }

    /// Attest `response.results[*].evidence_pointer`, bounded to the top `k`.
    pub fn attest_response_top_k(
        &mut self,
        response: &Value,
        k: usize,
    ) -> Vec<AttestationResult<()>> {
        let Some(results) = response.get("results").and_then(Value::as_array) else {
            return vec![Err(PointerAttestationError::new("results is not an array"))];
        };
        results
            .iter()
            .take(k.min(MAX_POINTER_ATTESTATIONS_PER_QUERY))
            .map(|result| match result.as_object() {
                Some(result) => match result.get("evidence_pointer") {
                    Some(pointer) => self.attest(pointer),
                    None => Err(PointerAttestationError::new(
                        "result has no evidence_pointer",
                    )),
                },
                None => Err(PointerAttestationError::new("result is not an object")),
            })
            .collect()
    }

    fn read_commit(&mut self, scope_id: &str, hash: &str) -> AttestationResult<CommitWire> {
        match self.read_object(scope_id, AttestedObjectKind::Commit, hash)? {
            CachedObject::Commit(commit) => Ok(commit),
            _ => Err(PointerAttestationError::new("commit cache type mismatch")),
        }
    }

    /// Re-read retained authority records immediately before returning current
    /// text, so a pointer cannot be validated against a stale pre-search HEAD.
    fn current_head(&self, scope_id: &str) -> AttestationResult<Option<String>> {
        let kio_dir = self
            .scope_kio_dirs
            .get(scope_id)
            .ok_or_else(|| {
                PointerAttestationError::new("pointer scope_id is not in the synthetic corpus")
            })?
            .try_clone()
            .map_err(|_| PointerAttestationError::new("scope capability unavailable"))?;
        read_current_head(&kio_dir)
    }

    fn read_tree(&mut self, scope_id: &str, hash: &str) -> AttestationResult<TreeWire> {
        match self.read_object(scope_id, AttestedObjectKind::Tree, hash)? {
            CachedObject::Tree(tree) => Ok(tree),
            _ => Err(PointerAttestationError::new("tree cache type mismatch")),
        }
    }

    fn read_chunk(
        &mut self,
        scope_id: &str,
        hash: &str,
        max_bytes: u64,
    ) -> AttestationResult<ChunkWire> {
        // Chunk text can be very large. Do not cache or clone it: the caller
        // either validates it once or moves it directly into the dump.
        let kio_dir = self
            .scope_kio_dirs
            .get(scope_id)
            .ok_or_else(|| {
                PointerAttestationError::new("pointer scope_id is not in the synthetic corpus")
            })?
            .try_clone()
            .map_err(|_| PointerAttestationError::new("scope capability unavailable"))?;
        if self.verified_bytes > MAX_POINTER_ATTESTATION_BYTES - max_bytes {
            return Err(PointerAttestationError::new(
                "pointer attestation byte bound exhausted",
            ));
        }
        self.read_chunk_object(&kio_dir, hash, max_bytes)
    }

    fn read_object(
        &mut self,
        scope_id: &str,
        kind: AttestedObjectKind,
        hash: &str,
    ) -> AttestationResult<CachedObject> {
        let key = ObjectKey {
            scope_id: scope_id.to_owned(),
            kind,
            hash: hash.to_owned(),
        };
        if let Some(cached) = self.object_cache.get(&key) {
            return match cached {
                CachedObject::Failure(error) => Err(error.clone()),
                object => Ok(object.clone()),
            };
        }
        if kind == AttestedObjectKind::Chunk {
            return self.read_uncached(scope_id, kind, hash);
        }
        let result = self.read_uncached(scope_id, kind, hash);
        let cached = match &result {
            Ok(object) => object.clone(),
            Err(error) => CachedObject::Failure(error.clone()),
        };
        self.object_cache.insert(key, cached);
        result
    }

    fn read_uncached(
        &mut self,
        scope_id: &str,
        kind: AttestedObjectKind,
        hash: &str,
    ) -> AttestationResult<CachedObject> {
        let kio_dir = self
            .scope_kio_dirs
            .get(scope_id)
            .ok_or_else(|| {
                PointerAttestationError::new("pointer scope_id is not in the synthetic corpus")
            })?
            .try_clone()
            .map_err(|_| PointerAttestationError::new("scope capability unavailable"))?;
        // `ObjectStore` owns exact object-size enforcement but does not accept a
        // caller budget. Reserve the maximum before opening so a failed/corrupt
        // object can never push this global evaluator bound past its limit.
        if self.verified_bytes > MAX_POINTER_ATTESTATION_BYTES - kind.max_bytes() {
            return Err(PointerAttestationError::new(
                "pointer attestation byte bound exhausted",
            ));
        }
        match kind {
            AttestedObjectKind::Commit | AttestedObjectKind::Tree => {
                let kind_dir = match kind {
                    AttestedObjectKind::Commit => "commits",
                    AttestedObjectKind::Tree => "trees",
                    AttestedObjectKind::Chunk => unreachable!(),
                };
                let bytes = self.read_cas_object(&kio_dir, kind_dir, hash, kind.max_bytes())?;
                match kind {
                    AttestedObjectKind::Commit => {
                        let commit: CommitWire = exact_canonical_json(&bytes, "commit object")?;
                        validate_commit_wire(&commit)?;
                        Ok(CachedObject::Commit(commit))
                    }
                    AttestedObjectKind::Tree => {
                        let tree: TreeWire = exact_canonical_json(&bytes, "tree object")?;
                        validate_tree_wire(&tree)?;
                        Ok(CachedObject::Tree(tree))
                    }
                    AttestedObjectKind::Chunk => unreachable!(),
                }
            }
            AttestedObjectKind::Chunk => Err(PointerAttestationError::new(
                "chunk reads bypass the metadata cache",
            )),
        }
    }

    fn read_cas_object(
        &mut self,
        kio_dir: &fs::File,
        kind_dir: &str,
        hash: &str,
        max_bytes: u64,
    ) -> AttestationResult<Vec<u8>> {
        let result = read_cap_cas_file(kio_dir, kind_dir, hash, max_bytes);
        let (bytes, consumed) = match result {
            Ok(value) => value,
            Err(error) => {
                self.charge(error.consumed)?;
                return Err(error.error);
            }
        };
        self.charge(consumed)?;
        if hash_bytes(&bytes) != hash {
            return Err(PointerAttestationError::new("CAS object hash mismatch"));
        }
        Ok(bytes)
    }

    fn read_chunk_object(
        &mut self,
        kio_dir: &fs::File,
        hash: &str,
        max_bytes: u64,
    ) -> AttestationResult<ChunkWire> {
        let result = read_cap_cas_file(kio_dir, "chunks", hash, max_bytes);
        let (bytes, consumed) = match result {
            Ok(value) => value,
            Err(error) => {
                self.charge(error.consumed)?;
                return Err(error.error);
            }
        };
        self.charge(consumed)?;
        let chunk: ChunkWire = exact_canonical_json(&bytes, "chunk object")?;
        validate_chunk_wire(&chunk)?;
        if chunk_identity_hash(&chunk)? != hash {
            return Err(PointerAttestationError::new(
                "chunk semantic identity does not match its fan-out key",
            ));
        }
        Ok(chunk)
    }

    fn charge(&mut self, bytes: u64) -> AttestationResult<()> {
        self.verified_bytes = self
            .verified_bytes
            .checked_add(bytes)
            .ok_or_else(|| PointerAttestationError::new("pointer attestation byte overflow"))?;
        if self.verified_bytes > MAX_POINTER_ATTESTATION_BYTES {
            return Err(PointerAttestationError::new(
                "pointer attestation byte bound exhausted",
            ));
        }
        Ok(())
    }
}

fn exact_canonical_json<T>(bytes: &[u8], label: &str) -> AttestationResult<T>
where
    T: for<'de> Deserialize<'de>,
{
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|_| PointerAttestationError::new(format!("{label} is not valid JSON")))?;
    let canonical = canonical_json_bytes(&value)
        .map_err(|_| PointerAttestationError::new(format!("{label} cannot be canonicalized")))?;
    if canonical != bytes {
        return Err(PointerAttestationError::new(format!(
            "{label} is not canonical JSON"
        )));
    }
    serde_json::from_value(value)
        .map_err(|_| PointerAttestationError::new(format!("{label} schema is invalid")))
}

fn validate_commit_wire(commit: &CommitWire) -> AttestationResult<()> {
    if commit.object_type != "commit"
        || !matches!(
            commit.commit_type.as_str(),
            "manual" | "auto" | "repaired" | "purged"
        )
        || !is_hash(&commit.tree)
        || !is_hash(&commit.tool_lock_hash)
        || commit
            .parent
            .as_deref()
            .is_some_and(|parent| !is_hash(parent))
        || !is_valid_created_at(&commit.created_at)
    {
        return Err(PointerAttestationError::new(
            "commit object violates evaluator wire invariants",
        ));
    }
    if commit.commit_type == "purged" {
        if commit.purged_raws.is_empty()
            || commit.purged_raws.iter().any(|raw| !is_hash(raw))
            || commit
                .purged_raws
                .windows(2)
                .any(|pair| pair[0].as_bytes() >= pair[1].as_bytes())
        {
            return Err(PointerAttestationError::new(
                "purged commit has invalid raw identity set",
            ));
        }
    } else if !commit.purged_raws.is_empty() {
        return Err(PointerAttestationError::new(
            "non-purged commit contains purged raw identities",
        ));
    }
    Ok(())
}

fn validate_tree_wire(tree: &TreeWire) -> AttestationResult<()> {
    if tree.object_type != "tree"
        || !is_hash(&tree.chunking_config_hash)
        || tree.entries.len() > MAX_TREE_ENTRIES
    {
        return Err(PointerAttestationError::new(
            "tree object violates evaluator wire invariants",
        ));
    }
    let mut previous: Option<&str> = None;
    for entry in &tree.entries {
        if previous.is_some_and(|path| path.as_bytes() >= entry.path.as_bytes())
            || !is_logical_direct_child(&entry.path)
            || entry.entry_type != "file"
            || !is_hash(&entry.raw_hash)
            || entry.normalize.as_ref().is_some_and(|normalize| {
                !is_hash(&normalize.tool_profile_hash) || !is_hash(&normalize.manifest_hash)
            })
        {
            return Err(PointerAttestationError::new(
                "tree entry violates evaluator wire invariants",
            ));
        }
        previous = Some(&entry.path);
    }
    Ok(())
}

fn validate_chunk_wire(chunk: &ChunkWire) -> AttestationResult<()> {
    if chunk.spec_version != 1
        || !is_hash(&chunk.raw_hash)
        || !is_hash(&chunk.tool_profile_hash)
        || !is_hash(&chunk.unit_content_hash)
        || !is_hash(&chunk.text_hash)
        || chunk.unit_key.is_empty()
        || chunk.byte_start > chunk.byte_end
        || hash_bytes(chunk.text.as_bytes()) != chunk.text_hash
    {
        return Err(PointerAttestationError::new(
            "chunk object violates evaluator wire invariants",
        ));
    }
    Ok(())
}

fn chunk_identity_hash(chunk: &ChunkWire) -> AttestationResult<String> {
    let mut identity = Map::new();
    identity.insert("byte_end".into(), Value::from(chunk.byte_end));
    identity.insert("byte_start".into(), Value::from(chunk.byte_start));
    identity.insert("gen".into(), Value::from(chunk.r#gen));
    identity.insert(
        "heading_path".into(),
        serde_json::to_value(&chunk.heading_path)
            .map_err(|_| PointerAttestationError::new("chunk identity is not serializable"))?,
    );
    identity.insert("raw_hash".into(), Value::from(chunk.raw_hash.clone()));
    if let Some(section_id) = chunk.section_id.as_ref().filter(|value| !value.is_empty()) {
        identity.insert("section_id".into(), Value::from(section_id.clone()));
    }
    identity.insert("spec_version".into(), Value::from(1));
    identity.insert(
        "tool_profile_hash".into(),
        Value::from(chunk.tool_profile_hash.clone()),
    );
    identity.insert("unit_key".into(), Value::from(chunk.unit_key.clone()));
    identity.insert(
        "unit_content_hash".into(),
        Value::from(chunk.unit_content_hash.clone()),
    );
    let canonical = canonical_json_bytes(&Value::Object(identity))
        .map_err(|_| PointerAttestationError::new("chunk identity cannot be canonicalized"))?;
    Ok(hash_bytes(&canonical))
}

fn is_logical_direct_child(path: &str) -> bool {
    !path.is_empty() && path != "." && path != ".." && !path.contains('/') && !path.contains('\0')
}

fn is_valid_created_at(value: &str) -> bool {
    let Some(body) = value.strip_suffix('Z') else {
        return false;
    };
    let datetime = match body.split_once('.') {
        Some((head, fraction))
            if !fraction.is_empty() && fraction.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            head
        }
        Some(_) => return false,
        None => body,
    };
    let bytes = datetime.as_bytes();
    if bytes.len() != 19
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes
            .iter()
            .enumerate()
            .any(|(index, byte)| !matches!(index, 4 | 7 | 10 | 13 | 16) && !byte.is_ascii_digit())
    {
        return false;
    }
    let field = |start: usize, end: usize| datetime[start..end].parse::<u32>().unwrap_or(u32::MAX);
    let year = field(0, 4);
    let month = field(5, 7);
    let day = field(8, 10);
    let hour = field(11, 13);
    let minute = field(14, 16);
    let second = field(17, 19);
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let max_day = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return false,
    };
    (1..=max_day).contains(&day) && hour <= 23 && minute <= 59 && second <= 59
}

fn read_scope_id(kio_dir: &fs::File) -> AttestationResult<String> {
    let bytes = read_cap_regular_file(kio_dir, "scope.json", MAX_SCOPE_RECORD_BYTES)
        .map_err(|_| PointerAttestationError::new("scope record unavailable"))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| PointerAttestationError::new("scope record is not valid JSON"))?;
    let object = value
        .as_object()
        .ok_or_else(|| PointerAttestationError::new("scope record is not a JSON object"))?;
    if object.get("kio_format_version").and_then(Value::as_str) != Some(KIO_FORMAT_VERSION) {
        return Err(PointerAttestationError::new(
            "scope record has incompatible format version",
        ));
    }
    let scope_id = object
        .get("scope_id")
        .and_then(Value::as_str)
        .filter(|value| is_ulid(value))
        .ok_or_else(|| PointerAttestationError::new("scope record has invalid scope_id"))?;
    Ok(scope_id.to_owned())
}

/// Bind current-tree extraction to the sole mutable authority record. The
/// linear-history schema deliberately has no branch ref: accepting only a
/// historical CAS path would let a search result smuggle an old snapshot into
/// a current-tree dump.
fn read_current_head(kio_dir: &fs::File) -> AttestationResult<Option<String>> {
    let head = read_cap_regular_file(kio_dir, "HEAD", MAX_SCOPE_RECORD_BYTES)
        .map_err(|_| PointerAttestationError::new("current HEAD unavailable"))?;
    let head = std::str::from_utf8(&head)
        .map_err(|_| PointerAttestationError::new("current HEAD is not UTF-8"))?
        .strip_suffix('\n')
        .ok_or_else(|| PointerAttestationError::new("current HEAD is not canonical"))?;
    if head == "unborn" {
        return Ok(None);
    }
    if !is_hash(head) {
        return Err(PointerAttestationError::new("current HEAD is invalid"));
    }
    Ok(Some(head.to_owned()))
}

#[derive(Debug)]
struct CapReadError {
    error: PointerAttestationError,
    consumed: u64,
}

/// Read the exact portable CAS slot by walking every fan-out component from
/// the retained `.kio` handle. Every directory and the final regular file is
/// opened no-follow; no public pathname is reconstructed.
fn read_cap_cas_file(
    kio_dir: &fs::File,
    kind_dir: &str,
    hash: &str,
    max_bytes: u64,
) -> Result<(Vec<u8>, u64), CapReadError> {
    if !is_hash(hash) {
        return Err(CapReadError {
            error: PointerAttestationError::new("invalid CAS object hash"),
            consumed: 0,
        });
    }
    let digest = &hash["sha256:".len()..];
    let mut directory = kio_dir.try_clone().map_err(|_| CapReadError {
        error: PointerAttestationError::new("CAS object unavailable"),
        consumed: 0,
    })?;
    for component in ["objects", kind_dir, &digest[..2], &digest[2..4]] {
        directory = cap_fs::open_dir_nofollow(&directory, Path::new(component)).map_err(|_| {
            CapReadError {
                error: PointerAttestationError::new("CAS object unavailable"),
                consumed: 0,
            }
        })?;
    }
    read_cap_regular_file_accounted(&directory, digest, max_bytes)
}

fn read_cap_regular_file(
    directory: &fs::File,
    name: &str,
    max_bytes: u64,
) -> AttestationResult<Vec<u8>> {
    read_cap_regular_file_accounted(directory, name, max_bytes)
        .map(|(bytes, _)| bytes)
        .map_err(|error| error.error)
}

fn read_cap_regular_file_accounted(
    directory: &fs::File,
    name: &str,
    max_bytes: u64,
) -> Result<(Vec<u8>, u64), CapReadError> {
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut file =
        cap_fs::open(directory, Path::new(name), &options).map_err(|_| CapReadError {
            error: PointerAttestationError::new("CAS object unavailable"),
            consumed: 0,
        })?;
    let metadata = file.metadata().map_err(|_| CapReadError {
        error: PointerAttestationError::new("CAS object unavailable"),
        consumed: 0,
    })?;
    if !metadata.is_file() {
        return Err(CapReadError {
            error: PointerAttestationError::new("CAS object unavailable"),
            consumed: 0,
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(CapReadError {
                error: PointerAttestationError::new("CAS object unavailable"),
                consumed: 0,
            });
        }
    }
    if metadata.len() > max_bytes {
        return Err(CapReadError {
            error: PointerAttestationError::new("CAS object exceeds its byte limit"),
            consumed: 0,
        });
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    let mut limited = file.by_ref().take(max_bytes.saturating_add(1));
    match limited.read_to_end(&mut bytes) {
        Ok(_) if bytes.len() as u64 <= max_bytes => {
            let consumed = bytes.len() as u64;
            Ok((bytes, consumed))
        }
        Ok(_) => Err(CapReadError {
            error: PointerAttestationError::new("CAS object exceeds its byte limit"),
            consumed: bytes.len() as u64,
        }),
        Err(_) => Err(CapReadError {
            error: PointerAttestationError::new("CAS object unavailable"),
            consumed: bytes.len() as u64,
        }),
    }
}

fn is_ulid(value: &str) -> bool {
    value.len() == 26
        && value.bytes().all(|byte| {
            matches!(byte, b'0'..=b'9' | b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z')
        })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use kio_core::{
        cas::{ChunkObject, ObjectKind, ObjectStore, hash_bytes},
        dag::{CommitObject, CommitStats, CommitType, NormalizeRef, TreeEntry, build_tree},
        scope::Repository,
    };
    use serde_json::{Value, json};
    use tempfile::TempDir;

    use crate::boundary::BoundCorpus;

    use super::{PointerAttestor, parse_pointer_wire, read_cap_regular_file};

    const RAW_HASH: &str =
        "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const PROFILE_HASH: &str =
        "sha256:2222222222222222222222222222222222222222222222222222222222222222";
    const MANIFEST_HASH: &str =
        "sha256:3333333333333333333333333333333333333333333333333333333333333333";
    const TOOL_LOCK_HASH: &str =
        "sha256:4444444444444444444444444444444444444444444444444444444444444444";
    const FROZEN_VALID_POINTER: &str = r#"{"chunk_hash":"sha256:5555555555555555555555555555555555555555555555555555555555555555","commit":"sha256:6666666666666666666666666666666666666666666666666666666666666666","path_at_commit":"document.md","raw_hash":"sha256:7777777777777777777777777777777777777777777777777777777777777777","schema_version":1,"scope_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","tool_profile_hash":"sha256:8888888888888888888888888888888888888888888888888888888888888888","tree":"sha256:9999999999999999999999999999999999999999999999999999999999999999"}"#;
    const FROZEN_MALFORMED_POINTERS: [&str; 8] = [
        r#"{"chunk_hash":"sha256:5555555555555555555555555555555555555555555555555555555555555555","commit":"sha256:6666666666666666666666666666666666666666666666666666666666666666","raw_hash":"sha256:7777777777777777777777777777777777777777777777777777777777777777","schema_version":2,"scope_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","tool_profile_hash":"sha256:8888888888888888888888888888888888888888888888888888888888888888"}"#,
        r#"{"byte_start":1,"chunk_hash":"sha256:5555555555555555555555555555555555555555555555555555555555555555","commit":"sha256:6666666666666666666666666666666666666666666666666666666666666666","raw_hash":"sha256:7777777777777777777777777777777777777777777777777777777777777777","schema_version":1,"scope_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","tool_profile_hash":"sha256:8888888888888888888888888888888888888888888888888888888888888888"}"#,
        r#"{"chunk_hash":"sha256:5555555555555555555555555555555555555555555555555555555555555555","commit":"sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","raw_hash":"sha256:7777777777777777777777777777777777777777777777777777777777777777","schema_version":1,"scope_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","tool_profile_hash":"sha256:8888888888888888888888888888888888888888888888888888888888888888"}"#,
        r#"{"chunk_hash":"sha256:5555555555555555555555555555555555555555555555555555555555555555","commit":"sha256:6666666666666666666666666666666666666666666666666666666666666666","path_at_commit":"../victim.md","raw_hash":"sha256:7777777777777777777777777777777777777777777777777777777777777777","schema_version":1,"scope_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","tool_profile_hash":"sha256:8888888888888888888888888888888888888888888888888888888888888888"}"#,
        r#"{"chunk_hash":"sha256:5555555555555555555555555555555555555555555555555555555555555555","commit":"sha256:6666666666666666666666666666666666666666666666666666666666666666","raw_hash":"sha256:7777777777777777777777777777777777777777777777777777777777777777","schema_version":1,"scope_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","tool_profile_hash":"sha256:8888888888888888888888888888888888888888888888888888888888888888","unexpected":true}"#,
        r#"{"chunk_hash":"sha256:5555555555555555555555555555555555555555555555555555555555555555","commit":"sha256:6666666666666666666666666666666666666666666666666666666666666666","heading_path":[],"raw_hash":"sha256:7777777777777777777777777777777777777777777777777777777777777777","schema_version":1,"scope_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","tool_profile_hash":"sha256:8888888888888888888888888888888888888888888888888888888888888888"}"#,
        r#"{"chunk_hash":"sha256:5555555555555555555555555555555555555555555555555555555555555555","commit":"sha256:6666666666666666666666666666666666666666666666666666666666666666","heading_path":null,"raw_hash":"sha256:7777777777777777777777777777777777777777777777777777777777777777","schema_version":1,"scope_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","tool_profile_hash":"sha256:8888888888888888888888888888888888888888888888888888888888888888"}"#,
        r#"{"chunk_hash":"sha256:5555555555555555555555555555555555555555555555555555555555555555","commit":"sha256:6666666666666666666666666666666666666666666666666666666666666666","heading_path":[""],"raw_hash":"sha256:7777777777777777777777777777777777777777777777777777777777777777","schema_version":1,"scope_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","tool_profile_hash":"sha256:8888888888888888888888888888888888888888888888888888888888888888"}"#,
    ];

    struct Fixture {
        root: TempDir,
        scope: String,
        pointer: Value,
        chunk: ChunkObject,
    }

    #[test]
    fn evaluator_pointer_wire_accepts_frozen_valid_and_rejects_malformed_vectors() {
        let value: Value = serde_json::from_str(FROZEN_VALID_POINTER).unwrap();
        parse_pointer_wire(&value).unwrap();
        assert_eq!(
            kio_core::cas::canonical_json_bytes(&value).unwrap(),
            FROZEN_VALID_POINTER.as_bytes()
        );
        for malformed in FROZEN_MALFORMED_POINTERS {
            let value: Value = serde_json::from_str(malformed).unwrap();
            assert!(parse_pointer_wire(&value).is_err(), "accepted {malformed}");
        }

        let mut too_many_headings: Value = serde_json::from_str(FROZEN_VALID_POINTER).unwrap();
        too_many_headings["heading_path"] = json!(vec!["heading"; 65]);
        assert!(parse_pointer_wire(&too_many_headings).is_err());
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let scope = "research".to_owned();
        let scope_root = root.path().join(&scope);
        fs::create_dir(&scope_root).unwrap();
        let repo = Repository::init(&scope_root).unwrap();
        let scope_id = repo.scope_identity().unwrap().scope_id;
        let store = ObjectStore::new(repo.kio_dir());

        let text = "historical pointer attestation";
        let text_hash = hash_bytes(text.as_bytes());
        let chunk = kio_core::cas::ChunkObject {
            spec_version: 1,
            raw_hash: RAW_HASH.to_owned(),
            tool_profile_hash: PROFILE_HASH.to_owned(),
            r#gen: 3,
            unit_key: "section:old".to_owned(),
            unit_content_hash: text_hash.clone(),
            heading_path: vec!["Old Document".to_owned(), "Historical".to_owned()],
            section_id: Some("old-document/historical".to_owned()),
            byte_start: 0,
            byte_end: text.len() as u64,
            text_hash,
            text: text.to_owned(),
        };
        let chunk_hash = store.write_chunk(&chunk).unwrap();
        let tree = build_tree(vec![TreeEntry {
            path: "old-name.md".to_owned(),
            entry_type: "file".to_owned(),
            raw_hash: RAW_HASH.to_owned(),
            normalize: Some(NormalizeRef {
                tool_profile_hash: PROFILE_HASH.to_owned(),
                r#gen: 3,
                manifest_hash: MANIFEST_HASH.to_owned(),
            }),
        }])
        .unwrap();
        let (tree_hash, _) = store
            .write_json(ObjectKind::Tree, &serde_json::to_value(&tree).unwrap())
            .unwrap();
        let commit = CommitObject::new(
            tree_hash.clone(),
            None,
            "2026-07-13T00:00:00Z".to_owned(),
            "synthetic attestation".to_owned(),
            TOOL_LOCK_HASH.to_owned(),
            CommitStats {
                files_added: 1,
                files_modified: 0,
                files_deleted: 0,
            },
            CommitType::Manual,
        )
        .unwrap();
        let (commit_hash, _) = store
            .write_json(ObjectKind::Commit, &serde_json::to_value(&commit).unwrap())
            .unwrap();
        Fixture {
            root,
            scope,
            pointer: json!({
                "schema_version": 1,
                "scope_id": scope_id,
                "commit": commit_hash,
                "tree": tree_hash,
                "raw_hash": RAW_HASH,
                "tool_profile_hash": PROFILE_HASH,
                "chunk_hash": chunk_hash,
                "path_at_commit": "old-name.md",
            }),
            chunk,
        }
    }

    fn write_raw_cas(scope_root: &std::path::Path, kind: &str, hash: &str, bytes: &[u8]) {
        let digest = hash.strip_prefix("sha256:").unwrap();
        let parent = scope_root
            .join(".kio/objects")
            .join(kind)
            .join(&digest[..2])
            .join(&digest[2..4]);
        fs::create_dir_all(&parent).unwrap();
        fs::write(parent.join(digest), bytes).unwrap();
    }

    #[test]
    fn attests_valid_pointer_and_does_not_cache_chunk_text() {
        let fixture = fixture();
        let mut attestor =
            PointerAttestor::new(fixture.root.path(), std::slice::from_ref(&fixture.scope))
                .unwrap();
        attestor.attest(&fixture.pointer).unwrap();
        let verified = attestor.verified_bytes();
        attestor.attest(&fixture.pointer).unwrap();
        // Commit/tree metadata are cached; chunks deliberately are not so a
        // large text payload is never retained and cloned by the attestor.
        assert!(attestor.verified_bytes() > verified);
    }

    #[test]
    fn current_chunk_text_accepts_only_the_explicit_missing_normalize_case() {
        let fixture = fixture();
        let store = ObjectStore::new(fixture.root.path().join(&fixture.scope).join(".kio"));
        let tree = build_tree(vec![TreeEntry {
            path: "old-name.md".to_owned(),
            entry_type: "file".to_owned(),
            raw_hash: RAW_HASH.to_owned(),
            normalize: None,
        }])
        .unwrap();
        let (tree_hash, _) = store
            .write_json(ObjectKind::Tree, &serde_json::to_value(&tree).unwrap())
            .unwrap();
        let commit = CommitObject::new(
            tree_hash.clone(),
            None,
            "2026-07-13T00:00:00Z".to_owned(),
            "current candidate".to_owned(),
            TOOL_LOCK_HASH.to_owned(),
            CommitStats {
                files_added: 1,
                files_modified: 0,
                files_deleted: 0,
            },
            CommitType::Manual,
        )
        .unwrap();
        let (commit_hash, _) = store
            .write_json(ObjectKind::Commit, &serde_json::to_value(&commit).unwrap())
            .unwrap();
        let mut pointer = fixture.pointer.clone();
        pointer["commit"] = json!(commit_hash);
        pointer["tree"] = json!(tree_hash);
        let kio = fixture.root.path().join(&fixture.scope).join(".kio");
        fs::write(
            kio.join("HEAD"),
            format!("{}\n", pointer["commit"].as_str().unwrap()),
        )
        .unwrap();

        let mut strict =
            PointerAttestor::new(fixture.root.path(), std::slice::from_ref(&fixture.scope))
                .unwrap();
        assert!(strict.attest(&pointer).is_err());
        let mut current =
            PointerAttestor::new(fixture.root.path(), std::slice::from_ref(&fixture.scope))
                .unwrap();
        assert_eq!(
            current.attest_current_chunk_text(&pointer).unwrap(),
            fixture.chunk.text
        );

        pointer["tool_profile_hash"] = json!(TOOL_LOCK_HASH);
        assert!(current.attest_current_chunk_text(&pointer).is_err());
    }

    #[test]
    fn current_chunk_text_rejects_historical_pointer_even_when_its_cas_path_is_valid() {
        let fixture = fixture();
        let kio = fixture.root.path().join(&fixture.scope).join(".kio");
        // The fixture pointer is a valid historical object, but no current
        // authority points at it (a freshly initialized repository is unborn).
        fs::write(
            kio.join("HEAD"),
            b"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
        )
        .unwrap();
        let mut attestor =
            PointerAttestor::new(fixture.root.path(), std::slice::from_ref(&fixture.scope))
                .unwrap();
        assert!(
            attestor
                .attest_current_chunk_text(&fixture.pointer)
                .is_err()
        );
    }

    #[test]
    fn rejects_tree_path_hash_and_identity_tuple_mismatches() {
        let fixture = fixture();
        let mut attestor =
            PointerAttestor::new(fixture.root.path(), std::slice::from_ref(&fixture.scope))
                .unwrap();
        let mut wrong_path = fixture.pointer.clone();
        wrong_path["path_at_commit"] = json!("current-name.md");
        assert!(attestor.attest(&wrong_path).is_err());

        let mut wrong_tree = fixture.pointer.clone();
        wrong_tree["tree"] =
            json!("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert!(attestor.attest(&wrong_tree).is_err());

        let mut wrong_raw = fixture.pointer.clone();
        wrong_raw["raw_hash"] =
            json!("sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        assert!(attestor.attest(&wrong_raw).is_err());

        let mut wrong_profile = fixture.pointer.clone();
        wrong_profile["tool_profile_hash"] =
            json!("sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc");
        assert!(attestor.attest(&wrong_profile).is_err());

        let mut wrong_range = fixture.pointer.clone();
        wrong_range["byte_start"] = json!(1);
        wrong_range["byte_end"] = json!(fixture.chunk.byte_end);
        assert!(attestor.attest(&wrong_range).is_err());

        let mut wrong_heading = fixture.pointer.clone();
        wrong_heading["heading_path"] = json!(["forged heading"]);
        assert!(attestor.attest(&wrong_heading).is_err());
    }

    #[test]
    fn rejects_chunk_generation_that_differs_from_the_tree_entry() {
        let fixture = fixture();
        let mut wrong_generation = fixture.chunk.clone();
        wrong_generation.r#gen = 4;
        let chunk_hash = ObjectStore::new(fixture.root.path().join(&fixture.scope).join(".kio"))
            .write_chunk(&wrong_generation)
            .unwrap();
        let mut pointer = fixture.pointer.clone();
        pointer["chunk_hash"] = json!(chunk_hash);

        let mut attestor =
            PointerAttestor::new(fixture.root.path(), std::slice::from_ref(&fixture.scope))
                .unwrap();
        assert!(attestor.attest(&pointer).is_err());
    }

    #[test]
    fn rejects_hash_matching_but_noncanonical_commit_bytes() {
        let fixture = fixture();
        let scope_root = fixture.root.path().join(&fixture.scope);
        let commit = fixture.pointer["commit"].as_str().unwrap();
        let digest = commit.strip_prefix("sha256:").unwrap();
        let canonical = fs::read(
            scope_root
                .join(".kio/objects/commits")
                .join(&digest[..2])
                .join(&digest[2..4])
                .join(digest),
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&canonical).unwrap();
        let noncanonical = serde_json::to_vec_pretty(&value).unwrap();
        let noncanonical_hash = hash_bytes(&noncanonical);
        write_raw_cas(&scope_root, "commits", &noncanonical_hash, &noncanonical);
        let mut pointer = fixture.pointer.clone();
        pointer["commit"] = json!(noncanonical_hash);

        let mut attestor =
            PointerAttestor::new(fixture.root.path(), std::slice::from_ref(&fixture.scope))
                .unwrap();
        assert!(attestor.attest(&pointer).is_err());
    }

    #[test]
    fn bounded_reader_rejects_oversized_and_non_regular_objects() {
        let directory = tempfile::tempdir().unwrap();
        let handle = cap_primitives::fs::open_ambient_dir(
            directory.path(),
            cap_primitives::ambient_authority(),
        )
        .unwrap();

        fs::write(directory.path().join("oversized"), b"{}").unwrap();
        assert!(read_cap_regular_file(&handle, "oversized", 1).is_err());

        fs::create_dir(directory.path().join("not-a-file")).unwrap();
        assert!(read_cap_regular_file(&handle, "not-a-file", 1024).is_err());
    }

    #[test]
    fn rejects_scope_not_declared_by_fixed_corpus_map() {
        let fixture = fixture();
        let mut attestor =
            PointerAttestor::new(fixture.root.path(), std::slice::from_ref(&fixture.scope))
                .unwrap();
        let mut pointer = fixture.pointer.clone();
        pointer["scope_id"] = json!("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert!(attestor.attest(&pointer).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn bound_attestor_keeps_original_scope_after_public_path_is_replaced() {
        use std::os::unix::fs::symlink;

        let fixture = fixture();
        let corpus =
            BoundCorpus::bind(fixture.root.path(), std::slice::from_ref(&fixture.scope)).unwrap();
        let mut attestor = PointerAttestor::from_bound_corpus(&corpus).unwrap();

        let public_scope = fixture.root.path().join(&fixture.scope);
        let retained_scope = fixture.root.path().join("retained-research");
        let victim = fixture.root.path().join("victim");
        fs::create_dir(&victim).unwrap();
        fs::rename(&public_scope, &retained_scope).unwrap();
        symlink(&victim, &public_scope).unwrap();

        // A pathname-based attestor would now read the replacement (or follow
        // its link). The retained `.kio` capability can only reach the scope
        // that was bound before the replacement.
        attestor.attest(&fixture.pointer).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_scope_record() {
        use std::os::unix::fs::symlink;

        let fixture = fixture();
        let scope_json = fixture
            .root
            .path()
            .join(&fixture.scope)
            .join(".kio/scope.json");
        let target = fixture.root.path().join("outside.json");
        fs::write(
            &target,
            serde_json::to_vec(&serde_json::json!({
                "kio_format_version": kio_core::scope::KIO_FORMAT_VERSION,
                "scope_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            }))
            .unwrap(),
        )
        .unwrap();
        fs::remove_file(&scope_json).unwrap();
        symlink(&target, &scope_json).unwrap();
        assert!(
            PointerAttestor::new(fixture.root.path(), std::slice::from_ref(&fixture.scope))
                .is_err()
        );
    }
}
