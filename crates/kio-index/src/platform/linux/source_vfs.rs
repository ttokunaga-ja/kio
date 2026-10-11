//! Linux retained source SQLite path and connection-owned verification.
use crate::Result;
use crate::platform::source::{BoundSourceIndex, source_file_identity};
use crate::platform::{BoundSqliteConnection, linux_bound_sqlite};
use rusqlite::OpenFlags;
pub(crate) fn open_bound_source_connection(
    source: &BoundSourceIndex,
    flags: OpenFlags,
) -> Result<BoundSqliteConnection> {
    let expected = source_file_identity(&source.file)?;
    let leaf = source
        .public_path
        .file_name()
        .expect("bound source index has a file name");
    linux_bound_sqlite::connection(&source._parent, leaf, (expected.dev, expected.ino), flags)
}
