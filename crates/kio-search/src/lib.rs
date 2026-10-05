//! Search primitives: RRF fusion, MMR diversification, cursor tokens,
//! query-hash canonicalization, and Evidence Pointer issue/serialization.
//!
//! The application layer (`kio-app`) supplies the execution engine (FTS5/vector
//! backends, multi-scope merge, paging); this crate holds the deterministic, contract-
//! frozen pieces those flows are built from (05-runtime.md §1.3-§1.5/§1.8,
//! 08-evidence-pointer-spec.md §2).

pub mod cursor;
pub mod evidence;
pub mod mmr;
pub mod object_uri;
pub mod query;
pub mod rrf;

use thiserror::Error;

pub use cursor::{CursorExcludedScope, CursorToken, ScopeCursor, ScopeMode};
pub use evidence::EvidencePointer;
pub use object_uri::{ObjectUri, RelatedImage, extract_related_images};
pub use query::{ChunkingConfigBinding, TimeTravelSelector};

pub type Result<T> = std::result::Result<T, SearchError>;

#[derive(Debug, Error)]
pub enum SearchError {
    #[error("search contract error: {0}")]
    Contract(String),
    #[error("invalid search cursor: {0}")]
    Cursor(String),
    #[error("evidence error: {0}")]
    Evidence(String),
}
