//! Shared temporary roots for CLI fixtures with private device state.

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
        use kio_core::private_fs::verify_owner_private_handle;
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
