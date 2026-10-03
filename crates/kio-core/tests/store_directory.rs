use std::{fs, path::Path};

use kio_core::store_dir::{Publication, StoreDirectory};

#[test]
fn entries_classify_normal_directories_and_regular_files() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let store = StoreDirectory::open(&root).unwrap();
    assert!(store.entries(Path::new("")).unwrap().is_empty());

    fs::create_dir(root.join("directory")).unwrap();
    fs::write(root.join("regular"), b"ordinary file").unwrap();
    fs::write(root.join("directory/nested"), b"nested file").unwrap();

    let entries = store.entries(Path::new("")).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "directory");
    assert!(entries[0].is_directory);
    assert!(!entries[0].is_regular_file);
    assert_eq!(entries[1].name, "regular");
    assert!(!entries[1].is_directory);
    assert!(entries[1].is_regular_file);

    let nested = store.entries(Path::new("directory")).unwrap();
    assert_eq!(nested.len(), 1);
    assert_eq!(nested[0].name, "nested");
    assert!(nested[0].is_regular_file);
    assert!(!nested[0].is_directory);
}

#[cfg(unix)]
#[test]
fn direct_lookup_does_not_open_siblings_and_regular_io_rejects_fifos() {
    use std::{ffi::CString, os::unix::fs::symlink};

    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let store = StoreDirectory::open(&root).unwrap();
    fs::write(root.join("original"), b"private").unwrap();
    fs::hard_link(root.join("original"), root.join("alias")).unwrap();
    symlink(root.join("missing"), root.join("dangling")).unwrap();
    let fifo = CString::new(root.join("pipe").as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

    assert!(!store.contains_entry(Path::new(".kio")).unwrap());
    assert!(store.contains_entry(Path::new("dangling")).unwrap());
    assert!(store.contains_entry(Path::new("pipe")).unwrap());
    assert!(store.contains_entry(Path::new("../escape")).is_err());
    assert!(store.read_optional(Path::new("pipe"), 16).is_err());
    assert!(store.open_regular_read(Path::new("pipe"), 16).is_err());
    assert!(store.entries(Path::new("")).is_err());

    // Init only owns .kio, and must ignore unsafe unrelated working entries.
    let repo = kio_core::scope::Repository::init(&root).unwrap();
    assert!(repo.head_commit_hash().unwrap().is_none());
    assert_eq!(fs::read(root.join("alias")).unwrap(), b"private");
}

#[cfg(unix)]
#[test]
fn retained_handles_are_independent_of_cwd_and_parent_replacement() {
    let left = tempfile::tempdir().unwrap();
    let right = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let left_root = left.path().canonicalize().unwrap();
    let right_root = right.path().canonicalize().unwrap();
    let left_store = left_root.join(".kio");
    let right_store = right_root.join(".kio");
    fs::create_dir(&left_store).unwrap();
    fs::create_dir(&right_store).unwrap();
    let a = StoreDirectory::open(&left_store).unwrap();
    let b = StoreDirectory::open(&right_store).unwrap();
    a.write_atomic(Path::new("state"), b"left", Publication::CreateOnly)
        .unwrap();
    b.write_atomic(Path::new("state"), b"right", Publication::CreateOnly)
        .unwrap();

    let cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(outside.path()).unwrap();
    assert_eq!(
        a.read_optional(Path::new("state"), 16).unwrap(),
        Some(b"left".to_vec())
    );
    assert_eq!(
        b.read_optional(Path::new("state"), 16).unwrap(),
        Some(b"right".to_vec())
    );
    std::env::set_current_dir(cwd).unwrap();

    let displaced = left.path().join("displaced");
    fs::rename(&left_store, &displaced).unwrap();
    fs::create_dir(&left_store).unwrap();
    fs::write(left_store.join("state"), b"outside").unwrap();
    a.write_atomic(Path::new("next"), b"retained", Publication::CreateOnly)
        .unwrap();
    assert_eq!(fs::read(displaced.join("next")).unwrap(), b"retained");
    assert!(!left_store.join("next").exists());
}

#[cfg(unix)]
#[test]
fn rejects_unsafe_leaf_and_keeps_create_only_atomic_with_bounded_reads() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join(".kio");
    fs::create_dir(&root).unwrap();
    let store = StoreDirectory::open(&root).unwrap();
    let outside = dir.path().join("outside");
    fs::write(&outside, b"outside").unwrap();
    symlink(&outside, root.join("symlink")).unwrap();
    assert!(store.read_optional(Path::new("symlink"), 32).is_err());
    fs::hard_link(&outside, root.join("hardlink")).unwrap();
    assert!(store.open_regular_read(Path::new("hardlink"), 32).is_err());

    store
        .write_atomic(Path::new("journal"), b"one", Publication::CreateOnly)
        .unwrap();
    assert!(
        store
            .write_atomic(Path::new("journal"), b"two", Publication::CreateOnly)
            .is_err()
    );
    assert_eq!(
        store.read_optional(Path::new("journal"), 3).unwrap(),
        Some(b"one".to_vec())
    );
    assert!(store.read_optional(Path::new("journal"), 2).is_err());
    store
        .write_atomic(Path::new("journal"), b"replacement", Publication::Replace)
        .unwrap();
    assert_eq!(
        store.read_optional(Path::new("journal"), 32).unwrap(),
        Some(b"replacement".to_vec())
    );
}

