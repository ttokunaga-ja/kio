use std::path::Path;

use kio_core::store_dir::{AtomicWorkspaceState, Publication, StoreDirectory};

fn store(path: &Path) -> StoreDirectory {
    std::fs::create_dir_all(path).unwrap();
    StoreDirectory::open(path).unwrap()
}

#[test]
fn retained_owner_publishes_a_regular_file_into_a_distinct_retained_parent() {
    let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let owner = store(&root.path().join("owner"));
    let target = store(&root.path().join("target"));

    target
        .write_atomic_with_owner(
            &owner,
            Path::new("published"),
            b"new",
            Publication::CreateOnly,
        )
        .unwrap();

    assert_eq!(
        target.read_optional(Path::new("published"), 16).unwrap(),
        Some(b"new".to_vec())
    );
    assert_eq!(owner.inspect_atomic().unwrap(), AtomicWorkspaceState::Clean);
}

#[test]
fn create_only_does_not_replace_an_existing_regular_file() {
    let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let owner = store(&root.path().join("owner"));
    let target = store(&root.path().join("target"));
    target
        .write_atomic_with_owner(
            &owner,
            Path::new("published"),
            b"old",
            Publication::CreateOnly,
        )
        .unwrap();

    assert!(
        target
            .write_atomic_with_owner(
                &owner,
                Path::new("published"),
                b"new",
                Publication::CreateOnly
            )
            .is_err()
    );
    assert_eq!(
        target.read_optional(Path::new("published"), 16).unwrap(),
        Some(b"old".to_vec())
    );
}

#[test]
fn upsert_replaces_a_verified_regular_file_across_retained_parents() {
    let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let owner = store(&root.path().join("owner"));
    let target = store(&root.path().join("target"));
    target
        .write_atomic_with_owner(
            &owner,
            Path::new("published"),
            b"old",
            Publication::CreateOnly,
        )
        .unwrap();

    target
        .write_atomic_with_owner(&owner, Path::new("published"), b"new", Publication::Upsert)
        .unwrap();

    assert_eq!(
        target.read_optional(Path::new("published"), 16).unwrap(),
        Some(b"new".to_vec())
    );
}

#[test]
fn private_gate_rejects_a_nonempty_existing_regular_file() {
    let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let store = store(root.path());
    store
        .write_atomic(Path::new("gate"), b"occupied", Publication::CreateOnly)
        .unwrap();

    assert!(store.lock_private_gate(Path::new("gate")).is_err());
}

#[cfg(windows)]
#[test]
fn atomic_write_rejects_ads_leaf_before_creating_a_workspace() {
    let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let owner = store(&root.path().join("owner"));
    let target = store(&root.path().join("target"));
    std::fs::write(root.path().join("target/existing"), b"base").unwrap();

    assert!(
        target
            .write_atomic_with_owner(
                &owner,
                Path::new("existing:stream"),
                b"replacement",
                Publication::Replace,
            )
            .is_err()
    );

    assert_eq!(
        std::fs::read(root.path().join("target/existing")).unwrap(),
        b"base"
    );
    assert!(!root.path().join("owner/.kio-atomic").exists());
    assert_eq!(
        owner.inspect_atomic().unwrap(),
        AtomicWorkspaceState::Absent
    );
}
