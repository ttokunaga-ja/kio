//! Content-addressed storage primitives.

#[cfg(test)]
mod embedding_format_tests;

mod embedding_metadata;
pub use embedding_metadata::{
    EmbeddingContext, EmbeddingMetadata, ImageInputReceipt, ProducerExecutionMode,
    ProducerProvenance,
};

mod bound;
mod image;
pub use image::ImageStore;
use image::write_bound_content_at;
mod store;

mod removal;
use removal::{CasRemovalKind, bound_remove_verified, remove_verified_cas_path};

use std::fs::{self, File, OpenOptions};
use std::io::Seek;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::error::{IoResultExt, KioError, Result};
use crate::purge::sync_directory;

#[cfg(all(test, windows))]
use crate::platform::cas_identity::windows_file_information;
#[cfg(all(test, unix))]
use crate::platform::cas_io::validate_open_regular_nofollow_with_before;
use crate::platform::cas_io::{
    bound_metadata_single_link, configure_no_follow, observe_regular_file_stability, replace_file,
    same_bound_regular_entry, same_open_file, sync_bound_directory,
};
use crate::platform::cas_semantics;

pub const CAS_STREAM_BUFFER_BYTES: usize = 64 * 1024;
pub const MAX_RAW_OBJECT_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_TREE_OBJECT_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_COMMIT_OBJECT_BYTES: u64 = 1024 * 1024;
/// Semantic chunk objects contain bounded normalized text, never raw file bytes.
pub const MAX_CHUNK_OBJECT_BYTES: u64 = 128 * 1024 * 1024;
/// Normalized-unit objects are canonical JSON representations of a single
/// markdownized prepared unit. They share the existing normalized unit size
/// ceiling and are read as one bounded immutable CAS body.
pub const MAX_NORMALIZED_UNIT_OBJECT_BYTES: u64 = 128 * 1024 * 1024;

/// An embedding object is an identity header plus one base64 vector line.
/// The adopted profile is 768 f32 (03 §7), so a real object runs ~4.4 KB; the
/// cap is generous enough for any plausible future width and still small enough
/// that a corrupt length is rejected before it is read.
pub const MAX_EMBEDDING_OBJECT_BYTES: u64 = 1024 * 1024;
/// A normalized-instance manifest is deliberately much smaller than a raw or
/// prepared object. Keeping the limit at the CAS boundary prevents inventory
/// and fsck from hashing a pathologically large object before the semantic
/// manifest reader can apply its own bound.
pub const MAX_MANIFEST_OBJECT_BYTES: u64 = 8 * 1024 * 1024;

mod read_budget;

pub(crate) use read_budget::CasReadAccess;
pub use read_budget::{VerifiedCasReadBudget, VerifiedCasReadCapability};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentObjectKind {
    Prepared,
    Image,
    /// PB01/PB02 (step4b-contract-tests-p2b.md §A/§B, 10-operations.md
    /// §7.5.1 L489): content-addressed embedding object (vector + declared
    /// dimensions), stored under `objects/embeddings/`.
    Embedding,
    /// PB02 (10 §7.5.1 L489): content-addressed manifest object, stored
    /// under `objects/manifests/`.
    Manifest,
    /// PB02 (10 §7.5.1 L489): content-addressed tool-lock object (canonical
    /// JCS bytes content hash — 03 §5.2), stored under `objects/toollocks/`.
    Toollock,
    /// Immutable normalized-unit JSON object, stored under
    /// `objects/normalized_unit_objects/`.
    NormalizedUnit,
}

impl ContentObjectKind {
    #[must_use]
    pub const fn directory(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Image => "image",
            Self::Embedding => "embeddings",
            Self::Manifest => "manifests",
            Self::Toollock => "toollocks",
            Self::NormalizedUnit => "normalized_unit_objects",
        }
    }

    #[must_use]
    pub const fn object_type(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Image => "image",
            Self::Embedding => "embedding",
            Self::Manifest => "manifest",
            Self::Toollock => "toollock",
            Self::NormalizedUnit => "normalized_unit",
        }
    }

    #[must_use]
    const fn max_bytes(self) -> u64 {
        match self {
            Self::Embedding => MAX_EMBEDDING_OBJECT_BYTES,
            Self::Manifest => MAX_MANIFEST_OBJECT_BYTES,
            Self::NormalizedUnit => MAX_NORMALIZED_UNIT_OBJECT_BYTES,
            Self::Prepared | Self::Image | Self::Toollock => MAX_RAW_OBJECT_BYTES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredContentObjectMetadata {
    pub kind: ContentObjectKind,
    pub hash: String,
    pub size_bytes: u64,
}

/// Durable Step 4 chunk object (03-data-model §8.1).
///
/// Its storage key is the semantic identity hash computed from the first eight
/// fields, not the content hash of this JSON payload. Keeping this type in core
/// prevents SQLite/JSONL acceleration rows from becoming Evidence truth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChunkObject {
    pub spec_version: u64,
    pub raw_hash: String,
    pub tool_profile_hash: String,
    pub r#gen: u64,
    pub unit_key: String,
    /// Hash of the exact normalized Markdown bytes. This is the stable content
    /// axis in chunk identity: a same-gen body correction gets a new chunk,
    /// while a byte-identical resurrection retains the old pointer identity.
    pub unit_content_hash: String,
    pub heading_path: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section_id: Option<String>,
    /// Unit-local UTF-8 byte offset, 0-based half-open (03 §8.1). Always
    /// present in a valid chunk object — part of the identity tuple, not an
    /// optional field like `section_id`.
    pub byte_start: u64,
    pub byte_end: u64,
    pub text_hash: String,
    pub text: String,
}

/// One embedding vector as CAS truth (03 §8.1, 04 §4.3).
///
/// The `embeddings` SQLite table and `chunk_vec` are an acceleration layer over
/// this — "真実は `objects/` にある", and `kio repair rebuild-db` rebuilds in the
/// order `objects/` → `embeddings` → `chunk_vec`. Until R25-6 the object did not
/// exist: `rebuild-db` snapshotted vectors out of the very database it was about
/// to replace, so losing `sqlite.db` meant buying every vector again from the
/// API. The user's knowledge was never at risk (that lives in
/// `objects/normalized`), but the money and the wall-clock were.
///
/// The stored bytes are fixed by 03 §8.1 as
/// five LF-terminated lines: JCS identity, base64 vector, vector digest,
/// JCS metadata, and metadata digest
/// — a text header a human can read next to a compact body, plus a digest of the
/// vector body alone. Its digest is not redundant with the storage key: the key
/// is the hash of the IDENTITY (what this vector is OF), so nothing else would
/// notice a bit flip inside the vector itself.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingObject {
    pub spec_version: u64,
    pub target_type: String,
    pub target_hash: String,
    pub profile_hash: String,
    pub modality: String,
    pub dimensions: u64,
    pub distance: String,
    /// Chunk filename text or a converted-image receipt digest (03 §8.1).
    /// `None` omits the key entirely, preserving original image/text identity.
    pub context: Option<EmbeddingContext>,
    pub vector: Vec<f32>,
    pub metadata: EmbeddingMetadata,
}

impl EmbeddingObject {
    /// The identity fields alone, as the JSON whose JCS hash is the storage key.
    ///
    /// Split out from [`Self::identity_hash`] so a caller that has no vector yet
    /// — the embedding lane deciding whether it already owns this vector — can
    /// address the object without inventing one.
    #[must_use]
    pub fn identity_value(
        target_type: &str,
        target_hash: &str,
        profile_hash: &str,
        modality: &str,
        dimensions: u64,
        distance: &str,
        context: Option<&EmbeddingContext>,
    ) -> Value {
        let mut value = Map::new();
        value.insert("dimensions".to_owned(), Value::from(dimensions));
        value.insert("distance".to_owned(), Value::from(distance));
        value.insert("modality".to_owned(), Value::from(modality));
        value.insert("profile_hash".to_owned(), Value::from(profile_hash));
        value.insert("spec_version".to_owned(), Value::from(1));
        value.insert("target_hash".to_owned(), Value::from(target_hash));
        value.insert("target_type".to_owned(), Value::from(target_type));
        if let Some(context) = context {
            let context = match context {
                EmbeddingContext::Text(text) => Value::from(text.clone()),
                EmbeddingContext::Image { image_input_hash } => {
                    serde_json::json!({ "image_input_hash": image_input_hash })
                }
            };
            value.insert("context".to_owned(), context);
        }
        Value::Object(value)
    }

    /// The storage key: `"sha256:" + base16(sha256(JCS(identity fields)))`.
    pub fn identity_hash(&self) -> Result<String> {
        self.validate()?;
        hash_json(&Self::identity_value(
            &self.target_type,
            &self.target_hash,
            &self.profile_hash,
            &self.modality,
            self.dimensions,
            &self.distance,
            self.context.as_ref(),
        ))
    }

    /// The canonical stored bytes (03 §8.1).
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let header = canonical_json_bytes(&Self::identity_value(
            &self.target_type,
            &self.target_hash,
            &self.profile_hash,
            &self.modality,
            self.dimensions,
            &self.distance,
            self.context.as_ref(),
        ))?;
        let vector_bytes = vector_to_le_bytes(&self.vector);
        let mut out = header;
        out.push(b'\n');
        out.extend_from_slice(base64_encode(&vector_bytes).as_bytes());
        out.push(b'\n');
        out.extend_from_slice(lower_hex(&Sha256::digest(&vector_bytes)).as_bytes());
        out.push(b'\n');
        let metadata = canonical_json_bytes(
            &serde_json::to_value(&self.metadata)
                .map_err(|error| embedding_corrupt_error(&error.to_string(), None))?,
        )?;
        out.extend_from_slice(&metadata);
        out.push(b'\n');
        out.extend_from_slice(lower_hex(&Sha256::digest(&metadata)).as_bytes());
        out.push(b'\n');
        Ok(out)
    }

    /// Parse and fully verify stored bytes.
    ///
    /// Every check 10 §7.5.1 asks `fsck` for lives here rather than in the
    /// caller, so a read through any path gets all of them: the vector's length
    /// matches the declared `dimensions`, no component is NaN or infinite, and
    /// the trailing digest matches the body.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| embedding_corrupt_error("embedding object is not UTF-8", None))?;
        let lines = text.split('\n').collect::<Vec<_>>();
        let [header, body, digest, metadata, metadata_digest, ""] = lines.as_slice() else {
            return Err(embedding_corrupt_error(
                "embedding object must have exactly five LF-terminated lines",
                None,
            ));
        };
        if lower_hex(&Sha256::digest(metadata.as_bytes())) != *metadata_digest {
            return Err(embedding_corrupt_error(
                "embedding metadata digest does not match its bytes",
                None,
            ));
        }
        let metadata: EmbeddingMetadata = serde_json::from_str(metadata)
            .map_err(|error| embedding_corrupt_error(&error.to_string(), None))?;
        let value: Value = serde_json::from_str(header)
            .map_err(|error| embedding_corrupt_error(&error.to_string(), None))?;
        let object = value.as_object().ok_or_else(|| {
            embedding_corrupt_error("embedding object header must be a JSON object", None)
        })?;
        let string = |key: &str| -> Result<String> {
            object
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| {
                    embedding_corrupt_error(&format!("embedding header lacks {key}"), None)
                })
        };
        let dimensions = object
            .get("dimensions")
            .and_then(Value::as_u64)
            .ok_or_else(|| embedding_corrupt_error("embedding header lacks dimensions", None))?;
        let vector_bytes = base64_decode(body)?;
        if lower_hex(&Sha256::digest(&vector_bytes)) != *digest {
            return Err(embedding_corrupt_error(
                "embedding vector digest does not match its bytes",
                None,
            ));
        }
        if vector_bytes.len() as u64 != dimensions.saturating_mul(4) {
            return Err(embedding_corrupt_error(
                "embedding vector length does not match declared dimensions",
                None,
            ));
        }
        let vector = vector_from_le_bytes(&vector_bytes);
        if vector.iter().any(|component| !component.is_finite()) {
            return Err(embedding_corrupt_error(
                "embedding vector holds a NaN or infinite component",
                None,
            ));
        }
        let parsed = Self {
            spec_version: object
                .get("spec_version")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            target_type: string("target_type")?,
            target_hash: string("target_hash")?,
            profile_hash: string("profile_hash")?,
            modality: string("modality")?,
            dimensions,
            distance: string("distance")?,
            context: object
                .get("context")
                .map(|context| {
                    serde_json::from_value(context.clone())
                        .map_err(|error| embedding_corrupt_error(&error.to_string(), None))
                })
                .transpose()?,
            vector,
            metadata,
        };
        parsed.validate()?;
        if parsed.to_bytes()? != bytes {
            return Err(embedding_corrupt_error(
                "embedding record is not in canonical current format",
                None,
            ));
        }
        Ok(parsed)
    }

    pub fn validate(&self) -> Result<()> {
        if self.spec_version != 1 {
            return Err(embedding_corrupt_error(
                "embedding spec_version must be exactly 1",
                None,
            ));
        }
        for (name, value) in [
            ("target_type", &self.target_type),
            ("target_hash", &self.target_hash),
            ("profile_hash", &self.profile_hash),
            ("modality", &self.modality),
            ("distance", &self.distance),
        ] {
            if value.is_empty() {
                return Err(embedding_corrupt_error(
                    &format!("embedding {name} must not be empty"),
                    None,
                ));
            }
        }
        if !is_hash(&self.target_hash)
            || !is_hash(&self.profile_hash)
            || !matches!(self.target_type.as_str(), "chunk" | "image")
        {
            return Err(embedding_corrupt_error(
                "embedding target/profile identity is invalid",
                None,
            ));
        }
        self.metadata.validate(
            &self.profile_hash,
            &self.target_type,
            &self.target_hash,
            self.context.as_ref(),
        )?;
        if self.dimensions == 0 || self.vector.len() as u64 != self.dimensions {
            return Err(embedding_corrupt_error(
                "embedding vector length must equal its declared dimensions",
                None,
            ));
        }
        if self.vector.iter().any(|component| !component.is_finite()) {
            return Err(embedding_corrupt_error(
                "embedding vector holds a NaN or infinite component",
                None,
            ));
        }
        let norm = self
            .vector
            .iter()
            .map(|value| f64::from(*value).powi(2))
            .sum::<f64>()
            .sqrt();
        if norm == 0.0 || (norm - 1.0).abs() > 0.0001 {
            return Err(embedding_corrupt_error(
                "embedding vector must have nonzero unit L2 norm",
                None,
            ));
        }
        Ok(())
    }
}

