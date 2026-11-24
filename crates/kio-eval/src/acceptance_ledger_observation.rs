//! SQL observations of a quiescent acceptance ledger. SQLite is only allowed
//! to open the detached copy, since even read-only opens may create sidecars.

use std::{io::Read, path::Path};

use kio_core::store_dir::StoreDirectory;
use rusqlite::{Connection, OpenFlags};

use crate::acceptance::{AcceptanceError, MAX_BINARY_BYTES};

pub(crate) fn with_snapshot<T>(
    database: &Path,
    observe: impl FnOnce(&Connection) -> Result<T, AcceptanceError>,
) -> Result<T, AcceptanceError> {
    let parent = database
        .parent()
        .ok_or_else(|| invalid("acceptance ledger has no parent"))?;
    let name = database
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("acceptance ledger has no UTF-8 filename"))?;
    let source = kio_core::private_fs::verify_private_directory(parent).map_err(store_error)?;

    let temp = tempfile::tempdir().map_err(io_error)?;
    let base = StoreDirectory::open(&temp.path().canonicalize().map_err(io_error)?)
        .map_err(store_error)?;
    let leaf = Path::new("private");
    let dir = base.create_directory(leaf).map_err(store_error)?;
    let copy = StoreDirectory::from_retained(dir, base.path().join(leaf)).map_err(store_error)?;

    // The caller has waited for its CLI to exit (or killed and reaped it).
    // Retain committed WAL frames, but never copy SHM coordination state.
    let wal = format!("{name}-wal");
    let shm = format!("{name}-shm");
    for file in [Path::new(name), Path::new(&wal), Path::new(&shm)] {
        if file != Path::new(name) && !source.contains_entry(file).map_err(store_error)? {
            continue;
        }
        source.ensure_owner_private(file).map_err(|error| {
            invalid(format!(
                "unsafe ledger input '{}': {error}",
                source.path().join(file).display()
            ))
        })?;
        if file == Path::new(&shm) {
            continue;
        }
        let input = source
            .open_regular_read(file, MAX_BINARY_BYTES)
            .map_err(store_error)?;
        let mut output = copy.create_private_file(file).map_err(store_error)?;
        if std::io::copy(&mut input.take(MAX_BINARY_BYTES + 1), &mut output).map_err(io_error)?
            > MAX_BINARY_BYTES
        {
            return Err(invalid(
                "acceptance ledger snapshot exceeded its byte limit",
            ));
        }
    }

    // Let SQLite reconstruct WAL coordination only in this disposable copy.
    for sidecar in [Path::new(&wal), Path::new(&shm)] {
        if !copy.contains_entry(sidecar).map_err(store_error)? {
            drop(copy.create_private_file(sidecar).map_err(store_error)?);
        }
    }
    drop(copy);
    let connection = Connection::open_with_flags(
        base.path().join(leaf).join(name),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|error| invalid(format!("open detached acceptance ledger: {error}")))?;
    let result = observe(&connection);
    drop(connection);
    drop(base);
    if result.is_ok() {
        temp.close().map_err(io_error)?;
    }
    result
}

fn invalid(message: impl Into<String>) -> AcceptanceError {
    AcceptanceError::Invalid(message.into())
}

fn io_error(error: std::io::Error) -> AcceptanceError {
    AcceptanceError::Io(format!("acceptance ledger snapshot: {error}"))
}

fn store_error(error: kio_core::KioError) -> AcceptanceError {
    AcceptanceError::Io(format!("acceptance ledger snapshot: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn observation_reads_committed_wal_frames_without_touching_live_sidecars() {
        let temp = tempfile::tempdir().unwrap();
        let outer = StoreDirectory::open(&temp.path().canonicalize().unwrap()).unwrap();
        let handle = outer.create_directory(Path::new("ledger")).unwrap();
        let source = StoreDirectory::from_retained(handle, outer.path().join("ledger")).unwrap();
        let name = "cost-ledger.sqlite";
        let path = source.path().join(name);
        drop(source.create_private_file(Path::new(name)).unwrap());
        let writer = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
        writer
            .execute_batch("CREATE TABLE evidence(value INTEGER);")
            .unwrap();
        for suffix in ["-wal", "-shm"] {
            let sidecar = format!("{name}{suffix}");
            drop(source.create_private_file(Path::new(&sidecar)).unwrap());
        }
        writer.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; INSERT INTO evidence VALUES (42);",
        ).unwrap();
        let wal = source.path().join(format!("{name}-wal"));
        assert!(fs::metadata(wal).unwrap().len() > 32);
        let state = || {
            [name, "cost-ledger.sqlite-wal", "cost-ledger.sqlite-shm"].map(|name| {
                match fs::read(source.path().join(name)) {
                    Ok(bytes) => Some(bytes),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => panic!("read live fixture {name}: {error}"),
                }
            })
        };
        let before = state();
        let read = |conn: &Connection| {
            conn.query_row("SELECT value FROM evidence", [], |row| row.get::<_, i32>(0))
                .map_err(|error| invalid(error.to_string()))
        };
        assert_eq!(with_snapshot(&path, read).unwrap(), 42);
        assert_eq!(state(), before);

        drop(writer);
        let before = state();
        assert!(before[1].is_none() && before[2].is_none());
        assert_eq!(with_snapshot(&path, read).unwrap(), 42);
        assert_eq!(state(), before);
    }
}
