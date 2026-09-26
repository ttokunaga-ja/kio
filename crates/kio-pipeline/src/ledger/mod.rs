//! `cost-ledger.sqlite`: the device-global cost/Batch-intent store (04-pipeline.md
//! §5.4 DDL, §5.8 2-phase protocol).
//! module split:
//!
//! - [`schema`] — DDL-of-record, connection bootstrap, canonical shape self-heal
//! - [`time`] — UTC epoch-ms calendar helpers (10 §11.4's one ISO8601 exception)
//! - [`model`] — row/enum types
//! - [`ops`] — the §5.8/§5.4 state machine: phase 1-3, idempotent recording,
//!   outcome validation, crash recovery, sync degenerate 2-phase, the
//!   query-embedding device row, budget cap check-then-reserve, and abandon.

pub mod backup;
pub(crate) mod lifecycle;
pub(crate) mod lifecycle_fs;
pub mod model;
pub mod ops;
pub(crate) mod ops_sql;
pub mod schema;
pub mod snapshot;
#[cfg(windows)]
pub(crate) mod snapshot_windows_security;
pub mod time;

pub use backup::{LedgerBackup, RestoreReadiness};
pub use lifecycle::{LedgerDb, PendingWriteRecovery};
pub use model::{BatchRequestRow, BatchState, CostLedgerRow, Outcome, RequestKind, TaskKey};
pub use schema::default_ledger_path;
pub use snapshot::{LedgerReadSnapshot, LedgerSnapshotError};