impl ChunkObject {
    /// Recompute the path-independent semantic identity frozen in docs/03 §8.1.
    pub fn identity_hash(&self) -> Result<String> {
        self.validate()?;
        let mut value = Map::new();
        value.insert("byte_end".to_owned(), Value::from(self.byte_end));
        value.insert("byte_start".to_owned(), Value::from(self.byte_start));
        value.insert("gen".to_owned(), Value::from(self.r#gen));
        value.insert(
            "heading_path".to_owned(),
            serde_json::to_value(&self.heading_path)
                .map_err(|error| KioError::schema(error.to_string()))?,
        );
        value.insert("raw_hash".to_owned(), Value::from(self.raw_hash.clone()));
        if let Some(section_id) = self.section_id.as_ref().filter(|value| !value.is_empty()) {
            value.insert("section_id".to_owned(), Value::from(section_id.clone()));
        }
        value.insert("spec_version".to_owned(), Value::from(1));
        value.insert(
            "tool_profile_hash".to_owned(),
            Value::from(self.tool_profile_hash.clone()),
        );
        value.insert("unit_key".to_owned(), Value::from(self.unit_key.clone()));
        value.insert(
            "unit_content_hash".to_owned(),
            Value::from(self.unit_content_hash.clone()),
        );
        // Null / absent fields are omitted from the identity object.
        value.retain(|_, value| !value.is_null());
        hash_json(&Value::Object(value))
    }

    pub fn validate(&self) -> Result<()> {
        if self.spec_version != 1 {
            return Err(chunk_corrupt_error(
                "chunk spec_version must be exactly 1",
                None,
            ));
        }
        if !is_hash(&self.raw_hash)
            || !is_hash(&self.tool_profile_hash)
            || !is_hash(&self.unit_content_hash)
            || !is_hash(&self.text_hash)
        {
            return Err(chunk_corrupt_error(
                "chunk contains an invalid logical hash",
                None,
            ));
        }
        if self.unit_key.is_empty() {
            return Err(chunk_corrupt_error(
                "chunk unit_key must not be empty",
                None,
            ));
        }
        if self.byte_start > self.byte_end {
            return Err(chunk_corrupt_error(
                "chunk byte_start/byte_end must be an ordered pair",
                None,
            ));
        }
        if hash_bytes(self.text.as_bytes()) != self.text_hash {
            return Err(chunk_corrupt_error(
                "chunk text_hash does not match exact text",
                None,
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Raw,
    Tree,
    Commit,
}

impl ObjectKind {
    #[must_use]
    pub const fn directory(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Tree => "trees",
            Self::Commit => "commits",
        }
    }

    #[must_use]
    pub const fn object_type(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Tree => "tree",
            Self::Commit => "commit",
        }
    }

    #[must_use]
    pub const fn max_bytes(self) -> u64 {
        match self {
            Self::Raw => MAX_RAW_OBJECT_BYTES,
            Self::Tree => MAX_TREE_OBJECT_BYTES,
            Self::Commit => MAX_COMMIT_OBJECT_BYTES,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StoredObject {
    pub kind: ObjectKind,
    pub hash: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredObjectMetadata {
    pub kind: ObjectKind,
    pub hash: String,
    pub size_bytes: u64,
}

#[derive(Debug)]
pub struct AccountedReadError {
    pub error: KioError,
    pub consumed_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct ObjectStore {
    reads: CasReadAccess,
    control: crate::control::ControlHandle,
    kio_dir: Option<PathBuf>,
    /// Scheduled writers retain these handles before taking their writer
    /// boundary.  The ambient `.kio/objects` pathname is deliberately never
    /// consulted by the bound read/write subset below.
    bound: Option<BoundObjectDirs>,
}

#[derive(Debug, Clone)]
struct BoundObjectDirs {
    kio: Arc<File>,
    objects: Arc<File>,
    raw: Arc<File>,
    trees: Arc<File>,
    commits: Arc<File>,
    chunks: Arc<Mutex<Option<Arc<File>>>>,
    prepared: Arc<Mutex<Option<Arc<File>>>>,
    image: Arc<Mutex<Option<Arc<File>>>>,
    embeddings: Arc<Mutex<Option<Arc<File>>>>,
    manifests: Arc<Mutex<Option<Arc<File>>>>,
    toollocks: Arc<Mutex<Option<Arc<File>>>>,
    normalized_units: Arc<Mutex<Option<Arc<File>>>>,
}

#[derive(Debug)]
pub struct BoundRawStage {
    parent: Arc<File>,
    name: String,
    file: Option<File>,
    raw_hash: String,
    size_bytes: u64,
}
impl BoundRawStage {
    #[must_use]
    pub fn raw_hash(&self) -> &str {
        &self.raw_hash
    }
    #[must_use]
    pub const fn size_bytes(&self) -> u64 {
        self.size_bytes
    }
}
impl Drop for BoundRawStage {
    fn drop(&mut self) {
        if !self.name.is_empty() {
            let _ = bound_remove(&self.parent, &self.name);
        }
    }
}

/// Exact prepared/image victim captured for one confirmed maintenance plan.
#[derive(Debug)]
pub struct PlannedContentRemoval(crate::store_dir::PlannedFileRemoval);
impl PlannedContentRemoval {
    pub fn revalidate(&self) -> Result<bool> {
        self.0.revalidate()
    }
    pub fn remove(self) -> Result<bool> {
        self.0.remove()
    }
}

pub(crate) fn ensure_bound_directory(directory: &File, label: &Path) -> Result<()> {
    use cap_primitives::fs as cap_fs;

    let metadata = cap_fs::Metadata::from_file(directory).kio_io(label)?;
    if !metadata.is_dir() {
        return Err(KioError::io(
            "bound CAS directory changed type",
            label.display().to_string(),
        ));
    }
    cas_semantics::validate_bound_directory_native(directory, label)?;
    let retained = crate::store_dir::StoreDirectory::from_retained(
        directory.try_clone().kio_io(label)?,
        label.to_path_buf(),
    )?;
    crate::private_fs::verify_private_directory_handle(&retained)
}

fn same_bound_directory(current: &File, retained: &File) -> Result<bool> {
    ensure_bound_directory(current, Path::new("bound object layout"))?;
    ensure_bound_directory(retained, Path::new("retained object layout"))?;
    cas_semantics::same_bound_directory_native(current, retained)
}

fn bound_open_dir(parent: &File, leaf: &str) -> Result<File> {
    ensure_bound_directory(parent, Path::new("CAS parent"))?;
    let name = bound_leaf(leaf, "directory")?;
    let directory = cap_primitives::fs::open_dir_nofollow(parent, name)
        .map_err(|error| bound_io_or_not_found(error, leaf))?;
    ensure_bound_directory(&directory, Path::new(leaf))?;
    let directory = cas_semantics::normalize_bound_child(directory, leaf)?;
    Ok(directory)
}

fn bound_ingest_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(".ingest-") else {
        return false;
    };
    let mut fields = rest.split('-');
    (0..3).all(|_| {
        fields.next().is_some_and(|field| {
            !field.is_empty() && field.bytes().all(|byte| byte.is_ascii_digit())
        })
    }) && fields.next().is_none()
}

fn bound_stage_corrupt(message: &str) -> KioError {
    KioError::new(
        "KIO-E-STORE-CORRUPT-001",
        message,
        serde_json::json!({}),
        crate::ExitCode::PermanentFailure,
    )
}

fn bound_open_or_create_dir(parent: &File, leaf: &str, create: bool) -> Result<File> {
    match bound_open_dir(parent, leaf) {
        Ok(dir) => Ok(dir),
        Err(error) if create && error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => {
            let name = bound_leaf(leaf, "directory")?;
            ensure_bound_directory(parent, Path::new("CAS parent"))?;
            let retained = crate::store_dir::StoreDirectory::from_retained(
                parent.try_clone().kio_io(Path::new(leaf))?,
                PathBuf::from("CAS parent"),
            )?;
            match retained.create_directory(name) {
                Ok(directory) => {
                    ensure_bound_directory(&directory, Path::new(leaf))?;
                    cas_semantics::normalize_bound_child(directory, leaf)
                }
                Err(error)
                    if error.error_code() == "KIO-E-STORE-IO-001"
                        && error.context().get("io_error_kind").and_then(Value::as_str)
                            == Some("already_exists") =>
                {
                    bound_open_dir(parent, leaf)
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

fn bound_stream_regular(
    reads: &CasReadAccess,
    parent: &File,
    leaf: &str,
    max: u64,
    mut consume: impl FnMut(&[u8]) -> Result<()>,
) -> (Result<u64>, u64) {
    let mut consumed = 0_u64;
    let result = (|| -> Result<u64> {
        reads.charge(0)?;
        ensure_bound_directory(parent, Path::new("CAS parent"))?;
        use cap_primitives::fs as cap_fs;
        let name = bound_leaf(leaf, "object")?;
        let label = Path::new(leaf);
        let before = cap_fs::stat(parent, name, cap_fs::FollowSymlinks::No)
            .map_err(|error| bound_io_or_not_found(error, leaf))?;
        if !before.is_file() || !bound_metadata_single_link(&before) {
            return Err(non_regular_object_error(label));
        }
        let mut options = cap_fs::OpenOptions::new();
        options
            .read(true)
            ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
        cas_semantics::configure_bound_read(&mut options);
        let mut file = cap_fs::open(parent, name, &options)
            .map_err(|error| bound_io_or_not_found(error, leaf))?;
        crate::private_fs::verify_private_regular_file_handle(&file)?;
        let opened = cap_fs::Metadata::from_file(&file).kio_io(label)?;
        let named = cap_fs::stat(parent, name, cap_fs::FollowSymlinks::No)
            .map_err(|error| bound_io_or_not_found(error, leaf))?;
        if !opened.is_file()
            || !named.is_file()
            || !bound_metadata_single_link(&opened)
            || !bound_metadata_single_link(&named)
            || !same_bound_regular_entry(parent, name, &file, &before, &named)?
        {
            return Err(non_regular_object_error(label));
        }
        if opened.len() > max {
            return Err(KioError::new(
                "KIO-E-STORE-OBJECT-OVERSIZED-001",
                "CAS object exceeds its byte limit",
                serde_json::json!({"max_bytes":max,"actual_bytes":opened.len()}),
                crate::ExitCode::PermanentFailure,
            ));
        }
        let mut buffer = [0_u8; CAS_STREAM_BUFFER_BYTES];
        loop {
            let cap = max
                .saturating_sub(consumed)
                .saturating_add(1)
                .min(buffer.len() as u64) as usize;
            let count = file.read(&mut buffer[..cap]).kio_io(label)?;
            if count == 0 {
                break;
            }
            consumed = consumed
                .checked_add(count as u64)
                .ok_or_else(|| bound_stage_corrupt("CAS read byte count overflow"))?;
            reads.charge(count as u64)?;
            if consumed > max {
                return Err(KioError::new(
                    "KIO-E-STORE-OBJECT-OVERSIZED-001",
                    "CAS object exceeds its byte limit",
                    serde_json::json!({"max_bytes":max,"actual_bytes":consumed}),
                    crate::ExitCode::PermanentFailure,
                ));
            }
            consume(&buffer[..count])?;
        }
        crate::private_fs::verify_private_regular_file_handle(&file)?;
        let finished = cap_fs::Metadata::from_file(&file).kio_io(label)?;
        let named = cap_fs::stat(parent, name, cap_fs::FollowSymlinks::No)
            .map_err(|error| bound_io_or_not_found(error, leaf))?;
        if consumed != opened.len()
            || finished.len() != opened.len()
            || finished.modified().ok() != opened.modified().ok()
            || !finished.is_file()
            || !named.is_file()
            || !bound_metadata_single_link(&finished)
            || !bound_metadata_single_link(&named)
            || !same_bound_regular_entry(parent, name, &file, &opened, &named)?
        {
            return Err(corrupt_object_error(
                label,
                "CAS object changed while reading",
                "stable",
                None,
            ));
        }
        Ok(consumed)
    })();
    (result, consumed)
}

fn bound_read_regular(
    reads: &CasReadAccess,
    parent: &File,
    leaf: &str,
    max: u64,
) -> Result<(u64, Vec<u8>)> {
    let mut bytes = Vec::new();
    let (result, _) = bound_stream_regular(reads, parent, leaf, max, |part| {
        bytes
            .try_reserve(part.len())
            .map_err(|_| bound_stage_corrupt("cannot allocate bounded CAS body"))?;
        bytes.extend_from_slice(part);
        Ok(())
    });
    result.map(|size| (size, bytes))
}

fn bound_read_chunk_accounted(
    reads: &CasReadAccess,
    parent: &File,
    leaf: &str,
    hash: &str,
) -> std::result::Result<(ChunkObject, u64), AccountedReadError> {
    let mut bytes = Vec::new();
    let (result, consumed_bytes) =
        bound_stream_regular(reads, parent, leaf, MAX_CHUNK_OBJECT_BYTES, |part| {
            bytes
                .try_reserve(part.len())
                .map_err(|_| chunk_size_error(u64::MAX))?;
            bytes.extend_from_slice(part);
            Ok(())
        });
    result
        .and_then(|size| decode_chunk(&bytes, hash, Path::new(leaf)).map(|chunk| (chunk, size)))
        .map_err(|error| AccountedReadError {
            error,
            consumed_bytes,
        })
}

fn bound_read_embedding(
    reads: &CasReadAccess,
    parent: &File,
    leaf: &str,
    hash: &str,
) -> Result<EmbeddingObject> {
    let (_, bytes) = bound_read_regular(reads, parent, leaf, MAX_EMBEDDING_OBJECT_BYTES)?;
    decode_embedding(&bytes, hash, Path::new(leaf))
}

/// Semantic keys intentionally differ from a hash of serialized bytes. The
/// caller validates that relationship; immutable publication compares the
/// entire bounded object, including text/vector data outside the key.
fn bound_write_semantic(parent: &File, leaf: &str, bytes: &[u8], max: u64) -> Result<()> {
    let verify = || -> Result<()> {
        let (_, existing) = bound_read_regular(&CasReadAccess::Unmetered, parent, leaf, max)?;
        if existing != bytes {
            return Err(bound_stage_corrupt(
                "semantic CAS key already contains different bytes",
            ));
        }
        Ok(())
    };
    match verify() {
        Ok(()) => return Ok(()),
        Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => {}
        Err(error) => return Err(error),
    }
    let (temp, mut file) = bound_create_temp(parent)?;
    let result = (|| {
        file.write_all(bytes).kio_io(Path::new(&temp))?;
        file.sync_all().kio_io(Path::new(&temp))?;
        drop(file);
        match cap_primitives::fs::hard_link(parent, Path::new(&temp), parent, Path::new(leaf)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(KioError::io(error.to_string(), leaf)),
        }
        bound_remove(parent, &temp)?;
        sync_bound_directory(parent, Path::new(leaf))?;
        verify()
    })();
    if result.is_err() {
        let _ = bound_remove(parent, &temp);
    }
    result
}

fn bound_read_verified_accounted(
    reads: &CasReadAccess,
    parent: &File,
    leaf: &str,
    kind: ObjectKind,
    hash: &str,
    materialize: bool,
) -> (Result<(u64, Vec<u8>)>, u64) {
    let mut bytes = Vec::new();
    let mut hasher = Sha256::new();
    let (result, consumed) = bound_stream_regular(reads, parent, leaf, kind.max_bytes(), |part| {
        hasher.update(part);
        if materialize {
            bytes
                .try_reserve(part.len())
                .map_err(|_| bound_stage_corrupt("cannot allocate bounded CAS body"))?;
            bytes.extend_from_slice(part);
        }
        Ok(())
    });
    let result = result.and_then(|size| {
        let actual = format!("sha256:{}", lower_hex(&hasher.finalize()));
        if actual != hash {
            return Err(corrupt_object_error(
                Path::new(leaf),
                "CAS object hash does not match filename",
                hash,
                Some(&actual),
            ));
        }
        Ok((size, bytes))
    });
    (result, consumed)
}

fn bound_read_verified(
    reads: &CasReadAccess,
    parent: &File,
    leaf: &str,
    kind: ObjectKind,
    hash: &str,
    materialize: bool,
) -> Result<(u64, Vec<u8>)> {
    bound_read_verified_accounted(reads, parent, leaf, kind, hash, materialize).0
}

/// Content namespaces use the same retained-parent, no-follow and identity
/// checks as raw/tree/commit objects. The content byte hash is deliberately
/// checked by `bound_verify_content`; bounded semantic reads only require a
/// regular stable leaf, matching the public-path API contract.
fn bound_verify_content(
    reads: &CasReadAccess,
    parent: &File,
    leaf: &str,
    kind: ContentObjectKind,
    hash: &str,
) -> Result<u64> {
    bound_verify_content_accounted(reads, parent, leaf, kind, hash).0
}

fn bound_verify_content_accounted(
    reads: &CasReadAccess,
    parent: &File,
    leaf: &str,
    kind: ContentObjectKind,
    hash: &str,
) -> (Result<u64>, u64) {
    let mut hasher = Sha256::new();
    let (result, consumed) = bound_stream_regular(reads, parent, leaf, kind.max_bytes(), |part| {
        hasher.update(part);
        Ok(())
    });
    let result = result.and_then(|size| {
        let actual = format!("sha256:{}", lower_hex(&hasher.finalize()));
        if actual != hash {
            return Err(corrupt_object_error(
                Path::new(leaf),
                "content object hash does not match filename",
                hash,
                Some(&actual),
            ));
        }
        Ok(size)
    });
    (result, consumed)
}

fn bound_create_temp(parent: &File) -> Result<(String, File)> {
    bound_create_private_temp(parent, ".kio-cas", "bound CAS temp")
}

fn bound_create_ingest_temp(parent: &File) -> Result<(String, File)> {
    bound_create_private_temp(parent, ".ingest", "bound raw stage")
}

fn bound_remove(parent: &File, leaf: &str) -> Result<()> {
    cap_primitives::fs::remove_file(parent, Path::new(leaf))
        .map_err(|error| KioError::io(error.to_string(), leaf))
}

fn bound_open_regular_handle(parent: &File, leaf: &str) -> Result<File> {
    ensure_bound_directory(parent, Path::new("CAS parent"))?;
    use cap_primitives::fs as cap_fs;
    let name = bound_leaf(leaf, "object")?;
    let before = cap_fs::stat(parent, name, cap_fs::FollowSymlinks::No)
        .map_err(|error| bound_io_or_not_found(error, leaf))?;
    if !before.is_file() || !bound_metadata_single_link(&before) {
        return Err(non_regular_object_error(Path::new(leaf)));
    }
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    cas_semantics::configure_bound_read(&mut options);
    let file =
        cap_fs::open(parent, name, &options).map_err(|error| bound_io_or_not_found(error, leaf))?;
    let opened = cap_fs::Metadata::from_file(&file).kio_io(Path::new(leaf))?;
    let after = cap_fs::stat(parent, name, cap_fs::FollowSymlinks::No)
        .map_err(|error| bound_io_or_not_found(error, leaf))?;
    if !opened.is_file()
        || !after.is_file()
        || !bound_metadata_single_link(&opened)
        || !bound_metadata_single_link(&after)
        || !same_bound_regular_entry(parent, name, &file, &before, &after)?
    {
        return Err(non_regular_object_error(Path::new(leaf)));
    }
    crate::private_fs::verify_private_regular_file_handle(&file)?;
    Ok(file)
}

fn bound_hash_open_regular(reads: &CasReadAccess, file: &mut File, max: u64) -> Result<String> {
    reads.charge(0)?;
    crate::private_fs::verify_private_regular_file_handle(file)?;
    file.seek(std::io::SeekFrom::Start(0))
        .map_err(|error| KioError::io(error.to_string(), "bound CAS repair source"))?;
    let mut total = 0_u64;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; CAS_STREAM_BUFFER_BYTES];
    loop {
        let cap = max
            .saturating_sub(total)
            .saturating_add(1)
            .min(buffer.len() as u64) as usize;
        let count = file
            .read(&mut buffer[..cap])
            .map_err(|error| KioError::io(error.to_string(), "bound CAS repair source"))?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .ok_or_else(|| bound_stage_corrupt("raw repair byte count overflow"))?;
        if total > max {
            return Err(object_size_error(ObjectKind::Raw, max, total));
        }
        reads.charge(count as u64)?;
        hasher.update(&buffer[..count]);
    }
    Ok(format!("sha256:{}", lower_hex(&hasher.finalize())))
}

fn bound_quarantine_matches(parent: &File, quarantine: &str, source: &File) -> Result<bool> {
    use cap_primitives::fs as cap_fs;
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    cas_semantics::configure_bound_read(&mut options);
    let quarantined = cap_fs::open(parent, bound_leaf(quarantine, "quarantine")?, &options)
        .map_err(|error| bound_io_or_not_found(error, quarantine))?;
    cas_semantics::same_repair_quarantine(source, &quarantined, Path::new(quarantine))
}

fn bound_visit_chunks(
    reads: &CasReadAccess,
    base: &File,
    visit: &mut impl FnMut(&str, &ChunkObject) -> Result<()>,
) -> Result<()> {
    bound_visit_chunks_with_limits(reads, base, 100_000, 512 * 1024 * 1024, visit)
}

fn bound_visit_chunks_with_limits(
    reads: &CasReadAccess,
    base: &File,
    max_entries: usize,
    max_bytes: u64,
    visit: &mut impl FnMut(&str, &ChunkObject) -> Result<()>,
) -> Result<()> {
    ensure_bound_directory(base, Path::new("CAS inventory base"))?;
    let mut entries = 0_usize;
    let mut total_bytes = 0_u64;
    let first_level = cap_primitives::fs::read_base_dir(base)
        .map_err(|error| KioError::io(error.to_string(), "bound chunks directory"))?;
    for first in first_level {
        entries += 1;
        if entries > max_entries {
            return Err(bound_stage_corrupt("chunk inventory exceeds entry limit"));
        }
        let first =
            first.map_err(|error| KioError::io(error.to_string(), "bound chunks directory"))?;
        let first_name = first.file_name();
        let first_name = first_name
            .to_str()
            .ok_or_else(|| bound_stage_corrupt("chunk fanout name is not UTF-8"))?;
        if !canonical_digest_fragment(first_name, 2) {
            return Err(bound_stage_corrupt("chunk fanout directory is malformed"));
        }
        let first_dir = bound_open_dir(base, first_name)?;
        let second_level = cap_primitives::fs::read_base_dir(&first_dir)
            .map_err(|error| KioError::io(error.to_string(), first_name))?;
        for second in second_level {
            entries += 1;
            if entries > max_entries {
                return Err(bound_stage_corrupt("chunk inventory exceeds entry limit"));
            }
            let second = second.map_err(|error| KioError::io(error.to_string(), first_name))?;
            let second_name = second
                .file_name()
                .to_str()
                .ok_or_else(|| bound_stage_corrupt("chunk fanout name is not UTF-8"))?
                .to_owned();
            if !canonical_digest_fragment(&second_name, 2) {
                return Err(bound_stage_corrupt("chunk fanout directory is malformed"));
            }
            let second_dir = bound_open_dir(&first_dir, &second_name)?;
            let leaves = cap_primitives::fs::read_base_dir(&second_dir)
                .map_err(|error| KioError::io(error.to_string(), &second_name))?;
            for leaf in leaves {
                entries += 1;
                if entries > max_entries {
                    return Err(bound_stage_corrupt("chunk inventory exceeds entry limit"));
                }
                let leaf = leaf.map_err(|error| KioError::io(error.to_string(), &second_name))?;
                let leaf_name = leaf
                    .file_name()
                    .to_str()
                    .ok_or_else(|| bound_stage_corrupt("chunk leaf is not UTF-8"))?
                    .to_owned();
                let digest = format!("sha256:{leaf_name}");
                if !is_hash(&digest)
                    || !leaf_name.starts_with(first_name)
                    || !leaf_name[2..].starts_with(&second_name)
                {
                    return Err(bound_stage_corrupt(
                        "chunk leaf does not match canonical fanout",
                    ));
                }
                let remaining = max_bytes.saturating_sub(total_bytes);
                let (_, bytes) = bound_read_regular(
                    reads,
                    &second_dir,
                    &leaf_name,
                    MAX_CHUNK_OBJECT_BYTES.min(remaining),
                )?;
                total_bytes += bytes.len() as u64;
                let object = decode_chunk(&bytes, &digest, Path::new(&leaf_name))?;
                visit(&digest, &object)?;
            }
        }
    }
    Ok(())
}

fn bound_embedding_hashes(reads: &CasReadAccess, base: &File) -> Result<Vec<String>> {
    const MAX_ENTRIES: usize = 100_000;
    const MAX_BYTES: u64 = 512 * 1024 * 1024;
    ensure_bound_directory(base, Path::new("CAS inventory base"))?;
    let mut hashes = Vec::new();
    let mut total_bytes = 0_u64;
    let first_level = cap_primitives::fs::read_base_dir(base)
        .map_err(|error| KioError::io(error.to_string(), "bound embeddings directory"))?;
    for first in first_level {
        let first =
            first.map_err(|error| KioError::io(error.to_string(), "bound embeddings directory"))?;
        let first_name = first.file_name();
        let first_name = first_name
            .to_str()
            .ok_or_else(|| bound_stage_corrupt("embedding fanout name is not UTF-8"))?;
        if !canonical_digest_fragment(first_name, 2) {
            return Err(bound_stage_corrupt(
                "embedding fanout directory is malformed",
            ));
        }
        let first_dir = bound_open_dir(base, first_name)?;
        let second_level = cap_primitives::fs::read_base_dir(&first_dir)
            .map_err(|error| KioError::io(error.to_string(), first_name))?;
        for second in second_level {
            let second = second.map_err(|error| KioError::io(error.to_string(), first_name))?;
            let second_name = second
                .file_name()
                .to_str()
                .ok_or_else(|| bound_stage_corrupt("embedding fanout name is not UTF-8"))?
                .to_owned();
            if !canonical_digest_fragment(&second_name, 2) {
                return Err(bound_stage_corrupt(
                    "embedding fanout directory is malformed",
                ));
            }
            let second_dir = bound_open_dir(&first_dir, &second_name)?;
            let leaves = cap_primitives::fs::read_base_dir(&second_dir)
                .map_err(|error| KioError::io(error.to_string(), &second_name))?;
            for leaf in leaves {
                let leaf = leaf.map_err(|error| KioError::io(error.to_string(), &second_name))?;
                let leaf_name = leaf
                    .file_name()
                    .to_str()
                    .ok_or_else(|| bound_stage_corrupt("embedding leaf is not UTF-8"))?
                    .to_owned();
                let digest = format!("sha256:{leaf_name}");
                if !is_hash(&digest)
                    || !leaf_name.starts_with(first_name)
                    || !leaf_name[2..].starts_with(&second_name)
                {
                    return Err(bound_stage_corrupt(
                        "embedding leaf does not match canonical fanout",
                    ));
                }
                let (_, bytes) =
                    bound_read_regular(reads, &second_dir, &leaf_name, MAX_EMBEDDING_OBJECT_BYTES)?;
                total_bytes = total_bytes.checked_add(bytes.len() as u64).ok_or_else(|| {
                    bound_stage_corrupt("embedding inventory byte count overflow")
                })?;
                if total_bytes > MAX_BYTES || hashes.len() >= MAX_ENTRIES {
                    return Err(bound_stage_corrupt(
                        "embedding inventory exceeds bounded entry or byte limit",
                    ));
                }
                decode_embedding(&bytes, &digest, Path::new(&leaf_name))?;
                hashes.push(digest);
            }
        }
    }
    hashes.sort();
    Ok(hashes)
}

fn canonical_digest_fragment(value: &str, length: usize) -> bool {
    value.len() == length
        && value.bytes().all(|byte| {
            byte.is_ascii_digit() || (byte.is_ascii_lowercase() && byte.is_ascii_hexdigit())
        })
}

fn bound_publish(
    parent: &File,
    temp: &str,
    leaf: &str,
    kind: ObjectKind,
    hash: &str,
    size: u64,
    expected: Option<&[u8]>,
) -> Result<()> {
    bound_publish_between(parent, temp, parent, leaf, kind, hash, size, expected)
}

#[allow(clippy::too_many_arguments)]
fn bound_publish_between(
    from: &File,
    temp: &str,
    to: &File,
    leaf: &str,
    kind: ObjectKind,
    hash: &str,
    size: u64,
    expected: Option<&[u8]>,
) -> Result<()> {
    match cap_primitives::fs::hard_link(from, Path::new(temp), to, Path::new(leaf)) {
        Ok(()) => {
            bound_remove(from, temp)?;
            sync_bound_directory(to, Path::new(leaf))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let (_, existing) =
                bound_read_verified(&CasReadAccess::Unmetered, to, leaf, kind, hash, true)?;
            if existing.len() as u64 != size || expected.is_some_and(|bytes| bytes != existing) {
                return Err(corrupt_object_error(
                    Path::new(leaf),
                    "CAS object bytes do not match existing object",
                    hash,
                    Some(&hash_bytes(&existing)),
                ));
            }
            bound_remove(from, temp)?;
        }
        Err(error) => return Err(KioError::io(error.to_string(), leaf)),
    }
    let (actual_size, actual) =
        bound_read_verified(&CasReadAccess::Unmetered, to, leaf, kind, hash, true)?;
    if actual_size != size || expected.is_some_and(|bytes| bytes != actual) {
        return Err(corrupt_object_error(
            Path::new(leaf),
            "published CAS object changed",
            hash,
            Some(&hash_bytes(&actual)),
        ));
    }
    Ok(())
}

fn bound_publish_content(
    parent: &File,
    temp: &str,
    leaf: &str,
    kind: ContentObjectKind,
    hash: &str,
    expected: &[u8],
) -> Result<()> {
    match cap_primitives::fs::hard_link(parent, Path::new(temp), parent, Path::new(leaf)) {
        Ok(()) => {
            bound_remove(parent, temp)?;
            sync_bound_directory(parent, Path::new(leaf))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let (_, existing) =
                bound_read_regular(&CasReadAccess::Unmetered, parent, leaf, kind.max_bytes())?;
            if hash_bytes(&existing) != hash || existing != expected {
                return Err(corrupt_object_error(
                    Path::new(leaf),
                    "CAS content bytes do not match existing object",
                    hash,
                    Some(&hash_bytes(&existing)),
                ));
            }
            bound_remove(parent, temp)?;
        }
        Err(error) => return Err(KioError::io(error.to_string(), leaf)),
    }
    let (actual_size, actual) =
        bound_read_regular(&CasReadAccess::Unmetered, parent, leaf, kind.max_bytes())?;
    if actual_size != expected.len() as u64 || hash_bytes(&actual) != hash || actual != expected {
        return Err(corrupt_object_error(
            Path::new(leaf),
            "published CAS content changed",
            hash,
            Some(&hash_bytes(&actual)),
        ));
    }
    Ok(())
}

fn bound_leaf<'a>(leaf: &'a str, kind: &str) -> Result<&'a Path> {
    let path = Path::new(leaf);
    if !matches!(
        path.components().next(),
        Some(std::path::Component::Normal(_))
    ) || path.components().count() != 1
    {
        return Err(KioError::invalid_usage(format!(
            "invalid bound CAS {kind} name"
        )));
    }
    Ok(path)
}

pub(crate) fn bound_io_or_not_found(error: std::io::Error, leaf: &str) -> KioError {
    if error.kind() == std::io::ErrorKind::NotFound {
        KioError::not_found(leaf)
    } else {
        KioError::io(error.to_string(), leaf)
    }
}

fn bound_create_private_temp(parent: &File, prefix: &str, label: &str) -> Result<(String, File)> {
    for attempt in 0..128_u32 {
        let name = format!(
            "{prefix}-{}-{}-{attempt}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos()),
        );
        let path = bound_leaf(&name, "temporary leaf")?;
        let retained = crate::store_dir::StoreDirectory::from_retained(
            parent.try_clone().kio_io(Path::new(label))?,
            PathBuf::from(label),
        )?;
        crate::private_fs::verify_private_directory_handle(&retained)?;
        match retained.create_private_file(path) {
            Ok(file) => {
                crate::private_fs::verify_private_regular_file_handle(&file)?;
                return Ok((name, file));
            }
            Err(error)
                if error.error_code() == "KIO-E-STORE-IO-001"
                    && error.context().get("io_error_kind").and_then(Value::as_str)
                        == Some("already_exists") =>
            {
                continue;
            }
            Err(error) => return Err(error),
        }
    }
    Err(KioError::io(
        "unable to allocate private bound CAS temporary file",
        label,
    ))
}

fn read_verified_object(
    reads: &CasReadAccess,
    path: &Path,
    kind: ObjectKind,
    expected_hash: &str,
    materialize: bool,
) -> Result<(u64, Vec<u8>)> {
    read_verified_object_accounted(reads, path, kind, expected_hash, materialize).0
}

fn read_verified_object_accounted(
    reads: &CasReadAccess,
    path: &Path,
    kind: ObjectKind,
    expected_hash: &str,
    materialize: bool,
) -> (Result<(u64, Vec<u8>)>, u64) {
    let mut consumed = 0_u64;
    let result = (|| -> Result<(u64, Vec<u8>)> {
        reads.charge(0)?;
        let mut file = open_private_cas_file(path)?;
        let metadata = file.metadata().kio_io(path)?;
        let limit = kind.max_bytes();
        if metadata.len() > limit {
            return Err(object_size_error(kind, limit, metadata.len()));
        }

        let mut bytes = Vec::new();
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; CAS_STREAM_BUFFER_BYTES];
        loop {
            let read_cap = limit
                .saturating_sub(consumed)
                .saturating_add(1)
                .min(buffer.len() as u64) as usize;
            let count = file.read(&mut buffer[..read_cap]).kio_io(path)?;
            if count == 0 {
                break;
            }
            consumed = consumed
                .checked_add(count as u64)
                .ok_or_else(|| object_size_error(kind, limit, u64::MAX))?;
            reads.charge(count as u64)?;
            if consumed > limit {
                return Err(object_size_error(kind, limit, consumed));
            }
            hasher.update(&buffer[..count]);
            if materialize {
                bytes.extend_from_slice(&buffer[..count]);
            }
        }
        let actual = format!("sha256:{}", lower_hex(&hasher.finalize()));
        if actual != expected_hash {
            return Err(corrupt_object_error(
                path,
                "CAS object hash mismatch",
                expected_hash,
                Some(&actual),
            ));
        }
        Ok((consumed, bytes))
    })();
    (result, consumed)
}

fn verify_content_object_path(
    reads: &CasReadAccess,
    path: &Path,
    kind: ContentObjectKind,
    expected_hash: &str,
) -> Result<u64> {
    verify_content_object_path_accounted(reads, path, kind, expected_hash).0
}

fn verify_content_object_path_accounted(
    reads: &CasReadAccess,
    path: &Path,
    kind: ContentObjectKind,
    expected_hash: &str,
) -> (Result<u64>, u64) {
    let mut consumed = 0_u64;
    let result = (|| -> Result<u64> {
        reads.charge(0)?;
        let mut file = open_private_cas_file(path)?;
        let metadata = file.metadata().kio_io(path)?;
        let limit = kind.max_bytes();
        if metadata.len() > limit {
            return Err(content_object_size_error(kind, limit, metadata.len()));
        }
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; CAS_STREAM_BUFFER_BYTES];
        loop {
            let read_cap = limit
                .saturating_sub(consumed)
                .saturating_add(1)
                .min(buffer.len() as u64) as usize;
            let count = file.read(&mut buffer[..read_cap]).kio_io(path)?;
            if count == 0 {
                break;
            }
            consumed = consumed.saturating_add(count as u64);
            reads.charge(count as u64)?;
            if consumed > limit {
                return Err(KioError::new(
                    "KIO-E-STORE-OBJECT-OVERSIZED-001",
                    "content object exceeds its byte limit",
                    serde_json::json!({
                        "object_type": kind.object_type(),
                        "max_bytes": limit,
                        "actual_bytes": consumed,
                    }),
                    crate::ExitCode::PermanentFailure,
                ));
            }
            hasher.update(&buffer[..count]);
        }
        let actual = format!("sha256:{}", lower_hex(&hasher.finalize()));
        if actual != expected_hash {
            return Err(corrupt_object_error(
                path,
                "content object hash mismatch",
                expected_hash,
                Some(&actual),
            ));
        }
        Ok(consumed)
    })();
    (result, consumed)
}

fn content_object_size_error(
    kind: ContentObjectKind,
    max_bytes: u64,
    actual_bytes: u64,
) -> KioError {
    KioError::new(
        "KIO-E-STORE-OBJECT-OVERSIZED-001",
        "content object exceeds its byte limit",
        serde_json::json!({
            "object_type": kind.object_type(),
            "max_bytes": max_bytes,
            "actual_bytes": actual_bytes,
        }),
        crate::ExitCode::PermanentFailure,
    )
}

fn copy_verified_object<W: Write>(
    reads: &CasReadAccess,
    path: &Path,
    kind: ObjectKind,
    expected_hash: &str,
    writer: &mut W,
) -> Result<u64> {
    reads.charge(0)?;
    let mut file = open_private_cas_file(path)?;
    let metadata = file.metadata().kio_io(path)?;
    let limit = kind.max_bytes();
    if metadata.len() > limit {
        return Err(object_size_error(kind, limit, metadata.len()));
    }

    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; CAS_STREAM_BUFFER_BYTES];
    loop {
        let read_cap = limit
            .saturating_sub(total)
            .saturating_add(1)
            .min(buffer.len() as u64) as usize;
        let count = file.read(&mut buffer[..read_cap]).kio_io(path)?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .ok_or_else(|| object_size_error(kind, limit, u64::MAX))?;
        reads.charge(count as u64)?;
        if total > limit {
            return Err(object_size_error(kind, limit, total));
        }
        hasher.update(&buffer[..count]);
        writer
            .write_all(&buffer[..count])
            .map_err(|error| KioError::io(error.to_string(), "CAS stream target"))?;
    }
    let actual = format!("sha256:{}", lower_hex(&hasher.finalize()));
    if actual != expected_hash {
        return Err(corrupt_object_error(
            path,
            "CAS object hash mismatch",
            expected_hash,
            Some(&actual),
        ));
    }
    Ok(total)
}

fn verify_existing_bytes(path: &Path, expected_hash: &str, expected: &[u8]) -> Result<()> {
    let mut file = open_private_cas_file(path)?;
    let metadata = file.metadata().kio_io(path)?;
    if metadata.len() != expected.len() as u64 {
        return Err(corrupt_object_error(
            path,
            "existing CAS object does not match expected bytes",
            expected_hash,
            None,
        ));
    }
    let mut offset = 0_usize;
    let mut buffer = [0_u8; CAS_STREAM_BUFFER_BYTES];
    loop {
        let count = file.read(&mut buffer).kio_io(path)?;
        if count == 0 {
            break;
        }
        let end = offset.checked_add(count).ok_or_else(|| {
            corrupt_object_error(
                path,
                "existing CAS object length overflow",
                expected_hash,
                None,
            )
        })?;
        if expected.get(offset..end) != Some(&buffer[..count]) {
            return Err(corrupt_object_error(
                path,
                "existing CAS object does not match expected bytes",
                expected_hash,
                None,
            ));
        }
        offset = end;
    }
    if offset != expected.len() {
        return Err(corrupt_object_error(
            path,
            "existing CAS object does not match expected bytes",
            expected_hash,
            None,
        ));
    }
    Ok(())
}

fn verify_named_cas_directory(
    directory: &crate::store_dir::StoreDirectory,
    path: &Path,
) -> Result<()> {
    crate::private_fs::verify_private_directory_handle(directory)?;
    let named = crate::platform::real_directory_identity(path).kio_io(path)?;
    if named != crate::platform::directory_handle_identity(&directory.root_handle()) {
        return Err(bound_stage_corrupt("CAS parent changed during publication"));
    }
    Ok(())
}

fn remove_private_temp(directory: &crate::store_dir::StoreDirectory, path: &Path) -> Result<()> {
    let leaf = path
        .file_name()
        .and_then(|leaf| leaf.to_str())
        .ok_or_else(|| KioError::invalid_usage("CAS temp has no UTF-8 leaf"))?;
    // Publication temporarily has two names for this private staged inode.
    // Removing its owned staging name is distinct from admitting a CAS leaf.
    bound_remove(&directory.root_handle(), leaf)
}

fn publish_temp_object(
    source: &crate::store_dir::StoreDirectory,
    temp_path: &Path,
    destination: &Path,
    expected_hash: &str,
    expected_len: u64,
    expected_bytes: Option<&[u8]>,
) -> Result<()> {
    let source_path = temp_path
        .parent()
        .ok_or_else(|| KioError::invalid_usage("CAS temp has no parent"))?;
    let destination_path = destination
        .parent()
        .ok_or_else(|| KioError::invalid_usage("CAS object has no parent"))?;
    verify_named_cas_directory(source, source_path)?;
    let target = crate::store_dir::StoreDirectory::open(destination_path)?;
    crate::private_fs::verify_private_directory_handle(&target)?;
    verify_named_cas_directory(source, source_path)?;
    let temp_leaf = temp_path
        .file_name()
        .ok_or_else(|| KioError::invalid_usage("CAS temp has no leaf"))?;
    let leaf = destination
        .file_name()
        .ok_or_else(|| KioError::invalid_usage("CAS object has no leaf"))?;
    match cap_primitives::fs::hard_link(
        &source.root_handle(),
        Path::new(temp_leaf),
        &target.root_handle(),
        Path::new(leaf),
    ) {
        Ok(()) => remove_private_temp(source, temp_path)?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            match expected_bytes {
                Some(bytes) => verify_existing_bytes(destination, expected_hash, bytes)?,
                None => verify_existing_matches_file(
                    destination,
                    temp_path,
                    expected_hash,
                    expected_len,
                )?,
            }
            remove_private_temp(source, temp_path)?;
        }
        Err(error) => {
            return Err(KioError::io(
                error.to_string(),
                destination.display().to_string(),
            ));
        }
    }
    source.sync()?;
    target.sync()?;
    verify_named_cas_directory(source, source_path)?;
    verify_named_cas_directory(&target, destination_path)
}

fn create_repair_quarantine(path: &Path, opened: &File) -> Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| KioError::io("CAS path has no parent", path.display().to_string()))?;
    for attempt in 0..32_u8 {
        let candidate = parent.join(format!(
            ".repair-quarantine-{}-{}-{attempt}",
            std::process::id(),
            unix_nanos()
        ));
        match fs::hard_link(path, &candidate) {
            Ok(()) => {
                let mut options = OpenOptions::new();
                options.read(true);
                configure_no_follow(&mut options);
                let linked = options.open(&candidate).kio_io(&candidate)?;
                if !same_open_file(opened, &linked)? {
                    let _ = fs::remove_file(&candidate);
                    return Err(non_regular_object_error(path));
                }
                return Ok(candidate);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(KioError::io(error.to_string(), path.display().to_string())),
        }
    }
    Err(KioError::io(
        "could not allocate raw repair quarantine",
        path.display().to_string(),
    ))
}

fn verify_existing_matches_file(
    destination: &Path,
    source: &Path,
    expected_hash: &str,
    expected_len: u64,
) -> Result<()> {
    let mut existing = open_private_cas_file(destination)?;
    if existing.metadata().kio_io(destination)?.len() != expected_len {
        return Err(corrupt_object_error(
            destination,
            "existing CAS object does not match expected bytes",
            expected_hash,
            None,
        ));
    }
    let mut source_file = open_private_cas_file(source)?;
    let mut existing_buffer = [0_u8; CAS_STREAM_BUFFER_BYTES];
    let mut source_buffer = [0_u8; CAS_STREAM_BUFFER_BYTES];
    loop {
        let existing_count = existing.read(&mut existing_buffer).kio_io(destination)?;
        let source_count = source_file.read(&mut source_buffer).kio_io(source)?;
        if existing_count != source_count
            || existing_buffer[..existing_count] != source_buffer[..source_count]
        {
            return Err(corrupt_object_error(
                destination,
                "existing CAS object does not match expected bytes",
                expected_hash,
                None,
            ));
        }
        if existing_count == 0 {
            return Ok(());
        }
    }
}

/// Read and fully verify the embedding object at one exact path. Path-scoped
/// (rather than hash-scoped) so callers can verify an exact candidate without
/// deriving a canonical pathname from its hash.
fn read_embedding_path(
    reads: &CasReadAccess,
    path: &Path,
    expected_hash: &str,
) -> Result<EmbeddingObject> {
    let bytes = read_bounded_regular_file_inner(path, MAX_EMBEDDING_OBJECT_BYTES, reads, true)?;
    decode_embedding(&bytes, expected_hash, path)
}

fn decode_embedding(bytes: &[u8], expected_hash: &str, path: &Path) -> Result<EmbeddingObject> {
    let object = EmbeddingObject::from_bytes(bytes)
        .map_err(|error| embedding_corrupt_error(error.message(), Some(path)))?;
    if object.to_bytes()? != bytes {
        return Err(embedding_corrupt_error(
            "embedding object bytes are not canonical",
            Some(path),
        ));
    }
    if object.identity_hash()? != expected_hash {
        return Err(embedding_corrupt_error(
            "embedding object identity does not match its storage key",
            Some(path),
        ));
    }
    Ok(object)
}

fn read_chunk_path(
    reads: &CasReadAccess,
    path: &Path,
    expected_hash: &str,
) -> Result<(ChunkObject, Vec<u8>)> {
    read_chunk_path_accounted(reads, path, expected_hash).0
}

fn read_chunk_path_accounted(
    reads: &CasReadAccess,
    path: &Path,
    expected_hash: &str,
) -> (Result<(ChunkObject, Vec<u8>)>, u64) {
    let mut consumed = 0_u64;
    let result = (|| -> Result<(ChunkObject, Vec<u8>)> {
        reads.charge(0)?;
        let mut file = open_private_cas_file(path)?;
        let metadata = file.metadata().kio_io(path)?;
        if metadata.len() > MAX_CHUNK_OBJECT_BYTES {
            return Err(chunk_size_error(metadata.len()));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        let mut buffer = [0_u8; CAS_STREAM_BUFFER_BYTES];
        loop {
            let remaining = MAX_CHUNK_OBJECT_BYTES
                .saturating_add(1)
                .saturating_sub(bytes.len() as u64);
            if remaining == 0 {
                break;
            }
            let read_cap = remaining.min(CAS_STREAM_BUFFER_BYTES as u64) as usize;
            let count = file.read(&mut buffer[..read_cap]).kio_io(path)?;
            if count == 0 {
                break;
            }
            consumed = consumed.saturating_add(count as u64);
            reads.charge(count as u64)?;
            bytes.extend_from_slice(&buffer[..count]);
        }
        consumed = bytes.len() as u64;
        if consumed > MAX_CHUNK_OBJECT_BYTES {
            return Err(chunk_size_error(consumed));
        }
        let object = decode_chunk(&bytes, expected_hash, path)?;
        Ok((object, bytes))
    })();
    (result, consumed)
}

fn decode_chunk(bytes: &[u8], expected_hash: &str, path: &Path) -> Result<ChunkObject> {
    let object: ChunkObject = serde_json::from_slice(bytes)
        .map_err(|_| chunk_corrupt_error("chunk object schema is invalid", Some(path)))?;
    object
        .validate()
        .map_err(|_| chunk_corrupt_error("chunk object semantics are invalid", Some(path)))?;
    if object.identity_hash()? != expected_hash {
        return Err(chunk_corrupt_error(
            "chunk semantic identity does not match its fan-out key",
            Some(path),
        ));
    }
    let canonical = canonical_json_bytes(
        &serde_json::to_value(&object).map_err(|error| KioError::schema(error.to_string()))?,
    )?;
    if canonical != bytes {
        return Err(chunk_corrupt_error(
            "chunk object is not canonical JSON",
            Some(path),
        ));
    }
    Ok(object)
}

fn create_private_temp(parent: &Path) -> Result<(PathBuf, File, crate::store_dir::StoreDirectory)> {
    let directory = crate::store_dir::StoreDirectory::open(parent)?;
    crate::private_fs::verify_private_directory_handle(&directory)?;
    let (name, file) =
        bound_create_private_temp(&directory.root_handle(), ".tmp", "CAS temporary parent")?;
    Ok((parent.join(name), file, directory))
}

/// Require a real directory at `path`, optionally creating that one missing
/// component.  Callers creating a nested path must validate every component
/// independently; `create_dir_all` would otherwise follow a hostile link.
pub fn ensure_real_directory(path: &Path, create: bool) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            let directory = crate::store_dir::StoreDirectory::open(path)?;
            crate::private_fs::verify_private_directory_handle(&directory)
        }
        Ok(_) => Err(non_regular_object_error(path)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
            let parent = path
                .parent()
                .ok_or_else(|| KioError::invalid_usage("CAS directory has no parent"))?;
            let leaf = path
                .file_name()
                .ok_or_else(|| KioError::invalid_usage("CAS directory has no leaf"))?;
            let directory = crate::store_dir::StoreDirectory::open(parent)?;
            crate::private_fs::verify_private_directory_handle(&directory)?;
            match directory.create_directory(Path::new(leaf)) {
                Ok(created) => ensure_bound_directory(&created, path),
                Err(error)
                    if error.error_code() == "KIO-E-STORE-IO-001"
                        && error.context().get("io_error_kind").and_then(Value::as_str)
                            == Some("already_exists") =>
                {
                    ensure_real_directory(path, false)
                }
                Err(error) => Err(error),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(KioError::not_found(path.display().to_string()))
        }
        Err(error) => Err(KioError::io(error.to_string(), path.display().to_string())),
    }
}

fn open_private_cas_file(path: &Path) -> Result<File> {
    let parent = path
        .parent()
        .ok_or_else(|| KioError::invalid_usage("CAS object has no parent"))?;
    let leaf = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| KioError::invalid_usage("CAS object has no UTF-8 leaf"))?;
    let directory = crate::store_dir::StoreDirectory::open(parent)?;
    crate::private_fs::verify_private_directory_handle(&directory)?;
    bound_open_regular_handle(&directory.root_handle(), leaf)
}

/// Open a real, single-link regular file without following its final symlink
/// or reparse point, and bind the returned handle to the verified path entry.
pub fn open_regular_nofollow(path: &Path) -> Result<File> {
    crate::platform::cas_io::open_regular_nofollow(path)
}

/// Open an existing metadata file for reading and mutation through the same
/// no-follow, single-link, identity-checked boundary as [`open_regular_nofollow`].
pub fn open_regular_nofollow_read_write(path: &Path) -> Result<File> {
    crate::platform::cas_io::open_regular_nofollow_read_write(path)
}

/// Open an existing ledger for append, or atomically create a new real regular
/// single-link ledger without following its final symlink or reparse point.
pub fn open_or_create_regular_nofollow_append(path: &Path) -> Result<File> {
    crate::platform::cas_io::open_or_create_regular_nofollow_append(path)
}

fn validate_open_regular_nofollow(path: &Path, file: &File) -> Result<()> {
    crate::platform::cas_io::validate_open_regular_nofollow(path, file)
}

fn corrupt_object_error(
    path: &Path,
    message: &str,
    expected: &str,
    actual: Option<&str>,
) -> KioError {
    KioError::new(
        "KIO-E-STORE-CORRUPT-001",
        message,
        serde_json::json!({
            "path": path,
            "expected": expected,
            "actual": actual,
        }),
        crate::ExitCode::PermanentFailure,
    )
}

fn chunk_corrupt_error(message: &str, path: Option<&Path>) -> KioError {
    KioError::new(
        "KIO-E-STORE-CORRUPT-001",
        message,
        serde_json::json!({ "path": path }),
        crate::ExitCode::PermanentFailure,
    )
}

fn embedding_corrupt_error(message: &str, path: Option<&Path>) -> KioError {
    KioError::new(
        "KIO-E-STORE-CORRUPT-001",
        message,
        serde_json::json!({ "path": path }),
        crate::ExitCode::PermanentFailure,
    )
}

/// f32 little-endian, the same representation the `embeddings` BLOB and the
/// replica use — so a vector round-trips between object, table and cache
/// without a conversion step anyone could get wrong.
fn vector_to_le_bytes(vector: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vector.len() * 4);
    for component in vector {
        out.extend_from_slice(&component.to_le_bytes());
    }
    out
}

fn vector_from_le_bytes(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|word| f32::from_le_bytes(*word))
        .collect()
}

/// Standard base64 with padding. Hand-rolled rather than pulled in as a
/// dependency: this is the only base64 in `kio-core`, and the alphabet is part
/// of a frozen on-disk format (03 §8.1) that should not move with a crate.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let bits = (u32::from(group[0]) << 16)
            | (u32::from(group.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(group.get(2).copied().unwrap_or(0));
        out.push(ALPHABET[(bits >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(bits >> 12) as usize & 0x3f] as char);
        out.push(if group.len() > 1 {
            ALPHABET[(bits >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if group.len() > 2 {
            ALPHABET[bits as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

fn base64_decode(text: &str) -> Result<Vec<u8>> {
    let decode = |byte: u8| -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some(u32::from(byte - b'A')),
            b'a'..=b'z' => Some(u32::from(byte - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(byte - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err(embedding_corrupt_error(
            "embedding vector is not padded base64",
            None,
        ));
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for group in bytes.chunks(4) {
        let padding = group.iter().rev().take_while(|byte| **byte == b'=').count();
        if padding > 2 || (padding > 0 && !std::ptr::eq(group, &bytes[bytes.len() - 4..])) {
            return Err(embedding_corrupt_error(
                "embedding vector has base64 padding before its final group",
                None,
            ));
        }
        let mut bits = 0u32;
        for (index, byte) in group.iter().enumerate() {
            let sextet = if *byte == b'=' {
                0
            } else {
                decode(*byte).ok_or_else(|| {
                    embedding_corrupt_error("embedding vector holds a non-base64 byte", None)
                })?
            };
            bits |= sextet << (18 - 6 * index);
        }
        out.push((bits >> 16) as u8);
        if padding < 2 {
            out.push((bits >> 8) as u8);
        }
        if padding < 1 {
            out.push(bits as u8);
        }
    }
    Ok(out)
}

fn chunk_size_error(actual: u64) -> KioError {
    KioError::new(
        "KIO-E-STORE-OBJECT-OVERSIZED-001",
        "chunk object exceeds its byte limit",
        serde_json::json!({
            "object_type": "chunk",
            "max_bytes": MAX_CHUNK_OBJECT_BYTES,
            "actual_bytes": actual,
        }),
        crate::ExitCode::PermanentFailure,
    )
}

pub(crate) fn non_regular_object_error(path: &Path) -> KioError {
    KioError::new(
        "KIO-E-STORE-CORRUPT-001",
        "CAS path is not a real regular file or directory",
        serde_json::json!({ "path": path }),
        crate::ExitCode::PermanentFailure,
    )
}

fn object_size_error(kind: ObjectKind, limit: u64, actual: u64) -> KioError {
    KioError::new(
        "KIO-E-STORE-OBJECT-OVERSIZED-001",
        "CAS object exceeds its byte limit",
        serde_json::json!({
            "object_type": kind.object_type(),
            "max_bytes": limit,
            "actual_bytes": actual,
        }),
        crate::ExitCode::PermanentFailure,
    )
}

#[must_use]
pub fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{}", lower_hex(&digest))
}

pub fn hash_json(value: &Value) -> Result<String> {
    canonical_json_bytes(value).map(|bytes| hash_bytes(&bytes))
}

/// Serialize `value` to RFC 8785 (JCS) canonical JSON bytes.
///
/// Backed by the `serde_jcs` crate so the byte output matches RFC 8785
/// exactly (the object hash contract in `docs/03-data-model.md` §8.1).
pub fn canonical_json_bytes(value: &Value) -> Result<Vec<u8>> {
    serde_jcs::to_vec(value).map_err(|err| KioError::schema(err.to_string()))
}

#[must_use]
pub fn is_hash(value: &str) -> bool {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return false;
    };
    hex.len() == 64
        && hex
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Map a logical `sha256:<hex>` identifier to its portable digest-only leaf.
pub fn hash_path_component(hash: &str) -> Result<&str> {
    if !is_hash(hash) {
        return Err(KioError::invalid_usage("invalid hash"));
    }
    Ok(&hash["sha256:".len()..])
}

pub fn fanout_path(base: impl AsRef<Path>, hash: &str) -> Result<PathBuf> {
    let digest = hash_path_component(hash)?;
    Ok(base
        .as_ref()
        .join(&digest[0..2])
        .join(&digest[2..4])
        .join(digest))
}

/// Read a non-CAS metadata record through the same no-follow/single-link boundary
/// as CAS objects while enforcing a caller-selected byte limit.
pub fn read_bounded_regular_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    read_bounded_regular_file_inner(path, max_bytes, &CasReadAccess::Unmetered, false)
}

/// Shared implementation for bounded metadata records and bounded CAS content
/// records. CAS callers charge each physical buffer before continuing; general
/// metadata reads deliberately do not participate in the CAS-only budget.
fn read_bounded_regular_file_inner(
    path: &Path,
    max_bytes: u64,
    reads: &CasReadAccess,
    private_cas: bool,
) -> Result<Vec<u8>> {
    reads.charge(0)?;
    let mut file = if private_cas {
        open_private_cas_file(path)?
    } else {
        open_regular_nofollow(path)?
    };
    let metadata = file.metadata().kio_io(path)?;
    let initial_stability = observe_regular_file_stability(&metadata, path)?;
    if metadata.len() > max_bytes {
        return Err(KioError::new(
            "KIO-E-STORE-OBJECT-OVERSIZED-001",
            "metadata record exceeds its byte limit",
            serde_json::json!({
                "path": path,
                "max_bytes": max_bytes,
                "actual_bytes": metadata.len(),
            }),
            crate::ExitCode::PermanentFailure,
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    let mut buffer = [0_u8; CAS_STREAM_BUFFER_BYTES];
    loop {
        let remaining = max_bytes
            .saturating_add(1)
            .saturating_sub(bytes.len() as u64);
        if remaining == 0 {
            break;
        }
        let read_cap = remaining.min(CAS_STREAM_BUFFER_BYTES as u64) as usize;
        let count = file.read(&mut buffer[..read_cap]).kio_io(path)?;
        if count == 0 {
            break;
        }
        reads.charge(count as u64)?;
        bytes.extend_from_slice(&buffer[..count]);
    }
    if bytes.len() as u64 > max_bytes {
        return Err(KioError::new(
            "KIO-E-STORE-OBJECT-OVERSIZED-001",
            "metadata record exceeds its byte limit",
            serde_json::json!({ "path": path, "max_bytes": max_bytes }),
            crate::ExitCode::PermanentFailure,
        ));
    }
    // The opened descriptor remains the authority for the bytes we consumed.
    // Rebind it to the path only after the read, then reject a replacement or
    // in-place length change before returning any parsed metadata to a caller.
    validate_open_regular_nofollow(path, &file)?;
    if private_cas {
        crate::private_fs::verify_private_regular_file_handle(&file)?;
    }
    let final_metadata = file.metadata().kio_io(path)?;
    let final_stability = observe_regular_file_stability(&final_metadata, path)?;
    if final_stability != initial_stability || final_metadata.len() != bytes.len() as u64 {
        return Err(KioError::new(
            "KIO-E-STORE-CORRUPT-001",
            "metadata record changed while reading",
            serde_json::json!({
                "path": path,
                "initial_bytes": metadata.len(),
                "final_bytes": final_metadata.len(),
                "read_bytes": bytes.len(),
            }),
            crate::ExitCode::PermanentFailure,
        ));
    }
    Ok(bytes)
}

/// `Some(path)` when the CAS slot is occupied by anything at all — a symlink or
/// a directory counts, so the per-kind verification below is what rejects it,
/// not a silent "not found".
fn occupied_slot(path: PathBuf) -> Result<Option<PathBuf>> {
    match fs::symlink_metadata(&path) {
        Ok(_) => Ok(Some(path)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(KioError::io(error.to_string(), path.display().to_string())),
    }
}

/// Admit the owned log directory and any existing live leaf before rotation,
/// locking, or append. Missing directories are created privately; existing
/// unsafe directories and files are never restricted or adopted.
pub(crate) fn private_log_parent(path: &Path) -> Result<crate::store_dir::StoreDirectory> {
    let parent = path
        .parent()
        .ok_or_else(|| KioError::io("path has no parent", path.display().to_string()))?;
    let leaf = path
        .file_name()
        .ok_or_else(|| KioError::io("path has no leaf", path.display().to_string()))?;
    let directory = crate::store_dir::StoreDirectory::open_or_create_directories(parent)?;
    crate::private_fs::verify_private_directory_handle(&directory)?;
    if directory.contains_entry(Path::new(leaf))? {
        directory.ensure_owner_private(Path::new(leaf))?;
    }
    Ok(directory)
}

#[cfg(test)]
fn append_jsonl(path: &Path, value: &Value) -> Result<()> {
    let directory = private_log_parent(path)?;
    let leaf = path
        .file_name()
        .ok_or_else(|| KioError::io("path has no leaf", path.display().to_string()))?;
    append_jsonl_at(&directory, Path::new(leaf), value)
}

pub(crate) fn append_jsonl_at(
    directory: &crate::store_dir::StoreDirectory,
    leaf: &Path,
    value: &Value,
) -> Result<()> {
    let path = directory.path().join(leaf);
    // Frame the whole record before one O_APPEND write, preserving existing
    // serialization and avoiding a separate newline write between records.
    let mut line = serde_json::to_string(value)
        .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
    line.push('\n');
    directory.append_private(leaf, line.as_bytes())?;
    let named =
        crate::platform::real_directory_identity(directory.path()).kio_io(directory.path())?;
    if named != crate::platform::directory_handle_identity(&directory.root_handle()) {
        return Err(KioError::io(
            "log parent changed during append",
            path.display().to_string(),
        ));
    }
    Ok(())
}

/// Encode bytes as canonical lowercase hexadecimal text.
#[must_use]
pub fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn jsonl_append_creates_private_owned_logs_below_external_ancestors() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        #[cfg(unix)]
        let external_before = {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
            fs::metadata(&root).unwrap().permissions().mode()
        };
        #[cfg(windows)]
        let external_acl = || {
            let output = std::process::Command::new("icacls.exe")
                .arg(&root)
                .output()
                .unwrap();
            assert!(output.status.success());
            output.stdout
        };
        #[cfg(windows)]
        let external_before = external_acl();
        let log = root.join("device/kio/logs/events.jsonl");
        append_jsonl(&log, &serde_json::json!({"event": 1})).unwrap();
        append_jsonl(&log, &serde_json::json!({"event": 2})).unwrap();
        for relative in ["device", "device/kio", "device/kio/logs"] {
            crate::private_fs::verify_private_directory(&root.join(relative)).unwrap();
        }
        let bytes = crate::private_fs::read_private_file(&log, 1024).unwrap();
        assert_eq!(bytes, b"{\"event\":1}\n{\"event\":2}\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&root).unwrap().permissions().mode(),
                external_before
            );
        }
        #[cfg(windows)]
        assert_eq!(external_acl(), external_before);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn jsonl_append_accepts_platform_var_alias_but_refuses_owned_symlink() {
        use std::os::unix::fs::symlink;
        let fixture = tempfile::tempdir_in("/private/var/tmp").unwrap();
        let root = fixture.path().canonicalize().unwrap();
        let alias = Path::new("/var").join(root.strip_prefix("/private/var").unwrap());
        let log = alias.join("logs/events.jsonl");
        append_jsonl(&log, &serde_json::json!({"platform_alias": true})).unwrap();
        let opened = crate::store_dir::StoreDirectory::open(&alias.join("logs")).unwrap();
        opened
            .ensure_owner_private(Path::new("events.jsonl"))
            .unwrap();
        assert_eq!(
            crate::private_fs::read_private_file(&root.join("logs/events.jsonl"), 1024).unwrap(),
            b"{\"platform_alias\":true}\n"
        );
        symlink(root.join("logs"), root.join("linked-logs")).unwrap();
        assert!(
            append_jsonl(
                &alias.join("linked-logs/other.jsonl"),
                &serde_json::json!({})
            )
            .is_err()
        );
        assert!(!root.join("logs/other.jsonl").exists());
    }

    #[test]
    fn jsonl_append_rejects_existing_unsafe_parent_without_adoption() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        let parent = root.join("logs");
        fs::create_dir(&parent).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        }
        #[cfg(windows)]
        {
            let status = std::process::Command::new("icacls.exe")
                .arg(&parent)
                .args(["/grant", "*S-1-1-0:(OI)(CI)F"])
                .status()
                .unwrap();
            assert!(status.success());
        }
        let identity = crate::platform::real_directory_identity(&parent).unwrap();
        #[cfg(unix)]
        let before = {
            use std::os::unix::fs::PermissionsExt;
            fs::metadata(&parent).unwrap().permissions().mode()
        };
        #[cfg(windows)]
        let parent_acl = || {
            std::process::Command::new("icacls.exe")
                .arg(&parent)
                .output()
                .unwrap()
                .stdout
        };
        #[cfg(windows)]
        let before = parent_acl();
        let log = parent.join("events.jsonl");
        assert!(append_jsonl(&log, &serde_json::json!({"must": "fail"})).is_err());
        assert!(!log.exists());
        assert_eq!(fs::read_dir(&parent).unwrap().count(), 0);
        assert_eq!(
            crate::platform::real_directory_identity(&parent).unwrap(),
            identity
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&parent).unwrap().permissions().mode(), before);
        }
        #[cfg(windows)]
        assert_eq!(parent_acl(), before);
        assert!(crate::private_fs::verify_private_directory(&parent).is_err());
    }

    #[test]
    fn jsonl_append_rejects_existing_unsafe_leaf_without_adoption() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        let external = crate::store_dir::StoreDirectory::open(&root).unwrap();
        external.create_directory(Path::new("logs")).unwrap();
        let log = root.join("logs/events.jsonl");
        fs::write(&log, b"preserve unsafe record\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&log, fs::Permissions::from_mode(0o644)).unwrap();
        }
        #[cfg(windows)]
        {
            let status = std::process::Command::new("icacls.exe")
                .arg(&log)
                .args(["/grant", "*S-1-1-0:F"])
                .status()
                .unwrap();
            assert!(status.success());
        }
        let before = fs::metadata(&log).unwrap();
        let stability = observe_regular_file_stability(&before, &log).unwrap();
        assert!(append_jsonl(&log, &serde_json::json!({"must": "fail"})).is_err());
        assert_eq!(fs::read(&log).unwrap(), b"preserve unsafe record\n");
        assert_eq!(
            observe_regular_file_stability(&fs::metadata(&log).unwrap(), &log).unwrap(),
            stability
        );
        assert!(crate::private_fs::read_private_file(&log, 1024).is_err());
    }

    fn make_cas_object_unsafe(path: &Path, directory: bool) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                path,
                fs::Permissions::from_mode(if directory { 0o755 } else { 0o644 }),
            )
            .unwrap();
        }
        #[cfg(windows)]
        {
            let grant = if directory {
                "*S-1-1-0:(OI)(CI)F"
            } else {
                "*S-1-1-0:F"
            };
            assert!(
                std::process::Command::new("icacls.exe")
                    .arg(path)
                    .args(["/grant", grant])
                    .status()
                    .unwrap()
                    .success()
            );
        }
    }

    #[test]
    fn cas_writers_create_private_fanout_and_content() {
        let (_fixture, store) = object_store();
        let raw = store.write_raw(b"private raw creation").unwrap();
        let prepared = store
            .write_content_object(ContentObjectKind::Prepared, b"private prepared creation")
            .unwrap();
        for path in [
            store.object_path(ObjectKind::Raw, &raw).unwrap(),
            store
                .content_path(ContentObjectKind::Prepared, &prepared)
                .unwrap(),
        ] {
            crate::private_fs::read_private_file(&path, 1024).unwrap();
            let mut parent = path.parent().unwrap();
            loop {
                crate::private_fs::verify_private_directory(parent).unwrap();
                if parent == store.ambient_kio_dir().unwrap() {
                    break;
                }
                parent = parent.parent().unwrap();
            }
        }
    }

    #[test]
    fn cas_rejects_unsafe_owned_leaf_for_bound_and_ambient_access() {
        for bound in [false, true] {
            let fixture = tempfile::tempdir().unwrap();
            let root = fixture.path().canonicalize().unwrap();
            let repo = crate::scope::Repository::init(&root).unwrap();
            let ambient = ObjectStore::new(repo.kio_dir());
            let bytes = b"private CAS object";
            let hash = ambient.write_raw(bytes).unwrap();
            let path = ambient.object_path(ObjectKind::Raw, &hash).unwrap();
            let store = if bound {
                let kio = crate::store_dir::StoreDirectory::open(repo.kio_dir()).unwrap();
                ObjectStore::from_bound_kio(&kio.root_handle()).unwrap()
            } else {
                ambient
            };
            make_cas_object_unsafe(&path, false);
            let before =
                observe_regular_file_stability(&fs::metadata(&path).unwrap(), &path).unwrap();
            assert!(store.read_object(ObjectKind::Raw, &hash).is_err());
            assert!(store.inspect_object(ObjectKind::Raw, &hash).is_err());
            assert!(store.write_raw(bytes).is_err());
            assert!(store.remove_raw(&hash).is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
            assert_eq!(
                observe_regular_file_stability(&fs::metadata(&path).unwrap(), &path).unwrap(),
                before
            );
            assert!(crate::private_fs::read_private_file(&path, 1024).is_err());
        }
    }

    #[test]
    fn cas_rejects_unsafe_owned_fanout_without_repair() {
        for bound in [false, true] {
            let fixture = tempfile::tempdir().unwrap();
            let root = fixture.path().canonicalize().unwrap();
            let repo = crate::scope::Repository::init(&root).unwrap();
            let ambient = ObjectStore::new(repo.kio_dir());
            let bytes = b"private content object";
            let hash = ambient
                .write_content_object(ContentObjectKind::Prepared, bytes)
                .unwrap();
            let path = ambient
                .content_path(ContentObjectKind::Prepared, &hash)
                .unwrap();
            let parent = path.parent().unwrap();
            let store = if bound {
                let kio = crate::store_dir::StoreDirectory::open(repo.kio_dir()).unwrap();
                ObjectStore::from_bound_kio(&kio.root_handle()).unwrap()
            } else {
                ambient
            };
            make_cas_object_unsafe(parent, true);
            let identity = crate::platform::real_directory_identity(parent).unwrap();
            let names = fs::read_dir(parent)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>();
            assert!(
                store
                    .read_content_object_bytes(ContentObjectKind::Prepared, &hash, 1024)
                    .is_err()
            );
            assert!(
                store
                    .write_content_object(ContentObjectKind::Prepared, bytes)
                    .is_err()
            );
            assert!(
                store
                    .remove_content(ContentObjectKind::Prepared, &hash)
                    .is_err()
            );
            assert_eq!(fs::read(&path).unwrap(), bytes);
            assert_eq!(
                crate::platform::real_directory_identity(parent).unwrap(),
                identity
            );
            assert_eq!(
                fs::read_dir(parent)
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name())
                    .collect::<Vec<_>>(),
                names
            );
            assert!(crate::private_fs::verify_private_directory(parent).is_err());
        }
    }

    fn private_cas_fixture_file(path: &Path) -> File {
        let directory = crate::store_dir::StoreDirectory::open(path.parent().unwrap()).unwrap();
        directory
            .create_private_file(Path::new(path.file_name().unwrap()))
            .unwrap()
    }

    fn object_store() -> (tempfile::TempDir, ObjectStore) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let external = crate::store_dir::StoreDirectory::open(&root).unwrap();
        external.create_directory(Path::new(".kio")).unwrap();
        let store = ObjectStore::new(root.join(".kio"));
        (dir, store)
    }

    fn embedding_object(mut vector: Vec<f32>) -> EmbeddingObject {
        let norm = vector
            .iter()
            .map(|value| f64::from(*value).powi(2))
            .sum::<f64>()
            .sqrt();
        if norm.is_finite() && norm > 0.0 {
            for value in &mut vector {
                *value = (f64::from(*value) / norm) as f32;
            }
        }
        EmbeddingObject {
            spec_version: 1,
            target_type: "chunk".to_owned(),
            target_hash: hash_bytes(b"text"),
            profile_hash: hash_bytes(b"profile"),
            modality: "multimodal".to_owned(),
            dimensions: vector.len() as u64,
            distance: "cosine".to_owned(),
            context: Some(EmbeddingContext::Text("recovery window".to_owned())),
            vector,
            metadata: EmbeddingMetadata {
                record_format: 2,
                producer: ProducerProvenance::non_native(
                    ProducerExecutionMode::DeterministicLibrary,
                    &hash_bytes(b"profile"),
                )
                .unwrap(),
                image_input_receipt: None,
            },
        }
    }

    #[test]
    fn verified_cas_read_budget_limits_actual_reads_and_cleans_up_after_error() {
        let (_dir, store) = object_store();
        let bytes = vec![b'b'; CAS_STREAM_BUFFER_BYTES + 2];
        let hash = store.write_raw(&bytes).unwrap();

        let owner = super::VerifiedCasReadBudget::new();
        let error = owner
            .run(CAS_STREAM_BUFFER_BYTES as u64, |reads| {
                store
                    .with_verified_reads(reads)?
                    .inspect_object(ObjectKind::Raw, &hash)
                    .map(|_| ())
            })
            .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-VERIFIED-BYTES-LIMIT-001");
        assert_eq!(error.exit_code(), crate::ExitCode::PermanentFailure);
        assert_eq!(error.context()["max_bytes"], CAS_STREAM_BUFFER_BYTES as u64);
        assert_eq!(error.context()["attempted_bytes"], bytes.len() as u64);
        assert_eq!(
            error.context()["consumed_bytes"],
            CAS_STREAM_BUFFER_BYTES as u64
        );

        // The failed scope must not leak into the next independent operation.
        let ((), consumed) = owner
            .run(bytes.len() as u64, |reads| {
                store
                    .with_verified_reads(reads)?
                    .inspect_object(ObjectKind::Raw, &hash)
                    .map(|_| ())
            })
            .unwrap();
        assert_eq!(consumed, bytes.len() as u64);
    }

    #[test]
    fn verified_cas_reads_share_one_limit_across_scopes_and_store_clones() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ObjectStore>();
        assert_send_sync::<super::VerifiedCasReadCapability>();
        let (_first_dir, first) = object_store();
        let (_second_dir, second) = object_store();
        let first_hash = first.write_raw(b"first").unwrap();
        let second_hash = second.write_raw(b"second").unwrap();
        let owner = super::VerifiedCasReadBudget::new();
        let read_all = |reads: &super::VerifiedCasReadCapability| -> Result<()> {
            let first = first.with_verified_reads(reads)?;
            let second = second.with_verified_reads(reads)?;
            first.inspect_object(ObjectKind::Raw, &first_hash)?;
            first.clone().inspect_object(ObjectKind::Raw, &first_hash)?;
            second.inspect_object(ObjectKind::Raw, &second_hash)?;
            Ok(())
        };
        let ((), consumed) = owner.run(16, read_all).unwrap();
        assert_eq!(consumed, 16);
        let error = owner.run(15, read_all).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-VERIFIED-BYTES-LIMIT-001");
        assert_eq!(error.context()["attempted_bytes"], 16);
        assert_eq!(error.context()["consumed_bytes"], 10);
    }

    #[test]
    fn verified_cas_owner_rejects_nested_reset_and_token_replacement() {
        let (_dir, store) = object_store();
        let hash = store.write_raw(b"owner").unwrap();
        let owner = super::VerifiedCasReadBudget::new();
        let ((), consumed) = owner
            .run(10, |reads| {
                let metered = store.with_verified_reads(reads)?;
                metered.inspect_object(ObjectKind::Raw, &hash)?;
                let error = owner.run(0, |_| Ok(())).unwrap_err();
                assert_eq!(error.error_code(), "KIO-E-STORE-VERIFIED-BYTES-LIMIT-001");
                let independent = super::VerifiedCasReadBudget::new();
                let ((), independent_consumed) = independent.run(5, |other_reads| {
                    let error = metered.with_verified_reads(other_reads).unwrap_err();
                    assert_eq!(error.error_code(), "KIO-E-STORE-VERIFIED-BYTES-LIMIT-001");
                    store
                        .with_verified_reads(other_reads)?
                        .inspect_object(ObjectKind::Raw, &hash)?;
                    Ok(())
                })?;
                assert_eq!(independent_consumed, 5);
                metered
                    .with_verified_reads(reads)?
                    .inspect_object(ObjectKind::Raw, &hash)?;
                Ok(())
            })
            .unwrap();
        assert_eq!(consumed, 10);
    }

    #[test]
    fn expired_verified_cas_token_rejects_empty_reads_and_never_reactivates() {
        let (_dir, store) = object_store();
        let hash = store.write_raw(b"").unwrap();
        let owner = super::VerifiedCasReadBudget::new();
        let ((old_reads, old_store), consumed) = owner
            .run(0, |reads| {
                let metered = store.with_verified_reads(reads)?;
                metered.inspect_object(ObjectKind::Raw, &hash)?;
                Ok((reads.clone(), metered))
            })
            .unwrap();
        assert_eq!(consumed, 0);
        assert_eq!(
            old_reads.ensure_active().unwrap_err().error_code(),
            "KIO-E-STORE-VERIFIED-BYTES-LIMIT-001"
        );
        assert_eq!(
            old_store
                .inspect_object(ObjectKind::Raw, &hash)
                .unwrap_err()
                .error_code(),
            "KIO-E-STORE-VERIFIED-BYTES-LIMIT-001"
        );
        let ((), consumed) = owner
            .run(0, |fresh_reads| {
                assert!(old_reads.ensure_active().is_err());
                assert!(old_store.inspect_object(ObjectKind::Raw, &hash).is_err());
                store
                    .with_verified_reads(fresh_reads)?
                    .inspect_object(ObjectKind::Raw, &hash)?;
                Ok(())
            })
            .unwrap();
        assert_eq!(consumed, 0);
        assert!(store.inspect_object(ObjectKind::Raw, &hash).is_ok());
    }

    #[test]
    fn verified_cas_tokens_close_after_error_and_unwind() {
        let owner = super::VerifiedCasReadBudget::new();
        let mut escaped = None;
        let error = owner
            .run::<()>(0, |reads| {
                escaped = Some(reads.clone());
                Err(KioError::invalid_usage("injected operation error"))
            })
            .unwrap_err();
        assert_eq!(error.exit_code(), crate::ExitCode::InvalidUsage);
        assert!(escaped.take().unwrap().ensure_active().is_err());
        let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            owner.run::<()>(0, |reads| {
                escaped = Some(reads.clone());
                panic!("injected operation unwind");
            })
        }));
        assert!(panic_result.is_err());
        assert!(escaped.take().unwrap().ensure_active().is_err());
        let ((), consumed) = owner.run(0, |reads| reads.ensure_active()).unwrap();
        assert_eq!(consumed, 0);
    }

