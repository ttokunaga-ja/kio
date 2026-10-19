//! Shared temporary roots for CLI fixtures with private device state.

#[allow(dead_code)]
pub fn canonical_tempdir() -> tempfile::TempDir {
    // Resolve the system root before creation: macOS /var is a symlink.
    // Preserve each fixture's child paths, including deliberate symlink targets.
    tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap()
}

/// Create a new private fixture directory through a retained parent.
/// Existing names fail closed; this never repairs an existing directory's ACL.
#[allow(dead_code)]
pub fn create_private_child_dir(parent: &std::path::Path, leaf: &str) -> std::path::PathBuf {
    use kio_core::store_dir::StoreDirectory;
    let parent = parent.canonicalize().unwrap();
    let directory = StoreDirectory::open(&parent).unwrap();
    let created = directory
        .create_directory(std::path::Path::new(leaf))
        .unwrap();
    let path = parent.join(leaf);
    // Validate the retained result before exposing its path to fixture consumers.
    let _created = StoreDirectory::from_retained(created, path.clone()).unwrap();
    path
}

/// Publish a new private fixture file within an already private parent.
#[cfg(windows)]
#[allow(dead_code)]
pub fn write_private_fixture_file(parent: &std::path::Path, leaf: &str, bytes: &[u8]) {
    use kio_core::store_dir::{Publication, StoreDirectory};
    StoreDirectory::open(parent)
        .unwrap()
        .write_atomic(std::path::Path::new(leaf), bytes, Publication::CreateOnly)
        .unwrap();
}

/// Prepare intentional raw SQLite fixture access without repairing occupied names.
#[allow(dead_code)]
pub fn prepare_private_sqlite_sidecars(path: &std::path::Path) -> kio_core::Result<()> {
    #[cfg(windows)]
    {
        use kio_core::platform::verify_owner_private_handle;
        use kio_core::store_dir::{Publication, StoreDirectory};
        use std::path::{Path, PathBuf};

        let parent = path.parent().expect("fixture database has a parent");
        let leaf = path.file_name().expect("fixture database has a leaf");
        let directory = StoreDirectory::open(parent)?;
        verify_owner_private_handle(&directory.root_handle())?;
        let main = Path::new(leaf);
        let main_handle = directory.open_regular_read(main, u64::MAX)?;
        verify_owner_private_handle(&main_handle)?;
        let sidecars: [PathBuf; 2] = ["-wal", "-shm"].map(|suffix| {
            let mut name = leaf.to_os_string();
            name.push(suffix);
            PathBuf::from(name)
        });

        // Validate every occupied name before creating any missing name.
        let mut missing = Vec::new();
        for sidecar in &sidecars {
            if directory.contains_entry(sidecar)? {
                let file = directory.open_regular_read(sidecar, u64::MAX)?;
                verify_owner_private_handle(&file)?;
            } else {
                missing.push(sidecar);
            }
        }
        for sidecar in missing {
            directory.write_atomic(sidecar, &[], Publication::CreateOnly)?;
        }

        // Recheck both existing and newly published names, plus the parent.
        verify_owner_private_handle(&directory.root_handle())?;
        for name in std::iter::once(main).chain(sidecars.iter().map(PathBuf::as_path)) {
            let file = directory.open_regular_read(name, u64::MAX)?;
            verify_owner_private_handle(&file)?;
        }
    }
    #[cfg(not(windows))]
    let _ = path;
    Ok(())
}

/// Inspect a completed CLI invocation without creating SQLite coordination files.
/// Drop this snapshot before another invocation and reopen it for fresh assertions.
#[allow(dead_code)]
pub fn immutable_sqlite(path: impl AsRef<std::path::Path>) -> rusqlite::Connection {
    let path = path.as_ref().canonicalize().unwrap();
    let path = path.to_str().expect("fixture database path must be UTF-8");
    // Canonical Windows paths use verbatim prefixes; SQLite file URIs use
    // forward slashes, an empty authority and an absolute drive/UNC path.
    let path = if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        format!("//{}", unc.replace('\\', "/"))
    } else if let Some(verbatim) = path.strip_prefix(r"\\?\") {
        format!("/{}", verbatim.replace('\\', "/"))
    } else if path.as_bytes().get(1) == Some(&b':') {
        format!("/{}", path.replace('\\', "/"))
    } else {
        path.to_owned()
    };
    let mut uri = String::from("file://");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b':' | b'-' | b'_' | b'.' | b'~') {
            uri.push(char::from(byte));
        } else {
            use std::fmt::Write;
            write!(uri, "%{byte:02X}").unwrap();
        }
    }
    uri.push_str("?mode=ro&immutable=1");
    rusqlite::Connection::open_with_flags(
        uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .unwrap()
}
