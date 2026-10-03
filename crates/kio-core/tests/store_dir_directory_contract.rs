use std::{fs, path::Path};

use kio_core::store_dir::{Publication, StoreDirectory};

fn store_root() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("kio-store-directory-")
        .tempdir_in(std::env::current_dir().unwrap())
        .unwrap()
}

#[cfg(windows)]
fn full_directory_identity(file: &std::fs::File) -> (u64, [u8; 16]) {
    use std::{mem, os::windows::io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
    };

    kio_core::cas::windows_directory_handle_identity(file)
        .expect("retained handle is a real non-reparse directory");
    let mut identity = FILE_ID_INFO::default();
    assert_ne!(
        unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle() as _,
                FileIdInfo,
                (&mut identity as *mut FILE_ID_INFO).cast(),
                mem::size_of::<FILE_ID_INFO>() as u32,
            )
        },
        0,
        "cannot inspect full directory identity"
    );
    (identity.VolumeSerialNumber, identity.FileId.Identifier)
}

fn assert_same_directory_identity(held: &StoreDirectory, published: &StoreDirectory) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let held = held.root_handle().metadata().unwrap();
        let published = published.root_handle().metadata().unwrap();
        assert_eq!((held.dev(), held.ino()), (published.dev(), published.ino()));
    }
    #[cfg(windows)]
    {
        let held = full_directory_identity(&held.root_handle());
        let published = full_directory_identity(&published.root_handle());
        assert_eq!(held, published);
    }
}

#[cfg(windows)]
#[test]
fn retained_repository_allows_namespace_moves_but_rejects_named_authority_replacement() {
    use kio_core::{
        cas::{ContentObjectKind, ObjectKind},
        management::ManagementBinding,
        scope::Repository,
    };

    let fixture = store_root();
    let fixture_path = fs::canonicalize(fixture.path()).unwrap();
    let root_path = fixture_path.join("scope");
    fs::create_dir(&root_path).unwrap();
    drop(Repository::init(&root_path).unwrap());

    // Supply the same raw, delete-share-denying handles as cap-primitives
    // callers. The repository must consume and normalize both originals.
    let scope =
        cap_primitives::fs::open_ambient_dir(&root_path, cap_primitives::ambient_authority())
            .unwrap();
    let kio = cap_primitives::fs::open_dir_nofollow(&scope, Path::new(".kio")).unwrap();
    let repo = Repository::open_bound_for_recovery(root_path.clone(), scope, kio).unwrap();
    let old_scope_id = repo.scope_identity().unwrap().scope_id;
    let root_id = full_directory_identity(repo.bound_root_handle().unwrap());
    let kio_id = full_directory_identity(repo.bound_kio_handle().unwrap());
    let binding = ManagementBinding::bind(&root_path).unwrap();
    let bytes = b"CAS bytes from the retained original store";
    let hash = repo.object_store().write_raw(bytes).unwrap();
    // Bind a historically lazy content namespace while the repository lives.
    let image_bytes = b"image bytes from the retained lazy namespace";
    let image_hash = repo
        .object_store()
        .write_content_object(ContentObjectKind::Image, image_bytes)
        .unwrap();

    fs::rename(root_path.join(".kio"), root_path.join("retained-kio")).unwrap();
    let replacement = Repository::init(&root_path).unwrap();
    assert_ne!(replacement.scope_identity().unwrap().scope_id, old_scope_id);
    assert!(binding.revalidate().is_err());
    assert!(repo.scope_identity().is_err());
    assert_eq!(
        full_directory_identity(repo.bound_kio_handle().unwrap()),
        kio_id
    );
    let retained_kio = StoreDirectory::from_retained(
        repo.bound_kio_handle().unwrap().try_clone().unwrap(),
        root_path.join(".kio"),
    )
    .unwrap();
    let scope_json = retained_kio
        .read_optional(Path::new("scope.json"), 4096)
        .unwrap()
        .unwrap();
    let scope_json: serde_json::Value = serde_json::from_slice(&scope_json).unwrap();
    assert_eq!(scope_json["scope_id"].as_str().unwrap(), old_scope_id);
    assert_eq!(
        repo.object_store()
            .read_object(ObjectKind::Raw, &hash)
            .unwrap()
            .bytes,
        bytes
    );
    assert_eq!(
        repo.object_store()
            .read_content_object_bytes(ContentObjectKind::Image, &image_hash, 4096)
            .unwrap(),
        image_bytes
    );
    assert!(
        replacement
            .object_store()
            .read_object(ObjectKind::Raw, &hash)
            .is_err()
    );

    fs::rename(&root_path, fixture_path.join("retained-scope")).unwrap();
    fs::create_dir(&root_path).unwrap();
    fs::write(root_path.join("sentinel"), b"replacement root").unwrap();
    assert_eq!(
        full_directory_identity(repo.bound_root_handle().unwrap()),
        root_id
    );
    assert!(binding.revalidate().is_err());
    assert_eq!(
        repo.object_store()
            .read_object(ObjectKind::Raw, &hash)
            .unwrap()
            .bytes,
        bytes
    );
    assert_eq!(
        repo.object_store()
            .read_content_object_bytes(ContentObjectKind::Image, &image_hash, 4096)
            .unwrap(),
        image_bytes
    );
    assert_eq!(
        fs::read(root_path.join("sentinel")).unwrap(),
        b"replacement root"
    );
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
    let held_staged = StoreDirectory::open(&root_path.join("staged")).unwrap();

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
    assert_eq!(
        held_staged
            .read_optional(Path::new("manifest.json"), 16)
            .unwrap(),
        Some(b"new".to_vec())
    );
    let published = StoreDirectory::open(&root_path.join("published")).unwrap();
    assert_same_directory_identity(&held_staged, &published);
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
    let held_staged = StoreDirectory::open(&root_path.join("staged")).unwrap();
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
    assert_eq!(
        held_staged.read_optional(Path::new("value"), 16).unwrap(),
        Some(b"kept".to_vec())
    );
    let published = StoreDirectory::open(&destination_path.join("published")).unwrap();
    assert_same_directory_identity(&held_staged, &published);
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

#[cfg(any(unix, windows))]
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
    source
        .write_atomic(
            Path::new("payload/value"),
            b"original payload",
            Publication::CreateOnly,
        )
        .unwrap();

    let moved_source = root_path.join("moved-source");
    fs::rename(&source_path, &moved_source).unwrap();
    fs::create_dir(&source_path).unwrap();
    fs::write(source_path.join("sentinel"), b"replacement").unwrap();
    let readopted_source = StoreDirectory::from_retained(
        source.root_handle().try_clone().unwrap(),
        source_path.clone(),
    )
    .unwrap();
    assert_same_directory_identity(&source, &readopted_source);
    let moved_destination = root_path.join("moved-destination");
    fs::rename(&destination_path, &moved_destination).unwrap();
    fs::create_dir(&destination_path).unwrap();
    fs::write(destination_path.join("sentinel"), b"replacement").unwrap();
    let readopted_destination = StoreDirectory::from_retained(
        destination.root_handle().try_clone().unwrap(),
        destination_path.clone(),
    )
    .unwrap();
    assert_same_directory_identity(&destination, &readopted_destination);

    readopted_source
        .rename_directory_between_create_only(
            Path::new("payload"),
            &readopted_destination,
            Path::new("published"),
        )
        .unwrap();
    assert!(moved_destination.join("published").is_dir());
    assert_eq!(
        fs::read(moved_destination.join("published/value")).unwrap(),
        b"original payload"
    );
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