    #[cfg(unix)]
    #[test]
    fn bounded_regular_file_rejects_links_and_detects_stability_drift() {
        use std::io::Write as _;
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        fs::write(&source, b"stable").unwrap();

        let hard_link = dir.path().join("hard-link");
        fs::hard_link(&source, &hard_link).unwrap();
        assert!(read_bounded_regular_file(&hard_link, 64).is_err());

        let symlink_path = dir.path().join("symlink");
        symlink(&source, &symlink_path).unwrap();
        assert!(read_bounded_regular_file(&symlink_path, 64).is_err());

        fs::remove_file(&hard_link).unwrap();
        let before =
            observe_regular_file_stability(&fs::metadata(&source).unwrap(), &source).unwrap();
        let mut rewrite = OpenOptions::new().write(true).open(&source).unwrap();
        rewrite.write_all(b"mutate").unwrap();
        rewrite.sync_all().unwrap();
        let after =
            observe_regular_file_stability(&fs::metadata(&source).unwrap(), &source).unwrap();
        assert_ne!(
            before, after,
            "same-length in-place rewrite escaped observation"
        );

        let before_replacement = fs::symlink_metadata(&source).unwrap();
        let replacement = dir.path().join("replacement");
        fs::write(&replacement, b"new file").unwrap();
        fs::rename(&replacement, &source).unwrap();
        let replacement_handle = File::open(&source).unwrap();
        assert!(
            validate_open_regular_nofollow_with_before(
                &source,
                &replacement_handle,
                &before_replacement,
            )
            .is_err()
        );
    }

