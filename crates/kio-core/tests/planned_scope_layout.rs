use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::fs;

use kio_core::{
    scope::{
        complete_planned_kio_layout, initialize_planned_kio_layout, validate_planned_kio_layout,
    },
    store_dir::{Publication, StoreDirectory},
};

const SCOPE_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

fn planned_stage() -> (tempfile::TempDir, PathBuf, StoreDirectory) {
    let fixture = tempfile::tempdir().unwrap();
    let parent_root = fixture.path().canonicalize().unwrap();
    let parent = StoreDirectory::open(&parent_root).unwrap();
    let stage = parent.create_directory(Path::new("stage")).unwrap();
    let root = parent_root.join("stage");
    let directory = StoreDirectory::from_retained(stage, root.clone()).unwrap();
    (fixture, root, directory)
}

fn read(directory: &StoreDirectory, name: &str) -> Option<Vec<u8>> {
    directory.read_optional(Path::new(name), 4096).unwrap()
}

fn entries(directory: &StoreDirectory, path: &str) -> Vec<String> {
    directory
        .entries(Path::new(path))
        .unwrap()
        .into_iter()
        .map(|entry| entry.name.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn validate_planned_layout_accepts_empty_partial_and_complete_stages_without_changes() {
    let (_fixture, root, directory) = planned_stage();
    let empty_before = entries(&directory, "");
    validate_planned_kio_layout(&directory, &root, SCOPE_ID, &[]).unwrap();
    assert_eq!(entries(&directory, ""), empty_before);

    let (_fixture, root, directory) = planned_stage();
    directory
        .write_atomic(Path::new("HEAD"), b"unborn\n", Publication::CreateOnly)
        .unwrap();
    directory
        .create_directory_all(Path::new("objects/raw"))
        .unwrap();
    let partial_before = entries(&directory, "");
    validate_planned_kio_layout(&directory, &root, SCOPE_ID, &[]).unwrap();
    assert_eq!(entries(&directory, ""), partial_before);
    assert_eq!(entries(&directory, "objects"), vec!["raw".to_owned()]);
    assert_eq!(read(&directory, "scope.json"), None);

    let (_fixture, root, directory) = planned_stage();
    let aux = [("management.json", b"planned-management".as_slice())];
    complete_planned_kio_layout(&directory, &root, SCOPE_ID, &aux).unwrap();
    let complete_before = entries(&directory, "");
    validate_planned_kio_layout(&directory, &root, SCOPE_ID, &aux).unwrap();
    assert_eq!(entries(&directory, ""), complete_before);
    assert_eq!(
        read(&directory, "management.json"),
        Some(b"planned-management".to_vec())
    );
}

#[test]
fn complete_planned_layout_fills_only_missing_expected_entries() {
    let (_fixture, root, directory) = planned_stage();
    directory
        .write_atomic(Path::new("HEAD"), b"unborn\n", Publication::CreateOnly)
        .unwrap();
    directory
        .create_directory_all(Path::new("objects/raw"))
        .unwrap();

    let aux = [("management.json", b"planned-management".as_slice())];
    complete_planned_kio_layout(&directory, &root, SCOPE_ID, &aux).unwrap();

    assert_eq!(read(&directory, "HEAD"), Some(b"unborn\n".to_vec()));
    assert_eq!(
        read(&directory, "management.json"),
        Some(b"planned-management".to_vec())
    );
    for path in [
        "objects/raw",
        "objects/trees",
        "objects/commits",
        "logs",
        "refs/tags-v1",
    ] {
        assert!(directory.open_directory(Path::new(path)).is_ok(), "{path}");
    }

    complete_planned_kio_layout(&directory, &root, SCOPE_ID, &aux).unwrap();
}

#[test]
fn planned_layout_validates_and_completes_with_an_empty_auxiliary_gate_locked() {
    let (_fixture, root, directory) = planned_stage();
    let gate_name = Path::new(".root-init-gate");
    let gate = directory.lock_private_gate(gate_name).unwrap();
    directory.ensure_owner_private(gate_name).unwrap();
    let aux = [(".root-init-gate", b"".as_slice())];

    let assert_gate_locked = || {
        let contender = directory.open_regular_read(gate_name, 0).unwrap();
        assert!(matches!(
            contender.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        assert_eq!(gate.metadata().unwrap().len(), 0);
    };
    assert_gate_locked();
    validate_planned_kio_layout(&directory, &root, SCOPE_ID, &aux).unwrap();
    assert_gate_locked();
    for _ in 0..2 {
        complete_planned_kio_layout(&directory, &root, SCOPE_ID, &aux).unwrap();
        validate_planned_kio_layout(&directory, &root, SCOPE_ID, &aux).unwrap();
        assert_gate_locked();
    }
}

#[test]
fn planned_layout_rejects_auxiliary_content_mismatches_before_writing() {
    for (actual, expected) in [
        (b"nonempty".as_slice(), b"".as_slice()),
        (b"wrong".as_slice(), b"right".as_slice()),
        (b"".as_slice(), b"right".as_slice()),
    ] {
        let (_fixture, root, directory) = planned_stage();
        directory
            .write_atomic(Path::new("auxiliary"), actual, Publication::CreateOnly)
            .unwrap();
        let aux = [("auxiliary", expected)];

        assert!(validate_planned_kio_layout(&directory, &root, SCOPE_ID, &aux).is_err());
        assert!(complete_planned_kio_layout(&directory, &root, SCOPE_ID, &aux).is_err());
        assert_eq!(read(&directory, "auxiliary"), Some(actual.to_vec()));
        assert_eq!(read(&directory, "HEAD"), None);
    }
}

#[test]
fn planned_layout_rejects_an_empty_auxiliary_directory_before_writing() {
    let (_fixture, root, directory) = planned_stage();
    directory.create_directory(Path::new("auxiliary")).unwrap();
    let aux = [("auxiliary", b"".as_slice())];

    assert!(validate_planned_kio_layout(&directory, &root, SCOPE_ID, &aux).is_err());
    assert!(complete_planned_kio_layout(&directory, &root, SCOPE_ID, &aux).is_err());
    assert_eq!(read(&directory, "HEAD"), None);
}

#[cfg(unix)]
#[test]
fn planned_layout_rejects_empty_auxiliary_links_before_writing() {
    use std::os::unix::fs::symlink;

    for hard_link in [false, true] {
        let (_fixture, root, directory) = planned_stage();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("empty");
        fs::write(&target, b"").unwrap();
        if hard_link {
            fs::hard_link(&target, root.join("auxiliary")).unwrap();
        } else {
            symlink(&target, root.join("auxiliary")).unwrap();
        }
        let aux = [("auxiliary", b"".as_slice())];

        assert!(validate_planned_kio_layout(&directory, &root, SCOPE_ID, &aux).is_err());
        assert!(complete_planned_kio_layout(&directory, &root, SCOPE_ID, &aux).is_err());
        assert_eq!(read(&directory, "HEAD"), None);
        assert!(fs::read(&target).unwrap().is_empty());
    }
}

#[test]
fn complete_planned_layout_rejects_bad_or_unknown_state_before_writing() {
    let (_fixture, root, directory) = planned_stage();
    directory
        .write_atomic(Path::new("HEAD"), b"wrong", Publication::CreateOnly)
        .unwrap();

    assert!(validate_planned_kio_layout(&directory, &root, SCOPE_ID, &[]).is_err());
    assert!(complete_planned_kio_layout(&directory, &root, SCOPE_ID, &[]).is_err());
    assert_eq!(read(&directory, "HEAD"), Some(b"wrong".to_vec()));
    assert_eq!(read(&directory, "scope.json"), None);

    let (_fixture, root, directory) = planned_stage();
    directory
        .write_atomic(Path::new("unrelated"), b"keep", Publication::CreateOnly)
        .unwrap();
    assert!(validate_planned_kio_layout(&directory, &root, SCOPE_ID, &[]).is_err());
    assert!(complete_planned_kio_layout(&directory, &root, SCOPE_ID, &[]).is_err());
    assert_eq!(read(&directory, "unrelated"), Some(b"keep".to_vec()));
    assert_eq!(read(&directory, "HEAD"), None);
}

#[cfg(unix)]
#[test]
fn complete_planned_layout_rejects_links_without_writing() {
    use std::os::unix::fs::symlink;

    let (_fixture, root, directory) = planned_stage();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), root.join("objects")).unwrap();

    assert!(complete_planned_kio_layout(&directory, &root, SCOPE_ID, &[]).is_err());
    assert_eq!(read(&directory, "HEAD"), None);
    assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
}

#[test]
fn complete_planned_layout_rejects_duplicate_or_builtin_auxiliary_names() {
    let (_fixture, root, directory) = planned_stage();
    assert!(
        complete_planned_kio_layout(&directory, &root, SCOPE_ID, &[("HEAD", b"replacement")],)
            .is_err()
    );
    assert_eq!(read(&directory, "HEAD"), None);

    let (_fixture, root, directory) = planned_stage();
    assert!(
        complete_planned_kio_layout(&directory, &root, SCOPE_ID, &[("objects", b"not-a-file")],)
            .is_err()
    );
    assert_eq!(read(&directory, "HEAD"), None);

    let (_fixture, root, directory) = planned_stage();
    assert!(
        complete_planned_kio_layout(
            &directory,
            &root,
            SCOPE_ID,
            &[("management.json", b"one"), ("management.json", b"two")],
        )
        .is_err()
    );
    assert_eq!(read(&directory, "HEAD"), None);

    let (_fixture, root, directory) = planned_stage();
    assert!(
        complete_planned_kio_layout(
            &directory,
            &root,
            SCOPE_ID,
            &[("nested/marker", b"not-a-leaf")],
        )
        .is_err()
    );
    assert_eq!(read(&directory, "HEAD"), None);

    let (_fixture, root, directory) = planned_stage();
    assert!(
        complete_planned_kio_layout(
            &directory,
            &root,
            SCOPE_ID,
            &[("marker\0suffix", b"not-a-leaf")],
        )
        .is_err()
    );
    assert_eq!(read(&directory, "HEAD"), None);
}

#[test]
fn initializer_refuses_a_second_create_only_invocation() {
    let (_fixture, root, directory) = planned_stage();
    initialize_planned_kio_layout(&directory, &root, SCOPE_ID).unwrap();

    assert!(initialize_planned_kio_layout(&directory, &root, SCOPE_ID).is_err());
    assert_eq!(read(&directory, "HEAD"), Some(b"unborn\n".to_vec()));
}
