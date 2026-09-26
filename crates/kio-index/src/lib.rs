//! Durable local index implementation.
//!
//! This crate owns chunking, FTS, embedding storage, tree projection, and
//! rebuild primitives used by the Kio pipeline and CLI.

pub mod aggregator;
pub mod chunking;
pub mod embedding_store;
pub mod fts;
pub mod registry;
#[cfg(windows)]
pub(crate) mod registry_windows_security;
pub mod rows;
mod search_projection;
pub mod vec;

use rusqlite::Error as SqliteError;
use thiserror::Error;

pub use rows::{
    ChunkRow, EmbeddingDistance, EmbeddingModality, EmbeddingRow, EmbeddingTargetType, TreeEntryRow,
};

pub use search_projection::project_search_text;

pub type Result<T> = std::result::Result<T, IndexError>;

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("index contract error: {0}")]
    Contract(String),
    #[error("index schema error: {0}")]
    Schema(String),
    #[error("index sqlite error: {0}")]
    Sqlite(#[from] SqliteError),
    #[error("scope registry snapshot is missing")]
    RegistryMissing,
    #[error("scope registry snapshot integrity error: {0}")]
    RegistryUnsafeIntegrity(String),
    #[error("scope registry snapshot was unstable or busy: {0}")]
    RegistryUnstableBusy(String),
}

impl From<serde_json::Error> for IndexError {
    fn from(value: serde_json::Error) -> Self {
        Self::Schema(value.to_string())
    }
}