    #[test]
    fn an_embedding_object_round_trips_through_its_stored_bytes() {
        let object = embedding_object(vec![0.5, -0.25, 0.125]);
        let bytes = object.to_bytes().unwrap();
        assert_eq!(EmbeddingObject::from_bytes(&bytes).unwrap(), object);
        // 03 §8.1: identity, vector/digest, metadata/digest, each with LF.
        let text = String::from_utf8(bytes).unwrap();
        let lines = text.split('\n').collect::<Vec<_>>();
        assert_eq!(lines.len(), 6, "{text}");
        assert_eq!(lines[5], "");
        assert!(lines[0].starts_with('{') && lines[0].ends_with('}'));
    }

    #[test]
    fn an_embedding_identity_ignores_the_vector_and_follows_the_context() {
        // The storage key names what the vector is OF. Two different vectors
        // for the same target collide by design (that is how a re-send is
        // idempotent); two different contexts must not (07 §5.3's addendum —
        // else two chunks with identical bodies in differently named files
        // share one wrong vector).
        let base = embedding_object(vec![1.0, 0.0]);
        let mut other_vector = base.clone();
        other_vector.vector = vec![0.0, 1.0];
        assert_eq!(
            base.identity_hash().unwrap(),
            other_vector.identity_hash().unwrap()
        );

        let mut other_context = base.clone();
        other_context.context = Some(EmbeddingContext::Text("control coverage".to_owned()));
        assert_ne!(
            base.identity_hash().unwrap(),
            other_context.identity_hash().unwrap()
        );

        let mut no_context = base.clone();
        no_context.context = None;
        assert_ne!(
            base.identity_hash().unwrap(),
            no_context.identity_hash().unwrap()
        );
    }