#[cfg(unix)]
#[test]
fn refuses_a_symlinked_store_leaf_even_when_target_is_a_valid_store() {
    use std::os::unix::fs::symlink;
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let real = root.join("real-store");
    fs::create_dir(&real).unwrap();
    symlink(&real, root.join(".kio")).unwrap();
    assert!(StoreDirectory::open(&root.join(".kio")).is_err());
}

#[cfg(unix)]
#[test]
fn optional_paths_accept_missing_parents_but_reject_symlinked_parents() {
    use std::os::unix::fs::symlink;
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap().join(".kio");
    fs::create_dir(&root).unwrap();
    let store = StoreDirectory::open(&root).unwrap();
    assert_eq!(
        store
            .read_optional(Path::new("publication-v1.json"), 64)
            .unwrap(),
        None
    );
    assert_eq!(
        store.read_optional(Path::new("missing/leaf"), 64).unwrap(),
        None
    );
    assert_eq!(
        store.entries_optional(Path::new("missing/dir")).unwrap(),
        None
    );
    let target = fixture.path().join("target");
    fs::create_dir(&target).unwrap();
    symlink(&target, root.join("missing")).unwrap();
    assert!(store.read_optional(Path::new("missing/leaf"), 64).is_err());
    assert!(store.entries_optional(Path::new("missing/dir")).is_err());
}

#[cfg(unix)]
#[test]
fn create_directory_is_exclusive_private_and_refuses_existing_links() {
    use std::os::unix::fs::{MetadataExt, symlink};
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap().join(".kio");
    fs::create_dir(&root).unwrap();
    let store = StoreDirectory::open(&root).unwrap();
    let created = store.create_directory(Path::new("fresh")).unwrap();
    assert!(created.metadata().unwrap().is_dir());
    assert_eq!(created.metadata().unwrap().mode() & 0o077, 0);
    fs::write(root.join("fresh/kept"), b"unchanged").unwrap();
    assert!(store.create_directory(Path::new("fresh")).is_err());
    assert_eq!(fs::read(root.join("fresh/kept")).unwrap(), b"unchanged");
    let target = fixture.path().join("target");
    fs::create_dir(&target).unwrap();
    symlink(&target, root.join("linked")).unwrap();
    assert!(store.create_directory(Path::new("linked")).is_err());
    assert!(target.read_dir().unwrap().next().is_none());
}

#[cfg(windows)]
#[test]
fn windows_retained_store_uses_nofollow_bounded_atomic_operations() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join(".kio");
    fs::create_dir(&root).unwrap();
    let store = StoreDirectory::open(&root).unwrap();
    store
        .create_directory_all(Path::new("state/journal"))
        .unwrap();
    store
        .write_atomic(
            Path::new("state/journal/HEAD"),
            b"one",
            Publication::CreateOnly,
        )
        .unwrap();
    assert!(
        store
            .write_atomic(
                Path::new("state/journal/HEAD"),
                b"two",
                Publication::CreateOnly
            )
            .is_err()
    );
    store
        .write_atomic(
            Path::new("state/journal/HEAD"),
            b"two",
            Publication::Replace,
        )
        .unwrap();
    assert_eq!(
        store
            .read_optional(Path::new("state/journal/HEAD"), 8)
            .unwrap(),
        Some(b"two".to_vec())
    );
}
