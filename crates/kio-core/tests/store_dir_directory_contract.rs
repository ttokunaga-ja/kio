use std::{fs, path::Path};

use kio_core::store_dir::{Publication, StoreDirectory};

fn store_root() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("kio-store-directory-")
        .tempdir_in(std::env::current_dir().unwrap())
        .unwrap()
}

#[test]
fn directory_publication_quarantine_and_removal_stay_under_retained_root() {
    let root = store_root();
    let root_path = fs::canonicalize(root.path()).unwrap();
    let store = StoreDirectory::open(&root_path).unwrap();

    store.create_directory(Path::new("staged")).unwrap();
    store
        .write_atomic(
            Path::new("staged/manifest.json"),
            b"new",
            Publication::CreateOnly,
        )
        .unwrap();
    store.create_directory(Path::new("old")).unwrap();
    store
        .write_atomic(
            Path::new("old/manifest.json"),
            b"old",
            Publication::CreateOnly,
        )
        .unwrap();

    let quarantine = store.quarantine_directory(Path::new("old")).unwrap();
    assert!(!store.contains_entry(Path::new("old")).unwrap());
    store
        .rename_directory_create_only(Path::new("staged"), Path::new("published"))
        .unwrap();
    assert_eq!(
        store
            .read_optional(Path::new("published/manifest.json"), 16)
            .unwrap(),
        Some(b"new".to_vec())
    );
    store.remove_directory_all(&quarantine).unwrap();
    assert!(!store.contains_entry(&quarantine).unwrap());
}

#[test]
fn directory_rename_is_create_only_and_same_parent() {
    let root = store_root();
    let root_path = fs::canonicalize(root.path()).unwrap();
    let store = StoreDirectory::open(&root_path).unwrap();
    store.create_directory(Path::new("one")).unwrap();
    store.create_directory(Path::new("two")).unwrap();
    assert!(
        store
            .rename_directory_create_only(Path::new("one"), Path::new("two"))
            .is_err()
    );
    assert!(
        store
            .rename_directory_create_only(Path::new("one"), Path::new("nested/two"))
            .is_err()
    );
}

#[test]
fn directory_rename_between_retained_parents_is_create_only() {
    let root = store_root();
    let root_path = fs::canonicalize(root.path()).unwrap();
    let source = StoreDirectory::open(&root_path).unwrap();
    let destination_path = root_path.join("destination");
    fs::create_dir(&destination_path).unwrap();
    let destination = StoreDirectory::open(&destination_path).unwrap();
    source.create_directory(Path::new("staged")).unwrap();
    source
        .write_atomic(Path::new("staged/value"), b"kept", Publication::CreateOnly)
        .unwrap();
    source
        .rename_directory_between_create_only(
            Path::new("staged"),
            &destination,
            Path::new("published"),
        )
        .unwrap();
    assert_eq!(
        destination
            .read_optional(Path::new("published/value"), 16)
            .unwrap(),
        Some(b"kept".to_vec())
    );
    source
        .create_directory(Path::new("occupied-source"))
        .unwrap();
    destination.create_directory(Path::new("occupied")).unwrap();
    assert!(
        source
            .rename_directory_between_create_only(
                Path::new("occupied-source"),
                &destination,
                Path::new("occupied")
            )
            .is_err()
    );
    assert!(source.contains_entry(Path::new("occupied-source")).unwrap());
    assert!(destination.contains_entry(Path::new("occupied")).unwrap());
}

#[cfg(unix)]
#[test]
fn cross_parent_rename_rejects_symlink_endpoints_without_touching_outside() {
    use std::os::unix::fs::symlink;

    let root = store_root();
    let root_path = fs::canonicalize(root.path()).unwrap();
    let source = StoreDirectory::open(&root_path).unwrap();
    let destination_path = root_path.join("destination");
    fs::create_dir(&destination_path).unwrap();
    let destination = StoreDirectory::open(&destination_path).unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("sentinel"), b"outside").unwrap();

    symlink(outside.path(), root_path.join("linked-source")).unwrap();
    assert!(
        source
            .rename_directory_between_create_only(
                Path::new("linked-source"),
                &destination,
                Path::new("published"),
            )
            .is_err()
    );
    source.create_directory(Path::new("real-source")).unwrap();
    symlink(outside.path(), destination_path.join("linked-destination")).unwrap();
    assert!(
        source
            .rename_directory_between_create_only(
                Path::new("real-source"),
                &destination,
                Path::new("linked-destination"),
            )
            .is_err()
    );
    assert!(source.contains_entry(Path::new("real-source")).unwrap());
    assert_eq!(
        fs::read(outside.path().join("sentinel")).unwrap(),
        b"outside"
    );
}