    #[test]
    fn a_bit_flip_inside_the_vector_is_caught_by_the_trailing_digest() {
        // The storage key cannot catch this: it hashes the identity, not the
        // body. Without the digest a corrupted vector would rebuild silently
        // and quietly degrade every search that touched it.
        let object = embedding_object(vec![0.5, -0.25]);
        let mut bytes = object.to_bytes().unwrap();
        let body_start = bytes.iter().position(|byte| *byte == b'\n').unwrap() + 1;
        bytes[body_start] = if bytes[body_start] == b'A' {
            b'B'
        } else {
            b'A'
        };
        let error = EmbeddingObject::from_bytes(&bytes).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
    }

    #[test]
    fn an_embedding_object_rejects_a_length_or_finiteness_violation() {
        let mut short = embedding_object(vec![0.5, 0.5]);
        short.dimensions = 3;
        assert!(short.to_bytes().is_err());

        let nan = embedding_object(vec![f32::NAN, 0.5]);
        assert!(nan.to_bytes().is_err());
    }

    #[test]
    fn writing_an_embedding_publishes_it_under_its_identity_and_reads_back() {
        let (_dir, store) = object_store();
        let object = embedding_object(vec![0.5, -0.25, 0.125]);
        let hash = store.write_embedding(&object).unwrap();
        assert_eq!(hash, object.identity_hash().unwrap());
        assert_eq!(store.read_embedding(&hash).unwrap(), object);
        assert_eq!(store.embedding_hashes().unwrap(), vec![hash.clone()]);
        // Idempotent: the same vector re-published verifies rather than errors.
        assert_eq!(store.write_embedding(&object).unwrap(), hash);
    }

