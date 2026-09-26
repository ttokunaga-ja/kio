//! Pipeline contracts and deterministic Step 2 helpers.

pub mod budget;
pub mod ledger;
pub mod markdownize;
pub mod policy;
pub mod prepare;
pub mod scan;
mod store_path;
pub mod task;
pub mod unsupported;
#[cfg(any(test, windows))]
mod windows_file;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, PipelineError>;

#[derive(Debug, Error)]
pub enum PipelineError {
    #[error("{code}: {message}")]
    Contract { code: &'static str, message: String },
    #[error("io error at {path}: {message}")]
    Io { path: String, message: String },
    /// A live writer owns a store lock. Kept distinct from I/O so CLI callers
    /// preserve the retryable `KIO-E-STORE-LOCKED-001` / exit-3 contract.
    #[error("KIO-E-STORE-LOCKED-001: store is locked: {path}")]
    Locked { path: String },
    #[error("schema error: {0}")]
    Schema(String),
    /// A persisted store file (tasks / cost ledger / unsupported inputs) could
    /// not be parsed — the on-disk record is corrupt, not a config/schema error
    /// (M1(c)). Surfaced as `KIO-E-STORE-CORRUPT-001` (exit 4), carrying the
    /// offending file path.
    #[error("KIO-E-STORE-CORRUPT-001: corrupt store file at {path}: {message}")]
    Corrupt { path: String, message: String },
    /// A persisted task's `input_path` escapes the scope: it is absolute, carries
    /// a path separator, or contains a `..`/`.` traversal component rather than
    /// naming a direct child of the scope root (03 §3.3; the same rule dag.rs
    /// enforces for tree entries). Surfaced as `KIO-E-STORE-PATH-001` (exit 2).
    /// A poisoned / shared `tasks.jsonl` must not let a resume read an arbitrary
    /// file and send it to an online adapter (P1).
    #[error("KIO-E-STORE-PATH-001: task input_path is not a scope-local file name: {path}")]
    Path { path: String },
    /// `cost-ledger.sqlite` (04 §5.4) transport error from `rusqlite`. Most call
    /// sites reclassify a `SQLITE_CONSTRAINT_CHECK` failure into
    /// [`PipelineError::contract`] `KIO-E-STORE-CONSTRAINT-001` (04 §5.8 / 06 §7)
    /// before it reaches this variant — this is the residual (busy/io/etc.).
    #[error("cost ledger sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A stable, owned cost-ledger snapshot could not be obtained for a
    /// read-only budget observation. Kept lossless and path-bound so callers
    /// preserve Missing versus unsafe-integrity versus retryable-busy
    /// semantics without consulting ambient path configuration again.
    #[error("cost ledger snapshot error at {path}: {source}")]
    LedgerSnapshot {
        path: String,
        #[source]
        source: ledger::LedgerSnapshotError,
    },
}

impl PipelineError {
    #[must_use]
    pub fn contract(code: &'static str, message: impl Into<String>) -> Self {
        Self::Contract {
            code,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn corrupt(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Corrupt {
            path: path.into(),
            message: message.into(),
        }
    }

    #[must_use]
    pub fn locked(path: impl Into<String>) -> Self {
        Self::Locked { path: path.into() }
    }

    #[must_use]
    pub fn path(path: impl Into<String>) -> Self {
        Self::Path { path: path.into() }
    }

    #[must_use]
    pub fn ledger_snapshot(path: impl Into<String>, source: ledger::LedgerSnapshotError) -> Self {
        Self::LedgerSnapshot {
            path: path.into(),
            source,
        }
    }
}

pub(crate) trait IoResultExt<T> {
    fn pipeline_io(self, path: &std::path::Path) -> Result<T>;
}

impl<T> IoResultExt<T> for std::io::Result<T> {
    fn pipeline_io(self, path: &std::path::Path) -> Result<T> {
        self.map_err(|err| PipelineError::Io {
            path: path.display().to_string(),
            message: err.to_string(),
        })
    }
}
