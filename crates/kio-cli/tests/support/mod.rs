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

/// Configure private home directories for a CLI fixture.
#[allow(dead_code)]
pub fn home_env(root: &std::path::Path) -> [(&'static str, std::path::PathBuf); 8] {
    [
        ("HOME", root.to_path_buf()),
        ("USERPROFILE", root.to_path_buf()),
        ("APPDATA", root.join(".test-config")),
        ("LOCALAPPDATA", root.join(".test-data")),
        ("XDG_CONFIG_HOME", root.join(".test-config")),
        ("XDG_DATA_HOME", root.join(".test-data")),
        ("XDG_STATE_HOME", root.join(".test-state")),
        ("XDG_CACHE_HOME", root.join(".test-cache")),
    ]
}

/// Publish the already-indexed generation under the renamed working filename.
#[allow(dead_code)]
pub fn bind_renamed_normalize(root: &std::path::Path, old_name: &str, new_name: &str) {
    use kio_core::scope::{PendingNormalizeRef, Repository};
    use std::collections::BTreeSet;
    let repo = Repository::open(root).unwrap();
    let head = repo
        .read_commit(&repo.head_commit_hash().unwrap().unwrap())
        .unwrap();
    let tree = repo.read_tree(&head.tree).unwrap();
    assert!(
        tree.find_entry(old_name)
            .unwrap()
            .unwrap()
            .normalize
            .is_some()
    );
    std::fs::rename(root.join(old_name), root.join(new_name)).unwrap();
    let references = tree
        .entries
        .iter()
        .map(Result::unwrap)
        .filter_map(|entry| {
            let path = if entry.path == old_name {
                new_name.to_owned()
            } else {
                entry.path
            };
            Some((
                path,
                PendingNormalizeRef {
                    expected_raw_hash: entry.raw_hash,
                    normalize: entry.normalize?,
                },
            ))
        })
        .collect();
    repo.auto_snapshot_with_bound_normalize(
        None,
        None,
        &BTreeSet::new(),
        &references,
        &BTreeSet::new(),
        None,
    )
    .unwrap();
}

/// 05 §1.7.2 / §4.2 (2026-08-11): `kio view --json` stopped returning a
/// `text` field -- it resolves to a full-text normalized view path plus a
/// view-local byte span instead. This is the only way left to recover
/// chunk-adjacent text from `view`'s JSON, and exercising it (read the file,
/// slice by the reported offsets) is what actually proves the unit-local ->
/// view-local offset translation is correct, unlike a plain field-content
/// assertion on the old `text` field ever did.
#[allow(dead_code)]
pub fn view_slice(viewed: &serde_json::Value) -> String {
    let view_path = viewed["view_path"]
        .as_str()
        .unwrap_or_else(|| panic!("view_path must be a resolvable path: {viewed}"));
    let start = viewed["view_byte_start"]
        .as_u64()
        .unwrap_or_else(|| panic!("view_byte_start must be present: {viewed}"))
        as usize;
    let end = viewed["view_byte_end"]
        .as_u64()
        .unwrap_or_else(|| panic!("view_byte_end must be present: {viewed}"))
        as usize;
    let bytes = std::fs::read(view_path)
        .unwrap_or_else(|err| panic!("failed to read view_path {view_path}: {err}"));
    String::from_utf8(bytes[start..end].to_vec()).unwrap()
}