    #[test]
    fn reading_an_embedding_rejects_bytes_filed_under_the_wrong_identity() {
        let (_dir, store) = object_store();
        let object = embedding_object(vec![1.0, 0.0]);
        let hash = store.write_embedding(&object).unwrap();

        let mut impostor = object.clone();
        impostor.target_hash = hash_bytes(b"someone-else");
        fs::write(
            store.embedding_path(&hash).unwrap(),
            impostor.to_bytes().unwrap(),
        )
        .unwrap();

        let error = store.read_embedding(&hash).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
    }

    fn stray_temp_files(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".tmp-"))
            .collect()
    }

    #[test]
    fn cand_043_existing_cas_slot_must_match_exact_bytes() {
        let (_dir, store) = object_store();
        let expected = b"expected payload";
        let hash = store.write_raw(expected).unwrap();
        let path = store.object_path(ObjectKind::Raw, &hash).unwrap();

        store.write_raw(expected).unwrap();
        fs::write(&path, b"poisoned payload").unwrap();
        let error = store.write_raw(expected).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
        assert_eq!(fs::read(&path).unwrap(), b"poisoned payload");
    }

    #[test]
    fn cand_043_existing_directory_is_not_an_idempotent_cas_write() {
        let (_dir, store) = object_store();
        let expected = b"expected payload";
        let hash = hash_bytes(expected);
        store.ensure_object_parent(ObjectKind::Tree, &hash).unwrap();
        let path = store.object_path(ObjectKind::Tree, &hash).unwrap();
        fs::create_dir(&path).unwrap();

        let error = store
            .write_object_bytes(ObjectKind::Tree, &hash, expected)
            .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
    }

    #[cfg(unix)]
    #[test]
    fn cand_043_existing_symlink_is_not_an_idempotent_cas_write() {
        use std::os::unix::fs::symlink;

        let (dir, store) = object_store();
        let expected = b"expected payload";
        let hash = hash_bytes(expected);
        store.ensure_object_parent(ObjectKind::Raw, &hash).unwrap();
        let path = store.object_path(ObjectKind::Raw, &hash).unwrap();
        let outside = dir.path().join("outside");
        fs::write(&outside, expected).unwrap();
        symlink(&outside, &path).unwrap();

        let error = store.write_raw(expected).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
    }

    #[test]
    fn cand_043_existing_hardlink_is_not_an_immutable_cas_slot() {
        let (dir, store) = object_store();
        let expected = b"expected payload";
        let hash = hash_bytes(expected);
        store.ensure_object_parent(ObjectKind::Raw, &hash).unwrap();
        let path = store.object_path(ObjectKind::Raw, &hash).unwrap();
        let outside = dir.path().join("outside-hardlink");
        fs::write(&outside, expected).unwrap();
        fs::hard_link(&outside, &path).unwrap();

        #[cfg(windows)]
        {
            let information = windows_file_information(&File::open(&path).unwrap()).unwrap();
            assert!(information.is_regular_file());
            assert!(!information.is_single_link());
        }

        let error = store.write_raw(expected).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
    }

    #[test]
    fn cand_019_streaming_raw_writer_enforces_exact_limit_and_cleans_temp() {
        let (_dir, store) = object_store();
        let bytes = vec![b'x'; CAS_STREAM_BUFFER_BYTES];
        let (hash, consumed) = store
            .write_raw_reader(&mut Cursor::new(&bytes), bytes.len() as u64)
            .unwrap();
        assert_eq!(consumed, bytes.len() as u64);
        assert_eq!(hash, hash_bytes(&bytes));

        let error = store
            .write_raw_reader(
                &mut Cursor::new(vec![b'y'; CAS_STREAM_BUFFER_BYTES + 1]),
                CAS_STREAM_BUFFER_BYTES as u64,
            )
            .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-OBJECT-OVERSIZED-001");
        let raw_base = store.ambient_kio_dir().unwrap().join("objects/raw");
        assert!(stray_temp_files(&raw_base).is_empty());
    }

    #[test]
    fn cand_046_raw_inspect_streams_verified_size_without_returning_body() {
        let (_dir, store) = object_store();
        let bytes = vec![b'z'; 2 * CAS_STREAM_BUFFER_BYTES + 17];
        let hash = store.write_raw(&bytes).unwrap();
        let metadata = store.inspect_by_hash(&hash).unwrap();
        assert_eq!(metadata.kind, ObjectKind::Raw);
        assert_eq!(metadata.hash, hash);
        assert_eq!(metadata.size_bytes, bytes.len() as u64);
    }

    #[test]
    fn cand_046_structured_object_above_limit_is_rejected_before_materialization() {
        let (_dir, store) = object_store();
        let hash = hash_bytes(b"placeholder");
        store
            .ensure_object_parent(ObjectKind::Commit, &hash)
            .unwrap();
        let path = store.object_path(ObjectKind::Commit, &hash).unwrap();
        let file = private_cas_fixture_file(&path);
        file.set_len(MAX_COMMIT_OBJECT_BYTES + 1).unwrap();

        let error = store.read_by_hash(&hash).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-OBJECT-OVERSIZED-001");
    }

    #[test]
    fn cand_046_digest_mismatch_still_reports_store_corruption() {
        let (_dir, store) = object_store();
        let hash = hash_bytes(b"expected");
        store.ensure_object_parent(ObjectKind::Raw, &hash).unwrap();
        let path = store.object_path(ObjectKind::Raw, &hash).unwrap();
        private_cas_fixture_file(&path)
            .write_all(b"different")
            .unwrap();

        let error = store.inspect_by_hash(&hash).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
    }

    #[test]
    fn purge_remove_raw_is_verified_and_idempotent() {
        let (_dir, store) = object_store();
        let bytes = b"purge target";
        let hash = store.write_raw(bytes).unwrap();
        assert!(store.remove_raw(&hash).unwrap());
        assert!(!store.object_path(ObjectKind::Raw, &hash).unwrap().exists());
        assert!(!store.remove_raw(&hash).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn purge_remove_raw_rejects_hardlink_without_unlinking_either_name() {
        let (dir, store) = object_store();
        let bytes = b"hardlinked purge target";
        let hash = store.write_raw(bytes).unwrap();
        let canonical = store.object_path(ObjectKind::Raw, &hash).unwrap();
        let outside = dir.path().join("outside-hardlink");
        fs::hard_link(&canonical, &outside).unwrap();
        assert_eq!(
            store.remove_raw(&hash).unwrap_err().error_code(),
            "KIO-E-STORE-CORRUPT-001"
        );
        assert_eq!(fs::read(&canonical).unwrap(), bytes);
        assert_eq!(fs::read(&outside).unwrap(), bytes);
    }

    fn chunk_object(text: &str) -> ChunkObject {
        ChunkObject {
            spec_version: 1,
            raw_hash: format!("sha256:{}", "a".repeat(64)),
            tool_profile_hash: format!("sha256:{}", "b".repeat(64)),
            r#gen: 3,
            unit_key: "page:12".to_owned(),
            unit_content_hash: format!("sha256:{}", "c".repeat(64)),
            heading_path: vec!["Auth".to_owned()],
            section_id: Some("auth".to_owned()),
            byte_start: 0,
            byte_end: text.len() as u64,
            text_hash: hash_bytes(text.as_bytes()),
            text: text.to_owned(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn purge_chunk_inventory_enforces_entry_and_aggregate_read_budgets() {
        let (dir, store) = object_store();
        let first = chunk_object("first inventory text");
        let first_id = store.write_chunk(&first).unwrap();
        let mut second = chunk_object("second inventory text");
        second.unit_key = "page:13".to_owned();
        let second_id = store.write_chunk(&second).unwrap();
        let first_path = store.chunk_path(&first_id).unwrap();
        let second_path = store.chunk_path(&second_id).unwrap();
        let total =
            fs::metadata(&first_path).unwrap().len() + fs::metadata(&second_path).unwrap().len();
        let base = File::open(dir.path().join(".kio/objects/chunks")).unwrap();
        let mut seen = 0;
        bound_visit_chunks_with_limits(
            &CasReadAccess::Unmetered,
            &base,
            100,
            total,
            &mut |_, _| {
                seen += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(seen, 2);
        let mut seen = 0;
        let error = bound_visit_chunks_with_limits(
            &CasReadAccess::Unmetered,
            &base,
            2,
            total,
            &mut |_, _| {
                seen += 1;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
        assert_eq!(seen, 0, "fanout directories count before the first leaf");
        let mut seen = 0;
        let error = bound_visit_chunks_with_limits(
            &CasReadAccess::Unmetered,
            &base,
            100,
            total - 1,
            &mut |_, _| {
                seen += 1;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-OBJECT-OVERSIZED-001");
        assert_eq!(
            seen, 1,
            "the remaining budget prevents reading the second body"
        );
        let unknown = first_path
            .parent()
            .unwrap()
            .join(".kio-cas-remove-unattributed");
        fs::write(&unknown, b"unknown plaintext").unwrap();
        assert!(
            bound_visit_chunks_with_limits(
                &CasReadAccess::Unmetered,
                &base,
                100,
                total,
                &mut |_, _| Ok(())
            )
            .is_err()
        );
        assert_eq!(fs::read(&unknown).unwrap(), b"unknown plaintext");
        assert!(first_path.exists() && second_path.exists());
    }

    #[test]
    fn bound_semantic_objects_preserve_identity_and_retained_namespace_after_replacement() {
        let fixture = tempfile::tempdir().unwrap();
        let kio = fixture.path().join(".kio");
        for kind in ["raw", "trees", "commits"] {
            crate::store_dir::StoreDirectory::open_or_create_directories(
                &kio.join("objects").join(kind),
            )
            .unwrap();
        }
        let handle =
            cap_primitives::fs::open_ambient_dir(&kio, cap_primitives::ambient_authority())
                .unwrap();
        let bound = ObjectStore::from_bound_kio(&handle).unwrap();
        drop(handle);
        let chunk = chunk_object("retained semantic text");
        let chunk_hash = bound.write_chunk(&chunk).unwrap();
        let embedding = embedding_object(vec![1.0, 0.0]);
        let embedding_hash = bound.write_embedding(&embedding).unwrap();
        assert_ne!(
            chunk_hash,
            hash_bytes(&canonical_json_bytes(&serde_json::to_value(&chunk).unwrap()).unwrap())
        );
        assert_ne!(embedding_hash, hash_bytes(&embedding.to_bytes().unwrap()));
        assert!(bound.object_path(ObjectKind::Raw, &chunk_hash).is_err());
        assert!(bound.chunk_path(&chunk_hash).is_err());
        assert!(bound.embedding_path(&embedding_hash).is_err());

        let parked = fixture.path().join("retained");
        fs::rename(&kio, &parked).unwrap();
        fs::create_dir(&kio).unwrap();
        assert_eq!(bound.read_chunk(&chunk_hash).unwrap(), chunk);
        assert_eq!(bound.read_embedding(&embedding_hash).unwrap(), embedding);
        assert_eq!(bound.write_chunk(&chunk).unwrap(), chunk_hash);
        assert_eq!(bound.write_embedding(&embedding).unwrap(), embedding_hash);
        let mut inconsistent = embedding.clone();
        inconsistent.vector = vec![0.0, 1.0];
        assert!(bound.write_embedding(&inconsistent).is_err());
        assert_eq!(bound.read_embedding(&embedding_hash).unwrap(), embedding);

        let plain = ObjectStore::new(&parked);
        let bytes = fs::read(plain.chunk_path(&chunk_hash).unwrap()).unwrap();
        fs::write(
            plain.chunk_path(&chunk_hash).unwrap(),
            vec![b'x'; bytes.len()],
        )
        .unwrap();
        let error = bound.read_chunk_accounted(&chunk_hash).unwrap_err();
        assert_eq!(error.consumed_bytes, bytes.len() as u64);
        assert_eq!(fs::read_dir(&kio).unwrap().count(), 0);
    }

    #[test]
    fn ct4_chunk_cas_round_trips_semantic_identity() {
        let (_dir, store) = object_store();
        let chunk = chunk_object("TTL is 3600 seconds.");
        let expected = chunk.identity_hash().unwrap();
        assert_eq!(store.write_chunk(&chunk).unwrap(), expected);
        assert_eq!(store.read_chunk(&expected).unwrap(), chunk);
        assert!(
            !fs::read(store.chunk_path(&expected).unwrap())
                .unwrap()
                .windows(b"chunk_hash".len())
                .any(|window| window == b"chunk_hash")
        );
    }

    #[test]
    fn chunk_identity_binds_normalized_unit_content() {
        let chunk = chunk_object("stable text");
        let original = chunk.identity_hash().unwrap();

        let mut retargeted = chunk.clone();
        retargeted.unit_content_hash = format!("sha256:{}", "d".repeat(64));
        assert_ne!(retargeted.identity_hash().unwrap(), original);

        let mut changed_body = chunk.clone();
        changed_body.text.push('!');
        changed_body.text_hash = hash_bytes(changed_body.text.as_bytes());
        assert_eq!(changed_body.identity_hash().unwrap(), original);
    }

    #[test]
    fn chunk_object_rejects_missing_unit_content_hash() {
        let mut value = serde_json::to_value(chunk_object("stable text")).unwrap();
        value.as_object_mut().unwrap().remove("unit_content_hash");
        assert!(serde_json::from_value::<ChunkObject>(value).is_err());
    }

    /// A corrupt content object is charged for every byte it made fsck read,
    /// so one aggregate budget still bounds an adversarial store.
    #[test]
    fn ct4_fsck_content_read_reports_consumed_bytes_even_when_it_fails() {
        let (_dir, store) = object_store();
        let chunk = chunk_object("single representation");
        let chunk_hash = store.write_chunk(&chunk).unwrap();
        let chunk_bytes = fs::read(store.chunk_path(&chunk_hash).unwrap()).unwrap();
        assert_eq!(
            store.read_chunk_with_size(&chunk_hash).unwrap().1,
            chunk_bytes.len() as u64
        );

        let image_bytes = b"image-object";
        let image_hash = hash_bytes(image_bytes);
        let image_path = store
            .content_path(ContentObjectKind::Image, &image_hash)
            .unwrap();
        crate::store_dir::StoreDirectory::open_or_create_directories(image_path.parent().unwrap())
            .unwrap();
        private_cas_fixture_file(&image_path)
            .write_all(image_bytes)
            .unwrap();
        assert_eq!(
            store
                .inspect_content_object(ContentObjectKind::Image, &image_hash)
                .unwrap()
                .size_bytes,
            image_bytes.len() as u64
        );

        let poisoned = b"poisoned-img";
        fs::write(&image_path, poisoned).unwrap();
        let accounted = store
            .inspect_content_accounted(ContentObjectKind::Image, &image_hash)
            .unwrap_err();
        assert_eq!(accounted.consumed_bytes, poisoned.len() as u64);
        assert_eq!(
            store
                .inspect_content_object(ContentObjectKind::Image, &image_hash)
                .unwrap_err()
                .error_code(),
            "KIO-E-STORE-CORRUPT-001"
        );
    }

    #[test]
    fn normalized_unit_content_objects_use_the_dedicated_immutable_namespace() {
        let (_dir, store) = object_store();
        let bytes = br#"{"markdown":"immutable normalized unit"}"#;
        let hash = store
            .write_content_object(ContentObjectKind::NormalizedUnit, bytes)
            .unwrap();
        assert_eq!(
            store
                .read_content_object_bytes(ContentObjectKind::NormalizedUnit, &hash, 1024)
                .unwrap(),
            bytes
        );
        let path = store
            .content_path(ContentObjectKind::NormalizedUnit, &hash)
            .unwrap();
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            hash.strip_prefix("sha256:")
        );
        assert_eq!(
            path.parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str()),
            hash.strip_prefix("sha256:").map(|digest| &digest[2..4])
        );
        assert_eq!(
            path.parent()
                .and_then(|parent| parent.parent())
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str()),
            hash.strip_prefix("sha256:").map(|digest| &digest[..2])
        );
        assert_eq!(
            path.parent()
                .and_then(|parent| parent.parent())
                .and_then(|parent| parent.parent())
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str()),
            Some("normalized_unit_objects")
        );
        assert_eq!(
            path.parent()
                .and_then(|parent| parent.parent())
                .and_then(|parent| parent.parent())
                .and_then(|parent| parent.parent())
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str()),
            Some("objects")
        );
    }

    #[test]
    fn manifest_content_objects_enforce_the_semantic_read_limit_at_cas_boundary() {
        let (_dir, store) = object_store();
        let hash = hash_bytes(b"manifest-size-fixture");
        let path = store
            .content_path(ContentObjectKind::Manifest, &hash)
            .unwrap();
        crate::store_dir::StoreDirectory::open_or_create_directories(path.parent().unwrap())
            .unwrap();
        let file = private_cas_fixture_file(&path);
        file.set_len(MAX_MANIFEST_OBJECT_BYTES + 1).unwrap();

        let error = store
            .inspect_content_accounted(ContentObjectKind::Manifest, &hash)
            .unwrap_err();
        assert_eq!(error.error.error_code(), "KIO-E-STORE-OBJECT-OVERSIZED-001");
        assert_eq!(error.consumed_bytes, 0);
    }

    #[test]
    fn ct4_fsck_repair_raw_atomically_replaces_one_corrupt_slot() {
        let (_dir, store) = object_store();
        let expected = b"recoverable raw body";
        let hash = store.write_raw(expected).unwrap();
        let path = store.object_path(ObjectKind::Raw, &hash).unwrap();
        fs::write(&path, b"poisoned raw bytes").unwrap();

        assert!(store.repair_raw(&hash, expected).unwrap());
        assert_eq!(fs::read(&path).unwrap(), expected);
        assert_eq!(
            store.inspect_object(ObjectKind::Raw, &hash).unwrap().hash,
            hash
        );
        assert!(fs::read_dir(path.parent().unwrap()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".repair-quarantine-")
        }));
    }

    #[test]
    fn ct4_fsck_failed_raw_read_reports_exact_consumed_bytes() {
        let (_dir, store) = object_store();
        let expected = b"expected accounted bytes";
        let hash = store.write_raw(expected).unwrap();
        let path = store.object_path(ObjectKind::Raw, &hash).unwrap();
        let poisoned = vec![b'x'; expected.len()];
        fs::write(&path, &poisoned).unwrap();

        let owner = super::VerifiedCasReadBudget::new();
        let (failure, consumed) = owner
            .run(poisoned.len() as u64, |reads| {
                let store = store.with_verified_reads(reads)?;
                Ok(store
                    .inspect_object_accounted(ObjectKind::Raw, &hash)
                    .unwrap_err())
            })
            .unwrap();
        assert_eq!(failure.error.error_code(), "KIO-E-STORE-CORRUPT-001");
        assert_eq!(failure.consumed_bytes, poisoned.len() as u64);
        assert_eq!(consumed, poisoned.len() as u64);
    }

    #[test]
    fn ct4_fsck_repair_raw_rejects_hardlink_without_external_unlink() {
        let (dir, store) = object_store();
        let expected = b"hardlink repair bytes";
        let hash = store.write_raw(expected).unwrap();
        let path = store.object_path(ObjectKind::Raw, &hash).unwrap();
        let outside = dir.path().join("outside-repair-link");
        fs::hard_link(&path, &outside).unwrap();
        fs::write(&path, b"linked corrupt bytes").unwrap();

        assert_eq!(
            store.repair_raw(&hash, expected).unwrap_err().error_code(),
            "KIO-E-STORE-CORRUPT-001"
        );
        assert!(path.exists());
        assert!(outside.exists());
        assert_eq!(fs::read(&outside).unwrap(), b"linked corrupt bytes");
    }

    #[test]
    fn ct4_chunk_cas_rejects_text_hash_and_fanout_mismatch() {
        let (_dir, store) = object_store();
        let chunk = chunk_object("trusted");
        let hash = store.write_chunk(&chunk).unwrap();
        let path = store.chunk_path(&hash).unwrap();
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["text"] = Value::from("poisoned");
        fs::write(&path, canonical_json_bytes(&value).unwrap()).unwrap();
        let poisoned_len = fs::metadata(&path).unwrap().len();
        assert_eq!(
            store
                .read_chunk_accounted(&hash)
                .unwrap_err()
                .consumed_bytes,
            poisoned_len
        );
        assert_eq!(
            store.read_chunk(&hash).unwrap_err().error_code(),
            "KIO-E-STORE-CORRUPT-001"
        );
    }

    #[test]
    fn purge_remove_chunk_verifies_identity_and_is_idempotent() {
        let (_dir, store) = object_store();
        let chunk = chunk_object("purge semantic chunk");
        let hash = store.write_chunk(&chunk).unwrap();
        assert!(store.remove_chunk(&hash).unwrap());
        assert!(!store.chunk_path(&hash).unwrap().exists());
        assert!(!store.remove_chunk(&hash).unwrap());
    }

    /// The embeddings namespace is keyed by identity, not by the bytes, so it
    /// needs its own removal primitive: `remove_content` re-hashes the leaf and
    /// so calls a healthy embedding object corrupt (the purge defect this
    /// method fixes — purge deleted the orphan SQLite row, then aborted the
    /// whole phase on the CAS object it was supposed to delete alongside it).
    #[test]
    fn purge_remove_embedding_verifies_identity_and_is_idempotent() {
        let (_dir, store) = object_store();
        let hash = store
            .write_embedding(&embedding_object(vec![0.5, -0.25, 0.125]))
            .unwrap();
        let path = store.embedding_path(&hash).unwrap();
        assert_eq!(
            store
                .remove_content(ContentObjectKind::Embedding, &hash)
                .unwrap_err()
                .error_code(),
            "KIO-E-STORE-CORRUPT-001"
        );
        assert!(path.exists());

        assert!(store.remove_embedding(&hash).unwrap());
        assert!(!path.exists());
        assert!(!store.remove_embedding(&hash).unwrap());
    }

    #[test]
    fn purge_remove_content_verifies_bytes_and_is_idempotent() {
        let (_dir, store) = object_store();
        let bytes = b"purge image content";
        let hash = hash_bytes(bytes);
        let path = store.content_path(ContentObjectKind::Image, &hash).unwrap();
        crate::store_dir::StoreDirectory::open_or_create_directories(path.parent().unwrap())
            .unwrap();
        private_cas_fixture_file(&path).write_all(bytes).unwrap();
        assert!(
            store
                .remove_content(ContentObjectKind::Image, &hash)
                .unwrap()
        );
        assert!(!path.exists());
        assert!(
            !store
                .remove_content(ContentObjectKind::Image, &hash)
                .unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn purge_remove_content_rejects_hardlink_without_unlinking() {
        let (dir, store) = object_store();
        let bytes = b"hardlinked purge image";
        let hash = hash_bytes(bytes);
        let path = store.content_path(ContentObjectKind::Image, &hash).unwrap();
        crate::store_dir::StoreDirectory::open_or_create_directories(path.parent().unwrap())
            .unwrap();
        private_cas_fixture_file(&path).write_all(bytes).unwrap();
        let outside = dir.path().join("outside-image-hardlink");
        fs::hard_link(&path, &outside).unwrap();
        assert_eq!(
            store
                .remove_content(ContentObjectKind::Image, &hash)
                .unwrap_err()
                .error_code(),
            "KIO-E-STORE-CORRUPT-001"
        );
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(fs::read(&outside).unwrap(), bytes);
    }

    #[cfg(unix)]
    #[test]
    fn bound_store_keeps_raw_publication_inside_retained_namespace_after_path_swap() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let kio = dir.path().join(".kio");
        let objects = kio.join("objects");
        for kind in ["raw", "trees", "commits"] {
            crate::store_dir::StoreDirectory::open_or_create_directories(&objects.join(kind))
                .unwrap();
        }
        let kio_handle = File::open(&kio).unwrap();
        let store = ObjectStore::from_bound_kio(&kio_handle).unwrap();
        let retained_raw = objects.join("raw-retained");
        fs::rename(objects.join("raw"), &retained_raw).unwrap();
        let victim = tempfile::tempdir().unwrap();
        symlink(victim.path(), objects.join("raw")).unwrap();

        for (kind, bytes) in [
            (ObjectKind::Tree, b"bound tree publication".as_slice()),
            (ObjectKind::Commit, b"bound commit publication".as_slice()),
        ] {
            let hash = hash_bytes(bytes);
            store.write_object_bytes(kind, &hash, bytes).unwrap();
            assert_eq!(store.read_object(kind, &hash).unwrap().bytes, bytes);
        }
        let bytes = b"bound raw publication";
        let hash = store.write_raw(bytes).unwrap();
        let digest = hash_path_component(&hash).unwrap();
        let retained_leaf = retained_raw
            .join(&digest[..2])
            .join(&digest[2..4])
            .join(digest);
        assert_eq!(fs::read(retained_leaf).unwrap(), bytes);
        assert!(fs::read_dir(victim.path()).unwrap().next().is_none());
        assert_eq!(
            store.read_object(ObjectKind::Raw, &hash).unwrap().bytes,
            bytes
        );
    }

    #[cfg(unix)]
    #[test]
    fn bound_store_does_not_create_missing_content_namespaces_until_written() {
        let dir = tempfile::tempdir().unwrap();
        let kio = dir.path().join(".kio");
        let objects = kio.join("objects");
        for kind in ["raw", "trees", "commits"] {
            crate::store_dir::StoreDirectory::open_or_create_directories(&objects.join(kind))
                .unwrap();
        }
        let kio_handle = File::open(&kio).unwrap();
        let store = ObjectStore::from_bound_kio(&kio_handle).unwrap();
        let mut names: Vec<_> = fs::read_dir(&objects)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["commits", "raw", "trees"]);

        let hash = store
            .write_content_object(ContentObjectKind::Toollock, b"bound lazy tool lock")
            .unwrap();
        assert!(
            objects
                .join(ContentObjectKind::Toollock.directory())
                .is_dir()
        );
        assert_eq!(
            store
                .read_content_object_bytes(ContentObjectKind::Toollock, &hash, 1024)
                .unwrap(),
            b"bound lazy tool lock"
        );
    }

    #[cfg(unix)]
    #[test]
    fn bound_store_keeps_every_content_namespace_inside_retained_parents_after_path_swap() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let kio = dir.path().join(".kio");
        let objects = kio.join("objects");
        for kind in ["raw", "trees", "commits"] {
            crate::store_dir::StoreDirectory::open_or_create_directories(&objects.join(kind))
                .unwrap();
        }
        let plain = ObjectStore::new(&kio);
        let kinds = [
            ContentObjectKind::Prepared,
            ContentObjectKind::Image,
            ContentObjectKind::Embedding,
            ContentObjectKind::Manifest,
            ContentObjectKind::Toollock,
            ContentObjectKind::NormalizedUnit,
        ];
        let old: Vec<_> = kinds
            .iter()
            .map(|kind| {
                let bytes = format!("retained {} content", kind.object_type()).into_bytes();
                let hash = plain.write_content_object(*kind, &bytes).unwrap();
                (*kind, hash, bytes)
            })
            .collect();
        let kio_handle = File::open(&kio).unwrap();
        let store = ObjectStore::from_bound_kio(&kio_handle).unwrap();

        let victim = tempfile::tempdir().unwrap();
        for kind in kinds {
            let public = objects.join(kind.directory());
            fs::rename(
                &public,
                objects.join(format!("{}-retained", kind.directory())),
            )
            .unwrap();
            symlink(victim.path(), public).unwrap();
        }

        for (kind, old_hash, old_bytes) in old {
            assert_eq!(
                store
                    .read_content_object_bytes(kind, &old_hash, old_bytes.len() as u64)
                    .unwrap(),
                old_bytes
            );
            assert_eq!(
                store
                    .inspect_content_object(kind, &old_hash)
                    .unwrap()
                    .size_bytes,
                old_bytes.len() as u64
            );

            let new_bytes = format!("new retained {} content", kind.object_type()).into_bytes();
            let new_hash = store.write_content_object(kind, &new_bytes).unwrap();
            assert_eq!(
                store
                    .read_content_object_bytes(kind, &new_hash, new_bytes.len() as u64)
                    .unwrap(),
                new_bytes
            );
            let digest = hash_path_component(&new_hash).unwrap();
            let retained_leaf = objects
                .join(format!("{}-retained", kind.directory()))
                .join(&digest[..2])
                .join(&digest[2..4])
                .join(digest);
            assert_eq!(fs::read(retained_leaf).unwrap(), new_bytes);
            assert!(store.content_path(kind, &new_hash).is_err());
        }
        assert!(fs::read_dir(victim.path()).unwrap().next().is_none());
    }
}