#[cfg(unix)]
#[test]
fn cross_parent_rename_uses_retained_parents_after_named_parent_replacement() {
    let root = store_root();
    let root_path = fs::canonicalize(root.path()).unwrap();
    let source_path = root_path.join("source-parent");
    let destination_path = root_path.join("destination-parent");
    fs::create_dir(&source_path).unwrap();
    fs::create_dir(&destination_path).unwrap();
    let source = StoreDirectory::open(&source_path).unwrap();
    let destination = StoreDirectory::open(&destination_path).unwrap();
    source.create_directory(Path::new("payload")).unwrap();

    let moved_source = root_path.join("moved-source");
    fs::rename(&source_path, &moved_source).unwrap();
    fs::create_dir(&source_path).unwrap();
    fs::write(source_path.join("sentinel"), b"replacement").unwrap();
    let moved_destination = root_path.join("moved-destination");
    fs::rename(&destination_path, &moved_destination).unwrap();
    fs::create_dir(&destination_path).unwrap();
    fs::write(destination_path.join("sentinel"), b"replacement").unwrap();

    source
        .rename_directory_between_create_only(
            Path::new("payload"),
            &destination,
            Path::new("published"),
        )
        .unwrap();
    assert!(moved_destination.join("published").is_dir());
    assert!(!source_path.join("payload").exists());
    assert_eq!(
        fs::read(source_path.join("sentinel")).unwrap(),
        b"replacement"
    );
    assert_eq!(
        fs::read(destination_path.join("sentinel")).unwrap(),
        b"replacement"
    );
}

#[test]
fn recursive_directory_removal_has_a_fail_closed_depth_bound() {
    let root = store_root();
    let root_path = fs::canonicalize(root.path()).unwrap();
    let store = StoreDirectory::open(&root_path).unwrap();
    let mut deep = std::path::PathBuf::from("deep");
    store.create_directory(&deep).unwrap();
    for index in 0..128 {
        deep.push(format!("d{index}"));
        store.create_directory(&deep).unwrap();
    }
    assert!(store.remove_directory_all(Path::new("deep")).is_err());
}

#[cfg(unix)]
#[test]
fn recursive_directory_removal_rejects_symlink_entries() {
    use std::os::unix::fs::symlink;

    let root = store_root();
    let root_path = fs::canonicalize(root.path()).unwrap();
    let store = StoreDirectory::open(&root_path).unwrap();
    store.create_directory(Path::new("unsafe")).unwrap();
    let target = root_path.join("target");
    fs::write(&target, b"outside").unwrap();
    symlink(&target, root_path.join("unsafe/link")).unwrap();
    assert!(store.remove_directory_all(Path::new("unsafe")).is_err());
    assert!(target.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn retained_root_is_operation_capable_after_its_named_path_is_renamed() {
    let cleanup = store_root();
    let root = tempfile::tempdir_in(cleanup.path()).unwrap();
    let original = fs::canonicalize(root.path()).unwrap();
    let store = StoreDirectory::open(&original).unwrap();
    let renamed = original.with_extension("renamed");
    fs::rename(&original, &renamed).unwrap();

    store.create_directory(Path::new("private")).unwrap();
    store
        .write_atomic(
            Path::new("private/manifest.json"),
            b"retained",
            Publication::CreateOnly,
        )
        .unwrap();
    store.sync().unwrap();
    store.root_handle().sync_all().unwrap();

    assert_eq!(
        fs::read(renamed.join("private/manifest.json")).unwrap(),
        b"retained"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn retained_directory_normalization_accepts_namespace_timestamp_changes() {
    let root = store_root();
    let root_path = fs::canonicalize(root.path()).unwrap();
    let store = StoreDirectory::open(&root_path).unwrap();
    fs::write(root_path.join("external-entry"), b"changed namespace").unwrap();

    let revalidated =
        StoreDirectory::from_retained(store.root_handle().try_clone().unwrap(), root_path.clone())
            .unwrap();
    revalidated.create_directory(Path::new("private")).unwrap();
    revalidated.sync().unwrap();

    assert!(root_path.join("private").is_dir());
}
